#!/usr/bin/env python3
"""把报告里的关键结论画成图（docs/img/*.png）。数据来自 logs/ 下的 JSON，不需要原始样本。

用法：scripts/plots.py            （路径见下面的参数默认值）

图里的文字用英文（服务器上没有中文字体）；中文说明写在 docs/REPORT.md 里每张图的下方。
"""
import argparse
import glob
import json
import os

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CA, CB = "#d62728", "#1f77b4"   # A 红，B 蓝


def load(p):
    with open(os.path.join(ROOT, p)) as f:
        return json.load(f)


def metric(r, prefix):
    return next(m for m in r["metrics"] if m["name"].strip().startswith(prefix))


def save(fig, out, name):
    path = os.path.join(out, name)
    fig.savefig(path, dpi=130, bbox_inches="tight")
    plt.close(fig)
    print("写出", os.path.relpath(path, ROOT))


def plot_ccdf(ci, ci_m, out):
    """进程内耗时的互补累积分布：纵轴是"比横轴更慢的请求占多少"。"""
    panels = [(ci, "As specified (T0 = send() entry)")]
    if ci_m:
        panels.append((ci_m, "Diagnostic: mfence before T0 (device wait moved out of seg1)"))
    fig, axes = plt.subplots(1, len(panels), figsize=(6.4 * len(panels), 4.4), sharey=True)
    axes = [axes] if len(panels) == 1 else list(axes)
    for ax, (c, title) in zip(axes, panels):
        for side, col in (("A", CA), ("B", CB)):
            d = c["cdf"]["inproc"][side]
            ax.step(d["ns"], d["ccdf"], where="post", color=col, lw=1.6, label=f"{side} ({'async runtime' if side == 'A' else 'hand-written loop'})")
        for q, lab in ((0.5, "p50"), (0.01, "p99"), (0.001, "p99.9"), (0.0001, "p99.99")):
            ax.axhline(q, color="gray", lw=0.6, ls=":")
            ax.text(1010, q, lab, va="bottom", ha="right", fontsize=8, color="gray")
        ax.set_yscale("log")
        ax.set_xlim(0, 1020)
        ax.set_ylim(1e-5, 1.2)
        ax.set_xlabel("in-process time seg1 + seg2 (ns)")
        ax.set_title(title, fontsize=10)
        ax.grid(True, which="major", alpha=0.25)
        ax.legend(loc="lower left", fontsize=9)
    axes[0].set_ylabel("fraction of requests slower than x")
    save(fig, out, "ccdf.png")


def plot_series(ci, out):
    """每秒一个点：10 分钟内 p50 / p99 怎么变。"""
    fig, axes = plt.subplots(2, 1, figsize=(10, 5.2), sharex=True)
    for ax, key, title in ((axes[0], "p99", "p99 (ns)"), (axes[1], "p50", "p50 (ns)")):
        for side, col in (("A", CA), ("B", CB)):
            y = ci["series"][side][key]
            ax.plot(range(len(y)), y, color=col, lw=0.8, label=side)
        ax.set_ylabel(title, fontsize=9)
        ax.grid(True, alpha=0.25)
        ax.legend(loc="upper right", ncol=2, fontsize=9)
    axes[1].set_xlabel("time (s)")
    fig.suptitle("In-process time over the 10-minute run: one point per second (A and B are separate runs)", fontsize=10, y=0.94)
    save(fig, out, "timeseries.png")


def plot_bootstrap(ci, ci_m, out):
    """A − B 的自助分布：整个分布离 0 多远，就是结论有多可靠。"""
    items = [(ci, "p50", "as specified: p50"), (ci, "p99", "as specified: p99")]
    if ci_m:
        items += [(ci_m, "p50", "mfence diagnostic: p50"), (ci_m, "p99", "mfence diagnostic: p99")]
    fig, axes = plt.subplots(1, len(items), figsize=(3.6 * len(items), 3.2))
    for ax, (c, q, title) in zip(axes, items):
        x = c["boot"][f"inproc_{q}_diff"]
        d = c["metrics"]["inproc"]["diff"][q]
        ax.hist(x, bins=40, color="#7f7f7f")
        ax.axvline(d["interp"], color="k", lw=1.2)
        for v in d["interp_ci"]:
            ax.axvline(v, color="k", lw=0.8, ls="--")
        ax.set_title(f"{title}\nA-B = {d['interp']:+.1f} ns  [{d['interp_ci'][0]:+.1f}, {d['interp_ci'][1]:+.1f}]", fontsize=9)
        ax.set_xlabel("A - B (ns)")
        ax.set_yticks([])
    fig.suptitle("Block-bootstrap distribution of A - B (in-process time; solid = estimate, dashed = 95% CI)", fontsize=10, y=1.04)
    save(fig, out, "bootstrap.png")


