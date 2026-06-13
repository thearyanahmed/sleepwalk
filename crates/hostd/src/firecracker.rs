//! The Firecracker control port and its real implementation.
//!
//! [`FirecrackerApi`] is the small trait every external Firecracker effect goes
//! through. [`Firecracker`] is the real implementation: it drives Firecracker's
//! HTTP API over the per-VM unix socket. The test stand-in lives next door in
//! [`crate::pseudo_firecracker`].
//!
//! Each implementor is bound to exactly one VM (it owns that VM's socket path),
//! so the methods take no VM argument.
//!
//! Endpoints and bodies are taken from the Firecracker v1.16.0 API spec
//! (`firecracker.yaml`):
//!
//! | op              | method | path               | body                                |
//! |-----------------|--------|--------------------|-------------------------------------|
//! | boot            | PUT    | `/actions`         | `{"action_type":"InstanceStart"}`   |
//! | pause           | PATCH  | `/vm`              | `{"state":"Paused"}`                |
//! | resume          | PATCH  | `/vm`              | `{"state":"Resumed"}`               |
//! | shutdown        | PUT    | `/actions`         | `{"action_type":"SendCtrlAltDel"}`  |
//! | create_snapshot | PUT    | `/snapshot/create` | `{mem_file_path, snapshot_path, snapshot_type:"Full"}` |
//! | load_snapshot   | PUT    | `/snapshot/load`   | `{snapshot_path, mem_backend{backend_type,backend_path}, resume_vm}` |
//!
//! Firecracker answers `204 No Content` on success and `400` (or a default
//! error) with a JSON `{"fault_message": "..."}` body on failure.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use thiserror::Error;
use tokio::net::UnixStream;

/// An error from a single Firecracker control operation.
#[derive(Debug, Error)]
pub enum FirecrackerError {
    /// The operation reached Firecracker but it rejected or failed it. `detail`
    /// carries enough to debug from a log line.
    #[error("firecracker rejected {op}: {detail}")]
    Rejected {
        /// The operation that failed (`boot`, `pause`, …).
        op: &'static str,
        /// Firecracker's error detail.
        detail: String,
    },

    /// Firecracker was unreachable (socket gone, process dead, I/O error).
    #[error("firecracker unreachable for {op}: {detail}")]
    Unreachable {
        /// The operation being attempted.
        op: &'static str,
        /// What went wrong reaching it.
        detail: String,
    },
}

/// Where a snapshot's two files are written (`PUT /snapshot/create`). The VM
/// must be paused first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotTarget {
    /// Destination for the guest memory dump (`mem_file_path`).
    pub mem_file: PathBuf,
    /// Destination for the VM state file (`snapshot_path`).
    pub state_file: PathBuf,
}

/// How memory is supplied when restoring a snapshot (`mem_backend`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemBackend {
    /// Restore memory eagerly from the snapshot's memory file.
    File {
        /// The memory file to read (`backend_path`).
        mem_file: PathBuf,
    },
    /// Serve memory lazily over a UFFD socket — the page server listens there
    /// (`backend_path`). This is the lazy-restore path.
    Uffd {
        /// The UFFD socket the page server is bound to.
        socket: PathBuf,
    },
}

/// Inputs for restoring a snapshot (`PUT /snapshot/load`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotSource {
    /// The VM state file to load (`snapshot_path`).
    pub state_file: PathBuf,
    /// How guest memory is provided.
    pub backend: MemBackend,
    /// Whether to resume the VM immediately after loading (`resume_vm`).
    pub resume: bool,
}

