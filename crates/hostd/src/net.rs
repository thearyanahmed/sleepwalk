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

use serde::{Deserialize, Serialize};
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
    /// Failed to broadcast the post-restore gratuitous ARP.
    #[error("gratuitous ARP announce failed: {0}")]
    Announce(String),
}

/// A VM's network identity: its host tap, guest MAC, and guest IP. Serializable
/// because it travels with a migration (the target re-creates the same tap before
/// restoring, so the guest keeps its MAC/IP on the new host).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    let net = NetId {
        tap: format!("sw-tap{idx}"),
        mac: format!("02:00:00:00:00:{host:02x}"),
        ip: format!("10.200.0.{host}"),
    };
    create_tap(&net.tap)?;
    Ok(net)
}

/// Create (or re-create) tap `name` on the bridge, up. Used on restore to
/// re-plumb a migrated VM's tap under the name baked into its snapshot.
///
/// # Errors
/// If an `ip` command fails.
pub fn create_tap(name: &str) -> Result<(), NetError> {
    run(&["tuntap", "add", name, "mode", "tap"])?;
    run(&["link", "set", name, "master", BRIDGE])?;
    run(&["link", "set", name, "up"])?;
    Ok(())
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

/// Broadcast a **gratuitous ARP** for `net` on the bridge so every host on the
/// overlay relearns the VM's location immediately after a migration.
///
/// Without it, the source host's bridge still believes the VM's MAC is local (on
/// the now-deleted tap) and must flood-and-relearn that it moved across the VXLAN
/// tunnel — measured at seconds of post-restore blackhole on the tunnel path,
/// even though the guest itself resumes in tens of milliseconds. The guest, now
/// on the target, shouting "my IP is at my MAC, here" collapses that to ~zero.
/// Best-effort: callers log and continue on error.
///
/// # Errors
/// If the MAC/IP can't be parsed or the raw frame can't be sent.
#[cfg(target_os = "linux")]
pub fn announce(net: &NetId) -> Result<(), NetError> {
    use std::ffi::CString;

    use nix::libc;

    let mac = parse_mac(&net.mac)?;
    let ip = parse_ipv4(&net.ip)?;

    // Ethernet (14) + ARP (28) = 42 bytes. Gratuitous ARP request: sender and
    // target protocol address are both the guest's, sender HW its MAC, broadcast.
    let mut frame = [0u8; 42];
    frame[0..6].copy_from_slice(&[0xff; 6]); // eth dst: broadcast
    frame[6..12].copy_from_slice(&mac); // eth src: guest MAC
    frame[12..14].copy_from_slice(&[0x08, 0x06]); // ethertype: ARP
    frame[14..16].copy_from_slice(&[0x00, 0x01]); // htype: Ethernet
    frame[16..18].copy_from_slice(&[0x08, 0x00]); // ptype: IPv4
    frame[18] = 6; // hlen
    frame[19] = 4; // plen
    frame[20..22].copy_from_slice(&[0x00, 0x01]); // op: request
    frame[22..28].copy_from_slice(&mac); // sender HW
    frame[28..32].copy_from_slice(&ip); // sender IP
    // target HW left zero (frame[32..38])
    frame[38..42].copy_from_slice(&ip); // target IP == sender IP (gratuitous)

    let ifname = CString::new(BRIDGE).map_err(|e| NetError::Announce(e.to_string()))?;
    // SAFETY: `if_nametoindex` reads a NUL-terminated name; returns 0 on error.
    let ifindex = unsafe { libc::if_nametoindex(ifname.as_ptr()) };
    if ifindex == 0 {
        return Err(NetError::Announce(format!("no interface {BRIDGE}")));
    }
    let proto = i32::from((libc::ETH_P_ARP as u16).to_be());
    // SAFETY: a raw AF_PACKET socket to send one frame; checked for error.
    let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW, proto) };
    if fd < 0 {
        return Err(NetError::Announce(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    // SAFETY: zeroed sockaddr_ll filled with a valid family/ifindex/dst below.
    let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    addr.sll_family = libc::AF_PACKET as u16;
    addr.sll_protocol = (libc::ETH_P_ARP as u16).to_be();
    addr.sll_ifindex = ifindex as i32;
    addr.sll_halen = 6;
    addr.sll_addr[..6].copy_from_slice(&[0xff; 6]);
    // SAFETY: send `frame` on the bound interface; `addr` is a valid sockaddr_ll.
    let sent = unsafe {
        libc::sendto(
            fd,
            frame.as_ptr().cast(),
            frame.len(),
            0,
            std::ptr::addr_of!(addr).cast(),
            std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    };
    // SAFETY: closing the socket we just opened.
    unsafe { libc::close(fd) };
    if sent < 0 {
        return Err(NetError::Announce(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok(())
}

/// Non-Linux stub so the crate builds on a dev box; the raw frame is Linux-only.
#[cfg(not(target_os = "linux"))]
pub fn announce(_net: &NetId) -> Result<(), NetError> {
    Ok(())
}

/// Parse a `aa:bb:cc:dd:ee:ff` MAC into 6 bytes.
#[cfg(target_os = "linux")]
fn parse_mac(s: &str) -> Result<[u8; 6], NetError> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return Err(NetError::Announce(format!("bad mac {s}")));
    }
    let mut out = [0u8; 6];
    for (b, p) in out.iter_mut().zip(parts) {
        *b = u8::from_str_radix(p, 16).map_err(|_| NetError::Announce(format!("bad mac {s}")))?;
    }
    Ok(out)
}

/// Parse a dotted-quad IPv4 into 4 bytes.
#[cfg(target_os = "linux")]
fn parse_ipv4(s: &str) -> Result<[u8; 4], NetError> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return Err(NetError::Announce(format!("bad ip {s}")));
    }
    let mut out = [0u8; 4];
    for (b, p) in out.iter_mut().zip(parts) {
        *b = p
            .parse()
            .map_err(|_| NetError::Announce(format!("bad ip {s}")))?;
    }
    Ok(out)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn parses_mac_and_ip() {
        assert_eq!(
            parse_mac("02:00:00:00:00:02").unwrap(),
            [0x02, 0, 0, 0, 0, 0x02]
        );
        assert_eq!(parse_ipv4("10.200.0.2").unwrap(), [10, 200, 0, 2]);
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse_mac("02:00:00").is_err());
        assert!(parse_mac("zz:00:00:00:00:00").is_err());
        assert!(parse_ipv4("10.200.0").is_err());
        assert!(parse_ipv4("10.200.0.999").is_err());
    }
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