def plot_burst(ci, out):
    """段②按批内位置：第 1 个包的差值是"纯"的抽象税，后面的包还要排在前面的包后面。"""
    keys = [("pos1", "1st in burst"), ("pos2", "2nd"), ("pos3", "3rd"), ("pos4+", "4th+")]
    b = ci["burst_position"]
    fig, ax = plt.subplots(figsize=(7.2, 4))
    w = 0.38
    for i, (k, lab) in enumerate(keys):
        a_, b_ = b[k]["A"]["mean"], b[k]["B"]["mean"]
        ax.bar(i - w / 2, a_, w, color=CA, label="A" if i == 0 else None)
        ax.bar(i + w / 2, b_, w, color=CB, label="B" if i == 0 else None)
        ax.text(i, max(a_, b_) + 6, f"A-B = {b[k]['diff']['mean']['value']:+.0f} ns\n({b[k]['share']['A']:.1%} of replies)", ha="center", fontsize=9)
    ax.set_xticks(range(len(keys)))
    ax.set_xticklabels([lab for _, lab in keys])
    ax.set_ylabel("mean seg2 (T2 -> T3), ns")
    ax.set_ylim(0, max(b[k]["A"]["mean"] for k, _ in keys) * 1.25)
    ax.set_title("seg2 by position of the reply inside its rx burst", fontsize=10)
    ax.legend(loc="upper left")
    ax.grid(True, axis="y", alpha=0.25)
    save(fig, out, "burst_position.png")


def plot_totals(pairs, out):
    """每个请求的平均耗时落在哪一段（平均值可以相加）。"""
    fig, ax = plt.subplots(figsize=(8.4, 1.3 + 0.75 * 2 * len(pairs)))
    ys, labels = [], []
    segs = [("seg③", "seg3 (wake -> next T0, not ranked)", "#bbbbbb"), ("seg①", "seg1 (send)", "#ff9d5c"), ("seg②", "seg2 (receive)", "#6fb07f")]
    y = 0
    for title, A, B in pairs:
        for side, r in (("B", B), ("A", A)):
            left = 0
            for i, (name, lab, col) in enumerate(segs):
                v = metric(r, name)["mean"]
                ax.barh(y, v, left=left, color=col, edgecolor="white", label=lab if y == 0 else None)
                ax.text(left + v / 2, y, f"{v:.0f}", ha="center", va="center", fontsize=9)
                left += v
            ax.text(left + 4, y, f"{left:.0f} ns", va="center", fontsize=9, fontweight="bold")
            ys.append(y)
            labels.append(f"{side}  ({title})")
            y += 1
        y += 0.5
    ax.set_yticks(ys)
    ax.set_yticklabels(labels, fontsize=9)
    ax.set_xlabel("mean time per request spent in our own code (ns)")
    ax.set_xlim(0, max(sum(metric(r, n)["mean"] for n in ("seg①", "seg②", "seg③")) for _, A, B in pairs for r in (A, B)) * 1.15)
    ax.legend(loc="lower center", bbox_to_anchor=(0.5, 1.0), ncol=3, fontsize=8, frameon=False)
    ax.grid(True, axis="x", alpha=0.25)
    save(fig, out, "totals.png")


def plot_abba(d, out):
    """逐对的 A − B：不同运行之间的波动。"""
    runs = {}
    for p in glob.glob(os.path.join(d, "[AB]-*.json")):
        with open(p) as f:
            r = json.load(f)
        runs[(r["client"][0], int(os.path.basename(p).split("-")[1].split(".")[0]))] = r
    idx = sorted({i for _, i in runs})
    idx = [i for i in idx if ("A", i) in runs and ("B", i) in runs]
    if not idx or "p50_interp" not in metric(runs[("A", idx[0])], "in-process"):
        return
    diff = lambda name, key: [metric(runs[("A", i)], name)[key] - metric(runs[("B", i)], name)[key] for i in idx]
    fig, axes = plt.subplots(1, 3, figsize=(12, 3.4))
    for ax, (name, key, title) in zip(axes, (("in-process", "p50_interp", "in-process p50"), ("in-process", "p99_interp", "in-process p99"),
                                              ("seg②", "mean", "seg2 mean"))):
        y = diff(name, key)
        cols = ["#444444" if i % 2 else "#aaaaaa" for i in idx]
        ax.bar(idx, y, color=cols)
        ax.axhline(0, color="k", lw=0.8)
        ax.axhline(sum(y) / len(y), color="tab:green", lw=1.2, ls="--")
        ax.set_title(f"{title}\nA - B per pair, mean {sum(y) / len(y):+.1f} ns", fontsize=9)
        ax.set_xlabel("pair (dark: A first, light: B first)")
        ax.set_xticks(idx)
        ax.grid(True, axis="y", alpha=0.25)
    axes[0].set_ylabel("A - B (ns)")
    save(fig, out, "abba.png")


