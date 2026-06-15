//! The control loop: turn a placement + pressure reading into an actual
//! migration by calling the hostd daemons.
//!
//! [`rebalance_once`] is the closed loop in miniature: ask [`pick_victim`] for
//! the single best move, then execute it by telling the **target** daemon to
//! receive and the **source** daemon to send. The daemon calls go through the
//! [`DaemonApi`] port, so the decision-and-execution logic tests against a
//! [`PseudoDaemon`] with no network; the real HTTP client is a separate impl.
//!
//! Order matters: the receiver is started first (it binds the data socket and
//! returns once listening), then the sender connects — so the target is always
//! ready before the source streams to it.

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use proto::{CompatClass, HostId, VmId};
use thiserror::Error;

use crate::placement::{Placement, Pressure, Rebalance, pick_victim};

/// A failure executing a rebalance.
#[derive(Debug, Error)]
pub enum CtlError {
    /// A chosen host has no endpoint registered in the fleet.
    #[error("host not in fleet: {0}")]
    UnknownHost(HostId),
    /// A daemon call failed.
    #[error("daemon {host} {op}: {detail}")]
    Daemon {
        /// The host whose daemon was called.
        host: HostId,
        /// The operation attempted (`recv` / `send`).
        op: &'static str,
        /// What went wrong.
        detail: String,
    },
}

/// How to reach one host's daemon and its data plane.
#[derive(Debug, Clone)]
pub struct HostEndpoint {
    /// The daemon control base URL, e.g. `http://10.0.0.2:8080`.
    pub control_url: String,
    /// What the target binds to receive a migration, e.g. `0.0.0.0:9000`.
    pub data_listen: String,
    /// What a source connects to to reach this host's data plane, e.g.
    /// `10.0.0.2:9000`.
    pub data_addr: String,
}

/// The fleet: each host's daemon + data-plane endpoints.
#[derive(Debug, Clone, Default)]
pub struct Fleet {
    hosts: BTreeMap<HostId, HostEndpoint>,
}

impl Fleet {
    /// An empty fleet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a host's endpoints.
    pub fn add(&mut self, host: HostId, endpoint: HostEndpoint) {
        self.hosts.insert(host, endpoint);
    }

    fn endpoint(&self, host: &HostId) -> Result<&HostEndpoint, CtlError> {
        self.hosts
            .get(host)
            .ok_or_else(|| CtlError::UnknownHost(host.clone()))
    }
}

