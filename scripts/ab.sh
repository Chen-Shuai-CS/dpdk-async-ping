#!/usr/bin/env bash
# A / B 交替对比：按 ABBA 顺序跑 N 对，抵消"对端状态随时间漂移"带来的系统性偏差，最后汇总。
#
#   scripts/ab.sh [对数 N=3] [每轮秒数=30] [其余参数...]
set -euo pipefail
source "$(dirname "$0")/common.sh"
pairs=${1:-3}; secs=${2:-30}; shift 2 || true
out="${AB_OUT:-$REPO_ROOT/logs/ab-$(date +%Y%m%d-%H%M%S)}"     # AB_OUT：由调用者指定输出目录（scripts/session.sh 用）
mkdir -p "$out"
for i in $(seq 1 "$pairs"); do
    if (( i % 2 )); then order="A B"; else order="B A"; fi
    for c in $order; do
        log "第 $i 对：$c"
        "$REPO_ROOT/scripts/run.sh" "$c" --delay-us 500 --duration-sec "$secs" --progress-sec 0 "$@" \
            --json "$out/$c-$i.json" > "$out/$c-$i.log" 2>&1 || warn "$c-$i 退出码非 0（见 $out/$c-$i.log）"
    done
done
python3 "$REPO_ROOT/scripts/summarize.py" "$out"/*.json | tee "$out/summary.txt"
