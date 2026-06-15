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
    SnapshotSource, SnapshotTarget,
};
use crate::process::FcProcess;
use crate::telemetry;
use crate::transfer::{OutboundFile, TransferError, recv_snapshot, send_snapshot};
use crate::uffd::{UffdError, UffdRestoreHandler};

const GUEST_MIB: u32 = 256;
const BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro";

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
        rootfs: find(&dir, |n| n.ends_with(".squashfs") || n.ends_with(".ext4"))
            .ok_or(MigrateError::MissingArtifact("rootfs"))?,
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
        is_read_only: true,
    })
    .await?;
    fc.boot().await?;
    if !wait_for_serial(&work.join("fc.log"), "login", secs(20)).await {
        let _ = proc.kill();
        let _ = std::fs::remove_dir_all(&work);
        return Err(MigrateError::BootTimeout);
    }

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
    let _ = proc.kill();
    let _ = std::fs::remove_dir_all(&work);

    let timing = SourceTiming {
        snapshot: t1 - t0,
        transfer: t2 - t1,
        bytes,
    };
    telemetry::migration_ok(timing.snapshot + timing.transfer, bytes);
    Ok(timing)
}

/// Receive one migration on `listener`, restore it via the UFFD page server, and
/// resume the guest. Tears the VM and handler down before returning.
///
/// # Errors
/// If receiving, restoring, or resuming fails.
pub async fn restore_target(fc_bin: &Path, listener: &TcpListener) -> Result<(), MigrateError> {
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

    let outcome = resume_from_snapshot(fc_bin, &work, &uffd_sock).await;

    stop.store(true, Ordering::Release);
    let _ = serve.join();
    let _ = std::fs::remove_dir_all(&work);
    outcome
}

/// Spawn the target Firecracker, load the snapshot with the UFFD backend, and
/// confirm it resumed. The handler thread is owned by the caller.
async fn resume_from_snapshot(
    fc_bin: &Path,
    work: &Path,
    uffd_sock: &Path,
) -> Result<(), MigrateError> {
    let mut proc = FcProcess::spawn(
        fc_bin,
        &work.join("fc.sock"),
        &work.join("fc.log"),
        secs(10),
    )
    .map_err(|e| io("spawn firecracker", e))?;
    let fc = Firecracker::new(work.join("fc.sock"));
    let res = fc
        .load_snapshot(SnapshotSource {
            state_file: work.join("state.snap"),
            backend: MemBackend::Uffd {
                socket: uffd_sock.to_path_buf(),
            },
            resume: true,
        })
        .await;
    if res.is_ok() {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let _ = proc.kill();
    res.map_err(MigrateError::from)
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

async fn wait_for_serial(log: &Path, needle: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(log)
            && text.contains(needle)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}
