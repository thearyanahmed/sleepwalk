//! The real [`FirecrackerApi`] — Firecracker's HTTP API over its per-VM unix
//! socket.
//!
//! Each call opens a connection to the socket, sends one HTTP/1.1 request, and
//! checks the status: Firecracker answers `204 No Content` on success and `400`
//! (or a default error) with a JSON `{"fault_message": "..."}` body on failure.
//! Endpoints and bodies are taken from the Firecracker v1.16.0 API spec
//! (`firecracker.yaml`):
//!
//! | op       | method | path       | body                                |
//! |----------|--------|------------|-------------------------------------|
//! | boot     | PUT    | `/actions` | `{"action_type":"InstanceStart"}`   |
//! | pause    | PATCH  | `/vm`      | `{"state":"Paused"}`                |
//! | resume   | PATCH  | `/vm`      | `{"state":"Resumed"}`               |
//! | shutdown | PUT    | `/actions` | `{"action_type":"SendCtrlAltDel"}`  |
//!
//! `SendCtrlAltDel` is an x86-only graceful power-off in Firecracker; on aarch64
//! there is no equivalent and the host reaps the process instead. Process spawn
//! and teardown are hostd's job around this client (a later slice); `RealFc` is
//! purely the API surface.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;

use super::{FcError, FirecrackerApi};

/// A Firecracker control client bound to one VM's API socket.
#[derive(Clone, Debug)]
pub struct RealFc {
    socket: PathBuf,
}

impl RealFc {
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

    /// Send one request to Firecracker and map the outcome to [`FcError`].
    ///
    /// Any failure reaching or talking to the socket is [`FcError::Unreachable`];
    /// a non-204 response is [`FcError::Rejected`] carrying Firecracker's
    /// `fault_message`.
    async fn send(
        &self,
        method: Method,
        path: &'static str,
        body: &'static str,
        op: &'static str,
    ) -> Result<(), FcError> {
        let unreachable = |detail: String| FcError::Unreachable { op, detail };

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
            .body(Full::new(Bytes::from_static(body.as_bytes())))
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
        Err(FcError::Rejected {
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

impl FirecrackerApi for RealFc {
    async fn boot(&self) -> Result<(), FcError> {
        self.send(
            Method::PUT,
            "/actions",
            r#"{"action_type":"InstanceStart"}"#,
            "boot",
        )
        .await
    }

    async fn pause(&self) -> Result<(), FcError> {
        self.send(Method::PATCH, "/vm", r#"{"state":"Paused"}"#, "pause")
            .await
    }

    async fn resume(&self) -> Result<(), FcError> {
        self.send(Method::PATCH, "/vm", r#"{"state":"Resumed"}"#, "resume")
            .await
    }

    async fn shutdown(&self) -> Result<(), FcError> {
        self.send(
            Method::PUT,
            "/actions",
            r#"{"action_type":"SendCtrlAltDel"}"#,
            "shutdown",
        )
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
    /// replies with `response`. Returns the bound `RealFc` and the capture slot.
    async fn stub(response: &'static str) -> (RealFc, Arc<Mutex<Captured>>) {
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
        (RealFc::new(path), captured)
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
        // Build a 400 response carrying the fault JSON.
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
            FcError::Rejected { op, detail } => {
                assert_eq!(op, "boot");
                assert_eq!(detail, "cannot start: already running");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_socket_is_unreachable() {
        let fc = RealFc::new(temp_socket()); // never bound
        let err = fc.boot().await.expect_err("must be unreachable");
        assert!(matches!(err, FcError::Unreachable { op: "boot", .. }));
    }
}
