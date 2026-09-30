//! 单线程 executor：任务槽 + 就绪队列 + Waker。

use crate::runtime;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, RawWaker, RawWakerVTable, Waker};

type BoxFuture = Pin<Box<dyn Future<Output = ()>>>;

/// 就绪队列：固定容量的环形数组 + 每个任务一个"已入队"标记（去重）。全部是 `Cell`，没有借用检查开销。
/// 由于去重，队列长度不会超过任务数，所以容量 = 任务槽数就够了，永不溢出。
struct ReadyQueue {
    buf: Box<[Cell<u32>]>,
    queued: Box<[Cell<bool>]>,
    head: Cell<usize>,
    len: Cell<usize>,
}

impl ReadyQueue {
    fn new(cap: usize) -> Self {
        ReadyQueue {
            buf: (0..cap).map(|_| Cell::new(0)).collect(),
            queued: (0..cap).map(|_| Cell::new(false)).collect(),
            head: Cell::new(0),
            len: Cell::new(0),
        }
    }

    #[inline]
    fn push(&self, idx: u32) {
        let q = &self.queued[idx as usize];
        if q.get() {
            return; // 已在队列里：同一个任务被唤醒多次只 poll 一次
        }
        q.set(true);
        let mut pos = self.head.get() + self.len.get();
        if pos >= self.buf.len() {
            pos -= self.buf.len();
        }
        self.buf[pos].set(idx);
        self.len.set(self.len.get() + 1);
    }

    /// 出队，并**在 poll 之前**清掉"已入队"标记：这样 poll 期间发生的 wake 仍能让它重新入队（否则会丢唤醒）。
    #[inline]
    fn pop(&self) -> Option<u32> {
        if self.len.get() == 0 {
            return None;
        }
        let h = self.head.get();
        let idx = self.buf[h].get();
        self.head.set(if h + 1 == self.buf.len() { 0 } else { h + 1 });
        self.len.set(self.len.get() - 1);
        self.queued[idx as usize].set(false);
        Some(idx)
    }
}

/// 一个任务槽。槽的数组容量固定、永不扩容，所以任务的 future 地址稳定（满足 Pin 的要求之外也不必担心重分配）。
struct TaskSlot {
    fut: RefCell<Option<BoxFuture>>,
    /// 代数：任务结束、槽被复用时 +1。旧任务的 waker 带着旧代数，唤醒时会被识别并忽略（避免叫醒别人）。
    gen: Cell<u16>,
}

pub(crate) struct Executor {
    rt_id: u16,
    slots: Box<[TaskSlot]>,
    free: RefCell<Vec<u32>>,
    ready: ReadyQueue,
    live: Cell<usize>,
}

impl Executor {
    pub(crate) fn new(rt_id: u16, capacity: usize) -> Executor {
        assert!(capacity > 0 && capacity <= u32::MAX as usize);
        Executor {
            rt_id,
            slots: (0..capacity).map(|_| TaskSlot { fut: RefCell::new(None), gen: Cell::new(0) }).collect(),
            free: RefCell::new((0..capacity as u32).rev().collect()),
            ready: ReadyQueue::new(capacity),
            live: Cell::new(0),
        }
    }

    pub(crate) fn spawn_boxed(&self, fut: BoxFuture) {
        let idx = self.free.borrow_mut().pop().expect("任务槽已满（RuntimeConfig::max_tasks）");
        *self.slots[idx as usize].fut.borrow_mut() = Some(fut);
        self.live.set(self.live.get() + 1);
        self.ready.push(idx); // 新任务先入队，等第一次 poll
    }

    /// 还没结束的任务数。
    pub(crate) fn live(&self) -> usize {
        self.live.get()
    }

    /// 就绪队列是否非空。
    pub(crate) fn has_ready(&self) -> bool {
        self.ready.len.get() > 0
    }

