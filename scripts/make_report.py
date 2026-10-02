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
        ("**自己代码的全部时间：① + ② + ③**", s1a + s2a + s3a, s1b + s2b + s3b, True),
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


SESSION_ROOTS = ("logs/v1/sessions", "logs/sessions")   # 各版本的复测会话


def all_sessions():
    """[(meta, 相对路径)]，按测量开始时间排序。"""
    out = []
    for root in SESSION_ROOTS:
        for d in glob.glob(os.path.join(ROOT, root, "*/")):
            rel = os.path.relpath(d, ROOT)
            if os.path.exists(os.path.join(d, "meta.json")):
                out.append((load(rel + "/meta.json"), rel))
    return sorted(out, key=lambda x: x[0]["started"])


def section_sessions(a):
    """不同代码版本、不同开机、不同日期的会话对比。"""
    entries = session_entries(a)
    if len(entries) < 2:
        return "（只有一个会话；重启后运行 scripts/session.sh <名字>）"
    head = ["| 会话 | 代码版本 | 这次开机的时间 | 测量开始（UTC） | 进程内 p50：A / B | 主考核 A − B：p50 [95% 区间] | p99 [95% 区间] | 每请求总账 "
            "| 交替多对的逐对差值：p50 | p99 | 每请求总账 | 对数 | 丢包 / 泄漏（全部轮次合计） |", "|---|---|---|---|---|---|---|---|---|---|---|---|---|"]
    rows = []
    for meta, pa, pb, cip, abd in entries:
        row = session_row(meta, pa, pb, cip, abd)
        rows.append(row.replace("** | ", f"** | {meta.get('version', '?')} | ", 1))
    boots = len({e[0]["boot_id"] for e in entries})
    vers = sorted({e[0].get("version", "?") for e in entries})
    note = (f"\n共 {len(rows)} 个会话，分属 {boots} 次不同的开机（由内核的 boot_id 区分），代码版本 {' / '.join(vers)}"
            "（v2 = v1 + `Sleep` 第一次被 poll 时不再读时钟，§5.1；v3 = v2 + 导出样本时预取下一条缓存行，只在开 `--samples` 时执行，§3.5）。")
    # 每个会话的主考核那一对，把每请求的 A − B 拆成三项；再按版本看这些会话之间差多少
    parts = [(meta, tax_parts(pa, pb, cip)) for meta, pa, pb, cip, _ in entries]
    tax = ["\n各会话主考核那一对的抽象税拆分（每请求的 A − B，ns；三项的含义见 §5）：\n",
           "| 会话 | 代码版本 | ① 接收路径本身 | ② 批内排队 | ③ 发送侧调度 | 每请求总账 | 进程内 p50 的 A − B | 进程内 p99 的 A − B |", "|---|---|---|---|---|---|---|---|"]
    for meta, t in parts:
        tax.append(f"| {meta['name']} | {meta.get('version', '?')} | {t['path']:+.1f} | {t['queue']:+.1f} | {t['send']:+.1f} | **{t['total']:+.1f}** "
                   f"| {t['p50']:+.1f} | {t['p99']:+.1f} |")
    spread = ["\n同一个版本的各个会话之间差多少（最小 ~ 最大；括号里是平均值）：\n", "| 代码版本 | 会话数 | ① 接收路径本身 | ② 批内排队 | ③ 发送侧调度 | 每请求总账 | 进程内 p50 的 A − B |", "|---|---|---|---|---|---|---|"]
    for v in vers:
        ts = [t for meta, t in parts if meta.get("version", "?") == v]
        rng = lambda k: (f"{min(t[k] for t in ts):+.1f} ~ {max(t[k] for t in ts):+.1f}（{statistics.mean(t[k] for t in ts):+.1f}）" if len(ts) > 1 else f"{ts[0][k]:+.1f}")
        spread.append(f"| {v} | {len(ts)} | {rng('path')} | {rng('queue')} | {rng('send')} | {rng('total')} | {rng('p50')} |")
    return "\n".join(head + rows) + note + "\n" + "\n".join(tax) + "\n" + "\n".join(spread)


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
        # 只列出现过"尾部变重"（A 的段② p99 超过 400 ns）的会话；其余会话的时间轴在 `day` 一节和会话表里
        if not any(r["client"].startswith("A") and metric(r, "seg②")["p99_interp"] > 400 for r in runs):
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


