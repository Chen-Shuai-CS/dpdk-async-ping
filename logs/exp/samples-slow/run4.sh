#!/usr/bin/env bash
# 第四轮：实验版（样本写入时预取下一条缓存行，见 prefetch.diff）能不能消除带样本时多出来的慢发送。
set -uo pipefail
cd "$(dirname "$0")/../../.."
out=logs/exp/samples-slow
fix_bin=$HOME/.cache/bq-build/exp/samples-prefetch/target/release
one() {  # one <标签> <cur|fix> <s|n>
    local o="$out/$1" extra=()
    [[ $3 == s ]] && extra=(--samples "logs/tmp/exp-$1.samples")
    if [[ $2 == fix ]]; then
        BQ_BIN_DIR="$fix_bin" RUN_LOG=/dev/null scripts/run.sh A --progress-sec 0 --json "$o.json" --delay-us 500 --duration-sec 60 "${extra[@]}" > "$o.log" 2>&1
    else
        RUN_LOG=/dev/null scripts/run.sh A --progress-sec 0 --json "$o.json" --delay-us 500 --duration-sec 60 "${extra[@]}" > "$o.log" 2>&1
    fi
    rm -f "logs/tmp/exp-$1.samples"
}
for i in 11 12 13 14; do
    one "v2-samples-$i" cur s
    one "fix-samples-$i" fix s
    one "v2-plain-$i" cur n
done
echo done
