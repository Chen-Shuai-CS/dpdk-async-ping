//! B：raw-ping —— 与 A 行为完全相同，但用一个手写的 busy-poll 循环 + 状态表实现。
//!
//! 每轮循环三步：
//! 1. `on_rx`：rx_burst → 解析 → 用 id 查表 → **就地**更新该 session 的状态（T2 / T3）
//! 2. `on_timers`：TimerHeap 里到期的 session → record(上一个 reply) → 发下一个 request（段③ → T0 / T1）
//! 3. `on_house`：每 100 µs 一次维护：ENA watchdog、TX 回收、超时扫描、停止判定
//!
//! 与 A 共用：数据面初始化、发送函数（段①）、协议解析、TimerHeap、直方图、维护动作。
//! 唯一的区别：状态转移由这个循环直接完成，不经过 Waker / 就绪队列 / poll。

use dpdk::tsc::{ns_to_cycles, StallWatch};
use dpdk::{Mbuf, RxBurst};
use pingkit::house::{maintain, House};
use pingkit::live::{self, Live};
use pingkit::stats::{port_summary, Report, STALL_RX_THRESHOLD_NS, STALL_THRESHOLD_NS};
use pingkit::{rdtsc, Args, Dataplane, SendError, Sender, Stamp, Stats, TimerHeap};
use pingproto::{arp_reply_in_place, classify, Rx};
use std::sync::Arc;
use std::time::{Duration, Instant};

const HOUSEKEEPING_NS: u64 = 100_000;
const TX_RETRY_NS: u64 = 1_000;

/// 一个 session 的状态（在 A 里，等价的 enum 由编译器从 async fn 生成）。
#[derive(Clone, Copy)]
enum State {
    /// 在 TimerHeap 里等 deadline（初始错峰 / 收到 reply 后的 delay / 超时后的 delay）
    Sleeping { deadline: u64 },
    /// 已发出 `seq`，等 reply；超时由维护节拍扫描
    Waiting { seq: u16, stamp: Stamp, timeout_at: u64 },
    /// 停止阶段已退出
    Done,
}

/// sleep 期间持有的 reply：SPEC 要求持有那个 rx mbuf 睡眠，醒来后才 record 并释放。
struct Held {
    mbuf: Mbuf,
    stamp: Stamp,
    seq: u16,
    /// 回复里带回的、我们发送时写入的 TSC；record 时用来核对它确实是这个请求的应答
    tx_tsc: u64,
    t2: u64,
    t3: u64,
}

struct Session {
    state: State,
    next_seq: u16,
    held: Option<Held>,
    /// 最近超时过的 seq，用来把之后到达的 reply 归为"迟到"而不是"未知"
    recent_timeouts: [Option<u16>; 4],
    rt_pos: usize,
}

impl Session {
    fn new(first_deadline: u64) -> Session {
        Session {
            state: State::Sleeping { deadline: first_deadline },
            next_seq: 0,
            held: None,
            recent_timeouts: [None; 4],
            rt_pos: 0,
        }
    }
    fn remember_timeout(&mut self, seq: u16) {
        self.recent_timeouts[self.rt_pos] = Some(seq);
        self.rt_pos = (self.rt_pos + 1) % self.recent_timeouts.len();
    }
}

struct Raw {
    dp: Dataplane,
    sender: Sender,
    sessions: Vec<Session>,
    timers: TimerHeap,
    stats: Stats,
    burst: RxBurst,
    delay: u64,
    timeout: u64,
    stopping: bool,
}

impl Raw {
    /// 第 1 步：收包。解析 → 查表 → 就地改状态。没有 waker、没有队列、没有 poll。
    #[inline(always)]
    fn on_rx(&mut self) -> Option<u64> {
        let got = self.dp.port.rx_burst(&mut self.burst);
        if got == 0 {
            return None;
        }
        let t2 = rdtsc(); // T2：rx_burst 返回（同一批包共用这个 T2）
        self.stats.on_burst(got);
        let (my_ip, peer_ip, my_mac) = (self.dp.endpoints.src_ip, self.dp.endpoints.dst_ip, self.dp.endpoints.src_mac);
        while let Some(mut m) = self.burst.next() {
            match classify(m.data(), my_ip, peer_ip) {
                Rx::EchoReply { id, seq, tx_tsc } => self.on_reply(m, id, seq, tx_tsc, t2),
                Rx::ForeignEchoReply { src, id, seq } => {
                    self.stats.c.foreign += 1;
                    self.stats.note_anomaly(format_args!(
                        "foreign：来自 {}.{}.{}.{} 的 echo reply（不是对端），id={id} seq={seq}，已丢弃",
                        src[0], src[1], src[2], src[3]
                    ));
                }
                Rx::ArpRequest => {
                    arp_reply_in_place(m.data_mut(), my_mac, my_ip);
                    if self.dp.port.tx(m).is_ok() {
                        self.stats.c.arp_replies += 1;
                    } else {
                        self.stats.c.other_rx += 1; // 没能应答：仍要落入某一类，保证收包对账
                    }
                }
                Rx::Other => self.stats.c.other_rx += 1, // m 在这里 drop，归还 mempool
            }
        }
        Some(t2)
    }

