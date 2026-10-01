#!/usr/bin/env bash
# 一键环境搭建（幂等，可重复运行）。以普通用户运行，需要免密 sudo。
#
#   scripts/setup.sh                 跑全部阶段
#   scripts/setup.sh dpdk igb_uio    只跑指定阶段
#
# 阶段：packages rust nic dpdk igb_uio hugepages cmdline irq
# 其中 cmdline（核隔离 + 大页启动参数）需要重启才生效；脚本不会自动重启。
set -euo pipefail
source "$(dirname "$0")/common.sh"

mkdir -p "$REPO_ROOT/logs" "$BUILD_ROOT"
exec > >(tee -a "$REPO_ROOT/logs/setup-$(date +%Y%m%d-%H%M%S).log") 2>&1

[[ $EUID -ne 0 ]] || die "请以普通用户运行（Rust 工具链装在用户目录），脚本内部按需 sudo"
sudo -n true 2>/dev/null || die "需要免密 sudo"

NEED_REBOOT=0

# ---------------------------------------------------------------------------
phase_packages() {
    log "packages：编译工具链、内核头文件、调试工具"
    sudo dnf install -y -q gcc gcc-c++ make git meson ninja-build python3-pip \
        numactl-devel clang clang-devel "kernel6.18-devel-$(uname -r)" \
        elfutils-libelf-devel perf numactl ethtool tcpdump jq strace sysstat
    # AL2023 的源里没有 python3-pyelftools，DPDK 构建需要它
    python3 -c 'import elftools' 2>/dev/null || sudo python3 -m pip install -q pyelftools
    # 离线分析与出图（scripts/ci.py、scripts/plots.py）用；运行 A / B 本身不需要
    python3 -c 'import numpy, matplotlib' 2>/dev/null || python3 -m pip install --user -q numpy matplotlib
    ok "gcc $(gcc -dumpversion)，clang $(clang --version | head -1 | awk '{print $3}')，meson $(meson --version)"
}

# ---------------------------------------------------------------------------
phase_rust() {
    log "rust：rustup + 固定版本工具链 $RUST_TOOLCHAIN"
    if ! command -v rustup >/dev/null && [[ ! -x "$HOME/.cargo/bin/rustup" ]]; then
        curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
    fi
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
    rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal -c clippy,rustfmt
    rustup default "$RUST_TOOLCHAIN"
    ok "$(rustc --version)"
}

# ---------------------------------------------------------------------------
phase_nic() {
    log "nic：探测 DPDK 网卡参数"
    if [[ -f "$NIC_ENV" ]] && ! ip link show "${DPDK_IFACE:-none}" >/dev/null 2>&1; then
        ok "已有 $NIC_ENV 且网卡不在内核中（已绑定 DPDK），保留现有配置"
    else
        "$REPO_ROOT/scripts/detect-nic.sh"
    fi
}

# ---------------------------------------------------------------------------
phase_dpdk() {
    log "dpdk：下载、编译、安装 DPDK $DPDK_VERSION 到 $DPDK_PREFIX"
    if pkg-config --exists libdpdk && [[ "$(pkg-config --modversion libdpdk)" == "$DPDK_VERSION" ]]; then
        ok "DPDK $DPDK_VERSION 已安装"; return
    fi
    local tarball="$BUILD_ROOT/dpdk-$DPDK_VERSION.tar.xz"
    local src="$BUILD_ROOT/dpdk-stable-$DPDK_VERSION"
    [[ -f "$tarball" ]] || curl -sSfL -o "$tarball" "https://fast.dpdk.org/rel/dpdk-$DPDK_VERSION.tar.xz"
    [[ -d "$src" ]] || tar -C "$BUILD_ROOT" -xf "$tarball"
    cd "$src"
    # 只编译需要的驱动：ENA（真网卡）+ null/ring（无网卡时做离线测试）
    if [[ ! -d build ]]; then
        meson setup build \
            --prefix="$DPDK_PREFIX" --libdir=lib64 --buildtype=release \
            -Dcpu_instruction_set=native \
            -Denable_drivers=bus/pci,bus/vdev,mempool/ring,net/ena,net/null,net/ring \
            -Denable_apps=test-pmd,proc-info \
            -Dtests=false -Denable_docs=false
    fi
    ninja -C build
    sudo meson install -C build --quiet
    echo "$DPDK_LIBDIR" | sudo tee /etc/ld.so.conf.d/dpdk.conf >/dev/null
    sudo ldconfig
    cd "$REPO_ROOT"
    ok "DPDK $(pkg-config --modversion libdpdk) 安装完成"
}

