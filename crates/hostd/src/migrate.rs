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
/// `$SLEEPWALK_ROOTFS`, if set, overrides the rootfs with an explicit path — how
/// the agent profile selects its own image without colliding with the synthetic
/// `guestd-rootfs-*` in the same dir.
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
    // The migration boots a guestd rootfs (guestd as init) so the guest can be
    // drained to quiescence before snapshotting. Default: the synthetic image in
    // the artifacts dir; override with an explicit path for the agent profile.
    let rootfs = match std::env::var("SLEEPWALK_ROOTFS") {
        Ok(p) => PathBuf::from(p),
        Err(_) => find(&dir, |n| n.starts_with("guestd-rootfs")).ok_or(
            MigrateError::MissingArtifact("guestd rootfs (run `just guest-rootfs`)"),
        )?,
    };
    Ok(Artifacts {
        fc_bin: find(&dir, |n| {
            n.starts_with("firecracker-") && !n.ends_with(".debug") && !n.ends_with(".tgz")
        })
        .ok_or(MigrateError::MissingArtifact("firecracker binary"))?,
        kernel: find(&dir, |n| n.starts_with("vmlinux"))
            .ok_or(MigrateError::MissingArtifact("kernel"))?,
        rootfs,
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

    let timing = snapshot_and_send(&fc, &work, addr, &[]).await?;
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
    extra: &[OutboundFile],
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
    let mut files = vec![
        OutboundFile {
            name: "mem.snap".to_owned(),
            path: mem.clone(),
        },
        OutboundFile {
            name: "state.snap".to_owned(),
            path: state,
        },
    ];
    files.extend_from_slice(extra);
    send_snapshot(addr, &files).await?;
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
/// Handshake then drain a guest over `link` (any transport), returning whether it
/// reached quiescence. On Busy, reopens the gate (DrainCancel) so the in-flight
/// turn runs on. Shared by the TCP (networked) and vsock drain paths.
async fn drain_to_quiescence<R, W>(link: &GuestLink<R, W>) -> std::io::Result<DrainState>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    link.handshake(std::collections::BTreeMap::new()).await?;
    let state = link.drain(DRAIN_DEADLINE).await?;
    if state == DrainState::Busy {
        let _ = link.send(proto::HostToGuest::DrainCancel).await;
    }
    Ok(state)
}

pub async fn migrate_running(vm: RunningVm, addr: &str) -> Result<MigrateOutcome, MigrateError> {
    // Drain to quiescence over the channel that survives a restore. A networked VM
    // is drained over the guest NETWORK (TCP) — Firecracker's vsock stops servicing
    // connections after a snapshot restore (both directions), but the guest network
    // does survive, so TCP is what lets a *restored* VM be drained and migrated
    // again. A non-networked VM falls back to vsock (it can't re-migrate anyway).
    //
    // Pre-snapshot failures must NOT destroy the VM: it is still alive here, so on
    // any error reaching/draining the guest we hand it back intact (StoodDown) for
    // the caller to re-register, rather than tearing it down.
    let drained = if let Some(net) = &vm.net {
        let addr = format!("{}:{}", net.ip, proto::GUEST_DRAIN_TCP_PORT);
        match GuestLink::connect_tcp_retry(&addr, secs(20)).await {
            Ok(link) => drain_to_quiescence(&link).await,
            Err(e) => {
                eprintln!("hostd: migrate: connect guest tcp {addr}: {e} — standing down, VM kept");
                return Ok(MigrateOutcome::StoodDown(vm));
            }
        }
    } else {
        match GuestLink::connect_retry(&vm.vsock_uds, proto::GUEST_VSOCK_PORT, secs(20)).await {
            Ok(link) => drain_to_quiescence(&link).await,
            Err(e) => {
                eprintln!("hostd: migrate: connect guest vsock: {e} — standing down, VM kept");
                return Ok(MigrateOutcome::StoodDown(vm));
            }
        }
    };
    match drained {
        Ok(DrainState::Quiescent) => {}
        Ok(DrainState::Busy) => return Ok(MigrateOutcome::StoodDown(vm)),
        Err(e) => {
            eprintln!("hostd: migrate: drain guest: {e} — standing down, VM kept");
            return Ok(MigrateOutcome::StoodDown(vm));
        }
    }

    // A networked VM carries its identity (tap/MAC/IP) alongside the snapshot so
    // the target can re-create the same tap before restoring — the guest keeps its
    // MAC/IP on the new host, and a client's connection follows it.
    let mut extra = Vec::new();
    // The guest's vsock uds path travels with the snapshot — groundwork for
    // re-migrating a restored VM. NOTE: this alone is not sufficient: Firecracker
    // does NOT re-create the host-side vsock socket on snapshot load, so the
    // target must additionally re-establish the vsock device before a restored VM
    // can be drained and moved again (the outstanding "terminal restored VM"
    // limitation). The path is carried so that fix has what it needs.
    {
        let vpath = vm.work.join("vsock.txt");
        if let Err(e) = std::fs::write(&vpath, vm.vsock_uds.to_string_lossy().as_bytes()) {
            vm.teardown();
            return Err(io("write vsock path", e));
        }
        extra.push(OutboundFile {
            name: "vsock.txt".to_owned(),
            path: vpath,
        });
    }
    if let Some(net) = &vm.net {
        let path = vm.work.join("net.json");
        match serde_json::to_vec(net) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(&path, bytes) {
                    vm.teardown();
                    return Err(io("write net identity", e));
                }
                extra.push(OutboundFile {
                    name: "net.json".to_owned(),
                    path,
                });
            }
            Err(e) => {
                vm.teardown();
                return Err(io("serialize net identity", std::io::Error::other(e)));
            }
        }
    }

    match snapshot_and_send(&vm.fc, &vm.work, addr, &extra).await {
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
    // mem.snap + state.snap, plus optional metadata files (vsock.txt, net.json).
    if !(2..=4).contains(&files.len()) {
        return Err(MigrateError::Snapshot(format!(
            "expected 2..4 files, got {}",
            files.len()
        )));
    }

    // Re-plumb the network before loading: a networked VM's snapshot names a host
    // tap (`host_dev_name`) that Firecracker re-binds on load, so the tap must
    // exist on this host first — created under the same name, on the overlay
    // bridge, so the guest keeps its MAC/IP.
    let net_path = work.join("net.json");
    let net = if net_path.exists() {
        let bytes = std::fs::read(&net_path).map_err(|e| io("read net identity", e))?;
        let net: crate::net::NetId = serde_json::from_slice(&bytes)
            .map_err(|e| io("parse net identity", std::io::Error::other(e)))?;
        if let Err(e) = crate::net::create_tap(&net.tap) {
            let _ = std::fs::remove_dir_all(&work);
            return Err(io("re-plumb vm tap", std::io::Error::other(e)));
        }
        Some(net)
    } else {
        None
    };

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
        // Restore failed: stop the page server, reap the half-started VM, drop the
        // re-plumbed tap, clean up.
        let _ = proc.kill();
        stop.store(true, Ordering::Release);
        let _ = serve.join();
        if let Some(net) = &net {
            crate::net::destroy(&net.tap);
        }
        let _ = std::fs::remove_dir_all(&work);
        return Err(MigrateError::from(e));
    }
    // Give the guest a beat to fault its first pages and prove it resumed.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Announce the VM's new location so every host on the overlay relearns its
    // MAC immediately — without this the source bridge floods/ages for seconds
    // before discovering the VM moved across the tunnel. Best-effort.
    if let Some(net) = &net
        && let Err(e) = crate::net::announce(net)
    {
        eprintln!("hostd: gratuitous ARP for {}: {e}", net.ip);
    }

    // The source's vsock uds path, carried over for completeness. Note vsock is
    // unusable after a restore (Firecracker stops servicing it), so a restored VM
    // is drained over TCP (the guest network) instead — see `migrate_running`.
    let vsock_uds = {
        let p = work.join("vsock.txt");
        if p.exists() {
            std::fs::read_to_string(&p)
                .map(|s| PathBuf::from(s.trim()))
                .unwrap_or_default()
        } else {
            PathBuf::new()
        }
    };

    let mib = mem_mib(&work.join("mem.snap"));
    Ok(RunningVm {
        id: proto::VmId::new(),
        proc,
        fc,
        work,
        vsock_uds,
        mib,
        uffd: Some(UffdState {
            stop,
            thread: serve,
        }),
        // Present for a networked VM: the tap was re-plumbed above under the
        // snapshot's name, so the guest kept its MAC/IP on this host.
        net,
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
