#!/usr/bin/env bash
# 把 SPEC 的硬性要求变成一条条可以机器核对的检查。全部通过则退出码 0。
#
#   scripts/check-compliance.sh            # 检查代码 + 已有的日志
#   scripts/check-compliance.sh --quick    # 跳过 clippy / 测试（只查静态项和日志）
#
# 这不是测试的替代品：它回答的是"交付物是否符合题目的每一条硬性规定"，而不是"代码对不对"。
set -uo pipefail
source "$(dirname "$0")/common.sh"
cd "$REPO_ROOT"
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true
quick=0; [[ "${1:-}" == --quick ]] && quick=1
pass=0; fail=0
check() {  # check <描述> <命令...>：命令成功 = 通过
    local what=$1; shift
    if "$@" >/dev/null 2>&1; then ok "$what"; pass=$((pass + 1)); else printf '\033[1;31m  ✘ %s\033[0m\n' "$what"; fail=$((fail + 1)); fi
}
json() { python3 - "$@" <<'PY'
import json, sys
path, expr = sys.argv[1], sys.argv[2]
r = json.load(open(path))
c, m = r["counters"], r["mbuf"]
ip = next(x for x in r["metrics"] if x["name"].startswith("in-process"))
sys.exit(0 if eval(expr) else 1)
PY
}

log "一、语言与依赖（SPEC：Rust；自研 executor / Waker / timer / reactor；不得使用现成 runtime）"
forbidden='^name = "(tokio|async-std|smol|glommio|monoio|futures|futures-[a-z-]+|mio|async-executor|async-io|async-task|embassy-[a-z-]+|compio|nuclei|bastion|actix-rt|may|polling)"$'
check "Cargo.lock 里没有任何现成的 async runtime / 执行器 / reactor 库" bash -c "! grep -Eq '$forbidden' Cargo.lock"
check "rt（runtime）只依赖本仓库的 dpdk 与 timerq 两个 crate" bash -c "[[ \$(sed -n '/^\[dependencies\]/,/^\[/p' crates/rt/Cargo.toml | grep -c '^[a-z]') == 2 ]] && grep -q 'dpdk = { path' crates/rt/Cargo.toml && grep -q 'timerq = { path' crates/rt/Cargo.toml"
check "rt 自己实现了 Waker（RawWakerVTable）、就绪队列、timer、poll-mode 主循环" bash -c "grep -q RawWakerVTable crates/rt/src/executor.rs && grep -q 'struct ReadyQueue' crates/rt/src/executor.rs && grep -q 'rx_burst' crates/rt/src/runtime.rs && grep -q 'fire' crates/rt/src/timer.rs"
check "主循环里没有任何会睡眠 / 阻塞的调用（epoll、sleep、park、condvar）" bash -c "! grep -En 'epoll|thread::sleep|park\(|Condvar|nanosleep|usleep' crates/rt/src/*.rs crates/async-ping/src/*.rs crates/raw-ping/src/*.rs crates/pingkit/src/sender.rs crates/pingkit/src/house.rs"

log "二、三个客户端与命令行（SPEC §6）"
check "async-ping（A）与 raw-ping（B）都能构建" cargo build --release -q
for b in async-ping raw-ping; do
    check "$b 接受 --delay-us / --duration-sec / --sessions" bash -c "target/release/$b --help | grep -q -- --delay-us && target/release/$b --help | grep -q -- --duration-sec && target/release/$b --help | grep -q -- --sessions"
done
check "A 和 B 共用同一份参数定义、同一个发送函数（段①）" bash -c "grep -q 'pingkit::{.*Args' crates/async-ping/src/main.rs && grep -q 'pingkit::{.*Args' crates/raw-ping/src/main.rs && grep -q 'sender.send' crates/async-ping/src/main.rs && grep -q 'sender.send' crates/raw-ping/src/main.rs"
check "C（系统 ping）的运行脚本存在" test -x scripts/run-c.sh

log "三、unsafe 的边界"
total=$(grep -rn 'unsafe' crates --include=*.rs | grep -v '^crates/dpdk-sys' | grep -Ev '^\S+:[0-9]+:\s*//' | grep -Ec 'unsafe (\{|fn|impl)|unsafe\{')
echo "    unsafe 块 / 函数共 $total 处（不含 bindgen 生成的 dpdk-sys）："
for c in dpdk rt pingkit async-ping raw-ping pingproto timerq; do
    n=$(grep -rn 'unsafe' "crates/$c" --include=*.rs | grep -Ev '^\S+:[0-9]+:\s*//' | grep -Ec 'unsafe (\{|fn|impl)|unsafe\{')
    printf '      %-12s %s\n' "$c" "$n"
