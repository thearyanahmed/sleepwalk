//! `hostd` — the per-host daemon.
//!
//! Long-lived: started once per server (under systemd), it owns that host's VMs
//! and exposes an HTTP surface. This first slice is the observable skeleton —
//!
//!   GET  /healthz                  liveness ("ok")
//!   GET  /metrics                  Prometheus exposition (scraped → Grafana)
//!   GET  /status                   this host's load + VMs (the rebalancer reads it)
//!   POST /vms/spawn?mib=MIB        boot a VM here and register it
//!   POST /migrate/send?to=IP:PORT  boot+snapshot a VM here, stream it to a peer
//!   POST /migrate/recv?listen=ADDR receive one migration, UFFD-restore + resume
//!
//! Migrations happen *through* the daemon: it runs continuously and is reaped
//! only by its service manager, so there is no per-migration process spawning or
//! `pkill`. The VM and migrate endpoints need Linux + `/dev/kvm`; elsewhere they
//! answer 501 so the daemon still builds and serves metrics on any platform.
//!
//! Usage: `hostd daemon <listen_addr> [host_id] [capacity_mib]`
//! (e.g. `hostd daemon 0.0.0.0:8080 a 768`)

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::net::TcpListener;

/// Shared daemon state handed to every request.
struct AppState {
    handle: PrometheusHandle,
    #[cfg(target_os = "linux")]
    registry: Arc<hostd::VmRegistry>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match (args.get(1).map(String::as_str), args.get(2)) {
        (Some("daemon"), Some(addr)) => {
            let addr: SocketAddr = match addr.parse() {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("hostd: bad listen address '{addr}': {e}");
                    std::process::exit(2);
                }
            };
            let host_id = args.get(3).cloned().unwrap_or_else(|| "host".to_owned());
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("hostd: tokio runtime: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = rt.block_on(daemon(addr, host_id)) {
                eprintln!("hostd: {e}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("usage: hostd daemon <listen_addr> [host_id]");
            std::process::exit(2);
        }
    }
}

async fn daemon(
    addr: SocketAddr,
    host_id: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Install the Prometheus recorder; serve its rendering from /metrics.
    let handle = hostd::telemetry::recorder()?;
    let state = Arc::new(AppState {
        handle,
        #[cfg(target_os = "linux")]
        registry: Arc::new(hostd::VmRegistry::new(proto::HostId::new(&host_id))),
    });
    let _ = &host_id; // used on Linux (registry); avoid unused warning elsewhere
    let listener = TcpListener::bind(addr).await?;
    println!("hostd daemon '{host_id}' listening on http://{addr} (/healthz, /metrics, /status)");

    loop {
        let (stream, _peer) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let service = service_fn(move |req| route(req, Arc::clone(&state)));
            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                eprintln!("hostd: connection error: {e}");
            }
        });
    }
}

async fn route(
    req: Request<hyper::body::Incoming>,
    state: Arc<AppState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let resp = match (req.method(), req.uri().path()) {
        (&Method::GET, "/healthz") => text(StatusCode::OK, "ok\n"),
        (&Method::GET, "/metrics") => text(StatusCode::OK, &state.handle.render()),
        (&Method::GET, "/status") => status(&state).await,
        (&Method::POST, "/vms/spawn") => vms_spawn(&req, &state).await,
        (&Method::POST, "/migrate/send") => migrate_send(&req, &state).await,
        (&Method::POST, "/migrate/recv") => migrate_recv(&req, &state).await,
        _ => text(StatusCode::NOT_FOUND, "not found\n"),
    };
    Ok(resp)
}

#[cfg(target_os = "linux")]
async fn status(state: &AppState) -> Response<Full<Bytes>> {
    let st = state.registry.status().await;
    match serde_json::to_string(&st) {
        Ok(json) => text(StatusCode::OK, &format!("{json}\n")),
        Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}\n")),
    }
}

#[cfg(target_os = "linux")]
async fn vms_spawn(
    req: &Request<hyper::body::Incoming>,
    state: &AppState,
) -> Response<Full<Bytes>> {
    let mib: u32 = query_param(req.uri().query(), "mib")
        .and_then(|s| s.parse().ok())
        .unwrap_or(128);
    let art = match hostd::discover_artifacts() {
        Ok(a) => a,
        Err(e) => return text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}\n")),
    };
    match state.registry.spawn(&art, mib).await {
        Ok(id) => text(StatusCode::OK, &format!("{{\"vm\":\"{id}\"}}\n")),
        Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}\n")),
    }
}