# ---------------------------------------------------------------------------
phase_igb_uio() {
    log "igb_uio：编译 dpdk-kmods 的 igb_uio（支持 wc_activate=1 写合并）"
    local kver; kver=$(uname -r)
    local dst="/lib/modules/$kver/extra/dpdk/igb_uio.ko"
    if [[ ! -f "$dst" ]]; then
        local km="$BUILD_ROOT/dpdk-kmods"
        [[ -d "$km" ]] || git clone -q "$KMODS_REPO" "$km"
        make -s -C "/usr/src/kernels/$kver" M="$km/linux/igb_uio" modules
        sudo install -D -m 0644 "$km/linux/igb_uio/igb_uio.ko" "$dst"
        sudo depmod -a
    fi
    echo "options igb_uio wc_activate=1" | sudo tee /etc/modprobe.d/igb_uio.conf >/dev/null
    ok "igb_uio 已安装：$dst（commit $(git -C "$BUILD_ROOT/dpdk-kmods" rev-parse --short HEAD 2>/dev/null || echo '?')）"
}

# ---------------------------------------------------------------------------
phase_hugepages() {
    log "hugepages：运行时预留 ${HUGEPAGES_2M} × 2 MiB（重启后由启动参数接管）"
    local f=/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages
    if (( $(cat $f) < HUGEPAGES_2M )); then
        echo "$HUGEPAGES_2M" | sudo tee "$f" >/dev/null
    fi
    mountpoint -q /dev/hugepages || sudo mount -t hugetlbfs nodev /dev/hugepages
    ok "HugePages_Total=$(awk '/HugePages_Total/{print $2}' /proc/meminfo)，HugePages_Free=$(awk '/HugePages_Free/{print $2}' /proc/meminfo)"
}

# ---------------------------------------------------------------------------
# 启动参数：
#   hugepages          启动时预留大页，避免运行一段时间后内存碎片导致分配不到
#   isolcpus/nohz_full/rcu_nocbs  让核 3 几乎只跑我们的 lcore
#   irqaffinity        新中断默认只落在 0-2
#   rcu_nocb_poll      RCU 回调线程自己轮询，不再给被隔离的核发 IPI
#   nosoftlockup nmi_watchdog=0   关掉周期性的看门狗检查（会打断 busy-poll 核）
#   tsc=reliable       关掉 clocksource watchdog 对 TSC 的周期性校验
CMDLINE_ARGS="default_hugepagesz=2M hugepagesz=2M hugepages=${HUGEPAGES_2M} \
isolcpus=managed_irq,domain,${DPDK_LCORE} nohz_full=${DPDK_LCORE} rcu_nocbs=${DPDK_LCORE} \
irqaffinity=${HOUSEKEEPING_CPUS} rcu_nocb_poll nosoftlockup nmi_watchdog=0 tsc=reliable"

phase_cmdline() {
    log "cmdline：写入内核启动参数（需要重启生效）"
    local missing=0 a
    for a in $CMDLINE_ARGS; do
        grep -qw -- "$a" /proc/cmdline || missing=1
    done
    if (( missing == 0 )); then ok "当前内核已带全部参数"; return; fi
    sudo grubby --update-kernel=ALL --args="$CMDLINE_ARGS"
    ok "已写入：$(sudo grubby --info=DEFAULT | sed -n 's/^args=//p')"
    NEED_REBOOT=1
}

# ---------------------------------------------------------------------------
phase_irq() {
    log "irq：irqbalance 避开核 ${DPDK_LCORE}，现有中断立即迁走"
    local cfg=/etc/sysconfig/irqbalance
    if ! grep -q "^IRQBALANCE_BANNED_CPULIST=${DPDK_LCORE}\$" "$cfg" 2>/dev/null; then
        sudo sed -i '/^IRQBALANCE_BANNED_CPULIST=/d' "$cfg" 2>/dev/null || true
        echo "IRQBALANCE_BANNED_CPULIST=${DPDK_LCORE}" | sudo tee -a "$cfg" >/dev/null
        sudo systemctl restart irqbalance
    fi
    local irq moved=0
    for irq in /proc/irq/[0-9]*; do
        if echo "$HOUSEKEEPING_CPUS" | sudo tee "$irq/smp_affinity_list" >/dev/null 2>&1; then
            moved=$((moved + 1))
        fi
    done
    ok "irqbalance 已排除核 ${DPDK_LCORE}；$moved 个中断的亲和性设为 ${HOUSEKEEPING_CPUS}"
}

# ---------------------------------------------------------------------------
ALL_PHASES=(packages rust nic dpdk igb_uio hugepages cmdline irq)
if (( $# )); then PHASES=("$@"); else PHASES=("${ALL_PHASES[@]}"); fi
for p in "${PHASES[@]}"; do
    declare -F "phase_$p" >/dev/null || die "未知阶段：$p（可选：${ALL_PHASES[*]}）"
    "phase_$p"
done

log "完成：${PHASES[*]}"
if (( NEED_REBOOT )); then
    warn "内核启动参数已更新，需要重启后生效（请手动重启：sudo reboot）"
fi
