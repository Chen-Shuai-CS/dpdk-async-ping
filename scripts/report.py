#!/usr/bin/env python3
"""从 A / B（/ C）的 JSON 报告生成 Markdown 表格，供 docs/REPORT.md 使用。

用法：scripts/report.py --a logs/A-x.json --b logs/B-y.json [--c logs/C-user-*/C.json ...]
"""
import argparse
import json


def load(p):
    with open(p) as f:
        return json.load(f)


def rows(r):
    return {m["name"].strip(): m for m in r["metrics"]}


def find(rs, prefix):
    for k, v in rs.items():
        if k.startswith(prefix):
            return v
    return None


QS = [("p50", "p50"), ("p90", "p90"), ("p99", "p99"), ("p99.9", "p99_9"), ("p99.99", "p99_99"), ("max", "max")]


def gates(r, label):
    c, m = r["counters"], r["mbuf"]
    leak = m["avail_initial"] - m["avail_final"]
    recon = c["sent"] - c["received"] - c["timeouts"] - c["in_flight_at_end"]
    ae = r["port"]["allowance_exceeded"]
    ax = "全部为 0" if all(v == 0 for _, v in ae) else ", ".join(f"{k}={v}" for k, v in ae if v)
    return (f"| {label} | {r['elapsed_sec']:.1f} s | {c['sent']:,} | {c['received']:,} | {c['timeouts']} "
            f"| {c['late']} | {recon} | {m['avail_initial']} → {m['avail_final']}（泄漏 {leak}） | {ax} | {r['exit_reason']} |")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--a", required=True)
    ap.add_argument("--b", required=True)
    ap.add_argument("--c", nargs="*", default=[])
    a = ap.parse_args()
    A, B = load(a.a), load(a.b)
    ra, rb = rows(A), rows(B)

    print("### 硬门槛\n")
    print("| 客户端 | 实际时长 | sent | received | 超时（丢包） | 迟到 | 对账差 | mbuf avail 初值 → 终值 | AWS allowance 超限 | 退出原因 |")
    print("|---|---|---|---|---|---|---|---|---|---|")
    print(gates(A, "A async-ping"))
    print(gates(B, "B raw-ping"))

    print("\n### 分位数（ns）与 A − B\n")
    print("| 指标 | | " + " | ".join(q for q, _ in QS) + " |")
    print("|---|---|" + "---|" * len(QS))
    for name in ["in-process", "seg①", "seg②", "end-to-end", "seg③", "sleep"]:
        x, y = find(ra, name), find(rb, name)
        if not x or not y:
            continue
        label = x["name"].split("[")[0].strip()
        print(f"| **{label}** | A | " + " | ".join(f"{x[k]:,}" for _, k in QS) + " |")
        print(f"| | B | " + " | ".join(f"{y[k]:,}" for _, k in QS) + " |")
        print(f"| | A − B | " + " | ".join(f"{x[k] - y[k]:+,}" for _, k in QS) + " |")

    print("\n### 段① 按「距上一次发送多久」分档（ns，诊断）\n")
    print("| 距上次发送 | A 样本占比 | A p50 | A p99 | B 样本占比 | B p50 | B p99 |")
    print("|---|---|---|---|---|---|---|")
    na, nb = find(ra, "seg①")["count"], find(rb, "seg①")["count"]
    for k in ra:
        if k.startswith("(诊断)"):
            x, y = ra[k], rb.get(k)
            if y is None:
                continue
            print(f"| {k.replace('(诊断) seg① ', '')} | {100 * x['count'] / max(na, 1):.1f}% | {x['p50']} | {x['p99']} "
                  f"| {100 * y['count'] / max(nb, 1):.1f}% | {y['p50']} | {y['p99']} |")

    print("\n### rx_burst 包数分布（非空 burst）\n")
    for label, r in (("A", A), ("B", B)):
        tot = sum(n for _, n in r["burst_sizes"])
        s = "  ".join(f"{k}: {100 * n / tot:.2f}%" for k, n in r["burst_sizes"] if 100 * n / tot >= 0.01)
        print(f"- {label}：{s}")

    if a.c:
        print("\n### 端到端：A（kernel bypass）vs C（系统 ping，内核网卡）（ns）\n")
        print("| 客户端 | 条件 | 样本 | 丢包 | " + " | ".join(q for q, _ in QS) + " |")
        print("|---|---|---|---|" + "---|" * len(QS))
        e = find(ra, "end-to-end")
        print(f"| A | {A['sessions']} 路，delay {A['delay_us']} µs | {e['count']:,} | {A['counters']['timeouts']} | "
              + " | ".join(f"{e[k]:,}" for _, k in QS) + " |")
        for p in a.c:
            C = load(p)
            m = C["metrics"][0]
            cond = f"{C['flows']} 路 × {C['interval_ms']:g} ms，口径 {C['mode']}"
            print(f"| C | {cond} | {m['count']:,} | {C['sent'] - C['received']} | "
                  + " | ".join(f"{m[k]:,}" for _, k in QS) + " |")


if __name__ == "__main__":
    main()
