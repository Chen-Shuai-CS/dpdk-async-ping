#!/usr/bin/env bash
# 一键重测：把报告里用到的所有数据重新跑一遍，再重新生成报告和图。全程约 2 小时。
#
#   scripts/campaign.sh              # 全部
#   scripts/campaign.sh main ab      # 只跑指定的阶段
#
# 阶段（按顺序）：
#   main   主考核：A、B 各 600 秒，同时导出原始样本 → 置信区间（logs/final/）
#   ab     A / B 交替 10 对 × 60 秒（logs/ab-<时间>/）
#   diag   诊断口径：读 T0 前 mfence（A、B 各 300 秒 + 置信区间）、sfence（各 60 秒）、
#          "T0 前多做 N 次普通写入"的剂量实验（logs/diag/）
#   aux    辅助对比：delay 800 µs 的 A / B、单路的 A（logs/final/）
#   fault  故障注入矩阵（logs/fault/<时间>/）
#   soak   长时间运行：A、B 各 30 分钟（logs/soak/）
#   report 重新生成 docs/REPORT.md 的表格与 docs/img/ 的图
#
# 要求：先提交代码再跑（报告会记录构建时的 git 提交，并核对源码树是干净的）。
# 原始样本文件很大（10 分钟约 440 MB），放在 logs/tmp/（不进仓库）；进仓库的是由它算出来的 ci.json。
set -euo pipefail
source "$(dirname "$0")/common.sh"
cd "$REPO_ROOT"
stages=("$@"); (( ${#stages[@]} )) || stages=(main ab diag aux fault soak report)
want() { local s; for s in "${stages[@]}"; do [[ $s == "$1" ]] && return 0; done; return 1; }
run() {  # run <A|B> <输出前缀> [参数...]：日志 → <前缀>.log，报告 → <前缀>.json
    local c=$1 out=$2; shift 2
    log "▶ $c → $out  $*"
    RUN_LOG=/dev/null scripts/run.sh "$c" --progress-sec 0 --json "$out.json" "$@" > "$out.log" 2>&1 || warn "$out 退出码非 0（见 $out.log）"
    grep -E "in-process|泄漏|^对账" "$out.log" | sed 's/^/    /'
}
mkdir -p logs/final logs/diag logs/soak logs/tmp

if want main; then
    run A logs/final/A-600 --delay-us 500 --duration-sec 600 --samples logs/tmp/A-600.samples
    run B logs/final/B-600 --delay-us 500 --duration-sec 600 --samples logs/tmp/B-600.samples
    python3 scripts/ci.py --a logs/tmp/A-600.samples --b logs/tmp/B-600.samples --out logs/final/ci.json
fi
if want ab; then
    scripts/ab.sh 10 60
fi
if want diag; then
    run B logs/diag/B-mfence --delay-us 500 --duration-sec 300 --diag-pre-t0 mfence --samples logs/tmp/B-mfence.samples
    run A logs/diag/A-mfence --delay-us 500 --duration-sec 300 --diag-pre-t0 mfence --samples logs/tmp/A-mfence.samples
    python3 scripts/ci.py --a logs/tmp/A-mfence.samples --b logs/tmp/B-mfence.samples --out logs/diag/ci-mfence.json
    run A logs/diag/A-sfence --delay-us 500 --duration-sec 60 --diag-pre-t0 sfence
    run B logs/diag/B-sfence --delay-us 500 --duration-sec 60 --diag-pre-t0 sfence
    # 剂量实验：读 T0 之前多做 N 次普通的内存写入（不带任何栅栏指令），看 B 的"背靠背发送"的等待在 N 多大时移到 T0 之前
    for n in 8 16 32 36 40 44 48 64 128; do
        run B "logs/diag/B-stores-$n" --delay-us 500 --duration-sec 20 --diag-pre-t0 stores --diag-stores "$n"
    done
    run A logs/diag/A-stores-64 --delay-us 500 --duration-sec 20 --diag-pre-t0 stores --diag-stores 64
fi
if want aux; then
    run A logs/final/A-d800 --delay-us 800 --duration-sec 60
    run B logs/final/B-d800 --delay-us 800 --duration-sec 60
    run A logs/final/A-1flow --delay-us 940 --duration-sec 30 --sessions 1
fi
if want fault; then
    python3 scripts/fault.py || warn "故障注入有未通过的场景"
fi
if want soak; then
    run A logs/soak/A-1800 --delay-us 500 --duration-sec 1800
    run B logs/soak/B-1800 --delay-us 500 --duration-sec 1800
fi
if want report; then
    python3 scripts/make_report.py
    python3 scripts/plots.py
fi
log "完成：${stages[*]}"
