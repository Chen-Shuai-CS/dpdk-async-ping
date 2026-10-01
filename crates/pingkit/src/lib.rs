//! A 和 B 共用的部分。原则：**除了"有没有 runtime"之外，两边执行的是同一份代码**，
//! 这样 A − B 减出来的才是抽象层成本。
//!
//! - [`args`]：命令行参数（两边完全相同）
//! - [`dataplane`]：EAL / mempool / 端口初始化，关停与 mbuf 泄漏核对
//! - [`sender`]：发送一个 echo request（段①：T0 → T1）
//! - `TimerHeap`（来自 `timerq`）：按 TSC deadline 排序的最小堆（A 的 timer 与 B 的 delay 都用它）
//! - [`hist`] / [`stats`]：直方图与报表
//! - [`samples`]：可选的逐样本原始记录（`--samples`），给离线的置信区间分析用
//! - [`envinfo`]：构建信息与运行环境，写进每份报告
//! - [`live`]：跑在另一个核上的进度上报线程
//! - [`house`]：周期性维护（ENA watchdog、TX 回收、超时扫描节拍、停止判定）

pub mod args;
pub mod dataplane;
pub mod envinfo;
pub mod hist;
pub mod house;
pub mod live;
pub mod samples;
pub mod sender;
pub mod stats;

pub use args::{Args, PreT0};
pub use dataplane::Dataplane;
pub use envinfo::EnvInfo;
pub use dpdk::tsc::rdtsc;
pub use sender::{SendError, Sender, Stamp};
pub use stats::Stats;
pub use timerq::TimerHeap;