def plot_drift(session_dir, out):
    """一个会话里每次运行一个点：A 的尾部状态怎么随时间变（横轴：距开机多少分钟）。"""
    import datetime
    meta_p = os.path.join(session_dir, "meta.json")
    if not os.path.exists(meta_p):
        return
    with open(meta_p) as f:
        meta = json.load(f)
    boot = datetime.datetime.strptime(meta["boot_time"], "%Y-%m-%d %H:%M:%S").replace(tzinfo=datetime.timezone.utc).timestamp()
    paths = [os.path.join(session_dir, f) for f in ("A-600.json", "B-600.json")]
    paths += glob.glob(os.path.join(session_dir, "ab", "[AB]-*.json")) + glob.glob(os.path.join(session_dir, "drift", "runs", "[AB]-*.json"))
    runs = []
    for p in paths:
        if os.path.exists(p):
            with open(p) as f:
                r = json.load(f)
            if not r.get("diag"):
                runs.append(r)
    if len(runs) < 30:
        return
    runs.sort(key=lambda r: r["env"]["started_unix"])
    t = lambda r: (r["env"]["started_unix"] + r["duration_sec"] / 2 - boot) / 60
    tot = lambda r: sum(metric(r, n)["mean"] for n in ("seg①", "seg②", "seg③"))
    A = [r for r in runs if r["client"].startswith("A")]
    B = [r for r in runs if r["client"].startswith("B")]
    fig, axes = plt.subplots(3, 1, figsize=(10, 7.6), sharex=True)
    panels = [(lambda r: metric(r, "seg②")["p99_interp"], "seg2 p99 (ns)"),
              (lambda r: metric(r, "in-process")["p99_interp"], "in-process p99 (ns)"),
              (tot, "seg1+seg2+seg3 mean (ns)")]
    for ax, (fn, label) in zip(axes, panels):
        for rs, col, lab in ((A, CA, "A"), (B, CB, "B")):
            ax.scatter([t(r) for r in rs], [fn(r) for r in rs], s=[10 if r["duration_sec"] < 60 else (26 if r["duration_sec"] < 600 else 70) for r in rs],
                       color=col, label=lab, alpha=0.8, linewidths=0)
        ax.set_ylabel(label, fontsize=9)
        ax.grid(True, alpha=0.25)
        ax.legend(loc="upper right", ncol=2, fontsize=9)
    axes[2].set_xlabel("minutes since boot (one point per run; large = 10-minute run, medium = 60 s, small = 20 s)")
    fig.suptitle(f"After the reboot: every run of session '{meta['name']}' on one time axis", fontsize=10, y=0.92)
    save(fig, out, "drift.png")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ci", default="logs/final/ci.json")
    ap.add_argument("--ci-mfence", default="logs/diag/ci-mfence.json")
    ap.add_argument("--main-a", default="logs/final/A-600.json")
    ap.add_argument("--main-b", default="logs/final/B-600.json")
    ap.add_argument("--diag-dir", default="logs/diag")
    ap.add_argument("--ab", default="", help="交替运行的目录；默认取最新的 logs/ab-*")
    ap.add_argument("--out", default="docs/img")
    a = ap.parse_args()
    out = os.path.join(ROOT, a.out)
    os.makedirs(out, exist_ok=True)
    ci = load(a.ci)
    ci_m = load(a.ci_mfence) if os.path.exists(os.path.join(ROOT, a.ci_mfence)) else None
    plot_ccdf(ci, ci_m, out)
    plot_series(ci, out)
    plot_bootstrap(ci, ci_m, out)
    plot_burst(ci, out)
    pairs = [("as specified", load(a.main_a), load(a.main_b))]
    ma, mb = os.path.join(a.diag_dir, "A-mfence.json"), os.path.join(a.diag_dir, "B-mfence.json")
    if os.path.exists(os.path.join(ROOT, ma)) and os.path.exists(os.path.join(ROOT, mb)):
        pairs.append(("mfence before T0", load(ma), load(mb)))
    plot_totals(pairs, out)
    ab = a.ab or (sorted(glob.glob(os.path.join(ROOT, "logs/ab-*")))[-1:] or [""])[0]
    if ab:
        plot_abba(ab if os.path.isabs(ab) else os.path.join(ROOT, ab), out)
    for d in sorted(glob.glob(os.path.join(ROOT, "logs/sessions/*/"))):
        plot_drift(d, out)


if __name__ == "__main__":
    main()
