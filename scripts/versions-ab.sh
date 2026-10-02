#!/usr/bin/env bash
# 某个旧版本与当前代码的同场对比，A 和 B 都比：适用于"这次改动同时动了 A 和 B"的情况（scripts/versions.sh 假定 B 不变）。
#
#   scripts/versions-ab.sh <旧版本标签> [轮数=10] [每次秒数=60]
#
# 每一轮按轮换的顺序各跑一次：旧版本的 A、当前的 A、旧版本的 B、当前的 B（都不带样本导出）。
# 逐轮配对之后能分别回答：这次改动让 A 变了多少、让 B 变了多少、让 A − B 变了多少。
# 结果在 logs/versions-ab/<旧>-vs-<当前>/，由 scripts/make_report.py 汇总。
set -euo pipefail
source "$(dirname "$0")/common.sh"
cd "$REPO_ROOT"
old=${1:?用法：scripts/versions-ab.sh <旧版本标签> [轮数] [每次秒数]}; rounds=${2:-10}; secs=${3:-60}
cur=$(python3 -c "import sys; sys.path.insert(0, 'scripts'); import write_meta; print(write_meta.detect_version())")
old_bin=$(scripts/build-version.sh "$old")
out="logs/versions-ab/$old-vs-$cur"
[[ -e "$out/meta.json" ]] && die "$out 已有结果"
mkdir -p "$out"
scripts/write_meta.py "$out/meta.json" "版本对比 $old ↔ $cur"
one() {  # one <Aold|Anew|Bold|Bnew> <轮次>
    local tag=$1 i=$2 c=${1:0:1} env=()
    [[ $tag == *old ]] && env=(BQ_BIN_DIR="$old_bin")
    log "第 $i 轮：$tag"
    env RUN_LOG=/dev/null "${env[@]}" scripts/run.sh "$c" --delay-us 500 --duration-sec "$secs" --progress-sec 0 \
        --json "$out/$tag-$i.json" > "$out/$tag-$i.log" 2>&1 || warn "$tag-$i 退出码非 0"
}
orders=("Aold Anew Bold Bnew" "Anew Bold Bnew Aold" "Bold Bnew Aold Anew" "Bnew Aold Anew Bold")
for i in $(seq 1 "$rounds"); do
    for t in ${orders[$(( (i - 1) % 4 ))]}; do one "$t" "$i"; done
done
log "版本对比完成 → $out"
