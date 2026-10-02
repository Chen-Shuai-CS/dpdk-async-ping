#!/usr/bin/env python3
"""把 docs/REPORT.md 里各个 <!-- BEGIN:x --> … <!-- END:x --> 之间的表格，用 logs/ 下的 JSON 重新生成。

用法：scripts/make_report.py            （使用下面 DEFAULTS 里的路径；可用同名参数覆盖）
"""
import argparse
import glob
import json
import os
import re
import statistics
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

DEFAULTS = {
    "main_a": "logs/final/A-600.json",
    "main_b": "logs/final/B-600.json",
    "rate_a": "logs/final/A-vsC.json",
    "rate_b": "logs/final/B-vsC.json",
    "one_a": "logs/final/A-1flow.json",
    "probe_a": "logs/probe/async-ping-probe.json",
    "probe_b": "logs/probe/raw-ping-probe.json",
    "ci": "logs/final/ci.json",
    "diag_dir": "logs/diag",
    "soak_a": "logs/soak/A-1800.json",
    "soak_b": "logs/soak/B-1800.json",
}

# 双侧 95% 的 t 分位数（自由度 = 对数 − 1），用于"逐对差值的平均值"的置信区间
T95 = {1: 12.706, 2: 4.303, 3: 3.182, 4: 2.776, 5: 2.571, 6: 2.447, 7: 2.365, 8: 2.306, 9: 2.262, 10: 2.228, 11: 2.201,
       12: 2.179, 13: 2.160, 14: 2.145, 15: 2.131, 19: 2.093, 21: 2.080, 27: 2.052, 29: 2.045, 35: 2.030, 49: 2.010}


def load(p):
    with open(os.path.join(ROOT, p)) as f:
        return json.load(f)


def metric(r, prefix):
    for m in r["metrics"]:
        if m["name"].strip().startswith(prefix):
            return m
    return None


def section_main(a):
    out = subprocess.run([sys.executable, os.path.join(ROOT, "scripts/report.py"), "--a", os.path.join(ROOT, a.main_a),
                          "--b", os.path.join(ROOT, a.main_b)], capture_output=True, text=True, check=True).stdout
    return out.strip()


def section_totals(a):
    """每个请求的平均耗时总账：平均值可以相加，分位数不能。"""
    A, B = load(a.main_a), load(a.main_b)
    mean = lambda r, pre: metric(r, pre)["mean"]
    s1a, s2a, s3a = mean(A, "seg①"), mean(A, "seg②"), mean(A, "seg③")
    s1b, s2b, s3b = mean(B, "seg①"), mean(B, "seg②"), mean(B, "seg③")
    rows = [
        ("段① 发送（T0 → T1）", s1a, s1b, False),
        ("段② 接收（T2 → T3）", s2a, s2b, False),
        ("段③ timer 发现到期 → 下一个 T0（不计分）", s3a, s3b, False),
        ("**排名口径：① + ②**", s1a + s2a, s1b + s2b, True),
        ("发送侧合计：③ + ①（发现到期 → T1）", s1a + s3a, s1b + s3b, False),
        ("**三段合计：① + ② + ③**", s1a + s2a + s3a, s1b + s2b + s3b, True),
    ]
    notes = {("B", "seg1"): "<- includes the stall", ("A", "seg3"): "<- scheduling + the stall land here (not ranked)",
             ("A", "seg2"): "<- the runtime's tax"}
    bars = []
    for lab, vals in (("B", (s3b, s1b, s2b)), ("A", (s3a, s1a, s2a))):
        for i, (seg, v) in enumerate(zip(("seg3", "seg1", "seg2"), vals)):
            line = (f"{lab if i == 0 else ' '}  {seg} |" + "#" * round(v / 10)).ljust(26) + f"{v:6.1f}"
            bars.append(line + ("  " + notes[(lab, seg)] if (lab, seg) in notes else ""))
        tot = sum(vals)
        tail = f"  <- A - B = {tot - (s1b + s2b + s3b):+.1f} ns per request" if lab == "A" else ""
        bars.append("   total".ljust(26) + f"{tot:6.1f}" + tail)
        if lab == "B":
            bars.append("")
    out = ["每个请求的平均耗时落在哪一段（ns；1 个 `#` ≈ 10 ns）：\n", "```text", *bars, "```", "",
           "| 每个请求的平均耗时（ns） | A | B | A − B |", "|---|---|---|---|"]
    for name, x, y, bold in rows:
        diff = f"**{x - y:+.1f}**" if bold else f"{x - y:+.1f}"
        out.append(f"| {name} | {x:.1f} | {y:.1f} | {diff} |")
    return "\n".join(out)


def mean_ci(xs):
    """平均值与 95% 置信区间（t 分布）。"""
    n = len(xs)
    m = statistics.fmean(xs)
    if n < 2:
        return m, m, m
    half = T95.get(n - 1, 1.96) * statistics.stdev(xs) / n ** 0.5
    return m, m - half, m + half


