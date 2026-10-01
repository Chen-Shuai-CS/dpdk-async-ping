#!/usr/bin/env python3
"""不确定度分析：从 A / B 的原始样本文件（运行时加 --samples）算出置信区间等，写成 ci.json。

用法：scripts/ci.py --a logs/final/A-600.samples --b logs/final/B-600.samples --out logs/final/ci.json

回答四个问题：
1. A − B 的每个分位数有多可信？           → 分块自助法（block bootstrap）的 95% 置信区间
2. 时间戳分辨率只有 10 ns，差值小于 10 ns 怎么办？→ 分组数据的插值分位数（见 quantiles()）
3. 结果在 10 分钟里稳定吗？               → 每秒一个点的 p50 / p99 / 平均值序列
4. 同一批里排在后面的包天然更晚，A / B 公平吗？ → 按"包在批内的位置"拆开段②

方法要点：
- 先分别求 A、B 的分位数，再相减（A 和 B 是两次独立的运行，样本之间没有一一对应关系，不能先相减）。
- 相邻样本不独立（同一批包、同一次被打断会连续影响很多样本），所以不能把 5000 万个样本当成 5000 万次独立观测。
  做法：把时间轴切成 20 秒一块，以"块"为单位有放回地重抽，块内的相关性原样保留。
  同时给出"假装样本独立"时的区间作对比，两者宽度之比的平方就是有效样本数缩水的倍数。
"""
import argparse
import json

import numpy as np

NB = 512          # 直方图格数（单位：TSC 步长，本机 10 ns）；超出的并入最后一格
BASE_SEC = 0.1    # 最细的时间块
QS = [("p50", 0.5), ("p90", 0.9), ("p99", 0.99), ("p99_9", 0.999), ("p99_99", 0.9999)]
SEED = 20261001


def load(path):
    """读样本文件 → (tsc_hz, 段①, 段②, T2 低 32 位)，后三者单位是 TSC 周期。"""
    hdr = np.fromfile(path, dtype="<u8", count=4)
    assert hdr[0].tobytes() == b"BQSAMPL1", f"{path} 不是样本文件"
    hz, n = int(hdr[1]), int(hdr[2])
    # 10 分钟有 5000 多万个样本：用内存映射读文件，并用尽量窄的整数类型，避免把内存撑爆
    r = np.memmap(path, dtype="<u8", mode="r", offset=32, shape=(n,))
    s1 = (r >> np.uint64(48)).astype(np.uint16)
    s2 = ((r >> np.uint64(32)) & np.uint64(0xFFFF)).astype(np.uint16)
    t2 = (r & np.uint64(0xFFFFFFFF)).astype(np.uint32)
    del r
    return hz, s1, s2, t2


def time_blocks(t2, hz):
    """T2 只存了低 32 位（约 1.6 秒回绕一次）；样本按时间顺序排列，逐个累加差值即可还原时间轴。
    返回（每个样本所在的时间块编号，总时长秒）。"""
    d = (t2[1:] - t2[:-1]).view(np.int32)      # 无符号相减自动回绕，再按有符号数解释 = 相邻样本的真实差值
    cyc = np.empty(len(t2), dtype=np.int64)
    cyc[0] = 0
    np.cumsum(d, dtype=np.int64, out=cyc[1:])
    del d
    dur = float(cyc[-1]) / hz
    cyc //= int(hz * BASE_SEC)
    return cyc.astype(np.int32), dur


def block_hist(values, blk, nb):
    """每个时间块一行的直方图：H[b, v] = 第 b 块里取值为 v 的样本数。"""
    key = blk.astype(np.int64)
    key *= NB
    key += np.minimum(values, NB - 1)
    return np.bincount(key, minlength=nb * NB).reshape(nb, NB).astype(np.float32)


