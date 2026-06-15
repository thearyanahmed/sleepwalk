//! Host CPU/TSC compatibility classes — part of the host-status contract.
//!
//! A Firecracker snapshot is only restorable on a host whose CPU is compatible
//! with the one that took it. The hard blocker in practice is the **TSC
//! frequency**: on restore Firecracker rescales the guest TSC via
//! `KVM_SET_TSC_KHZ`, which needs hardware TSC scaling on the target; if source
//! and host TSC differ by more than ~250 ppm and the target lacks scaling, the
//! restore fails with an opaque `EINVAL`. CPU *features* are a second axis
//! (handled by CPU templates) and never cross vendors.
//!
//! [`CompatClass`] captures the axes that decide restore compatibility so the
//! rebalancer can refuse a cross-class move *before* attempting it. The
//! comparison ([`compatible_with`](CompatClass::compatible_with)) is pure and
//! testable; hosts fill it in from the live system (see `hostd`'s detector).

use serde::{Deserialize, Serialize};

/// Firecracker rescales the guest TSC only past this tolerance (parts per
/// million); within it, source and host TSC are treated as the same frequency,
/// so no hardware TSC scaling is needed. Matches Firecracker's own threshold.
const TSC_TOLERANCE_PPM: u64 = 250;

/// The CPU/host axes that decide whether a snapshot taken on one host can be
/// restored on another. Two hosts are compatible iff [`compatible_with`] holds.
///
/// [`compatible_with`]: CompatClass::compatible_with
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatClass {
    /// CPU vendor string (`GenuineIntel` / `AuthenticAMD`). Never crossable.
    pub vendor: String,
    /// CPU model name. Different models can differ in features a template may not
    /// reconcile, so v0.1 requires an exact match.
    pub model: String,
    /// Measured TSC frequency in kHz — the migration-critical axis.
    pub tsc_khz: u32,
    /// Host kernel release (e.g. `6.1.155`); compared at major.minor tier.
    pub kernel: String,
}

impl CompatClass {
    /// Whether a snapshot taken on `self` can be restored on `other`.
    ///
    /// Same vendor and model, kernel matching at the major.minor tier, and TSC
    /// frequencies within [`TSC_TOLERANCE_PPM`]. The TSC check is the one that
    /// actually blocks restores in the field; the rest guard against feature and
    /// ABI drift a snapshot can encode.
    #[must_use]
    pub fn compatible_with(&self, other: &CompatClass) -> bool {
        self.vendor == other.vendor
            && self.model == other.model
            && kernel_tier(&self.kernel) == kernel_tier(&other.kernel)
            && tsc_within_tolerance(self.tsc_khz, other.tsc_khz)
    }

    /// A short, human-facing label for dashboards and logs, e.g.
    /// `GenuineIntel/DO-Premium-Intel/1995MHz/6.1`.
    #[must_use]
    pub fn label(&self) -> String {
        format!(
            "{}/{}/{}MHz/{}",
            self.vendor,
            self.model,
            self.tsc_khz / 1000,
            kernel_tier(&self.kernel)
        )
    }
}

/// True if two TSC frequencies are within [`TSC_TOLERANCE_PPM`] of each other.
fn tsc_within_tolerance(a: u32, b: u32) -> bool {
    let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
    let diff = u64::from(hi - lo);
    // diff/hi <= 250/1_000_000, in integers to avoid float rounding.
    diff.saturating_mul(1_000_000) <= u64::from(hi).saturating_mul(TSC_TOLERANCE_PPM)
}

/// The major.minor of a kernel release string (`6.1.155` -> `6.1`); the rest is
/// ignored so patch bumps don't split a class.
fn kernel_tier(release: &str) -> String {
    let mut parts = release.split('.');
    match (parts.next(), parts.next()) {
        (Some(major), Some(minor)) => {
            // minor may carry a suffix (`1-generic`); keep the leading digits.
            let minor: String = minor.chars().take_while(char::is_ascii_digit).collect();
            format!("{major}.{minor}")
        }
        (Some(major), None) => major.to_owned(),
        _ => release.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(vendor: &str, model: &str, tsc_khz: u32, kernel: &str) -> CompatClass {
        CompatClass {
            vendor: vendor.to_owned(),
            model: model.to_owned(),
            tsc_khz,
            kernel: kernel.to_owned(),
        }
    }

    #[test]
    fn identical_hosts_are_compatible() {
        let a = class("GenuineIntel", "Xeon", 1_995_000, "6.1.155");
        assert!(a.compatible_with(&a.clone()));
    }

    #[test]
    fn different_tsc_frequency_is_incompatible() {
        // The real incident: 1995 MHz vs 2100 MHz (~5%, far over 250 ppm).
        let a = class("GenuineIntel", "Xeon", 1_995_000, "6.1.155");
        let b = class("GenuineIntel", "Xeon", 2_100_000, "6.1.155");
        assert!(!a.compatible_with(&b));
    }

    #[test]
    fn tsc_within_250ppm_is_compatible() {
        // 250 ppm of 2_000_000 kHz is 500 kHz; 400 kHz apart stays compatible.
        let a = class("GenuineIntel", "Xeon", 2_000_000, "6.1.155");
        let b = class("GenuineIntel", "Xeon", 2_000_400, "6.1.155");
        assert!(a.compatible_with(&b));
        // 800 kHz apart exceeds the tolerance.
        let c = class("GenuineIntel", "Xeon", 2_000_800, "6.1.155");
        assert!(!a.compatible_with(&c));
    }

    #[test]
    fn vendor_mismatch_is_incompatible() {
        let intel = class("GenuineIntel", "Xeon", 2_000_000, "6.1.155");
        let amd = class("AuthenticAMD", "Xeon", 2_000_000, "6.1.155");
        assert!(!intel.compatible_with(&amd));
    }

    #[test]
    fn model_mismatch_is_incompatible() {
        let a = class("GenuineIntel", "Ice Lake", 2_000_000, "6.1.155");
        let b = class("GenuineIntel", "Cascade Lake", 2_000_000, "6.1.155");
        assert!(!a.compatible_with(&b));
    }

    #[test]
    fn kernel_patch_bumps_stay_in_class_but_minor_bumps_split() {
        let a = class("GenuineIntel", "Xeon", 2_000_000, "6.1.155");
        let patch = class("GenuineIntel", "Xeon", 2_000_000, "6.1.200");
        assert!(a.compatible_with(&patch)); // same 6.1 tier
        let minor = class("GenuineIntel", "Xeon", 2_000_000, "6.2.0");
        assert!(!a.compatible_with(&minor)); // 6.1 vs 6.2
    }

    #[test]
    fn label_is_compact_and_human_readable() {
        let a = class("GenuineIntel", "DO-Premium-Intel", 1_995_309, "6.1.155");
        assert_eq!(a.label(), "GenuineIntel/DO-Premium-Intel/1995MHz/6.1");
    }
}
