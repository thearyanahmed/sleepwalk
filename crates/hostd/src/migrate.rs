//! Migration orchestration: the source and target halves of moving one VM.
//!
//! Shared by the `migrate` CLI (benchmarking) and the `hostd` daemon (so a
//! migration is an API call, not a spawned-and-killed one-shot). Each function
//! is one migration; callers add looping, retry, and reporting.
//!
//! - [`migrate_source`] boots a VM, snapshots it, and streams it to a target,
//!   timing the source freeze window and recording it to [`crate::telemetry`].
//! - [`restore_target`] receives a snapshot, restores it via the UFFD page
//!   server, and resumes the guest.
//!
//! Linux-only: it drives Firecracker and userfaultfd. The `kvm` feature gates the
//! callers that actually exercise it against `/dev/kvm`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::net::TcpListener;

use crate::firecracker::{
    BootSource, Drive, Firecracker, FirecrackerApi, FirecrackerError, MachineConfig, MemBackend,
    SnapshotSource, SnapshotTarget, VsockConfig,
};
use crate::guestlink::{DrainState, GuestLink};
use crate::process::FcProcess;
use crate::registry::{RunningVm, UffdState};
use crate::telemetry;
use crate::transfer::{OutboundFile, TransferError, recv_snapshot, send_snapshot};
use crate::uffd::{UffdError, UffdRestoreHandler};

const GUEST_MIB: u32 = 256;
/// Boot args for the guestd rootfs: ext4 root, read-write, guestd as init.
const BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw init=/init";
/// The guest's vsock context id (host is always CID 2).
const GUEST_CID: u32 = 3;
/// How long to wait for the guest to drain to quiescence before snapshotting.
const DRAIN_DEADLINE: Duration = Duration::from_secs(5);

