#!/usr/bin/env bash
# 两个代码版本的 A 与 B 交替对比：同一时段、轮流运行，抵消"时段不同"带来的差别。
#
#   scripts/versions.sh [轮数=6] [每次秒数=60] [旧版本标签=v1]
#
# 每一轮按轮换的顺序各跑一次：旧版本的 A（记作 A1）、当前代码的 A（A2）、B。B 的代码在两个版本里相同（它不依赖 runtime）。
# 每次都导出原始样本，由 ci.py 按批内位置拆分段②；结果汇总成 <输出目录>/summary.json，样本用完即删。
set -euo pipefail
source "$(dirname "$0")/common.sh"
cd "$REPO_ROOT"
rounds=${1:-6}; secs=${2:-60}; old=${3:-v1}
old_bin=$(scripts/build-version.sh "$old")
out="${VERSIONS_OUT:-logs/versions/$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$out" logs/tmp
scripts/write_meta.py "$out/meta.json" "版本对比 $old ↔ 当前"
one() {  # one <A1|A2|B> <轮次>
    local tag=$1 i=$2 c=A env=()
    [[ $tag == B ]] && c=B
    [[ $tag == A1 ]] && env=(BQ_BIN_DIR="$old_bin")
    log "第 $i 轮：$tag"
    env RUN_LOG=/dev/null "${env[@]}" scripts/run.sh "$c" --delay-us 500 --duration-sec "$secs" --progress-sec 0 \
        --json "$out/$tag-$i.json" --samples "logs/tmp/ver-$tag-$i.samples" > "$out/$tag-$i.log" 2>&1 || warn "$tag-$i 退出码非 0"
}
orders=("A1 A2 B" "A2 B A1" "B A1 A2")
for i in $(seq 1 "$rounds"); do
    for t in ${orders[$(( (i - 1) % 3 ))]}; do one "$t" "$i"; done
    for t in A1 A2; do
        python3 scripts/ci.py --a "logs/tmp/ver-$t-$i.samples" --b "logs/tmp/ver-B-$i.samples" --out "logs/tmp/ver-ci-$t-$i.json" --block-sec 5 > /dev/null
    done
    rm -f logs/tmp/ver-*-"$i".samples
done
python3 - "$out" "$rounds" <<'PY'
import json, sys
out, rounds = sys.argv[1], int(sys.argv[2])
rows = []
for i in range(1, rounds + 1):
    rd = {"round": i}
    for t in ("A1", "A2"):
        c = json.load(open(f"logs/tmp/ver-ci-{t}-{i}.json"))
        bp, m = c["burst_position"], c["metrics"]
        d1 = bp["pos1"]["diff"]["mean"]["value"]
        pos = ("pos1", "pos2", "pos3", "pos4+")
        rd[t] = {
            "seg2_mean_by_position": {k: {"A": bp[k]["A"]["mean"], "B": bp[k]["B"]["mean"], "diff": bp[k]["diff"]["mean"]["value"], "share": bp[k]["share"]["A"]} for k in pos},
            "queueing_extra_ns": round(sum(bp[k]["share"]["A"] * (bp[k]["diff"]["mean"]["value"] - d1) for k in pos[1:]), 2),
            "seg2_diff": {"mean": m["seg2"]["diff"]["mean"]["value"], "p50": m["seg2"]["diff"]["p50"]["interp"], "p99": m["seg2"]["diff"]["p99"]["interp"]},
            "inproc_diff": {"mean": m["inproc"]["diff"]["mean"]["value"], "p50": m["inproc"]["diff"]["p50"]["interp"], "p99": m["inproc"]["diff"]["p99"]["interp"]},
        }
    rows.append(rd)
json.dump({"rounds": rows}, open(f"{out}/summary.json", "w"), ensure_ascii=False, indent=1)
PY
rm -f logs/tmp/ver-ci-*.json
log "版本对比完成 → $out"