    #[inline(always)]
    fn on_reply(&mut self, m: Mbuf, id: u16, seq: u16, tx_tsc: u64, t2: u64) {
        let Some(s) = self.sessions.get_mut(id as usize) else {
            self.stats.c.unexpected += 1;
            self.stats.note_anomaly(format_args!("unexpected：id={id} 超出 session 范围，seq={seq}"));
            return;
        };
        match s.state {
            State::Waiting { seq: want, stamp, .. } if want == seq => {
                let t3 = rdtsc(); // T3：reply 交到该 session 的状态机，可以开始算延迟
                self.stats.c.received += 1;
                s.held = Some(Held { mbuf: m, stamp, seq, tx_tsc, t2, t3 });
                let deadline = t3 + self.delay;
                s.state = State::Sleeping { deadline };
                self.timers.push(deadline, id as u32);
            }
            _ => {
                if s.recent_timeouts.contains(&Some(seq)) {
                    self.stats.c.late += 1;
                } else {
                    self.stats.c.unexpected += 1;
                    let state = match s.state {
                        State::Waiting { seq: w, .. } => format!("正在等 seq={w}"),
                        State::Sleeping { .. } => "sleep 中".to_string(),
                        State::Done => "已结束".to_string(),
                    };
                    let next = s.next_seq;
                    self.stats.note_anomaly(format_args!("unexpected：id={id} seq={seq}，该 session {state}，next_seq={next}"));
                }
            }
        }
    }

    /// 第 2 步：到期的 session → record(上一个 reply) → 发下一个 request。
    #[inline(always)]
    fn on_timers(&mut self, now: u64) -> bool {
        // `now` = "timer 发现到期"的时刻，由主循环每轮读一次（与 A 的 runtime 主循环相同）
        let mut fired = false;
        while let Some((deadline, id)) = self.timers.pop_expired(now) {
            fired = true;
            let s = &mut self.sessions[id as usize];
            if !matches!(s.state, State::Sleeping { .. }) {
                continue; // 防御：不应发生
            }
            // SPEC 的 loop 形状：sleep 结束后才 record(reply)，然后 reply（mbuf）被释放
            if let Some(h) = s.held.take() {
                self.stats.on_reply(h.stamp, h.t2, h.t3);
                self.stats.verify_echo(h.stamp, h.tx_tsc, id as u16, h.seq);
                drop(h.mbuf);
            }
            if self.stopping {
                s.state = State::Done;
                continue;
            }
            let seq = s.next_seq;
            let t0 = rdtsc(); // T0：record 之后、发送之前——与 A 的 send() 入口是同一个位置
            match self.sender.send(&self.dp.port, t0, id as u16, seq) {
                Ok(stamp) => {
                    self.stats.on_wake(deadline, now, stamp.t0);
                    self.stats.c.sent += 1;
                    s.next_seq = seq.wrapping_add(1);
                    s.state = State::Waiting { seq, stamp, timeout_at: stamp.t0 + self.timeout };
                }
                Err(e) => {
                    match e {
                        SendError::NoMbuf => self.stats.c.no_mbuf += 1,
                        SendError::TxFull => self.stats.c.tx_full += 1,
                    }
                    // 稍后重试（不在本轮 while 里立刻重试，避免 TX 持续满时死循环）
                    let retry = now + ns_to_cycles(TX_RETRY_NS, self.dp.hz);
                    s.state = State::Sleeping { deadline: retry };
                    self.timers.push(retry, id);
                }
            }
        }
        fired
    }

    /// 第 3 步（每 100 µs）：超时扫描。超时的请求计为丢失，session 照常 delay 后发下一个。
    fn scan_timeouts(&mut self, now: u64) {
        for (id, s) in self.sessions.iter_mut().enumerate() {
            if let State::Waiting { seq, timeout_at, .. } = s.state {
                if now >= timeout_at {
                    self.stats.c.timeouts += 1;
                    s.remember_timeout(seq);
                    let deadline = now + self.delay;
                    s.state = State::Sleeping { deadline };
                    self.timers.push(deadline, id as u32);
                }
            }
        }
    }

    fn all_done(&self) -> bool {
        self.sessions.iter().all(|s| matches!(s.state, State::Done))
    }
}

