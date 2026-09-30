#!/usr/bin/env bash
# 把网卡还给内核的 ena 驱动（调试用：比如想用内核工具看这张网卡，或重新探测参数）。
set -euo pipefail
source "$(dirname "$0")/common.sh"
[[ -n "${DPDK_PCI:-}" ]] || die "缺少 $NIC_ENV"

cur=$(basename "$(readlink -f "/sys/bus/pci/devices/$DPDK_PCI/driver" 2>/dev/null)" 2>/dev/null || true)
if [[ "$cur" == "ena" ]]; then ok "$DPDK_PCI 已在内核 ena 驱动下"; exit 0; fi
sudo dpdk-devbind.py -b ena "$DPDK_PCI"
ok "$DPDK_PCI → ena"
sleep 2
ip -br addr show "$DPDK_IFACE" || true
