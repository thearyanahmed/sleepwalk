//! Two-process A→B migration.
//!
//! Two cooperating commands, one per host:
//!
//!   migrate recv <listen_addr>     # on the target (B): listen, receive the
//!                                  # snapshot, UFFD-restore, keep the VM alive
//!   migrate send <target_addr>     # on the source (A): boot a VM, snapshot it,
//!                                  # stream mem+vmstate to B, report timings
//!
//! `recv` must be running before `send` connects. Over loopback both run on one
//! host (`recv 127.0.0.1:9000` then `send 127.0.0.1:9000`); for a real two-host
//! move, run `recv` on droplet B and point `send` at B's IP. B must have the same
//! Firecracker, kernel, and rootfs artifacts at the same paths the snapshot
//! references (CPU-homogeneous hosts — see ADR-004).
//!
//! Build: requires the `kvm` feature and Linux + `/dev/kvm` + `just fetch`.

fn main() {
    #[cfg(target_os = "linux")]
    linux::main();
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("migrate requires Linux (userfaultfd + KVM)");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use hostd::{
        BootSource, Drive, FcProcess, Firecracker, FirecrackerApi, MachineConfig, MemBackend,
        OutboundFile, SnapshotSource, SnapshotTarget, UffdRestoreHandler, recv_snapshot,
        send_snapshot,
    };
    use tokio::net::TcpListener;

    const GUEST_MIB: u32 = 256;
    const BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro";

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        match (args.get(1).map(String::as_str), args.get(2)) {
            (Some("recv"), Some(addr)) => rt.block_on(recv(addr)),
            (Some("send"), Some(addr)) => rt.block_on(send(addr)),
            _ => {
                eprintln!("usage: migrate <recv|send> <addr>");
                std::process::exit(2);
            }
        }
    }

    /// Target side: receive a snapshot and bring the VM up via the UFFD server.
    async fn recv(listen_addr: &str) {
        let fc_bin = require(&artifacts_dir(), "firecracker binary", |n| {
            n.starts_with("firecracker-") && !n.ends_with(".debug") && !n.ends_with(".tgz")
        });
        let work = std::env::temp_dir().join(format!("sleepwalk-recv-{}", std::process::id()));
        std::fs::create_dir_all(&work).expect("work dir");

        let listener = TcpListener::bind(listen_addr).await.expect("bind listener");
        println!(
            "[recv] listening on {listen_addr}, work dir {}",
            work.display()
        );

        let files = recv_snapshot(&listener, &work)
            .await
            .expect("recv snapshot");
        println!("[recv] received {} files", files.len());

        let mem = work.join("mem.snap");
        let state = work.join("state.snap");
        assert!(mem.exists() && state.exists(), "snapshot files present");

        let uffd_sock = work.join("uffd.sock");
        let handler = UffdRestoreHandler::bind(&uffd_sock).expect("bind uffd handler");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let mem_thread = mem.clone();
        let serve = std::thread::spawn(move || {
            let _ = handler.serve(&mem_thread, &stop_thread);
        });

        let mut proc = FcProcess::spawn(
            &fc_bin,
            &work.join("fc.sock"),
            &work.join("fc.log"),
            secs(10),
        )
        .expect("spawn fc");
        let fc = Firecracker::new(work.join("fc.sock"));
        fc.load_snapshot(SnapshotSource {
            state_file: state,
            backend: MemBackend::Uffd { socket: uffd_sock },
            resume: true,
        })
        .await
        .expect("restore + resume");
        println!("[recv] restored and resumed — VM is alive on this host");

        // Hold the VM up so the migration is observable, then tear down. A real
        // deployment would keep it indefinitely; this is the build/test harness.
        let hold = Duration::from_millis(env_or("SLEEPWALK_MIGRATE_HOLD_MS", 5000));
        tokio::time::sleep(hold).await;

        fc.pause().await.expect("pause");
        fc.resume().await.expect("resume");
        println!(
            "[recv] VM still responsive after {} ms; shutting down",
            hold.as_millis()
        );

        stop.store(true, Ordering::Release);
        let _ = proc.kill();
        let _ = serve.join();
        let _ = std::fs::remove_dir_all(&work);
    }

    /// Source side: boot a VM, snapshot it, and stream it to the target.
    async fn send(target_addr: &str) {
        let art = artifacts_dir();
        let fc_bin = require(&art, "firecracker binary", |n| {
            n.starts_with("firecracker-") && !n.ends_with(".debug") && !n.ends_with(".tgz")
        });
        let kernel = require(&art, "kernel", |n| n.starts_with("vmlinux"));
        let rootfs = require(&art, "rootfs", |n| {
            n.ends_with(".squashfs") || n.ends_with(".ext4")
        });
        let work = std::env::temp_dir().join(format!("sleepwalk-send-{}", std::process::id()));
        std::fs::create_dir_all(&work).expect("work dir");

        let mut proc = FcProcess::spawn(
            &fc_bin,
            &work.join("fc.sock"),
            &work.join("fc.log"),
            secs(10),
        )
        .expect("spawn fc");
        let fc = Firecracker::new(work.join("fc.sock"));
        fc.configure_machine(MachineConfig {
            vcpu_count: 1,
            mem_size_mib: GUEST_MIB,
        })
        .await
        .expect("machine");
        fc.configure_boot_source(BootSource {
            kernel_image: kernel,
            boot_args: BOOT_ARGS.to_owned(),
        })
        .await
        .expect("boot source");
        fc.configure_drive(Drive {
            drive_id: "rootfs".to_owned(),
            path_on_host: rootfs,
            is_root_device: true,
            is_read_only: true,
        })
        .await
        .expect("drive");
        fc.boot().await.expect("boot");
        assert!(
            wait_for_serial(&work.join("fc.log"), "login", secs(20)).await,
            "source VM never reached userspace"
        );
        println!("[send] VM booted; migrating to {target_addr}");

        let mem = work.join("mem.snap");
        let state = work.join("state.snap");

        let freeze_start = Instant::now();
        fc.pause().await.expect("pause");
        fc.create_snapshot(SnapshotTarget {
            mem_file: mem.clone(),
            state_file: state.clone(),
        })
        .await
        .expect("snapshot");
        let snapshot_done = Instant::now();

        send_snapshot(
            target_addr,
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
        .await
        .expect("send snapshot");
        let sent = Instant::now();

        let bytes = std::fs::metadata(&mem).map(|m| m.len()).unwrap_or(0);
        println!(
            "[send] snapshot {:.1} ms, transfer {:.1} ms ({} bytes) — source VM released",
            (snapshot_done - freeze_start).as_secs_f64() * 1000.0,
            (sent - snapshot_done).as_secs_f64() * 1000.0,
            bytes,
        );

        let _ = proc.kill();
        let _ = std::fs::remove_dir_all(&work);
    }

    fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

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
}
