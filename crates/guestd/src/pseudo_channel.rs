//! A scripted, recording stand-in for the vsock channel, used in tests.
//!
//! [`PseudoChannel`] implements [`GuestChannel`](crate::channel::GuestChannel)
//! without a real vsock. Pre-load the messages hostd "sends" with
//! [`push_inbound`](PseudoChannel::push_inbound); inspect what the supervisor
//! sent with [`sent`](PseudoChannel::sent). [`recv`](PseudoChannel) returns
//! [`ChannelError::Closed`] once the scripted inbox is drained, modelling a
//! hung-up peer.

use std::collections::VecDeque;
use std::sync::Mutex;

use proto::{GuestToHost, HostToGuest};

use crate::channel::{ChannelError, GuestChannel};

/// A fake guest channel that scripts inbound messages and records outbound ones.
#[derive(Debug, Default)]
pub struct PseudoChannel {
    inbox: Mutex<VecDeque<HostToGuest>>,
    sent: Mutex<Vec<GuestToHost>>,
}

impl PseudoChannel {
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

impl GuestChannel for PseudoChannel {
    async fn send(&self, msg: GuestToHost) -> Result<(), ChannelError> {
        #[allow(clippy::unwrap_used)]
        self.sent.lock().unwrap().push(msg);
        Ok(())
    }

    async fn recv(&self) -> Result<HostToGuest, ChannelError> {
        self.lock_inbox().pop_front().ok_or(ChannelError::Closed)
    }
}
