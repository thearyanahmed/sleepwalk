//! Request handling and state for the `ramstate` demo workload.
//!
//! The whole point is the [`State`]: a counter, a request tally, a small log,
//! and a `boot_id` stamped once when the process starts. All of it lives only in
//! process memory. When the VM is migrated, the snapshot carries this memory to
//! the new host and the *same* process resumes — so the counter keeps climbing
//! and `boot_id` stays the same. A reset counter or a changed `boot_id` would
//! mean the process restarted and the RAM was lost; continuity is the proof the
//! migration preserved live memory.
//!
//! The socket I/O lives in `main.rs`; everything here is pure and unit-tested.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The in-RAM state whose survival across a migration we are proving.
pub struct State {
    /// Bumped by `POST /inc`. Must continue across a move, never reset.
    pub counter: AtomicU64,
    /// Total requests handled — another monotonic in-RAM value.
    pub requests: AtomicU64,
    /// Stamped once at process start; identical before and after a migration,
    /// different if the process ever restarted.
    pub boot_id: u64,
    /// The last few actions, newest last — a heap structure that must travel too.
    pub log: Mutex<Vec<String>>,
}

impl State {
    /// Fresh state for a process that started with `boot_id`.
    #[must_use]
    pub fn new(boot_id: u64) -> Self {
        Self {
            counter: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            boot_id,
            log: Mutex::new(Vec::new()),
        }
    }
}

/// Handle a request, mutating state, and return `(status, json_body)`.
///
/// Routes: `POST /inc` bumps the counter; `GET /state` (or `GET /`) reports it.
pub fn handle(state: &State, method: &str, path: &str) -> (u16, String) {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let path = path.split('?').next().unwrap_or(path);
    match (method, path) {
        ("POST", "/inc") => {
            let n = state.counter.fetch_add(1, Ordering::Relaxed) + 1;
            push_log(state, format!("inc -> {n}"));
            (200, compact_json(state)) // just the new value, not the whole log
        }
        ("GET", "/state" | "/") => (200, state_json(state)),
        _ => (404, "{\"error\":\"not found\"}".to_owned()),
    }
}

/// A compact one-line view: boot id + counter, no log. The `/inc` response, so a
/// tight client loop isn't flooded with the recent-entries list.
#[must_use]
pub fn compact_json(state: &State) -> String {
    format!(
        "{{\"boot_id\":{},\"counter\":{}}}",
        state.boot_id,
        state.counter.load(Ordering::Relaxed)
    )
}

/// Append to the log, keeping only the most recent entries (bounded memory).
fn push_log(state: &State, entry: String) {
    if let Ok(mut log) = state.log.lock() {
        log.push(entry);
        if log.len() > 20 {
            let drop = log.len() - 20;
            log.drain(0..drop);
        }
    }
}

/// Render the current state as JSON.
#[must_use]
pub fn state_json(state: &State) -> String {
    let counter = state.counter.load(Ordering::Relaxed);
    let requests = state.requests.load(Ordering::Relaxed);
    let recent = state.log.lock().map(|l| l.clone()).unwrap_or_default();
    let recent: Vec<String> = recent
        .iter()
        .map(|e| format!("\"{}\"", e.replace('"', "'")))
        .collect();
    format!(
        "{{\"boot_id\":{},\"counter\":{counter},\"requests\":{requests},\"recent\":[{}]}}",
        state.boot_id,
        recent.join(",")
    )
}

/// Parse the method and path out of an HTTP request line (`GET /x HTTP/1.1`).
#[must_use]
pub fn parse_request_line(line: &str) -> Option<(String, String)> {
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    Some((method, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_line() {
        assert_eq!(
            parse_request_line("GET /state HTTP/1.1"),
            Some(("GET".to_owned(), "/state".to_owned()))
        );
        assert_eq!(parse_request_line(""), None);
    }

    #[test]
    fn inc_advances_counter_and_state_reflects_it() {
        let s = State::new(42);
        let (code, inc_body) = handle(&s, "POST", "/inc");
        assert_eq!(code, 200);
        // /inc is compact: counter + boot, but NOT the recent log.
        assert!(inc_body.contains("\"counter\":1"), "{inc_body}");
        assert!(inc_body.contains("\"boot_id\":42"), "{inc_body}");
        assert!(
            !inc_body.contains("recent"),
            "/inc must be compact: {inc_body}"
        );
        // /state carries the full view including the recent log.
        let (code, body) = handle(&s, "GET", "/state");
        assert_eq!(code, 200);
        assert!(body.contains("\"counter\":1"), "{body}");
        assert!(body.contains("\"recent\":[\"inc -> 1\"]"), "{body}");
    }

    #[test]
    fn unknown_route_is_404() {
        let s = State::new(1);
        assert_eq!(handle(&s, "GET", "/nope").0, 404);
    }

    #[test]
    fn query_string_is_ignored_for_routing() {
        let s = State::new(1);
        assert_eq!(handle(&s, "GET", "/state?x=1").0, 200);
    }

    #[test]
    fn log_stays_bounded() {
        let s = State::new(1);
        for _ in 0..30 {
            handle(&s, "POST", "/inc");
        }
        let len = s.log.lock().map(|l| l.len()).unwrap_or(0);
        assert!(len <= 20, "log grew unbounded: {len}");
    }
}
