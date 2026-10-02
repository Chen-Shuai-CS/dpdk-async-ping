#!/usr/bin/env bash
# 实验：v1（仓库之外构建的二进制）的 A 在 600 秒带 --samples 的运行里慢发送占比约 4%，不带 --samples 的短运行里约 0.5%。
# 是 --samples 触发的，还是时长触发的？各 60 秒，四种组合轮流 3 遍。
set -uo pipefail
cd "$(dirname "$0")/../../.."
out=logs/exp/samples-slow
old_bin=$(scripts/build-version.sh v1)
one() {  # one <标签> <v1|v2> <是否带样本> <秒数>
    local o="$out/$1" extra=()
    [[ $3 == s ]] && extra=(--samples "logs/tmp/exp-$1.samples")
    if [[ $2 == v1 ]]; then
        BQ_BIN_DIR="$old_bin" RUN_LOG=/dev/null scripts/run.sh A --progress-sec 0 --json "$o.json" --delay-us 500 --duration-sec "$4" "${extra[@]}" > "$o.log" 2>&1
    else
        RUN_LOG=/dev/null scripts/run.sh A --progress-sec 0 --json "$o.json" --delay-us 500 --duration-sec "$4" "${extra[@]}" > "$o.log" 2>&1
    fi
    rm -f "logs/tmp/exp-$1.samples"
}
for i in 1 2 3; do
    one "v1-samples-$i" v1 s 60
    one "v1-plain-$i"   v1 n 60
    one "v2-samples-$i" v2 s 60
    one "v2-plain-$i"   v2 n 60
done
echo done
