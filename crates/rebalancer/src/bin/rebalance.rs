//! `rebalance` — converge a fleet to balanced memory pressure.
//!
//! Polls each hostd daemon's live `GET /status` for its pressure and the VMs it
//! runs, asks the control loop for the single best move, executes it by calling
//! the daemons, and repeats until no host is over the watermark (or no cooler
//! host remains). Pressure in → migrations out, no manual orchestration and no
//! injected numbers: the fleet state is whatever the daemons actually report.
//!
//!   rebalance <config.json>
//!
//! Config (endpoints only — pressure and VMs are read live):
//! ```json
//! {
//!   "watermark": 0.30,
//!   "max_steps": 10,
//!   "hosts": [
//!     {"id":"a","control_url":"http://10.0.0.1:8080","data_listen":"0.0.0.0:9000","data_addr":"10.0.0.1:9000"},
//!     {"id":"b","control_url":"http://10.0.0.2:8080","data_listen":"0.0.0.0:9000","data_addr":"10.0.0.2:9000"}
//!   ]
//! }
//! ```

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use proto::{HostId, VmId};
use rebalancer::{DaemonApi, Fleet, HostEndpoint, Placement, Pressure, rebalance_once};
use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    watermark: f64,
    #[serde(default = "default_max_steps")]
    max_steps: u32,
    hosts: Vec<HostCfg>,
}

fn default_max_steps() -> u32 {
    10
}

#[derive(Deserialize, Clone)]
struct HostCfg {
    id: String,
    control_url: String,
    data_listen: String,
    data_addr: String,
}

/// One host's live status, as returned by `GET /status`.
#[derive(Deserialize)]
struct StatusResp {
    #[allow(dead_code)]
    host: String,
    vms: Vec<String>,
    pressure: f64,
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
    for h in &cfg.hosts {
        fleet.add(
            HostId::new(&h.id),
            HostEndpoint {
                control_url: h.control_url.clone(),
                data_listen: h.data_listen.clone(),
                data_addr: h.data_addr.clone(),
            },
        );
    }
    let api = HttpDaemon;

    for step in 1..=cfg.max_steps {
        // Read the live fleet state: each host's pressure and its VMs.
        let mut placement = Placement::new();
        let mut pressure = BTreeMap::new();
        let mut idle = BTreeMap::new();
        for h in &cfg.hosts {
            let host = HostId::new(&h.id);
            let st = fetch_status(&h.control_url)
                .await
                .with_context(|| format!("status of {}", h.id))?;
            pressure.insert(host.clone(), Pressure::new(st.pressure));
            for vm in &st.vms {
                let id = vm.parse::<VmId>().with_context(|| format!("vm id {vm}"))?;
                placement.assign(host.clone(), id);
                // No live idle signal yet (that is the /proc-sampling step); treat
                // every VM as equally idle so the victim choice is deterministic.
                idle.insert(id, Duration::from_secs(60));
            }
        }

        match rebalance_once(
            &placement,
            &pressure,
            &idle,
            Pressure::new(cfg.watermark),
            &fleet,
            &api,
        )
        .await
        .context("rebalance step")?
        {
            Some(mv) => {
                println!(
                    "step {step}: migrated VM {} from {} to {}",
                    mv.vm, mv.from, mv.to
                );
                // Let the target finish restoring + registering before re-reading
                // the fleet, so the next step sees the new placement.
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            None => {
                println!("step {step}: converged (no host over watermark)");
                return Ok(());
            }
        }
    }
    println!("stopped after {} steps (max reached)", cfg.max_steps);
    Ok(())
}

/// The HTTP client side of [`DaemonApi`]: POSTs to the hostd control endpoints.
struct HttpDaemon;

impl DaemonApi for HttpDaemon {
    async fn migrate_recv(&self, control_url: &str, listen: &str) -> Result<(), String> {
        post(&format!("{control_url}/migrate/recv?listen={listen}")).await
    }
    async fn migrate_send(&self, control_url: &str, vm: &str, to: &str) -> Result<(), String> {
        post(&format!("{control_url}/migrate/send?vm={vm}&to={to}")).await
    }
}

/// Open an HTTP/1 connection to the host/port in `url`, returning the sender.
async fn connect(
    url: &hyper::Uri,
) -> Result<hyper::client::conn::http1::SendRequest<Empty<Bytes>>, String> {
    let host = url.host().ok_or_else(|| format!("no host in {url}"))?;
    let port = url.port_u16().unwrap_or(80);
    let stream = tokio::net::TcpStream::connect((host, port))
        .await
        .map_err(|e| format!("connect {host}:{port}: {e}"))?;
    let (sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .map_err(|e| format!("handshake: {e}"))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(sender)
}

/// GET `url`/status and parse the body.
async fn fetch_status(control_url: &str) -> Result<StatusResp> {
    let url = format!("{control_url}/status");
    let uri: hyper::Uri = url.parse().with_context(|| format!("bad url {url}"))?;
    let mut sender = connect(&uri).await.map_err(anyhow::Error::msg)?;
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    let req = hyper::Request::builder()
        .method(hyper::Method::GET)
        .uri(path)
        .header(hyper::header::HOST, uri.host().unwrap_or(""))
        .body(Empty::<Bytes>::new())
        .context("build request")?;
    let resp = sender.send_request(req).await.context("send")?;
    let body = resp.into_body().collect().await.context("body")?.to_bytes();
    serde_json::from_slice(&body).context("parse status json")
}

/// POST `url` with an empty body; Ok on a 2xx, Err otherwise.
async fn post(url: &str) -> Result<(), String> {
    let uri: hyper::Uri = url.parse().map_err(|e| format!("bad url {url}: {e}"))?;
    let mut sender = connect(&uri).await?;
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    let req = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri(path)
        .header(hyper::header::HOST, uri.host().unwrap_or(""))
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
