//! `hostd` — the per-host daemon.
//!
//! Long-lived: started once per server (under systemd), it owns that host's VMs
//! and exposes an HTTP surface. This first slice is the observable skeleton —
//!
//!   GET /healthz   liveness ("ok")
//!   GET /metrics   Prometheus exposition (scraped by Prometheus, shown in Grafana)
//!
//! Migration control endpoints (drive a send/receive, managed VM lifecycle) land
//! next; the point of the daemon is that migrations happen through it, with no
//! per-migration process spawning or `pkill` — it runs continuously and is
//! reaped only by its service manager.
//!
//! Usage: `hostd daemon <listen_addr>`   (e.g. `hostd daemon 0.0.0.0:8080`)

use std::convert::Infallible;
use std::net::SocketAddr;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::net::TcpListener;

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
            if let Err(e) = rt.block_on(daemon(addr)) {
                eprintln!("hostd: {e}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("usage: hostd daemon <listen_addr>");
            std::process::exit(2);
        }
    }
}

async fn daemon(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Install the Prometheus recorder; serve its rendering from /metrics.
    let handle = hostd::telemetry::recorder()?;
    let listener = TcpListener::bind(addr).await?;
    println!("hostd daemon listening on http://{addr} (/healthz, /metrics)");

    loop {
        let (stream, _peer) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let handle = handle.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| route(req, handle.clone()));
            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                eprintln!("hostd: connection error: {e}");
            }
        });
    }
}

async fn route(
    req: Request<hyper::body::Incoming>,
    handle: PrometheusHandle,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let resp = match req.uri().path() {
        "/healthz" => text(StatusCode::OK, "ok\n"),
        "/metrics" => text(StatusCode::OK, &handle.render()),
        _ => text(StatusCode::NOT_FOUND, "not found\n"),
    };
    Ok(resp)
}

fn text(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(body.to_owned())))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from_static(b"error"))))
}
