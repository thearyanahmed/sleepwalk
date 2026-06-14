//! Two-process A→B migration (single run, or a counted benchmark).
//!
//! Two cooperating commands, one per host:
//!
//!   migrate recv <listen_addr> [count]   # target (B): accept `count` migrations,
//!                                         # UFFD-restore each and bring it up
//!   migrate send <target_addr> [count]   # source (A): for each of `count` runs
//!                                         # boot a VM, snapshot it, stream it to B,
//!                                         # and time snapshot + transfer
//!
//! `recv` must be running before `send` connects, and both must use the same
//! `count`. Over loopback both run on one host; for a two-host move run `recv` on
//! droplet B and point `send` at B's IP. B must have the same Firecracker,
//! kernel, and rootfs at the same paths the snapshot references (CPU-homogeneous
//! hosts — ADR-004).
//!
//! This times the **source side** of the freeze window — pause → snapshot →
//! transfer-complete — over one clock. The target's UFFD restore/resume (lazy,
//! roughly constant) is not included, so the figure is a lower bound on total
//! perceived downtime. Each run boots a fresh VM (A→B repeated, not a single VM
//! bounced back and forth).
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
        let addr = args.get(2).cloned();
        let count: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        match (args.get(1).map(String::as_str), addr) {
            (Some("recv"), Some(addr)) => rt.block_on(recv(&addr, count)),
            (Some("send"), Some(addr)) => rt.block_on(send(&addr, count)),
            _ => {
                eprintln!("usage: migrate <recv|send> <addr> [count]");
                std::process::exit(2);
            }
        }
    }

    /// One migration's source-side timing, in milliseconds.
    struct Timing {
        snapshot_ms: f64,
        transfer_ms: f64,
    }

    /// Source side: run `count` migrations to `addr`, timing each.
    async fn send(addr: &str, count: usize) {
        let art = artifacts_dir();
        let fc_bin = require(&art, "firecracker binary", |n| {
            n.starts_with("firecracker-") && !n.ends_with(".debug") && !n.ends_with(".tgz")
        });
        let kernel = require(&art, "kernel", |n| n.starts_with("vmlinux"));
        let rootfs = require(&art, "rootfs", |n| {
            n.ends_with(".squashfs") || n.ends_with(".ext4")
        });

        let mut timings = Vec::with_capacity(count);
        let mut bytes = 0u64;
        for i in 0..count {
            let (t, b) = migrate_once(&fc_bin, &kernel, &rootfs, addr, i).await;
            bytes = b;
            println!(
                "run {:>2}: snapshot {:.1} ms, transfer {:.1} ms, total {:.1} ms",
                i + 1,
                t.snapshot_ms,
                t.transfer_ms,
                t.snapshot_ms + t.transfer_ms
            );
            timings.push(t);
        }
        report(&timings, bytes);
    }

    /// Boot a VM, snapshot it, and stream it to `addr`; return the timing + bytes.
    async fn migrate_once(
        fc_bin: &Path,
        kernel: &Path,
        rootfs: &Path,
        addr: &str,
        i: usize,
    ) -> (Timing, u64) {
        let work = std::env::temp_dir().join(format!("sleepwalk-send-{}-{i}", std::process::id()));
        std::fs::create_dir_all(&work).expect("work dir");

        let mut proc = FcProcess::spawn(
            fc_bin,
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
            kernel_image: kernel.to_path_buf(),
            boot_args: BOOT_ARGS.to_owned(),
        })
        .await
        .expect("boot source");
        fc.configure_drive(Drive {
            drive_id: "rootfs".to_owned(),
            path_on_host: rootfs.to_path_buf(),
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

        let mem = work.join("mem.snap");
        let state = work.join("state.snap");

        let t0 = Instant::now();
        fc.pause().await.expect("pause");
        fc.create_snapshot(SnapshotTarget {
            mem_file: mem.clone(),
            state_file: state.clone(),
        })
        .await
        .expect("snapshot");
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
        .await
        .expect("send snapshot");
        let t2 = Instant::now();

        let bytes = std::fs::metadata(&mem).map(|m| m.len()).unwrap_or(0);
        let _ = proc.kill();
        let _ = std::fs::remove_dir_all(&work);
        (
            Timing {
                snapshot_ms: (t1 - t0).as_secs_f64() * 1000.0,
                transfer_ms: (t2 - t1).as_secs_f64() * 1000.0,
            },
            bytes,
        )
    }

    fn report(timings: &[Timing], bytes: u64) {
        if timings.is_empty() {
            println!("no runs");
            return;
        }
        let totals: Vec<f64> = timings
            .iter()
            .map(|t| t.snapshot_ms + t.transfer_ms)
            .collect();
        let snaps: Vec<f64> = timings.iter().map(|t| t.snapshot_ms).collect();
        let xfers: Vec<f64> = timings.iter().map(|t| t.transfer_ms).collect();
        let stat = |v: &[f64]| {
            let n = v.len() as f64;
            let min = v.iter().copied().fold(f64::INFINITY, f64::min);
            let max = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            (min, max, v.iter().sum::<f64>() / n)
        };
        let (tmin, tmax, tmean) = stat(&totals);
        let (smin, smax, smean) = stat(&snaps);
        let (xmin, xmax, xmean) = stat(&xfers);

        println!(
            "\n=== A->B migration source cost ({} runs, snapshot + transfer) ===",
            timings.len()
        );
        println!("  snapshot  min {smin:.1}  max {smax:.1}  mean {smean:.1} ms");
        println!("  transfer  min {xmin:.1}  max {xmax:.1}  mean {xmean:.1} ms");
        println!("  total     min {tmin:.1}  max {tmax:.1}  mean {tmean:.1} ms");
        println!("  bytes moved / run: {bytes}");

        let json = serde_json::json!({
            "kind": "cross_host_source_cost",
            "runs": timings.len(),
            "guest_mib": GUEST_MIB,
            "bytes_moved": bytes,
            "snapshot_ms": snaps,
            "transfer_ms": xfers,
            "total_ms": totals,
            "total_min_ms": tmin, "total_max_ms": tmax, "total_mean_ms": tmean,
            "note": "source side only (pause->snapshot->transfer-complete); excludes target UFFD restore/resume; 1 vCPU, not publication-valid",
        });
        println!("{}", serde_json::to_string(&json).unwrap_or_default());
    }

    /// Target side: accept and restore `count` migrations in turn.
    async fn recv(listen_addr: &str, count: usize) {
        let fc_bin = require(&artifacts_dir(), "firecracker binary", |n| {
            n.starts_with("firecracker-") && !n.ends_with(".debug") && !n.ends_with(".tgz")
        });
        let listener = TcpListener::bind(listen_addr).await.expect("bind listener");
        println!("[recv] listening on {listen_addr} for {count} migration(s)");

        for i in 0..count {
            let work =
                std::env::temp_dir().join(format!("sleepwalk-recv-{}-{i}", std::process::id()));
            std::fs::create_dir_all(&work).expect("work dir");

            let files = recv_snapshot(&listener, &work)
                .await
                .expect("recv snapshot");
            assert_eq!(files.len(), 2, "snapshot is two files");

            let uffd_sock = work.join("uffd.sock");
            let handler = UffdRestoreHandler::bind(&uffd_sock).expect("bind uffd handler");
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = Arc::clone(&stop);
            let mem_thread = work.join("mem.snap");
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
                state_file: work.join("state.snap"),
                backend: MemBackend::Uffd { socket: uffd_sock },
                resume: true,
            })
            .await
            .expect("restore + resume");
            // Let the resumed guest fault a little, then retire it for the next run.
            tokio::time::sleep(Duration::from_millis(300)).await;
            println!("[recv] run {} restored and resumed", i + 1);

            stop.store(true, Ordering::Release);
            let _ = proc.kill();
            let _ = serve.join();
            let _ = std::fs::remove_dir_all(&work);
        }
        println!("[recv] done ({count} migrations)");
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
