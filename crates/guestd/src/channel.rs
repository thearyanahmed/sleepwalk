//! The vsock channel port.
//!
//! [`GuestChannel`] is the seam between the supervisor's logic and the wire. The
//! real implementation (a later slice) is an `AF_VSOCK` socket carrying
//! newline-delimited JSON; the scripted test stand-in lives in
//! [`crate::pseudo_channel`], so the supervisor in [`crate::guest`] is testable
//! without a real vsock.

use proto::{GuestToHost, HostToGuest};
use thiserror::Error;

/// An error moving a message over the guest channel.
#[derive(Debug, Error)]
pub enum ChannelError {
    /// The peer (hostd) closed the connection.
    #[error("guest channel closed")]
    Closed,

    /// An I/O error on the underlying transport.
    #[error("guest channel io: {0}")]
    Io(String),
}

/// The vsock channel the supervisor uses to talk to hostd.
///
/// `&self` rather than `&mut self`: a real vsock connection is split into
/// independent read and write halves, so sending and receiving do not need
/// exclusive access to one object.
pub trait GuestChannel {
    /// Send one message to hostd.
    fn send(
        &self,
        msg: GuestToHost,
    ) -> impl std::future::Future<Output = Result<(), ChannelError>> + Send;

    /// Receive the next message from hostd, awaiting one if necessary.
    fn recv(&self) -> impl std::future::Future<Output = Result<HostToGuest, ChannelError>> + Send;
}
