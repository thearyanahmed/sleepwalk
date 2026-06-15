//! Driving an open-loop load at a live guest over vsock.
//!
//! [`VsockTurnDriver`] is the [`harness::TurnDriver`] that turns the abstract
//! "run turn N" into a real [`RunTurn`](proto::HostToGuest::RunTurn) over a
//! [`GuestLink`], and resolves when the guest reports it done. Because the load
//! is open-loop, many turns are in flight at once on one connection, so a single
//! reader task demuxes each [`TurnEnded`](proto::GuestToHost::TurnEnded) back to
//! the turn that is waiting on it (correlated by the host-assigned id). Point a
//! migration's freeze at this and the stalled turns show up in the latency tail
//! exactly as a client would feel them.
//!
//! A turn carries a per-turn deadline: if no completion arrives in time the turn
//! resolves anyway and is counted as **dropped** — the client's view of a request
//! that never came back. This is load-bearing for the freeze instrument: pausing
//! a VM resets the in-flight vsock RX queue, so turns put on the wire mid-pause
//! are lost on resume. Without the deadline their waiters would hang forever;
//! with it they land in the tail and the drop count is reported, instead of the
//! run wedging.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use proto::{GuestToHost, HostToGuest, TurnId};
use tokio::sync::{Mutex, oneshot};

use crate::guestlink::GuestLink;

type Waiters = Arc<Mutex<HashMap<TurnId, oneshot::Sender<()>>>>;

/// An open-loop turn driver backed by a live guest over vsock.
#[derive(Clone)]
pub struct VsockTurnDriver {
    link: Arc<GuestLink>,
    waiters: Waiters,
    deadline: Duration,
    dropped: Arc<AtomicU64>,
    /// The VM this driver targets — the `vm_id` label on the test-turn metric, so
    /// the load's request rate can be sliced per VM in the dashboard.
    vm_id: Arc<str>,
}

impl VsockTurnDriver {
    /// Wrap a connected, handshaken [`GuestLink`] and spawn the completion
    /// router. `deadline` bounds how long a single turn waits for its completion
    /// before being counted as dropped. The router reads the guest's stream and
    /// wakes each turn when its `TurnEnded` arrives; when the link closes it drops
    /// every outstanding waiter, so no [`run_turn`](harness::TurnDriver::run_turn)
    /// hangs.
    #[must_use]
    pub fn new(link: Arc<GuestLink>, deadline: Duration, vm_id: impl Into<Arc<str>>) -> Self {
        let waiters: Waiters = Arc::new(Mutex::new(HashMap::new()));
        let link_r = Arc::clone(&link);
        let waiters_r = Arc::clone(&waiters);
        tokio::spawn(async move {
            loop {
                match link_r.recv().await {
                    Ok(GuestToHost::TurnEnded { turn_id, .. }) => {
                        if let Some(tx) = waiters_r.lock().await.remove(&turn_id) {
                            let _ = tx.send(());
                        }
                    }
                    // TurnStarted / Pong / anything else: not a completion.
                    Ok(_) => {}
                    // Link closed (guest gone, e.g. snapshot taken): drop all
                    // waiters so their `rx.await` resolves and the run unwinds.
                    Err(_) => break,
                }
            }
        });
        Self {
            link,
            waiters,
            deadline,
            dropped: Arc::new(AtomicU64::new(0)),
            vm_id: vm_id.into(),
        }
    }

    /// How many turns were dropped (no completion within the deadline).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl harness::TurnDriver for VsockTurnDriver {
    async fn run_turn(&self, turn: u64) {
        let id = TurnId::from_u64(turn);
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.insert(id, tx);
        if self
            .link
            .send(HostToGuest::RunTurn { turn_id: id })
            .await
            .is_err()
        {
            self.waiters.lock().await.remove(&id);
            crate::telemetry::test_turn(&self.vm_id, false);
            return;
        }
        // Resolves when the router gets this turn's TurnEnded, when the link
        // closes (sender dropped), or when the deadline passes — a turn lost to a
        // pause/resume never gets a completion, so the deadline is what unwedges
        // it and books it as a client-visible drop.
        if tokio::time::timeout(self.deadline, rx).await.is_err() {
            self.waiters.lock().await.remove(&id);
            self.dropped.fetch_add(1, Ordering::Relaxed);
            crate::telemetry::test_turn(&self.vm_id, false);
        } else {
            crate::telemetry::test_turn(&self.vm_id, true);
        }
    }
}