/// Per-process sequence so concurrent/looped migrations get distinct work dirs.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// A failure during a migration.
#[derive(Debug, Error)]
pub enum MigrateError {
    /// A Firecracker control operation failed.
    #[error(transparent)]
    Firecracker(#[from] FirecrackerError),
    /// The snapshot transfer failed.
    #[error(transparent)]
    Transfer(#[from] TransferError),
    /// The UFFD page server failed.
    #[error(transparent)]
    Uffd(#[from] UffdError),
    /// A filesystem/socket operation failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted.
        context: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The source VM did not reach userspace before the boot deadline.
    #[error("source VM never reached userspace")]
    BootTimeout,
    /// The guest did not reach quiescence before the drain deadline — by the race
    /// rule the in-flight turn wins, so the migration stands down.
    #[error("guest not quiescent before drain deadline (a turn was in flight)")]
    NotQuiescent,
    /// A required artifact (Firecracker binary, kernel, rootfs) was not found.
    #[error("artifact not found: {0} — run `just fetch`")]
    MissingArtifact(&'static str),
    /// The received snapshot was malformed.
    #[error("malformed snapshot: {0}")]
    Snapshot(String),
}

fn io(context: &str, source: std::io::Error) -> MigrateError {
    MigrateError::Io {
        context: context.to_owned(),
        source,
    }
}

/// The artifacts needed to boot a source VM.
#[derive(Debug, Clone)]
pub struct Artifacts {
    /// The Firecracker binary.
    pub fc_bin: PathBuf,
    /// The guest kernel.
    pub kernel: PathBuf,
    /// The root filesystem image.
    pub rootfs: PathBuf,
}

/// One migration's source-side timing.
#[derive(Debug, Clone, Copy)]
pub struct SourceTiming {
    /// Pause → snapshot-written.
    pub snapshot: Duration,
    /// Snapshot-written → transfer-complete.
    pub transfer: Duration,
    /// Bytes in the memory snapshot.
    pub bytes: u64,
}

/// Locate the Firecracker binary, kernel, and rootfs under the artifacts dir
/// (`$SLEEPWALK_ARTIFACTS`, else `<repo>/images/artifacts`).
///
/// # Errors
/// If the directory or any artifact is missing.
pub fn discover_artifacts() -> Result<Artifacts, MigrateError> {
    let dir = match std::env::var("SLEEPWALK_ARTIFACTS") {
        Ok(d) => PathBuf::from(d),
        Err(_) => Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../images/artifacts")
            .canonicalize()
            .map_err(|e| io("resolve artifacts dir", e))?,
    };
    Ok(Artifacts {
        fc_bin: find(&dir, |n| {
            n.starts_with("firecracker-") && !n.ends_with(".debug") && !n.ends_with(".tgz")
        })
        .ok_or(MigrateError::MissingArtifact("firecracker binary"))?,
        kernel: find(&dir, |n| n.starts_with("vmlinux"))
            .ok_or(MigrateError::MissingArtifact("kernel"))?,
        // The migration boots the guestd rootfs (guestd as init) so the guest can
        // be drained to quiescence over vsock before snapshotting.
        rootfs: find(&dir, |n| n.starts_with("guestd-rootfs")).ok_or(
            MigrateError::MissingArtifact("guestd rootfs (run `just guest-rootfs`)"),
        )?,
    })
}

/// Boot a VM, snapshot it, and stream it to `addr`. Records the freeze window to
/// telemetry and returns the timing.
///
/// # Errors
/// If any lifecycle, snapshot, or transfer step fails.
pub async fn migrate_source(art: &Artifacts, addr: &str) -> Result<SourceTiming, MigrateError> {
    let work = work_dir("src");
    std::fs::create_dir_all(&work).map_err(|e| io("create work dir", e))?;

    let mut proc = FcProcess::spawn(
        &art.fc_bin,
        &work.join("fc.sock"),
        &work.join("fc.log"),
        secs(10),
    )
    .map_err(|e| io("spawn firecracker", e))?;
    let fc = Firecracker::new(work.join("fc.sock"));
    fc.configure_machine(MachineConfig {
        vcpu_count: 1,
        mem_size_mib: GUEST_MIB,
    })
    .await?;
    fc.configure_boot_source(BootSource {
        kernel_image: art.kernel.clone(),
        boot_args: BOOT_ARGS.to_owned(),
    })
    .await?;
    fc.configure_drive(Drive {
        drive_id: "rootfs".to_owned(),
        path_on_host: art.rootfs.clone(),
        is_root_device: true,
        is_read_only: false,
    })
    .await?;
    // The vsock host-side UDS lives directly under the temp dir, not the per-VM
    // work dir: Firecracker re-binds this exact path when the snapshot is loaded
    // on the target, so its parent must exist there too (every host has /tmp; a
    // source-only work dir would not). Unique per migration to avoid stale-socket
    // clashes on a host that migrates repeatedly.
    let vsock_uds = std::env::temp_dir().join(format!(
        "sleepwalk-vsock-{}-{}.sock",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fc.configure_vsock(VsockConfig {
        guest_cid: GUEST_CID,
        uds_path: vsock_uds.clone(),
    })
    .await?;
    fc.boot().await?;

    // Reach the live guest over vsock, hand it secrets, and drain it to a
    // verified idle gap *before* snapshotting — the safety gate. On any failure
    // here the VM is left intact on the source (nothing has been snapshotted).
    let abort = |proc: &mut FcProcess, work: &Path| {
        let _ = proc.kill();
        let _ = std::fs::remove_dir_all(work);
        let _ = std::fs::remove_file(&vsock_uds);
    };
    let link = match GuestLink::connect_retry(&vsock_uds, proto::GUEST_VSOCK_PORT, secs(20)).await {
        Ok(link) => link,
        Err(e) => {
            abort(&mut proc, &work);
            return Err(io("connect guest vsock", e));
        }
    };
    if let Err(e) = link.handshake(std::collections::BTreeMap::new()).await {
        abort(&mut proc, &work);
        return Err(io("guest handshake", e));
    }
    match link.drain(DRAIN_DEADLINE).await {
        Ok(DrainState::Quiescent) => {}
        Ok(DrainState::Busy) => {
            abort(&mut proc, &work);
            return Err(MigrateError::NotQuiescent);
        }
        Err(e) => {
            abort(&mut proc, &work);
            return Err(io("drain guest", e));
        }
    }
    drop(link); // close the control link before pausing

    let timing = snapshot_and_send(&fc, &work, addr).await?;
    let _ = proc.kill();
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&vsock_uds);
    telemetry::migration_ok(timing.snapshot + timing.transfer, timing.bytes);
    Ok(timing)
}

/// Pause `fc`, snapshot its memory + device state into `work`, and stream both
/// files to `addr`. Returns the freeze-window timing; the caller owns the VM's
/// lifecycle (kill / teardown) and any telemetry.
///
/// # Errors
/// If pausing, snapshotting, or the transfer fails.
async fn snapshot_and_send(
    fc: &Firecracker,
    work: &Path,
    addr: &str,
) -> Result<SourceTiming, MigrateError> {
    let mem = work.join("mem.snap");
    let state = work.join("state.snap");

    let t0 = Instant::now();
    fc.pause().await?;
    fc.create_snapshot(SnapshotTarget {
        mem_file: mem.clone(),
        state_file: state.clone(),
    })
    .await?;
    let t1 = Instant::now();
    send_snapshot(
        addr,
        &[
            OutboundFile {
                name: "mem.snap".to_owned(),
                path: mem.clone(),
            },
            OutboundFile {
                name: "state.snap".to_owned(),
                path: state,
            },
        ],
    )
    .await?;
    let t2 = Instant::now();

    let bytes = std::fs::metadata(&mem).map(|m| m.len()).unwrap_or(0);
    Ok(SourceTiming {
        snapshot: t1 - t0,
        transfer: t2 - t1,
        bytes,
    })
}

/// How migrating a registered, running VM ended.
pub enum MigrateOutcome {
    /// The VM was drained, snapshotted, streamed to the target, and torn down
    /// here. The freeze-window timing is included.
    Moved(SourceTiming),
    /// The guest was busy at the drain deadline — by the race rule the turn wins,
    /// so the migration stood down and the VM is handed back intact to re-register.
    StoodDown(RunningVm),
}

/// Migrate a registered, running [`RunningVm`] to `addr`: connect to its guest
/// over vsock, drain it to quiescence, then snapshot and stream it, tearing the
/// source VM down on success. On a busy guest it stands down and returns the VM
/// intact (gate reopened) for the caller to re-register.
///
/// # Errors
/// If the guest is unreachable, the handshake/drain fails for a reason other than
/// busyness, or the snapshot/transfer fails. The VM is torn down on error.
pub async fn migrate_running(vm: RunningVm, addr: &str) -> Result<MigrateOutcome, MigrateError> {
    let link =
        match GuestLink::connect_retry(&vm.vsock_uds, proto::GUEST_VSOCK_PORT, secs(20)).await {
            Ok(link) => link,
            Err(e) => {
                vm.teardown();
                return Err(io("connect guest vsock", e));
            }
        };
    if let Err(e) = link.handshake(std::collections::BTreeMap::new()).await {
        vm.teardown();
        return Err(io("guest handshake", e));
    }
    match link.drain(DRAIN_DEADLINE).await {
        Ok(DrainState::Quiescent) => {}
        Ok(DrainState::Busy) => {
            // Reopen the gate so the in-flight turn (and any queued behind it) run
            // on, then hand the VM back to be re-registered on this host.
            let _ = link.send(proto::HostToGuest::DrainCancel).await;
            drop(link);
            return Ok(MigrateOutcome::StoodDown(vm));
        }
        Err(e) => {
            vm.teardown();
            return Err(io("drain guest", e));
        }
    }
    drop(link);

    match snapshot_and_send(&vm.fc, &vm.work, addr).await {
        Ok(timing) => {
            telemetry::migration_ok(timing.snapshot + timing.transfer, timing.bytes);
            vm.teardown();
            Ok(MigrateOutcome::Moved(timing))
        }
        Err(e) => {
            telemetry::migration_failed();
            vm.teardown();
            Err(e)
        }
    }
}

/// Receive one migration on `listener`, restore it via the UFFD page server,
/// resume the guest, and tear it down — the one-shot path for the benchmark CLI
/// (prove a VM round-trips). The daemon uses [`restore_register`] instead.
///
/// # Errors
/// If receiving, restoring, or resuming fails.
pub async fn restore_target(fc_bin: &Path, listener: &TcpListener) -> Result<(), MigrateError> {
    let vm = receive_and_restore(fc_bin, listener).await?;
    vm.teardown();
    Ok(())
}

/// Receive one migration and return the restored VM, **left running** with its
/// UFFD page server owned by the returned [`RunningVm`] — for the daemon to
/// register into its fleet.
///
/// # Errors
/// If receiving, restoring, or resuming fails.
pub async fn restore_register(
    fc_bin: &Path,
    listener: &TcpListener,
) -> Result<RunningVm, MigrateError> {
    receive_and_restore(fc_bin, listener).await
}

/// The shared restore core: receive the snapshot, stand up the UFFD page server,
/// spawn the target Firecracker, load the snapshot, and resume — returning the
/// live VM (process + page-server thread). The caller decides whether to keep it
/// (register) or tear it down.
async fn receive_and_restore(
    fc_bin: &Path,
    listener: &TcpListener,
) -> Result<RunningVm, MigrateError> {
    let work = work_dir("tgt");
    std::fs::create_dir_all(&work).map_err(|e| io("create work dir", e))?;

    let files = recv_snapshot(listener, &work).await?;
    if files.len() != 2 {
        return Err(MigrateError::Snapshot(format!(
            "expected 2 files, got {}",
            files.len()
        )));
    }

    let uffd_sock = work.join("uffd.sock");
    let handler = UffdRestoreHandler::bind(&uffd_sock).map_err(|e| io("bind uffd socket", e))?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let mem_thread = work.join("mem.snap");
    let serve = std::thread::spawn(move || {
        let _ = handler.serve(&mem_thread, &stop_thread);
    });

    let mut proc = FcProcess::spawn(
        fc_bin,
        &work.join("fc.sock"),
        &work.join("fc.log"),
        secs(10),
    )
    .map_err(|e| io("spawn firecracker", e))?;
    let fc = Firecracker::new(work.join("fc.sock"));
    let load = fc
        .load_snapshot(SnapshotSource {
            state_file: work.join("state.snap"),
            backend: MemBackend::Uffd {
                socket: uffd_sock.to_path_buf(),
            },
            resume: true,
        })
        .await;
    if let Err(e) = load {
        // Restore failed: stop the page server, reap the half-started VM, clean up.
        let _ = proc.kill();
        stop.store(true, Ordering::Release);
        let _ = serve.join();
        let _ = std::fs::remove_dir_all(&work);
        return Err(MigrateError::from(e));
    }
    // Give the guest a beat to fault its first pages and prove it resumed.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mib = mem_mib(&work.join("mem.snap"));
    Ok(RunningVm {
        id: proto::VmId::new(),
        proc,
        fc,
        work,
        // The restored VM's vsock socket path lives in the snapshot, not chosen
        // here, so it is not re-migratable yet — terminal on this host for now.
        vsock_uds: PathBuf::new(),
        mib,
        uffd: Some(UffdState {
            stop,
            thread: serve,
        }),
    })
}

/// A snapshot's guest memory size in MiB, from the mem file length.
fn mem_mib(mem: &Path) -> u32 {
    let bytes = std::fs::metadata(mem).map(|m| m.len()).unwrap_or(0);
    (bytes / (1024 * 1024)) as u32
}

/// Bind a receiver socket for [`restore_target`].
///
/// # Errors
/// If the address cannot be bound.
pub async fn bind_receiver(addr: &str) -> Result<TcpListener, MigrateError> {
    TcpListener::bind(addr)
        .await
        .map_err(|e| io("bind receiver", e))
}

fn work_dir(role: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("sleepwalk-{role}-{}-{n}", std::process::id()))
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn find(dir: &Path, pick: impl Fn(&str) -> bool + Copy) -> Option<PathBuf> {
    let mut subdirs = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if pick(&entry.file_name().to_string_lossy()) {
            return Some(path);
        }
    }
    subdirs.into_iter().find_map(|d| find(&d, pick))
}