/// The control surface hostd drives for one microVM.
///
/// The lifecycle operations map onto Firecracker's API (see the module docs).
/// `shutdown` issues `SendCtrlAltDel`, an x86-only graceful power-off; on
/// aarch64 there is no equivalent and the host reaps the process instead.
/// Process spawn and teardown are hostd's job around this client (a later
/// slice); the implementations here are purely the API surface.
pub trait FirecrackerApi {
    /// Start the configured guest (boot the kernel).
    fn boot(&self) -> impl std::future::Future<Output = Result<(), FirecrackerError>> + Send;
    /// Pause the VM (vCPUs stopped); prerequisite for snapshotting.
    fn pause(&self) -> impl std::future::Future<Output = Result<(), FirecrackerError>> + Send;
    /// Resume a paused VM.
    fn resume(&self) -> impl std::future::Future<Output = Result<(), FirecrackerError>> + Send;
    /// Stop the VM and its Firecracker process.
    fn shutdown(&self) -> impl std::future::Future<Output = Result<(), FirecrackerError>> + Send;
    /// Snapshot the paused VM to the given files.
    fn create_snapshot(
        &self,
        target: SnapshotTarget,
    ) -> impl std::future::Future<Output = Result<(), FirecrackerError>> + Send;
    /// Restore a VM from a snapshot, optionally resuming it.
    fn load_snapshot(
        &self,
        source: SnapshotSource,
    ) -> impl std::future::Future<Output = Result<(), FirecrackerError>> + Send;
}

/// The real control client, bound to one VM's API socket.
#[derive(Clone, Debug)]
pub struct Firecracker {
    socket: PathBuf,
}

impl Firecracker {
    /// Bind to the Firecracker API unix socket at `socket` (typically
    /// [`crate::statedir::VmDir::api_socket`]).
    #[must_use]
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// The API socket path.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Send one request to Firecracker and map the outcome to
    /// [`FirecrackerError`].
    ///
    /// Any failure reaching or talking to the socket is
    /// [`FirecrackerError::Unreachable`]; a non-204 response is
    /// [`FirecrackerError::Rejected`] carrying Firecracker's `fault_message`.
    async fn send(
        &self,
        method: Method,
        path: &'static str,
        body: Bytes,
        op: &'static str,
    ) -> Result<(), FirecrackerError> {
        let unreachable = |detail: String| FirecrackerError::Unreachable { op, detail };

        let stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|e| unreachable(format!("connect {}: {e}", self.socket.display())))?;

        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| unreachable(format!("handshake: {e}")))?;
        // Drive the connection; it ends when this request's response is done.
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(hyper::header::HOST, "localhost")
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(Full::new(body))
            .map_err(|e| unreachable(format!("build request: {e}")))?;

        let resp = sender
            .send_request(req)
            .await
            .map_err(|e| unreachable(format!("send: {e}")))?;

        let status = resp.status();
        if status == hyper::StatusCode::NO_CONTENT {
            return Ok(());
        }

        let bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| unreachable(format!("read response body: {e}")))?
            .to_bytes();
        Err(FirecrackerError::Rejected {
            op,
            detail: fault_message(&bytes, status.as_u16()),
        })
    }
}

/// Pull Firecracker's `fault_message` out of an error body, falling back to the
/// raw body or just the status code.
fn fault_message(body: &[u8], status: u16) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("fault_message")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| {
            if body.is_empty() {
                format!("HTTP {status}")
            } else {
                format!("HTTP {status}: {}", String::from_utf8_lossy(body))
            }
        })
}

/// Serialize a JSON value into a request body, mapping a serialize failure to a
/// (practically unreachable) error tagged with `op`.
fn json_body(value: &serde_json::Value, op: &'static str) -> Result<Bytes, FirecrackerError> {
    serde_json::to_vec(value)
        .map(Bytes::from)
        .map_err(|e| FirecrackerError::Unreachable {
            op,
            detail: format!("serialize body: {e}"),
        })
}

/// Render a path for a JSON string field (lossy on the rare non-UTF-8 path).
fn path_str(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}

