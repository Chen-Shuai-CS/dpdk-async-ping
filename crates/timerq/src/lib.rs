//! 按 TSC deadline 排序的最小堆（纯数据结构，不含任何调度逻辑）。

use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// 按 TSC deadline 排序的最小堆，payload 是一个 u32（A 里是 timer 槽位号，B 里是 session 号）。
///
/// 选二叉堆而不是时间轮：本题只有约 64 个 timer，插入 O(log 64) ≈ 6 次比较，且精确到 TSC；
/// 时间轮的 O(1) 优势要在成千上万个 timer 时才体现，还要引入槽粒度带来的误差。
pub struct TimerHeap {
    heap: BinaryHeap<Reverse<(u64, u32)>>,
}

impl TimerHeap {
    pub fn with_capacity(n: usize) -> Self {
        TimerHeap { heap: BinaryHeap::with_capacity(n) }
    }

    #[inline]
    pub fn push(&mut self, deadline: u64, payload: u32) {
        self.heap.push(Reverse((deadline, payload)));
    }

    /// 最早的 deadline（O(1)）。
    #[inline]
    pub fn next_deadline(&self) -> Option<u64> {
        self.heap.peek().map(|Reverse((d, _))| *d)
    }

    /// 若最早的 timer 已到期（deadline ≤ now）则弹出它。
    #[inline]
    pub fn pop_expired(&mut self, now: u64) -> Option<(u64, u32)> {
        match self.heap.peek() {
            Some(Reverse((d, _))) if *d <= now => self.heap.pop().map(|Reverse(x)| x),
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pops_in_deadline_order() {
        let mut h = TimerHeap::with_capacity(8);
        for (d, p) in [(30, 3), (10, 1), (20, 2)] {
            h.push(d, p);
        }
        assert_eq!(h.next_deadline(), Some(10));
        assert_eq!(h.pop_expired(5), None);
        assert_eq!(h.pop_expired(25), Some((10, 1)));
        assert_eq!(h.pop_expired(25), Some((20, 2)));
        assert_eq!(h.pop_expired(25), None);
        assert_eq!(h.len(), 1);
    }
}
