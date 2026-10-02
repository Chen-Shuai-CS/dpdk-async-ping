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
    /// 诊断（`probe` 特性）：段①的三个子步骤
    #[cfg(feature = "probe")]
    pub probe: SendProbe,
}

/// 诊断（`probe` 特性）：把段①拆成"取 mbuf / 写包 / tx_burst"，并记下这是第几次发送（≈ 发送队列的位置）。
/// 每个子步骤都多含一次约 16 ns 的时钟读取，所以 probe 构建的段①绝对值偏大，只用来看尾巴落在哪一步。
#[cfg(feature = "probe")]
#[derive(Debug, Clone, Copy, Default)]
pub struct SendProbe {
    pub alloc: u64,
    pub build: u64,
    pub tx: u64,
    pub ring_slot: u8,
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
    #[cfg(feature = "probe")]
    count: std::cell::Cell<u64>,
    #[cfg(feature = "fault")]
    fault: Option<TxFault>,
}

/// 故障注入（`fault` 特性）：T0 落在 [from, to) 之内的发送一律失败，用来测"发送持续失败时程序还能不能按时收尾"。
#[cfg(feature = "fault")]
#[derive(Clone, Copy)]
struct TxFault {
    kind: SendError,
    from: u64,
    to: u64,
}

#[cfg(feature = "fault")]
impl TxFault {
    /// `BQ_FAULT_TX=txfull:2`（从启动后第 2 秒起一直失败）或 `nombuf:2-6`（第 2 秒到第 6 秒之间失败）。
    fn from_env() -> Option<TxFault> {
        let v = std::env::var("BQ_FAULT_TX").ok()?;
        let (kind, span) = v.split_once(':')?;
        let kind = match kind {
            "txfull" => SendError::TxFull,
            "nombuf" => SendError::NoMbuf,
            _ => return None,
        };
        let (from, to) = match span.split_once('-') {
            Some((a, b)) => (a.parse::<f64>().ok()?, Some(b.parse::<f64>().ok()?)),
            None => (span.parse::<f64>().ok()?, None),
        };
        let (now, hz) = (rdtsc(), dpdk::tsc::hz() as f64);
        let at = |sec: f64| now + (sec * hz) as u64;
        eprintln!("★ 故障注入：发送在启动后 {from} 秒 ~ {} 之间一律失败（{kind:?}）", to.map_or("结束".to_string(), |t| format!("{t} 秒")));
        Some(TxFault { kind, from: at(from), to: to.map_or(u64::MAX, at) })
    }
}

impl Sender {
    pub fn new(pool: &'static Mempool, tmpl: EchoTemplate) -> Sender {
        Sender {
            pool,
            tmpl,
            last_t1: std::cell::Cell::new(0),
            #[cfg(feature = "probe")]
            count: std::cell::Cell::new(0),
            #[cfg(feature = "fault")]
            fault: TxFault::from_env(),
        }
    }

    /// 取 mbuf → 写入模板、id、seq、T0、增量校验和 → tx_burst（1 个包，一次 doorbell）→ 读 T1。
    ///
    /// `t0` 由调用者在**决定发送的那一刻**读取并传入（SPEC §7）：
    /// A 在 `send()` 的第一行读（早于向 runtime 查找端口），B 在循环判定该发之后、调用本函数之前读。
    /// 这样两边的段①覆盖的是语义相同的一段，A 为了发包而经过 runtime 的那一步也被计入。
    #[inline(always)]
    pub fn send(&self, port: &Port, t0: u64, id: u16, seq: u16) -> Result<Stamp, SendError> {
        #[cfg(feature = "fault")]
        if let Some(f) = self.fault {
            if t0 >= f.from && t0 < f.to {
                return Err(f.kind);
            }
        }
        let Some(mut m) = self.pool.alloc() else { return Err(SendError::NoMbuf) };
        #[cfg(feature = "probe")]
        let ta = rdtsc();
        m.set_len(self.tmpl.len());
        self.tmpl.write_request(m.data_mut(), id, seq, t0);
        #[cfg(feature = "probe")]
        let tb = rdtsc();
        match port.tx(m) {
            Ok(()) => {
                let t1 = rdtsc();
                let since_prev_tx = t0.saturating_sub(self.last_t1.replace(t1));
                Ok(Stamp {
                    t0,
                    t1,
                    since_prev_tx,
                    #[cfg(feature = "probe")]
                    probe: SendProbe {
                        alloc: ta - t0,
                        build: tb - ta,
                        tx: t1 - tb,
                        ring_slot: (self.count.replace(self.count.get() + 1) % 32) as u8,
                    },
                })
            }
            Err(m) => {
                drop(m);
                Err(SendError::TxFull)
            }
        }
    }
}