impl FirecrackerApi for Firecracker {
    async fn boot(&self) -> Result<(), FirecrackerError> {
        self.send(
            Method::PUT,
            "/actions",
            Bytes::from_static(br#"{"action_type":"InstanceStart"}"#),
            "boot",
        )
        .await
    }

    async fn pause(&self) -> Result<(), FirecrackerError> {
        self.send(
            Method::PATCH,
            "/vm",
            Bytes::from_static(br#"{"state":"Paused"}"#),
            "pause",
        )
        .await
    }

    async fn resume(&self) -> Result<(), FirecrackerError> {
        self.send(
            Method::PATCH,
            "/vm",
            Bytes::from_static(br#"{"state":"Resumed"}"#),
            "resume",
        )
        .await
    }

    async fn shutdown(&self) -> Result<(), FirecrackerError> {
        self.send(
            Method::PUT,
            "/actions",
            Bytes::from_static(br#"{"action_type":"SendCtrlAltDel"}"#),
            "shutdown",
        )
        .await
    }

    async fn create_snapshot(&self, target: SnapshotTarget) -> Result<(), FirecrackerError> {
        let body = json_body(
            &serde_json::json!({
                "mem_file_path": path_str(&target.mem_file),
                "snapshot_path": path_str(&target.state_file),
                "snapshot_type": "Full",
            }),
            "create_snapshot",
        )?;
        self.send(Method::PUT, "/snapshot/create", body, "create_snapshot")
            .await
    }

    async fn load_snapshot(&self, source: SnapshotSource) -> Result<(), FirecrackerError> {
        let (backend_type, backend_path) = match &source.backend {
            MemBackend::File { mem_file } => ("File", path_str(mem_file)),
            MemBackend::Uffd { socket } => ("Uffd", path_str(socket)),
        };
        let body = json_body(
            &serde_json::json!({
                "snapshot_path": path_str(&source.state_file),
                "mem_backend": { "backend_type": backend_type, "backend_path": backend_path },
                "resume_vm": source.resume,
            }),
            "load_snapshot",
        )?;
        self.send(Method::PUT, "/snapshot/load", body, "load_snapshot")
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;
    use tokio::sync::Mutex;

    use super::*;

    /// What a stub server captured from the one request it served.
    #[derive(Default)]
    struct Captured {
        method: String,
        path: String,
        body: String,
    }

    /// A throwaway unix-socket path under the temp dir.
    fn temp_socket() -> PathBuf {
        std::env::temp_dir().join(format!("sleepwalk-fc-{}.sock", uuid::Uuid::new_v4()))
    }

    /// Read one HTTP/1.1 request fully (headers + Content-Length body).
    async fn read_request(stream: &mut UnixStream) -> (String, String, String) {
        let mut data = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = stream.read(&mut tmp).await.expect("read");
            if n == 0 {
                break;
            }
            data.extend_from_slice(&tmp[..n]);
            if let Some(hdr_end) = find(&data, b"\r\n\r\n") {
                let headers = &data[..hdr_end];
                let want = content_length(headers);
                if data.len() - (hdr_end + 4) >= want {
                    break;
                }
            }
        }
        let hdr_end = find(&data, b"\r\n\r\n").expect("headers");
        let line_end = find(&data, b"\r\n").expect("request line");
        let line = String::from_utf8_lossy(&data[..line_end]);
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let path = parts.next().unwrap_or_default().to_owned();
        let body = String::from_utf8_lossy(&data[hdr_end + 4..]).into_owned();
        (method, path, body)
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn content_length(headers: &[u8]) -> usize {
        let text = String::from_utf8_lossy(headers).to_lowercase();
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("content-length:") {
                return v.trim().parse().unwrap_or(0);
            }
        }
        0
    }

    /// Spawn a stub that accepts one connection, records the request, and
    /// replies with `response`. Returns the bound client and the capture slot.
    async fn stub(response: &'static str) -> (Firecracker, Arc<Mutex<Captured>>) {
        let path = temp_socket();
        let listener = UnixListener::bind(&path).expect("bind");
        let captured = Arc::new(Mutex::new(Captured::default()));
        let cap = Arc::clone(&captured);
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let (method, p, body) = read_request(&mut stream).await;
            {
                let mut c = cap.lock().await;
                c.method = method;
                c.path = p;
                c.body = body;
            }
            stream.write_all(response.as_bytes()).await.expect("write");
            stream.flush().await.expect("flush");
        });
        (Firecracker::new(path), captured)
    }

    const OK_204: &str = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";

    #[tokio::test]
    async fn boot_sends_instance_start_and_succeeds_on_204() {
        let (fc, cap) = stub(OK_204).await;
        fc.boot().await.expect("boot ok");
        let c = cap.lock().await;
        assert_eq!(c.method, "PUT");
        assert_eq!(c.path, "/actions");
        assert_eq!(c.body, r#"{"action_type":"InstanceStart"}"#);
    }

    #[tokio::test]
    async fn pause_sends_patch_vm_paused() {
        let (fc, cap) = stub(OK_204).await;
        fc.pause().await.expect("pause ok");
        let c = cap.lock().await;
        assert_eq!(c.method, "PATCH");
        assert_eq!(c.path, "/vm");
        assert_eq!(c.body, r#"{"state":"Paused"}"#);
    }

    #[tokio::test]
    async fn resume_sends_patch_vm_resumed() {
        let (fc, cap) = stub(OK_204).await;
        fc.resume().await.expect("resume ok");
        let c = cap.lock().await;
        assert_eq!(c.body, r#"{"state":"Resumed"}"#);
    }

    #[tokio::test]
    async fn shutdown_sends_send_ctrl_alt_del() {
        let (fc, cap) = stub(OK_204).await;
        fc.shutdown().await.expect("shutdown ok");
        let c = cap.lock().await;
        assert_eq!(c.path, "/actions");
        assert_eq!(c.body, r#"{"action_type":"SendCtrlAltDel"}"#);
    }

    #[tokio::test]
    async fn fault_response_becomes_rejected_with_message() {
        let body = r#"{"fault_message":"cannot start: already running"}"#;
        let resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .into_boxed_str(),
        );
        let (fc, _cap) = stub(resp).await;
        let err = fc.boot().await.expect_err("must be rejected");
        match err {
            FirecrackerError::Rejected { op, detail } => {
                assert_eq!(op, "boot");
                assert_eq!(detail, "cannot start: already running");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_socket_is_unreachable() {
        let fc = Firecracker::new(temp_socket()); // never bound
        let err = fc.boot().await.expect_err("must be unreachable");
        assert!(matches!(
            err,
            FirecrackerError::Unreachable { op: "boot", .. }
        ));
    }

    #[tokio::test]
    async fn create_snapshot_sends_paths_and_full_type() {
        let (fc, cap) = stub(OK_204).await;
        fc.create_snapshot(SnapshotTarget {
            mem_file: PathBuf::from("/snap/mem"),
            state_file: PathBuf::from("/snap/state"),
        })
        .await
        .expect("create ok");

        let c = cap.lock().await;
        assert_eq!(c.method, "PUT");
        assert_eq!(c.path, "/snapshot/create");
        let body: serde_json::Value = serde_json::from_str(&c.body).expect("json body");
        assert_eq!(body["mem_file_path"], "/snap/mem");
        assert_eq!(body["snapshot_path"], "/snap/state");
        assert_eq!(body["snapshot_type"], "Full");
    }

    #[tokio::test]
    async fn load_snapshot_with_uffd_backend_requests_lazy_restore() {
        let (fc, cap) = stub(OK_204).await;
        fc.load_snapshot(SnapshotSource {
            state_file: PathBuf::from("/snap/state"),
            backend: MemBackend::Uffd {
                socket: PathBuf::from("/run/uffd.sock"),
            },
            resume: true,
        })
        .await
        .expect("load ok");

        let c = cap.lock().await;
        assert_eq!(c.path, "/snapshot/load");
        let body: serde_json::Value = serde_json::from_str(&c.body).expect("json body");
        assert_eq!(body["snapshot_path"], "/snap/state");
        assert_eq!(body["mem_backend"]["backend_type"], "Uffd");
        assert_eq!(body["mem_backend"]["backend_path"], "/run/uffd.sock");
        assert_eq!(body["resume_vm"], true);
    }

    #[tokio::test]
    async fn load_snapshot_with_file_backend_uses_mem_file() {
        let (fc, cap) = stub(OK_204).await;
        fc.load_snapshot(SnapshotSource {
            state_file: PathBuf::from("/snap/state"),
            backend: MemBackend::File {
                mem_file: PathBuf::from("/snap/mem"),
            },
            resume: false,
        })
        .await
        .expect("load ok");

        let c = cap.lock().await;
        let body: serde_json::Value = serde_json::from_str(&c.body).expect("json body");
        assert_eq!(body["mem_backend"]["backend_type"], "File");
        assert_eq!(body["mem_backend"]["backend_path"], "/snap/mem");
        assert_eq!(body["resume_vm"], false);
    }
}
