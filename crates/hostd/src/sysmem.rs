//! Host memory pressure, sampled from the kernel.
//!
//! The rebalancer balances on how loaded each host is; [`memory_pressure`] is
//! that signal, read live from `/proc/meminfo` as the fraction of RAM in use:
//! `(MemTotal - MemAvailable) / MemTotal`. `MemAvailable` (not `MemFree`) is the
//! kernel's own estimate of what a new workload could claim without swapping, so
//! it already discounts reclaimable cache — the right denominator for "can this
//! host take another VM".
//!
//! The parser is split out and pure so it is testable anywhere; only the read of
//! `/proc/meminfo` is Linux-specific (it returns `0.0` if the file is absent).

/// The fraction of host memory in use, in `[0, 1]`, from `/proc/meminfo`.
/// Returns `0.0` if the file cannot be read or parsed (e.g. off Linux).
#[must_use]
pub fn memory_pressure() -> f64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| parse_pressure(&s))
        .unwrap_or(0.0)
}

/// Parse the in-use fraction from `/proc/meminfo` contents. `None` if either
/// `MemTotal` or `MemAvailable` is missing or `MemTotal` is zero.
fn parse_pressure(meminfo: &str) -> Option<f64> {
    let mut total = None;
    let mut available = None;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available = parse_kb(rest);
        }
    }
    let total = total?;
    let available = available?;
    if total == 0 {
        return None;
    }
    Some(total.saturating_sub(available) as f64 / total as f64)
}

/// The first whitespace-separated integer in `s` (the kB value of a meminfo line).
fn parse_kb(s: &str) -> Option<u64> {
    s.split_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
MemTotal:        1000000 kB
MemFree:          100000 kB
MemAvailable:     250000 kB
Buffers:           20000 kB
";

    #[test]
    fn parses_in_use_fraction_from_available() {
        // (1_000_000 - 250_000) / 1_000_000 = 0.75 — uses MemAvailable, not MemFree.
        let p = parse_pressure(SAMPLE).expect("parses");
        assert!((p - 0.75).abs() < 1e-9, "got {p}");
    }

    #[test]
    fn missing_fields_yield_none() {
        assert!(parse_pressure("MemTotal: 1000 kB\n").is_none());
        assert!(parse_pressure("MemAvailable: 500 kB\n").is_none());
        assert!(parse_pressure("").is_none());
    }

    #[test]
    fn zero_total_is_none() {
        assert!(parse_pressure("MemTotal: 0 kB\nMemAvailable: 0 kB\n").is_none());
    }

    #[test]
    fn full_and_empty_extremes() {
        assert_eq!(
            parse_pressure("MemTotal: 100 kB\nMemAvailable: 0 kB\n"),
            Some(1.0)
        );
        assert_eq!(
            parse_pressure("MemTotal: 100 kB\nMemAvailable: 100 kB\n"),
            Some(0.0)
        );
    }
}
