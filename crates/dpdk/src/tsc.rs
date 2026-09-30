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
