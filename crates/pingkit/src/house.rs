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

/// 两边共用的维护动作。
#[inline(never)]
pub fn maintain(eal: &Eal, port: &Port, stats: &Stats, live: &Live) {
    eal.timer_manage();
    let _ = port.tx_done_cleanup(0);
    live.publish(stats);
}