done
missing=$(python3 - <<'PY'
import glob, re
bad = []
for path in glob.glob("crates/**/*.rs", recursive=True):
    if "dpdk-sys" in path:
        continue
    lines = open(path).read().splitlines()
    for i, l in enumerate(lines):
        if re.search(r"unsafe\s*\{", l) and not l.strip().startswith("//"):
            ctx = "\n".join(lines[max(0, i - 6): i + 1])
            if "SAFETY" not in ctx:
                bad.append(f"{path}:{i + 1}")
print("\n".join(bad))
PY
)
check "每一个 unsafe 块上方都有 SAFETY 说明" test -z "$missing"
[[ -n "$missing" ]] && echo "$missing" | sed 's/^/      缺说明：/'
check "协议解析（pingproto）与 timer 堆（timerq）完全不含 unsafe" bash -c "! grep -rn 'unsafe' crates/pingproto/src crates/timerq/src | grep -Ev ':\s*//' | grep -q ."

if (( ! quick )); then
    log "四、静态检查与测试"
    check "cargo clippy --all-targets：0 警告" bash -c "! cargo clippy --release --all-targets 2>&1 | grep -Eq '^(warning|error)'"
    tests=$(cargo test --release 2>&1 | grep -c '^test .* ok$')
    check "cargo test：全部通过（$tests 个）" bash -c "! cargo test --release 2>&1 | grep -Eq 'FAILED|panicked at|error\['"
fi

log "五、硬门槛：主考核日志（SPEC：≥ 10 分钟不崩、零 mbuf 泄漏、零丢包或有解释）"
for t in A B; do
    f=logs/final/$t-600.json
    if [[ ! -f $f ]]; then check "$t：存在 $f" false; continue; fi
    check "$t：运行时长 ≥ 600 秒，正常退出" json "$f" 'r["elapsed_sec"] >= 600 and "duration" in r["exit_reason"]'
    check "$t：零 mbuf 泄漏" json "$f" 'm["avail_initial"] == m["avail_final"]'
    check "$t：零丢包（0 超时）" json "$f" 'c["timeouts"] == 0'
    check "$t：请求对账与收包对账都为 0" json "$f" 'c["sent"] - c["received"] - c["timeouts"] - c["in_flight_at_end"] == 0 and c["rx_pkts"] - (c["received"] + c["late"] + c["unexpected"] + c["foreign"] + c["other_rx"] + c["arp_replies"]) == 0'
    check "$t：样本数 = received − tsc_mismatch，且没有时间戳核对不符" json "$f" 'ip["count"] == c["received"] - c["tsc_mismatch"] and c["tsc_mismatch"] == 0'
    check "$t：AWS 限额计数、网卡丢弃计数全为 0" json "$f" 'all(v == 0 for _, v in r["port"]["allowance_exceeded"]) and r["port"]["imissed"] == 0 and r["port"]["ierrors"] == 0 and r["port"]["rx_nombuf"] == 0'
    check "$t：是排名口径的运行（没有开任何诊断开关），且由干净的源码树构建" json "$f" 'r["diag"] == [] and not r["env"]["git_dirty"]'
done
check "A 与 B 的主考核来自同一个代码版本" python3 -c "
import json, sys
a, b = (json.load(open(f'logs/final/{t}-600.json'))['env']['git_commit'] for t in 'AB')
sys.exit(0 if a == b and a not in ('', 'unknown') else 1)"

log "六、交付物（SPEC §8）"
check "README.md、docs/REPORT.md（延迟报告）、docs/WORKLOG.md 都在" bash -c "test -s README.md && test -s docs/REPORT.md && test -s docs/WORKLOG.md"
check "一键搭环境 / 一键运行脚本可执行" bash -c "test -x scripts/setup.sh && test -x scripts/run.sh && test -x scripts/check-env.sh"
check "报告由脚本从日志生成（可复现）" bash -c "test -x scripts/make_report.py && grep -q 'BEGIN:main' docs/REPORT.md"
check "故障注入结果存在且全部通过" bash -c "f=\$(ls -d logs/fault/*/ | tail -1)summary.json && python3 -c \"import json,sys; r=json.load(open('\$f')); sys.exit(0 if r and all(x['ok'] for x in r) else 1)\""

echo
if (( fail == 0 )); then log "全部 $pass 项通过"; else printf '\033[1;31m%d 项未通过（%d 项通过）\033[0m\n' "$fail" "$pass"; fi
exit $(( fail > 0 ))
