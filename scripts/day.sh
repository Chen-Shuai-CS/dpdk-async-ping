#!/usr/bin/env bash
# 一次开机里同时测两个代码版本的完整会话（跨天、跨重启的复测用）。
#
#   scripts/day.sh <名字> [旧版本标签=v1] [轮流的轮数=14] [连续监测分钟数=60]
#
# 阶段（按时间顺序；开头与 10 月 1 日重启后的那次会话相同，便于对照"开机后 20 ~ 40 分钟"那个时段）：
#   1. 旧版本的 A 600 秒 → B 600 秒（都导出样本）→ 置信区间
#   2. 三者轮流：旧版本的 A（A1）、当前代码的 A（A2）、B，各 60 秒，顺序逐轮轮换
#   3. 三者连续监测：各 20 秒，一直轮下去
#   4. 当前代码的 A 600 秒 → B 600 秒 → 置信区间
#   5. 整理成两个会话目录：logs/sessions/<名字>-<旧版本> 和 logs/sessions/<名字>-<当前版本>，供 make_report.py 汇总
#
# 第 2、3 阶段里，任何一个版本的 A 一旦出现"尾部变重"（段② p99 超过 DAY_HEAVY_NS，默认 400 ns），
# 立刻补跑一组 mfence 口径（那个版本的 A + B，各 20 秒），最多 DAY_MFENCE_MAX 组（默认 20）。
# 原始记录在 logs/sessions/<名字>-raw/。
#
# DAY_ORDER=rot-first 时顺序改为 2 → 3 → 4 → 1：一开机就让三者轮流跑（看开机后头半小时里两个版本各是什么样），
# 两对主考核放到最后，并且先测当前版本、再测旧版本（与默认顺序相反，用来抵消"谁先谁后"）。
set -uo pipefail
source "$(dirname "$0")/common.sh"
cd "$REPO_ROOT"
name=${1:?用法：scripts/day.sh <名字> [旧版本标签] [轮流的轮数] [连续监测分钟数]}
old=${2:-v1}; rounds=${3:-14}; drift_min=${4:-60}
heavy_ns=${DAY_HEAVY_NS:-400}; mf_max=${DAY_MFENCE_MAX:-20}; mf_done=0
order=${DAY_ORDER:-main-first}
[[ $order == main-first || $order == rot-first ]] || die "DAY_ORDER 只能是 main-first 或 rot-first"
main_sec=${DAY_MAIN_SEC:-600}; rot_sec=${DAY_ROT_SEC:-60}; drift_sec=${DAY_DRIFT_SEC:-20}   # 只为试跑脚本而留的开关
cur=$(python3 -c "import sys; sys.path.insert(0, 'scripts'); import write_meta; print(write_meta.detect_version())")
raw="logs/sessions/$name-raw"; s_old="logs/sessions/$name-$old"; s_cur="logs/sessions/$name-$cur"
[[ -e "$s_old/A-600.json" || -e "$s_cur/A-600.json" ]] && die "$name 已有结果，换一个名字"
mkdir -p "$raw/rot" "$raw/drift" "$raw/mfence" "$s_old" "$s_cur" logs/tmp

scripts/bind.sh > "$raw/bind.txt" 2>&1 || die "绑定网卡失败（见 $raw/bind.txt）"
scripts/check-env.sh > "$raw/check-env.txt" 2>&1 || die "环境自检未通过（见 $raw/check-env.txt）"
old_bin=$(scripts/build-version.sh "$old") || die "编译 $old 失败"
scripts/write_meta.py "$s_old/meta.json" "$name-$old" --version "$old" --commit "$(git rev-parse --short=12 "$old^{commit}")"
log "环境自检通过；旧版本 $old，当前版本 $cur"

one() {  # one <A1|A2|B> <输出前缀> [参数...]：A1 = 旧版本的 A，A2 = 当前代码的 A
    local tag=$1 o=$2 c=A; shift 2
    [[ $tag == B ]] && c=B
    if [[ $tag == A1 ]]; then
        BQ_BIN_DIR="$old_bin" RUN_LOG=/dev/null scripts/run.sh "$c" --progress-sec 0 --json "$o.json" "$@" > "$o.log" 2>&1 || warn "$o 退出码非 0"
    else
        RUN_LOG=/dev/null scripts/run.sh "$c" --progress-sec 0 --json "$o.json" "$@" > "$o.log" 2>&1 || warn "$o 退出码非 0"
    fi
}
header='utc\tclient\tsent\tlost\tleak\tinproc_mean\tinproc_p50\tinproc_p99\tseg1_mean\tseg1_slow_pct\tseg2_mean\tseg2_p50\tseg2_p90\tseg2_p99\tseg3_mean\ttotal_mean\trtt_p50_us\n'
row() {  # row <json> <标签>
    python3 - "$1" "$2" <<'PY'
import json, sys, datetime
r = json.load(open(sys.argv[1]))
g = lambda q: next(m for m in r["metrics"] if m["name"].strip().startswith(q))
ip, s1, s2, s3, e = g("in-process"), g("seg①"), g("seg②"), g("seg③"), g("end-to-end")
t = datetime.datetime.fromtimestamp(r["env"]["started_unix"], datetime.timezone.utc).strftime("%Y-%m-%d %H:%M:%S")
print("\t".join(str(x) for x in [
    t, sys.argv[2], r["counters"]["sent"], r["counters"]["timeouts"], r["mbuf"]["avail_initial"] - r["mbuf"]["avail_final"],
    round(ip["mean"], 1), ip["p50_interp"], ip["p99_interp"], round(s1["mean"], 1), round(r["seg1_slow_percent"], 3),
    round(s2["mean"], 1), s2["p50_interp"], s2["p90_interp"], s2["p99_interp"], round(s3["mean"], 1),
    round(s1["mean"] + s2["mean"] + s3["mean"], 1), round(e["p50"] / 1000)]))
PY
}
seg2_p99() { python3 -c "import json,sys; r=json.load(open(sys.argv[1])); print(int(next(m for m in r['metrics'] if m['name'].startswith('seg②'))['p99_interp']))" "$1" 2>/dev/null || echo 0; }
[[ -f "$raw/mfence.tsv" ]] || printf "$header" > "$raw/mfence.tsv"
maybe_mfence() {  # maybe_mfence <A1|A2> <刚跑完的 json> <编号>：尾部变重就补一组 mfence 口径
    local tag=$1 j=$2 id=$3
    (( $(seg2_p99 "$j") > heavy_ns && mf_done < mf_max )) || return 0
    mf_done=$((mf_done + 1))
    log "  $tag 的段② p99 超过 $heavy_ns ns → 补一组 mfence 口径（第 $mf_done 组）"
    one "$tag" "$raw/mfence/$tag-$id" --delay-us 500 --duration-sec 20 --diag-pre-t0 mfence; row "$raw/mfence/$tag-$id.json" "$tag" >> "$raw/mfence.tsv"
    one B "$raw/mfence/B-$id" --delay-us 500 --duration-sec 20 --diag-pre-t0 mfence;        row "$raw/mfence/B-$id.json" B >> "$raw/mfence.tsv"
}