def raw_runs(raw):
    """<名字>-raw 里的全部运行：[(标签 A1/A2/B, 阶段 main/rot/drift, 报告)]，按开始时间排序。A1 = 旧版本的 A，A2 = 当前版本的 A。"""
    name = os.path.basename(raw.rstrip("/"))[: -len("-raw")]
    out = []
    for phase in ("rot", "drift"):
        for p in glob.glob(os.path.join(raw, phase, "*.json")):
            with open(p) as f:
                out.append((os.path.basename(p).split("-")[0], phase, json.load(f)))
    for d in glob.glob(os.path.join(os.path.dirname(raw.rstrip("/")), name + "-v*")):
        ver = load(os.path.relpath(d, ROOT) + "/meta.json").get("version", "?")
        tag = "A1" if ver == "v1" else "A2"
        for side in ("A", "B"):
            p = os.path.join(d, side + "-600.json")
            if os.path.exists(p):
                with open(p) as f:
                    out.append((tag if side == "A" else "B", "main", json.load(f)))
    return sorted(out, key=lambda x: x[2]["env"]["started_unix"])


def section_day(a):
    """同一次开机里两个版本的 A 和 B 轮流跑（scripts/day.sh）。"""
    import datetime
    out = []
    tot = lambda r: sum(metric(r, n)["mean"] for n in ("seg①", "seg②", "seg③"))
    for raw in sorted(glob.glob(os.path.join(ROOT, "logs/sessions/*-raw/"))):
        runs = raw_runs(raw)
        if len(runs) < 20:
            continue
        name = os.path.basename(raw.rstrip("/"))[: -len("-raw")]
        metas = [load(os.path.relpath(d, ROOT) + "/meta.json") for d in sorted(glob.glob(os.path.join(os.path.dirname(raw.rstrip("/")), name + "-v*")))]
        boot_time = metas[0]["boot_time"]
        boot = datetime.datetime.strptime(boot_time, "%Y-%m-%d %H:%M:%S").replace(tzinfo=datetime.timezone.utc).timestamp()
        lost = sum(r["counters"]["timeouts"] for _, _, r in runs)
        leak = sum(r["mbuf"]["avail_initial"] - r["mbuf"]["avail_final"] for _, _, r in runs)
        out.append(f"**会话 `{name}`**（开机时间 {boot_time} UTC；共 {len(runs)} 次运行，丢包合计 {lost}，泄漏合计 {leak}）。"
                   "A1 = v1 的 A，A2 = v2 的 A；B 的代码在两个版本里相同。\n")
        # (1) 轮流的那些轮：逐轮配对
        rot = {}
        for tag, phase, r in runs:
            if phase == "rot":
                i = int(r["env"]["argv"][r["env"]["argv"].index("--json") + 1].rsplit("-", 1)[1].split(".")[0])
                rot.setdefault(i, {})[tag] = r
        idx = sorted(i for i, v in rot.items() if len(v) == 3)
        if idx:
            stats = [("进程内 p50", lambda r: metric(r, "in-process")["p50_interp"]), ("进程内 p99", lambda r: metric(r, "in-process")["p99_interp"]),
                     ("段② 平均", lambda r: metric(r, "seg②")["mean"]), ("段② p99", lambda r: metric(r, "seg②")["p99_interp"]),
                     ("① + ③ 平均（发送侧）", lambda r: metric(r, "seg①")["mean"] + metric(r, "seg③")["mean"]), ("① + ② + ③ 平均（总账）", tot)]
            out += [f"三者轮流各 60 秒，共 {len(idx)} 轮（开机后 {(min(rot[i]['B']['env']['started_unix'] for i in idx) - boot) / 60:.0f} ~ "
                    f"{(max(rot[i]['B']['env']['started_unix'] for i in idx) - boot) / 60:.0f} 分钟）。逐轮配对后的平均值与 95% 区间（ns）：\n",
                    "| 指标 | v1：A1 − B | v2：A2 − B | **v2 − v1（A2 − A1）** | v2 更低的轮数 |", "|---|---|---|---|---|"]
            for label, fn in stats:
                d1 = [fn(rot[i]["A1"]) - fn(rot[i]["B"]) for i in idx]
                d2 = [fn(rot[i]["A2"]) - fn(rot[i]["B"]) for i in idx]
                dd = [fn(rot[i]["A2"]) - fn(rot[i]["A1"]) for i in idx]
                f = lambda xs: "{:+.1f} [{:+.1f}, {:+.1f}]".format(*mean_ci(xs))
                out.append(f"| {label} | {f(d1)} | {f(d2)} | **{f(dd)}** | {sum(x < 0 for x in dd)} / {len(dd)} |")
            sl = lambda tag: [rot[i][tag]["seg1_slow_percent"] for i in idx]
            out.append(f"\n段① ≥ 125 ns 的占比：A1 {min(sl('A1')):.2f}% ~ {max(sl('A1')):.2f}%，A2 {min(sl('A2')):.2f}% ~ {max(sl('A2')):.2f}%，"
                       f"B {min(sl('B')):.2f}% ~ {max(sl('B')):.2f}%。\n")
        # (2) 按距开机多久分窗
        out += ["全部运行按开始时刻距开机多久分窗，每个窗口取各次运行的中位数（ns）：\n",
                "| 距开机 | 运行次数 A1 / A2 / B | 段② p99：A1 / A2 / B | 段① ≥ 125 ns 占比：A1 / A2 | 进程内 p99 的 A − B：v1 / v2 | 每请求总账的 A − B：v1 / v2 |",
                "|---|---|---|---|---|---|"]
        med = statistics.median
        width = 600
        last = int(max(r["env"]["started_unix"] for _, _, r in runs) - boot) // width
        for w in range(last + 1):
            sel = {t: [r for tag, _, r in runs if tag == t and w * width <= r["env"]["started_unix"] - boot < (w + 1) * width] for t in ("A1", "A2", "B")}
            if not any(sel.values()):
                continue
            m = lambda t, fn: med(fn(r) for r in sel[t]) if sel[t] else None
            cell = lambda v, fmt="{:.0f}": "—" if v is None else fmt.format(v)
            s2 = lambda r: metric(r, "seg②")["p99_interp"]
            ip = lambda r: metric(r, "in-process")["p99_interp"]
            diff = lambda t, fn: None if (m(t, fn) is None or m("B", fn) is None) else m(t, fn) - m("B", fn)
            out.append(f"| {w * 10} ~ {w * 10 + 10} 分钟 | {len(sel['A1'])} / {len(sel['A2'])} / {len(sel['B'])} "
                       f"| {cell(m('A1', s2))} / {cell(m('A2', s2))} / {cell(m('B', s2))} "
                       f"| {cell(m('A1', lambda r: r['seg1_slow_percent']), '{:.2f}%')} / {cell(m('A2', lambda r: r['seg1_slow_percent']), '{:.2f}%')} "
                       f"| {cell(diff('A1', ip), '{:+.0f}')} / {cell(diff('A2', ip), '{:+.0f}')} "
                       f"| {cell(diff('A1', tot), '{:+.1f}')} / {cell(diff('A2', tot), '{:+.1f}')} |")
        # (3) 自动补测的 mfence 口径
        mf = sorted(glob.glob(os.path.join(raw, "mfence", "A*.json")))
        heavy = {t: sum(1 for tag, ph, r in runs if tag == t and ph != "main" and metric(r, "seg②")["p99_interp"] > 400) for t in ("A1", "A2")}
        out.append(f"\n\"尾部变重\"（段② p99 超过 400 ns）的运行：A1 {heavy['A1']} 次，A2 {heavy['A2']} 次；因此自动补测的 `mfence` 口径共 {len(mf)} 组。")
        if mf:
            rows = []
            for p in mf:
                pb = os.path.join(os.path.dirname(p), "B-" + os.path.basename(p).split("-", 1)[1])
                if not os.path.exists(pb):
                    continue
                with open(p) as f:
                    ra = json.load(f)
                with open(pb) as f:
                    rb = json.load(f)
                rows.append((os.path.basename(p).split("-")[0], metric(ra, "in-process")["p50_interp"] - metric(rb, "in-process")["p50_interp"],
                             metric(ra, "in-process")["p99_interp"] - metric(rb, "in-process")["p99_interp"], metric(ra, "seg②")["p99_interp"], tot(ra) - tot(rb)))
            for t in ("A1", "A2"):
                x = [r for r in rows if r[0] == t]
                if x:
                    out.append(f"- {t}（{'v1' if t == 'A1' else 'v2'}）在重状态下的 `mfence` 口径，{len(x)} 组的中位数：进程内 p50 的 A − B {med(r[1] for r in x):+.1f}，"
                               f"p99 的 A − B {med(r[2] for r in x):+.1f}，A 的段② p99 {med(r[3] for r in x):.0f}，每请求总账的 A − B {med(r[4] for r in x):+.1f}。")
        out.append("")
    pooled = pooled_rotation()
    if pooled:
        out.append(pooled)
    return "\n".join(out).rstrip() if out else "（没有两个版本同场的会话数据；运行 scripts/day.sh）"


