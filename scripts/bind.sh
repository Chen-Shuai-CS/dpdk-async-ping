#!/usr/bin/env bash
# 把 config/nic.env 里的网卡交给 DPDK：加载 igb_uio（写合并 wc_activate=1）→ 解绑内核驱动 → 绑定 igb_uio。
# 幂等：已经绑好就直接返回。重启后网卡会回到内核，run.sh 会先调用本脚本。
set -euo pipefail
source "$(dirname "$0")/common.sh"
[[ -n "${DPDK_PCI:-}" ]] || die "缺少 $NIC_ENV，请先运行 scripts/detect-nic.sh"

# 保险：绝不碰 SSH 所在的网卡
if [[ -e "/sys/class/net/${KERNEL_IFACE}/device" ]]; then
    ssh_pci=$(basename "$(readlink -f "/sys/class/net/${KERNEL_IFACE}/device")")
    [[ "$ssh_pci" != "$DPDK_PCI" ]] || die "DPDK_PCI=$DPDK_PCI 是 SSH 网卡 ${KERNEL_IFACE}，拒绝绑定"
fi

driver_of() { basename "$(readlink -f "/sys/bus/pci/devices/$1/driver" 2>/dev/null)" 2>/dev/null || true; }

sudo modprobe uio
sudo modprobe igb_uio          # /etc/modprobe.d/igb_uio.conf 提供 wc_activate=1
# igb_uio 不把 wc_activate 暴露到 sysfs，只能确认配置文件；是否真的生效，
# 要在 DPDK 进程运行时看 /sys/kernel/debug/x86/pat_memtype_list（见 check-env.sh）
grep -q 'wc_activate=1' /etc/modprobe.d/igb_uio.conf 2>/dev/null && wc=1 || wc=0
[[ "$wc" == "1" ]] || warn "/etc/modprobe.d/igb_uio.conf 未设置 wc_activate=1"

cur=$(driver_of "$DPDK_PCI")
if [[ "$cur" == "igb_uio" ]]; then
    ok "$DPDK_PCI 已绑定 igb_uio（wc_activate=$wc）"
    exit 0
fi

log "解绑 $DPDK_PCI（当前驱动：${cur:-无}），绑定 igb_uio"
if ip link show "$DPDK_IFACE" >/dev/null 2>&1; then
    sudo ip link set dev "$DPDK_IFACE" down   # 去掉路由，devbind 才不会把它当作 active 接口拒绝
fi
sudo dpdk-devbind.py -b igb_uio "$DPDK_PCI"
[[ "$(driver_of "$DPDK_PCI")" == "igb_uio" ]] || die "绑定失败"
ok "$DPDK_PCI → igb_uio（wc_activate=$wc）"
dpdk-devbind.py --status-dev net | sed -n '1,12p'
