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
    use std::time::{SystemTime, UNIX_EPOCH};

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
        let mut child: Option<Child> = None;
        let mut lines = None;
        let mut child_done = false;
        loop {
            let chan = match serve(DEFAULT_PORT).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("guestd: vsock serve: {e}");
                    continue;
                }
            };
            let mut g = Guest::new(VmId::new(), version.clone(), chan);
            if let Err(e) = g.handshake().await {
                eprintln!("guestd: handshake: {e}");
                continue;
            }

            if child.is_none() {
                match spawn_child(&cmd, &g) {
                    Ok((c, l)) => {
                        child = Some(c);
                        lines = Some(l);
                        println!("guestd: handshake complete; child running");
                    }
                    Err(e) => {
                        eprintln!("guestd: spawn `{cmd}`: {e}");
                        std::process::exit(1);
                    }
                }
            } else {
                // Reconnected on the target host after a restore: announce we are
                // alive again (the `Resumed` clock fix-up trigger); the child and
                // its in-RAM state came across in the snapshot untouched.
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

            let Some(reader) = lines.as_mut() else {
                eprintln!("guestd: wrap: child stdout missing");
                serve_messages_only(&mut g).await;
                continue;
            };

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
                    line = reader.next_line() => match line {
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

    /// Read the wrap-mode configuration from the environment, or `None` for the
    /// host-driven mode. `SLEEPWALK_WRAP_CMD` selects wrap mode and is the
    /// command run under `/bin/sh -c`; `SLEEPWALK_WRAP_START` / `_END` override
    /// the default turn-boundary markers.
    fn wrap_config_from_env() -> Option<(String, WrapConfig)> {
        let cmd = std::env::var("SLEEPWALK_WRAP_CMD").ok()?;
        let mut cfg = WrapConfig::default();
        if let Ok(s) = std::env::var("SLEEPWALK_WRAP_START") {
            cfg.start_marker = s;
        }
        if let Ok(e) = std::env::var("SLEEPWALK_WRAP_END") {
            cfg.end_marker = e;
        }
        Some((cmd, cfg))
    }

    /// Spawn the wrapped command with the boot secrets as its environment, and
    /// return its handle plus a line reader over its stdout.
    fn spawn_child<C: GuestChannel>(
        cmd: &str,
        g: &Guest<C>,
    ) -> std::io::Result<(
        Child,
        tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    )> {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(cmd).stdout(Stdio::piped());
        // Secrets reach the workload via the environment only — never the rootfs,
        // never the kernel cmdline (visible in /proc/cmdline and host `ps`).
        for (k, v) in g.secrets() {
            command.env(k, v);
        }
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("child stdout not piped"))?;
        Ok((child, BufReader::new(stdout).lines()))
    }
}