#[cfg(not(target_os = "linux"))]
async fn status(_state: &AppState) -> Response<Full<Bytes>> {
    text(StatusCode::NOT_IMPLEMENTED, "registry requires Linux\n")
}

#[cfg(not(target_os = "linux"))]
async fn vms_spawn(
    _req: &Request<hyper::body::Incoming>,
    _state: &AppState,
) -> Response<Full<Bytes>> {
    text(StatusCode::NOT_IMPLEMENTED, "registry requires Linux\n")
}

/// Extract a query parameter `key` from a `k=v&k2=v2` query string.
#[cfg(target_os = "linux")]
fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_owned())
    })
}

#[cfg(target_os = "linux")]
fn timing_json(t: &hostd::SourceTiming) -> String {
    serde_json::json!({
        "snapshot_ms": t.snapshot.as_secs_f64() * 1000.0,
        "transfer_ms": t.transfer.as_secs_f64() * 1000.0,
        "bytes": t.bytes,
    })
    .to_string()
}

#[cfg(target_os = "linux")]
async fn migrate_send(
    req: &Request<hyper::body::Incoming>,
    state: &AppState,
) -> Response<Full<Bytes>> {
    let query = req.uri().query();
    let Some(to) = query_param(query, "to") else {
        return text(StatusCode::BAD_REQUEST, "missing ?to=IP:PORT\n");
    };

    // ?vm=ID migrates that registered, running VM (the fleet path). Without it,
    // boot a fresh VM and send it (the standalone benchmark path).
    if let Some(vm) = query_param(query, "vm") {
        let id = match vm.parse::<proto::VmId>() {
            Ok(id) => id,
            Err(e) => return text(StatusCode::BAD_REQUEST, &format!("bad vm id: {e}\n")),
        };
        let Some(running) = state.registry.take(&id).await else {
            return text(StatusCode::NOT_FOUND, "no such vm here\n");
        };
        return match hostd::migrate_running(running, &to).await {
            Ok(hostd::MigrateOutcome::Moved(t)) => {
                text(StatusCode::OK, &format!("{}\n", timing_json(&t)))
            }
            Ok(hostd::MigrateOutcome::StoodDown(vm)) => {
                state.registry.insert(vm).await;
                text(StatusCode::CONFLICT, "guest busy; migration stood down\n")
            }
            Err(e) => {
                hostd::telemetry::migration_failed();
                text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}\n"))
            }
        };
    }

    let art = match hostd::discover_artifacts() {
        Ok(a) => a,
        Err(e) => return text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}\n")),
    };
    match hostd::migrate_source(&art, &to).await {
        Ok(t) => text(StatusCode::OK, &format!("{}\n", timing_json(&t))),
        Err(e) => {
            hostd::telemetry::migration_failed();
            text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}\n"))
        }
    }
}

#[cfg(target_os = "linux")]
async fn migrate_recv(
    req: &Request<hyper::body::Incoming>,
    state: &AppState,
) -> Response<Full<Bytes>> {
    let Some(listen) = query_param(req.uri().query(), "listen") else {
        return text(StatusCode::BAD_REQUEST, "missing ?listen=ADDR\n");
    };
    let art = match hostd::discover_artifacts() {
        Ok(a) => a,
        Err(e) => return text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}\n")),
    };
    let listener = match hostd::bind_receiver(&listen).await {
        Ok(l) => l,
        Err(e) => return text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}\n")),
    };
    // Return once we are listening; the restore runs in the background so the
    // caller (the rebalancer) can fire the source send next. The restored VM is
    // registered into this host's fleet; outcome is recorded to telemetry.
    let fc_bin = art.fc_bin.clone();
    let registry = std::sync::Arc::clone(&state.registry);
    tokio::spawn(async move {
        match hostd::restore_register(&fc_bin, &listener).await {
            Ok(vm) => registry.insert(vm).await,
            Err(e) => {
                hostd::telemetry::migration_failed();
                eprintln!("hostd: restore failed: {e}");
            }
        }
    });
    text(StatusCode::ACCEPTED, "receiving\n")
}

#[cfg(not(target_os = "linux"))]
async fn migrate_send(
    _req: &Request<hyper::body::Incoming>,
    _state: &AppState,
) -> Response<Full<Bytes>> {
    text(StatusCode::NOT_IMPLEMENTED, "migration requires Linux\n")
}

#[cfg(not(target_os = "linux"))]
async fn migrate_recv(
    _req: &Request<hyper::body::Incoming>,
    _state: &AppState,
) -> Response<Full<Bytes>> {
    text(StatusCode::NOT_IMPLEMENTED, "migration requires Linux\n")
}

fn text(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(body.to_owned())))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from_static(b"error"))))
}
