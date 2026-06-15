#!/usr/bin/env bash
# Host network for sleepwalk VMs.
#
# Creates a Linux bridge that every per-VM tap attaches to, gives it the guest
# gateway address, and NATs the guest subnet out the host's uplink so guests
# reach the internet (egress, O6). Per-VM taps are created by the daemon; this
# sets up the shared fabric once per host. A VXLAN tunnel to peer hosts (so a
# VM's L2 — and thus its IP — spans hosts) is layered onto this same bridge in a
# later step; the bridge name and subnet are the contract both share.
#
# Usage: net-host.sh up | down | status   (run as root)
set -euo pipefail

BR=br-sw
SUBNET=10.200.0.0/24
GATEWAY=10.200.0.1/24

case "${1:-up}" in
up)
    ip link show "$BR" >/dev/null 2>&1 || ip link add "$BR" type bridge
    ip addr show dev "$BR" | grep -q "10.200.0.1/24" || ip addr add "$GATEWAY" dev "$BR"
    ip link set "$BR" up
    sysctl -wq net.ipv4.ip_forward=1
    uplink=$(ip route show default | awk '{print $5; exit}')
    [ -n "$uplink" ] || {
        echo "net-host: no default route / uplink found" >&2
        exit 1
    }
    # Idempotent NAT + forwarding for the guest subnet.
    iptables -t nat -C POSTROUTING -s "$SUBNET" -o "$uplink" -j MASQUERADE 2>/dev/null ||
        iptables -t nat -A POSTROUTING -s "$SUBNET" -o "$uplink" -j MASQUERADE
    iptables -C FORWARD -i "$BR" -j ACCEPT 2>/dev/null || iptables -A FORWARD -i "$BR" -j ACCEPT
    iptables -C FORWARD -o "$BR" -j ACCEPT 2>/dev/null || iptables -A FORWARD -o "$BR" -j ACCEPT
    echo "net up: bridge $BR gateway ${GATEWAY%/*} subnet $SUBNET uplink $uplink"
    ;;
down)
    uplink=$(ip route show default | awk '{print $5; exit}')
    [ -n "$uplink" ] && iptables -t nat -D POSTROUTING -s "$SUBNET" -o "$uplink" -j MASQUERADE 2>/dev/null || true
    iptables -D FORWARD -i "$BR" -j ACCEPT 2>/dev/null || true
    iptables -D FORWARD -o "$BR" -j ACCEPT 2>/dev/null || true
    ip link del "$BR" 2>/dev/null || true
    echo "net down"
    ;;
status)
    ip -br addr show dev "$BR" 2>/dev/null || echo "no $BR"
    ip -br link show type bridge_slave 2>/dev/null | grep "$BR" || true
    ;;
*)
    echo "usage: net-host.sh up | down | status" >&2
    exit 2
    ;;
esac
