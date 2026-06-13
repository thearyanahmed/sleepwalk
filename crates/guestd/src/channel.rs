//! The vsock channel port and a scripted test fake.
//!
//! [`GuestChannel`] is the seam between the supervisor's logic and the wire. The
//! real implementation (a later slice) is an `AF_VSOCK` socket carrying
//! newline-delimited JSON; this slice ships only the port and a [`FakeChannel`]
//! so the supervisor in [`crate::guest`] is testable without a real vsock.

use std::collections::VecDeque;
use std::sync::Mutex;

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

/// A scripted, recording fake channel for tests.
///
/// Pre-load the messages hostd "sends" with [`push_inbound`](Self::push_inbound);
/// inspect what the supervisor sent with [`sent`](Self::sent). [`recv`] returns
/// [`ChannelError::Closed`] once the scripted inbox is drained, modelling a
/// hung-up peer.
#[derive(Debug, Default)]
pub struct FakeChannel {
    inbox: Mutex<VecDeque<HostToGuest>>,
    sent: Mutex<Vec<GuestToHost>>,
}

impl FakeChannel {
    /// A fresh fake with an empty inbox.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Script a message for the supervisor to receive, FIFO.
    pub fn push_inbound(&self, msg: HostToGuest) {
        self.lock_inbox().push_back(msg);
    }

    /// The messages the supervisor has sent, in order.
    #[must_use]
    pub fn sent(&self) -> Vec<GuestToHost> {
        #[allow(clippy::unwrap_used)]
        self.sent.lock().unwrap().clone()
    }

    fn lock_inbox(&self) -> std::sync::MutexGuard<'_, VecDeque<HostToGuest>> {
        #[allow(clippy::unwrap_used)]
        self.inbox.lock().unwrap()
    }
}

impl GuestChannel for FakeChannel {
    async fn send(&self, msg: GuestToHost) -> Result<(), ChannelError> {
        #[allow(clippy::unwrap_used)]
        self.sent.lock().unwrap().push(msg);
        Ok(())
    }

    async fn recv(&self) -> Result<HostToGuest, ChannelError> {
        self.lock_inbox().pop_front().ok_or(ChannelError::Closed)
    }
}