    /// 把就绪队列跑空：出队 → poll → Ready 则释放任务槽。
    #[inline]
    pub(crate) fn run_ready(&self) {
        while let Some(idx) = self.ready.pop() {
            let slot = &self.slots[idx as usize];
            let waker = make_waker(self.rt_id, slot.gen.get(), idx);
            let mut cx = Context::from_waker(&waker);
            // 在 poll 期间持有本槽的借用：task 自己能触发的动作（wake、spawn 到别的槽、timer、mailbox）
            // 都不会访问本槽的 fut，所以不会与这个借用冲突。
            let mut fut = slot.fut.borrow_mut();
            #[cfg(feature = "probe")]
            crate::probe::set_poll_start(dpdk::tsc::rdtsc());
            let done = match fut.as_mut() {
                Some(f) => f.as_mut().poll(&mut cx).is_ready(),
                None => false, // 过期的入队（任务已结束），忽略
            };
            if done {
                *fut = None; // drop 掉 future：它持有的 mbuf 等资源在这里归还
                drop(fut);
                slot.gen.set(slot.gen.get().wrapping_add(1));
                self.free.borrow_mut().push(idx);
                self.live.set(self.live.get() - 1);
            }
        }
    }

    /// 丢弃所有未完成的任务（关停时释放它们持有的 mbuf）。
    pub(crate) fn drop_all(&self) {
        for (i, slot) in self.slots.iter().enumerate() {
            let f = slot.fut.borrow_mut().take();
            if f.is_some() {
                drop(f);
                slot.gen.set(slot.gen.get().wrapping_add(1));
                self.free.borrow_mut().push(i as u32);
                self.live.set(self.live.get() - 1);
            }
        }
        while self.ready.pop().is_some() {}
    }

    #[inline]
    fn wake(&self, gen: u16, idx: u32) {
        if let Some(slot) = self.slots.get(idx as usize) {
            if slot.gen.get() == gen {
                self.ready.push(idx);
            }
        }
    }
}

/// 在当前 runtime 上 spawn 一个任务（只能在 runtime 线程上、task 内部调用）。
pub fn spawn(fut: impl Future<Output = ()> + 'static) {
    runtime::with_core(|c| c.exec.spawn_boxed(Box::pin(fut)));
}

// ---------------------------------------------------------------------------
// Waker
//
// data = rt_id(16) | gen(16) | idx(32)，打包进指针宽度的整数；从不被解引用。
// clone / drop 是空操作：没有引用计数，也就没有原子操作。
//
// std 规定 Waker 是 Send + Sync，理论上可能被送到别的线程调用。为保证内存安全：
// wake 时通过线程局部变量找到"当前线程上正在运行的 runtime"，只有 rt_id 匹配才会碰就绪队列；
// 在别的线程上被调用（那里没有这个 runtime）→ 直接 abort，绝不访问非线程安全的数据。
// ---------------------------------------------------------------------------

static VTABLE: RawWakerVTable = RawWakerVTable::new(waker_clone, waker_wake, waker_wake, waker_drop);

#[inline]
fn make_waker(rt_id: u16, gen: u16, idx: u32) -> Waker {
    let data = ((rt_id as usize) << 48) | ((gen as usize) << 32) | idx as usize;
    // SAFETY: vtable 中的函数只把 data 当整数使用、从不解引用，且满足 RawWaker 的其余约定（见上）。
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::without_provenance(data), &VTABLE)) }
}

unsafe fn waker_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &VTABLE)
}

unsafe fn waker_wake(data: *const ()) {
    let d = data.addr();
    let (rt_id, gen, idx) = ((d >> 48) as u16, (d >> 32) as u16, d as u32);
    let handled = runtime::try_with_core(|c| {
        if c.exec.rt_id == rt_id {
            c.exec.wake(gen, idx);
            true
        } else {
            false
        }
    });
    match handled {
        Some(true) => {}
        Some(false) => foreign_wake("另一个 runtime"),
        None if runtime::ran_on_this_thread(rt_id) => {} // runtime 已在本线程结束（关停阶段）：无害地忽略
        None => foreign_wake("别的线程"),
    }
}

unsafe fn waker_drop(_: *const ()) {}

#[cold]
fn foreign_wake(where_: &str) -> ! {
    eprintln!("rt: 在{where_}上调用了本 runtime 的 Waker；单线程 runtime 不支持跨线程唤醒，abort 以避免数据竞争");
    std::process::abort()
}
