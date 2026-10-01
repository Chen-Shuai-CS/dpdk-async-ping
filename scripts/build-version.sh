#!/usr/bin/env bash
# 把某个历史版本（git 标签）的 A / B 编译出来，放在仓库之外，供对比测试用。
#
#   scripts/build-version.sh v1        →  ~/.cache/bq-build/versions/v1/target/release/{async-ping,raw-ping}
#
# 之后用 BQ_BIN_DIR 让 run.sh 运行那个版本：
#   BQ_BIN_DIR=$(scripts/build-version.sh v1) scripts/run.sh A --delay-us 500 --duration-sec 60
#
# 用 git worktree 检出，所以那个二进制报告里的"构建：commit …"就是标签对应的提交。
set -euo pipefail
source "$(dirname "$0")/common.sh"
tag=${1:?用法：scripts/build-version.sh <标签，如 v1>}
dir="$BUILD_ROOT/versions/$tag"
if [[ ! -d "$dir/.git" && ! -f "$dir/.git" ]]; then
    mkdir -p "$(dirname "$dir")"
    git -C "$REPO_ROOT" worktree add --detach "$dir" "$tag" >&2
    cp "$REPO_ROOT/config/nic.env" "$dir/config/" 2>/dev/null || true
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env"
(cd "$dir" && cargo build --release -q --bin async-ping --bin raw-ping) >&2
echo "$dir/target/release"
