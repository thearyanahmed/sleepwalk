//! The host side of the guest protocol, over Firecracker's vsock device.
//!
//! [`GuestLink`] dials the guest's vsock port through Firecracker's host-side
//! unix socket (the `CONNECT <port>\n` handshake), then speaks the protocol —
//! sending [`HostToGuest`], receiving [`GuestToHost`], newline-delimited JSON
//! (the mirror of guestd's channel). It is how hostd reaches a live guest to
//! complete the boot handshake and to **drain to quiescence before snapshotting**
//! — the verified idle gap that makes a migration safe.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::time::Duration;

use proto::{GuestToHost, HostToGuest};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

/// The result of asking the guest to drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainState {
    /// Gated and idle — safe to snapshot.
    Quiescent,
    /// A turn is still in flight; not safe (the race rule: the turn wins).
    Busy,
}

/// A connected control link to one guest over vsock.
#[derive(Debug)]
pub struct GuestLink {
    reader: Mutex<BufReader<ReadHalf<UnixStream>>>,
    writer: Mutex<WriteHalf<UnixStream>>,
}

impl GuestLink {
    /// Connect to the guest's vsock `port` via Firecracker's host-side `uds`.
    ///
    /// # Errors
    /// If the socket can't be reached or Firecracker rejects the CONNECT.
    pub async fn connect(uds: &Path, port: u32) -> io::Result<Self> {
        let stream = UnixStream::connect(uds).await?;
        let (read, mut write) = tokio::io::split(stream);
        let mut reader = BufReader::new(read);
        write
            .write_all(format!("CONNECT {port}\n").as_bytes())
            .await?;
        write.flush().await?;
        // Firecracker replies "OK <host_port>\n" once the guest accepts. Keep the
        // BufReader afterwards — it may already hold the guest's first message.
        let mut ok = String::new();
        reader.read_line(&mut ok).await?;
        if !ok.starts_with("OK") {
            return Err(io::Error::other(format!(
                "vsock CONNECT to port {port} failed: {}",
                ok.trim()
            )));
        }
        Ok(Self {
            reader: Mutex::new(reader),
            writer: Mutex::new(write),
        })
    }

    /// Retry [`connect`](Self::connect) until it succeeds or `timeout` elapses —
    /// the guest may still be booting when first dialed.
    ///
    /// # Errors
    /// The last connection error if `timeout` elapses first.
    pub async fn connect_retry(uds: &Path, port: u32, timeout: Duration) -> io::Result<Self> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last = io::Error::other("no attempt");
        while tokio::time::Instant::now() < deadline {
            match Self::connect(uds, port).await {
                Ok(link) => return Ok(link),
                Err(e) => {
                    last = e;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
        Err(last)
    }

    /// Send one message to the guest.
    ///
    /// # Errors
    /// On serialize or write failure.
    pub async fn send(&self, msg: HostToGuest) -> io::Result<()> {
        let mut line = serde_json::to_vec(&msg).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut writer = self.writer.lock().await;
        writer.write_all(&line).await?;
        writer.flush().await
    }

    /// Receive one message from the guest.
    ///
    /// # Errors
    /// On EOF (peer closed) or a malformed line.
    pub async fn recv(&self) -> io::Result<GuestToHost> {
        let mut line = String::new();
        let mut reader = self.reader.lock().await;
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "guest closed"));
        }
        serde_json::from_str(line.trim_end()).map_err(io::Error::other)
    }

    /// Complete the boot handshake: receive the guest's `Hello`, reply `Secrets`.
    ///
    /// # Errors
    /// If the first message is not `Hello`, or I/O fails.
    pub async fn handshake(&self, env: BTreeMap<String, String>) -> io::Result<()> {
        match self.recv().await? {
            GuestToHost::Hello { .. } => self.send(HostToGuest::Secrets { env }).await,
            other => Err(io::Error::other(format!("expected Hello, got {other:?}"))),
        }
    }

    /// Request a drain and wait (up to `deadline`) for the guest's ack, returning
    /// whether it reached quiescence. Non-`DrainAck` messages (stray turn
    /// signals) are skipped.
    ///
    /// # Errors
    /// On I/O failure or if no ack arrives before `deadline`.
    pub async fn drain(&self, deadline: Duration) -> io::Result<DrainState> {
        self.send(HostToGuest::DrainRequest { deadline }).await?;
        let ack = tokio::time::timeout(deadline, async {
            loop {
                if let GuestToHost::DrainAck { in_flight } = self.recv().await? {
                    return io::Result::Ok(in_flight);
                }
            }
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no DrainAck before deadline"))??;
        Ok(if ack.is_none() {
            DrainState::Quiescent
        } else {
            DrainState::Busy
        })
    }
}
