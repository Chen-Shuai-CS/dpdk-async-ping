#!/usr/bin/env python3
"""汇总多轮 A / B 的 JSON 报告：逐轮列出关键分位数，再给出各自的中位数与 A − B。

用法：scripts/summarize.py logs/A-*.json logs/B-*.json
"""
import json
import statistics
import sys


def load(path):
    with open(path) as f:
        r = json.load(f)
    m = {row["name"].split()[0] if not row["name"].startswith("  ") else row["name"].strip(): row for row in r["metrics"]}
    return r, m


def pick(m, key_prefix):
    for k, v in m.items():
        if k.startswith(key_prefix):
            return v
    return None


COLS = [
    ("in-process", "mean"), ("in-process", "p50"), ("in-process", "p99"), ("in-process", "p99_9"), ("in-process", "p99_99"),
    ("seg①", "p50"), ("seg①", "p99"), ("seg②", "p50"), ("seg②", "p99"), ("end-to-end", "p50"),
]


def main(paths):
    runs = {"A": [], "B": []}
    print(f"{'run':<34} {'sent':>9} {'loss':>5} {'leak':>5} " + " ".join(f"{c[0][:6]}.{c[1]:<6}" for c in COLS))
    for p in sorted(paths):
        r, m = load(p)
        tag = "A" if r["client"].startswith("A") else "B"
        vals = []
        for name, q in COLS:
            row = pick(m, name)
            vals.append(row[q] if row else 0)
        runs[tag].append(vals)
        c = r["counters"]
        leak = r["mbuf"]["avail_initial"] - r["mbuf"]["avail_final"]
        name = p.split("/")[-1]
        print(f"{name:<34} {c['sent']:>9} {c['timeouts']:>5} {leak:>5} "
              + " ".join(f"{v:>13.1f}" if isinstance(v, float) else f"{v:>13}" for v in vals))
    if runs["A"] and runs["B"]:
        print()
        med = {k: [statistics.median(col) for col in zip(*v)] for k, v in runs.items()}
        print(f"{'各轮中位数':<34} {'':>9} {'':>5} {'':>5} " + " ".join(f"{c[0][:6]}.{c[1]:<6}" for c in COLS))
        for k in ("A", "B"):
            print(f"{k + f'（{len(runs[k])} 轮）':<34} {'':>9} {'':>5} {'':>5} " + " ".join(f"{v:>13.0f}" for v in med[k]))
        diff = [a - b for a, b in zip(med["A"], med["B"])]
        print(f"{'A − B':<34} {'':>9} {'':>5} {'':>5} " + " ".join(f"{v:>+13.0f}" for v in diff))


if __name__ == "__main__":
    main(sys.argv[1:])
