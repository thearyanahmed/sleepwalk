//! Metrics — the pull-based `/metrics` endpoint Prometheus scrapes.
//!
//! Uses the [`metrics`] facade so the rest of the code records numbers without
//! knowing about Prometheus; [`install_exporter`] wires those to a Prometheus
//! text endpoint served over HTTP (the daemon calls it once at startup, Grafana
//! reads Prometheus). Recording when no recorder is installed is a no-op, so the
//! library and its tests never depend on the exporter being up.

use std::net::SocketAddr;
use std::time::Duration;

use metrics::{
    Unit, counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram,
};
use metrics_exporter_prometheus::{BuildError, PrometheusBuilder, PrometheusHandle};

/// Migrations that completed successfully.
pub const MIGRATIONS_TOTAL: &str = "sleepwalk_migrations_total";
/// Migration attempts that failed (before a successful retry, or terminally).
pub const MIGRATION_FAILURES_TOTAL: &str = "sleepwalk_migration_failures_total";
/// Source freeze window in seconds: pause → snapshot → transfer-complete.
pub const FREEZE_WINDOW_SECONDS: &str = "sleepwalk_freeze_window_seconds";
/// Cumulative snapshot bytes moved across migrations.
pub const SNAPSHOT_BYTES_TOTAL: &str = "sleepwalk_snapshot_bytes_total";
/// Per-VM presence: a gauge labelled `vm_id`, `host`, `ip`, set to 1 while the
/// VM runs on this host and 0 once it leaves (migrated out or torn down). The
/// label set is what makes a migration visible: the same `vm_id` flips to a new
/// `host`/`ip`. Filter on `== 1` to list only VMs that are actually here.
pub const VM_INFO: &str = "sleepwalk_vm_info";
/// Test-side turns driven at a guest, labelled `vm_id` and `outcome` (`ok` or
/// `dropped`). This is the load generator's own view, not the VM's: `rate()` of
/// the `ok` series is the request rate the demo holds flat through a migration,
/// and the `dropped` series spikes by exactly the turns lost to the freeze.
pub const TEST_TURNS_TOTAL: &str = "sleepwalk_test_turns_total";

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
    describe_gauge!(
        VM_INFO,
        "VM presence on this host (1 = here), labelled by vm_id/host/ip"
    );
    describe_counter!(
        TEST_TURNS_TOTAL,
        "Load-generator turns by vm_id and outcome (ok/dropped)"
    );
}

/// Record one load-generator turn against `vm_id`: `ok` true if the guest
/// completed it, false if it was dropped (no completion in the deadline, or the
/// link was gone).
pub fn test_turn(vm_id: &str, ok: bool) {
    let outcome = if ok { "ok" } else { "dropped" };
    counter!(TEST_TURNS_TOTAL, "vm_id" => vm_id.to_owned(), "outcome" => outcome).increment(1);
}

/// Mark VM `vm_id` as present on `host` with address `ip` (set the gauge to 1).
/// Call on spawn and on restore-as-migration-target.
pub fn vm_present(vm_id: &str, host: &str, ip: &str) {
    gauge!(VM_INFO, "vm_id" => vm_id.to_owned(), "host" => host.to_owned(), "ip" => ip.to_owned())
        .set(1.0);
}

/// Mark VM `vm_id` as gone from `host` (set the gauge to 0): migrated out or torn
/// down. The series lingers at 0 so panels filtering `== 1` drop it cleanly.
pub fn vm_absent(vm_id: &str, host: &str, ip: &str) {
    gauge!(VM_INFO, "vm_id" => vm_id.to_owned(), "host" => host.to_owned(), "ip" => ip.to_owned())
        .set(0.0);
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
/// at `/metrics` (the exporter's own HTTP listener). Call once at startup.
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

/// Install the global Prometheus recorder and return a handle to render the
/// metrics text on demand — for serving `/metrics` from one's own HTTP server.
///
/// # Errors
/// If a recorder is already installed.
pub fn recorder() -> Result<PrometheusHandle, BuildError> {
    let handle = PrometheusBuilder::new().install_recorder()?;
    describe();
    Ok(handle)
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
        vm_present("vm-7", "a", "10.200.0.5");
        vm_absent("vm-9", "a", "10.200.0.6");
        test_turn("vm-7", true);
        test_turn("vm-7", false);

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
        // The presence gauge carries its labels through to the exposition, and a
        // present VM reads 1.
        assert!(text.contains(VM_INFO), "vm_info gauge:\n{text}");
        assert!(
            text.contains("vm_id=\"vm-7\"") && text.contains("ip=\"10.200.0.5\""),
            "vm_info labels:\n{text}"
        );
        // Test-turn counter splits by outcome.
        assert!(text.contains(TEST_TURNS_TOTAL), "test turns:\n{text}");
        assert!(
            text.contains("outcome=\"ok\"") && text.contains("outcome=\"dropped\""),
            "test-turn outcomes:\n{text}"
        );
    }
}
