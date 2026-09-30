#!/usr/bin/env bash
# 环境自检：重启后、或每次跑测试前执行。只读检查，不修改任何东西。
set -uo pipefail
source "$(dirname "$0")/common.sh"

fail=0
check() {  # check <描述> <命令...>
    local desc=$1; shift
    if "$@" >/dev/null 2>&1; then ok "$desc"; else warn "$desc —— 未满足"; fail=1; fi
}

log "内核启动参数"
for a in hugepages=${HUGEPAGES_2M} isolcpus=managed_irq,domain,${DPDK_LCORE} nohz_full=${DPDK_LCORE} \
         rcu_nocbs=${DPDK_LCORE} irqaffinity=${HOUSEKEEPING_CPUS} tsc=reliable; do
    check "cmdline 含 $a" grep -qw -- "$a" /proc/cmdline
done
check "核 ${DPDK_LCORE} 已隔离（/sys/devices/system/cpu/isolated）" \
    grep -qx "${DPDK_LCORE}" /sys/devices/system/cpu/isolated
check "核 ${DPDK_LCORE} 为 nohz_full" grep -qx "${DPDK_LCORE}" /sys/devices/system/cpu/nohz_full
check "时钟源为 tsc" grep -qx tsc /sys/devices/system/clocksource/clocksource0/current_clocksource

log "大页"
total=$(awk '/HugePages_Total/{print $2}' /proc/meminfo)
check "HugePages_Total=$total ≥ ${HUGEPAGES_2M}" test "$total" -ge "$HUGEPAGES_2M"
check "/dev/hugepages 已挂载" mountpoint -q /dev/hugepages

log "中断"
bad=$(for f in /proc/irq/[0-9]*/effective_affinity_list; do
        l=$(cat "$f" 2>/dev/null) || continue
        [[ ",$l," == *",${DPDK_LCORE},"* || "$l" == "${DPDK_LCORE}" ]] && echo "$f"
      done | wc -l)
check "没有中断落在核 ${DPDK_LCORE}（发现 $bad 个）" test "$bad" -eq 0
check "irqbalance 已排除核 ${DPDK_LCORE}" grep -q "^IRQBALANCE_BANNED_CPULIST=${DPDK_LCORE}$" /etc/sysconfig/irqbalance

log "软件"
check "DPDK ${DPDK_VERSION}" test "$(pkg-config --modversion libdpdk 2>/dev/null)" = "$DPDK_VERSION"
check "igb_uio 模块已安装" test -f "/lib/modules/$(uname -r)/extra/dpdk/igb_uio.ko"
check "rustc ${RUST_TOOLCHAIN}" bash -c "source \$HOME/.cargo/env && rustc --version | grep -q ${RUST_TOOLCHAIN}"

log "网卡"
drv=$(basename "$(readlink -f "/sys/bus/pci/devices/${DPDK_PCI}/driver" 2>/dev/null)" 2>/dev/null)
if [[ "$drv" == "igb_uio" ]]; then ok "${DPDK_PCI} 已绑定 igb_uio"; else warn "${DPDK_PCI} 当前驱动：${drv:-无}（运行 scripts/bind.sh）"; fi
check "SSH 网卡 ${KERNEL_IFACE} 在线" ip link show "${KERNEL_IFACE}" up

echo
if (( fail )); then warn "有未满足项（见上）"; exit 1; else ok "环境检查全部通过"; fi
