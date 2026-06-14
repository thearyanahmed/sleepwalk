//! The measurement report: per-migration records in, markdown out.
//!
//! A run emits a [`RunReport`] as JSON (one object per run, carrying the
//! per-migration records, the turn-latency slices, and the idle-gap histogram);
//! [`render_markdown`] turns that JSON into the tables published in
//! `results/report.md` and the README. The two halves are split on purpose: the
//! JSON is the durable artifact (re-renderable, diffable, machine-readable for
//! plotting), and the markdown is a pure function of it.
//!
//! Every rendered number is printed next to its methodology — machine, pinned
//! `versions.toml` hash, run and warm-up counts — because a latency without the
//! conditions that produced it is not a measurement. The numeric fields are
//! stored in their base unit (microseconds, bytes) so no precision is lost in
//! the artifact; the renderer formats them for humans.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::recorder::LatencyStats;

/// The conditions a run was measured under. Printed beside every number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Methodology {
    /// Human description of the host (CPU, RAM, dev tier / path).
    pub machine: String,
    /// The `images/versions.toml` content hash the run pinned.
    pub versions_hash: String,
    /// Measured runs contributing to the numbers.
    pub runs: u32,
    /// Warm-up runs discarded before measuring.
    pub warmup_runs: u32,
}

/// One migration's measured cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationRecord {
    /// The VM that moved.
    pub vm: String,
    /// Source host.
    pub from: String,
    /// Target host.
    pub to: String,
    /// Freeze window — how long the VM was paused (the O2 headline). Microseconds.
    pub freeze_window_us: u64,
    /// End-to-end relocation time, intent to cut-over. Microseconds.
    pub e2e_us: u64,
    /// Bytes streamed to the target (memory + vmstate).
    pub bytes_moved: u64,
    /// Page faults served on the target during lazy restore.
    pub faults_served: u64,
}

/// A turn-latency slice: the distribution over some subset of turns (e.g. those
/// overlapping a migration vs. a clean baseline).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencySlice {
    /// What this slice covers ("clean", "migration-overlapping").
    pub label: String,
    /// Samples in the slice.
    pub count: u64,
    /// Median turn latency. Microseconds.
    pub p50_us: u64,
    /// 99th-percentile turn latency. Microseconds.
    pub p99_us: u64,
}

impl LatencySlice {
    /// Build a slice from recorded [`LatencyStats`] and a label.
    #[must_use]
    pub fn from_stats(label: impl Into<String>, stats: &LatencyStats) -> Self {
        Self {
            label: label.into(),
            count: stats.count,
            p50_us: dur_us(stats.p50),
            p99_us: dur_us(stats.p99),
        }
    }
}

/// One bucket of the idle-gap histogram: how many idle gaps fell at or below
/// `upper_ms` (and above the previous bucket's bound).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdleGapBucket {
    /// Inclusive upper bound of the bucket, in milliseconds.
    pub upper_ms: u64,
    /// Count of idle gaps in the bucket.
    pub count: u64,
}

/// The full report for one measurement run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReport {
    /// Conditions the run was measured under.
    pub methodology: Methodology,
    /// Per-migration cost records.
    pub migrations: Vec<MigrationRecord>,
    /// Turn latency with no migration overlapping (the baseline).
    pub latency_clean: LatencySlice,
    /// Turn latency for turns overlapping a migration (the O5 claim).
    pub latency_overlapping: LatencySlice,
    /// The idle-gap distribution — the instrument for real-workload gap study.
    pub idle_gap_buckets: Vec<IdleGapBucket>,
}

