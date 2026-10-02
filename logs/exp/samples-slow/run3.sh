#!/usr/bin/env bash
# 第三轮：对照组 B 带 / 不带 --samples（B 的代码两个版本相同），看样本导出对 A − B 的影响。
set -uo pipefail
cd "$(dirname "$0")/../../.."
out=logs/exp/samples-slow
for i in 7 8 9 10; do
    RUN_LOG=/dev/null scripts/run.sh B --progress-sec 0 --json "$out/B-samples-$i.json" --delay-us 500 --duration-sec 60 --samples "logs/tmp/exp-B-$i.samples" > "$out/B-samples-$i.log" 2>&1
    rm -f "logs/tmp/exp-B-$i.samples"
    RUN_LOG=/dev/null scripts/run.sh B --progress-sec 0 --json "$out/B-plain-$i.json" --delay-us 500 --duration-sec 60 > "$out/B-plain-$i.log" 2>&1
done
echo done
