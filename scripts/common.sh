# shellcheck shell=bash
# 公共配置与小工具，被 setup.sh / bind.sh / run.sh 通过 `source` 引入。

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ---- 版本与路径 ------------------------------------------------------------
DPDK_VERSION="${DPDK_VERSION:-25.11.3}"
DPDK_PREFIX="${DPDK_PREFIX:-/usr/local}"
DPDK_LIBDIR="${DPDK_PREFIX}/lib64"
BUILD_ROOT="${BUILD_ROOT:-$HOME/.cache/bq-build}"      # 源码与构建目录（不进 git）
RUST_TOOLCHAIN="${RUST_TOOLCHAIN:-1.98.1}"
KMODS_REPO="${KMODS_REPO:-https://dpdk.org/git/dpdk-kmods}"

# ---- CPU 规划（4 核，无超线程）----------------------------------------------
# 核 0-2：OS / SSH / 系统 ping(C) / 统计上报线程；核 3：runtime 独占
DPDK_LCORE="${DPDK_LCORE:-3}"
HOUSEKEEPING_CPUS="${HOUSEKEEPING_CPUS:-0-2}"

# ---- 大页 --------------------------------------------------------------------
HUGEPAGES_2M="${HUGEPAGES_2M:-1024}"                   # 1024 × 2 MiB = 2 GiB

# ---- 网卡配置：由 scripts/detect-nic.sh 生成 --------------------------------
NIC_ENV="${REPO_ROOT}/config/nic.env"
# shellcheck disable=SC1090
[[ -f "$NIC_ENV" ]] && source "$NIC_ENV"

export PKG_CONFIG_PATH="${DPDK_LIBDIR}/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"

# ---- 小工具 ------------------------------------------------------------------
log()  { printf '\033[1;34m[%s]\033[0m %s\n' "$(date +%H:%M:%S)" "$*"; }
ok()   { printf '\033[1;32m  ✔\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m  ! %s\033[0m\n' "$*"; }
die()  { printf '\033[1;31m  ✘ %s\033[0m\n' "$*" >&2; exit 1; }

imds() {  # IMDSv2 查询：imds <path>
    local token
    token=$(curl -s -X PUT "http://169.254.169.254/latest/api/token" \
        -H "X-aws-ec2-metadata-token-ttl-seconds: 60")
    curl -s -H "X-aws-ec2-metadata-token: $token" "http://169.254.169.254/latest/meta-data/$1"
}