fn main() {
    let args = Args::load();
    live::install_signal_handlers();
    let dp = Dataplane::open(&args).unwrap_or_else(|e| {
        eprintln!("初始化失败：{e}");
        std::process::exit(2);
    });
    let hz = dp.hz;
    let xstats_before = dp.port.xstats();
    let live = Arc::new(Live::default());
    let reporter = live::spawn_reporter(live.clone(), args.report_core, Duration::from_secs(args.progress_sec), "B raw");

    let n = args.sessions as usize;
    let delay = ns_to_cycles(args.delay_us * 1000, hz);
    // 起点定在 1 ms 之后：下面的准备工作不应算进任何 session 的 sleep 误差（见 START_LEAD_NS）
    let start = rdtsc() + ns_to_cycles(pingkit::house::START_LEAD_NS, hz);
    let end = start + ns_to_cycles(args.duration_sec * 1_000_000_000, hz);
    let mut raw = Raw {
        sender: Sender::new(dp.pool, dp.tmpl.clone()),
        dp,
        // 初始相位：session 均匀错开在一个 delay 周期内，避免同时发、同时回
        sessions: (0..n).map(|i| Session::new(start + delay * i as u64 / n as u64)).collect(),
        timers: TimerHeap::with_capacity(2 * n),
        stats: Stats::with_hz(hz),
        burst: RxBurst::new(),
        delay,
        timeout: ns_to_cycles(args.timeout_us * 1000, hz),
        stopping: false,
    };
    for (i, s) in raw.sessions.iter().enumerate() {
        if let State::Sleeping { deadline } = s.state {
            raw.timers.push(deadline, i as u32);
        }
    }
    let wall = Instant::now();
    let mut house = House::new(ns_to_cycles(HOUSEKEEPING_NS, hz), start);
    let mut exit_reason = String::from("到达 --duration-sec，停止发送并等在途请求收尾");
    // ---------------- 主循环：busy-poll，永不睡眠 ----------------
    // 与 A 的 runtime 主循环同构：收包 → 读一次时钟 → 到期的 timer → 维护节拍
    let mut watch =
        StallWatch::new(ns_to_cycles(STALL_THRESHOLD_NS, hz), ns_to_cycles(STALL_RX_THRESHOLD_NS, hz), rdtsc());
    let mut worked = true; // 上一次读时钟之后是否干过活
    loop {
        if let Some(t2) = raw.on_rx() {
            watch.tick_rx(t2, worked); // 停顿检测（取包前）：在处理完这批包之后才调用，不插在 T2 与 T3 之间
            worked = true;
        }
        let now = rdtsc();
        watch.tick(now, worked); // 停顿检测（空轮询）：上一次读时钟之后什么都没干却隔了很久 → 被外部打断
        worked = raw.on_timers(now);
        if house.due(now) {
            worked = true;
            maintain(&raw.dp.eal, &raw.dp.port, &mut raw.stats, &live);
            raw.scan_timeouts(now);
            if !raw.stopping {
                if raw.dp.port.reset_requested() {
                    exit_reason = "网卡请求 reset（ENA watchdog），提前停止".into();
                    break;
                }
                if live::stop_requested() {
                    exit_reason = "收到 SIGINT/SIGTERM，停止发送并收尾".into();
                    raw.stopping = true;
                } else if now >= end {
                    raw.stopping = true;
                }
            }
            if raw.stopping && raw.all_done() {
                break;
            }
        }
    }
    raw.stats.stalls = watch;
    finish(raw, &args, wall, xstats_before, exit_reason, live, reporter);
}

/// 关停与报告：先释放程序持有的所有 mbuf，再停端口，最后核对 mempool。
fn finish(
    mut raw: Raw,
    args: &Args,
    wall: Instant,
    xstats_before: Vec<(String, u64)>,
    exit_reason: String,
    live: Arc<Live>,
    reporter: Option<std::thread::JoinHandle<()>>,
) {
    let elapsed = wall.elapsed().as_secs_f64();
    live.publish(&raw.stats);
    live.finish();
    if let Some(r) = reporter {
        let _ = r.join();
    }
    raw.stats.c.in_flight_at_end =
        raw.sessions.iter().filter(|s| matches!(s.state, State::Waiting { .. })).count() as u64;
    let port = port_summary(&raw.dp.port, &xstats_before);
    let (stats, hz) = (raw.stats, raw.dp.hz);
    // 释放程序持有的所有 mbuf：session 里 held 的 reply、RxBurst 里未取走的包
    drop(raw.sessions);
    drop(raw.burst);
    let (eal, mbuf) = raw.dp.shutdown();
    Report::new("B · raw-ping（手写 busy-poll，无 runtime）", args, elapsed, &stats, hz, port, mbuf, exit_reason)
        .emit(args.json.as_deref());
    // SAFETY: 所有 Mbuf / Port 都已释放或关闭，mempool 此后不再被访问。
    unsafe { eal.cleanup() };
    std::process::exit(if mbuf.leaked() == 0 { 0 } else { 3 });
}
