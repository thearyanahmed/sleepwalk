//! Metrics — the pull-based `/metrics` endpoint Prometheus scrapes.
//!
//! Uses the [`metrics`] facade so the rest of the code records numbers without
//! knowing about Prometheus; [`install_exporter`] wires those to a Prometheus
//! text endpoint served over HTTP (the daemon calls it once at startup, Grafana
//! reads Prometheus). Recording when no recorder is installed is a no-op, so the
//! library and its tests never depend on the exporter being up.

use std::net::SocketAddr;
use std::time::Duration;

use metrics::{Unit, counter, describe_counter, describe_histogram, histogram};
use metrics_exporter_prometheus::{BuildError, PrometheusBuilder};

/// Migrations that completed successfully.
pub const MIGRATIONS_TOTAL: &str = "sleepwalk_migrations_total";
/// Migration attempts that failed (before a successful retry, or terminally).
pub const MIGRATION_FAILURES_TOTAL: &str = "sleepwalk_migration_failures_total";
/// Source freeze window in seconds: pause → snapshot → transfer-complete.
pub const FREEZE_WINDOW_SECONDS: &str = "sleepwalk_freeze_window_seconds";
/// Cumulative snapshot bytes moved across migrations.
pub const SNAPSHOT_BYTES_TOTAL: &str = "sleepwalk_snapshot_bytes_total";

/// Register descriptions and units for the metrics. Idempotent; call at startup.
pub fn describe() {
    describe_counter!(MIGRATIONS_TOTAL, "Migrations completed successfully");
    describe_counter!(MIGRATION_FAILURES_TOTAL, "Migration attempts that failed");
    describe_histogram!(
        FREEZE_WINDOW_SECONDS,
        Unit::Seconds,
        "Source freeze window: pause to transfer-complete"
    );
    describe_counter!(
        SNAPSHOT_BYTES_TOTAL,
        Unit::Bytes,
        "Cumulative snapshot bytes moved"
    );
}

/// Record a successful migration's source cost.
pub fn migration_ok(freeze: Duration, bytes: u64) {
    counter!(MIGRATIONS_TOTAL).increment(1);
    histogram!(FREEZE_WINDOW_SECONDS).record(freeze.as_secs_f64());
    counter!(SNAPSHOT_BYTES_TOTAL).increment(bytes);
}

/// Record a failed migration attempt.
pub fn migration_failed() {
    counter!(MIGRATION_FAILURES_TOTAL).increment(1);
}

/// Install the global Prometheus recorder and serve the metrics text on `addr`
/// at `/metrics`. Call once at daemon startup, inside a tokio runtime.
///
/// # Errors
/// If a recorder is already installed or the listener cannot be set up.
pub fn install_exporter(addr: SocketAddr) -> Result<(), BuildError> {
    PrometheusBuilder::new()
        .with_http_listener(addr)
        .install()?;
    describe();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded metrics render into the Prometheus text exposition, by name.
    #[test]
    fn records_render_into_prometheus_text() {
        let handle = PrometheusBuilder::new()
            .install_recorder()
            .expect("install recorder");
        describe();
        migration_ok(Duration::from_millis(1500), 268_435_456);
        migration_ok(Duration::from_millis(1600), 268_435_456);
        migration_failed();

        let text = handle.render();
        assert!(
            text.contains(MIGRATIONS_TOTAL),
            "migrations counter:\n{text}"
        );
        assert!(text.contains(MIGRATION_FAILURES_TOTAL), "failures:\n{text}");
        assert!(
            text.contains(FREEZE_WINDOW_SECONDS),
            "freeze histogram:\n{text}"
        );
        assert!(text.contains(SNAPSHOT_BYTES_TOTAL), "bytes:\n{text}");
    }
}
