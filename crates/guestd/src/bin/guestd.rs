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
    use std::process::Stdio;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use guestd::GuestChannel;
    use guestd::guest::Guest;
    use guestd::vsock::{DEFAULT_PORT, serve};
    use guestd::wrap::{WrapConfig, apply_signal};
    use proto::{GuestdVersion, Timestamp, VmId};
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::{Child, Command};

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
        // Wrap mode if a command is declared; otherwise the host drives turns.
        match wrap_config_from_env() {
            Some((cmd, cfg)) => run_wrapped(version, cmd, cfg).await,
            None => run_host_driven(version).await,
        }
    }

    /// Serve host messages on one connection until the host disconnects (recv
    /// errors) or a handler fails. The shared inner loop of both modes.
    async fn serve_messages_only<C: GuestChannel>(g: &mut Guest<C>) {
        while let Ok(msg) = g.channel().recv().await {
            if let Err(e) = g.handle(msg, now()).await {
                eprintln!("guestd: handle: {e}");
                break;
            }
        }
    }

    /// Native / host-driven mode: turns arrive as `RunTurn` over vsock. One host
    /// session at a time; loop so the target host can reconnect after a move.
    async fn run_host_driven(version: GuestdVersion) {
        loop {
            // Bind+accept with a timeout so a listener gone stale across a
            // snapshot/restore (its accept never fires on the new host) is dropped
            // and re-bound. Without this a restored VM can never be reconnected to,
            // so it can't be drained for a re-migration — the "terminal restored
            // VM" limitation.
            let chan = match tokio::time::timeout(Duration::from_secs(5), serve(DEFAULT_PORT)).await
            {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    eprintln!("guestd: vsock serve: {e}");
                    continue;
                }
                Err(_) => continue, // accept timed out — re-bind a fresh listener
            };
            // A fresh VmId per process; hostd binds the connection to it via the
            // Hello. (A real deployment passes the id in on the kernel cmdline.)
            let mut g = Guest::new(VmId::new(), version.clone(), chan);
            if let Err(e) = g.handshake().await {
                eprintln!("guestd: handshake: {e}");
                continue;
            }
            println!("guestd: handshake complete; serving host messages");
            serve_messages_only(&mut g).await;
        }
    }

    /// Wrap mode (zero-code adoption): supervise `cmd` and infer turn
    /// boundaries from its stdout per `cfg`. The child is spawned once, after the
    /// first handshake hands it the boot secrets as environment, and outlives
    /// every host (re)connection — its in-RAM state is exactly what the snapshot
    /// carries across a migration. guestd is re-created per connection (the
    /// source host's connection dies at restore; the target host dials in fresh).
    async fn run_wrapped(version: GuestdVersion, cmd: String, cfg: WrapConfig) {
        println!("guestd: wrap mode — supervising `{cmd}`");
        // Start the workload immediately — it must not wait for a host to dial
        // in. Its in-RAM state is what the snapshot carries across a migration;
        // guestd is re-created per host connection (the source connection dies at
        // restore; the target host dials in fresh), but the child runs throughout.
        let (_child, mut lines) = match spawn_child(&cmd) {
            Ok(cl) => cl,
            Err(e) => {
                eprintln!("guestd: spawn `{cmd}`: {e}");
                std::process::exit(1);
            }
        };
        let mut child_done = false;
        let mut first_connection = true;
        loop {
            // Bind+accept with a timeout so a listener gone stale across a
            // snapshot/restore (its accept never fires on the new host) is dropped
            // and re-bound. Without this a restored VM can never be reconnected to,
            // so it can't be drained for a re-migration — the "terminal restored
            // VM" limitation.
            let chan = match tokio::time::timeout(Duration::from_secs(5), serve(DEFAULT_PORT)).await
            {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    eprintln!("guestd: vsock serve: {e}");
                    continue;
                }
                Err(_) => continue, // accept timed out — re-bind a fresh listener
            };
            let mut g = Guest::new(VmId::new(), version.clone(), chan);
            if let Err(e) = g.handshake().await {
                eprintln!("guestd: handshake: {e}");
                continue;
            }

            if first_connection {
                first_connection = false;
                println!("guestd: host connected; child already running");
            } else {
                // A later connection means we came up on a new host after a
                // restore: announce we are alive (the `Resumed` clock fix-up
                // trigger); the child and its in-RAM state rode the snapshot here.
                if let Err(e) = g.resume(now()).await {
                    eprintln!("guestd: resume: {e}");
                    continue;
                }
                println!("guestd: resumed on new host; child intact");
            }

            // Once the child has closed stdout there is nothing left to infer —
            // just keep serving host messages (PID 1 must never exit).
            if child_done {
                serve_messages_only(&mut g).await;
                continue;
            }

            loop {
                tokio::select! {
                    // Host message (drain, ping, secrets, …).
                    msg = g.channel().recv() => match msg {
                        Ok(m) => {
                            if let Err(e) = g.handle(m, now()).await {
                                eprintln!("guestd: handle: {e}");
                                break;
                            }
                        }
                        Err(_) => break, // host gone; reconnect (e.g. after a move)
                    },
                    // A line of child output: a turn boundary, or pass-through.
                    line = lines.next_line() => match line {
                        Ok(Some(l)) => match cfg.classify(&l) {
                            Some(sig) => {
                                if let Err(e) = apply_signal(&mut g, sig, now()).await {
                                    eprintln!("guestd: turn signal: {e}");
                                }
                            }
                            None => println!("{l}"), // forward ordinary output
                        },
                        Ok(None) => {
                            println!("guestd: wrapped child closed stdout");
                            child_done = true;
                            break;
                        }
                        Err(e) => {
                            eprintln!("guestd: child stdout: {e}");
                            break;
                        }
                    },
                }
            }

            // If the child finished mid-connection, serve the rest of it.
            if child_done {
                serve_messages_only(&mut g).await;
            }
        }
    }

    /// Path the rootfs can drop a wrap command into when no env is set — how the
    /// minimal guest image (which has no shell to export env) selects wrap mode.
    const WRAP_CMD_FILE: &str = "/etc/sleepwalk/wrap-cmd";

    /// Resolve the wrap-mode configuration, or `None` for host-driven mode.
    ///
    /// The command comes from `SLEEPWALK_WRAP_CMD`, or failing that the contents
    /// of [`WRAP_CMD_FILE`] (so a baked rootfs needs no env). It is exec'd
    /// directly, split on whitespace — there is no shell in the minimal guest, so
    /// shell syntax is not available. `SLEEPWALK_WRAP_START` / `_END` override the
    /// default turn-boundary markers.
    fn wrap_config_from_env() -> Option<(String, WrapConfig)> {
        let cmd = std::env::var("SLEEPWALK_WRAP_CMD")
            .ok()
            .or_else(|| std::fs::read_to_string(WRAP_CMD_FILE).ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())?;
        let mut cfg = WrapConfig::default();
        if let Ok(s) = std::env::var("SLEEPWALK_WRAP_START") {
            cfg.start_marker = s;
        }
        if let Ok(e) = std::env::var("SLEEPWALK_WRAP_END") {
            cfg.end_marker = e;
        }
        Some((cmd, cfg))
    }

    /// Spawn the wrapped command (exec'd directly, argv split on whitespace) and
    /// return its handle plus a line reader over its stdout.
    ///
    /// The child starts at boot, before any host connects, so it does not receive
    /// the `Secrets` env (which arrives on a later handshake) — wrap mode is for
    /// workloads that need no boot secret. Native mode is the path for secret
    /// handoff at exec.
    fn spawn_child(
        cmd: &str,
    ) -> std::io::Result<(
        Child,
        tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    )> {
        let mut argv = cmd.split_whitespace();
        let program = argv
            .next()
            .ok_or_else(|| std::io::Error::other("empty wrap command"))?;
        let mut command = Command::new(program);
        command.args(argv).stdout(Stdio::piped());
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("child stdout not piped"))?;
        Ok((child, BufReader::new(stdout).lines()))
    }
}
