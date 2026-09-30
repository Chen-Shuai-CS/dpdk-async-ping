#!/usr/bin/env python3
"""汇总 C（系统 ping）的逐包 RTT：分位数口径与 A / B 的报表相同（ns，最近秩法）。"""
import argparse
import json
import math
import re

TIME_RE = re.compile(r"time=([0-9.]+) ms")
SUM_RE = re.compile(r"(\d+) packets transmitted, (\d+) received")


def quantile(sorted_vals, q):
    """与 Rust 侧 Hist::quantile 相同的最近秩法：rank = ceil(q * n)。"""
    n = len(sorted_vals)
    rank = min(max(math.ceil(q * n), 1), n)
    return sorted_vals[rank - 1]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("files", nargs="+")
    ap.add_argument("--mode", default="user")
    ap.add_argument("--flows", type=int)
    ap.add_argument("--interval-ms", type=float)
    ap.add_argument("--duration-sec", type=float)
    ap.add_argument("--json")
    a = ap.parse_args()

    rtts, sent, recv = [], 0, 0
    for p in a.files:
        with open(p, errors="replace") as f:
            for line in f:
                m = TIME_RE.search(line)
                if m:
                    rtts.append(float(m.group(1)) * 1e6)  # ms → ns（ping 的分辨率是 1 µs）
                    continue
                m = SUM_RE.search(line)
                if m:
                    sent += int(m.group(1))
                    recv += int(m.group(2))
    rtts.sort()
    n = len(rtts)
    row = {
        "name": f"end-to-end（ping -{'U 用户态↔用户态' if a.mode == 'user' else ' 默认：内核接收时间戳'}）",
        "count": n,
        "min": round(rtts[0]) if n else 0,
        "mean": sum(rtts) / n if n else 0,
        **{k: round(quantile(rtts, q)) if n else 0 for k, q in
           [("p50", .5), ("p90", .9), ("p99", .99), ("p99_9", .999), ("p99_99", .9999)]},
        "max": round(rtts[-1]) if n else 0,
    }
    print(f"\n==================== C · 系统 ping（内核网卡） ====================")
    print(f"{a.flows} 路 × 每 {a.interval_ms} ms，{a.duration_sec} s，口径：{a.mode}")
    print(f"sent {sent}  received {recv}  lost {sent - recv}  （聚合 {sent / a.duration_sec:.0f} 包/秒）")
    print(f"\n{'metric (ns)':<40} {'count':>9} {'min':>8} {'mean':>8} {'p50':>8} {'p90':>8} {'p99':>8} {'p99.9':>8} {'p99.99':>8} {'max':>9}")
    print(f"{row['name']:<40} {n:>9} {row['min']:>8} {row['mean']:>8.0f} {row['p50']:>8} {row['p90']:>8} "
          f"{row['p99']:>8} {row['p99_9']:>8} {row['p99_99']:>8} {row['max']:>9}")
    if a.json:
        with open(a.json, "w") as f:
            json.dump({"client": "C · system ping", "mode": a.mode, "flows": a.flows, "interval_ms": a.interval_ms,
                       "duration_sec": a.duration_sec, "sent": sent, "received": recv, "metrics": [row]}, f,
                      ensure_ascii=False, indent=2)


if __name__ == "__main__":
    main()
