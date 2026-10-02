//! TSC 时间戳。本机 TSC 为 invariant、2.6 GHz；换算一律用 [`hz`]，不要用核心频率。

/// 读 TSC。A 和 B 的所有打点都用这一个函数。
///
/// 用 `rdtscp` 而不是 `rdtsc`：rdtsc 不是序列化指令，乱序执行时可能在它**前面**的访存
/// （比如读刚被网卡 DMA 进来、还不在缓存里的包数据）完成之前就被执行，
/// 导致"前面的代码越短，被藏掉的时间越多"——这会系统性地偏向 B（T2→T3 之间代码短）。
/// rdtscp 会等前面所有指令执行完、所有读都完成后才读 TSC。代价：约 16 ns / 次（rdtsc 约 8 ns），两边相同。
#[inline(always)]
pub fn rdtsc() -> u64 {
    let mut aux = 0u32;
    // SAFETY: rdtscp 在本机可用（CPU 标志含 rdtscp），只写 aux，无其他副作用。
    unsafe { core::arch::x86_64::__rdtscp(&mut aux) }
}

/// TSC 频率（Hz），由 EAL 启动时校准。
pub fn hz() -> u64 {
    // SAFETY: 纯查询。
    unsafe { dpdk_sys::rte_get_tsc_hz() }
}

/// 把 TSC 差值换算成纳秒。
#[inline]
pub fn cycles_to_ns(cycles: u64, hz: u64) -> u64 {
    ((cycles as u128 * 1_000_000_000) / hz as u128) as u64
}

#[inline]
pub fn ns_to_cycles(ns: u64, hz: u64) -> u64 {
    ((ns as u128 * hz as u128) / 1_000_000_000) as u64
}

/// 诊断用（`--diag-pre-t0 sfence`）：执行一次 `sfence`。
#[inline(always)]
pub fn sfence() {
    // SAFETY: SSE 是 x86_64 的基线指令集；sfence 只约束存储的完成顺序，不读写任何内存。
    unsafe { core::arch::x86_64::_mm_sfence() }
}

/// 诊断用（`--diag-pre-t0 mfence`）：执行一次 `mfence`，等此前所有的写入（包括还在排队的设备写入）真正完成后才返回。
#[inline(always)]
pub fn mfence() {
    // SAFETY: SSE2 是 x86_64 的基线指令集；mfence 只约束访存的完成顺序，不读写任何内存。
    unsafe { core::arch::x86_64::_mm_mfence() }
}

/// 提前把 `p` 所在的缓存行以"可写"状态取进缓存（`prefetchw`）。只是一个提示：不读写内存，地址无效也不会出错。
///
/// 用在"过一会儿才会写到、但写的时候不能等"的地方（`pingkit::samples`）：一次要等内存的写入会挂在 CPU 的写入队列里，
/// 拖慢紧随其后的发送（见 README §5.1 第 15 项）。
#[inline(always)]
pub fn prefetch_write<T>(p: *const T) {
    // SAFETY: SSE 是 x86_64 的基线指令集；预取指令不解引用指针，对任何地址（包括越界、未映射的地址）都不会产生异常。
    unsafe { core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_ET0 }>(p as *const i8) }
}

/// "读一次时钟"的标定结果，单位 TSC 周期。
#[derive(Debug, Clone, Copy)]
pub struct ClockCost {
    /// 相邻两次读数之差的最小值、中位数
    pub min: u64,
    pub median: u64,
    /// 平均值（总耗时 ÷ 次数）。读数有步长时，单次差值只能取步长的整数倍，平均值才是真实成本
    pub mean: f64,
    /// 所有差值的最大公约数 = TSC 读数的步长 = 时间戳的分辨率。
    /// 有些 CPU 的 TSC 不是每个周期加 1，而是每隔固定时间跳一步（本机：每 10 ns 跳 26）
    pub step: u64,
}

/// 标定"读一次时钟"本身的成本：连续读 `n + 1` 次，统计相邻读数之差。
/// 每个被测段（T1 − T0、T3 − T2）都恰好包含一次读时钟。冷路径：只在启动时调用一次。
pub fn clock_read_cost(n: usize) -> ClockCost {
    fn gcd(a: u64, b: u64) -> u64 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let mut d = Vec::with_capacity(n);
    let first = rdtsc();
    let mut prev = first;
    for _ in 0..n {
        let now = rdtsc();
        d.push(now - prev);
        prev = now;
    }
    d.sort_unstable();
    ClockCost {
        min: d[0],
        median: d[n / 2],
        mean: (prev - first) as f64 / n as f64,
        step: d.iter().fold(0, |g, &x| gcd(g, x)),
    }
}