impl RunReport {
    /// Parse a report from its JSON artifact.
    ///
    /// # Errors
    /// Returns the `serde_json` error if the JSON is malformed or the shape does
    /// not match.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Serialize this report to pretty JSON (the durable run artifact).
    ///
    /// # Errors
    /// Returns the `serde_json` error if serialization fails.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// Render a report as the markdown published to `results/report.md`.
#[must_use]
pub fn render_markdown(report: &RunReport) -> String {
    let mut out = String::new();
    out.push_str("# sleepwalk measurement report\n\n");
    render_methodology(&mut out, &report.methodology);
    render_migrations(&mut out, &report.migrations);
    render_latency(&mut out, &report.latency_clean, &report.latency_overlapping);
    render_idle_gaps(&mut out, &report.idle_gap_buckets);
    out
}

fn render_methodology(out: &mut String, m: &Methodology) {
    out.push_str("## Methodology\n\n");
    out.push_str(&format!("- **Machine:** {}\n", m.machine));
    out.push_str(&format!("- **Versions hash:** `{}`\n", m.versions_hash));
    out.push_str(&format!(
        "- **Runs:** {} measured ({} warm-up discarded)\n\n",
        m.runs, m.warmup_runs
    ));
}

fn render_migrations(out: &mut String, records: &[MigrationRecord]) {
    out.push_str("## Migrations\n\n");
    if records.is_empty() {
        out.push_str("_No migrations recorded._\n\n");
        return;
    }
    out.push_str("| VM | From → To | Freeze (ms) | E2E (ms) | Bytes moved | Faults served |\n");
    out.push_str("|----|-----------|------------:|---------:|------------:|--------------:|\n");
    for r in records {
        out.push_str(&format!(
            "| {} | {} → {} | {} | {} | {} | {} |\n",
            r.vm,
            r.from,
            r.to,
            ms(r.freeze_window_us),
            ms(r.e2e_us),
            r.bytes_moved,
            r.faults_served,
        ));
    }
    // Freeze-window summary (the O2 headline) over all migrations.
    let freezes: Vec<u64> = records.iter().map(|r| r.freeze_window_us).collect();
    let (min, med, max) = min_median_max(&freezes);
    out.push_str(&format!(
        "\n**Freeze window** across {} migration(s): min {} ms · median {} ms · max {} ms.\n\n",
        records.len(),
        ms(min),
        ms(med),
        ms(max),
    ));
}

fn render_latency(out: &mut String, clean: &LatencySlice, overlapping: &LatencySlice) {
    out.push_str("## Turn latency: clean vs. migration-overlapping\n\n");
    out.push_str("| Slice | Samples | p50 (ms) | p99 (ms) |\n");
    out.push_str("|-------|--------:|---------:|---------:|\n");
    for s in [clean, overlapping] {
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            s.label,
            s.count,
            ms(s.p50_us),
            ms(s.p99_us),
        ));
    }
    let delta = i64::try_from(overlapping.p99_us).unwrap_or(i64::MAX)
        - i64::try_from(clean.p99_us).unwrap_or(i64::MAX);
    out.push_str(&format!(
        "\n**p99 delta** (overlapping − clean): {} ms.\n\n",
        ms_signed(delta),
    ));
}

fn render_idle_gaps(out: &mut String, buckets: &[IdleGapBucket]) {
    out.push_str("## Idle-gap distribution\n\n");
    if buckets.is_empty() {
        out.push_str("_No idle gaps recorded._\n");
        return;
    }
    out.push_str("| ≤ ms | Count |\n");
    out.push_str("|-----:|------:|\n");
    for b in buckets {
        out.push_str(&format!("| {} | {} |\n", b.upper_ms, b.count));
    }
}

