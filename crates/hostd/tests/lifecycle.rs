//! Real single-host Firecracker lifecycle (needs `/dev/kvm` + fetched
//! artifacts). Gated behind `--features real-vm` so it never runs in the
//! everywhere unit/mock suite; drive it with `just lifecycle-test` on a Linux
//! box that has run `just fetch`.
//!
//! It spawns a real Firecracker process, configures and boots a microVM through
//! the control client, and asserts the guest reached userspace by watching its
//! serial console for the login banner — then pauses, resumes, and reaps it.
#![cfg(all(target_os = "linux", feature = "real-vm"))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use hostd::{BootSource, Drive, FcProcess, Firecracker, FirecrackerApi, MachineConfig};

/// The artifacts directory: `$SLEEPWALK_ARTIFACTS` or `<repo>/images/artifacts`.
fn artifacts_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SLEEPWALK_ARTIFACTS") {
        return PathBuf::from(dir);
    }
    // CARGO_MANIFEST_DIR is crates/hostd; the artifacts live at the repo root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../images/artifacts")
        .canonicalize()
        .expect("artifacts dir; run `just fetch` first")
}

/// First entry under `dir` (recursing one level) whose file name satisfies
/// `pick`. Used to locate versioned artifacts without hard-coding versions.
fn find(dir: &Path, pick: impl Fn(&str) -> bool + Copy) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            subdirs.push(path);
        } else if pick(&name) {
            return Some(path);
        }
    }
    subdirs.into_iter().find_map(|d| find(&d, pick))
}

fn require(dir: &Path, what: &str, pick: impl Fn(&str) -> bool + Copy) -> PathBuf {
    find(dir, pick)
        .unwrap_or_else(|| panic!("no {what} under {} — run `just fetch`", dir.display()))
}

/// Poll the serial log until it contains `needle` or `timeout` elapses.
async fn wait_for_serial(log: &Path, needle: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(log) {
            if text.contains(needle) {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

#[tokio::test]
async fn boots_a_real_microvm_to_userspace() {
    let art = artifacts_dir();
    let fc_bin = require(&art, "firecracker binary", |n| {
        n.starts_with("firecracker-") && !n.ends_with(".debug") && !n.ends_with(".tgz")
    });
    let kernel = require(&art, "kernel", |n| n.starts_with("vmlinux"));
    let rootfs = require(&art, "rootfs", |n| {
        n.ends_with(".squashfs") || n.ends_with(".ext4")
    });

    let tmp = std::env::temp_dir();
    let stamp = std::process::id();
    let sock = tmp.join(format!("sleepwalk-lifecycle-{stamp}.sock"));
    let serial = tmp.join(format!("sleepwalk-lifecycle-{stamp}.log"));

    let mut proc = FcProcess::spawn(&fc_bin, &sock, &serial, Duration::from_secs(10))
        .expect("spawn firecracker");

    let fc = Firecracker::new(&sock);
    fc.configure_machine(MachineConfig {
        vcpu_count: 1,
        mem_size_mib: 256,
    })
    .await
    .expect("configure machine");
    fc.configure_boot_source(BootSource {
        kernel_image: kernel,
        boot_args: "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro".to_owned(),
    })
    .await
    .expect("configure boot source");
    fc.configure_drive(Drive {
        drive_id: "rootfs".to_owned(),
        path_on_host: rootfs,
        is_root_device: true,
        is_read_only: true,
    })
    .await
    .expect("configure drive");
    fc.boot().await.expect("boot");

    // The guest reaches userspace: the Ubuntu rootfs auto-logs-in on ttyS0.
    let booted = wait_for_serial(&serial, "login", Duration::from_secs(20)).await;
    assert!(
        booted,
        "guest did not reach userspace; serial:\n{}",
        std::fs::read_to_string(&serial).unwrap_or_default()
    );

    // The lifecycle transitions work against a real VM, not just the mock.
    fc.pause().await.expect("pause");
    fc.resume().await.expect("resume");

    proc.kill().expect("reap firecracker");
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&serial);
}
