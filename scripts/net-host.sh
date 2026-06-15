#!/usr/bin/env bash
# Host network for sleepwalk VMs.
#
# Creates a Linux bridge that every per-VM tap attaches to, optionally gives it
# the guest gateway address, and NATs the guest subnet out the host's uplink so
# guests reach the internet (egress, O6). A VXLAN tunnel to a peer host puts both
# hosts' taps on one L2 segment, so a VM's MAC/IP — and thus a client's
# connection — follow it across a migration. Per-VM taps are made by the daemon;
# this sets up the shared fabric once per host.
#
# Usage (run as root):
#   net-host.sh up [GATEWAY_CIDR|none]   bridge up; assign gateway (default
#                                        10.200.0.1/24) or 'none' (peer host)
#   net-host.sh vxlan <LOCAL_IP> <REMOTE_IP>   tunnel this host to a peer
#   net-host.sh down
#   net-host.sh status
set -euo pipefail

BR=br-sw
VX=vxlan-sw
VNI=42
SUBNET=10.200.0.0/24

case "${1:-up}" in
up)
    gw="${2:-10.200.0.1/24}"
    ip link show "$BR" >/dev/null 2>&1 || ip link add "$BR" type bridge
    ip link set "$BR" up
    if [ "$gw" != "none" ]; then
        ip addr show dev "$BR" | grep -q "${gw%/*}/" || ip addr add "$gw" dev "$BR"
    fi
    sysctl -wq net.ipv4.ip_forward=1
    uplink=$(ip route show default | awk '{print $5; exit}')
    [ -n "$uplink" ] || {
        echo "net-host: no default route / uplink found" >&2
        exit 1
    }
    iptables -t nat -C POSTROUTING -s "$SUBNET" -o "$uplink" -j MASQUERADE 2>/dev/null ||
        iptables -t nat -A POSTROUTING -s "$SUBNET" -o "$uplink" -j MASQUERADE
    iptables -C FORWARD -i "$BR" -j ACCEPT 2>/dev/null || iptables -A FORWARD -i "$BR" -j ACCEPT
    iptables -C FORWARD -o "$BR" -j ACCEPT 2>/dev/null || iptables -A FORWARD -o "$BR" -j ACCEPT
    echo "net up: bridge $BR gateway $gw subnet $SUBNET uplink $uplink"
    ;;
vxlan)
    local_ip="${2:?usage: net-host.sh vxlan <LOCAL_IP> <REMOTE_IP>}"
    remote_ip="${3:?usage: net-host.sh vxlan <LOCAL_IP> <REMOTE_IP>}"
    uplink=$(ip route show default | awk '{print $5; exit}')
    ip link del "$VX" 2>/dev/null || true
    ip link add "$VX" type vxlan id "$VNI" dev "$uplink" \
        local "$local_ip" remote "$remote_ip" dstport 4789
    ip link set "$VX" master "$BR"
    ip link set "$VX" up
    echo "vxlan up: $VX vni $VNI $local_ip -> $remote_ip on $BR"
    ;;
down)
    ip link del "$VX" 2>/dev/null || true
    uplink=$(ip route show default | awk '{print $5; exit}')
    [ -n "$uplink" ] && iptables -t nat -D POSTROUTING -s "$SUBNET" -o "$uplink" -j MASQUERADE 2>/dev/null || true
    iptables -D FORWARD -i "$BR" -j ACCEPT 2>/dev/null || true
    iptables -D FORWARD -o "$BR" -j ACCEPT 2>/dev/null || true
    ip link del "$BR" 2>/dev/null || true
    echo "net down"
    ;;
status)
    ip -br addr show dev "$BR" 2>/dev/null || echo "no $BR"
    ip -br link show master "$BR" 2>/dev/null || true
    ;;
*)
    echo "usage: net-host.sh up [GATEWAY_CIDR|none] | vxlan <LOCAL_IP> <REMOTE_IP> | down | status" >&2
    exit 2
    ;;
esac
