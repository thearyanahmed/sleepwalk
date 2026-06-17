# Networking

A migrated VM must keep its identity so that anything talking to it follows it to the
new host. sleepwalk does this by carrying the network identity with the snapshot and
re-creating it on the target before the VM resumes.

## Network identity travels with the snapshot

For a networked VM the source writes `net.json` alongside the snapshot — the tap device
name, MAC, and IP. The target reads it and **re-plumbs the tap before loading**:

```rust
// target side, before load_snapshot:
let net: NetId = serde_json::from_slice(&bytes)?;
create_tap(&net.tap)?;            // same name, on the overlay bridge
```

The snapshot itself names that host tap (`host_dev_name`), which Firecracker re-binds
on load — so the tap must already exist, under the same name. Because the tap, MAC, and
IP are reconstructed identically, **the guest keeps its MAC/IP on the new host**, and a
client's connection follows it.

## Gratuitous ARP on resume

After the VM resumes, the target broadcasts a **gratuitous ARP** (`net::announce`) so
every host on the overlay relearns the VM's MAC immediately:

```rust
if let Some(net) = &net {
    if let Err(e) = net::announce(net) {
        eprintln!("hostd: gratuitous ARP for {}: {e}", net.ip);
    }
}
```

Without it the source bridge would flood and age for several seconds before discovering
the VM moved across the tunnel — a window of black-holed packets. The ARP is
best-effort; a failure is logged, not fatal.

## Two transport channels to the guest

The guest protocol is served on **both** vsock and a TCP port on the guest network:

| Channel | Used for | Survives restore? |
|---------|----------|-------------------|
| **vsock** (`GUEST_VSOCK_PORT`) | Boot handshake, turns, secret handoff. | **No** — Firecracker stops servicing vsock after a snapshot restore. |
| **Guest-net TCP** (`GUEST_DRAIN_TCP_PORT`) | Draining a *restored* VM, so it can be re-migrated. | **Yes** — the guest network survives a restore. |

This is why a networked VM is drained over TCP on the [source side](../migration/source.md):
it is the only channel that lets an already-migrated VM be drained and moved again.

## The agent profile's egress

The [agent demo](demos.md) needs the guest to reach an external model endpoint over
HTTPS. That requires guest egress — tap → NAT, with a double NAT through the dev VM on
the macOS path (fine for outbound HTTPS) — plus, for inbound access, DNAT so you can
`curl` the agent's HTTP server. These are wired by the demo scripts (`net-host.sh`,
`start-agent.sh`), not the core.

## Topologies

| Topology | Transfer | Use |
|----------|----------|-----|
| Single machine, two logical hosts (separate chroots + state dirs) | unix socket / loopback TCP | Fast iteration; dev only. |
| Two VMs / two machines / two droplets | real network TCP, real tap re-plumbing | Final measurements; the **release evidence** must be two separate droplets. |
