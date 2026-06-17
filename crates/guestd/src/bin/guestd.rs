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
    use std::collections::BTreeMap;
    use std::process::Stdio;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use guestd::GuestChannel;
    use guestd::guest::Guest;
    use guestd::vsock::{DEFAULT_PORT, serve};
    use guestd::wrap::{TurnTracker, WrapConfig};
    use proto::{GuestToHost, GuestdVersion, HostToGuest, Timestamp, VmId};
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

    /// Serve the guest protocol over TCP on the guest network, in parallel with
    /// the vsock listener. This is the channel hostd uses to **drain** the guest:
    /// Firecracker's vsock stops servicing connections after a snapshot restore,
    /// but the guest network survives, so a restored VM is reachable here — which
    /// is what makes draining (and re-migrating) a restored VM possible. One
    /// session at a time; loops to re-accept (the next host, after a move).
    async fn serve_tcp_drain(version: GuestdVersion, tracker: TurnTracker) {
        use tokio::net::TcpListener;
        let port = proto::GUEST_DRAIN_TCP_PORT;
        loop {
            let listener = match TcpListener::bind(("0.0.0.0", port)).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("guestd: tcp drain bind: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            loop {
                let stream = match listener.accept().await {
                    Ok((s, _)) => s,
                    Err(_) => break, // re-bind
                };
                let chan = guestd::JsonLineChannel::new(stream);
                let mut g = Guest::new(VmId::new(), version.clone(), chan);
                if g.handshake().await.is_err() {
                    continue;
                }
                // Answer drains from the shared turn tracker (the child reader keeps
                // it live) — so a restored VM drained over TCP reports its live turn
                // state, never "quiescent" mid-turn.
                serve_wrap_messages(g.channel(), &tracker).await;
            }
        }
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
        // One turn tracker, shared by the wrap child reader (writer) and both drain
        // responders (vsock + TCP). Empty/quiescent in host-driven mode (no child).
        let tracker = TurnTracker::new();
        // Also serve the protocol over TCP on the guest network — the drain
        // channel that survives a snapshot restore (vsock does not).
        tokio::spawn(serve_tcp_drain(version.clone(), tracker.clone()));
        // Wrap mode if a command is declared; otherwise the host drives turns.
        match wrap_config_from_env() {
            Some((cmd, cfg)) => run_wrapped(version, cmd, cfg, wrap_await_secrets(), tracker).await,
            None => run_host_driven(version).await,
        }
    }

    /// Serve a host connection in wrap mode: answer drains from the shared turn
    /// tracker and reply to liveness pings. Wrap mode is **passive** — it does not
    /// gate or queue turns (`DrainCancel`/`RunTurn` are no-ops) and secrets are
    /// delivered to the child at spawn — so no `Guest` state is needed here. Returns
    /// when the host disconnects; the caller re-accepts.
    async fn serve_wrap_messages<C: GuestChannel>(chan: &C, tracker: &TurnTracker) {
        while let Ok(msg) = chan.recv().await {
            let res = match msg {
                HostToGuest::DrainRequest { .. } => {
                    chan.send(GuestToHost::DrainAck {
                        in_flight: tracker.in_flight(),
                    })
                    .await
                }
                HostToGuest::Ping => chan.send(GuestToHost::Pong).await,
                _ => Ok(()),
            };
            if res.is_err() {
                break; // host gone; caller re-accepts (e.g. after a move)
            }
        }
    }

    /// Continuously drain the wrapped child's stdout in its own task: classify each
    /// line into a turn boundary (updating `tracker`) or forward it to the log. This
    /// runs for the child's whole life, independent of any host connection — so the
    /// stdout pipe never blocks and a drain always sees live turn state. The `Child`
    /// handle is held here to keep the process from being reaped early.
    fn spawn_reader(child: Child, mut lines: ChildLines, cfg: WrapConfig, tracker: TurnTracker) {
        tokio::spawn(async move {
            let _child = child;
            loop {
                match lines.next_line().await {
                    Ok(Some(l)) => match cfg.classify(&l) {
                        Some(sig) => tracker.apply(sig),
                        None => println!("{l}"),
                    },
                    Ok(None) => {
                        println!("guestd: wrapped child closed stdout");
                        break;
                    }
                    Err(e) => {
                        eprintln!("guestd: child stdout: {e}");
                        break;
                    }
                }
            }
        });
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
            // Bind+accept with a timeout so a listener that went stale is dropped
            // and re-bound rather than blocking forever. (Note: this does NOT by
            // itself make a *restored* VM reconnectable — Firecracker's virtio-vsock
            // does not deliver host->guest connections to a guest after snapshot
            // restore, so a restored VM still can't be drained/re-migrated over the
            // host-initiated channel; that needs a guest-initiated reconnect.)
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

    /// Wrap mode (zero-code adoption): supervise `cmd` and infer turn boundaries
    /// from its stdout per `cfg`. The child is spawned once and read continuously by
    /// a dedicated task (see [`spawn_reader`]) that keeps the shared `tracker` live;
    /// its in-RAM state rides the snapshot across a migration. This loop only
    /// accepts host connections over vsock and answers their drains/pings from the
    /// tracker — child reading is decoupled from it, so the stdout pipe never blocks
    /// and drain state stays correct even with no host connected.
    ///
    /// Spawn timing: immediately at boot for the synthetic profile (no secret), or
    /// deferred until the first handshake delivers `Secrets` for the await-secrets
    /// profile (e.g. a coding agent that needs an API key at exec).
    async fn run_wrapped(
        version: GuestdVersion,
        cmd: String,
        cfg: WrapConfig,
        await_secrets: bool,
        tracker: TurnTracker,
    ) {
        println!("guestd: wrap mode — supervising `{cmd}`");
        let mut spawned = false;
        if !await_secrets {
            match spawn_child(&cmd, &BTreeMap::new()) {
                Ok((child, lines)) => {
                    spawn_reader(child, lines, cfg.clone(), tracker.clone());
                    spawned = true;
                }
                Err(e) => {
                    eprintln!("guestd: spawn `{cmd}`: {e}");
                    std::process::exit(1);
                }
            }
        }
        let mut first_connection = true;
        loop {
            // Bind+accept with a timeout so a stale listener is dropped and re-bound
            // rather than blocking forever. (vsock does not deliver host->guest
            // connections after a snapshot restore; a *restored* VM is drained over
            // the TCP channel instead — see `serve_tcp_drain`.)
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
                if await_secrets && !spawned {
                    // Deferred spawn: the handshake just delivered `Secrets`, so
                    // start the child now with them in its environment.
                    match spawn_child(&cmd, g.secrets()) {
                        Ok((child, lines)) => {
                            spawn_reader(child, lines, cfg.clone(), tracker.clone());
                            spawned = true;
                            println!("guestd: secrets received; child started");
                        }
                        Err(e) => {
                            eprintln!("guestd: spawn `{cmd}`: {e}");
                            std::process::exit(1);
                        }
                    }
                } else {
                    println!("guestd: host connected; child running");
                }
            } else if let Err(e) = g.resume(now()).await {
                // A later connection means a restore on a new host: announce we are
                // alive (the `Resumed` clock fix-up trigger).
                eprintln!("guestd: resume: {e}");
                continue;
            } else {
                println!("guestd: resumed on new host; child intact");
            }

            serve_wrap_messages(g.channel(), &tracker).await;
        }
    }

    /// A line reader over the wrapped child's stdout.
    type ChildLines = tokio::io::Lines<BufReader<tokio::process::ChildStdout>>;

    /// Path the rootfs can drop a wrap command into when no env is set — how the
    /// minimal guest image (which has no shell to export env) selects wrap mode.
    const WRAP_CMD_FILE: &str = "/etc/sleepwalk/wrap-cmd";

    /// Presence selects "defer the wrapped child until the first handshake delivers
    /// `Secrets`, then spawn it with them in its environment" — the path a workload
    /// that needs a boot secret (e.g. a coding agent's API key) uses. Created by the
    /// rootfs build for that profile; absent for the synthetic profile.
    const WRAP_AWAIT_SECRETS_FILE: &str = "/etc/sleepwalk/wrap-await-secrets";

    /// Whether to defer the wrapped child until `Secrets` arrive (see
    /// [`WRAP_AWAIT_SECRETS_FILE`]). Off by default; on via that file or the
    /// `SLEEPWALK_WRAP_AWAIT_SECRETS` env var.
    fn wrap_await_secrets() -> bool {
        std::env::var_os("SLEEPWALK_WRAP_AWAIT_SECRETS").is_some()
            || std::path::Path::new(WRAP_AWAIT_SECRETS_FILE).exists()
    }

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

    /// Spawn the wrapped command (exec'd directly, argv split on whitespace) with
    /// `env` in its environment, returning its handle plus a stdout line reader.
    ///
    /// `env` is empty for the synthetic profile (spawned at boot, no secret) and
    /// the handshake `Secrets` for the await-secrets profile (spawned after the
    /// first handshake so a coding agent gets its API key at exec). Secrets stay in
    /// memory only — never the rootfs image, never the kernel cmdline.
    fn spawn_child(cmd: &str, env: &BTreeMap<String, String>) -> std::io::Result<(Child, ChildLines)> {
        let mut argv = cmd.split_whitespace();
        let program = argv
            .next()
            .ok_or_else(|| std::io::Error::other("empty wrap command"))?;
        let mut command = Command::new(program);
        command.args(argv).envs(env).stdout(Stdio::piped());
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("child stdout not piped"))?;
        Ok((child, BufReader::new(stdout).lines()))
    }
}
