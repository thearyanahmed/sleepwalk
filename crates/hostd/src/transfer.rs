//! Snapshot transfer between hosts.
//!
//! A snapshot is two files (the memory dump and the VM state); this module
//! streams them over any byte stream — loopback, unix socket, or TCP between
//! hosts. Each file is framed as `name · length · data · CRC32`, and the data is
//! moved in fixed chunks so an 8 GB memory file never has to fit in RAM. The
//! receiver verifies the CRC before accepting a file. A zero-length name marks
//! the end of the stream.
//!
//! Resumability is intentionally out of scope for v0 (a failed transfer is
//! retried whole); the snapshot is still on the source until cutover.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crc32fast::Hasher;
use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Chunk size for streaming file data (bounds peak memory).
const CHUNK: usize = 64 * 1024;

/// A failure moving a snapshot.
#[derive(Debug, Error)]
pub enum TransferError {
    /// An I/O error on a file or the stream.
    #[error("transfer io: {0}")]
    Io(String),

    /// A received file's CRC did not match — corruption in transit.
    #[error("checksum mismatch for {file}: expected {expected:08x}, got {got:08x}")]
    Checksum {
        /// The file whose checksum failed.
        file: String,
        /// The CRC the sender computed.
        expected: u32,
        /// The CRC the receiver computed.
        got: u32,
    },

    /// The stream did not follow the framing (bad name, short data, …).
    #[error("transfer protocol error: {0}")]
    Protocol(String),
}

fn io(e: std::io::Error) -> TransferError {
    TransferError::Io(e.to_string())
}

/// One file to send: the `name` the receiver writes it under, and the local
/// `path` to read it from.
#[derive(Clone, Debug)]
pub struct OutboundFile {
    /// The base name the receiver will write (no path separators).
    pub name: String,
    /// The source path on this host.
    pub path: PathBuf,
}

/// Stream `files` over `writer`, framed and checksummed, ending with a marker.
pub async fn send_files<W>(writer: &mut W, files: &[OutboundFile]) -> Result<(), TransferError>
where
    W: AsyncWrite + Unpin,
{
    for f in files {
        let name = f.name.as_bytes();
        let name_len = u16::try_from(name.len())
            .map_err(|_| TransferError::Protocol(format!("name too long: {}", f.name)))?;
        let mut src = File::open(&f.path).await.map_err(io)?;
        let len = src.metadata().await.map_err(io)?.len();

        writer.write_u16(name_len).await.map_err(io)?;
        writer.write_all(name).await.map_err(io)?;
        writer.write_u64(len).await.map_err(io)?;

        let mut hasher = Hasher::new();
        let mut buf = vec![0u8; CHUNK];
        let mut remaining = len;
        while remaining > 0 {
            let want = remaining.min(CHUNK as u64) as usize;
            let n = src.read(&mut buf[..want]).await.map_err(io)?;
            if n == 0 {
                return Err(TransferError::Protocol(format!(
                    "{} is shorter than its declared length",
                    f.name
                )));
            }
            hasher.update(&buf[..n]);
            writer.write_all(&buf[..n]).await.map_err(io)?;
            remaining -= n as u64;
        }
        writer.write_u32(hasher.finalize()).await.map_err(io)?;
    }
    writer.write_u16(0).await.map_err(io)?; // end-of-stream marker
    writer.flush().await.map_err(io)?;
    Ok(())
}

/// Receive files into `dest_dir`, verifying each CRC. Returns the written paths.
pub async fn recv_files<R>(reader: &mut R, dest_dir: &Path) -> Result<Vec<PathBuf>, TransferError>
where
    R: AsyncRead + Unpin,
{
    let mut written = Vec::new();
    loop {
        let name_len = reader.read_u16().await.map_err(io)?;
        if name_len == 0 {
            break;
        }
        let mut name_buf = vec![0u8; usize::from(name_len)];
        reader.read_exact(&mut name_buf).await.map_err(io)?;
        let name = String::from_utf8(name_buf)
            .map_err(|_| TransferError::Protocol("non-utf8 file name".to_owned()))?;
        // Refuse anything that could escape dest_dir.
        if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err(TransferError::Protocol(format!("unsafe file name: {name}")));
        }
        let len = reader.read_u64().await.map_err(io)?;

        let dest = dest_dir.join(&name);
        let mut out = File::create(&dest).await.map_err(io)?;
        let mut hasher = Hasher::new();
        let mut buf = vec![0u8; CHUNK];
        let mut remaining = len;
        while remaining > 0 {
            let want = remaining.min(CHUNK as u64) as usize;
            reader.read_exact(&mut buf[..want]).await.map_err(io)?;
            hasher.update(&buf[..want]);
            out.write_all(&buf[..want]).await.map_err(io)?;
            remaining -= want as u64;
        }
        out.flush().await.map_err(io)?;

        let expected = reader.read_u32().await.map_err(io)?;
        let got = hasher.finalize();
        if expected != got {
            return Err(TransferError::Checksum {
                file: name,
                expected,
                got,
            });
        }
        written.push(dest);
    }
    Ok(written)
}

/// Connect to a receiver at `addr` and stream the snapshot `files` over TCP.
///
/// The host-to-host path: the source calls this after `create_snapshot` to move
/// the memory + vmstate to the target, which is running [`recv_snapshot`].
///
/// # Errors
/// If the connection, the framed send, or the shutdown fails.
pub async fn send_snapshot(addr: &str, files: &[OutboundFile]) -> Result<(), TransferError> {
    let mut stream = TcpStream::connect(addr).await.map_err(io)?;
    send_files(&mut stream, files).await?;
    stream.shutdown().await.map_err(io)?;
    Ok(())
}

