//! 对数-线性直方图（HdrHistogram 的简化版），记录 TSC 周期数。
//!
//! - 小于 256 的值精确记录（本机 256 周期 ≈ 98 ns）；
//! - 更大的值每个 2 的幂区间分 128 个桶，相对误差 < 0.8%；
//! - 记录一次：一次 lzcnt + 几次移位/加法 + 一次自增，在 T3 之后才记录，不进入被测段；
//! - 计数用 u64：单个桶永远不会溢出（u32 在 9 万样本/秒下约 66 小时就可能溢出）。每个直方图约 36 KB。

const P: u32 = 8; // 精度位数
const EXACT: u64 = 1 << P; // 256
const HALF: u64 = EXACT / 2; // 128
const MAX_MSB: u32 = 40; // 2^40 周期 ≈ 423 s，更大的值夹到最后一个桶
const BUCKETS: usize = EXACT as usize + (MAX_MSB - P + 1) as usize * HALF as usize;

#[derive(Clone)]
pub struct Hist {
    counts: Box<[u64; BUCKETS]>,
    n: u64,
    min: u64,
    max: u64,
    sum: u128,
}

#[inline]
fn index(v: u64) -> usize {
    if v < EXACT {
        return v as usize;
    }
    let msb = (63 - v.leading_zeros()).min(MAX_MSB);
    let shift = msb - P + 1; // ≥ 1
    let top = (v >> shift).min(EXACT - 1); // ∈ [128, 256)
    (EXACT + (shift as u64 - 1) * HALF + (top - HALF)) as usize
}

/// 桶 i 覆盖的区间 [lo, hi)。
fn bounds(i: usize) -> (u64, u64) {
    let i = i as u64;
    if i < EXACT {
        return (i, i + 1);
    }
    let shift = (i - EXACT) / HALF + 1;
    let top = (i - EXACT) % HALF + HALF;
    (top << shift, (top + 1) << shift)
}

impl Default for Hist {
    fn default() -> Self {
        Hist { counts: Box::new([0; BUCKETS]), n: 0, min: u64::MAX, max: 0, sum: 0 }
    }
}

impl Hist {
    #[inline]
    pub fn record(&mut self, v: u64) {
        self.counts[index(v)] += 1;
        self.n += 1;
        self.sum += v as u128;
        if v < self.min {
            self.min = v;
        }
        if v > self.max {
            self.max = v;
        }
    }

    pub fn count(&self) -> u64 {
        self.n
    }

    pub fn min(&self) -> u64 {
        if self.n == 0 { 0 } else { self.min }
    }

    pub fn max(&self) -> u64 {
        self.max
    }

    pub fn mean(&self) -> f64 {
        if self.n == 0 { 0.0 } else { self.sum as f64 / self.n as f64 }
    }

