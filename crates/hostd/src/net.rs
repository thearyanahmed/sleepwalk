//! Per-VM host network plumbing.
//!
//! Each networked VM gets a tap device on the shared `br-sw` bridge (set up by
//! `scripts/net-host.sh`), plus a stable MAC and IP. The MAC and IP are fixed for
//! the VM's life — they are its identity on the network, and must survive a
//! migration so a client's connection follows it — so [`NetId`] captures them and
//! travels with the VM.
//!
//! Linux-only: it shells out to `ip` to create taps. The guest is given its
//! address by the kernel at boot via the `ip=` cmdline ([`boot_arg`]); no in-guest
//! configuration is needed.

use std::process::Command;

use thiserror::Error;

/// The shared bridge per-VM taps attach to (matches `scripts/net-host.sh`).
pub const BRIDGE: &str = "br-sw";
/// The guest gateway (the bridge's address).
pub const GATEWAY: &str = "10.200.0.1";
/// The guest subnet mask.
pub const NETMASK: &str = "255.255.255.0";

/// A failure plumbing a VM's network.
#[derive(Debug, Error)]
pub enum NetError {
    /// An `ip` command failed.
    #[error("`{cmd}` failed: {detail}")]
    Command {
        /// The command line attempted.
        cmd: String,
        /// stderr or the spawn error.
        detail: String,
    },
    /// The VM index is outside the assignable host range (2..=254).
    #[error("vm network index {0} out of range (max 252)")]
    OutOfRange(u32),
}

/// A VM's network identity: its host tap, guest MAC, and guest IP.
#[derive(Debug, Clone)]
pub struct NetId {
    /// The host tap device backing the VM's interface.
    pub tap: String,
    /// The guest MAC (stable across migration).
    pub mac: String,
    /// The guest IP (stable across migration).
    pub ip: String,
}

/// Allocate a [`NetId`] for VM index `idx` and create its tap on the bridge, up.
///
/// # Errors
/// If `idx` exceeds the host range, or an `ip` command fails.
pub fn create(idx: u32) -> Result<NetId, NetError> {
    let host = idx
        .checked_add(2)
        .filter(|h| *h <= 254)
        .ok_or(NetError::OutOfRange(idx))?;
    let ip = format!("10.200.0.{host}");
    let mac = format!("02:00:00:00:00:{host:02x}");
    let tap = format!("sw-tap{idx}");

    run(&["tuntap", "add", &tap, "mode", "tap"])?;
    run(&["link", "set", &tap, "master", BRIDGE])?;
    run(&["link", "set", &tap, "up"])?;
    Ok(NetId { tap, mac, ip })
}

/// Tear down a VM's tap (best effort — used on teardown).
pub fn destroy(tap: &str) {
    let _ = run(&["link", "del", tap]);
}

/// The kernel cmdline fragment that configures `eth0` from `net` at boot.
#[must_use]
pub fn boot_arg(net: &NetId) -> String {
    format!("ip={}::{}:{}::eth0:off", net.ip, GATEWAY, NETMASK)
}

/// Run an `ip` subcommand, mapping a non-zero exit to [`NetError`].
fn run(args: &[&str]) -> Result<(), NetError> {
    let out = Command::new("ip")
        .args(args)
        .output()
        .map_err(|e| NetError::Command {
            cmd: format!("ip {}", args.join(" ")),
            detail: e.to_string(),
        })?;
    if out.status.success() {
        Ok(())
    } else {
        Err(NetError::Command {
            cmd: format!("ip {}", args.join(" ")),
            detail: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        })
    }
}