def section_ab(a):
    """A / B 交替多轮：逐对列出，再对"逐对差值"做统计（反映不同运行之间的波动，这是单次运行的置信区间看不到的）。"""
    parts = []
    for d in sorted(glob.glob(os.path.join(ROOT, a.ab))):
        runs = {}
        for p in sorted(glob.glob(d + "/[AB]-*.json")):
            with open(p) as f:
                r = json.load(f)
            runs[(r["client"][0], int(os.path.basename(p).split("-")[1].split(".")[0]))] = r
        idx = sorted({i for _, i in runs} & {i for c, i in runs if c == "A"} & {i for c, i in runs if c == "B"})
        if not idx or "p50_interp" not in metric(runs[("A", idx[0])], "in-process"):
            continue
        secs = runs[("A", idx[0])]["duration_sec"]
        stats = [("进程内 p50", "in-process", "p50_interp"), ("进程内 p99", "in-process", "p99_interp"),
                 ("进程内平均", "in-process", "mean"), ("段② p50", "seg②", "p50_interp"), ("段② p99", "seg②", "p99_interp"),
                 ("段② 平均", "seg②", "mean"), ("段① 平均", "seg①", "mean"), ("① + ② + ③ 平均", None, None)]

        def val(r, name, key):
            if name is None:
                return sum(metric(r, n)["mean"] for n in ("seg①", "seg②", "seg③"))
            return metric(r, name)[key]

        diffs = {label: [val(runs[("A", i)], n, k) - val(runs[("B", i)], n, k) for i in idx] for label, n, k in stats}
        lines = [f"**{len(idx)} 对 × {secs} 秒**（`{os.path.relpath(d, ROOT)}`；奇数对先 A 后 B，偶数对先 B 后 A；全部 {2 * len(idx)} 轮都是 0 丢包、0 泄漏）\n"
                 if all(runs[k]["counters"]["timeouts"] == 0 and runs[k]["mbuf"]["avail_initial"] == runs[k]["mbuf"]["avail_final"] for k in runs)
                 else f"**{len(idx)} 对 × {secs} 秒**（`{os.path.relpath(d, ROOT)}`；奇数对先 A 后 B，偶数对先 B 后 A；**有的轮次出现了丢包，见表后的说明**）\n",
                 "逐对结果（ns；分位数为插值分位数，括号里是程序直接打印的格点值）：\n",
                 "| 对 | 顺序 | 进程内 p50：A / B | A − B | 进程内 p99：A / B | A − B | 进程内平均 A − B | 段① ≥ 125 ns 占比：A / B | 往返 p50：A / B（µs） |",
                 "|---|---|---|---|---|---|---|---|---|"]
        for n_, i in enumerate(idx):
            A, B = runs[("A", i)], runs[("B", i)]
            ma, mb = metric(A, "in-process"), metric(B, "in-process")
            ea, eb = metric(A, "end-to-end"), metric(B, "end-to-end")
            lines.append(
                f"| {i} | {'A → B' if i % 2 else 'B → A'} | {ma['p50_interp']:.1f}（{ma['p50']}）/ {mb['p50_interp']:.1f}（{mb['p50']}） "
                f"| {diffs['进程内 p50'][n_]:+.1f} | {ma['p99_interp']:.1f}（{ma['p99']}）/ {mb['p99_interp']:.1f}（{mb['p99']}） "
                f"| {diffs['进程内 p99'][n_]:+.1f} | {diffs['进程内平均'][n_]:+.1f} "
                f"| {A['seg1_slow_percent']:.2f}% / {B['seg1_slow_percent']:.2f}% | {ea['p50'] / 1000:.0f} / {eb['p50'] / 1000:.0f} |")
        lines += ["", f"对 {len(idx)} 个逐对差值（A − B，ns）做统计：\n",
                  "| 指标 | 平均 | 95% 置信区间 | 最小 | 最大 | A − B > 0 的对数 | 先 A 后 B 的对：平均 | 先 B 后 A 的对：平均 |", "|---|---|---|---|---|---|---|---|"]
        for label, _, _ in stats:
            x = diffs[label]
            m, lo, hi = mean_ci(x)
            ab = [v for v, i in zip(x, idx) if i % 2]
            ba = [v for v, i in zip(x, idx) if not i % 2]
            lines.append(f"| {label} | **{m:+.1f}** | [{lo:+.1f}, {hi:+.1f}] | {min(x):+.1f} | {max(x):+.1f} | {sum(v > 0 for v in x)} / {len(x)} "
                         f"| {statistics.fmean(ab):+.1f} | {statistics.fmean(ba) if ba else float('nan'):+.1f} |")
        # 有丢包的轮次：逐个说明（丢了多少、有没有迟到 / 重复的回复、当时主循环停了多久、网卡计数器有没有被重置）
        for (side, i), r in sorted(runs.items(), key=lambda kv: (kv[0][1], kv[0][0])):
            c = r["counters"]
            if c["timeouts"] == 0:
                continue
            stall = max(r["stalls"]["max_ns"], r["stalls"]["rx_max_ns"]) / 1e6
            rebased = r["port"]["ipackets"] - c["rx_pkts"]
            lines.append(f"\n**第 {i} 对的 {side} 有 {c['timeouts']} 个请求超时**（迟到收到 {c['late']} 个，另收到 {c['unexpected']} 个对不上号的回复）。"
                         f"这次运行里主循环最长的一次停顿是 {stall:.1f} ms"
                         + (f"，网卡自身的接收计数与程序收到的包数相差 {rebased:,}（计数器在运行中途被重置过）" if rebased else "")
                         + "。零泄漏，两条对账仍为 0。原因见 §8。")
        slow_a = [runs[("A", i)]["seg1_slow_percent"] for i in idx]
        slow_b = [runs[("B", i)]["seg1_slow_percent"] for i in idx]
        p99_a = [metric(runs[("A", i)], "seg①")["p99"] for i in idx]
        lines.append(f"\n段① ≥ 125 ns 的占比在各轮之间的范围：A {min(slow_a):.2f}% ~ {max(slow_a):.2f}%，B {min(slow_b):.2f}% ~ {max(slow_b):.2f}%。"
                     f"A 的段① p99 各轮为 {' / '.join(str(v) for v in p99_a)} ns：占比低于 1% 时读数是 70 ~ 80 ns，一旦高于 1% 就跳到 130 ns 以上。")
        parts.append("\n".join(lines))
    return "\n\n".join(parts) if parts else "（没有找到新格式的交替运行结果）"


def c_row(label, cond, r):
    is_c = "flows" in r
    m = r["metrics"][0] if is_c else metric(r, "end-to-end")
    lost = (r["sent"] - r["received"]) if is_c else r["counters"]["timeouts"]
    pps = r.get("actual_pps") if is_c else r["counters"]["sent"] / r["elapsed_sec"]   # 都是实测：发出的包数 ÷ 实际时长
    us = lambda v: f"{v / 1000:,.1f}"
    return (f"| {label} | {cond} | {pps:,.0f} | {m['count']:,} | {lost} | {us(m['min'])} | {us(m['p50'])} | {us(m['p90'])} | {us(m['p99'])} "
            f"| {us(m['p99_9'])} | {us(m['p99_99'])} | {us(m['max'])} |")


def latest_c(mode, flows):
    p = os.path.join(ROOT, f"logs/c/{mode}-{flows}flow/C.json")
    if not os.path.exists(p):
        return None
    with open(p) as f:
        return json.load(f)


def section_c(a):
    hdr = ("| 客户端 | 条件 | 实测速率（包/秒） | 样本 | 丢包 | min | p50 | p90 | p99 | p99.9 | p99.99 | max |\n"
           "|---|---|---|---|---|---|---|---|---|---|---|---|")
    ra, rb, r1 = load(a.rate_a), load(a.rate_b), load(a.one_a)
    out = ["#### 场景一：64 路、速率对齐，端到端（µs）\n", hdr,
           c_row("A", f"64 session，delay {ra['delay_us']} µs", ra),
           c_row("B", f"64 session，delay {rb['delay_us']} µs", rb)]
    for mode, desc in (("user", "64 × `ping -U -i 0.001`（用户态↔用户态）"), ("kernel", "64 × `ping -i 0.001`（内核收包时间戳）")):
        r = latest_c(mode, 64)
        if r:
            out.append(c_row("C", desc, r))
    out += ["\n#### 场景二：单路、低速率，端到端（µs）\n", hdr,
            c_row("A", f"1 session，delay {r1['delay_us']} µs", r1)]
    for mode, desc in (("user", "1 × `ping -U -i 0.001`（用户态↔用户态）"), ("kernel", "1 × `ping -i 0.001`（内核收包时间戳）")):
        r = latest_c(mode, 1)
        if r:
            out.append(c_row("C", desc, r))
    return "\n".join(out)


def section_probe(a):
    out = []
    try:
        pa, pb = load(a.probe_a), load(a.probe_b)
    except FileNotFoundError:
        return "（未找到 probe 运行结果）"
    out += ["段①的三个子步骤（ns；只统计距上次发送 ≥ 250 ns 的发送；每步各含一次约 18 ns 的时钟读取）：\n",
            "| | | p50 | p90 | p99 | p99.9 | p99.99 |", "|---|---|---|---|---|---|---|"]
    for name in ("(probe) ①取 mbuf", "(probe) ①写包", "(probe) ①tx_burst"):
        for label, r in (("A", pa), ("B", pb)):
            m = metric(r, name)
            if m:
                out.append(f"| {name.replace('(probe) ①', '')} | {label} | {m['p50']} | {m['p90']} | {m['p99']} | {m['p99_9']} | {m['p99_99']} |")
    out.append("\n`tx_burst` 超过 125 ns 的占比，按「发送计数 % 32」分（32 个 LLQ 条目 = 一个 4 KB 页）：\n")
    out += ["| 余数 | " + " | ".join(str(i) for i in range(32)) + " |", "|---|" + "---|" * 32]
    for label, r in (("A", pa), ("B", pb)):
        notes = [n for n in r.get("probe_notes", []) if "tx_burst" in n]
        if notes:
            vals = re.findall(r"\d+:([0-9.]+)%", notes[0])
            out.append(f"| {label} | " + " | ".join(vals) + " |")
    return "\n".join(out)


QN = [("p50", "p50"), ("p90", "p90"), ("p99", "p99"), ("p99.9", "p99_9"), ("p99.99", "p99_99")]


def ci_str(c):
    return f"[{c[0]:+.1f}, {c[1]:+.1f}]"


