//! Runtime 本体：把 executor、timer 和 DPDK 端口粘在一个 busy-poll 主循环里。

use crate::executor::Executor;
use crate::timer::Timers;
use dpdk::tsc::{rdtsc, StallWatch};
use dpdk::{Mbuf, Port, RxBurst};
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::sync::atomic::{AtomicU16, Ordering};

/// 应用提供的"协议侧"：runtime 负责轮询网卡和调度，应用负责理解包的内容。
///
/// 它是泛型参数（不是 trait object），所以调用是静态分发、可内联的。
pub trait Driver {
    /// 一次非空的 rx_burst（统计用）。
    #[inline]
    fn on_burst(&self, _n: usize) {}

    /// 每个收到的包。`t2` 是 rx_burst 返回的时刻（同一批包相同）。
    /// 典型实现：解析 → 按某个 key 找到 [`crate::sync::Mailbox`] → `put`（其中会 wake 等待的 task）。
    fn on_packet(&self, m: Mbuf, t2: u64, port: &Port);

    /// 每个维护节拍调用一次。返回 `false` 表示请求 runtime 立即退出 `run`（例如网卡要求 reset）。
    fn on_tick(&self, now: u64, port: &Port) -> bool;
}

#[derive(Debug, Clone, Copy)]
pub struct RuntimeConfig {
    /// 任务槽容量（固定，不扩容）
    pub max_tasks: usize,
    /// timer 槽的初始容量
    pub max_timers: usize,
    /// 维护节拍（TSC 周期）
    pub tick_cycles: u64,
    /// 主循环停顿的判定阈值（TSC 周期），见 `dpdk::tsc::StallWatch`：
    /// 空轮询超过前者、或收到包那一轮在取包之前超过后者，就记为一次"被外部打断"
    pub stall_threshold_cycles: u64,
    pub stall_rx_threshold_cycles: u64,
}

/// `run` 为什么返回。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunExit {
    /// 所有 task 都已结束
    AllTasksDone,
    /// `Driver::on_tick` 请求立即退出
    Aborted,
}

pub(crate) struct Core {
    pub(crate) exec: Executor,
    pub(crate) timers: RefCell<Timers>,
    port: Cell<*const Port>,
    stalls: Cell<StallWatch>,
}

thread_local! {
    /// 当前线程上正在运行的 runtime（只在 run / drop 期间非空）。
    static CURRENT: Cell<*const Core> = const { Cell::new(std::ptr::null()) };
    /// 本线程最近运行过的 runtime id：用来区分"runtime 已结束后的迟到 wake"（无害）与"跨线程 wake"（abort）。
    static LAST_RT: Cell<u16> = const { Cell::new(0) };
}

static NEXT_RT_ID: AtomicU16 = AtomicU16::new(1);

/// 在当前 runtime 上执行 `f`；不在 runtime 内则返回 None。
#[inline]
pub(crate) fn try_with_core<R>(f: impl FnOnce(&Core) -> R) -> Option<R> {
    let p = CURRENT.with(|c| c.get());
    // SAFETY: CURRENT 只在 `Enter` 守卫存活期间指向一个活着的 Core（Runtime 通过 &self 借出），
    // 而 &Core 的使用被限制在闭包内，不会逃逸。
    if p.is_null() { None } else { Some(f(unsafe { &*p })) }
}

#[inline]
pub(crate) fn with_core<R>(f: impl FnOnce(&Core) -> R) -> R {
    try_with_core(f).expect("只能在 rt runtime 的 task 内调用")
}

pub(crate) fn ran_on_this_thread(rt_id: u16) -> bool {
    LAST_RT.with(|c| c.get()) == rt_id
}

/// 在 task 内访问 runtime 正在驱动的端口（例如发送）。
#[inline]
pub fn with_port<R>(f: impl FnOnce(&Port) -> R) -> R {
    with_core(|c| {
        let p = c.port.get();
        assert!(!p.is_null(), "runtime 当前没有在驱动端口");
        // SAFETY: port 指针只在 `run(&self, port: &Port, ..)` 期间被设置，借用在此期间有效。
        f(unsafe { &*p })
    })
}

/// 设置 / 恢复 CURRENT 的守卫。
struct Enter {
    prev: *const Core,
}

impl Enter {
    fn new(core: &Core, rt_id: u16) -> Enter {
        let prev = CURRENT.with(|c| c.replace(core));
        LAST_RT.with(|c| c.set(rt_id));
        Enter { prev }
    }
}

impl Drop for Enter {
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(self.prev));
    }
}

/// 单线程 runtime。`!Send + !Sync`（内含 Cell / RefCell / 裸指针）：只在创建它的线程上使用。
pub struct Runtime {
    id: u16,
    core: Box<Core>,
    tick: u64,
    stall_threshold: u64,
    stall_rx_threshold: u64,
}

