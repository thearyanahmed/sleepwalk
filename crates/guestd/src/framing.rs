//! Newline-delimited JSON framing of the guest protocol over a byte stream.
//!
//! [`JsonLineChannel`] is the portable, transport-agnostic implementation of
//! [`GuestChannel`]: it serializes each outbound message as one JSON line and
//! reads one line per inbound message. It works over any [`AsyncRead`] +
//! [`AsyncWrite`] stream, so it is tested over an in-memory duplex with no real
//! vsock. The real `VsockChannel` (a later slice) is just this channel over an
//! `AF_VSOCK` stream.

use proto::{GuestToHost, HostToGuest};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf, split,
};
use tokio::sync::Mutex;

use crate::channel::{ChannelError, GuestChannel};

/// A [`GuestChannel`] that frames messages as newline-delimited JSON over a
/// stream's read and write halves.
///
/// The read and write halves are guarded independently, so a `send` and a `recv`
/// can be in flight at once — `send` never blocks waiting on a slow `recv`.
#[derive(Debug)]
pub struct JsonLineChannel<R, W> {
    reader: Mutex<BufReader<R>>,
    writer: Mutex<W>,
}

impl<S> JsonLineChannel<ReadHalf<S>, WriteHalf<S>>
where
    S: AsyncRead + AsyncWrite,
{
    /// Frame messages over `stream`, splitting it into independent halves.
    pub fn new(stream: S) -> Self {
        let (read, write) = split(stream);
        Self {
            reader: Mutex::new(BufReader::new(read)),
            writer: Mutex::new(write),
        }
    }
}

fn io_err(e: std::io::Error) -> ChannelError {
    ChannelError::Io(e.to_string())
}

impl<R, W> GuestChannel for JsonLineChannel<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    async fn send(&self, msg: GuestToHost) -> Result<(), ChannelError> {
        let mut line =
            serde_json::to_vec(&msg).map_err(|e| ChannelError::Malformed(e.to_string()))?;
        line.push(b'\n');
        let mut writer = self.writer.lock().await;
        writer.write_all(&line).await.map_err(io_err)?;
        writer.flush().await.map_err(io_err)?;
        Ok(())
    }

    async fn recv(&self) -> Result<HostToGuest, ChannelError> {
        let mut line = String::new();
        let mut reader = self.reader.lock().await;
        let n = reader.read_line(&mut line).await.map_err(io_err)?;
        drop(reader);
        if n == 0 {
            return Err(ChannelError::Closed);
        }
        serde_json::from_str(line.trim_end()).map_err(|e| ChannelError::Malformed(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use proto::{GuestdVersion, VmId};
    use tokio::io::AsyncWriteExt;

    use super::*;

    #[tokio::test]
    async fn sends_one_json_line_per_message() {
        let (guest, host) = tokio::io::duplex(1024);
        let chan = JsonLineChannel::new(guest);
        chan.send(GuestToHost::Hello {
            vm_id: VmId::from_uuid(uuid::Uuid::nil()),
            guestd_version: GuestdVersion::new("0.1.0"),
        })
        .await
        .expect("send");

        // The host side reads exactly one newline-terminated JSON line that
        // decodes back to the message.
        let mut host = BufReader::new(host);
        let mut line = String::new();
        host.read_line(&mut line).await.expect("read line");
        assert!(line.ends_with('\n'));
        let decoded: GuestToHost = serde_json::from_str(line.trim_end()).expect("decode");
        assert!(matches!(decoded, GuestToHost::Hello { .. }));
    }

    #[tokio::test]
    async fn receives_messages_written_by_the_peer() {
        let (guest, mut host) = tokio::io::duplex(1024);
        let chan = JsonLineChannel::new(guest);
        // A payload variant and a unit variant, each one canonical JSON line.
        host.write_all(b"{\"DrainRequest\":{\"deadline_ms\":250}}\n")
            .await
            .expect("write drain");
        host.write_all(b"\"Ping\"\n").await.expect("write ping");

        let first = chan.recv().await.expect("recv 1");
        assert!(matches!(first, HostToGuest::DrainRequest { .. }));
        let second = chan.recv().await.expect("recv 2");
        assert!(matches!(second, HostToGuest::Ping));
    }

    #[tokio::test]
    async fn closed_peer_surfaces_as_closed() {
        let (guest, host) = tokio::io::duplex(1024);
        let chan = JsonLineChannel::new(guest);
        drop(host); // peer hangs up
        let err = chan.recv().await.expect_err("must be closed");
        assert!(matches!(err, ChannelError::Closed));
    }

    #[tokio::test]
    async fn garbage_line_is_malformed_not_a_panic() {
        let (guest, mut host) = tokio::io::duplex(1024);
        let chan = JsonLineChannel::new(guest);
        host.write_all(b"this is not json\n").await.expect("write");
        let err = chan.recv().await.expect_err("must be malformed");
        assert!(matches!(err, ChannelError::Malformed(_)));
    }
}