def ci_table(c):
    """A − B 的点估计与 95% 区间（分块自助法）。"""
    names = {"inproc": "进程内 ① + ②", "seg1": "段①（发送）", "seg2": "段②（接收）"}
    out = ["| 指标 | 统计量 | A | B | A − B（格点） | **A − B（插值）** | 95% 置信区间 | A 更慢的把握 |", "|---|---|---|---|---|---|---|---|"]
    for key in ("inproc", "seg1", "seg2"):
        m = c["metrics"][key]
        d = m["diff"]["mean"]
        out.append(f"| **{names[key]}** | 平均 | {m['A']['mean']:.1f} | {m['B']['mean']:.1f} | — | **{d['value']:+.1f}** | {ci_str(d['ci'])} | {d['p_gt0']:.0%} |")
        for label, q in QN:
            d = m["diff"][q]
            out.append(f"| | {label} | {m['A'][q]['interp']:.1f} | {m['B'][q]['interp']:.1f} | {d['grid']:+d} | **{d['interp']:+.1f}** "
                       f"| {ci_str(d['interp_ci'])} | {d['p_gt0']:.0%} |")
    return out


def section_ci(a):
    try:
        c = load(a.ci)
    except FileNotFoundError:
        return "（未找到 ci.json；先用 --samples 运行 A / B，再运行 scripts/ci.py）"
    ia, ib = c["inputs"]["A"], c["inputs"]["B"]
    out = [f"样本：A {ia['samples']:,} 个，B {ib['samples']:,} 个（与主考核是同一次运行）；时间块 {c['block_sec']:g} 秒，重抽 {c['replicates']} 次；"
           f"时间戳分辨率 {c['tsc_step_ns']:g} ns。单位 ns。\n"]
    out += ci_table(c)
    e = c["effective"]
    n_all = ia["samples"] + ib["samples"]
    out += ["", f"**样本并不独立，块长的选择很重要。** 同样的数据，用不同的方式重抽，A − B 的 95% 区间如下：\n",
            "| 重抽方式 | 进程内平均 A − B | 进程内 p50 A − B | 进程内 p99 A − B |", "|---|---|---|---|",
            f"| 假装 {n_all / 1e8:.2f} 亿个样本相互独立 | {ci_str(c['iid']['mean_diff_ci'])} | {ci_str(c['iid']['p50_diff_ci'])} | {ci_str(c['iid']['p99_diff_ci'])} |"]
    for r in c["block_sensitivity"]:
        mark = "**" if abs(r["block_sec"] - c["block_sec"]) < 1e-9 else ""
        out.append(f"| {mark}按 {r['block_sec']:g} 秒分块（{r['blocks']} 块）{mark} | {ci_str(r['mean_diff_ci'])} | {ci_str(r['p50_diff_ci'])} | {ci_str(r['p99_diff_ci'])} |")
    out.append(f"\n区间随块长增大而变宽：相关性不只存在于相邻的样本之间，还存在于几秒到几十秒的尺度上（对端的往返时间会换档，见下面的每秒序列）。"
               f"块太短会把这部分相关性切断、把区间算窄，所以上表采用 {c['block_sec']:g} 秒的块；按这个块长，区间比\"假装独立\"宽 "
               f"{e['p99_diff_ci']['design_effect'] ** 0.5:.0f} ~ {max(v['design_effect'] for v in e.values()) ** 0.5:.0f} 倍。"
               "块再长（60 秒，只剩 10 块）区间还会略宽，但块数太少时重抽本身就不可靠。"
               "所以**单次运行的区间应当看作不确定度的下限**；不同运行之间的波动见 §3.3。")
    sf = c["seg1_slow_fraction"]
    out.append(f"\n**段① 的 p99 为什么不稳定。** 段① ≥ {sf['threshold_ns']:.0f} ns 的样本占比：A {sf['A']['percent']:.2f}%"
               f"（95% 区间 {sf['A']['ci'][0]:.2f}% ~ {sf['A']['ci'][1]:.2f}%），B {sf['B']['percent']:.2f}%（{sf['B']['ci'][0]:.2f}% ~ {sf['B']['ci'][1]:.2f}%）。")
    sa, sb = c["series"]["A"]["summary"], c["series"]["B"]["summary"]
    out += ["", "**10 分钟内的变化范围。** 把每一秒单独算一次（进程内耗时，ns）：\n",
            "| | 每秒 p50：最小 / 中位 / 最大 | 每秒 p99：最小 / 中位 / 最大 | 每秒平均：最小 / 中位 / 最大 |", "|---|---|---|---|"]
    for lab, x in (("A", sa), ("B", sb)):
        out.append(f"| {lab} | " + " | ".join(f"{x[k]['min']:.1f} / {x[k]['median']:.1f} / {x[k]['max']:.1f}" for k in ("p50", "p99", "mean")) + " |")
    return "\n".join(out)


def section_burst(a):
    try:
        c = load(a.ci)
    except FileNotFoundError:
        return "（未找到 ci.json）"
    b = c["burst_position"]
    out = ["| 这个回复在它那一批里的位置 | 占比：A / B | 段② 平均：A / B | **A − B** | 95% 区间 | 段② p50：A / B | **A − B** | 95% 区间 | 段② p99 A − B | 95% 区间 |",
           "|---|---|---|---|---|---|---|---|---|---|"]
    for key in ("alone", "pos1", "pos2", "pos3", "pos4+"):
        r = b[key]
        d = r["diff"]
        out.append(f"| {r['label']} | {r['share']['A']:.1%} / {r['share']['B']:.1%} | {r['A']['mean']:.1f} / {r['B']['mean']:.1f} "
                   f"| **{d['mean']['value']:+.1f}** | {ci_str(d['mean']['ci'])} | {r['A']['p50']['interp']:.1f} / {r['B']['p50']['interp']:.1f} "
                   f"| **{d['p50']['interp']:+.1f}** | {ci_str(d['p50']['interp_ci'])} | {d['p99']['interp']:+.1f} | {ci_str(d['p99']['interp_ci'])} |")
    ia, ib = c["inputs"]["A"], c["inputs"]["B"]

    def recon(r, info):
        """程序自己统计的 rx_burst 分布（含所有包）与由样本还原出的（只含按时到达的 echo reply）逐档相比。"""
        prog = {}
        for k, n in r["burst_sizes"]:
            key = str(k) if k < 8 else "8+"
            prog[key] = prog.get(key, 0) + n
        smp = info["burst_sizes_from_samples"]
        diff = sum(abs(prog.get(k, 0) - smp.get(k, 0)) for k in set(prog) | set(smp))
        cc = r["counters"]
        return diff, cc["foreign"] + cc["other_rx"] + cc["arp_replies"] + cc["late"] + cc["unexpected"]

    (da, na), (db, nb) = recon(load(a.main_a), ia), recon(load(a.main_b), ib)
    out.append(f"\n核对这种还原方法：把由样本还原出的批大小分布，与程序运行时自己统计的 rx_burst 分布逐档相比，"
               f"A 总共相差 {da} 批、B 相差 {db} 批，而运行中收到的非 echo 包分别是 {na} 个、{nb} 个"
               f"（程序的统计包含所有包，样本只含 echo reply；一个非 echo 包若与回复同批到达，会同时改变相邻两档的计数）——差别全部来自这些包。"
               f"同一批内段②随位置递增的比例：A {ia['seg2_nondecreasing_within_burst']:.4%}、B {ib['seg2_nondecreasing_within_burst']:.4%}。")
    return "\n".join(out)


