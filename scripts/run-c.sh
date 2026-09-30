#!/usr/bin/env bash
# C：系统 ping（iputils），走内核网卡（device-number 0），打同一个对端。
#
#   scripts/run-c.sh [--flows 64] [--interval-ms 1] [--duration-sec 60] [--payload 64] [--mode user|kernel]
#
# 可比性设计（详见 README）：
# - 同一对端、同一子网路径，只是本端换成内核网卡；帧长相同（-s 64 → 106 B）；
# - 64 个 ping 进程 = 64 路并发（每个进程的 ICMP id 不同），每路 1 ms 间隔 → 聚合 6.4 万包/秒；
#   iputils 的 -i 以整数毫秒计（实测 -i 0.0005 会退化为"收到即发"），1 ms 是能取到的最接近 A 速率的值；
# - --mode user（默认）用 ping -U：用户态到用户态的延迟，对应 A 的 T3 − T0；
#   --mode kernel 用 ping 默认口径：接收时刻取内核的 SO_TIMESTAMP（不含唤醒进程与拷贝）；
# - ping 进程绑在核 0-2，不碰 runtime 的核 3。
set -euo pipefail
source "$(dirname "$0")/common.sh"

flows=64; interval_ms=1; dur=60; payload=64; mode=user
while (( $# )); do
    case "$1" in
        --flows) flows=$2; shift 2 ;;
        --interval-ms) interval_ms=$2; shift 2 ;;
        --duration-sec) dur=$2; shift 2 ;;
        --payload) payload=$2; shift 2 ;;
        --mode) mode=$2; shift 2 ;;
        *) die "未知参数：$1" ;;
    esac
done
[[ -n "${KERNEL_IFACE:-}" && -n "${PEER_IP:-}" ]] || die "缺少 $NIC_ENV"
uflag=""; [[ "$mode" == user ]] && uflag="-U"
count=$(( dur * 1000 / interval_ms ))
interval=$(awk -v m="$interval_ms" 'BEGIN{printf "%.3f", m/1000}')
out="$REPO_ROOT/logs/C-$mode-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$out"
log "C：$flows 路 × 每 ${interval_ms} ms，${dur} s，payload ${payload} B，口径 $mode，网卡 $KERNEL_IFACE → $out"

# 一次 sudo 启动全部进程；进程的逐个启动本身会自然错开相位
sudo taskset -c "$HOUSEKEEPING_CPUS" bash -c "
    for i in \$(seq 1 $flows); do
        ping -n $uflag -I $KERNEL_IFACE -i $interval -c $count -s $payload -W 1 $PEER_IP > '$out/ping-'\$i'.txt' 2>&1 &
    done
    wait
"
sudo chown -R "$(id -u):$(id -g)" "$out"
python3 "$REPO_ROOT/scripts/summarize_c.py" --mode "$mode" --flows "$flows" --interval-ms "$interval_ms" \
    --duration-sec "$dur" --json "$out/C.json" "$out"/ping-*.txt | tee "$out/summary.txt"
