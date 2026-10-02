//! 协议侧：实现 `rt::Driver`。runtime 负责轮询网卡与调度；这里负责理解包的内容。

use dpdk::{Eal, Mbuf, Port};
use pingkit::house::maintain;
use pingkit::live::{self, Live};
use pingkit::{judge, Sender, Stats, Verdict};
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
    /// 在等的那个请求发送时写进包里的 T0：回复必须原样带回它才算数（见 `pingkit::matching`）
    expect_t0: Cell<u64>,
    pub timeout_at: Cell<u64>,
    recent_timeouts: Cell<[Option<u16>; 4]>,
    rt_pos: Cell<usize>,
    pub mailbox: Mailbox<Result<Reply, Timeout>>,
}

impl Flow {
    fn new() -> Flow {
        Flow {
            expect: Cell::new(None),
            expect_t0: Cell::new(0),
            timeout_at: Cell::new(0),
            recent_timeouts: Cell::new([None; 4]),
            rt_pos: Cell::new(0),
            mailbox: Mailbox::new(),
        }
    }

    /// 发送成功后登记"我在等 seq，它带着 T0 = t0"（在 reply 可能被处理之前，单线程下不存在竞态）。
    #[inline]
    pub fn arm(&self, seq: u16, t0: u64, timeout_at: u64) {
        self.expect.set(Some(seq));
        self.expect_t0.set(t0);
        self.timeout_at.set(timeout_at);
    }

    /// 一个 echo reply 到了：判定它是不是在等的那个请求的应答。只有 `Accept` 才结束等待；
    /// 其余三种都不改变在途请求的状态和它的超时时刻。
    #[inline(always)]
    pub fn accept(&self, seq: u16, echoed_tsc: u64) -> Verdict {
        let v = judge(self.expect.get().map(|s| (s, self.expect_t0.get())), seq, echoed_tsc, || self.timed_out_before(seq));
        if v == Verdict::Accept {
            self.expect.set(None);
        }
        v
    }

    /// 超时扫描：在途请求到了超时时刻就结束等待并记住这个 seq（之后到达的回复归为"迟到"）。返回超时的 seq。
    pub fn expire(&self, now: u64) -> Option<u16> {
        let seq = self.expect.get()?;
        if now < self.timeout_at.get() {
            return None;
        }
        self.expect.set(None);
        self.remember_timeout(seq);
        Some(seq)
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
    pub peer_ip: [u8; 4],
    pub my_mac: [u8; 6],
    pub live: Arc<Live>,
    /// 诊断开关 `--diag-pre-t0`（默认 None）
    pub diag_pre_t0: Option<pingkit::Diag>,
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
        peer_ip: [u8; 4],
        my_mac: [u8; 6],
        live: Arc<Live>,
        diag_pre_t0: Option<pingkit::Diag>,
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
            peer_ip,
            my_mac,
            live,
            diag_pre_t0,
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
        match classify(m.data(), sh.my_ip, sh.peer_ip) {
            Rx::EchoReply { id, seq, tx_tsc } => {
                let Some(f) = sh.flows.get(id as usize) else {
                    let mut st = sh.stats.borrow_mut();
                    st.c.unexpected += 1;
                    st.note_anomaly(format_args!("unexpected：id={id} 超出 session 范围，seq={seq}"));
                    return;
                };
                match f.accept(seq, tx_tsc) {
                    Verdict::Accept => {
                        if f.mailbox.put(Ok(Reply { mbuf: m, t2 })).is_err() {
                            sh.stats.borrow_mut().c.unexpected += 1; // 不应发生：信箱里已有未取走的东西
                        }
                        #[cfg(feature = "probe")]
                        sh.probe_put_done.set(dpdk::tsc::rdtsc());
                    }
                    // 拒绝：m 在这里 drop（归还 mempool）；在途请求原样保留，等真正的回复或原定的超时
                    Verdict::TscMismatch => sh.stats.borrow_mut().on_tsc_mismatch(id, seq, tx_tsc, f.expect_t0.get()),
                    Verdict::Late => sh.stats.borrow_mut().c.late += 1,
                    Verdict::Unexpected => {
                        let mut st = sh.stats.borrow_mut();
                        st.c.unexpected += 1;
                        let expect = f.expect.get();
                        st.note_anomaly(format_args!("unexpected：id={id} seq={seq}，该 session 正在等 {expect:?}"));
                    }
                }
            }
            Rx::ForeignEchoReply { src, id, seq } => {
                let mut st = sh.stats.borrow_mut();
                st.c.foreign += 1;
                st.note_anomaly(format_args!(
                    "foreign：来自 {}.{}.{}.{} 的 echo reply（不是对端），id={id} seq={seq}，已丢弃",
                    src[0], src[1], src[2], src[3]
                ));
            }
            Rx::ArpRequest => {
                arp_reply_in_place(m.data_mut(), sh.my_mac, sh.my_ip);
                let mut st = sh.stats.borrow_mut();
                if port.tx(m).is_ok() {
                    st.c.arp_replies += 1;
                } else {
                    st.c.other_rx += 1; // 没能应答：仍要落入某一类，保证收包对账
                }
            }
            Rx::Other => sh.stats.borrow_mut().c.other_rx += 1,
        }
    }

