#!/usr/bin/env bash
# 把 SPEC 的硬性要求变成一条条可以机器核对的检查。全部通过则退出码 0。
#
#   scripts/check-compliance.sh              # 检查代码 + 已有的日志
#   scripts/check-compliance.sh --quick      # 跳过 clippy / 测试（只查静态项和日志）
#   scripts/check-compliance.sh --self-test  # 检查"检查器"自己：命令不存在、非零退出、输出里没有错误字样但失败了……都必须判为未通过
#
# 这不是测试的替代品：它回答的是"交付物是否符合题目的每一条硬性规定"，而不是"代码对不对"。
#
# 判定规则：**一律以命令的退出码为准**，不靠"输出里没有出现错误字样"。后者在命令根本没跑起来（比如没有装 cargo）、
# 或者失败信息不含预设的字样（比如编译失败）时会误报通过。
# 每一节的标题注明它是"本次重新执行"还是"检查已有的日志"：后者只说明上一次测量的结果符合要求，不代表刚刚又测了一遍。
set -uo pipefail
source "$(dirname "$0")/common.sh"
cd "$REPO_ROOT"
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true
quick=0; [[ "${1:-}" == --quick ]] && quick=1
pass=0; fail=0
check() {  # check <描述> <命令...>：命令的退出码为 0 = 通过；其他任何情况（包括命令不存在）= 未通过
    local what=$1; shift
    if "$@" >/dev/null 2>&1; then ok "$what"; pass=$((pass + 1)); else printf '\033[1;31m  ✘ %s\033[0m\n' "$what"; fail=$((fail + 1)); fi
}
no_match() {  # no_match <正则> <文件...>：没有任何一行匹配才算通过。grep 自己出错（文件不存在等，退出码 ≥ 2）算未通过
    grep -Eq -- "$@"
    [[ $? -eq 1 ]]
}

if [[ "${1:-}" == --self-test ]]; then
    # 用已知结果的命令去喂 check / no_match，核对判定。期望通过的放前一组，期望未通过的放后一组。
    t_ok=0; t_bad=0
    expect() {  # expect <pass|fail> <描述> <命令...>
        local want=$1 what=$2; shift 2
        local got=fail
        "$@" >/dev/null 2>&1 && got=pass
        if [[ $got == "$want" ]]; then ok "$what → 判为$([[ $want == pass ]] && echo 通过 || echo 未通过)"; t_ok=$((t_ok + 1))
        else printf '\033[1;31m  ✘ %s：应当判为 %s，实际判为 %s\033[0m\n' "$what" "$want" "$got"; t_bad=$((t_bad + 1)); fi
    }
    tmp=$(mktemp); printf 'fn main() {}\n' > "$tmp"
    log "检查器自检：退出码判定"
    expect pass "命令成功（退出码 0）" true
    expect fail "命令失败（退出码 1）" false
    expect fail "命令不存在（退出码 127）" this-command-does-not-exist --version
    expect fail "输出里全是 ok，但退出码非 0（例如测试进程崩溃）" bash -c 'echo "test a ... ok"; echo "test result: ok. 1 passed"; exit 101'
    expect fail "输出里没有任何预设的错误字样，但退出码非 0（例如编译失败）" bash -c 'echo "could not compile"; exit 101'
    expect pass "输出里有 warning 字样，但退出码为 0" bash -c 'echo "warning: just a word"; exit 0'
    log "检查器自检：no_match（"不得出现"类检查）"
    expect pass "文件存在且没有匹配" no_match 'tokio' "$tmp"
    expect fail "文件存在且有匹配" no_match 'fn main' "$tmp"
    expect fail "文件不存在（grep 退出码 2）" no_match 'tokio' /nonexistent/file
    rm -f "$tmp"
    echo
    if (( t_bad == 0 )); then log "检查器自检：$t_ok 项全部符合预期"; else printf '\033[1;31m检查器自检：%d 项不符合预期\033[0m\n' "$t_bad"; fi
    exit $(( t_bad > 0 ))
fi
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
check "Cargo.lock 里没有任何现成的 async runtime / 执行器 / reactor 库" no_match "$forbidden" Cargo.lock
check "rt（runtime）只依赖本仓库的 dpdk 与 timerq 两个 crate" bash -c "[[ \$(sed -n '/^\[dependencies\]/,/^\[/p' crates/rt/Cargo.toml | grep -c '^[a-z]') == 2 ]] && grep -q 'dpdk = { path' crates/rt/Cargo.toml && grep -q 'timerq = { path' crates/rt/Cargo.toml"
check "rt 自己实现了 Waker（RawWakerVTable）、就绪队列、timer、poll-mode 主循环" bash -c "grep -q RawWakerVTable crates/rt/src/executor.rs && grep -q 'struct ReadyQueue' crates/rt/src/executor.rs && grep -q 'rx_burst' crates/rt/src/runtime.rs && grep -q 'fire' crates/rt/src/timer.rs"
check "主循环里没有任何会睡眠 / 阻塞的调用（epoll、sleep、park、condvar）" no_match 'epoll|thread::sleep|park\(|Condvar|nanosleep|usleep' crates/rt/src/*.rs crates/async-ping/src/*.rs crates/raw-ping/src/*.rs crates/pingkit/src/sender.rs crates/pingkit/src/house.rs

log "二、三个客户端与命令行（SPEC §6）〔本次重新执行〕"
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
check "协议解析（pingproto）与 timer 堆（timerq）完全不含 unsafe" bash -c "test -d crates/pingproto/src && test -d crates/timerq/src && [[ -z \"\$(grep -rn 'unsafe' crates/pingproto/src crates/timerq/src | grep -Ev ':\s*//')\" ]]"

