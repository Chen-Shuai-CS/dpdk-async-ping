use dpdk::tsc::rdtsc;
use dpdk::{Mempool, Port};
use pingproto::EchoTemplate;

/// 一次发送的两个时刻。
#[derive(Debug, Clone, Copy, Default)]
pub struct Stamp {
    /// T0：决定发这个 request 的时刻（send 入口）
    pub t0: u64,
    /// T1：tx_burst 返回、doorbell 已敲
    pub t1: u64,
    /// 诊断：距离上一次发送（上一个 T1）多久。ENA 每次发送前有一次 sfence，
    /// 要等上一个包的写合并缓冲排空，所以背靠背的发送段①会明显更长。
    pub since_prev_tx: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// mempool 里没有空闲 mbuf
    NoMbuf,
    /// TX 环满，网卡没有接收（mbuf 已释放）
    TxFull,
}

/// 发送 echo request：**段①（T0 → T1）的全部代码**。A 的 `send().await` 和 B 的循环调用的是同一个函数。
pub struct Sender {
    pool: &'static Mempool,
    tmpl: EchoTemplate,
    last_t1: std::cell::Cell<u64>,
}

impl Sender {
    pub fn new(pool: &'static Mempool, tmpl: EchoTemplate) -> Sender {
        Sender { pool, tmpl, last_t1: std::cell::Cell::new(0) }
    }

    /// 取 mbuf → 写入模板、id、seq、当前 TSC、增量校验和 → tx_burst（1 个包，一次 doorbell）。
    #[inline(always)]
    pub fn send(&self, port: &Port, id: u16, seq: u16) -> Result<Stamp, SendError> {
        let t0 = rdtsc();
        let Some(mut m) = self.pool.alloc() else { return Err(SendError::NoMbuf) };
        m.set_len(self.tmpl.len());
        self.tmpl.write_request(m.data_mut(), id, seq, t0);
        match port.tx(m) {
            Ok(()) => {
                let t1 = rdtsc();
                let since_prev_tx = t0.saturating_sub(self.last_t1.replace(t1));
                Ok(Stamp { t0, t1, since_prev_tx })
            }
            Err(m) => {
                drop(m);
                Err(SendError::TxFull)
            }
        }
    }
}