    /// 分位数（q ∈ [0,1]）。返回所在桶的中点（精确区间内即精确值），并夹在 [min, max] 内。
    pub fn quantile(&self, q: f64) -> u64 {
        if self.n == 0 {
            return 0;
        }
        let rank = ((q * self.n as f64).ceil() as u64).clamp(1, self.n);
        let mut acc = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            acc += c;
            if acc >= rank {
                let (lo, hi) = bounds(i);
                let mid = if hi - lo == 1 { lo } else { lo + (hi - lo) / 2 };
                return mid.clamp(self.min, self.max);
            }
        }
        self.max
    }

    /// 插值分位数（返回值单位仍是 TSC 周期，但带小数）。`step` = TSC 读数的步长（见 `dpdk::tsc::ClockCost::step`）。
    ///
    /// 本机的 TSC 每 10 ns 才跳一步（步长 26 周期），所以任何时间差都只能是 10 ns 的整数倍：
    /// 一个真实长度为 x 的间隔会被量成 x 两侧的格点之一（平均值仍是 x）。普通分位数因此只能落在格点上，
    /// A、B 两个分位数相减也只能得到 10 ns 的整数倍，分不清"差 2 ns"和"差 9 ns"。
    ///
    /// 估计方法分两种情况，取决于目标分位数落在哪种桶里：
    /// - **窄桶（桶宽 ≤ step，本机即 4096 周期 ≈ 1.6 µs 以内的值）**：一个桶里至多有一个时钟格点。
    ///   用分组数据的标准做法：把每一格的样本看成均匀分布在 [格点 − step/2, 格点 + step/2) 内，再在格内线性插值。
    ///   得到的是"真实分布被 ±1 格的对称窗口抹平之后"的分位数：分布平缓处误差远小于一格，紧贴硬边界处可到半格。
    ///   进程内耗时、段①、段②的 p50 ~ p99.9 都在这个范围里，排名指标用的就是这种情况。
    /// - **宽桶（桶宽 > step）**：一个桶里有好几个格点，直方图已经分不清它们，"按格点插值"的前提不成立。
    ///   这时只在桶 [lo, hi) 内线性插值，并夹在 [min, max] 内——精度就是桶宽（相对 < 0.8%），不比普通分位数更准。
    ///   端到端延迟（几十到几百微秒）属于这种情况。
    ///
    /// 这是模型估计，小数位数不代表测量精度。A 和 B 用的是同一把尺子，但两边分布的形状不同时，
    /// 插值的偏差不保证完全抵消（硬边界附近最多半格），所以报告里同时给出格点值和区间。
    pub fn quantile_interp(&self, q: f64, step: u64) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        let step = step.max(1);
        let rank = q * self.n as f64;
        let target = (rank.ceil() as u64).clamp(1, self.n);
        /// 累计计数时的一"格"：窄桶按时钟格点归并（相邻几个桶可能属于同一格），宽桶自成一格。
        #[derive(Clone, Copy, PartialEq)]
        enum Cell {
            Grid(u64),
            Bucket(usize),
        }
        let (mut below, mut cell, mut in_cell) = (0u64, Cell::Grid(0), 0u64);
        for (i, &c) in self.counts.iter().enumerate() {
            if c == 0 {
                continue;
            }
            let (lo, hi) = bounds(i);
            // 窄桶里那个格点值：实测值都是 step 的整数倍，桶宽不超过 step 时一个桶里至多有一个，就是不超过桶上界的最大那个整数倍
            let this = if hi - lo <= step { Cell::Grid((hi - 1) / step) } else { Cell::Bucket(i) };
            if this != cell {
                if below + in_cell >= target {
                    break;
                }
                below += in_cell;
                (cell, in_cell) = (this, 0);
            }
            in_cell += c;
        }
        let frac = ((rank - below as f64) / in_cell as f64).clamp(0.0, 1.0);
        match cell {
            Cell::Grid(g) => (g as f64 - 0.5 + frac) * step as f64,
            Cell::Bucket(i) => {
                let (lo, hi) = bounds(i);
                (lo as f64 + frac * (hi - lo) as f64).clamp(self.min as f64, self.max as f64)
            }
        }
    }

    /// 取值 ≥ `v` 的样本占比（`v` 落在桶中间时按桶的下界算）。
    pub fn fraction_at_or_above(&self, v: u64) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        let n: u64 = self.counts.iter().enumerate().filter(|(i, _)| bounds(*i).0 >= v).map(|(_, c)| c).sum();
        n as f64 / self.n as f64
    }

    /// 合并另一个直方图（多轮结果先合并再求分位数，而不是对分位数求平均）。
    pub fn merge(&mut self, o: &Hist) {
        for (a, b) in self.counts.iter_mut().zip(o.counts.iter()) {
            *a += *b;
        }
        self.n += o.n;
        self.sum += o.sum;
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_and_bounds_are_consistent() {
        for v in (0..5000u64).chain([1 << 20, 123_456_789, 2_600_000_000, u64::MAX / 4]) {
            let (lo, hi) = bounds(index(v));
            let vv = v.min((1u64 << (MAX_MSB + 1)) - 1);
            assert!(lo <= vv && vv < hi, "v={v} idx={} [{lo},{hi})", index(v));
            if (EXACT..(1 << MAX_MSB)).contains(&v) {
                assert!(((hi - lo) as f64) / (v as f64) <= 1.0 / 128.0 + 1e-12);
            }
        }
        assert!(index(u64::MAX) < BUCKETS);
    }

    #[test]
    fn quantiles_of_uniform_data() {
        let mut h = Hist::default();
        for v in 1..=100_000u64 {
            h.record(v);
        }
        for (q, want) in [(0.5, 50_000.0), (0.99, 99_000.0), (0.9999, 99_990.0)] {
            let got = h.quantile(q) as f64;
            assert!((got - want).abs() / want < 0.01, "q={q} got={got} want={want}");
        }
        assert_eq!(h.quantile(1.0), 100_000);
        assert_eq!(h.min(), 1);
    }

    /// 模拟本机的时钟：真实间隔连续分布，但两端的时间戳都只能取 26 周期（10 ns）的整数倍。
    /// 普通分位数只能落在格点上；插值分位数应当把真实分位数还原到远小于一格的精度。
    #[test]
    fn interpolated_quantiles_recover_sub_step_resolution() {
        const STEP: u64 = 26;
        let mut h = Hist::default();
        let mut truth = Vec::new();
        let mut x = 0x9E37_79B9_7F4A_7C15u64; // xorshift：测试不依赖外部随机数库
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..400_000 {
            let len = 300 + rnd() % 400; // 真实间隔：300..700 周期，均匀
            let start = rnd() % 1_000_000; // 相位随机
            let measured = (start + len) / STEP * STEP - start / STEP * STEP;
            h.record(measured);
            truth.push(len);
        }
        truth.sort_unstable();
        // 分布内部：误差远小于一格。紧贴分布的硬边界（这里的 q = 0.99，真实分布在 700 处戛然而止）：
        // 量化相当于把分布抹平了约 ±1 格，边界被抹宽，误差可到半格——这是这把尺子的固有极限，如实写在测试里。
        for (q, tolerance) in [(0.1, 4.0), (0.5, 4.0), (0.9, 4.0), (0.99, STEP as f64 / 2.0)] {
            let want = truth[(q * truth.len() as f64) as usize] as f64;
            let grid = h.quantile(q);
            let interp = h.quantile_interp(q, STEP);
            let off = grid % STEP;
            assert!(off.min(STEP - off) <= 2, "普通分位数应当落在格点附近（最多偏到 4 周期宽的桶的中点）：{grid}");
            assert!((interp - want).abs() < tolerance, "q={q} 插值 {interp:.1} 真值 {want}（一格 = {STEP}）");
        }
        // 同一把尺子量两个只差 5 个周期（约 2 ns）的分布：格点分位数分不出，插值分位数分得出
        let mut h2 = Hist::default();
        for &len in &truth {
            let start = rnd() % 1_000_000;
            h2.record((start + len + 5) / STEP * STEP - start / STEP * STEP);
        }
        let d = h2.quantile_interp(0.5, STEP) - h.quantile_interp(0.5, STEP);
        assert!((d - 5.0).abs() < 2.5, "两个分布的中位数相差 5 周期，插值分位数之差为 {d:.2}");
    }

    /// 审查意见 R6：宽桶（桶宽 > 时钟步长）里不能按格点插值。原来的公式把整个桶映射到桶上界附近的一个格点，
    /// 对"所有样本都是 262,158 周期"会给出 264,186（偏高 2028 周期 ≈ 780 ns）。
    #[test]
    fn interpolation_in_wide_buckets_stays_inside_the_bucket() {
        const STEP: u64 = 26;
        // 单值分布：结果就是那个值（桶内插值后夹在 [min, max] 里）
        let mut h = Hist::default();
        for _ in 0..1000 {
            h.record(262_158);
        }
        for q in [0.01, 0.5, 0.99, 1.0] {
            assert_eq!(h.quantile_interp(q, STEP), 262_158.0, "q={q}");
        }
        // 一个宽桶里均匀铺开的值：各分位数落在桶内相应的位置，误差不超过桶宽
        let (lo, hi) = bounds(index(262_158));
        assert!(hi - lo > STEP, "这个测试要的是宽桶");
        let mut h = Hist::default();
        for v in lo..hi {
            h.record(v);
        }
        for q in [0.1, 0.5, 0.9] {
            let want = lo as f64 + q * (hi - lo) as f64;
            let got = h.quantile_interp(q, STEP);
            assert!((got - want).abs() <= 1.0, "q={q} got={got} want={want}");
            assert!(got >= lo as f64 && got <= hi as f64);
        }
    }

    /// 窄桶与宽桶的分界（桶宽从 16 变成 32 周期的地方是 4096）：两侧各用各的估计方法，结果都不离谱。
    #[test]
    fn interpolation_across_the_narrow_wide_boundary() {
        const STEP: u64 = 26;
        assert!(bounds(index(4095)).1 - bounds(index(4095)).0 <= STEP);
        assert!(bounds(index(4096)).1 - bounds(index(4096)).0 > STEP);
        let mut h = Hist::default();
        // 两个格点值：4082 = 157 × 26（窄桶一侧），4108 = 158 × 26（宽桶一侧），各一半
        for _ in 0..500 {
            h.record(4082);
            h.record(4108);
        }
        let (p25, p75) = (h.quantile_interp(0.25, STEP), h.quantile_interp(0.75, STEP));
        assert!((p25 - 4082.0).abs() <= STEP as f64 / 2.0, "p25={p25}");
        let (lo, hi) = bounds(index(4108));
        assert!(p75 >= lo as f64 && p75 <= hi as f64 && p75 <= 4108.0, "p75={p75} 应在 [{lo},{hi}) 内且不超过最大值");
        assert!(p25 < p75);
    }

    /// 长尾混合分布：主体在窄桶区（几百周期），尾巴在宽桶区（几十万周期）。
    /// 主体的分位数与没有尾巴时完全相同；尾部的分位数落在尾巴所在的桶里。
    #[test]
    fn interpolation_of_a_long_tailed_mixture() {
        const STEP: u64 = 26;
        let (mut body, mut mixed) = (Hist::default(), Hist::default());
        for k in 0..10_000u64 {
            let v = (12 + k % 8) * STEP; // 312 ~ 494 周期，都是格点
            body.record(v);
            mixed.record(v);
        }
        for _ in 0..10 {
            mixed.record(300_014); // 尾巴：约 115 µs
        }
        // 10 个尾部样本占 0.1%，对 p50 的秩几乎没有影响：结果相差远小于一格
        assert!((mixed.quantile_interp(0.5, STEP) - body.quantile_interp(0.5, STEP)).abs() < 1.0);
        let tail = mixed.quantile_interp(0.9999, STEP);
        let (lo, hi) = bounds(index(300_014));
        assert!(tail >= lo as f64 && tail <= hi as f64, "尾部分位数 {tail} 应在 [{lo},{hi}) 内");
        assert!(tail <= 300_014.0, "不超过实际的最大值");
    }

    #[test]
    fn fraction_at_or_above_counts_the_tail() {
        let mut h = Hist::default();
        for v in [10, 20, 30, 40, 50, 60, 70, 80, 90, 100] {
            h.record(v);
        }
        assert_eq!(h.fraction_at_or_above(71), 0.3);
        assert_eq!(h.fraction_at_or_above(0), 1.0);
        assert_eq!(h.fraction_at_or_above(101), 0.0);
        assert_eq!(Hist::default().fraction_at_or_above(1), 0.0);
    }
}
