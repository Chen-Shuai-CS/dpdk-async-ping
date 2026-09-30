#!/usr/bin/env bash
# 探测要交给 DPDK 的那张 ENI，把参数写进 config/nic.env。
# 必须在内核还管着这张网卡时运行（绑定到 DPDK 后内核里就看不到它的 MAC 了）。
#
# 用法：scripts/detect-nic.sh [device-number]    默认 1（device-number 0 留给内核/SSH）
set -euo pipefail
source "$(dirname "$0")/common.sh"

DEV_NO="${1:-1}"
PEER_IP="${PEER_IP:-10.202.8.15}"                 # SPEC §3.1
PEER_MAC="${PEER_MAC:-06:ff:fd:b6:f0:cd}"          # SPEC §3.1

find_mac_by_devno() {
    local want=$1 mac
    for mac in $(imds network/interfaces/macs/ | tr -d '/'); do
        [[ "$(imds "network/interfaces/macs/$mac/device-number")" == "$want" ]] && { echo "$mac"; return; }
    done
}

DPDK_MAC=$(find_mac_by_devno "$DEV_NO")
KERNEL_MAC=$(find_mac_by_devno 0)
[[ -n "$DPDK_MAC" ]] || die "IMDS 里找不到 device-number=$DEV_NO 的 ENI"
[[ "$DPDK_MAC" != "$KERNEL_MAC" ]] || die "不能把 device-number 0（SSH 那张）交给 DPDK"

DPDK_IP=$(imds "network/interfaces/macs/$DPDK_MAC/local-ipv4s" | head -1)
CIDR=$(imds "network/interfaces/macs/$DPDK_MAC/subnet-ipv4-cidr-block")

iface_by_mac() {
    local want=$1 f
    for f in /sys/class/net/*/address; do
        [[ "$(cat "$f")" == "$want" ]] && { basename "$(dirname "$f")"; return; }
    done
}
DPDK_IFACE=$(iface_by_mac "$DPDK_MAC")
KERNEL_IFACE=$(iface_by_mac "$KERNEL_MAC")
[[ -n "$DPDK_IFACE" ]] || die "内核里找不到 MAC=$DPDK_MAC 的网卡（可能已绑定到 DPDK；如需重新探测请先 scripts/unbind.sh）"
DPDK_PCI=$(basename "$(readlink -f "/sys/class/net/$DPDK_IFACE/device")")

cat > "$NIC_ENV" <<EOF
# 由 scripts/detect-nic.sh 于 $(date -u +%FT%TZ) 自动生成。换网卡：重跑本脚本，或手改下面几行。
DPDK_PCI=$DPDK_PCI          # 绑定到 DPDK 的 PCI 地址
DPDK_IFACE=$DPDK_IFACE           # 它在内核里的名字（绑定后消失）
DPDK_MAC=$DPDK_MAC    # 源 MAC（AWS 只放行本 ENI 的 MAC/IP）
DPDK_IP=$DPDK_IP        # 源 IP
DPDK_CIDR=$CIDR
KERNEL_IFACE=$KERNEL_IFACE         # device-number 0：SSH 与系统 ping(C) 用
PEER_IP=$PEER_IP
PEER_MAC=$PEER_MAC
EOF
log "已写入 $NIC_ENV"
cat "$NIC_ENV"