def pooled_rotation():
    """把所有"v1 的 A、v2 的 A、B 同场轮流各 60 秒"的轮次合在一起（不同开机、不同日期），逐轮配对。全部不带样本导出。"""
    def rd(p):
        with open(p) as f:
            return json.load(f)
    rounds, sources = [], []   # [(A1, A2, B)]
    for d, a1, a2 in ((os.path.join(ROOT, "logs/exp/sleep-unchecked"), "A", "X"),) + tuple(
            (d, "A1", "A2") for d in sorted(glob.glob(os.path.join(ROOT, "logs/versions/*/"))) + sorted(glob.glob(os.path.join(ROOT, "logs/sessions/*-raw/rot/")))):
        n = 0
        for pb in sorted(glob.glob(os.path.join(d, "B-*.json"))):
            i = os.path.basename(pb)[2:]
            p1, p2 = os.path.join(d, f"{a1}-{i}"), os.path.join(d, f"{a2}-{i}")
            if os.path.exists(p1) and os.path.exists(p2):
                rounds.append((rd(p1), rd(p2), rd(pb)))
                n += 1
        if n:
            sources.append(n)
    if len(rounds) < 10:
        return ""
    tot = lambda r: sum(metric(r, n)["mean"] for n in ("seg①", "seg②", "seg③"))
    stats = [("进程内 p50", lambda r: metric(r, "in-process")["p50_interp"]), ("进程内 p99", lambda r: metric(r, "in-process")["p99_interp"]),
             ("段② 平均", lambda r: metric(r, "seg②")["mean"]), ("段② p99", lambda r: metric(r, "seg②")["p99_interp"]),
             ("① + ③ 平均（发送侧）", lambda r: metric(r, "seg①")["mean"] + metric(r, "seg③")["mean"]), ("① + ② + ③ 平均（总账）", tot)]
    f = lambda xs: "{:+.1f} [{:+.1f}, {:+.1f}]".format(*mean_ci(xs))
    out = [f"**所有同场轮流的轮次合并**（共 {len(rounds)} 轮 = {' + '.join(str(n) for n in sources)}，各 60 秒，全部不带样本导出；逐轮配对后的平均值与 95% 区间，ns）：\n",
           "| 指标 | v1：A1 − B | v2：A2 − B | **v2 − v1（A2 − A1）** | v2 更低的轮数 |", "|---|---|---|---|---|"]
    for label, fn in stats:
        d1 = [fn(a1) - fn(b) for a1, _, b in rounds]
        d2 = [fn(a2) - fn(b) for _, a2, b in rounds]
        dd = [fn(a2) - fn(a1) for a1, a2, _ in rounds]
        out.append(f"| {label} | {f(d1)} | {f(d2)} | **{f(dd)}** | {sum(x < 0 for x in dd)} / {len(dd)} |")
    lost = sum(r["counters"]["timeouts"] for rr in rounds for r in rr)
    out.append(f"\n这 {3 * len(rounds)} 次运行丢包合计 {lost}。")
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


