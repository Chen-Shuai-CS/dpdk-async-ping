#!/usr/bin/env bash
# 漂移监测：A、B 每 20 秒交替一次，连续跑指定的分钟数，每次运行记一行。
#
#   scripts/drift.sh <输出目录> [分钟数=60] [每次秒数=20]
#
# 用途：A 的尾部分位数存在一个随时间变化的"状态"（见 docs/REPORT.md §3.4）。10 分钟一次的长测看不清它什么时候来、持续多久，
# 用很多次短测把时间轴铺密。输出 <目录>/drift.tsv（一次运行一行）和每次运行的 JSON。
set -euo pipefail
source "$(dirname "$0")/common.sh"
cd "$REPO_ROOT"
out=${1:?用法：scripts/drift.sh <输出目录> [分钟数] [每次秒数]}; mins=${2:-60}; secs=${3:-20}
mkdir -p "$out/runs"
tsv="$out/drift.tsv"
[[ -f $tsv ]] || printf 'utc\tclient\tsent\tlost\tleak\tinproc_mean\tinproc_p50\tinproc_p99\tseg1_mean\tseg1_slow_pct\tseg2_mean\tseg2_p50\tseg2_p90\tseg2_p99\tseg3_mean\ttotal_mean\trtt_p50_us\n' > "$tsv"
end=$(( $(date +%s) + mins * 60 ))
i=0
while (( $(date +%s) < end )); do
    i=$((i + 1))
    for c in A B; do
        j="$out/runs/$c-$(printf '%03d' $i).json"
        RUN_LOG=/dev/null scripts/run.sh "$c" --delay-us 500 --duration-sec "$secs" --progress-sec 0 --json "$j" > /dev/null 2>&1 || true
        python3 - "$j" "$c" >> "$tsv" <<'PY'
import json, sys, datetime
r = json.load(open(sys.argv[1]))
g = lambda q: next(m for m in r["metrics"] if m["name"].strip().startswith(q))
ip, s1, s2, s3, e = g("in-process"), g("seg①"), g("seg②"), g("seg③"), g("end-to-end")
t = datetime.datetime.fromtimestamp(r["env"]["started_unix"], datetime.timezone.utc).strftime("%Y-%m-%d %H:%M:%S")
print("\t".join(str(x) for x in [
    t, sys.argv[2], r["counters"]["sent"], r["counters"]["timeouts"], r["mbuf"]["avail_initial"] - r["mbuf"]["avail_final"],
    round(ip["mean"], 1), ip["p50_interp"], ip["p99_interp"], round(s1["mean"], 1), round(r["seg1_slow_percent"], 3),
    round(s2["mean"], 1), s2["p50_interp"], s2["p90_interp"], s2["p99_interp"], round(s3["mean"], 1),
    round(s1["mean"] + s2["mean"] + s3["mean"], 1), round(e["p50"] / 1000)]))
PY
    done
done
log "漂移监测完成：$i 对 → $tsv"