def section_diag(a):
    d = os.path.join(ROOT, a.diag_dir)
    rows = [("按 SPEC 的口径（主考核）", a.main_a, a.main_b)]
    for mode, label in (("sfence", "读 T0 前先 `sfence`"), ("mfence", "读 T0 前先 `mfence`")):
        pa, pb = os.path.join(a.diag_dir, f"A-{mode}.json"), os.path.join(a.diag_dir, f"B-{mode}.json")
        if os.path.exists(os.path.join(ROOT, pa)) and os.path.exists(os.path.join(ROOT, pb)):
            rows.append((label, pa, pb))
    if len(rows) == 1:
        return "（未找到诊断运行结果）"
    out = ["进程内耗时 ① + ②（ns；格点分位数 / 平均值）：\n", "| 口径 | | 平均 | p50 | p90 | p99 | p99.9 | p99.99 | 段① ≥ 125 ns 占比 | 段① p99 | 段③ 平均 | ① + ② + ③ 平均 |",
           "|---|---|---|---|---|---|---|---|---|---|---|---|"]
    for label, pa, pb in rows:
        A, B = load(pa), load(pb)
        ma, mb = metric(A, "in-process"), metric(B, "in-process")
        tot = lambda r: sum(metric(r, n)["mean"] for n in ("seg①", "seg②", "seg③"))
        for lab, r, m in (("A", A, ma), ("B", B, mb)):
            out.append(f"| {label if lab == 'A' else ''} | {lab} | {m['mean']:.1f} | " + " | ".join(str(m[q]) for _, q in QN)
                       + f" | {r.get('seg1_slow_percent', float('nan')):.2f}% | {metric(r, 'seg①')['p99']} | {metric(r, 'seg③')['mean']:.1f} | {tot(r):.1f} |")
        out.append(f"| | **A − B** | **{ma['mean'] - mb['mean']:+.1f}** | " + " | ".join(f"**{ma[q] - mb[q]:+d}**" for _, q in QN)
                   + f" | | {metric(A, 'seg①')['p99'] - metric(B, 'seg①')['p99']:+d} | {metric(A, 'seg③')['mean'] - metric(B, 'seg③')['mean']:+.1f} | **{tot(A) - tot(B):+.1f}** |")
    cij = os.path.join(d, "ci-mfence.json")
    if os.path.exists(cij):
        with open(cij) as f:
            c = json.load(f)
        ia, ib = c["inputs"]["A"], c["inputs"]["B"]
        out += ["", f"`mfence` 口径下 A − B 的置信区间（同样的分块自助法；A {ia['samples']:,} 个样本，B {ib['samples']:,} 个）：\n"] + ci_table(c)
    return "\n".join(out)


def tax_parts(pa, pb, cip):
    """把每请求的 A − B 拆成三项：接收路径本身 / 批内排队 / 发送侧调度（见 README §1.1）。"""
    A, B, c = load(pa), load(pb), load(cip)
    mean = lambda r, n: metric(r, n)["mean"]
    bp = c["burst_position"]
    pos = ("pos1", "pos2", "pos3", "pos4+")
    path = bp["pos1"]["diff"]["mean"]["value"]
    queue = sum(bp[k]["share"]["A"] * (bp[k]["diff"]["mean"]["value"] - path) for k in pos[1:])
    incr = lambda side: [bp[pos[i + 1]][side]["mean"] - bp[pos[i]][side]["mean"] for i in range(3)]
    d = c["metrics"]
    return {
        "path": path, "queue": queue, "send": mean(A, "seg①") + mean(A, "seg③") - mean(B, "seg①") - mean(B, "seg③"),
        "seg1": mean(A, "seg①") - mean(B, "seg①"), "seg2": mean(A, "seg②") - mean(B, "seg②"), "seg3": mean(A, "seg③") - mean(B, "seg③"),
        "total": sum(mean(A, n) - mean(B, n) for n in ("seg①", "seg②", "seg③")),
        "incr_a": incr("A"), "incr_b": incr("B"),
        "p50": d["inproc"]["diff"]["p50"]["interp"], "p99": d["inproc"]["diff"]["p99"]["interp"],
        "seg2_p50": d["seg2"]["diff"]["p50"]["interp"], "seg2_p99": d["seg2"]["diff"]["p99"]["interp"],
        "a": {q: d["inproc"]["A"][q]["interp"] for q in ("p50", "p99")}, "b": {q: d["inproc"]["B"][q]["interp"] for q in ("p50", "p99")},
        "a_seg2": {q: d["seg2"]["A"][q]["interp"] for q in ("p50", "p99")}, "a_seg2_mean": mean(A, "seg②"),
    }


def session_row(meta, pa, pb, ci_path, ab_dir):
    """一个会话一行：主考核口径的 A − B（单次运行，带区间）+ 交替多对的逐对差值。"""
    A, B = load(pa), load(pb)
    c = load(ci_path)["metrics"]["inproc"]["diff"]
    tot = lambda r: sum(metric(r, n)["mean"] for n in ("seg①", "seg②", "seg③"))
    lost = A["counters"]["timeouts"] + B["counters"]["timeouts"]
    leak = sum(r["mbuf"]["avail_initial"] - r["mbuf"]["avail_final"] for r in (A, B))
    cells = [f"**{meta['name']}**", meta["boot_time"], meta["started"][:16],
             f"{metric(A, 'in-process')['p50_interp']:.1f} / {metric(B, 'in-process')['p50_interp']:.1f}",
             f"**{c['p50']['interp']:+.1f}** {ci_str(c['p50']['interp_ci'])}", f"**{c['p99']['interp']:+.1f}** {ci_str(c['p99']['interp_ci'])}",
             f"**{tot(A) - tot(B):+.1f}**"]
    runs = {}
    for p in sorted(glob.glob(os.path.join(ROOT, ab_dir, "[AB]-*.json"))):
        with open(p) as f:
            r = json.load(f)
        runs[(r["client"][0], int(os.path.basename(p).split("-")[1].split(".")[0]))] = r
    idx = sorted(i for c_, i in runs if c_ == "A" and ("B", i) in runs)
    if idx:
        d = lambda fn: [fn(runs[("A", i)]) - fn(runs[("B", i)]) for i in idx]
        for fn in (lambda r: metric(r, "in-process")["p50_interp"], lambda r: metric(r, "in-process")["p99_interp"], tot):
            m, lo, hi = mean_ci(d(fn))
            cells.append(f"**{m:+.1f}** [{lo:+.1f}, {hi:+.1f}]")
        lost += sum(r["counters"]["timeouts"] for r in runs.values())
        leak += sum(r["mbuf"]["avail_initial"] - r["mbuf"]["avail_final"] for r in runs.values())
        cells.append(f"{len(idx)} 对")
    else:
        cells += ["—", "—", "—", "—"]
    cells.append(f"{lost} / {leak}")
    return "| " + " | ".join(cells) + " |"


SESSION_ROOT = "logs/sessions"   # 重启后 / 另一天的复测会话（scripts/session.sh）


def all_sessions():
    """[(meta, 相对路径)]，按测量开始时间排序。"""
    out = []
    for d in glob.glob(os.path.join(ROOT, SESSION_ROOT, "*/")):
        rel = os.path.relpath(d, ROOT)
        if os.path.exists(os.path.join(d, "meta.json")) and os.path.exists(os.path.join(d, "ci.json")):
            out.append((load(rel + "/meta.json"), rel))
    return sorted(out, key=lambda x: x[0]["started"])


def session_entries(a):
    """正式数据 + 各复测会话：[(meta, A 主考核, B 主考核, ci, 交替对比目录)]，按测量开始时间排序。"""
    entries = []
    ab = sorted(glob.glob(os.path.join(ROOT, a.ab)))
    meta_p = os.path.join(os.path.dirname(a.main_a), "meta.json")
    if os.path.exists(os.path.join(ROOT, meta_p)) and os.path.exists(os.path.join(ROOT, a.ci)):
        entries.append((load(meta_p), a.main_a, a.main_b, a.ci, os.path.relpath(ab[-1], ROOT) if ab else "none"))
    for meta, rel in all_sessions():
        entries.append((meta, rel + "/A-600.json", rel + "/B-600.json", rel + "/ci.json", rel + "/ab"))
    return sorted(entries, key=lambda e: e[0]["started"])


