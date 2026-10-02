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
#   aux    与系统 ping（C）的端到端对比：先测 C 并取它**实测**的速率，再给 A / B 选 delay 使实测速率对齐（logs/c/、logs/final/）
#   fault  故障注入矩阵（logs/fault/<时间>/）
#   soak   长时间运行：A、B 各 30 分钟（logs/soak/）
#   probe  probe 构建的诊断（段①、段②的子步骤），各 30 秒（logs/probe/）
#   report 重新生成 docs/REPORT.md 的表格与 docs/img/ 的图
#
# 要求：先提交代码再跑（报告会记录构建时的 git 提交，并核对源码树是干净的）。
# 原始样本文件很大（10 分钟约 440 MB），放在 logs/tmp/（不进仓库）；进仓库的是由它算出来的 ci.json。
set -euo pipefail
source "$(dirname "$0")/common.sh"
cd "$REPO_ROOT"
stages=("$@"); (( ${#stages[@]} )) || stages=(main ab diag aux fault soak probe report)
want() { local s; for s in "${stages[@]}"; do [[ $s == "$1" ]] && return 0; done; return 1; }
run() {  # run <A|B> <输出前缀> [参数...]：日志 → <前缀>.log，报告 → <前缀>.json
    local c=$1 out=$2; shift 2
    log "▶ $c → $out  $*"
    RUN_LOG=/dev/null scripts/run.sh "$c" --progress-sec 0 --json "$out.json" "$@" > "$out.log" 2>&1 || warn "$out 退出码非 0（见 $out.log）"
    grep -E "in-process|泄漏|^对账" "$out.log" | sed 's/^/    /'
}
mkdir -p logs/final logs/diag logs/soak logs/tmp

if want main; then
    scripts/write_meta.py logs/final/meta.json "正式数据"
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
    # A 对 C（系统 ping）的端到端对比。C 的速率不能按 -i 的设定推算（实测比名义低百分之几），所以：
    # 先测 C，拿到它实测的速率；再给 A / B 选一个 delay，使两边的实测速率尽量接近。
    # A 是"收到回复后等 delay 再发"，周期 = 往返时间 + delay，而往返时间随速率和环境变化，所以先试跑 10 秒量一次，再修正一次。
    rm -rf logs/c; mkdir -p logs/c
    for spec in "user 64 60" "kernel 64 60" "user 1 30" "kernel 1 30"; do
        # shellcheck disable=SC2086
        set -- $spec
        log "▶ C（$1，$2 路，$3 秒）→ logs/c/$1-$2flow"
        C_OUT="$REPO_ROOT/logs/c/$1-$2flow" scripts/run-c.sh --mode "$1" --flows "$2" --duration-sec "$3" | grep -E "^速率|^sent" | sed 's/^/    /'
        rm -f "logs/c/$1-$2flow"/ping-*.txt     # 逐包的原始输出很大（64 路 60 秒约 230 MB），汇总完就删；C.json 里有全部统计
    done
    matched_delay() {  # matched_delay <session 数> <C 的 C.json> <试跑用的 delay>：输出让 A 的实测速率对齐 C 的 delay（µs）
        RUN_LOG=/dev/null scripts/run.sh A --progress-sec 0 --json "logs/tmp/match-$1.json" --delay-us "$3" --duration-sec 10 --sessions "$1" > /dev/null 2>&1
        python3 - "$1" "$2" "$3" "logs/tmp/match-$1.json" <<'PY'
import json, sys
n, d0 = int(sys.argv[1]), float(sys.argv[3])
target = json.load(open(sys.argv[2]))["actual_pps"]
r = json.load(open(sys.argv[4]))
got = r["counters"]["sent"] / r["elapsed_sec"]
print(max(0, round(d0 + (n / target - n / got) * 1e6)))   # 周期 = session 数 ÷ 速率；差多少周期就把 delay 调多少
PY
    }
    d64=$(matched_delay 64 logs/c/user-64flow/C.json 900)
    d1=$(matched_delay 1 logs/c/user-1flow/C.json 940)
    log "对齐 C 的实测速率：64 路用 delay $d64 µs，单路用 delay $d1 µs"
    run A logs/final/A-vsC --delay-us "$d64" --duration-sec 60
    run B logs/final/B-vsC --delay-us "$d64" --duration-sec 60
    run A logs/final/A-1flow --delay-us "$d1" --duration-sec 30 --sessions 1
fi
if want fault; then
    python3 scripts/fault.py || warn "故障注入有未通过的场景"
fi
if want soak; then
    run A logs/soak/A-1800 --delay-us 500 --duration-sec 1800
    run B logs/soak/B-1800 --delay-us 500 --duration-sec 1800
fi
if want probe; then
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
    cargo build --release -q -p async-ping -p raw-ping --features probe --target-dir target-probe
    mkdir -p logs/probe
    for b in async-ping raw-ping; do
        log "▶ probe $b"
        sudo "target-probe/release/$b" --pci "$DPDK_PCI" --src-ip "$DPDK_IP" --dst-ip "$PEER_IP" --dst-mac "$PEER_MAC" --lcore "$DPDK_LCORE" \
            --delay-us 500 --duration-sec 30 --progress-sec 0 --json "logs/probe/$b-probe.json" > "logs/probe/$b-probe.log" 2>&1 || warn "probe $b 失败"
        sudo chown "$(id -u):$(id -g)" "logs/probe/$b-probe.json"
    done
fi
if want report; then
    python3 scripts/make_report.py
    python3 scripts/plots.py
fi
log "完成：${stages[*]}"