def section_versions(a):
    """代码版本 v1 → v2 的对比。"""
    out = []
    cols = [("v1 主考核", "logs/v1/final/A-600.json", "logs/v1/final/B-600.json", "logs/v1/final/ci.json"),
            ("**v2 主考核**", a.main_a, a.main_b, a.ci),
            ("v1 · mfence 口径", "logs/v1/diag/A-mfence.json", "logs/v1/diag/B-mfence.json", "logs/v1/diag/ci-mfence.json"),
            ("**v2 · mfence 口径**", os.path.join(a.diag_dir, "A-mfence.json"), os.path.join(a.diag_dir, "B-mfence.json"), os.path.join(a.diag_dir, "ci-mfence.json"))]
    cols = [c for c in cols if all(os.path.exists(os.path.join(ROOT, p)) for p in c[1:])]
    if len(cols) >= 2:
        t = [tax_parts(pa, pb, ci) for _, pa, pb, ci in cols]
        f1 = lambda v: f"{v:+.1f}"
        tri = lambda v: " / ".join(f"{x:.0f}" for x in v)
        rows = [("A 的段②：平均 / p50 / p99（ns）", lambda x: f"{x['a_seg2_mean']:.1f} / {x['a_seg2']['p50']:.0f} / {x['a_seg2']['p99']:.0f}"),
                ("每往后一个位置，段②增加：A", lambda x: tri(x["incr_a"])),
                ("每往后一个位置，段②增加：B", lambda x: tri(x["incr_b"])),
                ("**② 批内排队**（A − B，每请求平均）", lambda x: f"**{x['queue']:+.1f}**"),
                ("① 接收路径本身（A − B）", lambda x: f1(x["path"])),
                ("③ 发送侧调度（A − B）", lambda x: f1(x["send"])),
                ("每请求总账（A − B）", lambda x: f"**{x['total']:+.1f}**"),
                ("段②：平均 / p50 / p99 的 A − B", lambda x: f"{x['seg2']:+.1f} / {x['seg2_p50']:+.1f} / {x['seg2_p99']:+.1f}"),
                ("进程内 p50：A / B / A − B", lambda x: f"{x['a']['p50']:.1f} / {x['b']['p50']:.1f} / **{x['p50']:+.1f}**"),
                ("进程内 p99：A / B / A − B", lambda x: f"{x['a']['p99']:.1f} / {x['b']['p99']:.1f} / **{x['p99']:+.1f}**")]
        out += ["两个版本各自的测量（不同时段跑的，所以除了代码之外还有时段的差别；ns）：\n",
                "| | " + " | ".join(c[0] for c in cols) + " |", "|---|" + "---|" * len(cols)]
        for label, fn in rows:
            out.append(f"| {label} | " + " | ".join(fn(x) for x in t) + " |")
    # 同一时段轮流跑的对比
    rounds = []
    exp = os.path.join(ROOT, "logs/exp/sleep-unchecked/summary.json")
    if os.path.exists(exp):
        with open(exp) as f:
            for r in json.load(f)["rounds"]:
                rounds.append(("并入之前的实验", r["A"], r["X"]))
    for p in sorted(glob.glob(os.path.join(ROOT, "logs/versions/*/summary.json"))):
        with open(p) as f:
            for r in json.load(f)["rounds"]:
                rounds.append((os.path.basename(os.path.dirname(p)), r["A1"], r["A2"]))
    if rounds:
        pos = ("pos1", "pos2", "pos3", "pos4+")
        inc = lambda x: " / ".join(f"{x['seg2_mean_by_position'][pos[i + 1]]['A'] - x['seg2_mean_by_position'][pos[i]]['A']:.0f}" for i in range(3))
        out += ["", f"同一时段轮流跑（每一轮里 v1 的 A、v2 的 A、B 各跑 60 秒，消除时段的差别；共 {len(rounds)} 轮）。表里都是\"A − B\"，ns：\n",
                "| 轮 | 来源 | ② 批内排队：v1 → v2 | 每往后一个位置 A 的段②增加：v1 → v2 | 段② p99 的差值：v1 → v2 | 段② 平均的差值：v1 → v2 | 进程内 p50 的差值：v1 → v2 | 进程内 p99 的差值：v1 → v2 |",
                "|---|---|---|---|---|---|---|---|"]
        for i, (src, o, n) in enumerate(rounds, 1):
            out.append(f"| {i} | {src} | {o['queueing_extra_ns']:+.1f} → **{n['queueing_extra_ns']:+.1f}** | {inc(o)} → {inc(n)} "
                       f"| {o['seg2_diff']['p99']:+.1f} → **{n['seg2_diff']['p99']:+.1f}** | {o['seg2_diff']['mean']:+.1f} → {n['seg2_diff']['mean']:+.1f} "
                       f"| {o['inproc_diff']['p50']:+.1f} → {n['inproc_diff']['p50']:+.1f} | {o['inproc_diff']['p99']:+.1f} → {n['inproc_diff']['p99']:+.1f} |")
        chg = lambda key, sub=None: [(n[key][sub] if sub else n[key]) - (o[key][sub] if sub else o[key]) for _, o, n in rounds]
        line = []
        for label, xs in (("② 批内排队", chg("queueing_extra_ns")), ("段② p99 的差值", chg("seg2_diff", "p99")), ("段② 平均的差值", chg("seg2_diff", "mean")),
                          ("进程内 p50 的差值", chg("inproc_diff", "p50")), ("进程内 p99 的差值", chg("inproc_diff", "p99"))):
            m, lo, hi = mean_ci(xs)
            line.append(f"{label} {m:+.1f}（95% 区间 {lo:+.1f} ~ {hi:+.1f}，{sum(x < 0 for x in xs)} / {len(xs)} 轮下降）")
        out.append(f"\n逐轮的变化量（v2 − v1）：" + "；".join(line) + "。")
    return "\n".join(out) if out else "（没有可对比的数据）"