def section_sessions(a):
    """当前版本在不同开机、不同日期的会话对比（正式数据也算一个会话）。"""
    entries = session_entries(a)
    if len(entries) < 2:
        return "（只有正式数据这一个会话；重启后运行 scripts/session.sh <名字>）"
    head = ["| 会话 | 代码版本 | 这次开机的时间（UTC） | 测量开始（UTC） | 进程内 p50：A / B | 主考核 A − B：p50 [95% 区间] | p99 [95% 区间] | 每请求总账 "
            "| 交替 10 对的逐对差值：p50 | p99 | 每请求总账 | 对数 | 丢包 / 泄漏（主考核 + 交替合计） |", "|---|---|---|---|---|---|---|---|---|---|---|---|---|"]
    rows = []
    for meta, pa, pb, cip, abd in entries:
        row = session_row(meta, pa, pb, cip, abd)
        rows.append(row.replace("** | ", f"** | {meta.get('version', '?')} | ", 1))
    import datetime
    boots = len({e[0]["boot_id"] for e in entries})
    bj = lambda t: datetime.datetime.strptime(t[:19], "%Y-%m-%d %H:%M:%S") + datetime.timedelta(hours=8)   # 北京时间 = UTC + 8
    days = sorted({bj(e[0]["started"]).strftime("%m-%d") for e in entries})
    when = "、".join(f"{e[0]['name']} {bj(e[0]['started']).strftime('%m-%d %H:%M')}" for e in entries)
    note = (f"\n共 {len(rows)} 个会话，分属 {boots} 次不同的开机（由内核的 boot_id 区分）。表里的时间是 UTC；"
            f"按北京时间，各会话的开始时刻是：{when}（{len(days)} 个自然日）。")
    parts = [(meta, tax_parts(pa, pb, cip), load(pa), load(pb)) for meta, pa, pb, cip, _ in entries]
    tax = ["\n各会话主考核那一对的拆分（每请求的 A − B，ns；三项的含义见 §5）与环境：\n",
           "| 会话 | ① 接收路径本身 | ② 批内排队 | ③ 发送侧调度 | 每请求总账 | 段① ≥ 125 ns 占比：A / B | 往返 p50：A / B（µs） | 往返 min：A / B（µs） | 网卡发送计数的偏移 |", "|---|---|---|---|---|---|---|---|---|"]
    for meta, t, A, B in parts:
        ea, eb = metric(A, "end-to-end"), metric(B, "end-to-end")
        off = A["port"]["opackets"] - A["counters"]["sent"] - A["counters"]["arp_replies"]
        tax.append(f"| {meta['name']} | {t['path']:+.1f} | {t['queue']:+.1f} | {t['send']:+.1f} | **{t['total']:+.1f}** "
                   f"| {A['seg1_slow_percent']:.2f}% / {B['seg1_slow_percent']:.2f}% | {ea['p50'] / 1000:.0f} / {eb['p50'] / 1000:.0f} | {ea['min'] / 1000:.1f} / {eb['min'] / 1000:.1f} | {off:,} |")
    seg = ["\n各会话交替 10 对的逐对差值按三段拆开（各段平均值的 A − B，ns，括号里是 95% 区间；最后两列是 A、B 各自段①平均值在 10 次运行里的中位数）：\n",
           "| 会话 | 段① | 段② | 段③ | A 的段①平均 | B 的段①平均 |", "|---|---|---|---|---|---|"]
    for meta, _, _, _, abd in entries:
        runs = {}
        for p in sorted(glob.glob(os.path.join(ROOT, abd, "[AB]-*.json"))):
            with open(p) as f:
                r = json.load(f)
            runs[(r["client"][0], int(os.path.basename(p).split("-")[1].split(".")[0]))] = r
        idx = sorted(i for c_, i in runs if c_ == "A" and ("B", i) in runs)
        if not idx:
            continue
        cells = [meta["name"]]
        for n in ("seg①", "seg②", "seg③"):
            m, lo, hi = mean_ci([metric(runs[("A", i)], n)["mean"] - metric(runs[("B", i)], n)["mean"] for i in idx])
            cells.append(f"**{m:+.1f}** [{lo:+.1f}, {hi:+.1f}]")
        for c_ in "AB":
            cells.append(f"{statistics.median(metric(runs[(c_, i)], 'seg①')['mean'] for i in idx):.1f}")
        seg.append("| " + " | ".join(cells) + " |")
    return "\n".join(head + rows) + note + "\n" + "\n".join(tax) + "\n" + "\n".join(seg)


def session_runs(rel):
    """一个会话里的所有运行（主考核、交替各轮、漂移监测的各次短测），按开始时间排序。"""
    paths = [os.path.join(ROOT, rel, f) for f in ("A-600.json", "B-600.json")]
    paths += glob.glob(os.path.join(ROOT, rel, "ab", "[AB]-*.json")) + glob.glob(os.path.join(ROOT, rel, "drift", "runs", "[AB]-*.json"))
    runs = []
    for p in paths:
        if os.path.exists(p):
            with open(p) as f:
                r = json.load(f)
            if not r.get("diag"):
                runs.append(r)
    return sorted(runs, key=lambda r: r["env"]["started_unix"])


