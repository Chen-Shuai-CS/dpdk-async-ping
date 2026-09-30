#!/usr/bin/env bash
# 一键运行。
#
#   scripts/run.sh A --delay-us 500 --duration-sec 60      # A：async-ping（跑在自研 runtime 上）
#   scripts/run.sh B --delay-us 500 --duration-sec 60      # B：raw-ping（手写 busy-poll）
#   scripts/run.sh C ...                                   # C：系统 ping（见 scripts/run-c.sh）
#
# 其余参数原样传给程序（--sessions / --payload / --timeout-us / --progress-sec ...）。
# 输出同时写入 logs/<A|B>-<时间>.log，报告另存 logs/<A|B>-<时间>.json。
set -euo pipefail
source "$(dirname "$0")/common.sh"

which=${1:-}; shift || true
case "$which" in
    A|a) bin=async-ping; tag=A ;;
    B|b) bin=raw-ping;   tag=B ;;
    C|c) exec "$REPO_ROOT/scripts/run-c.sh" "$@" ;;
    *) echo "用法：$0 A|B|C [参数...]"; exit 1 ;;
esac

[[ -n "${DPDK_PCI:-}" ]] || die "缺少 $NIC_ENV（先运行 scripts/setup.sh）"
"$REPO_ROOT/scripts/bind.sh" >/dev/null           # 重启后网卡会回到内核，这里自动重新绑定

# shellcheck disable=SC1091
source "$HOME/.cargo/env"
(cd "$REPO_ROOT" && cargo build --release -q --bin "$bin")

mkdir -p "$REPO_ROOT/logs"
ts=$(date +%Y%m%d-%H%M%S)
log_file="$REPO_ROOT/logs/$tag-$ts.log"
json_file="$REPO_ROOT/logs/$tag-$ts.json"
json_arg=(--json "$json_file")
for a in "$@"; do
    if [[ "$a" == --json* ]]; then json_arg=(); json_file="（由调用者指定）"; fi
done
log "运行 $tag（$bin）$*  → $log_file"
set +e
sudo "$REPO_ROOT/target/release/$bin" \
    --pci "$DPDK_PCI" --src-ip "$DPDK_IP" --dst-ip "$PEER_IP" --dst-mac "$PEER_MAC" \
    --lcore "$DPDK_LCORE" "${json_arg[@]}" "$@" 2>&1 | tee "$log_file"
rc=${PIPESTATUS[0]}
set -e
sudo chown "$(id -u):$(id -g)" "$json_file" 2>/dev/null || true
log "退出码 $rc；报告：$json_file"
exit "$rc"
