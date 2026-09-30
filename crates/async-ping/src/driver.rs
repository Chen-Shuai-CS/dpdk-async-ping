//! 协议侧：实现 `rt::Driver`。runtime 负责轮询网卡与调度；这里负责理解包的内容。

use dpdk::{Eal, Mbuf, Port};
use pingkit::house::maintain;
use pingkit::live::{self, Live};
use pingkit::{Sender, Stats};
use pingproto::{arp_reply_in_place, classify, Rx};
use rt::sync::Mailbox;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

/// reactor 交给 session 的 reply：mbuf 的所有权 + 它的 T2。
pub struct Reply {
    /// 只为持有所有权：session sleep 期间 mbuf 不归还（SPEC §6.1），record 之后随 Reply 一起 drop
    #[allow(dead_code)]
    pub mbuf: Mbuf,
    pub t2: u64,
}

/// `wait_reply` 超时。
#[derive(Debug)]
pub struct Timeout;

/// 一个 session 在协议侧的状态：正在等哪个 seq、何时超时、以及交付 reply 的信箱。
pub struct Flow {
    pub expect: Cell<Option<u16>>,
    pub timeout_at: Cell<u64>,
    recent_timeouts: Cell<[Option<u16>; 4]>,
    rt_pos: Cell<usize>,
    pub mailbox: Mailbox<Result<Reply, Timeout>>,
}

impl Flow {
    fn new() -> Flow {
        Flow {
            expect: Cell::new(None),
            timeout_at: Cell::new(0),
            recent_timeouts: Cell::new([None; 4]),
            rt_pos: Cell::new(0),
            mailbox: Mailbox::new(),
        }
    }

    /// 发送成功后登记"我在等 seq"（在 reply 可能被处理之前，单线程下不存在竞态）。
    #[inline]
    pub fn arm(&self, seq: u16, timeout_at: u64) {
        self.expect.set(Some(seq));
        self.timeout_at.set(timeout_at);
    }

    fn remember_timeout(&self, seq: u16) {
        let mut r = self.recent_timeouts.get();
        let p = self.rt_pos.get();
        r[p] = Some(seq);
        self.recent_timeouts.set(r);
        self.rt_pos.set((p + 1) % r.len());
    }

    fn timed_out_before(&self, seq: u16) -> bool {
        self.recent_timeouts.get().contains(&Some(seq))
    }
}

/// session task 与 driver 共享的状态（单线程，Rc 共享）。
pub struct Shared {
    pub flows: Box<[Flow]>,
    pub stats: RefCell<Stats>,
    pub sender: Sender,
    /// 以下均为 TSC 周期
    pub delay: u64,
    pub timeout: u64,
    pub tx_retry: u64,
    pub end: u64,
    pub stopping: Cell<bool>,
    pub exit_reason: RefCell<String>,
    pub my_ip: [u8; 4],
    pub my_mac: [u8; 6],
    pub live: Arc<Live>,
    /// 诊断：最近一次 Mailbox::put（含 wake）返回的时刻
    #[cfg(feature = "probe")]
    pub probe_put_done: Cell<u64>,
    /// 诊断：段②的三个子段 [T2→put 返回, put 返回→开始 poll, 开始 poll→T3]
    #[cfg(feature = "probe")]
    pub probe: RefCell<[pingkit::hist::Hist; 3]>,
}

impl Shared {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sessions: usize,
        sender: Sender,
        delay: u64,
        timeout: u64,
        tx_retry: u64,
        end: u64,
        my_ip: [u8; 4],
        my_mac: [u8; 6],
        live: Arc<Live>,
    ) -> Shared {
        Shared {
            flows: (0..sessions).map(|_| Flow::new()).collect(),
            stats: RefCell::new(Stats::default()),
            sender,
            delay,
            timeout,
            tx_retry,
            end,
            stopping: Cell::new(false),
            exit_reason: RefCell::new("到达 --duration-sec，停止发送并等在途请求收尾".into()),
            my_ip,
            my_mac,
            live,
            #[cfg(feature = "probe")]
            probe_put_done: Cell::new(0),
            #[cfg(feature = "probe")]
            probe: RefCell::new(Default::default()),
        }
    }
}

pub struct IcmpDriver<'a> {
    pub sh: Rc<Shared>,
    pub eal: &'a Eal,
}

impl rt::Driver for IcmpDriver<'_> {
    #[inline]
    fn on_burst(&self, n: usize) {
        self.sh.stats.borrow_mut().on_burst(n);
    }

    /// 解析 → 用 id 找到 Flow → 把 reply（mbuf 所有权 + T2）放进它的信箱（put 里会 wake 该 session）。
    #[inline]
    fn on_packet(&self, mut m: Mbuf, t2: u64, port: &Port) {
        let sh = &*self.sh;
        match classify(m.data(), sh.my_ip) {
            Rx::EchoReply { id, seq, .. } => {
                let Some(f) = sh.flows.get(id as usize) else {
                    sh.stats.borrow_mut().c.unexpected += 1;
                    return;
                };
                if f.expect.get() == Some(seq) {
                    f.expect.set(None);
                    if f.mailbox.put(Ok(Reply { mbuf: m, t2 })).is_err() {
                        sh.stats.borrow_mut().c.unexpected += 1; // 不应发生：信箱里已有未取走的东西
                    }
                    #[cfg(feature = "probe")]
                    sh.probe_put_done.set(dpdk::tsc::rdtsc());
                } else if f.timed_out_before(seq) {
                    sh.stats.borrow_mut().c.late += 1;
                } else {
                    sh.stats.borrow_mut().c.unexpected += 1;
                }
            }
            Rx::ArpRequest => {
                arp_reply_in_place(m.data_mut(), sh.my_mac, sh.my_ip);
                if port.tx(m).is_ok() {
                    sh.stats.borrow_mut().c.arp_replies += 1;
                }
            }
            Rx::Other => sh.stats.borrow_mut().c.other_rx += 1,
        }
    }

    /// 维护节拍（与 B 相同）：DPDK timer / TX 回收 / 发布计数 → 超时扫描 → 停止判定。
    fn on_tick(&self, now: u64, port: &Port) -> bool {
        let sh = &*self.sh;
        maintain(self.eal, port, &sh.stats.borrow(), &sh.live);
        for f in sh.flows.iter() {
            if let Some(seq) = f.expect.get() {
                if now >= f.timeout_at.get() {
                    f.expect.set(None);
                    f.remember_timeout(seq);
                    sh.stats.borrow_mut().c.timeouts += 1;
                    let _ = f.mailbox.put(Err(Timeout)); // wait_reply 从这里返回 Err
                }
            }
        }
        if !sh.stopping.get() {
            if port.reset_requested() {
                *sh.exit_reason.borrow_mut() = "网卡请求 reset（ENA watchdog），提前停止".into();
                return false;
            }
            if live::stop_requested() {
                *sh.exit_reason.borrow_mut() = "收到 SIGINT/SIGTERM，停止发送并收尾".into();
                sh.stopping.set(true);
            } else if now >= sh.end {
                sh.stopping.set(true);
            }
        }
        true
    }
}