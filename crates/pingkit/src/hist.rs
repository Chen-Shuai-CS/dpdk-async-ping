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
}
