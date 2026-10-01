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
}


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


def section_ab(a):
    dirs = sorted(glob.glob(os.path.join(ROOT, a.ab)))
    cols = [("in-process", "p50"), ("in-process", "p99"), ("in-process", "p99_9"), ("in-process", "p99_99"),
            ("seg②", "p50"), ("seg②", "p99"), ("seg①", "p99"), ("seg①", "p99_9")]
    parts = []
    for d in dirs:
        runs = {"A": [], "B": []}
        secs = 0
        for p in sorted(glob.glob(d + "/*.json")):
            with open(p) as f:
                r = json.load(f)
            secs = r["duration_sec"]
            runs["A" if r["client"].startswith("A") else "B"].append([metric(r, n)[q] for n, q in cols])
        if not runs["A"] or not runs["B"]:
            continue
        med = {k: [statistics.median(c) for c in zip(*v)] for k, v in runs.items()}
        lines = [f"**{len(runs['A'])} 对 × {secs} 秒**（`{os.path.relpath(d, ROOT)}`，各轮中位数，ns）\n",
                 "| | 进程内 p50 | p99 | p99.9 | p99.99 | 段② p50 | 段② p99 | 段① p99 | 段① p99.9 |",
                 "|---|---|---|---|---|---|---|---|---|"]
        for k in ("A", "B"):
            lines.append(f"| {k} | " + " | ".join(f"{x:.0f}" for x in med[k]) + " |")
        lines.append("| **A − B** | " + " | ".join(f"{x - y:+.0f}" for x, y in zip(med["A"], med["B"])) + " |")
        per_run = "；".join(
            f"{k} 各轮进程内 p99 = " + " / ".join(str(v[1]) for v in runs[k]) + "，段① p99 = " + " / ".join(str(v[6]) for v in runs[k])
            for k in ("A", "B"))
        lines.append(f"\n逐轮：{per_run}。")
        parts.append("\n".join(lines))
    return "\n\n".join(parts)


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


def main():
    ap = argparse.ArgumentParser()
    for k, v in DEFAULTS.items():
        ap.add_argument("--" + k.replace("_", "-"), default=v)
    ap.add_argument("--ab", default="logs/ab-*", help="ABBA 结果目录（glob）；默认全部")
    ap.add_argument("--out", default="docs/REPORT.md")
    a = ap.parse_args()
    path = os.path.join(ROOT, a.out)
    with open(path) as f:
        doc = f.read()
    for name, fn in (("main", section_main), ("totals", section_totals), ("ab", section_ab), ("c", section_c), ("probe", section_probe)):
        pat = re.compile(rf"(<!-- BEGIN:{name} -->).*?(<!-- END:{name} -->)", re.S)
        if not pat.search(doc):
            print(f"警告：{a.out} 里没有 {name} 标记", file=sys.stderr)
            continue
        body = fn(a)
        doc = pat.sub(lambda m: m.group(1) + "\n" + body + "\n" + m.group(2), doc)
    with open(path, "w") as f:
        f.write(doc)
    print(f"已更新 {a.out}")


if __name__ == "__main__":
    main()
