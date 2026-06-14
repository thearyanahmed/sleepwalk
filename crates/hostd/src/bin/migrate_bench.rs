//! Migration freeze-window benchmark.
//!
//! Boots one microVM, then migrates it in a ping-pong: snapshot the live VM and
//! lazily restore it onto a fresh Firecracker via the UFFD page server, settle,
//! repeat. Each cycle times the **freeze window** — pause → snapshot →
//! restore-and-resume — which is the guest's perceived downtime (the pages then
//! fault in lazily after resume, off the critical path).
//!
//! Output: every per-cycle timing plus min / max / mean, printed and written as
//! JSON, so the same measurement can be re-captured later against richer
//! workloads (real agents) without changing the harness.
//!
//! Single host: the snapshot files are local, so this measures freeze window
//! only — no network transfer. Numbers from a small/loaded box are not
//! publication-valid; this is the instrument, not the result.
//!
//! Run: `just migrate-bench` (or `cargo run -p hostd --features kvm
//! --bin migrate-bench`). Tunable via `SLEEPWALK_BENCH_CYCLES` (default 20),
//! `SLEEPWALK_BENCH_SETTLE_MS` (default 1000), `SLEEPWALK_ARTIFACTS`.

fn main() {
    #[cfg(target_os = "linux")]
    linux::main();
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("migrate-bench requires Linux (userfaultfd + KVM)");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use hostd::{
        BootSource, Drive, FcProcess, Firecracker, FirecrackerApi, MachineConfig, MemBackend,
        SnapshotSource, SnapshotTarget, UffdRestoreHandler,
    };

    const GUEST_MIB: u32 = 256;
    const BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro";

    /// A live VM and, if it was UFFD-restored, the handler thread serving it.
    struct Live {
        proc: FcProcess,
        client: Firecracker,
        handler: Option<Handler>,
    }

    /// A running UFFD restore handler thread and the flag that stops it.
    struct Handler {
        stop: Arc<AtomicBool>,
        join: JoinHandle<()>,
    }

    impl Handler {
        fn stop(self) {
            self.stop.store(true, Ordering::Release);
            let _ = self.join.join();
        }
    }

    pub fn main() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(run());
    }

    async fn run() {
        let cycles: usize = env_or("SLEEPWALK_BENCH_CYCLES", 20);
        let settle = Duration::from_millis(env_or("SLEEPWALK_BENCH_SETTLE_MS", 1000));

        let art = artifacts_dir();
        let fc_bin = require(&art, "firecracker binary", |n| {
            n.starts_with("firecracker-") && !n.ends_with(".debug") && !n.ends_with(".tgz")
        });
        let kernel = require(&art, "kernel", |n| n.starts_with("vmlinux"));
        let rootfs = require(&art, "rootfs", |n| {
            n.ends_with(".squashfs") || n.ends_with(".ext4")
        });

        let tmp = std::env::temp_dir().join(format!("sleepwalk-bench-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("tmp dir");

        // Boot the first VM (no handler — plain anonymous memory).
        let mut live = boot_initial(&fc_bin, &kernel, &rootfs, &tmp).await;

        let mut freeze_ms = Vec::with_capacity(cycles);
        let mut bytes_moved = 0u64;

        for i in 0..cycles {
            // Pre-spawn the target + start its page-fault handler BEFORE pausing,
            // so neither counts against the freeze window.
            let mem = tmp.join(format!("mem-{i}.snap"));
            let state = tmp.join(format!("state-{i}.snap"));
            let uffd_sock = tmp.join(format!("uffd-{i}.sock"));
            let tgt_sock = tmp.join(format!("fc-{i}.sock"));
            let tgt_serial = tmp.join(format!("fc-{i}.log"));

            let handler = UffdRestoreHandler::bind(&uffd_sock).expect("bind uffd handler");
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = Arc::clone(&stop);
            let mem_thread = mem.clone();
            let join = std::thread::spawn(move || {
                let _ = handler.serve(&mem_thread, &stop_thread);
            });
            let target_proc = FcProcess::spawn(&fc_bin, &tgt_sock, &tgt_serial, secs(10))
                .expect("spawn target fc");
            let target = Firecracker::new(&tgt_sock);

            // ---- freeze window starts ----
            let t0 = Instant::now();
            live.client.pause().await.expect("pause live");
            live.client
                .create_snapshot(SnapshotTarget {
                    mem_file: mem.clone(),
                    state_file: state.clone(),
                })
                .await
                .expect("snapshot");
            target
                .load_snapshot(SnapshotSource {
                    state_file: state.clone(),
                    backend: MemBackend::Uffd {
                        socket: uffd_sock.clone(),
                    },
                    resume: true,
                })
                .await
                .expect("load + resume");
            let elapsed = t0.elapsed();
            // ---- freeze window ends (target resumed) ----

            freeze_ms.push(elapsed.as_secs_f64() * 1000.0);
            if bytes_moved == 0 {
                bytes_moved = std::fs::metadata(&mem).map(|m| m.len()).unwrap_or(0);
            }
            println!(
                "cycle {:>2}: freeze {:.3} ms",
                i + 1,
                elapsed.as_secs_f64() * 1000.0
            );

            // Retire the old live VM and its handler; the target is now live.
            let old = std::mem::replace(
                &mut live,
                Live {
                    proc: target_proc,
                    client: target,
                    handler: Some(Handler { stop, join }),
                },
            );
            retire(old);

            tokio::time::sleep(settle).await;
        }

        retire(live);
        report(&freeze_ms, bytes_moved, settle, &tmp);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    async fn boot_initial(fc_bin: &Path, kernel: &Path, rootfs: &Path, tmp: &Path) -> Live {
        let sock = tmp.join("fc-initial.sock");
        let serial = tmp.join("fc-initial.log");
        let proc = FcProcess::spawn(fc_bin, &sock, &serial, secs(10)).expect("spawn initial fc");
        let client = Firecracker::new(&sock);
        client
            .configure_machine(MachineConfig {
                vcpu_count: 1,
                mem_size_mib: GUEST_MIB,
            })
            .await
            .expect("machine");
        client
            .configure_boot_source(BootSource {
                kernel_image: kernel.to_path_buf(),
                boot_args: BOOT_ARGS.to_owned(),
            })
            .await
            .expect("boot source");
        client
            .configure_drive(Drive {
                drive_id: "rootfs".to_owned(),
                path_on_host: rootfs.to_path_buf(),
                is_root_device: true,
                is_read_only: true,
            })
            .await
            .expect("drive");
        client.boot().await.expect("boot");
        assert!(
            wait_for_serial(&serial, "login", secs(20)).await,
            "initial VM never reached userspace"
        );
        Live {
            proc,
            client,
            handler: None,
        }
    }

    fn retire(mut live: Live) {
        let _ = live.proc.kill();
        if let Some(handler) = live.handler {
            handler.stop();
        }
    }

    fn report(freeze_ms: &[f64], bytes_moved: u64, settle: Duration, tmp: &Path) {
        if freeze_ms.is_empty() {
            println!("no cycles run");
            return;
        }
        let count = freeze_ms.len();
        let min = freeze_ms.iter().copied().fold(f64::INFINITY, f64::min);
        let max = freeze_ms.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mean = freeze_ms.iter().sum::<f64>() / count as f64;

        println!("\n=== migration freeze window ({count} cycles, single host) ===");
        println!("  min  {min:.3} ms");
        println!("  max  {max:.3} ms");
        println!("  mean {mean:.3} ms");
        println!("  bytes moved (snapshot mem file): {bytes_moved}");

        let json = serde_json::json!({
            "kind": "single_host_freeze_window",
            "cycles": count,
            "guest_mib": GUEST_MIB,
            "settle_ms": settle.as_millis() as u64,
            "bytes_moved": bytes_moved,
            "freeze_ms": freeze_ms,
            "min_ms": min,
            "max_ms": max,
            "mean_ms": mean,
            "note": "single-host snapshot->UFFD-restore; freeze window only, no network transfer; not benchmark-valid on a small/shared box",
        });
        let out = tmp.join("migrate-bench.json");
        if std::fs::write(
            &out,
            serde_json::to_string_pretty(&json).unwrap_or_default(),
        )
        .is_ok()
        {
            println!("\nJSON: {}", out.display());
        }
        // Also print the JSON so it lands in the command output for capture.
        println!("{}", serde_json::to_string(&json).unwrap_or_default());
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