def regroup(H, k):
    """把相邻 k 行合并成一行（改变块长）。"""
    nb = (H.shape[0] + k - 1) // k
    out = np.zeros((nb, H.shape[1]), dtype=H.dtype)
    np.add.at(out, np.arange(H.shape[0]) // k, H)
    return out


def quantiles(h, q, step_ns):
    """h：一行或多行直方图。返回（格点分位数，插值分位数），单位 ns。

    格点分位数：与程序报表相同的定义（第 ⌈q·n⌉ 小的样本所在的格），只能是 10 ns 的整数倍。
    插值分位数：时间戳每 10 ns 才跳一步，一个真实长度为 x 的间隔会被量成 x 两侧的格点之一（平均值仍是 x）。
      把每一格的样本看成均匀分布在 [格点 − 5 ns, 格点 + 5 ns) 内，再在格内线性插值，
      得到的是"真实分布被 ±10 ns 的对称窗口平滑之后"的分位数，分辨率不再受 10 ns 限制。
      A 和 B 受到的平滑完全相同，所以两者相减是公平的。
    """
    h = np.atleast_2d(h).astype(np.float64)
    cum = np.cumsum(h, axis=1)
    n = cum[:, -1]
    rank = q * n
    idx = (cum >= np.maximum(np.ceil(rank), 1)[:, None]).argmax(axis=1)
    rows = np.arange(len(n))
    below = np.where(idx > 0, cum[rows, np.maximum(idx - 1, 0)], 0.0)
    cnt = cum[rows, idx] - below
    frac = np.clip((rank - below) / np.maximum(cnt, 1), 0, 1)
    return idx * step_ns, (idx - 0.5 + frac) * step_ns


def mean_of(h, step_ns):
    h = np.atleast_2d(h).astype(np.float64)
    return (h @ np.arange(h.shape[1])) / h.sum(axis=1) * step_ns


def tail_of(h, first_bin):
    h = np.atleast_2d(h).astype(np.float64)
    return h[:, first_bin:].sum(axis=1) / h.sum(axis=1)


def resample_blocks(H, reps, rng):
    """分块自助：有放回地抽 nb 个块，返回 reps 行合并后的直方图。"""
    nb = H.shape[0]
    W = rng.multinomial(nb, np.full(nb, 1.0 / nb), size=reps).astype(np.float32)
    return W @ H


def resample_iid(H, reps, rng):
    """假装样本相互独立：按总体直方图的比例重抽同样多的样本。"""
    tot = H.sum(axis=0).astype(np.float64)
    return rng.multinomial(int(tot.sum()), tot / tot.sum(), size=reps).astype(np.float32)


def ci(x):
    lo, hi = np.percentile(x, [2.5, 97.5])
    return [round(float(lo), 2), round(float(hi), 2)]


def stat_table(H, step_ns):
    """点估计：平均值 + 各分位数（格点 / 插值）。"""
    tot = H.sum(axis=0)
    out = {"mean": round(float(mean_of(tot, step_ns)[0]), 2)}
    for name, q in QS:
        g, i = quantiles(tot, q, step_ns)
        out[name] = {"grid": int(round(g[0])), "interp": round(float(i[0]), 2)}
    return out


def boot_stats(Hb, step_ns):
    """对一组重抽出来的直方图求各统计量 → {名字: 长度为 reps 的数组}。"""
    out = {"mean": mean_of(Hb, step_ns)}
    for name, q in QS:
        g, i = quantiles(Hb, q, step_ns)
        out[name + ":grid"], out[name + ":interp"] = g, i
    return out


def compare(HA, HB, step_ns, reps, rng, keep=None):
    """A、B 各自的点估计与区间，以及 A − B 的点估计与区间。"""
    pa, pb = stat_table(HA, step_ns), stat_table(HB, step_ns)
    ba, bb = boot_stats(resample_blocks(HA, reps, rng), step_ns), boot_stats(resample_blocks(HB, reps, rng), step_ns)
    res = {"A": pa, "B": pb, "diff": {}}
    res["A"]["mean_ci"], res["B"]["mean_ci"] = ci(ba["mean"]), ci(bb["mean"])
    d = ba["mean"] - bb["mean"]
    res["diff"]["mean"] = {"value": round(pa["mean"] - pb["mean"], 2), "ci": ci(d), "p_gt0": round(float((d > 0).mean()), 4)}
    boots = {}
    for name, _ in QS:
        for side, p, b in (("A", pa, ba), ("B", pb, bb)):
            res[side][name]["interp_ci"] = ci(b[name + ":interp"])
            res[side][name]["grid_ci"] = ci(b[name + ":grid"])
        dg = ba[name + ":grid"] - bb[name + ":grid"]
        di = ba[name + ":interp"] - bb[name + ":interp"]
        res["diff"][name] = {
            "grid": pa[name]["grid"] - pb[name]["grid"],
            "grid_ci": ci(dg),
            "interp": round(pa[name]["interp"] - pb[name]["interp"], 2),
            "interp_ci": ci(di),
            # 重抽结果里 A − B > 0 的比例：接近 1 → A 确实更慢；接近 0 → A 确实更快；在中间 → 分不出
            "p_gt0": round(float((di > 0).mean()), 4),
        }
        if keep and name in keep:
            boots[name] = np.round(di, 2).tolist()
    return res, boots


def burst_positions(t2, s2):
    """同一次 rx_burst 收到的包共用一个 T2，且样本按 T3 的先后写入，所以"T2 相同的相邻样本"就是同一批，
    在这一段里的序号就是它在批内的位置。返回（批内位置，批大小），都从 1 数起。"""
    n = len(t2)
    new = np.empty(n, dtype=bool)
    new[0] = True
    np.not_equal(t2[1:], t2[:-1], out=new[1:])
    starts = np.flatnonzero(new).astype(np.int32)
    sizes = np.diff(np.append(starts, np.int32(n))).astype(np.int32)
    gid = np.cumsum(new, dtype=np.int32)
    gid -= 1
    pos = np.arange(1, n + 1, dtype=np.int32)
    pos -= starts[gid]
    size = sizes[gid]
    del gid
    # 核对假设：同一批内段②应当随位置递增（后处理的包 T3 更晚）
    same = ~new[1:]
    mono = float((s2[1:][same] >= s2[:-1][same]).mean()) if same.any() else 1.0
    return pos, size, sizes, mono


def analyse_one(path, label):
    hz, s1, s2, t2 = load(path)
    # TSC 读数的步长 = 所有时间差的最大公约数。饱和值（0xFFFF）是截断出来的，不是真实的时间差，要排除
    head = np.concatenate([s1[:2_000_000], s2[:2_000_000]]).astype(np.int64)
    step = max(int(np.gcd.reduce(head[head < 0xFFFF])), 1)
    del head
    step_ns = step * 1e9 / hz
    blk, dur = time_blocks(t2, hz)
    nb = int(blk.max()) + 1
    clipped = int(((s1 == 0xFFFF) | (s2 == 0xFFFF)).sum())
    u1, u2 = s1 // np.uint16(step), s2 // np.uint16(step)
    del s1
    H = {
        "inproc": block_hist(u1 + u2, blk, nb),
        "seg1": block_hist(u1, blk, nb),
        "seg2": block_hist(u2, blk, nb),
    }
    pos, size, sizes, mono = burst_positions(t2, s2)
    del t2, s2
    for k, name in ((1, "pos1"), (2, "pos2"), (3, "pos3")):
        m = pos == k
        H[name] = block_hist(u2[m], blk[m], nb)
    m = pos >= 4
    H["pos4+"] = block_hist(u2[m], blk[m], nb)
    m = size == 1
    H["alone"] = block_hist(u2[m], blk[m], nb)
    H["alone_inproc"] = block_hist((u1 + u2)[m], blk[m], nb)
    bs = np.bincount(np.minimum(sizes, 8), minlength=9)[1:]
    info = {
        "path": path, "samples": int(len(u1)), "tsc_hz": hz, "tsc_step_cycles": step, "tsc_step_ns": round(step_ns, 3),
        "span_sec": round(dur, 2), "clipped_at_25us": clipped,
        "burst_sizes_from_samples": {str(i + 1) if i < 7 else "8+": int(c) for i, c in enumerate(bs)},
        "seg2_nondecreasing_within_burst": round(mono, 6),
    }
    print(f"[{label}] {len(u1):,} 个样本，{dur:.1f} 秒，TSC 步长 {step} 周期 = {step_ns:.2f} ns，"
          f"饱和（≥ 25 µs）{clipped} 个，批内段②递增的比例 {mono:.4%}")
    return info, H, step_ns


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--a", required=True)
    ap.add_argument("--b", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--block-sec", type=float, default=20.0,
                    help="分块自助的块长（秒）。块要长到能把样本之间的相关性包在块内；实测区间随块长变宽，到 20 秒左右才趋于稳定")
    ap.add_argument("--reps", type=int, default=2000, help="重抽次数")
    a = ap.parse_args()
    rng = np.random.default_rng(SEED)

    ia, HA, step_ns = analyse_one(a.a, "A")
    ib, HB, step_b = analyse_one(a.b, "B")
    assert abs(step_ns - step_b) < 1e-9, "A 和 B 的 TSC 步长不同？"
    k = max(1, round(a.block_sec / BASE_SEC))
    A = {n: regroup(h, k) for n, h in HA.items()}
    B = {n: regroup(h, k) for n, h in HB.items()}

    out = {"inputs": {"A": ia, "B": ib}, "block_sec": k * BASE_SEC, "replicates": a.reps, "seed": SEED,
           "tsc_step_ns": round(step_ns, 3), "metrics": {}, "boot": {}}

    # 1. 主指标的置信区间
    for name in ("inproc", "seg1", "seg2"):
        res, boots = compare(A[name], B[name], step_ns, a.reps, rng, keep=("p50", "p99") if name == "inproc" else None)
        out["metrics"][name] = res
        for q, v in boots.items():
            out["boot"][f"{name}_{q}_diff"] = v

    # 2. 块长敏感性 + 与"假装独立"的对比（看进程内 p99 和平均值的 A − B）
    sens = []
    for sec in (0.1, 1.0, 5.0, 20.0, 60.0):
        kk = max(1, round(sec / BASE_SEC))
        ra, rb = regroup(HA["inproc"], kk), regroup(HB["inproc"], kk)
        ba, bb = resample_blocks(ra, a.reps, rng), resample_blocks(rb, a.reps, rng)
        row = {"block_sec": sec, "blocks": int(ra.shape[0]), "mean_diff_ci": ci(mean_of(ba, step_ns) - mean_of(bb, step_ns))}
        for qn, q in (("p50", 0.5), ("p99", 0.99)):
            row[f"{qn}_diff_ci"] = ci(quantiles(ba, q, step_ns)[1] - quantiles(bb, q, step_ns)[1])
        sens.append(row)
    ia_, ib_ = resample_iid(HA["inproc"], a.reps, rng), resample_iid(HB["inproc"], a.reps, rng)
    iid = {"mean_diff_ci": ci(mean_of(ia_, step_ns) - mean_of(ib_, step_ns))}
    for qn, q in (("p50", 0.5), ("p99", 0.99)):
        iid[f"{qn}_diff_ci"] = ci(quantiles(ia_, q, step_ns)[1] - quantiles(ib_, q, step_ns)[1])
    ref = min(sens, key=lambda r: abs(r["block_sec"] - k * BASE_SEC))
    width = lambda c: max(c[1] - c[0], 1e-9)
    n_total = ia["samples"] + ib["samples"]
    out["block_sensitivity"] = sens
    out["iid"] = iid
    out["effective"] = {
        stat: {"design_effect": round((width(ref[stat]) / width(iid[stat])) ** 2, 1),
               "effective_samples": int(n_total / max((width(ref[stat]) / width(iid[stat])) ** 2, 1))}
        for stat in ("mean_diff_ci", "p50_diff_ci", "p99_diff_ci")
    }

    # 3. 段① 落在"悬崖"哪一侧：段① > 125 ns 的样本占比（p99 是不是会翻，就看它在 1% 的哪一边）
    first = int(np.floor(125 / step_ns)) + 1
    ex = {}
    for side, H in (("A", A["seg1"]), ("B", B["seg1"])):
        b = tail_of(resample_blocks(H, a.reps, rng), first) * 100
        ex[side] = {"percent": round(float(tail_of(H.sum(axis=0), first)[0] * 100), 4), "ci": [round(x, 4) for x in ci(b)]}
    out["seg1_slow_fraction"] = {"threshold_ns": first * step_ns, "note": "段① ≥ 这个值的样本占比（%）", **ex}

    # 4. 每秒一个点的时间序列（进程内耗时）
    series = {}
    for side, H in (("A", HA["inproc"]), ("B", HB["inproc"])):
        h1 = regroup(H, round(1.0 / BASE_SEC))
        h1 = h1[h1.sum(axis=1) > 0.5 * np.median(h1.sum(axis=1))]   # 去掉首尾不完整的那一秒
        p50, p99, mean = quantiles(h1, 0.5, step_ns)[1], quantiles(h1, 0.99, step_ns)[1], mean_of(h1, step_ns)
        series[side] = {
            "p50": np.round(p50, 1).tolist(), "p99": np.round(p99, 1).tolist(), "mean": np.round(mean, 1).tolist(),
            "summary": {n: {"min": round(float(v.min()), 1), "median": round(float(np.median(v)), 1),
                            "max": round(float(v.max()), 1), "std": round(float(v.std()), 2)}
                        for n, v in (("p50", p50), ("p99", p99), ("mean", mean))},
        }
    out["series"] = series

    # 5. 按批内位置拆开段②
    burst = {}
    for name, label in (("pos1", "批内第 1 个"), ("pos2", "批内第 2 个"), ("pos3", "批内第 3 个"), ("pos4+", "批内第 4 个及以后"),
                        ("alone", "独占一批（批大小 = 1）")):
        res, _ = compare(A[name], B[name], step_ns, a.reps, rng)
        res["label"] = label
        res["share"] = {"A": round(float(A[name].sum() / A["seg2"].sum()), 4), "B": round(float(B[name].sum() / B["seg2"].sum()), 4)}
        burst[name] = res
    res, _ = compare(A["alone_inproc"], B["alone_inproc"], step_ns, a.reps, rng)
    res["label"] = "独占一批的样本：进程内耗时 ①+②"
    burst["alone_inproc"] = res
    out["burst_position"] = burst

    # 6. 画图用的互补累积分布
    cdf = {}
    for name in ("inproc", "seg1", "seg2"):
        cdf[name] = {}
        for side, H in (("A", A[name]), ("B", B[name])):
            tot = H.sum(axis=0).astype(np.float64)
            ccdf = 1 - np.cumsum(tot) / tot.sum()
            last = int(np.flatnonzero(tot).max())
            cdf[name][side] = {"ns": (np.arange(last + 1) * step_ns).round(1).tolist(), "ccdf": ccdf[: last + 1].tolist(),
                               "pdf": (tot[: last + 1] / tot.sum()).tolist()}
    out["cdf"] = cdf

    with open(a.out, "w") as f:
        json.dump(out, f, ensure_ascii=False)
    print(f"已写 {a.out}")
    m = out["metrics"]["inproc"]["diff"]
    print(f"进程内耗时 A − B：平均 {m['mean']['value']:+.1f} ns {m['mean']['ci']}；"
          + "；".join(f"{q} 格点 {m[q]['grid']:+d} {m[q]['grid_ci']} / 插值 {m[q]['interp']:+.1f} {m[q]['interp_ci']}" for q, _ in QS[:3]))


if __name__ == "__main__":
    main()
