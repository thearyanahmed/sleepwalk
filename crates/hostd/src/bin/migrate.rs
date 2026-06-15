//! `migrate` — CLI benchmark wrapper over [`hostd::migrate`].
//!
//!   migrate recv <listen_addr> [count]   # restore `count` migrations
//!   migrate send <target_addr> [count]   # migrate `count` times, report stats
//!
//! `recv` must be listening before `send` connects, with the same `count`. Over
//! loopback both run on one host; cross-host, run `recv` on B and point `send`
//! at B's IP (CPU-homogeneous hosts — ADR-004). A transfer that drops is retried
//! per run; nothing panics. Times the source freeze window (pause → snapshot →
//! transfer-complete); the target's lazy restore/resume is not included. The
//! daemon (`hostd`) is the production path; this binary is for measurement.
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
    use std::time::Duration;

    use anyhow::{Context, Result};
    use hostd::{
        SourceTiming, bind_receiver, discover_artifacts, migrate_source, restore_target, telemetry,
    };

    /// Attempts per run before the source gives up.
    const MAX_TRIES: u32 = 3;

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        let addr = args.get(2).cloned();
        let count: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1);
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("migrate: tokio runtime: {e}");
                std::process::exit(1);
            }
        };
        let result = match (args.get(1).map(String::as_str), addr) {
            (Some("recv"), Some(addr)) => rt.block_on(recv(&addr, count)),
            (Some("send"), Some(addr)) => rt.block_on(send(&addr, count)),
            _ => {
                eprintln!("usage: migrate <recv|send> <addr> [count]");
                std::process::exit(2);
            }
        };
        if let Err(e) = result {
            eprintln!("migrate: {e:#}");
            std::process::exit(1);
        }
    }

    async fn send(addr: &str, count: usize) -> Result<()> {
        let art = discover_artifacts().context("discover artifacts")?;
        let mut timings: Vec<SourceTiming> = Vec::with_capacity(count);
        let mut done = 0usize;
        while done < count {
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                match migrate_source(&art, addr).await {
                    Ok(t) => {
                        done += 1;
                        println!(
                            "run {:>2}/{}: snapshot {:.1} ms, transfer {:.1} ms, total {:.1} ms",
                            done,
                            count,
                            ms(t.snapshot),
                            ms(t.transfer),
                            ms(t.snapshot) + ms(t.transfer)
                        );
                        timings.push(t);
                        break;
                    }
                    Err(e) => {
                        telemetry::migration_failed();
                        eprintln!("run {} attempt {attempt}/{MAX_TRIES} failed: {e}", done + 1);
                        if attempt >= MAX_TRIES {
                            return Err(anyhow::Error::new(e).context(format!(
                                "run {} failed after {MAX_TRIES} attempts",
                                done + 1
                            )));
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
        report(&timings);
        Ok(())
    }

    async fn recv(addr: &str, count: usize) -> Result<()> {
        let art = discover_artifacts().context("discover artifacts")?;
        let listener = bind_receiver(addr).await.context("bind receiver")?;
        println!("[recv] listening on {addr} for {count} migration(s)");
        let mut done = 0usize;
        while done < count {
            match restore_target(&art.fc_bin, &listener).await {
                Ok(()) => {
                    done += 1;
                    println!("[recv] {done}/{count} restored and resumed");
                }
                Err(e) => {
                    telemetry::migration_failed();
                    eprintln!("[recv] migration failed, awaiting retry: {e}");
                }
            }
        }
        println!("[recv] done ({count} migrations)");
        Ok(())
    }

    fn report(timings: &[SourceTiming]) {
        if timings.is_empty() {
            println!("no successful runs");
            return;
        }
        let snaps: Vec<f64> = timings.iter().map(|t| ms(t.snapshot)).collect();
        let xfers: Vec<f64> = timings.iter().map(|t| ms(t.transfer)).collect();
        let totals: Vec<f64> = timings
            .iter()
            .map(|t| ms(t.snapshot) + ms(t.transfer))
            .collect();
        let bytes = timings[0].bytes;
        let stat = |v: &[f64]| {
            let n = v.len() as f64;
            (
                v.iter().copied().fold(f64::INFINITY, f64::min),
                v.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                v.iter().sum::<f64>() / n,
            )
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
            "bytes_moved": bytes,
            "snapshot_ms": snaps,
            "transfer_ms": xfers,
            "total_ms": totals,
            "total_min_ms": tmin, "total_max_ms": tmax, "total_mean_ms": tmean,
            "note": "source side only (pause->snapshot->transfer-complete); excludes target restore/resume; 1 vCPU, not publication-valid",
        });
        println!("{}", serde_json::to_string(&json).unwrap_or_default());
    }

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1000.0
    }
}
