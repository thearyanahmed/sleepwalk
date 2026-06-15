//! A→B migration with the memory moved over the network (needs `/dev/kvm` +
//! fetched artifacts). Gated behind `--features kvm`; run with
//! `just migrate-test`.
//!
//! Unlike `restore.rs` (which restores from the same on-disk snapshot), this
//! snapshots the source, **streams the memory + vmstate over a TCP socket** into
//! a separate directory, and restores the target from that received copy via the
//! UFFD page server — the same path a two-host move takes. Here both ends
//! run on one host over loopback; pointing the sender at another droplet's IP is
//! the only change for a true cross-host migration.
#![cfg(all(target_os = "linux", feature = "kvm"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use hostd::{
    BootSource, Drive, FcProcess, Firecracker, FirecrackerApi, MachineConfig, MemBackend,
    OutboundFile, SnapshotSource, SnapshotTarget, UffdRestoreHandler, recv_snapshot, send_snapshot,
};
use tokio::net::TcpListener;

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

async fn within<T>(what: &str, fut: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), fut)
        .await
        .unwrap_or_else(|_| panic!("{what} timed out"))
}

#[tokio::test]
async fn migrates_a_vm_with_memory_moved_over_tcp() {
    let art = artifacts_dir();
    let fc_bin = require(&art, "firecracker binary", |n| {
        n.starts_with("firecracker-") && !n.ends_with(".debug") && !n.ends_with(".tgz")
    });
    let kernel = require(&art, "kernel", |n| n.starts_with("vmlinux"));
    let rootfs = require(&art, "rootfs", |n| n.ends_with(".squashfs"));

    let base = std::env::temp_dir().join(format!("sleepwalk-migrate-{}", std::process::id()));
    // `src` stands in for host A's state dir, `dst` for host B's — separate
    // directories, so nothing is shared; the bytes only reach B over the socket.
    let src = base.join("hostA");
    let dst = base.join("hostB");
    std::fs::create_dir_all(&src).expect("src dir");
    std::fs::create_dir_all(&dst).expect("dst dir");

    // --- Host A: boot, reach userspace, pause, snapshot. ---
    let mut fc_a =
        FcProcess::spawn(&fc_bin, &src.join("a.sock"), &src.join("a.log"), secs(10)).expect("fc A");
    let a = Firecracker::new(src.join("a.sock"));
    a.configure_machine(MachineConfig {
        vcpu_count: 1,
        mem_size_mib: 256,
    })
    .await
    .expect("machine");
    a.configure_boot_source(BootSource {
        kernel_image: kernel,
        boot_args: "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro".to_owned(),
    })
    .await
    .expect("boot source");
    a.configure_drive(Drive {
        drive_id: "rootfs".to_owned(),
        path_on_host: rootfs,
        is_root_device: true,
        is_read_only: true,
    })
    .await
    .expect("drive");
    a.boot().await.expect("boot");
    assert!(
        wait_for_serial(&src.join("a.log"), "login", secs(20)).await,
        "host A never reached userspace"
    );
    a.pause().await.expect("pause A");
    a.create_snapshot(SnapshotTarget {
        mem_file: src.join("mem.snap"),
        state_file: src.join("state.snap"),
    })
    .await
    .expect("snapshot A");
    fc_a.kill().expect("reap A");

    // --- Move the snapshot A→B over TCP (loopback here, a droplet IP for real). ---
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind receiver");
    let addr = listener.local_addr().expect("addr").to_string();
    let dst_recv = dst.clone();
    let receiver = tokio::spawn(async move { recv_snapshot(&listener, &dst_recv).await });
    send_snapshot(
        &addr,
        &[
            OutboundFile {
                name: "mem.snap".to_owned(),
                path: src.join("mem.snap"),
            },
            OutboundFile {
                name: "state.snap".to_owned(),
                path: src.join("state.snap"),
            },
        ],
    )
    .await
    .expect("send snapshot");
    let received = receiver.await.expect("join").expect("recv snapshot");
    assert_eq!(received.len(), 2, "both snapshot files received on B");
    assert!(dst.join("mem.snap").exists() && dst.join("state.snap").exists());

    // --- Host B: restore from the RECEIVED copy via the UFFD page server. ---
    let uffd_sock = dst.join("uffd.sock");
    let handler = UffdRestoreHandler::bind(&uffd_sock).expect("bind handler");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let mem_b = dst.join("mem.snap");
    let serve = thread::spawn(move || handler.serve(&mem_b, &stop_thread));

    let mut fc_b =
        FcProcess::spawn(&fc_bin, &dst.join("b.sock"), &dst.join("b.log"), secs(10)).expect("fc B");
    let b = Firecracker::new(dst.join("b.sock"));
    within(
        "load_snapshot on B",
        b.load_snapshot(SnapshotSource {
            state_file: dst.join("state.snap"),
            backend: MemBackend::Uffd {
                socket: uffd_sock.clone(),
            },
            resume: true,
        }),
    )
    .await
    .expect("restore B");

    // The migrated VM is alive on B: it responds to pause/resume.
    within("pause B", b.pause()).await.expect("pause restored");
    within("resume B", b.resume())
        .await
        .expect("resume restored");

    stop.store(true, Ordering::Release);
    fc_b.kill().expect("reap B");
    serve.join().expect("handler thread").expect("serve");
    let _ = std::fs::remove_dir_all(&base);
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}
