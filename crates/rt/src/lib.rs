//! # rt：单核、kernel-bypass 的 async runtime
//!
//! 一个包从网卡到 task 的路径（SPEC §1 的核心问题）：
//!
//! ```text
//!  NIC ──DMA──▶ RX 环 ──rx_burst──▶ [reactor] ──Driver::on_packet──▶ Mailbox::put ──wake──▶ [ready queue]
//!                        (T2)        runtime 主循环     应用提供的分类器      runtime 原语      runtime
//!                                                                                            │
//!  session task ◀── Future::poll 返回 Ready ◀── executor 出队 & poll ◀──────────────────────┘
//!     (T3)
//! ```
//!
//! runtime 提供的东西：
//! - **executor**（[`executor`]）：固定容量的任务槽 + 环形就绪队列；单线程、不做 work stealing。
//! - **Waker**：data 里编码 (runtime id, 代数, 任务号)，clone / drop 都是空操作，
//!   wake = 把任务号推进就绪队列。不在 runtime 线程上被调用时直接 abort，保证不会发生数据竞争。
//! - **timer**（[`timer`]）：TSC deadline 的最小堆；[`sleep`] 返回 deadline 与"timer 发现到期"的时刻，
//!   用来统计 sleep 误差和段③。
//! - **poll-mode reactor**（[`Runtime::run`]）：主循环里调用 rx_burst，把每个包交给应用提供的 [`Driver`]；
//!   **每分发一个包就立刻跑一遍就绪队列**，让被唤醒的 task 马上恢复，而不是等整批包分发完。
//! - **Mailbox**（[`sync::Mailbox`]）：reactor 与 task 之间交接 mbuf 所有权的单槽信箱。
//!
//! 不提供：跨核调度、IO 以外的阻塞操作、通用网络协议栈。

pub mod executor;
#[cfg(feature = "probe")]
pub mod probe;
pub mod runtime;
pub mod sync;
pub mod timer;

pub use executor::spawn;
pub use runtime::{with_port, Driver, RunExit, Runtime, RuntimeConfig};
pub use timer::{sleep, sleep_until, SleepInfo};