/// Microseconds of a [`Duration`], saturating.
fn dur_us(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// Format microseconds as milliseconds with three decimals.
fn ms(us: u64) -> String {
    format!("{:.3}", us as f64 / 1000.0)
}

/// Format a signed microsecond delta as milliseconds with three decimals.
fn ms_signed(us: i64) -> String {
    format!("{:.3}", us as f64 / 1000.0)
}

/// Min, median, and max of a slice of values. The median is the lower-middle
/// element for an even count (no interpolation — these are coarse summaries).
/// Returns zeros for an empty slice.
fn min_median_max(values: &[u64]) -> (u64, u64, u64) {
    if values.is_empty() {
        return (0, 0, 0);
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let min = sorted[0];
    let max = sorted[sorted.len() - 1];
    let med = sorted[(sorted.len() - 1) / 2];
    (min, med, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RunReport {
        RunReport {
            methodology: Methodology {
                machine: "test-host (8 vCPU, 16 GB)".to_owned(),
                versions_hash: "abc123".to_owned(),
                runs: 10,
                warmup_runs: 2,
            },
            migrations: vec![
                MigrationRecord {
                    vm: "vm-1".to_owned(),
                    from: "host-a".to_owned(),
                    to: "host-b".to_owned(),
                    freeze_window_us: 8_000,
                    e2e_us: 1_200_000,
                    bytes_moved: 536_870_912,
                    faults_served: 4_096,
                },
                MigrationRecord {
                    vm: "vm-2".to_owned(),
                    from: "host-a".to_owned(),
                    to: "host-b".to_owned(),
                    freeze_window_us: 12_000,
                    e2e_us: 1_500_000,
                    bytes_moved: 1_073_741_824,
                    faults_served: 8_192,
                },
            ],
            latency_clean: LatencySlice {
                label: "clean".to_owned(),
                count: 1000,
                p50_us: 40_000,
                p99_us: 90_000,
            },
            latency_overlapping: LatencySlice {
                label: "migration-overlapping".to_owned(),
                count: 50,
                p50_us: 42_000,
                p99_us: 95_000,
            },
            idle_gap_buckets: vec![
                IdleGapBucket {
                    upper_ms: 100,
                    count: 12,
                },
                IdleGapBucket {
                    upper_ms: 1000,
                    count: 340,
                },
            ],
        }
    }

    /// The report round-trips through its JSON artifact byte-for-byte.
    #[test]
    fn run_report_round_trips_through_json() {
        let report = sample();
        let json = report.to_json().expect("serialize");
        let back = RunReport::from_json(&json).expect("deserialize");
        assert_eq!(report, back);
    }

    /// The markdown carries every section, the methodology, and key numbers in
    /// human units.
    #[test]
    fn markdown_renders_all_sections_with_numbers() {
        let md = render_markdown(&sample());
        assert!(md.contains("## Methodology"));
        assert!(md.contains("## Migrations"));
        assert!(md.contains("## Turn latency: clean vs. migration-overlapping"));
        assert!(md.contains("## Idle-gap distribution"));
        // Methodology numbers and versions hash are present.
        assert!(md.contains("`abc123`"));
        assert!(md.contains("10 measured (2 warm-up discarded)"));
        // Freeze windows render in ms: 8_000 us -> 8.000, 12_000 us -> 12.000.
        assert!(md.contains("8.000"), "freeze window vm-1:\n{md}");
        assert!(md.contains("12.000"), "freeze window vm-2:\n{md}");
        // Freeze summary: median of [8000, 12000] is the lower-middle = 8.000.
        assert!(md.contains("median 8.000 ms"), "freeze summary:\n{md}");
        // p99 delta: 95_000 - 90_000 = 5_000 us = 5.000 ms.
        assert!(md.contains("5.000 ms"), "p99 delta:\n{md}");
    }

    /// An empty report renders the placeholder sections rather than panicking.
    #[test]
    fn empty_report_renders_placeholders() {
        let mut r = sample();
        r.migrations.clear();
        r.idle_gap_buckets.clear();
        let md = render_markdown(&r);
        assert!(md.contains("_No migrations recorded._"));
        assert!(md.contains("_No idle gaps recorded._"));
    }

    #[test]
    fn min_median_max_handles_even_and_empty() {
        assert_eq!(min_median_max(&[]), (0, 0, 0));
        assert_eq!(min_median_max(&[5]), (5, 5, 5));
        // Even count: lower-middle median.
        assert_eq!(min_median_max(&[10, 20, 30, 40]), (10, 20, 40));
    }

    /// A slice built from recorded stats keeps p50/p99 and the count.
    #[test]
    fn latency_slice_from_stats() {
        let stats = LatencyStats {
            count: 5,
            p50: Duration::from_millis(40),
            p90: Duration::from_millis(80),
            p99: Duration::from_millis(95),
            p99_9: Duration::from_millis(99),
            max: Duration::from_millis(100),
        };
        let slice = LatencySlice::from_stats("clean", &stats);
        assert_eq!(slice.count, 5);
        assert_eq!(slice.p50_us, 40_000);
        assert_eq!(slice.p99_us, 95_000);
    }
}
