#!/usr/bin/env bash
# 一次独立的"测量会话"：在一次新的开机（或另一天）里，把最核心的两组数据重测一遍，结果单独存放，不覆盖正式数据。
#
#   scripts/session.sh <名字> [连续监测的分钟数=60]        例如：scripts/session.sh day2
#   SESSION_VERSION=v1 scripts/session.sh day2-v1          用某个历史版本（git 标签）的二进制跑同样的会话
#
# 做的事（约 45 分钟 + 连续监测）：
#   1. 绑定网卡、环境自检（任何一项不满足就停下）
#   2. 记录这次开机的身份（boot_id、开机时间）和代码版本
#   3. 主考核口径：A、B 各 600 秒，导出原始样本 → 置信区间
#   4. A / B 交替 10 对 × 60 秒
#   5. 连续监测（scripts/drift.sh）：A、B 每 20 秒交替一次；A 的尾部一旦变重，立刻补一对 mfence 口径
# 结果在 logs/sessions/<名字>/；用 scripts/make_report.py 汇总成"各会话对比"表。
#
# 目的：回答"换一次开机 / 换一天，结论还成不成立"。两三个会话估不出方差，只能看结论是否一致。
set -euo pipefail
source "$(dirname "$0")/common.sh"
cd "$REPO_ROOT"
name=${1:?用法：scripts/session.sh <名字> [连续监测的分钟数]}
drift_min=${2:-60}
out="logs/sessions/$name"
[[ -e "$out/A-600.json" ]] && die "$out 已有结果，换一个名字"
mkdir -p "$out" logs/tmp

scripts/bind.sh > "$out/bind.txt" 2>&1 || die "绑定网卡失败（见 $out/bind.txt）"
scripts/check-env.sh > "$out/check-env.txt" 2>&1 || die "环境自检未通过（见 $out/check-env.txt）"
log "环境自检通过"

# 指定了历史版本：把它编译到仓库之外，之后所有的 run.sh 都用那份二进制（B 的代码在各版本里相同）
ver=${SESSION_VERSION:-}
if [[ -n "$ver" ]]; then
    BQ_BIN_DIR=$(scripts/build-version.sh "$ver"); export BQ_BIN_DIR
    scripts/write_meta.py "$out/meta.json" "$name" --version "$ver" --commit "$(git rev-parse --short=12 "$ver^{commit}")"
else
    # 整个会话自始至终用同一份二进制文件：在这里编译一次，之后的 run.sh 不再调用 cargo。
    # 原因见 REPORT §3.5：v4-reboot2 会话里，cargo 在主考核之后把内容完全相同的二进制又写了一遍，
    # 此后 A 的段①整体慢了约 5 ns，直到这个文件再次被重写才恢复。
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
    cargo build --release -q --bin async-ping --bin raw-ping
    BQ_BIN_DIR="$REPO_ROOT/target/release"; export BQ_BIN_DIR
    scripts/write_meta.py "$out/meta.json" "$name"
fi
(cd "$BQ_BIN_DIR" && md5sum async-ping raw-ping && stat -c '%n 写入于 %y' async-ping raw-ping) > "$out/binaries.txt"

run() {
    local c=$1 o=$2; shift 2
    log "▶ $c → $o  $*"
    RUN_LOG=/dev/null scripts/run.sh "$c" --progress-sec 0 --json "$o.json" "$@" > "$o.log" 2>&1 || warn "$o 退出码非 0（见 $o.log）"
    grep -E "in-process|泄漏|^对账" "$o.log" | sed 's/^/    /'
}
run A "$out/A-600" --delay-us 500 --duration-sec 600 --samples "logs/tmp/$name-A.samples"
run B "$out/B-600" --delay-us 500 --duration-sec 600 --samples "logs/tmp/$name-B.samples"
python3 scripts/ci.py --a "logs/tmp/$name-A.samples" --b "logs/tmp/$name-B.samples" --out "$out/ci.json"
AB_OUT="$out/ab" scripts/ab.sh 10 60
(( drift_min > 0 )) && scripts/drift.sh "$out/drift" "$drift_min" 20
log "会话 $name 完成 → $out"
