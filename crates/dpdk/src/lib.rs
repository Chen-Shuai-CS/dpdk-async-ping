//! DPDK 的安全封装（只覆盖本项目用到的部分）。
//!
//! 所有 `unsafe` 都集中在这个 crate 里，每一处都写了 `SAFETY:` 说明。
//! 上层（`rt` runtime、`async-ping`、`raw-ping`）只使用这里暴露的安全 API。
//!
//! 所有权模型：
//! - [`Mbuf`]：一个包缓冲区的唯一所有者，Drop 时归还 mempool。内部是裸指针，
//!   因此自动是 `!Send + !Sync`：不能被送到别的线程，与单核 runtime 的设计一致。
//! - [`Mempool`]：创建后泄漏为 `&'static`，从类型上保证 mbuf 不会比它所属的池活得更久。
//! - [`Port`]：rx 交出 mbuf 的所有权；tx 成功则所有权交给网卡，失败则原样还给调用者。

mod error;
mod eal;
mod mbuf;
mod mempool;
mod port;
pub mod tsc;

pub use eal::Eal;
pub use error::{Error, Result};
pub use mbuf::Mbuf;
pub use mempool::Mempool;
pub use port::{Port, PortStats, RxBurst, RX_BURST_MAX};
