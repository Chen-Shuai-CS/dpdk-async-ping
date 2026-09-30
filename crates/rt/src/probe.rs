//! 诊断（`probe` 特性）：记录 executor 最近一次开始 poll 任务的 TSC，
//! 让应用把段②拆成"分类+投递+wake / 回到 executor+出队 / poll 到恢复"三个子段。默认不编译。

use std::cell::Cell;

thread_local! {
    static POLL_START: Cell<u64> = const { Cell::new(0) };
}

#[inline(always)]
pub(crate) fn set_poll_start(t: u64) {
    POLL_START.with(|c| c.set(t));
}

/// 最近一次 executor 开始 poll 任务的时刻。
#[inline(always)]
pub fn poll_start() -> u64 {
    POLL_START.with(|c| c.get())
}
