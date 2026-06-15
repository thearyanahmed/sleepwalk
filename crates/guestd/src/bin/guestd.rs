//! `guestd` — the in-VM supervisor process.
//!
//! Runs inside the microVM (as the init process of a minimal rootfs): it listens
//! on vsock, completes the boot handshake with hostd, and then reacts to host
//! messages (secrets, drain requests, liveness) for the life of the guest. It
//! never exits — as PID 1 that would panic the kernel, and as the supervisor it
//! is meant to stay up across migrations, re-accepting the host's connection on
//! the new machine.
//!
//! Linux-only (vsock). Built static against musl for the rootfs; see
//! `scripts/build-guest-rootfs.sh`.

fn main() {
    #[cfg(target_os = "linux")]
    linux::main();
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("guestd runs inside a Linux microVM (vsock)");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::time::{SystemTime, UNIX_EPOCH};

    use guestd::GuestChannel;
    use guestd::guest::Guest;
    use guestd::vsock::{DEFAULT_PORT, serve};
    use proto::{GuestdVersion, Timestamp, VmId};

    /// The guest's wall clock as a protocol timestamp. A pre-epoch clock (unset
    /// RTC) clamps to 0; the host applies clock fix-up on resume regardless.
    fn now() -> Timestamp {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Timestamp::from_nanos(nanos)
    }

    pub fn main() {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("guestd: tokio runtime: {e}");
                std::process::exit(1);
            }
        };
        rt.block_on(run());
    }

    async fn run() {
        let version = GuestdVersion::new(env!("CARGO_PKG_VERSION"));
        println!("guestd: listening on vsock port {DEFAULT_PORT}");
        // One host session at a time; loop so the host can reconnect (e.g. after
        // a migration the target host dials in again).
        loop {
            let chan = match serve(DEFAULT_PORT).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("guestd: vsock serve: {e}");
                    continue;
                }
            };
            // A fresh VmId per process; hostd binds the connection to it via the
            // Hello. (A real deployment passes the id in on the kernel cmdline.)
            let mut g = Guest::new(VmId::new(), version.clone(), chan);
            if let Err(e) = g.handshake().await {
                eprintln!("guestd: handshake: {e}");
                continue;
            }
            println!("guestd: handshake complete; serving host messages");
            // Serve host messages until the host disconnects (recv errors), then
            // loop back to re-accept a session (e.g. the target host after a move).
            while let Ok(msg) = g.channel().recv().await {
                if let Err(e) = g.handle(msg, now()).await {
                    eprintln!("guestd: handle: {e}");
                    break;
                }
            }
        }
    }
}
