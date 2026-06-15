//! `loadtest` — drive an open-loop turn load at a live VM and measure latency.
//!
//! Boots the guestd rootfs, completes the boot handshake, then drives turns over
//! vsock with the host-side [`VsockTurnDriver`](hostd::VsockTurnDriver), recording
//! each turn's latency from its *intended* time. This is the steady-state
//! perceived-latency instrument against a real guest: with `freeze=0` it reports a
//! clean baseline (sub-millisecond turns, zero drops).
//!
//! With a non-zero freeze it pauses and resumes the guest mid-run as a crude
//! downtime proxy. Caveat: vsock is host-local and its connection does not survive
//! a pause/resume — turns put on the wire from the pause onward are lost and show
//! as drops, so this *overstates* a real migration (which re-establishes the link
//! on the target and replays gated turns). The faithful perceived-downtime number
//! needs a client reaching the guest over the network, which follows the VM across
//! hosts — the two-host networking step.
//!
//! Usage: `loadtest [rate_per_sec] [duration_secs] [freeze_at_secs] [freeze_ms]`
//! (defaults: 50 1/s over 10s, a 1500ms freeze starting at t=4s; freeze=0 for a
//! pure baseline).
#![cfg(all(target_os = "linux", feature = "kvm"))]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use harness::{Arrivals, Schedule, run_load};
use hostd::{
    BootSource, Drive, FcProcess, Firecracker, FirecrackerApi, GuestLink, MachineConfig,
    VsockConfig, VsockTurnDriver, discover_artifacts,
};

const GUEST_MIB: u32 = 256;
const GUEST_CID: u32 = 3;
const BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw init=/init";

static SEQ: AtomicU64 = AtomicU64::new(0);

fn arg_f64(n: usize, default: f64) -> f64 {
    std::env::args()
        .nth(n)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let rate = arg_f64(1, 50.0);
    let duration = Duration::from_secs_f64(arg_f64(2, 10.0));
    let freeze_at = Duration::from_secs_f64(arg_f64(3, 4.0));
    let freeze = Duration::from_millis(arg_f64(4, 1500.0) as u64);

    let art = discover_artifacts()?;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let work =
        std::env::temp_dir().join(format!("sleepwalk-loadtest-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&work)?;
    let vsock_uds: PathBuf = std::env::temp_dir().join(format!(
        "sleepwalk-loadtest-vsock-{}-{seq}.sock",
        std::process::id()
    ));

    let mut proc = FcProcess::spawn(
        &art.fc_bin,
        &work.join("fc.sock"),
        &work.join("fc.log"),
        Duration::from_secs(10),
    )?;
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
    fc.configure_vsock(VsockConfig {
        guest_cid: GUEST_CID,
        uds_path: vsock_uds.clone(),
    })
    .await?;
    fc.boot().await?;

    let link =
        GuestLink::connect_retry(&vsock_uds, proto::GUEST_VSOCK_PORT, Duration::from_secs(20))
            .await?;
    link.handshake(BTreeMap::new()).await?;
    // A turn that gets no reply within this deadline counts as a client-visible
    // drop. Generous vs. a healthy turn (sub-ms) so only genuinely lost turns
    // (those put on the wire while the guest is paused) hit it.
    let driver = Arc::new(VsockTurnDriver::new(Arc::new(link), Duration::from_secs(3)));

    let schedule = Schedule::generate(rate, duration, Arrivals::Poisson { seed: 1 });
    let n = schedule.len();
    println!(
        "loadtest: {n} turns at {rate}/s over {:?}; freeze {:?} at t={:?}",
        duration, freeze, freeze_at
    );

    // Run the load open-loop; mid-run, freeze the guest to mimic the snapshot
    // window. Turns scheduled during the freeze stall until resume — the spike.
    let load = {
        let driver = Arc::clone(&driver);
        tokio::spawn(async move { run_load(&schedule, driver).await })
    };

    // freeze=0 means a clean baseline run: no pause at all. Note: a pause/resume
    // on a single host does not preserve the vsock connection (it is host-local
    // and its RX queue resets), so turns put on the wire from the pause onward are
    // lost — the freeze here is a crude downtime proxy, not the real number. That
    // number needs a client reaching the guest over the network, which follows the
    // VM across a real migration (the two-host networking step).
    if !freeze.is_zero() {
        tokio::time::sleep(freeze_at).await;
        println!("loadtest: freezing guest for {freeze:?}");
        fc.pause().await?;
        tokio::time::sleep(freeze).await;
        fc.resume().await?;
        println!("loadtest: resumed");
    }

    let stats = load.await?;
    println!(
        "loadtest: count={} dropped={} p50={:?} p99={:?} max={:?}",
        stats.count,
        driver.dropped(),
        stats.p50,
        stats.p99,
        stats.max
    );

    let _ = proc.kill();
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&vsock_uds);
    Ok(())
}
