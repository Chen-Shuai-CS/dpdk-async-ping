//! TSC timer：deadline 小顶堆（`timerq::TimerHeap`）+ 槽位表。
//!
//! 主循环每轮用同一个 `now` 检查堆顶，把所有到期的 timer 标记为已触发并唤醒对应 task。
//! [`sleep`] 醒来时返回 [`SleepInfo`]：deadline 与"timer 发现到期"的时刻，
//! 由调用方据此统计 sleep 误差（发现 − deadline）和段③（发现 → 下一个 T0）。

use crate::runtime;
use dpdk::tsc::rdtsc;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use timerq::TimerHeap;

/// 一次 sleep 的时间信息（TSC 周期）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SleepInfo {
    pub deadline: u64,
    /// timer 发现到期的时刻（主循环本轮读到的 now）。若首次 poll 时已过期，则为那次 poll 的时刻。
    pub fired_at: u64,
}

enum Slot {
    Free,
    Pending(Waker),
    Fired(u64),
    /// Sleep 在触发前被 drop：留在堆里，弹出时再回收（O(1) 取消，不必在堆里查找）
    Cancelled,
}

pub(crate) struct Timers {
    heap: TimerHeap,
    slots: Vec<Slot>,
    free: Vec<u32>,
}

impl Timers {
    pub(crate) fn new(capacity: usize) -> Timers {
        Timers { heap: TimerHeap::with_capacity(capacity), slots: Vec::with_capacity(capacity), free: Vec::new() }
    }

    fn alloc(&mut self, deadline: u64, waker: Waker) -> u32 {
        let idx = match self.free.pop() {
            Some(i) => {
                self.slots[i as usize] = Slot::Pending(waker);
                i
            }
            None => {
                self.slots.push(Slot::Pending(waker));
                (self.slots.len() - 1) as u32
            }
        };
        self.heap.push(deadline, idx);
        idx
    }

    fn release(&mut self, idx: u32) {
        self.slots[idx as usize] = Slot::Free;
        self.free.push(idx);
    }

    /// 触发所有 deadline ≤ now 的 timer。返回唤醒的数量。
    #[inline]
    pub(crate) fn fire(&mut self, now: u64) -> usize {
        let mut n = 0;
        while let Some((_, idx)) = self.heap.pop_expired(now) {
            match std::mem::replace(&mut self.slots[idx as usize], Slot::Fired(now)) {
                Slot::Pending(w) => {
                    w.wake(); // 只会把任务号推进就绪队列，不会重入 Timers
                    n += 1;
                }
                Slot::Cancelled => self.release(idx),
                other => self.slots[idx as usize] = other, // 不应发生
            }
        }
        n
    }
}

/// 睡到 `now + cycles`（TSC 周期）。
pub fn sleep(cycles: u64) -> Sleep {
    Sleep { deadline: rdtsc() + cycles, slot: None }
}

/// 睡到绝对时刻 `deadline`（TSC）。
pub fn sleep_until(deadline: u64) -> Sleep {
    Sleep { deadline, slot: None }
}

pub struct Sleep {
    deadline: u64,
    slot: Option<u32>,
}

impl Future for Sleep {
    type Output = SleepInfo;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<SleepInfo> {
        let deadline = self.deadline;
        let slot = self.slot;
        let (ready, new_slot) = runtime::with_core(|c| {
            let mut t = c.timers.borrow_mut();
            match slot {
                None => {
                    let now = rdtsc();
                    if now >= deadline {
                        (Some(now), None)
                    } else {
                        (None, Some(t.alloc(deadline, cx.waker().clone())))
                    }
                }
                Some(idx) => match &mut t.slots[idx as usize] {
                    Slot::Fired(at) => {
                        let at = *at;
                        t.release(idx);
                        (Some(at), None)
                    }
                    Slot::Pending(w) => {
                        if !w.will_wake(cx.waker()) {
                            *w = cx.waker().clone();
                        }
                        (None, Some(idx))
                    }
                    _ => unreachable!("Sleep 的槽位状态不一致"),
                },
            }
        });
        self.slot = new_slot;
        match ready {
            Some(fired_at) => Poll::Ready(SleepInfo { deadline, fired_at }),
            None => Poll::Pending,
        }
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        if let Some(idx) = self.slot.take() {
            runtime::try_with_core(|c| {
                let mut t = c.timers.borrow_mut();
                match t.slots[idx as usize] {
                    Slot::Pending(_) => t.slots[idx as usize] = Slot::Cancelled,
                    Slot::Fired(_) => t.release(idx),
                    _ => {}
                }
            });
        }
    }
}
