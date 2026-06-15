//! `hostd` — the per-host daemon.
//!
//! Runs Firecracker microVMs on one host: drives their lifecycle, serves their
//! memory pages on restore (UFFD), and moves snapshots between hosts. This first
//! slice is the host-agnostic core:
//!
//! - [`firecracker::FirecrackerApi`] — the control port every Firecracker effect
//!   goes through, with the real [`firecracker::Firecracker`] (HTTP over the
//!   per-VM unix socket) and a recording [`pseudo_firecracker::PseudoFirecracker`]
//!   for tests.
//! - [`vm::Vm`] — the lifecycle orchestrator (boot / pause / resume / shutdown)
//!   that enforces a legal operation order.
//! - [`statedir::VmDir`] — the per-VM on-disk layout and jailer chroot target.
//! - [`drain::DrainCoordinator`] — folds the guest's wire signals plus locally
//!   sampled infra/storage state into a [`drain::DrainVerdict`] (the host half
//!   of the drain protocol; verified, not assumed).
//! - `uffd::PageFaultServer` (Linux only) — serves guest-memory page faults from
//!   the snapshot file on demand, the core of lazy restore.
//!
//! Jailer spawn + process teardown and the end-to-end test against a real
//! Firecracker require `/dev/kvm` and land in a later slice; the logic here is
//! tested against [`pseudo_firecracker::PseudoFirecracker`], and the real client
//! against a stub unix-socket server — both with no VM.
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod compat;
pub mod drain;
pub mod firecracker;
#[cfg(target_os = "linux")]
pub mod guestlink;
#[cfg(target_os = "linux")]
pub mod guestload;
#[cfg(target_os = "linux")]
pub mod migrate;
#[cfg(target_os = "linux")]
pub mod net;
pub mod process;
pub mod pseudo_firecracker;
pub mod quiesce;
#[cfg(target_os = "linux")]
pub mod registry;
pub mod statedir;
pub mod sysmem;
pub mod telemetry;
pub mod transfer;
#[cfg(target_os = "linux")]
pub mod uffd;
pub mod vm;

pub use compat::CompatClass;
pub use drain::{DrainCoordinator, DrainVerdict};
pub use firecracker::{
    BootSource, Drive, Firecracker, FirecrackerApi, FirecrackerError, MachineConfig, MemBackend,
    NetworkConfig, SnapshotSource, SnapshotTarget, VsockConfig,
};
#[cfg(target_os = "linux")]
pub use guestlink::{DrainState, GuestLink};
#[cfg(target_os = "linux")]
pub use guestload::VsockTurnDriver;
#[cfg(target_os = "linux")]
pub use migrate::{
    Artifacts, MigrateError, MigrateOutcome, SourceTiming, bind_receiver, discover_artifacts,
    migrate_running, migrate_source, restore_register, restore_target,
};
pub use process::FcProcess;
pub use pseudo_firecracker::PseudoFirecracker;
pub use quiesce::{
    AppLayer, InfraLayer, InfraThresholds, QuiescenceDetector, QuiescenceReport, StorageLayer,
};
#[cfg(target_os = "linux")]
pub use registry::{HostStatus, RunningVm, VmRegistry};
pub use statedir::VmDir;
pub use transfer::{
    OutboundFile, TransferError, recv_files, recv_snapshot, send_files, send_snapshot,
};
#[cfg(target_os = "linux")]
pub use uffd::{
    FilePageSource, GuestRegionUffdMapping, PageFaultServer, PageSource, UffdError,
    UffdRestoreHandler, create_uffd,
};
pub use vm::{LifecycleError, RunState, Vm};