def session_entries(a):
    """全部会话：[(meta, A 主考核, B 主考核, ci, 交替对比目录)]，按测量开始时间排序。"""
    entries = []
    v1_ab = sorted(glob.glob(os.path.join(ROOT, "logs/v1/ab-*")))
    if os.path.exists(os.path.join(ROOT, "logs/v1/final/meta.json")) and v1_ab:
        entries.append((load("logs/v1/final/meta.json"), "logs/v1/final/A-600.json", "logs/v1/final/B-600.json", "logs/v1/final/ci.json",
                        os.path.relpath(v1_ab[-1], ROOT)))
    ab = sorted(glob.glob(os.path.join(ROOT, a.ab)))
    meta_p = os.path.join(os.path.dirname(a.main_a), "meta.json")
    if os.path.exists(os.path.join(ROOT, meta_p)) and os.path.exists(os.path.join(ROOT, a.ci)):
        entries.append((load(meta_p), a.main_a, a.main_b, a.ci, os.path.relpath(ab[-1], ROOT) if ab else "none"))
    for meta, rel in all_sessions():
        if os.path.exists(os.path.join(ROOT, rel, "ci.json")):
            entries.append((meta, rel + "/A-600.json", rel + "/B-600.json", rel + "/ci.json", rel + "/ab"))
    return sorted(entries, key=lambda e: e[0]["started"])