# ---- 1. 旧版本的主考核 ----
phase_old_main() {
log "阶段 1：$old 的 A 600 秒 → B 600 秒"
one A1 "$s_old/A-600" --delay-us 500 --duration-sec "$main_sec" --samples "logs/tmp/$name-$old-A.samples"
one B  "$s_old/B-600" --delay-us 500 --duration-sec "$main_sec" --samples "logs/tmp/$name-$old-B.samples"
python3 scripts/ci.py --a "logs/tmp/$name-$old-A.samples" --b "logs/tmp/$name-$old-B.samples" --out "$s_old/ci.json" | tail -1
}

# ---- 2. 三者轮流，各 60 秒 ----
phase_rot() {
log "阶段 2：A1 / A2 / B 轮流，各 60 秒 × $rounds 轮"
printf "$header" > "$raw/rot.tsv"
orders=("A1 A2 B" "A2 B A1" "B A1 A2")
for i in $(seq 1 "$rounds"); do
    for t in ${orders[$(( (i - 1) % 3 ))]}; do
        one "$t" "$raw/rot/$t-$i" --delay-us 500 --duration-sec "$rot_sec"
        row "$raw/rot/$t-$i.json" "$t" >> "$raw/rot.tsv"
        [[ $t != B ]] && maybe_mfence "$t" "$raw/rot/$t-$i.json" "rot$i"
    done
done
}

# ---- 3. 三者连续监测，各 20 秒 ----
phase_drift() {
log "阶段 3：A1 / A2 / B 连续监测，各 20 秒，共 $drift_min 分钟"
printf "$header" > "$raw/drift.tsv"
end=$(( $(date +%s) + drift_min * 60 )); i=0
while (( $(date +%s) < end )); do
    i=$((i + 1)); id=$(printf '%03d' $i)
    for t in ${orders[$(( (i - 1) % 3 ))]}; do
        one "$t" "$raw/drift/$t-$id" --delay-us 500 --duration-sec "$drift_sec"
        row "$raw/drift/$t-$id.json" "$t" >> "$raw/drift.tsv"
        [[ $t != B ]] && maybe_mfence "$t" "$raw/drift/$t-$id.json" "drift$id"
    done
done
}

# ---- 4. 当前版本的主考核 ----
phase_cur_main() {
log "阶段 4：$cur 的 A 600 秒 → B 600 秒"
scripts/write_meta.py "$s_cur/meta.json" "$name-$cur"
one A2 "$s_cur/A-600" --delay-us 500 --duration-sec "$main_sec" --samples "logs/tmp/$name-$cur-A.samples"
one B  "$s_cur/B-600" --delay-us 500 --duration-sec "$main_sec" --samples "logs/tmp/$name-$cur-B.samples"
python3 scripts/ci.py --a "logs/tmp/$name-$cur-A.samples" --b "logs/tmp/$name-$cur-B.samples" --out "$s_cur/ci.json" | tail -1
}

if [[ $order == rot-first ]]; then
    phase_rot; phase_drift; phase_cur_main; phase_old_main
else
    phase_old_main; phase_rot; phase_drift; phase_cur_main
fi

# ---- 5. 整理成两个会话目录（make_report.py 认的布局：ab/A-i、B-i；drift/runs/A-xxx、B-xxx）----
for pair in "$s_old A1" "$s_cur A2"; do
    set -- $pair; d=$1; a=$2
    mkdir -p "$d/ab" "$d/drift/runs"
    for f in "$raw"/rot/"$a"-*.json;   do cp "$f" "$d/ab/A-${f##*-}"; done
    for f in "$raw"/rot/B-*.json;      do cp "$f" "$d/ab/"; done
    for f in "$raw"/drift/"$a"-*.json; do cp "$f" "$d/drift/runs/A-${f##*-}"; done
    for f in "$raw"/drift/B-*.json;    do cp "$f" "$d/drift/runs/"; done
done
log "会话 $name 完成 → $s_old ， $s_cur （原始记录 $raw）"