/// The hostd daemon control surface the rebalancer drives. The HTTP client
/// implements this against `POST /migrate/recv` and `POST /migrate/send`;
/// [`PseudoDaemon`] implements it for tests.
pub trait DaemonApi {
    /// Tell the daemon at `control_url` to receive a migration on `listen`. Must
    /// return once the daemon is listening (not when the migration finishes).
    fn migrate_recv(
        &self,
        control_url: &str,
        listen: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;

    /// Tell the daemon at `control_url` to send its registered VM `vm` to `to`.
    fn migrate_send(
        &self,
        control_url: &str,
        vm: &str,
        to: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;
}

/// Run one rebalance step: pick the best move and execute it, or do nothing.
///
/// Returns the [`Rebalance`] that was carried out, or `None` if no move was
/// warranted (no host over `high_watermark`, or none cooler to move to).
///
/// # Errors
/// If a chosen host is missing from `fleet`, or a daemon call fails.
pub async fn rebalance_once(
    placement: &Placement,
    pressure: &BTreeMap<HostId, Pressure>,
    idle: &BTreeMap<VmId, Duration>,
    high_watermark: Pressure,
    compat: &BTreeMap<HostId, CompatClass>,
    fleet: &Fleet,
    api: &impl DaemonApi,
) -> Result<Option<Rebalance>, CtlError> {
    let Some(mv) = pick_victim(placement, pressure, idle, high_watermark, compat) else {
        return Ok(None);
    };
    let target = fleet.endpoint(&mv.to)?;
    let source = fleet.endpoint(&mv.from)?;

    // Receiver first (binds + listens), then the sender connects.
    api.migrate_recv(&target.control_url, &target.data_listen)
        .await
        .map_err(|detail| CtlError::Daemon {
            host: mv.to.clone(),
            op: "recv",
            detail,
        })?;
    api.migrate_send(&source.control_url, &mv.vm.to_string(), &target.data_addr)
        .await
        .map_err(|detail| CtlError::Daemon {
            host: mv.from.clone(),
            op: "send",
            detail,
        })?;
    Ok(Some(mv))
}

/// A recording fake daemon for tests: logs the calls it received and can be
/// primed to fail one of them.
#[derive(Debug, Default)]
pub struct PseudoDaemon {
    calls: std::sync::Mutex<Vec<String>>,
    fail: std::sync::Mutex<Option<&'static str>>,
}

impl PseudoDaemon {
    /// A fake that succeeds every call.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Prime the fake so the next `recv` or `send` fails.
    pub fn fail_on(&self, op: &'static str) {
        #[allow(clippy::unwrap_used)]
        let mut f = self.fail.lock().unwrap();
        *f = Some(op);
    }

    /// The ordered calls the fake received, as `"op url arg"` strings.
    #[must_use]
    pub fn calls(&self) -> Vec<String> {
        #[allow(clippy::unwrap_used)]
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, op: &'static str, url: &str, arg: &str) -> Result<(), String> {
        #[allow(clippy::unwrap_used)]
        self.calls.lock().unwrap().push(format!("{op} {url} {arg}"));
        #[allow(clippy::unwrap_used)]
        let mut f = self.fail.lock().unwrap();
        if *f == Some(op) {
            *f = None;
            return Err(format!("injected failure on {op}"));
        }
        Ok(())
    }
}

impl DaemonApi for PseudoDaemon {
    async fn migrate_recv(&self, control_url: &str, listen: &str) -> Result<(), String> {
        self.record("recv", control_url, listen)
    }
    async fn migrate_send(&self, control_url: &str, vm: &str, to: &str) -> Result<(), String> {
        self.record("send", control_url, &format!("{vm} {to}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placement::Placement;

    fn host(name: &str) -> HostId {
        HostId::new(name)
    }

    /// `a` and `b` in one compatible class.
    fn compat() -> BTreeMap<HostId, CompatClass> {
        let c = CompatClass {
            vendor: "GenuineIntel".to_owned(),
            model: "Xeon".to_owned(),
            tsc_khz: 2_000_000,
            kernel: "6.1.155".to_owned(),
        };
        BTreeMap::from([(host("a"), c.clone()), (host("b"), c)])
    }

    fn fleet() -> Fleet {
        let mut f = Fleet::new();
        f.add(
            host("a"),
            HostEndpoint {
                control_url: "http://a:8080".to_owned(),
                data_listen: "0.0.0.0:9000".to_owned(),
                data_addr: "a:9000".to_owned(),
            },
        );
        f.add(
            host("b"),
            HostEndpoint {
                control_url: "http://b:8080".to_owned(),
                data_listen: "0.0.0.0:9000".to_owned(),
                data_addr: "b:9000".to_owned(),
            },
        );
        f
    }

    /// host-a is hot, host-b cool: the loop drives a recv on B then a send on A,
    /// in that order, with the right endpoints.
    #[tokio::test]
    async fn drives_recv_then_send_for_the_chosen_move() {
        let vm = VmId::new();
        let mut placement = Placement::new();
        placement.assign(host("a"), vm);
        placement.assign(host("b"), VmId::new());
        let pressure = BTreeMap::from([
            (host("a"), Pressure::new(0.95)),
            (host("b"), Pressure::new(0.10)),
        ]);
        let idle = BTreeMap::from([(vm, Duration::from_secs(30))]);
        let api = PseudoDaemon::new();

        let mv = rebalance_once(
            &placement,
            &pressure,
            &idle,
            Pressure::new(0.80),
            &compat(),
            &fleet(),
            &api,
        )
        .await
        .expect("rebalance")
        .expect("a move");

        assert_eq!(mv.from, host("a"));
        assert_eq!(mv.to, host("b"));
        // Receiver on B first (its control URL + listen addr), then sender on A.
        assert_eq!(
            api.calls(),
            [
                "recv http://b:8080 0.0.0.0:9000".to_owned(),
                format!("send http://a:8080 {vm} b:9000"),
            ]
        );
    }

    /// Nothing over the watermark: no move, no daemon calls.
    #[tokio::test]
    async fn does_nothing_when_no_host_is_hot() {
        let vm = VmId::new();
        let mut placement = Placement::new();
        placement.assign(host("a"), vm);
        let pressure = BTreeMap::from([
            (host("a"), Pressure::new(0.40)),
            (host("b"), Pressure::new(0.10)),
        ]);
        let idle = BTreeMap::from([(vm, Duration::from_secs(30))]);
        let api = PseudoDaemon::new();

        let result = rebalance_once(
            &placement,
            &pressure,
            &idle,
            Pressure::new(0.80),
            &compat(),
            &fleet(),
            &api,
        )
        .await
        .expect("rebalance");

        assert!(result.is_none());
        assert!(api.calls().is_empty());
    }

    /// A failed receiver call surfaces as an error and the sender is never called.
    #[tokio::test]
    async fn recv_failure_aborts_before_send() {
        let vm = VmId::new();
        let mut placement = Placement::new();
        placement.assign(host("a"), vm);
        placement.assign(host("b"), VmId::new());
        let pressure = BTreeMap::from([
            (host("a"), Pressure::new(0.95)),
            (host("b"), Pressure::new(0.10)),
        ]);
        let idle = BTreeMap::from([(vm, Duration::from_secs(30))]);
        let api = PseudoDaemon::new();
        api.fail_on("recv");

        let err = rebalance_once(
            &placement,
            &pressure,
            &idle,
            Pressure::new(0.80),
            &compat(),
            &fleet(),
            &api,
        )
        .await
        .expect_err("recv failure must surface");

        assert!(matches!(err, CtlError::Daemon { op: "recv", .. }));
        // Only the receiver was attempted; the sender was not reached.
        assert_eq!(api.calls(), ["recv http://b:8080 0.0.0.0:9000"]);
    }
}