if (( ! quick )); then
    log "四、静态检查与测试〔本次重新执行；以 cargo 的退出码为准〕"
    # -D warnings：任何一条警告都让 clippy 以非零退出码结束。命令不存在、编译失败同样是非零。
    check "cargo clippy --workspace --all-targets -- -D warnings：退出码 0" cargo clippy --release --workspace --all-targets -- -D warnings
    # 测试只跑一次：保留这一次的输出和退出码；从输出里数出来的个数只用于展示，不参与判定
    test_log=$(mktemp)
    cargo test --release --workspace > "$test_log" 2>&1; test_rc=$?
    tests=$(grep -c '^test .* ok$' "$test_log")
    check "cargo test --workspace：退出码 0（本次执行，$tests 个测试通过）" test "$test_rc" -eq 0
    (( test_rc == 0 )) || { echo "      cargo test 的退出码是 $test_rc，输出的最后几行："; tail -15 "$test_log" | sed 's/^/      /'; }
    rm -f "$test_log"
fi

log "五、硬门槛：主考核日志（SPEC：≥ 10 分钟不崩、零 mbuf 泄漏、零丢包或有解释）〔检查已有的日志 logs/final/，不重新运行〕"
for t in A B; do
    f=logs/final/$t-600.json
    if [[ ! -f $f ]]; then check "$t：存在 $f" false; continue; fi
    check "$t：运行时长 ≥ 600 秒，正常退出" json "$f" 'r["elapsed_sec"] >= 600 and "duration" in r["exit_reason"]'
    check "$t：零 mbuf 泄漏" json "$f" 'm["avail_initial"] == m["avail_final"]'
    check "$t：零丢包（0 超时）" json "$f" 'c["timeouts"] == 0'
    check "$t：请求对账与收包对账都为 0" json "$f" 'c["sent"] - c["received"] - c["timeouts"] - c["in_flight_at_end"] == 0 and c["rx_pkts"] - (c["received"] + c["late"] + c["unexpected"] + c["foreign"] + c["tsc_mismatch"] + c["other_rx"] + c["arp_replies"]) == 0'
    check "$t：样本数 = received，且没有被拒绝的回复（时间戳核对不符）" json "$f" 'ip["count"] == c["received"] and c["tsc_mismatch"] == 0'
    check "$t：AWS 限额计数、网卡丢弃计数全为 0" json "$f" 'all(v == 0 for _, v in r["port"]["allowance_exceeded"]) and r["port"]["imissed"] == 0 and r["port"]["ierrors"] == 0 and r["port"]["rx_nombuf"] == 0'
    check "$t：是排名口径的运行（没有开任何诊断开关），且由干净的源码树构建" json "$f" 'r["diag"] == [] and not r["env"]["git_dirty"]'
done
check "A 与 B 的主考核来自同一个代码版本" python3 -c "
import json, sys
a, b = (json.load(open(f'logs/final/{t}-600.json'))['env']['git_commit'] for t in 'AB')
sys.exit(0 if a == b and a not in ('', 'unknown') else 1)"

log "六、交付物（SPEC §8）〔检查文件与已有的日志〕"
check "README.md、docs/REPORT.md（延迟报告）、docs/WORKLOG.md 都在" bash -c "test -s README.md && test -s docs/REPORT.md && test -s docs/WORKLOG.md"
check "一键搭环境 / 一键运行脚本可执行" bash -c "test -x scripts/setup.sh && test -x scripts/run.sh && test -x scripts/check-env.sh"
check "报告由脚本从日志生成（可复现）" bash -c "test -x scripts/make_report.py && grep -q 'BEGIN:main' docs/REPORT.md"
check "故障注入结果存在且全部通过" bash -c "f=\$(ls -d logs/fault/*/ | tail -1)summary.json && python3 -c \"import json,sys; r=json.load(open('\$f')); sys.exit(0 if r and all(x['ok'] for x in r) else 1)\""

echo
if (( fail == 0 )); then log "全部 $pass 项通过"; else printf '\033[1;31m%d 项未通过（%d 项通过）\033[0m\n' "$fail" "$pass"; fi
exit $(( fail > 0 ))
