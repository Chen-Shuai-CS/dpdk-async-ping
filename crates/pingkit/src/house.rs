use crate::live::Live;
use crate::stats::Stats;
use dpdk::{Eal, Port};

/// 周期性维护的节拍器。A 和 B 用同样的周期、做同样的事：
/// - `rte_timer_manage()`：驱动 ENA 驱动的 watchdog（保活超时、TX 完成丢失检测）
/// - `tx_done_cleanup`：在空闲时回收已发送完成的 TX mbuf，避免回收工作落进 tx_burst（段①）
/// - 把计数器发布给上报线程
/// - 超时扫描、停止判定（由调用方在 `due()` 返回 true 时顺带做）
pub struct House {
    period: u64,
    next: u64,
}

impl House {
    pub fn new(period_cycles: u64, now: u64) -> House {
        House { period: period_cycles, next: now + period_cycles }
    }

    #[inline(always)]
    pub fn due(&mut self, now: u64) -> bool {
        if now >= self.next {
            self.next = now + self.period;
            true
        } else {
            false
        }
    }
}

/// 运行的"起点"定在现在之后这么久：让创建 runtime、分配直方图、spawn 任务等准备工作都在起点之前完成。
/// 否则各 session 的初始相位 deadline 在主循环开始转之前就已经过去，
/// 第一次 sleep 会被记成"迟到了上百微秒"，污染 sleep 误差的最大值（实测踩过这个坑）。
pub const START_LEAD_NS: u64 = 1_000_000;

/// 两边共用的维护动作。顺便给自己计时（每 100 µs 才多两次读时钟）：
/// 维护期间主循环不收包、不查 timer，它的耗时直接决定 sleep 误差和"包在 RX 环里等待"的上限。
#[inline(never)]
pub fn maintain(eal: &Eal, port: &Port, stats: &mut Stats, live: &Live) {
    let t0 = dpdk::tsc::rdtsc();
    eal.timer_manage();
    let t1 = dpdk::tsc::rdtsc();
    let _ = port.tx_done_cleanup(0);
    live.publish(stats);
    let t2 = dpdk::tsc::rdtsc();
    stats.house_timer.record(t1 - t0);
    stats.house_total.record(t2 - t0);
}
