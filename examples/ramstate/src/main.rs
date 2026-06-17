//! `ramstate` — a minimal in-RAM stateful HTTP server (see [`ramstate`] lib).
//!
//! Single-threaded, std-only, one connection at a time — enough for manual
//! `curl` verification. Listens on `0.0.0.0:$PORT` (default 8000). Runs inside
//! the guest as a child of `guestd` (wrap mode); its memory is what a migration
//! must preserve.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ramstate::{State, busy_secs, handle, parse_request_line};

fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8000);
    // boot_id: seconds-since-epoch captured once, now. Stored in RAM, so it is
    // identical after a migration and different on a fresh start.
    let boot_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let state = State::new(boot_id);

    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ramstate: bind 0.0.0.0:{port}: {e}");
            std::process::exit(1);
        }
    };
    println!("ramstate: listening on 0.0.0.0:{port} boot_id={boot_id}");

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut buf = [0u8; 1024];
        let n = match stream.read(&mut buf) {
            Ok(0) | Err(_) => continue,
            Ok(n) => n,
        };
        let req = String::from_utf8_lossy(&buf[..n]);
        let first = req.lines().next().unwrap_or("");
        let (status, body) = match parse_request_line(first) {
            Some((method, path)) => {
                // POST /busy?secs=N stalls the single-threaded loop for N seconds
                // before replying, so a concurrent idle-probe times out and
                // migrate-when-idle treats it as a turn-in-progress, not a gap.
                if method == "POST"
                    && path.split('?').next() == Some("/busy")
                    && let Some(secs) = busy_secs(&path)
                {
                    sleep(Duration::from_secs(secs));
                }
                handle(&state, &method, &path)
            }
            None => (400, "{\"error\":\"bad request\"}".to_owned()),
        };
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            _ => "Error",
        };
        let resp = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes());
    }
}