impl Runtime {
    pub fn new(cfg: RuntimeConfig) -> Runtime {
        let id = NEXT_RT_ID.fetch_add(1, Ordering::Relaxed);
        Runtime {
            id,
            core: Box::new(Core {
                exec: Executor::new(id, cfg.max_tasks),
                timers: RefCell::new(Timers::new(cfg.max_timers)),
                port: Cell::new(std::ptr::null()),
                stalls: Cell::new(StallWatch::default()),
            }),
            tick: cfg.tick_cycles,
            stall_threshold: cfg.stall_threshold_cycles,
            stall_rx_threshold: cfg.stall_rx_threshold_cycles,
        }
    }

    /// 在 `run` 之前 spawn 初始任务。
    pub fn spawn(&self, fut: impl Future<Output = ()> + 'static) {
        self.core.exec.spawn_boxed(Box::pin(fut));
    }

    pub fn live_tasks(&self) -> usize {
        self.core.exec.live()
    }

    /// 最近一次 `run` 期间主循环被外部打断的统计（空轮询间隔超过阈值的次数 / 累计 / 最长）。
    pub fn stalls(&self) -> StallWatch {
        self.core.stalls.get()
    }

    /// 主循环（busy-poll，永不睡眠），直到所有 task 结束或 driver 请求退出。
    ///
    /// 每轮：
    /// 1. **reactor**：rx_burst → 对每个包调用 `driver.on_packet`，**紧接着跑一遍就绪队列**，
    ///    让刚被唤醒的 task 立刻恢复（与 B"处理一个包、更新一个状态"的顺序对齐，
    ///    不让同一批里后面的包拖慢前面的包）；
    /// 2. **timer**：用本轮的 now 触发所有到期 timer，再跑就绪队列；
    /// 3. **维护节拍**：`driver.on_tick`，再跑就绪队列；所有 task 结束则返回。
    pub fn run<D: Driver>(&self, port: &Port, driver: &D) -> RunExit {
        let _enter = Enter::new(&self.core, self.id);
        let core: &Core = &self.core;
        core.port.set(port);
        let _clear_port = ClearPort(core);
        let mut burst = RxBurst::new();
        core.exec.run_ready();
        let start = rdtsc();
        let mut next_tick = start + self.tick;
        let mut watch = StallWatch::new(self.stall_threshold, self.stall_rx_threshold, start);
        let mut worked = true; // 上一次读时钟之后是否干过活（触发 timer / 维护）
        loop {
            // 1. poll-mode reactor
            let n = port.rx_burst(&mut burst);
            if n > 0 {
                let t2 = rdtsc(); // T2：rx_burst 返回
                driver.on_burst(n);
                for m in burst.by_ref() {
                    driver.on_packet(m, t2, port);
                    core.exec.run_ready();
                }
                // 停顿检测（取包前）：放在处理完这批包之后，不插在 T2 与 T3 之间
                watch.tick_rx(t2, worked);
                worked = true;
            }
            // 2. timers
            let now = rdtsc(); // "timer 发现到期"的时刻
            // 停顿检测（空轮询）：复用这次时钟读数。上一次读时钟之后什么都没干，间隔却很长 → 被外部打断
            watch.tick(now, worked);
            worked = false;
            let fired = core.timers.borrow_mut().fire(now);
            if fired > 0 {
                worked = true;
                core.exec.run_ready();
            }
            // 3. 维护节拍
            if now >= next_tick {
                next_tick = now + self.tick;
                worked = true;
                if !driver.on_tick(now, port) {
                    core.stalls.set(watch);
                    return RunExit::Aborted;
                }
                core.exec.run_ready();
                if core.exec.live() == 0 {
                    core.stalls.set(watch);
                    return RunExit::AllTasksDone;
                }
            }
        }
    }
}

impl Runtime {
    /// 不驱动网卡，只跑 executor + timer，直到所有 task 结束。
    /// 用于单元测试（不需要 DPDK / 网卡），也适用于纯 timer 的场景。在其中调用 [`with_port`] 会 panic。
    ///
    /// 若所有 task 都在等待、却既没有就绪任务也没有待触发的 timer，则 panic（死锁，永远不会再有人唤醒它们）。
    pub fn run_offline(&self) {
        let _enter = Enter::new(&self.core, self.id);
        let core: &Core = &self.core;
        core.exec.run_ready();
        while core.exec.live() > 0 {
            let fired = core.timers.borrow_mut().fire(rdtsc());
            core.exec.run_ready();
            if fired == 0 && core.timers.borrow().is_empty() && core.exec.live() > 0 && !core.exec.has_ready() {
                panic!("rt::run_offline：{} 个 task 都在等待，但没有待触发的 timer，也没有就绪任务（死锁）", core.exec.live());
            }
        }
    }
}

struct ClearPort<'a>(&'a Core);

impl Drop for ClearPort<'_> {
    fn drop(&mut self) {
        self.0.port.set(std::ptr::null());
    }
}

impl Drop for Runtime {
    /// 丢弃所有未完成的 task（释放它们持有的 mbuf、登记的 timer）。
    fn drop(&mut self) {
        let _enter = Enter::new(&self.core, self.id);
        self.core.exec.drop_all();
    }
}
