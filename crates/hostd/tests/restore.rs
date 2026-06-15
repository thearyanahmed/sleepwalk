//! Single-host snapshot → UFFD lazy restore of a live VM (needs `/dev/kvm` +
//! fetched artifacts). Gated behind `--features kvm`; run with
//! `just restore-test` on a KVM host after `just fetch`.
//!
//! This is the first end-to-end proof of the core idea: boot a VM, snapshot it,
//! then restore it on a fresh Firecracker whose guest memory is served lazily by
//! our [`UffdRestoreHandler`] from the snapshot file — and show the restored VM
//! is alive (it pauses and resumes after restore, which a guest stuck on an
//! unserved page fault could not do). Every post-restore step is wrapped in a
//! timeout so a broken handler fails loudly instead of hanging.
#![cfg(all(target_os = "linux", feature = "kvm"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use hostd::{
    BootSource, Drive, FcProcess, Firecracker, FirecrackerApi, MachineConfig, MemBackend,
    SnapshotSource, SnapshotTarget, UffdRestoreHandler,
};

fn artifacts_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SLEEPWALK_ARTIFACTS") {
        return PathBuf::from(dir);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../images/artifacts")
        .canonicalize()
        .expect("artifacts dir; run `just fetch` first")
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

fn require(dir: &Path, what: &str, pick: impl Fn(&str) -> bool + Copy) -> PathBuf {
    find(dir, pick)
        .unwrap_or_else(|| panic!("no {what} under {} — run `just fetch`", dir.display()))
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

/// Await `fut`, panicking with `what` if it does not finish within 10s.
async fn within<T>(what: &str, fut: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), fut)
        .await
        .unwrap_or_else(|_| panic!("{what} timed out — restore handler likely not serving faults"))
}

#[tokio::test]
async fn snapshot_then_uffd_restore_keeps_the_vm_alive() {
    let art = artifacts_dir();
    let fc_bin = require(&art, "firecracker binary", |n| {
        n.starts_with("firecracker-") && !n.ends_with(".debug") && !n.ends_with(".tgz")
    });
    let kernel = require(&art, "kernel", |n| n.starts_with("vmlinux"));
    let rootfs = require(&art, "rootfs", |n| n.ends_with(".squashfs"));

    let tmp = std::env::temp_dir().join(format!("sleepwalk-restore-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).expect("tmp dir");
    let sock1 = tmp.join("fc1.sock");
    let serial1 = tmp.join("fc1.log");
    let sock2 = tmp.join("fc2.sock");
    let serial2 = tmp.join("fc2.log");
    let uffd_sock = tmp.join("uffd.sock");
    let mem_file = tmp.join("mem.snap");
    let state_file = tmp.join("vmstate.snap");

    // 1. Boot a VM and let it reach userspace.
    let mut fc1_proc =
        FcProcess::spawn(&fc_bin, &sock1, &serial1, Duration::from_secs(10)).expect("spawn fc1");
    let fc1 = Firecracker::new(&sock1);
    fc1.configure_machine(MachineConfig {
        vcpu_count: 1,
        mem_size_mib: 256,
    })
    .await
    .expect("machine");
    fc1.configure_boot_source(BootSource {
        kernel_image: kernel,
        boot_args: "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro".to_owned(),
    })
    .await
    .expect("boot source");
    fc1.configure_drive(Drive {
        drive_id: "rootfs".to_owned(),
        path_on_host: rootfs,
        is_root_device: true,
        is_read_only: true,
    })
    .await
    .expect("drive");
    fc1.boot().await.expect("boot");
    assert!(
        wait_for_serial(&serial1, "login", Duration::from_secs(20)).await,
        "fc1 never reached userspace"
    );

    // 2. Pause and snapshot it, then stop the source Firecracker.
    fc1.pause().await.expect("pause");
    fc1.create_snapshot(SnapshotTarget {
        mem_file: mem_file.clone(),
        state_file: state_file.clone(),
    })
    .await
    .expect("create snapshot");
    fc1_proc.kill().expect("reap fc1");
    assert!(
        mem_file.exists() && state_file.exists(),
        "snapshot files written"
    );

    // 3. Start the UFFD handler (listening before Firecracker connects), then
    //    restore onto a fresh Firecracker whose memory it serves lazily.
    let handler = UffdRestoreHandler::bind(&uffd_sock).expect("bind uffd handler");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let mem_for_thread = mem_file.clone();
    let serve = thread::spawn(move || handler.serve(&mem_for_thread, &stop_thread));

    let mut fc2_proc =
        FcProcess::spawn(&fc_bin, &sock2, &serial2, Duration::from_secs(10)).expect("spawn fc2");
    let fc2 = Firecracker::new(&sock2);

    within(
        "load_snapshot",
        fc2.load_snapshot(SnapshotSource {
            state_file: state_file.clone(),
            backend: MemBackend::Uffd {
                socket: uffd_sock.clone(),
            },
            resume: true,
        }),
    )
    .await
    .expect("load snapshot");

    // 4. The restored VM is alive: it responds to pause/resume, which a guest
    //    blocked on an unserved page fault could not. Pages were served lazily by
    //    our handler to get here.
    within("pause after restore", fc2.pause())
        .await
        .expect("pause restored vm");
    within("resume after restore", fc2.resume())
        .await
        .expect("resume restored vm");

    // Teardown.
    stop.store(true, Ordering::Release);
    fc2_proc.kill().expect("reap fc2");
    serve
        .join()
        .expect("handler thread")
        .expect("handler serve");
    let _ = std::fs::remove_dir_all(&tmp);
}
