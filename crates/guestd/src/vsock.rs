//! Real `AF_VSOCK` transport for the guest supervisor.
//!
//! [`serve`] is what guestd runs inside the VM: it binds a vsock port, accepts
//! the host's connection, and frames the guest protocol over it with the same
//! [`JsonLineChannel`](crate::framing::JsonLineChannel) used everywhere else — so
//! the supervisor logic is unchanged whether the bytes flow over an in-memory
//! duplex (tests) or a real vsock socket (in a VM). [`connect`] is the dialer for
//! the host side and for loopback tests.
//!
//! Linux-only: `AF_VSOCK` is a Linux address family. The host reaches the guest
//! through Firecracker's vsock device (a host-side unix socket); that wiring is
//! the next slice — here the transport itself is proven over vsock loopback
//! (`VMADDR_CID_LOCAL`), no VM required.

use std::io;

use tokio::io::{ReadHalf, WriteHalf};
use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener, VsockStream};

use crate::framing::JsonLineChannel;

/// The vsock port guestd listens on; the host connects here.
pub const DEFAULT_PORT: u32 = 5252;

/// Bind `port` on any CID, accept one host connection, and return a framed
/// [`GuestChannel`] over it. The guest supervisor runs on the returned channel.
///
/// # Errors
/// If the bind or accept fails.
pub async fn serve(
    port: u32,
) -> io::Result<JsonLineChannel<ReadHalf<VsockStream>, WriteHalf<VsockStream>>> {
    let listener = VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, port))?;
    let (stream, _peer) = listener.accept().await?;
    Ok(JsonLineChannel::new(stream))
}

/// Connect to a guest's vsock at `cid:port` (host side and loopback tests).
///
/// # Errors
/// If the connection fails.
pub async fn connect(cid: u32, port: u32) -> io::Result<VsockStream> {
    VsockStream::connect(VsockAddr::new(cid, port)).await
}

// The loopback round-trip needs Linux + the vsock_loopback module, so it is
// gated behind a feature run explicitly (`just vsock-test` on a Linux host) and
// kept out of the everywhere suite. `GuestChannel` is also exercised over an
// in-memory duplex in `framing`'s tests.
#[cfg(all(test, feature = "vsock-test"))]
mod tests {
    use std::time::Duration;

    use proto::{GuestToHost, GuestdVersion, VmId};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio_vsock::VMADDR_CID_LOCAL;

    use super::*;
    use crate::channel::GuestChannel;
    use crate::guest::Guest;

    const PORT: u32 = 0x5757;

    /// A full guest⇄host handshake over real vsock loopback: the guest sends
    /// Hello, the host replies Secrets, the host drains, the guest acks.
    #[tokio::test(flavor = "multi_thread")]
    async fn handshake_and_drain_over_vsock_loopback() {
        // Guest side: serve, run the supervisor handshake, then handle one drain.
        let guest = tokio::spawn(async {
            let chan = serve(PORT).await.expect("serve");
            let mut g = Guest::new(
                VmId::from_uuid(uuid::Uuid::nil()),
                GuestdVersion::new("0.1.0"),
                chan,
            );
            g.handshake().await.expect("handshake");
            let msg = g.channel().recv().await.expect("recv drain");
            g.handle(msg).await.expect("handle drain");
        });

        // Host side: connect over loopback (retry until the guest has bound).
        let mut stream = None;
        for _ in 0..50 {
            match connect(VMADDR_CID_LOCAL, PORT).await {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        let stream = stream.expect("connect to guest");
        let (read, mut write) = tokio::io::split(stream);
        let mut reader = BufReader::new(read);

        // Guest's first line is Hello.
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read hello");
        let hello: GuestToHost = serde_json::from_str(line.trim_end()).expect("hello json");
        assert!(matches!(hello, GuestToHost::Hello { .. }));

        // Reply Secrets (completes the guest handshake), then drain.
        write
            .write_all(b"{\"type\":\"Secrets\",\"env\":{}}\n")
            .await
            .expect("write secrets");
        write
            .write_all(b"{\"type\":\"DrainRequest\",\"deadline_ms\":5000}\n")
            .await
            .expect("write drain");
        write.flush().await.expect("flush");

        // Guest acks the drain (idle → in_flight null).
        line.clear();
        reader.read_line(&mut line).await.expect("read ack");
        let ack: GuestToHost = serde_json::from_str(line.trim_end()).expect("ack json");
        assert!(matches!(ack, GuestToHost::DrainAck { in_flight: None }));

        guest.await.expect("guest task");
    }
}
