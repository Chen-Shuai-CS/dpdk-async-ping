#!/usr/bin/env bash
# 第二轮：(1) 再重复一遍四种组合；(2) 加上 T0 之前的 mfence：如果带样本时多出来的慢发送也是"写入队列没排空"，mfence 之后应当消失。
set -uo pipefail
cd "$(dirname "$0")/../../.."
out=logs/exp/samples-slow
old_bin=$(scripts/build-version.sh v1)
one() {  # one <标签> <v1|v2> <s|n> [额外参数...]
    local tag=$1 ver=$2 smp=$3; shift 3
    local o="$out/$tag" extra=("$@")
    [[ $smp == s ]] && extra+=(--samples "logs/tmp/exp-$tag.samples")
    if [[ $ver == v1 ]]; then
        BQ_BIN_DIR="$old_bin" RUN_LOG=/dev/null scripts/run.sh A --progress-sec 0 --json "$o.json" --delay-us 500 --duration-sec 60 "${extra[@]}" > "$o.log" 2>&1
    else
        RUN_LOG=/dev/null scripts/run.sh A --progress-sec 0 --json "$o.json" --delay-us 500 --duration-sec 60 "${extra[@]}" > "$o.log" 2>&1
    fi
    rm -f "logs/tmp/exp-$tag.samples"
}
for i in 4 5 6; do
    one "v2-samples-mfence-$i" v2 s --diag-pre-t0 mfence
    one "v2-plain-mfence-$i"   v2 n --diag-pre-t0 mfence
    one "v2-samples-$i" v2 s
    one "v2-plain-$i"   v2 n
    one "v1-samples-$i" v1 s
    one "v1-plain-$i"   v1 n
done
echo done
