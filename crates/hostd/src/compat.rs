//! Detecting this host's [`CompatClass`](proto::CompatClass) from the live
//! system. The class type and the compatibility predicate live in `proto` (the
//! host-status contract); this module only fills it in for the host we run on.

pub use proto::CompatClass;

/// Detect this host's compatibility class.
///
/// On Linux/x86_64 (the hosts sleepwalk runs on) this reads the CPU vendor/model
/// from `/proc/cpuinfo`, the kernel release, and measures the TSC frequency.
/// Elsewhere (e.g. a macOS dev box) it returns a placeholder so the type is
/// still constructible for non-VM code paths.
#[must_use]
pub fn detect() -> CompatClass {
    detect_impl()
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn detect_impl() -> CompatClass {
    let (vendor, model) = cpu_vendor_model();
    CompatClass {
        vendor,
        model,
        tsc_khz: measure_tsc_khz(),
        kernel: kernel_release(),
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn cpu_vendor_model() -> (String, String) {
    let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let field = |key: &str| -> String {
        info.lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                (k.trim() == key).then(|| v.trim().to_owned())
            })
            .unwrap_or_else(|| "unknown".to_owned())
    };
    (field("vendor_id"), field("model name"))
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn kernel_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "unknown".to_owned())
}

/// Measure the TSC frequency by reading the counter across a fixed wall-clock
/// window. Self-contained — no dependence on the kernel boot log, which may have
/// rotated away on a long-running host.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn measure_tsc_khz() -> u32 {
    use std::time::{Duration, Instant};
    let start = Instant::now();
    // SAFETY: `_rdtsc` is a baseline x86_64 instruction with no preconditions;
    // reading the timestamp counter has no side effects.
    let c0 = unsafe { core::arch::x86_64::_rdtsc() };
    std::thread::sleep(Duration::from_millis(200));
    let c1 = unsafe { core::arch::x86_64::_rdtsc() };
    let elapsed = start.elapsed().as_secs_f64();
    if elapsed <= 0.0 {
        return 0;
    }
    ((c1.wrapping_sub(c0) as f64 / elapsed) / 1000.0).round() as u32
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn detect_impl() -> CompatClass {
    CompatClass {
        vendor: "unknown".to_owned(),
        model: "unknown".to_owned(),
        tsc_khz: 0,
        kernel: "unknown".to_owned(),
    }
}