    /// 维护节拍（与 B 相同）：DPDK timer / TX 回收 / 发布计数 → 超时扫描 → 停止判定。
    fn on_tick(&self, now: u64, port: &Port) -> bool {
        let sh = &*self.sh;
        maintain(self.eal, port, &mut sh.stats.borrow_mut(), &sh.live);
        for f in sh.flows.iter() {
            if f.expire(now).is_some() {
                sh.stats.borrow_mut().c.timeouts += 1;
                let _ = f.mailbox.put(Err(Timeout)); // wait_reply 从这里返回 Err
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

#[cfg(test)]
mod tests {
    use super::*;

    // 审查意见 R1 的四个验收场景，在 session 的协议状态（`Flow`）上验证完整的状态转换。
    // B 的同名测试（`raw-ping/src/main.rs`）跑的是同一组场景、同一组预期。

    /// 同 id/seq、错误时间戳的包先到，随后正确的包到：前者被拒绝，后者完成请求，只完成一次。
    #[test]
    fn wrong_tsc_first_then_the_real_reply() {
        let f = Flow::new();
        f.arm(7, 1_000, 5_000);
        assert_eq!(f.accept(7, 999), Verdict::TscMismatch);
        assert_eq!((f.expect.get(), f.timeout_at.get()), (Some(7), 5_000), "被拒绝的包不得改变在途请求");
        assert_eq!(f.accept(7, 1_000), Verdict::Accept);
        assert_eq!(f.expect.get(), None);
        assert_eq!(f.accept(7, 1_000), Verdict::Unexpected, "同一个回复再来一次（重复包）不能再完成一次");
    }

    /// 错误的包先到，正确的包一直不来：原请求按原定的时刻超时。
    #[test]
    fn wrong_tsc_first_and_the_real_reply_never_comes() {
        let f = Flow::new();
        f.arm(7, 1_000, 5_000);
        assert_eq!(f.accept(7, 999), Verdict::TscMismatch);
        assert_eq!(f.expire(4_999), None, "超时时刻不因被拒绝的包提前或推后");
        assert_eq!(f.expire(5_000), Some(7));
        assert_eq!(f.expect.get(), None);
        assert_eq!(f.accept(7, 1_000), Verdict::Late, "超时之后真正的回复才到：归为迟到");
    }

    /// seq 回绕之后，上一圈同一个 seq 的旧回复到达：不得完成新请求。
    #[test]
    fn stale_reply_after_seq_wraparound() {
        let f = Flow::new();
        f.arm(7, 1_000, 5_000);
        assert_eq!(f.accept(7, 1_000), Verdict::Accept);
        // …… 65536 个请求之后，seq 又是 7，T0 当然不同
        f.arm(7, 900_000, 905_000);
        assert_eq!(f.accept(7, 1_000), Verdict::TscMismatch, "上一圈的回复（重放 / 迟到很久）");
        assert_eq!(f.expect.get(), Some(7));
        assert_eq!(f.accept(7, 900_000), Verdict::Accept);
    }

    /// 没有在途请求时到达的回复、seq 对不上的回复：都不影响状态。
    #[test]
    fn replies_that_match_nothing() {
        let f = Flow::new();
        assert_eq!(f.accept(3, 1), Verdict::Unexpected);
        f.arm(7, 1_000, 5_000);
        assert_eq!(f.accept(6, 1_000), Verdict::Unexpected);
        assert_eq!((f.expect.get(), f.timeout_at.get()), (Some(7), 5_000));
    }
}