/// How long a receiver waits for the sender to connect before giving up. A live
/// migration connects within a snapshot's time; a sender that stood down (drain
/// found the guest busy) never connects, so this bounds how long the receiver
/// holds its port — without it, a stood-down migration leaks the listener and the
/// next attempt can't bind (`Address already in use`).
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Accept one connection on `listener` and receive a snapshot into `dest_dir`,
/// verifying each file's CRC. Returns the written paths.
///
/// # Errors
/// If no sender connects within [`ACCEPT_TIMEOUT`], or accepting, receiving, or a
/// checksum check fails.
pub async fn recv_snapshot(
    listener: &TcpListener,
    dest_dir: &Path,
) -> Result<Vec<PathBuf>, TransferError> {
    let (mut stream, _) = match tokio::time::timeout(ACCEPT_TIMEOUT, listener.accept()).await {
        Ok(accepted) => accepted.map_err(io)?,
        Err(_) => {
            return Err(io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "no sender connected before timeout",
            )));
        }
    };
    recv_files(&mut stream, dest_dir).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("sleepwalk-xfer-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).expect("mkdir");
        d
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).expect("write file");
    }

    #[tokio::test]
    async fn round_trips_files_byte_for_byte() {
        let src = temp_dir();
        let mem = src.join("mem");
        let state = src.join("state");
        write(&mem, &vec![0xABu8; 200_000]); // spans several chunks
        write(&state, b"vmstate-blob");

        let mut buf: Vec<u8> = Vec::new();
        send_files(
            &mut buf,
            &[
                OutboundFile {
                    name: "mem".to_owned(),
                    path: mem.clone(),
                },
                OutboundFile {
                    name: "state".to_owned(),
                    path: state.clone(),
                },
            ],
        )
        .await
        .expect("send");

        let dest = temp_dir();
        let written = recv_files(&mut buf.as_slice(), &dest).await.expect("recv");

        assert_eq!(written.len(), 2);
        assert_eq!(
            std::fs::read(dest.join("mem")).unwrap(),
            vec![0xABu8; 200_000]
        );
        assert_eq!(std::fs::read(dest.join("state")).unwrap(), b"vmstate-blob");
    }

    #[tokio::test]
    async fn corruption_in_transit_is_caught_by_the_checksum() {
        let src = temp_dir();
        let f = src.join("f");
        write(&f, b"abcdefghij");
        let mut buf: Vec<u8> = Vec::new();
        send_files(
            &mut buf,
            &[OutboundFile {
                name: "f".to_owned(),
                path: f,
            }],
        )
        .await
        .expect("send");

        // Flip a byte inside the data region: 2 (name_len) + 1 (name) + 8 (len).
        buf[11 + 4] ^= 0xFF;

        let dest = temp_dir();
        let err = recv_files(&mut buf.as_slice(), &dest)
            .await
            .expect_err("corruption must be caught");
        assert!(matches!(err, TransferError::Checksum { .. }));
    }

    #[tokio::test]
    async fn a_name_that_escapes_the_dir_is_rejected() {
        // Hand-frame one file named "../evil" with no data.
        let mut buf: Vec<u8> = Vec::new();
        let name = b"../evil";
        buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
        buf.extend_from_slice(name);
        buf.extend_from_slice(&0u64.to_be_bytes()); // len 0
        buf.extend_from_slice(&0u32.to_be_bytes()); // crc of empty
        buf.extend_from_slice(&0u16.to_be_bytes()); // end marker

        let dest = temp_dir();
        let err = recv_files(&mut buf.as_slice(), &dest)
            .await
            .expect_err("path traversal must be rejected");
        assert!(matches!(err, TransferError::Protocol(_)));
    }

    #[tokio::test]
    async fn empty_transfer_writes_nothing() {
        let mut buf: Vec<u8> = Vec::new();
        send_files(&mut buf, &[]).await.expect("send none");
        let dest = temp_dir();
        let written = recv_files(&mut buf.as_slice(), &dest).await.expect("recv");
        assert!(written.is_empty());
    }

    #[tokio::test]
    async fn snapshot_round_trips_over_tcp() {
        let src = temp_dir();
        let mem = src.join("mem.snap");
        let state = src.join("state.snap");
        write(&mem, &vec![0x5Au8; 200_000]);
        write(&state, b"vmstate-blob");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let dest = temp_dir();
        let dest_for_task = dest.clone();
        let recv = tokio::spawn(async move { recv_snapshot(&listener, &dest_for_task).await });

        send_snapshot(
            &addr,
            &[
                OutboundFile {
                    name: "mem.snap".to_owned(),
                    path: mem,
                },
                OutboundFile {
                    name: "state.snap".to_owned(),
                    path: state,
                },
            ],
        )
        .await
        .expect("send over tcp");

        let written = recv.await.expect("join").expect("recv over tcp");
        assert_eq!(written.len(), 2);
        assert_eq!(
            std::fs::read(dest.join("mem.snap")).unwrap(),
            vec![0x5Au8; 200_000]
        );
        assert_eq!(
            std::fs::read(dest.join("state.snap")).unwrap(),
            b"vmstate-blob"
        );
    }
}
