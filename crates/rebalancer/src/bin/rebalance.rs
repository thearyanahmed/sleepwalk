//! `rebalance` — run one rebalance step against a fleet from a JSON config.
//!
//! Reads each host's daemon endpoint, current memory pressure (real or injected),
//! and the VMs it hosts; asks the control loop for the single best move; and, if
//! one is warranted, executes it by calling the hostd daemons. This closes the
//! loop: pressure in → migration out, with no manual orchestration.
//!
//!   rebalance <config.json>
//!
//! Config:
//! ```json
//! {
//!   "watermark": 0.8,
//!   "hosts": [
//!     {"id":"a","control_url":"http://10.0.0.1:8080","data_listen":"0.0.0.0:9000","data_addr":"10.0.0.1:9000","pressure":0.95,"vms":["w1"]},
//!     {"id":"b","control_url":"http://10.0.0.2:8080","data_listen":"0.0.0.0:9000","data_addr":"10.0.0.2:9000","pressure":0.10,"vms":["w2"]}
//!   ]
//! }
//! ```

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use http_body_util::Empty;
use hyper::body::Bytes;
use proto::{HostId, VmId};
use rebalancer::{DaemonApi, Fleet, HostEndpoint, Placement, Pressure, rebalance_once};
use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    watermark: f64,
    hosts: Vec<HostCfg>,
}

#[derive(Deserialize)]
struct HostCfg {
    id: String,
    control_url: String,
    data_listen: String,
    data_addr: String,
    pressure: f64,
    #[serde(default)]
    vms: Vec<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: rebalance <config.json>")?;
    let cfg: Config = serde_json::from_str(
        &std::fs::read_to_string(&path).with_context(|| format!("read {path}"))?,
    )
    .context("parse config")?;

    let mut fleet = Fleet::new();
    let mut placement = Placement::new();
    let mut pressure = BTreeMap::new();
    let mut idle = BTreeMap::new();
    for h in &cfg.hosts {
        let host = HostId::new(&h.id);
        fleet.add(
            host.clone(),
            HostEndpoint {
                control_url: h.control_url.clone(),
                data_listen: h.data_listen.clone(),
                data_addr: h.data_addr.clone(),
            },
        );
        pressure.insert(host.clone(), Pressure::new(h.pressure));
        for _ in &h.vms {
            let vm = VmId::new();
            placement.assign(host.clone(), vm);
            idle.insert(vm, Duration::from_secs(60));
        }
    }

    let api = HttpDaemon;
    match rebalance_once(
        &placement,
        &pressure,
        &idle,
        Pressure::new(cfg.watermark),
        &fleet,
        &api,
    )
    .await
    .context("rebalance")?
    {
        Some(mv) => println!("rebalanced: migrated a VM {} -> {}", mv.from, mv.to),
        None => println!("no rebalance needed (no host over watermark)"),
    }
    Ok(())
}

/// The HTTP client side of [`DaemonApi`]: POSTs to the hostd control endpoints.
struct HttpDaemon;

impl DaemonApi for HttpDaemon {
    async fn migrate_recv(&self, control_url: &str, listen: &str) -> Result<(), String> {
        post(&format!("{control_url}/migrate/recv?listen={listen}")).await
    }
    async fn migrate_send(&self, control_url: &str, to: &str) -> Result<(), String> {
        post(&format!("{control_url}/migrate/send?to={to}")).await
    }
}

/// POST `url` with an empty body; Ok on a 2xx, Err otherwise.
async fn post(url: &str) -> Result<(), String> {
    let uri: hyper::Uri = url.parse().map_err(|e| format!("bad url {url}: {e}"))?;
    let host = uri.host().ok_or_else(|| format!("no host in {url}"))?;
    let port = uri.port_u16().unwrap_or(80);
    let stream = tokio::net::TcpStream::connect((host, port))
        .await
        .map_err(|e| format!("connect {host}:{port}: {e}"))?;
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .map_err(|e| format!("handshake: {e}"))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    let req = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri(path)
        .header(hyper::header::HOST, host)
        .body(Empty::<Bytes>::new())
        .map_err(|e| format!("build request: {e}"))?;
    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| format!("send: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {status}"))
    }
}
