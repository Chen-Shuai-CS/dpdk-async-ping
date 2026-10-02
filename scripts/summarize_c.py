#!/usr/bin/env python3
"""汇总 C（系统 ping）的逐包 RTT：分位数口径与 A / B 的报表相同（ns，最近秩法）。

速率是**实测**的：每个 ping 进程结束时会报告自己实际跑了多久（"…, time 62200ms"，从它发第一个包到收完最后一个包），
用"它发出的包数 ÷ 这个时长"得到每一路的实际速率，再把各路加起来。名义速率（按 -i 的设定算）只作对照——
ping 的定时并不精确，实际速率通常比名义的低百分之几。
"""
import argparse
import json
import math
import re

TIME_RE = re.compile(r"time=([0-9.]+) ms")
SUM_RE = re.compile(r"(\d+) packets transmitted, (\d+) received.*?time (\d+)ms")


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
    ap.add_argument("--wall-sec", type=float, default=0.0, help="整批 ping 从启动到全部退出的墙上时间（含进程启动与收尾）")
    ap.add_argument("--json")
    a = ap.parse_args()

    rtts, sent, recv = [], 0, 0
    flow_sec, flow_pps = [], []   # 每一路：实际跑了多久、实际速率
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
                    sec = int(m.group(3)) / 1000
                    if sec > 0:
                        flow_sec.append(sec)
                        flow_pps.append(int(m.group(1)) / sec)
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
    nominal_pps = a.flows * 1000 / a.interval_ms
    actual_pps = sum(flow_pps)
    flow_sec.sort()
    print(f"sent {sent}  received {recv}  lost {sent - recv}")
    if flow_sec:
        print(f"速率：名义 {nominal_pps:,.0f} 包/秒（{a.flows} 路 × 每 {a.interval_ms:g} ms）；**实测 {actual_pps:,.0f} 包/秒**"
              f"（比名义{'低' if actual_pps < nominal_pps else '高'} {abs(1 - actual_pps / nominal_pps) * 100:.1f}%）")
        print(f"每一路实际跑了 {flow_sec[0]:.2f} ~ {flow_sec[-1]:.2f} 秒（中位数 {flow_sec[len(flow_sec) // 2]:.2f}；设定 {a.duration_sec:g} 秒，"
              f"是每个 ping 自己报告的“首包发出 → 末包收完”）；整批从启动到全部退出 {a.wall_sec:.1f} 秒（含进程启动与收尾）")
    print(f"\n{'metric (ns)':<40} {'count':>9} {'min':>8} {'mean':>8} {'p50':>8} {'p90':>8} {'p99':>8} {'p99.9':>8} {'p99.99':>8} {'max':>9}")
    print(f"{row['name']:<40} {n:>9} {row['min']:>8} {row['mean']:>8.0f} {row['p50']:>8} {row['p90']:>8} "
          f"{row['p99']:>8} {row['p99_9']:>8} {row['p99_99']:>8} {row['max']:>9}")
    if a.json:
        with open(a.json, "w") as f:
            json.dump({"client": "C · system ping", "mode": a.mode, "flows": a.flows, "interval_ms": a.interval_ms,
                       "duration_sec": a.duration_sec, "sent": sent, "received": recv,
                       "nominal_pps": nominal_pps, "actual_pps": actual_pps, "wall_sec": a.wall_sec,
                       "flow_elapsed_sec": {"min": flow_sec[0], "median": flow_sec[len(flow_sec) // 2], "max": flow_sec[-1]} if flow_sec else None,
                       "metrics": [row]}, f,
                      ensure_ascii=False, indent=2)


if __name__ == "__main__":
    main()