def section_drift(a):
    """重启后的会话按"距开机多久"分窗，看 A 的尾部状态怎么随时间变。"""
    import datetime
    out = []
    for meta, rel in all_sessions():
        runs = session_runs(rel)
        if len(runs) < 30:
            continue
        boot = datetime.datetime.strptime(meta["boot_time"], "%Y-%m-%d %H:%M:%S").replace(tzinfo=datetime.timezone.utc).timestamp()
        tot = lambda r: sum(metric(r, n)["mean"] for n in ("seg①", "seg②", "seg③"))
        med = statistics.median
        out += [f"会话 `{meta['name']}`（代码版本 {meta.get('version', '?')}，开机时间 {meta['boot_time']} UTC）共 {len(runs)} 次运行，按开始时刻距开机多久分窗，每个窗口取各次运行的中位数（ns）：\n",
                "| 距开机 | 运行次数 A / B | A 段② p99 | B 段② p99 | A 段① ≥ 125 ns 占比 | A 段③ 平均 | 进程内 p50：A − B | 进程内 p99：A − B | 每请求总账：A − B | 丢包 / 泄漏 |",
                "|---|---|---|---|---|---|---|---|---|---|"]
        width = 600
        for w in range(0, int(max(r["env"]["started_unix"] for r in runs) - boot) // width + 1):
            sel = [r for r in runs if w * width <= r["env"]["started_unix"] - boot < (w + 1) * width]
            A = [r for r in sel if r["client"].startswith("A")]
            B = [r for r in sel if r["client"].startswith("B")]
            if not A or not B:
                if sel:
                    r = sel[0]
                    out.append(f"| {w * 10} ~ {w * 10 + 10} 分钟 | {len(A)} / {len(B)} | " + (f"{metric(r, 'seg②')['p99_interp']:.0f}" if A else "—") + " | "
                               + (f"{metric(r, 'seg②')['p99_interp']:.0f}" if B else "—") + f" | " + (f"{r['seg1_slow_percent']:.2f}%" if A else "—")
                               + " | " + (f"{metric(r, 'seg③')['mean']:.0f}" if A else "—") + f" | — | — | — | {r['counters']['timeouts']} / {r['mbuf']['avail_initial'] - r['mbuf']['avail_final']} |")
                continue
            m = lambda rs, name, key: med(metric(r, name)[key] for r in rs)
            lost = sum(r["counters"]["timeouts"] for r in sel)
            leak = sum(r["mbuf"]["avail_initial"] - r["mbuf"]["avail_final"] for r in sel)
            out.append(f"| {w * 10} ~ {w * 10 + 10} 分钟 | {len(A)} / {len(B)} | {m(A, 'seg②', 'p99_interp'):.0f} | {m(B, 'seg②', 'p99_interp'):.0f} "
                       f"| {med(r['seg1_slow_percent'] for r in A):.2f}% | {m(A, 'seg③', 'mean'):.0f} "
                       f"| {m(A, 'in-process', 'p50_interp') - m(B, 'in-process', 'p50_interp'):+.1f} "
                       f"| **{m(A, 'in-process', 'p99_interp') - m(B, 'in-process', 'p99_interp'):+.0f}** "
                       f"| **{med(tot(r) for r in A) - med(tot(r) for r in B):+.1f}** | {lost} / {leak} |")
        out.append("")
    return "\n".join(out).rstrip() if out else "（没有足够密的会话数据；运行 scripts/drift.sh）"


def section_tax(a):
    """每请求的 A − B 拆成三项（接收路径本身 / 批内排队 / 发送侧调度），主考核口径与 mfence 口径各一列。"""
    cols = [("主考核", a.main_a, a.main_b, a.ci),
            ("`mfence` 口径", os.path.join(a.diag_dir, "A-mfence.json"), os.path.join(a.diag_dir, "B-mfence.json"), os.path.join(a.diag_dir, "ci-mfence.json"))]
    cols = [c for c in cols if all(os.path.exists(os.path.join(ROOT, p)) for p in c[1:])]
    if not cols:
        return "（没有数据）"
    t = [tax_parts(pa, pb, ci) for _, pa, pb, ci in cols]
    tri = lambda v: " / ".join(f"{x:.0f}" for x in v)
    rows = [("① 接收路径本身（批内第 1 个包的段②差值）", lambda x: f"{x['path']:+.1f}"),
            ("② 批内排队（后面的包多等，按占比平摊）", lambda x: f"{x['queue']:+.1f}"),
            ("③ 发送侧调度（段① + 段③ 的差值）", lambda x: f"{x['send']:+.1f}"),
            ("**合计 = 每请求总账**", lambda x: f"**{x['total']:+.1f}**"),
            ("其中段①的差值", lambda x: f"{x['seg1']:+.1f}"),
            ("每往后一个位置，段②增加：A", lambda x: tri(x["incr_a"])),
            ("每往后一个位置，段②增加：B", lambda x: tri(x["incr_b"]))]
    out = ["每个请求的平均值，A − B，ns：\n", "| 来源 | " + " | ".join(c[0] for c in cols) + " |", "|---|" + "---|" * len(cols)]
    for label, fn in rows:
        out.append(f"| {label} | " + " | ".join(fn(x) for x in t) + " |")
    return "\n".join(out)


def section_vab(a):
    """上一个版本与当前版本的同场对比，A 和 B 都比（scripts/versions-ab.sh）。"""
    dirs = sorted(glob.glob(os.path.join(ROOT, "logs/versions-ab/*/")))
    if not dirs:
        return "（没有数据；运行 scripts/versions-ab.sh <旧版本标签>）"
    out = []
    for d in dirs:
        name = os.path.basename(d.rstrip("/"))
        old, cur = name.split("-vs-")
        runs = {}
        for p in glob.glob(os.path.join(d, "[AB]*-*.json")):
            tag, i = os.path.basename(p)[:-5].rsplit("-", 1)
            with open(p) as f:
                runs[(tag, int(i))] = json.load(f)
        idx = sorted(i for t, i in runs if t == "Aold" and all((x, i) in runs for x in ("Anew", "Bold", "Bnew")))
        if not idx:
            continue
        tot = lambda r: sum(metric(r, n)["mean"] for n in ("seg①", "seg②", "seg③"))
        stats = [("进程内 p50", lambda r: metric(r, "in-process")["p50_interp"]), ("进程内 p99", lambda r: metric(r, "in-process")["p99_interp"]),
                 ("进程内 平均", lambda r: metric(r, "in-process")["mean"]),
                 ("段① 平均", lambda r: metric(r, "seg①")["mean"]),
                 ("段② 平均", lambda r: metric(r, "seg②")["mean"]), ("段② p50", lambda r: metric(r, "seg②")["p50_interp"]),
                 ("段② p99", lambda r: metric(r, "seg②")["p99_interp"]),
                 ("段③ 平均", lambda r: metric(r, "seg③")["mean"]), ("① + ② + ③ 平均（总账）", tot)]
        f = lambda xs: "{:+.1f} [{:+.1f}, {:+.1f}]".format(*mean_ci(xs))
        g = lambda t, i: runs[(t, i)]
        allr = [runs[(t, i)] for t in ("Aold", "Anew", "Bold", "Bnew") for i in idx]
        lost = sum(r["counters"]["timeouts"] for r in allr)
        leak = sum(r["mbuf"]["avail_initial"] - r["mbuf"]["avail_final"] for r in allr)
        mism = sum(r["counters"]["tsc_mismatch"] for r in allr)
        out += [f"**{old} ↔ {cur}**：{len(idx)} 轮，每轮里 {old} 的 A、{cur} 的 A、{old} 的 B、{cur} 的 B 各跑 {allr[0]['duration_sec']} 秒（顺序逐轮轮换，都不带样本导出）；"
                f"共 {len(allr)} 次运行，丢包合计 {lost}，泄漏合计 {leak}，被拒绝的回复合计 {mism}。逐轮配对后的平均值与 95% 区间（ns）：\n",
                f"| 指标 | A：{cur} − {old} | B：{cur} − {old} | A − B（{old}） | A − B（{cur}） | **A − B 的变化** | A − B 变小的轮数 |", "|---|---|---|---|---|---|---|"]
        for label, fn in stats:
            da = [fn(g("Anew", i)) - fn(g("Aold", i)) for i in idx]
            db = [fn(g("Bnew", i)) - fn(g("Bold", i)) for i in idx]
            ab_old = [fn(g("Aold", i)) - fn(g("Bold", i)) for i in idx]
            ab_new = [fn(g("Anew", i)) - fn(g("Bnew", i)) for i in idx]
            dd = [n - o for n, o in zip(ab_new, ab_old)]
            out.append(f"| {label} | {f(da)} | {f(db)} | {f(ab_old)} | {f(ab_new)} | **{f(dd)}** | {sum(x < 0 for x in dd)} / {len(dd)} |")
        sl = lambda t: [g(t, i)["seg1_slow_percent"] for i in idx]
        out.append(f"\n段① ≥ 125 ns 的占比：{old} 的 A {min(sl('Aold')):.2f}% ~ {max(sl('Aold')):.2f}%，{cur} 的 A {min(sl('Anew')):.2f}% ~ {max(sl('Anew')):.2f}%；"
                   f"{old} 的 B {min(sl('Bold')):.2f}% ~ {max(sl('Bold')):.2f}%，{cur} 的 B {min(sl('Bnew')):.2f}% ~ {max(sl('Bnew')):.2f}%。\n")
    return "\n".join(out).rstrip()


def section_bisect(a):
    """v4 的第一版为什么让慢发送变多：五个版本同场轮流（logs/r1-bisect/）。"""
    d = os.path.join(ROOT, "logs/r1-bisect")
    labels = [("v3", "v3"), ("m1r2", "v3 + 最小的改动（接收时核对 + 停止检查），其余不动"), ("m2", "同上，再去掉 sleep 之后的那次核对"),
              ("v4", "v4 的第一版（去掉了那次核对）"), ("vb", "**最终的 v4**（保留那次核对）")]
    out = ["各 20 秒，五个版本轮流跑 3 轮；占比给出最小 ~ 最大，其余是 3 次的中位数（ns）：\n",
           "| 版本 | A：段① ≥ 125 ns 占比 | A：段① 平均 | A：段③ 平均 | A：进程内 p50 / p99 | B：段① ≥ 125 ns 占比 | B：段① 平均 | B：段③ 平均 | B：进程内 p50 / p99 |", "|---|---|---|---|---|---|---|---|---|"]
    med = statistics.median
    n = 0
    for key, label in labels:
        cells = [label]
        for c in "AB":
            rs = []
            for p in sorted(glob.glob(os.path.join(d, f"{c}-{key}-*.json"))):
                with open(p) as f:
                    rs.append(json.load(f))
            if not rs:
                cells += ["—"] * 4
                continue
            n += len(rs)
            sl = [r["seg1_slow_percent"] for r in rs]
            mm = lambda name, q: med(metric(r, name)[q] for r in rs)
            cells += [f"{min(sl):.2f}% ~ {max(sl):.2f}%", f"{mm('seg①', 'mean'):.1f}", f"{mm('seg③', 'mean'):.1f}",
                      f"{mm('in-process', 'p50_interp'):.1f} / {mm('in-process', 'p99_interp'):.1f}"]
        out.append("| " + " | ".join(cells) + " |")
    return "\n".join(out) + f"\n\n共 {n} 次运行。各版本的含义和重现方法见 `logs/r1-bisect/README.md`。" if n else "（没有数据）"


def gap_row(r, prefix):
    for m in r["metrics"]:
        if m["name"].strip().startswith("(诊断) seg① " + prefix):
            return m
    return None


def section_gap(a):
    """§4.1：按"距上一次发送多久"分档之后，A 和 B 的段①差在哪。"""
    A, B = load(a.main_a), load(a.main_b)
    out = []
    n = {k: metric(r, "seg①")["count"] for k, r in (("A", A), ("B", B))}
    for k, r in (("B", B), ("A", A)):
        g = gap_row(r, "距上次发送<100ns")
        far = gap_row(r, "500ns–2µs")
        share = 100 * g["count"] / n[k] if g else 0
        if g and g["count"] >= 100:
            out.append(f"- {k} 有 {share:.2f}% 的发送紧跟在上一次发送之后（间隔不到 100 ns）；这些发送的段① p50 是 {g['p50']} ns，"
                       f"而间隔足够时（500 ns ~ 2 µs 那一档）是 {far['p50']} ns。")
        else:
            out.append(f"- {k} 几乎没有这样的发送（间隔不到 100 ns 的占 {share:.3f}%）。")
    rows = []
    for prefix in ("250–500ns", "500ns–2µs", "≥2µs"):
        ga, gb = gap_row(A, prefix), gap_row(B, prefix)
        rows.append(f"{prefix.replace('–', ' ~ ')}：{ga['p50']} / {gb['p50']}（p99 {ga['p99']} / {gb['p99']}）")
    out.append("- 间隔在 250 ns 以上的各档，A / B 的段① p50 分别是 " + "；".join(rows) + " ns。")
    return "\n".join(out)


def section_placement(a):
    """§4.4：同一段停顿落在哪一段——A、B、以及加了 mfence 的 A、B 并排。"""
    rows = [("A", a.main_a), ("B", a.main_b), ("B，读 T0 前加 `mfence`", os.path.join(a.diag_dir, "B-mfence.json")),
            ("A，读 T0 前加 `mfence`", os.path.join(a.diag_dir, "A-mfence.json"))]
    out = ["| | 计分路径（① + ②）p50 | 计分路径 p99 | 段① ≥ 125 ns 占比 | 段③ p99（不计分） | ① + ② + ③ 平均 |", "|---|---|---|---|---|---|"]
    for label, p in rows:
        if not os.path.exists(os.path.join(ROOT, p)):
            continue
        r = load(p)
        ip = metric(r, "in-process")
        out.append(f"| {label} | {ip['p50']} | {ip['p99']} | {r['seg1_slow_percent']:.2f}% | {metric(r, 'seg③')['p99']} "
                   f"| {sum(metric(r, n)['mean'] for n in ('seg①', 'seg②', 'seg③')):.1f} |")
    return "\n".join(out)


def section_summary(a):
    """不进报告：把各文档要引用的关键数字集中打印出来（--print summary），写文档时从这里抄。"""
    A, B, c = load(a.main_a), load(a.main_b), load(a.ci)
    out = []
    for k, r in (("A", A), ("B", B)):
        ip, s1, s2, s3 = (metric(r, n) for n in ("in-process", "seg①", "seg②", "seg③"))
        out.append(f"主考核 {k}：样本 {ip['count']:,}，sent {r['counters']['sent']:,}，丢 {r['counters']['timeouts']}，泄漏 {r['mbuf']['avail_initial'] - r['mbuf']['avail_final']}，"
                   f"进程内 p50/p90/p99/p99.9/p99.99 = {ip['p50']}/{ip['p90']}/{ip['p99']}/{ip['p99_9']}/{ip['p99_99']}，平均 {ip['mean']:.1f}；"
                   f"段① p50/p99 {s1['p50']}/{s1['p99']} 平均 {s1['mean']:.1f}；段② p50/p99 {s2['p50']}/{s2['p99']} 平均 {s2['mean']:.1f}；"
                   f"段③ p50/p99 {s3['p50']}/{s3['p99']} 平均 {s3['mean']:.1f}；慢发送 {r['seg1_slow_percent']:.2f}%；"
                   f"sleep 误差 p50/p99 {metric(r, 'sleep error')['p50']}/{metric(r, 'sleep error')['p99']}；"
                   f"端到端 p50/p99 {metric(r, 'end-to-end')['p50'] / 1000:.1f}/{metric(r, 'end-to-end')['p99'] / 1000:.1f} µs；速率 {r['counters']['sent'] / r['elapsed_sec']:,.0f}/s；"
                   f"停顿 {r['stalls']['count'] / r['elapsed_sec']:.0f} 次/秒 最长 {r['stalls']['max_ns'] / 1000:.0f} µs；commit {r['env']['git_commit']}")
    m = c["metrics"]
    for key, label in (("inproc", "进程内"), ("seg1", "段①"), ("seg2", "段②")):
        d = m[key]["diff"]
        out.append(f"{label} A − B（插值）：" + "，".join(f"{q} {d[q]['interp']:+.1f} {ci_str(d[q]['interp_ci'])}" for q in ("p50", "p90", "p99", "p99_9") if q in d)
                   + f"；平均 {d['mean']['value']:+.1f}")
    t = tax_parts(a.main_a, a.main_b, a.ci)
    out.append(f"税单（主考核）：接收路径本身 {t['path']:+.1f}，批内排队 {t['queue']:+.1f}，发送侧调度 {t['send']:+.1f}，合计 {t['total']:+.1f}；"
               f"每往后一个位置 A {t['incr_a']} B {t['incr_b']}")
    cm = os.path.join(a.diag_dir, "ci-mfence.json")
    if os.path.exists(os.path.join(ROOT, cm)):
        tm = tax_parts(os.path.join(a.diag_dir, "A-mfence.json"), os.path.join(a.diag_dir, "B-mfence.json"), cm)
        dm = load(cm)["metrics"]["inproc"]["diff"]
        out.append(f"mfence 口径 A − B：平均 {dm['mean']['value']:+.1f}，p50 {dm['p50']['interp']:+.1f}，p99 {dm['p99']['interp']:+.1f}；"
                   f"段①差 {tm['seg1']:+.1f}；税单 {tm['path']:+.1f} / {tm['queue']:+.1f} / {tm['send']:+.1f}，合计 {tm['total']:+.1f}")
    return "\n".join(out)


def section_stores(a):
    """剂量实验：T0 之前多做 N 次普通写入。"""
    d = os.path.join(ROOT, a.diag_dir)
    runs = []
    for p in glob.glob(os.path.join(d, "B-stores-*.json")):
        with open(p) as f:
            runs.append((int(os.path.basename(p).split("-")[2].split(".")[0]), "B", json.load(f)))
    if not runs:
        return "（未找到剂量实验结果）"
    runs.sort(key=lambda x: x[0])
    runs.insert(0, (0, "B", load(a.main_b)))
    for p in sorted(glob.glob(os.path.join(d, "A-stores-*.json"))):
        with open(p) as f:
            runs.append((int(os.path.basename(p).split("-")[2].split(".")[0]), "A", json.load(f)))
    runs.append((0, "A", load(a.main_a)))
    out = ["| 客户端 | 读 T0 之前多做的写入次数 | 距上次发送 < 250 ns 的发送占比 | 这些发送的段①平均 | 段① p99 | 段① ≥ 125 ns 占比 | 段③ 平均 | 进程内 p99 |",
           "|---|---|---|---|---|---|---|---|"]
    for n, side, r in runs:
        buckets = [m for m in r["metrics"] if "seg①" in m["name"] and "诊断" in m["name"]]
        tot = metric(r, "seg①")["count"]
        close = buckets[0]["count"] + buckets[1]["count"]
        close_mean = (buckets[0]["mean"] * buckets[0]["count"] + buckets[1]["mean"] * buckets[1]["count"]) / close if close else float("nan")
        label = "0（主考核）" if n == 0 else str(n)
        out.append(f"| {side} | {label} | {100 * close / tot:.2f}% | {'—' if close < 100 else f'{close_mean:.0f} ns'} | {metric(r, 'seg①')['p99']} "
                   f"| {r.get('seg1_slow_percent', float('nan')):.2f}% | {metric(r, 'seg③')['mean']:.0f} | {metric(r, 'in-process')['p99']} |")
    return "\n".join(out)


def section_fault(a):
    dirs = sorted(glob.glob(os.path.join(ROOT, "logs/fault/*/summary.md")))
    if not dirs:
        return "（未找到故障注入结果；运行 scripts/fault.py）"
    with open(dirs[-1]) as f:
        body = f.read().strip()
    return f"来源：`{os.path.relpath(os.path.dirname(dirs[-1]), ROOT)}/`（每个场景的完整日志与 JSON 都在里面）\n\n" + body


def section_soak(a):
    out = ["| 客户端 | 实际时长 | sent | received | 超时（丢包） | 迟到 | 对账差 | 收包对账差 | mbuf 泄漏 | 网卡发送计数 − sent − ARP 应答 | 网卡接收计数 − 程序收到的包 "
           "| 网卡丢弃 / AWS 限额计数 | 进程内 p50 / p99 / p99.99（ns） | 最长一次被打断 |",
           "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"]
    found = False
    for lab, p in (("A", a.soak_a), ("B", a.soak_b)):
        try:
            r = load(p)
        except FileNotFoundError:
            continue
        found = True
        c, m, ip, po = r["counters"], r["mbuf"], metric(r, "in-process"), r["port"]
        # 收到的每个包恰好落入一类；tsc_mismatch（被拒绝的回复）是其中一类，与程序、fault.py、check-compliance.sh 的公式一致
        rx = c["rx_pkts"] - (c["received"] + c["late"] + c["unexpected"] + c.get("foreign", 0) + c.get("tsc_mismatch", 0) + c["other_rx"] + c["arp_replies"])
        drops = po["imissed"] + po["ierrors"] + po["oerrors"] + po["rx_nombuf"] + sum(v for _, v in po["allowance_exceeded"])
        out.append(f"| {lab} | {r['elapsed_sec']:.1f} s | {c['sent']:,} | {c['received']:,} | {c['timeouts']} | {c['late']} "
                   f"| {c['sent'] - c['received'] - c['timeouts'] - c['in_flight_at_end']} | {rx} | {m['avail_initial'] - m['avail_final']} "
                   f"| {po['opackets'] - c['sent'] - c['arp_replies']} | {po['ipackets'] - c['rx_pkts']} | {drops} "
                   f"| {ip['p50']} / {ip['p99']} / {ip['p99_99']} | {max(r['stalls']['max_ns'], r['stalls']['rx_max_ns']) / 1e3:.0f} µs |")
    return "\n".join(out) if found else "（未找到长时间运行结果）"


def section_env(a):
    A = load(a.main_a)
    e = A.get("env")
    if not e:
        return "（主考核的 JSON 里没有环境信息）"
    B = load(a.main_b).get("env", {})
    rows = [
        ("代码版本（git 提交）", f"`{e['git_commit']}`" + ("（构建时源码有未提交改动）" if e["git_dirty"] else "（构建时源码树干净）")
         + ("" if B.get("git_commit") == e["git_commit"] else f"；B：`{B.get('git_commit')}`")),
        ("编译器 / 构建", f"{e['rustc']}，{e['profile']}" + (f"，features: {', '.join(e['features'])}" if e["features"] else "")),
        ("DPDK", e["dpdk"]), ("CPU", e["cpu_model"]), ("内核", e["kernel"]), ("内核启动参数", f"`{e['kernel_cmdline']}`"),
        ("网卡", f"{e['nic_pci']}，驱动 {e['nic_driver']}，写合并映射：{'是' if e['nic_write_combining'] else '未检测到'}"),
        ("时钟", f"clocksource {e['clocksource']}；CPU 标志 {' '.join(e['tsc_flags'])}；TSC {A['tsc_hz'] / 1e9:.3f} GHz"),
        ("时间戳分辨率", f"TSC 读数每 {e['tsc_step_cycles']} 个周期跳一步 = **{e['tsc_step_ns']:.1f} ns**（所有时间差都是它的整数倍）"),
        ("读一次时钟的成本", f"平均 {e['clock_read_mean_ns']:.1f} ns（A）/ {B.get('clock_read_mean_ns', float('nan')):.1f} ns（B）；每个被测段恰好包含一次"),
        ("周期 → 纳秒换算的准确度", f"与内核 CLOCK_MONOTONIC_RAW 相比，10 分钟内相差 {e['tsc_vs_monotonic_ppm']:+.1f} ppm（A）/ {B.get('tsc_vs_monotonic_ppm', float('nan')):+.1f} ppm（B）"),
        ("命令行", f"`{' '.join(e['argv'][1:])}`"),
    ]
    return "\n".join(["| 项目 | 值 |", "|---|---|"] + [f"| {k} | {v} |" for k, v in rows])


def sections_table():
    return (("main", section_main), ("totals", section_totals), ("ab", section_ab), ("vab", section_vab), ("bisect", section_bisect), ("sessions", section_sessions), ("drift", section_drift), ("gap", section_gap), ("placement", section_placement), ("summary", section_summary), ("c", section_c), ("probe", section_probe),
            ("ci", section_ci), ("burst", section_burst), ("tax", section_tax), ("diag", section_diag), ("stores", section_stores),
            ("fault", section_fault), ("soak", section_soak), ("env", section_env))


def main():
    ap = argparse.ArgumentParser()
    for k, v in DEFAULTS.items():
        ap.add_argument("--" + k.replace("_", "-"), default=v)
    ap.add_argument("--ab", default="logs/ab-*", help="ABBA 结果目录（glob）；默认全部")
    ap.add_argument("--out", default="docs/REPORT.md")
    ap.add_argument("--only", default="", help="只更新这些小节（逗号分隔）")
    ap.add_argument("--print", default="", help="不写文件，只把这个小节打印出来")
    a = ap.parse_args()
    if a.print:
        print(dict(sections_table())[a.print](a))
        return
    path = os.path.join(ROOT, a.out)
    with open(path) as f:
        doc = f.read()
    sections = sections_table()
    for name, fn in sections:
        pat = re.compile(rf"(<!-- BEGIN:{name} -->).*?(<!-- END:{name} -->)", re.S)
        if not pat.search(doc):
            if name != "summary":   # summary 只用于 --print，不进报告
                print(f"警告：{a.out} 里没有 {name} 标记", file=sys.stderr)
            continue
        if a.only and name not in a.only.split(","):
            continue
        body = fn(a)
        doc = pat.sub(lambda m: m.group(1) + "\n" + body + "\n" + m.group(2), doc)
    with open(path, "w") as f:
        f.write(doc)
    print(f"已更新 {a.out}")


if __name__ == "__main__":
    main()
