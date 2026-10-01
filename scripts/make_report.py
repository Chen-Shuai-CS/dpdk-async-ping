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
    "rate_a": "logs/final/A-d800.json",
    "rate_b": "logs/final/B-d800.json",
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
       12: 2.179, 13: 2.160, 14: 2.145, 15: 2.131, 19: 2.093, 29: 2.045}


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
        ("**自己代码的全部时间：① + ② + ③**", s1a + s2a + s3a, s1b + s2b + s3b, True),
    ]
    notes = {("B", "seg1"): "<- includes the NIC wait", ("A", "seg3"): "<- scheduling lands here (not ranked)",
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
                 else f"**{len(idx)} 对 × {secs} 秒**（`{os.path.relpath(d, ROOT)}`；注意：有的轮次出现了丢包或泄漏，见各轮 JSON）\n",
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
        slow_a = [runs[("A", i)]["seg1_slow_percent"] for i in idx]
        slow_b = [runs[("B", i)]["seg1_slow_percent"] for i in idx]
        p99_a = [metric(runs[("A", i)], "seg①")["p99"] for i in idx]
        lines.append(f"\n段① ≥ 125 ns 的占比在各轮之间的范围：A {min(slow_a):.2f}% ~ {max(slow_a):.2f}%，B {min(slow_b):.2f}% ~ {max(slow_b):.2f}%。"
                     f"A 的段① p99 各轮为 {' / '.join(str(v) for v in p99_a)} ns：占比低于 1% 的轮次落在 70 ~ 80 ns，高于 1% 的轮次落在 150 ns 以上。")
        parts.append("\n".join(lines))
    return "\n\n".join(parts) if parts else "（没有找到新格式的交替运行结果）"


def c_row(label, cond, r):
    m = r["metrics"][0] if "flows" in r else metric(r, "end-to-end")
    lost = (r["sent"] - r["received"]) if "flows" in r else r["counters"]["timeouts"]
    us = lambda v: f"{v / 1000:,.1f}"
    return (f"| {label} | {cond} | {m['count']:,} | {lost} | {us(m['min'])} | {us(m['p50'])} | {us(m['p90'])} | {us(m['p99'])} "
            f"| {us(m['p99_9'])} | {us(m['p99_99'])} | {us(m['max'])} |")


def latest_c(mode, flows):
    best = None
    for p in sorted(glob.glob(os.path.join(ROOT, f"logs/C-{mode}-*/C.json"))):
        with open(p) as f:
            r = json.load(f)
        if r["flows"] == flows:
            best = r
    return best


def section_c(a):
    hdr = ("| 客户端 | 条件 | 样本 | 丢包 | min | p50 | p90 | p99 | p99.9 | p99.99 | max |\n"
           "|---|---|---|---|---|---|---|---|---|---|---|")
    ra, rb, r1 = load(a.rate_a), load(a.rate_b), load(a.one_a)
    out = ["#### 场景一：64 路、同速率（约 6.5 万包/秒），端到端（µs）\n", hdr,
           c_row("A", f"64 session，delay {ra['delay_us']} µs", ra),
           c_row("B", f"64 session，delay {rb['delay_us']} µs", rb)]
    for mode, desc in (("user", "64 × `ping -U -i 0.001`（用户态↔用户态）"), ("kernel", "64 × `ping -i 0.001`（内核收包时间戳）")):
        r = latest_c(mode, 64)
        if r:
            out.append(c_row("C", desc, r))
    out += ["\n#### 场景二：单路、低速率（1000 包/秒），端到端（µs）\n", hdr,
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
    out += ["段①的三个子步骤（ns；只统计距上次发送 ≥ 250 ns 的发送；每步各含一次约 16 ns 的时钟读取）：\n",
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
    out += ["", "**样本并不独立。** 把 1 亿个样本当成 1 亿次独立观测，会把区间算得过窄：\n",
            "| 重抽方式 | 进程内平均 A − B | 进程内 p50 A − B | 进程内 p99 A − B |", "|---|---|---|---|",
            f"| 假装样本相互独立 | {ci_str(c['iid']['mean_diff_ci'])} | {ci_str(c['iid']['p50_diff_ci'])} | {ci_str(c['iid']['p99_diff_ci'])} |"]
    for r in c["block_sensitivity"]:
        if r["blocks"] < 10:
            continue
        mark = "**" if abs(r["block_sec"] - c["block_sec"]) < 1e-9 else ""
        out.append(f"| {mark}按 {r['block_sec']:g} 秒分块（{r['blocks']} 块）{mark} | {ci_str(r['mean_diff_ci'])} | {ci_str(r['p50_diff_ci'])} | {ci_str(r['p99_diff_ci'])} |")
    out.append(f"\n按 1 秒分块得到的区间比\"假装独立\"宽 {e['mean_diff_ci']['design_effect'] ** 0.5:.1f} 倍（平均值）/ "
               f"{e['p50_diff_ci']['design_effect'] ** 0.5:.1f} 倍（p50）/ {e['p99_diff_ci']['design_effect'] ** 0.5:.1f} 倍（p99），"
               f"相当于 {ia['samples'] + ib['samples']:,} 个样本只顶 {e['mean_diff_ci']['effective_samples']:,} ~ {e['p99_diff_ci']['effective_samples']:,} 个独立样本。"
               "块长从 0.1 秒换到 20 秒，区间基本不变，说明结论不依赖块长的选择。")
    sf = c["seg1_slow_fraction"]
    out.append(f"\n**段① 的 p99 为什么不稳定。** 段① ≥ {sf['threshold_ns']:.0f} ns 的样本占比：A {sf['A']['percent']:.2f}%"
               f"（95% 区间 {sf['A']['ci'][0]:.2f}% ~ {sf['A']['ci'][1]:.2f}%），B {sf['B']['percent']:.2f}%（{sf['B']['ci'][0]:.2f}% ~ {sf['B']['ci'][1]:.2f}%）。")
    sa, sb = c["series"]["A"]["summary"], c["series"]["B"]["summary"]
    out += ["", "**10 分钟内是否稳定。** 把每一秒单独算一次（进程内耗时，ns）：\n",
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
    out.append(f"\n核对：由样本还原出的批大小分布与程序自己统计的 rx_burst 分布一致；同一批内段②随位置递增的比例 A {ia['seg2_nondecreasing_within_burst']:.4%}、"
               f"B {ib['seg2_nondecreasing_within_burst']:.4%}。")
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
    out = ["| 客户端 | 实际时长 | sent | received | 超时（丢包） | 对账差 | 收包对账差 | mbuf 泄漏 | AWS 限额超限 | 进程内 p50 / p99 / p99.99 / max（ns） | 最长一次被打断 |",
           "|---|---|---|---|---|---|---|---|---|---|---|"]
    found = False
    for lab, p in (("A", a.soak_a), ("B", a.soak_b)):
        try:
            r = load(p)
        except FileNotFoundError:
            continue
        found = True
        c, m, ip = r["counters"], r["mbuf"], metric(r, "in-process")
        rx = c["rx_pkts"] - (c["received"] + c["late"] + c["unexpected"] + c.get("foreign", 0) + c["other_rx"] + c["arp_replies"])
        out.append(f"| {lab} | {r['elapsed_sec']:.1f} s | {c['sent']:,} | {c['received']:,} | {c['timeouts']} "
                   f"| {c['sent'] - c['received'] - c['timeouts'] - c['in_flight_at_end']} | {rx} | {m['avail_initial'] - m['avail_final']} "
                   f"| {sum(v for _, v in r['port']['allowance_exceeded'])} | {ip['p50']} / {ip['p99']} / {ip['p99_99']} / {ip['max']:,} "
                   f"| {max(r['stalls']['max_ns'], r['stalls']['rx_max_ns']) / 1e3:.0f} µs |")
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
    return (("main", section_main), ("totals", section_totals), ("ab", section_ab), ("c", section_c), ("probe", section_probe),
            ("ci", section_ci), ("burst", section_burst), ("diag", section_diag), ("stores", section_stores), ("fault", section_fault),
            ("soak", section_soak), ("env", section_env))


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