def section_samples(a):
    """样本导出（--samples）对测量的干扰，以及 v3 的纠正（logs/exp/samples-slow/）。"""
    import re
    d = os.path.join(ROOT, "logs/exp/samples-slow")
    groups = {}
    for p in sorted(glob.glob(os.path.join(d, "*.json"))):
        with open(p) as f:
            groups.setdefault(re.sub(r"-\d+$", "", os.path.basename(p)[:-5]), []).append(json.load(f))
    if not groups:
        return "（没有实验数据：logs/exp/samples-slow/）"
    rows = [("v2-plain", "A（v2），不导出样本"), ("v2-samples", "**A（v2），导出样本**"), ("v2-plain-mfence", "A（v2），不导出样本，T0 前加 `mfence`"),
            ("v2-samples-mfence", "A（v2），导出样本，T0 前加 `mfence`"), ("fix-samples", "**A（实验版：写样本时预取下一条缓存行），导出样本**"),
            ("v1-plain", "A（v1），不导出样本"), ("v1-samples", "A（v1），导出样本"), ("B-plain", "B，不导出样本"), ("B-samples", "B，导出样本")]
    med = statistics.median
    out = ["各 60 秒，同一时段轮流跑；除占比给出最小 ~ 最大之外，其余各列是各次运行的中位数（ns）：\n",
           "| 组合 | 次数 | 段① ≥ 125 ns 的占比 | 段① 平均 / p99 | 段③ 平均 | ① + ② + ③ | 进程内 p50 / p99 / 平均 |", "|---|---|---|---|---|---|---|"]
    for key, label in rows:
        rs = groups.get(key)
        if not rs:
            continue
        mm = lambda n, q: med(metric(r, n)[q] for r in rs)
        sl = [r["seg1_slow_percent"] for r in rs]
        out.append(f"| {label} | {len(rs)} | {min(sl):.2f}% ~ {max(sl):.2f}% | {mm('seg①', 'mean'):.1f} / {mm('seg①', 'p99'):.0f} | {mm('seg③', 'mean'):.1f} "
                   f"| {med(sum(metric(r, n)['mean'] for n in ('seg①', 'seg②', 'seg③')) for r in rs):.1f} "
                   f"| {mm('in-process', 'p50_interp'):.1f} / {mm('in-process', 'p99_interp'):.1f} / {mm('in-process', 'mean'):.1f} |")
    lost = sum(r["counters"]["timeouts"] for rs in groups.values() for r in rs)
    leak = sum(r["mbuf"]["avail_initial"] - r["mbuf"]["avail_final"] for rs in groups.values() for r in rs)
    out.append(f"\n共 {sum(len(v) for v in groups.values())} 次运行，丢包合计 {lost}，泄漏合计 {leak}。")
    # 各会话的 10 分钟主考核（都带样本导出）里的慢发送占比
    out += ["\n各会话的 10 分钟主考核（全部带样本导出）里，段① ≥ 125 ns 的占比：\n", "| 会话 | 代码版本 | A | B |", "|---|---|---|---|"]
    for meta, pa, pb, _, _ in session_entries(a):
        out.append(f"| {meta['name']} | {meta.get('version', '?')} | {load(pa)['seg1_slow_percent']:.2f}% | {load(pb)['seg1_slow_percent']:.2f}% |")
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
        rx = c["rx_pkts"] - (c["received"] + c["late"] + c["unexpected"] + c.get("foreign", 0) + c["other_rx"] + c["arp_replies"])
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
    return (("main", section_main), ("totals", section_totals), ("ab", section_ab), ("c", section_c), ("probe", section_probe),
            ("ci", section_ci), ("burst", section_burst), ("diag", section_diag), ("stores", section_stores), ("sessions", section_sessions), ("drift", section_drift), ("day", section_day), ("samples", section_samples), ("versions", section_versions),
            ("fault", section_fault),
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