/// 主循环停顿检测。A 的 runtime 主循环和 B 的循环用的是同一个检测器，分两类统计：
///
/// 1. **空轮询停顿**（[`tick`](Self::tick)）：这一轮没收到包、上一次读时钟之后也没干活（没有 timer 到期、没做维护），
///    正常只要几十纳秒。ENA 驱动在空轮询里不做任何重活（接收环的回填只发生在收到包的那次调用里），
///    所以间隔超过阈值就说明这段时间我们的代码根本没在运行——被外部打断了（中断、虚拟机宿主机借走 CPU）。
/// 2. **取包前停顿**（[`tick_rx`](Self::tick_rx)）：这一轮收到了包，但从上一次读时钟到 rx_burst 返回隔了很久。
///    长停顿之后往往立刻有包可收，只看第 1 类会把最长的那些停顿漏掉。
///    收一批包（含回填）正常只要几微秒以内，所以这一类用更大的阈值。
#[derive(Debug, Clone, Copy, Default)]
pub struct StallWatch {
    last: u64,
    threshold: u64,
    rx_threshold: u64,
    /// 空轮询停顿：次数 / 累计（TSC 周期）/ 最长
    pub count: u64,
    pub total: u64,
    pub max: u64,
    /// 取包前停顿：次数 / 累计 / 最长
    pub rx_count: u64,
    pub rx_total: u64,
    pub rx_max: u64,
}

impl StallWatch {
    pub fn new(threshold_cycles: u64, rx_threshold_cycles: u64, now: u64) -> StallWatch {
        StallWatch { last: now, threshold: threshold_cycles, rx_threshold: rx_threshold_cycles, ..Default::default() }
    }

    /// 每轮循环读完时钟后调用。`busy` = 上一次调用以来是否干过活（干过活的那一段不算停顿）。
    #[inline(always)]
    pub fn tick(&mut self, now: u64, busy: bool) {
        let gap = now.wrapping_sub(self.last);
        self.last = now;
        if !busy && gap > self.threshold {
            self.note(gap, false);
        }
    }

    /// 收到包的那一轮，用 T2（rx_burst 返回的时刻）调用。可以在处理完这批包之后再调，不必插在 T2 与 T3 之间。
    #[inline(always)]
    pub fn tick_rx(&mut self, t2: u64, busy: bool) {
        let gap = t2.wrapping_sub(self.last);
        self.last = t2;
        if !busy && gap > self.rx_threshold {
            self.note(gap, true);
        }
    }

    #[cold]
    fn note(&mut self, gap: u64, rx: bool) {
        if rx {
            self.rx_count += 1;
            self.rx_total += gap;
            self.rx_max = self.rx_max.max(gap);
        } else {
            self.count += 1;
            self.total += gap;
            self.max = self.max.max(gap);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StallWatch;

    #[test]
    fn clock_read_cost_is_sane() {
        let c = super::clock_read_cost(10_000);
        assert!(c.min <= c.median, "{c:?}");
        assert!(c.step >= 1 && c.min.is_multiple_of(c.step) && c.median.is_multiple_of(c.step), "{c:?}");
        assert!(c.mean > 1.0 && c.mean < 10_000.0, "读一次时钟不该超过几微秒：{c:?}");
    }

    #[test]
    fn stall_watch_separates_idle_and_pre_rx_stalls() {
        let mut w = StallWatch::new(100, 1_000, 0);
        w.tick(50, false); // 空轮询，间隔短：不算
        w.tick(5_000, true); // 间隔长，但这一段在干活：不算
        w.tick(5_600, false); // 空轮询，间隔 600 > 100：算
        w.tick_rx(5_900, false); // 收到包，取包前只隔 300 < 1000：不算
        w.tick(6_500, true); // 处理这批包的时间：不算
        w.tick_rx(9_500, false); // 收到包，取包前隔了 3000 > 1000：算（长停顿后立刻有包）
        w.tick(9_600, true);
        assert_eq!((w.count, w.total, w.max), (1, 600, 600));
        assert_eq!((w.rx_count, w.rx_total, w.rx_max), (1, 3_000, 3_000));
    }
}
