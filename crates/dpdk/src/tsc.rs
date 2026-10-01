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
