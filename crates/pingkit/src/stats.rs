use crate::dataplane::LeakReport;
use crate::Args;
use dpdk::Port;
use crate::envinfo::EnvInfo;
use crate::hist::Hist;
use crate::samples::SampleLog;
use crate::sender::Stamp;
use dpdk::tsc::cycles_to_ns;
use dpdk::RX_BURST_MAX;
use serde::Serialize;

/// 计数器。A 和 B 在相同语义的位置递增。
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Counters {
    /// 成功交给网卡的 request
    pub sent: u64,
    /// 超时前收到的 reply（= 延迟分布的样本数）
    pub received: u64,
    /// 超时的 request（计为丢失，不进入延迟分布）
    pub timeouts: u64,
    /// 超时之后才到达的 reply（能对上某个已超时的 seq）
    pub late: u64,
    /// id 越界、或 seq 对不上任何在途/已超时请求的 echo reply
    pub unexpected: u64,
    /// 源 IP 不是对端的 echo reply：不是对我们任何请求的应答，绝不交给 session
    pub foreign: u64,
    /// 回复里带回的发送时间戳 ≠ 我们发这个请求时写入的 T0（重复包 / 损坏 / 伪造）。
    /// 这样的回复已计入 received，但**不进入延迟分布**：样本数 = received − tsc_mismatch
    pub tsc_mismatch: u64,
    /// 其他与本程序无关的帧（非 IPv4/ARP、不是给我的……）
    pub other_rx: u64,
    /// 回答过的 ARP request
    pub arp_replies: u64,
    /// tx_burst 因 TX 环满没有接收（会重试）
    pub tx_full: u64,
    /// mempool 取不到 mbuf（会重试）
    pub no_mbuf: u64,
    /// 结束时仍在途（已发送、未收到、也未超时）
    pub in_flight_at_end: u64,
    pub rx_bursts: u64,
    pub rx_pkts: u64,
}

pub struct Stats {
    /// 段①：T1 − T0
    pub seg1: Hist,
    /// 段②：T3 − T2
    pub seg2: Hist,
    /// 进程内耗时 = 段① + 段②（排名指标）
    pub inproc: Hist,
    /// 端到端：T3 − T0
    pub e2e: Hist,
    /// 段③：timer 发现到期 → 下一个 T0
    pub seg3: Hist,
    /// sleep 误差：timer 发现到期的时刻 − deadline
    pub sleep_err: Hist,
    /// deadline → 下一个 T0（= sleep 误差 + 段③；"段③"的另一种读法）
    pub wake_total: Hist,
    /// 诊断：每次维护节拍里 `rte_timer_manage`（ENA watchdog）的耗时、整个维护动作的耗时
    pub house_timer: Hist,
    pub house_total: Hist,
    /// 主循环空转停顿（见 `dpdk::tsc::StallWatch`），结束时由调用方填入
    pub stalls: dpdk::tsc::StallWatch,
    /// 诊断：段① 按"距上一次发送多久"分档：<100 ns / 100–250 / 250–500 / 500 ns–2 µs / ≥2 µs。
    /// ENA 每次发送前有一次 sfence，要等上一个包的写合并缓冲排空，所以间隔越短段①越长。
    pub seg1_by_gap: [Hist; 5],
    /// 分档边界（TSC 周期），见 [`Stats::with_hz`]
    pub gap_edges: [u64; 4],
    /// 诊断（`probe` 特性）：段①的子步骤 [取 mbuf, 写包, tx_burst]，只统计距上次发送 ≥ 250 ns 的发送
    #[cfg(feature = "probe")]
    pub probe_send: [Hist; 3],
    /// 诊断（`probe` 特性）：按"发送计数 % 32"（≈ LLQ 条目在 4 KB 页内的位置）统计 [总数, tx_burst 慢的次数]
    #[cfg(feature = "probe")]
    pub probe_slot: [[u64; 2]; 32],
    /// 非空 rx_burst 的包数分布（下标 = 一次收到几个包）
    pub burst_sizes: [u64; RX_BURST_MAX + 1],
    pub c: Counters,
    /// 异常包明细（unexpected 等），最多记录 [`ANOMALY_LOG_MAX`] 条，冷路径
    pub anomalies: Vec<String>,
    /// 可选的逐样本原始记录（`--samples`）；没开时容量为 0，`push` 什么都不做
    pub samples: SampleLog,
}

pub const ANOMALY_LOG_MAX: usize = 16;

/// 段①"慢"的分界线（见 [`Report::seg1_slow_percent`]）。
pub const SEG1_SLOW_NS: u64 = 125;

/// 主循环停顿的判定阈值（见 `dpdk::tsc::StallWatch`）：
/// 一轮什么都没干的空轮询正常只要几十纳秒，超过 1 µs 就说明被外部打断了；
/// 收到包的那一轮，取包前超过 10 µs 也算（收一批包连同回填正常只要几微秒以内）。
pub const STALL_THRESHOLD_NS: u64 = 1_000;
pub const STALL_RX_THRESHOLD_NS: u64 = 10_000;

impl Default for Stats {
    fn default() -> Self {
        Stats {
            seg1: Hist::default(),
            seg2: Hist::default(),
            inproc: Hist::default(),
            e2e: Hist::default(),
            seg3: Hist::default(),
            sleep_err: Hist::default(),
            wake_total: Hist::default(),
            house_timer: Hist::default(),
            house_total: Hist::default(),
            stalls: dpdk::tsc::StallWatch::default(),
            seg1_by_gap: Default::default(),
            #[cfg(feature = "probe")]
            probe_send: Default::default(),
            #[cfg(feature = "probe")]
            probe_slot: [[0; 2]; 32],
            gap_edges: [260, 650, 1_300, 5_200], // 100/250/500/2000 ns @ 2.6 GHz；with_hz 按实际频率重设
            burst_sizes: [0; RX_BURST_MAX + 1],
            c: Counters::default(),
            anomalies: Vec::new(),
            samples: SampleLog::default(),
        }
    }
}

impl Stats {
    pub fn with_hz(hz: u64) -> Stats {
        let c = |ns| dpdk::tsc::ns_to_cycles(ns, hz);
        Stats { gap_edges: [c(100), c(250), c(500), c(2_000)], ..Stats::default() }
    }

    /// 一个端到端样本（按 SPEC 的 loop 形状，在 sleep 之后的 record(reply) 处调用）。
    #[inline]
    pub fn on_reply(&mut self, s: Stamp, t2: u64, t3: u64) {
        let s1 = s.t1.saturating_sub(s.t0);
        let s2 = t3.saturating_sub(t2);
        self.seg1.record(s1);
        let bucket = self.gap_edges.iter().take_while(|&&e| s.since_prev_tx >= e).count();
        self.seg1_by_gap[bucket].record(s1);
        #[cfg(feature = "probe")]
        if bucket >= 2 {
            // 排除"距上次发送 < 250 ns"的发送：那一类的写合并等待已由上面的分档单独统计
            let p = s.probe;
            self.probe_send[0].record(p.alloc);
            self.probe_send[1].record(p.build);
            self.probe_send[2].record(p.tx);
            let slot = &mut self.probe_slot[p.ring_slot as usize];
            slot[0] += 1;
            if p.tx > self.gap_edges[1] / 2 {
                slot[1] += 1; // tx_burst > 125 ns（正常约 35 ns + 16 ns 时钟读取）
            }
        }
        self.seg2.record(s2);
        self.inproc.record(s1 + s2);
        self.e2e.record(t3.saturating_sub(s.t0));
        self.samples.push(s1, s2, t2);
    }

    /// 一次 sleep 结束：deadline、timer 发现到期的时刻、随后的下一个 T0。
    ///
    /// SPEC 的"段③ = sleep 到期 → 下一个 T0"有两种读法，两种都记录：
    /// - "到期" = timer 发现到期的那一刻 → `seg3`（另有 `sleep_err` = 发现 − deadline）；
    /// - "到期" = deadline 本身 → `wake_total` = `sleep_err` + `seg3`。
    #[inline]
    pub fn on_wake(&mut self, deadline: u64, detected: u64, t0_next: u64) {
        self.sleep_err.record(detected.saturating_sub(deadline));
        self.seg3.record(t0_next.saturating_sub(detected));
        self.wake_total.record(t0_next.saturating_sub(deadline));
    }

    /// 在 record(reply) 时核对：回复带回的时间戳必须等于我们发这个请求时写入的 T0。
    /// 放在 sleep 之后，不在任何被测段里。返回 false 表示对不上：调用方**不得**把它计入延迟分布
    /// （它不是这个请求的应答，用它算出来的延迟没有意义）。
    #[inline]
    #[must_use]
    pub fn verify_echo(&mut self, s: Stamp, echoed_tsc: u64, id: u16, seq: u16) -> bool {
        if echoed_tsc == s.t0 {
            return true;
        }
        self.c.tsc_mismatch += 1;
        self.note_anomaly(format_args!(
            "tsc_mismatch：id={id} seq={seq}，回复带回的 TSC={echoed_tsc}，我们写入的 T0={}；该样本不进入延迟分布",
            s.t0
        ));
        false
    }

    /// `probe` 构建的附加诊断文本。
    pub fn probe_notes(&self) -> Vec<String> {
        #[cfg(feature = "probe")]
        {
            let s: Vec<String> = self
                .probe_slot
                .iter()
                .enumerate()
                .map(|(i, v)| format!("{i}:{:.2}%", 100.0 * v[1] as f64 / v[0].max(1) as f64))
                .collect();
            vec![format!("[probe] tx_burst > 125 ns 的占比，按「发送计数 % 32」分（32 个 LLQ 条目 = 一个 4 KB 页）：{}", s.join(" "))]
        }
        #[cfg(not(feature = "probe"))]
        Vec::new()
    }

    /// 记录一个异常包（冷路径）：计数由调用方负责，这里只保存前 16 条明细用于事后解释。
    #[cold]
    pub fn note_anomaly(&mut self, what: std::fmt::Arguments) {
        if self.anomalies.len() < ANOMALY_LOG_MAX {
            self.anomalies.push(format!("[tsc {}] {}", dpdk::tsc::rdtsc(), what));
        }
    }

    #[inline]
    pub fn on_burst(&mut self, n: usize) {
        self.c.rx_bursts += 1;
        self.c.rx_pkts += n as u64;
        self.burst_sizes[n] += 1;
    }

    /// `step` = TSC 读数步长（周期），用于插值分位数。
    pub fn rows(&self, hz: u64, step: u64) -> Vec<MetricRow> {
        [
            ("in-process ①+②  [排名指标]", &self.inproc),
            ("seg① send  T1−T0", &self.seg1),
            ("seg② recv  T3−T2", &self.seg2),
            ("end-to-end T3−T0", &self.e2e),
            ("seg③ wake→next T0", &self.seg3),
            ("sleep error", &self.sleep_err),
            ("deadline→next T0 (误差+③)", &self.wake_total),
            ("  (诊断) seg① 距上次发送<100ns", &self.seg1_by_gap[0]),
            ("  (诊断) seg① 100–250ns", &self.seg1_by_gap[1]),
            ("  (诊断) seg① 250–500ns", &self.seg1_by_gap[2]),
            ("  (诊断) seg① 500ns–2µs", &self.seg1_by_gap[3]),
            ("  (诊断) seg① ≥2µs", &self.seg1_by_gap[4]),
            #[cfg(feature = "probe")]
            ("  (probe) ①取 mbuf", &self.probe_send[0]),
            #[cfg(feature = "probe")]
            ("  (probe) ①写包", &self.probe_send[1]),
            #[cfg(feature = "probe")]
            ("  (probe) ①tx_burst", &self.probe_send[2]),
            ("  (诊断) 维护: rte_timer_manage", &self.house_timer),
            ("  (诊断) 维护: 整个节拍", &self.house_total),
        ]
        .into_iter()
        .map(|(name, h)| MetricRow::from_hist(name, h, hz, step))
        .collect()
    }
}

/// 一行报表（单位：纳秒）。
#[derive(Debug, Clone, Serialize)]
pub struct MetricRow {
    pub name: String,
    pub count: u64,
    pub min: u64,
    pub mean: f64,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub p99_9: u64,
    pub p99_99: u64,
    pub max: u64,
    /// 插值分位数（ns，带小数）：不受"时间戳每 10 ns 才跳一步"的限制，见 `Hist::quantile_interp`。
    /// 只写进 JSON，供离线对比 A − B 时使用；屏幕上的表仍是上面的普通分位数。
    pub p50_interp: f64,
    pub p90_interp: f64,
    pub p99_interp: f64,
    pub p99_9_interp: f64,
    pub p99_99_interp: f64,
}

impl MetricRow {
    fn from_hist(name: &str, h: &Hist, hz: u64, step: u64) -> MetricRow {
        let ns = |c: u64| cycles_to_ns(c, hz);
        let interp = |q: f64| (h.quantile_interp(q, step) * 1e10 / hz as f64).round() / 10.0;
        MetricRow {
            name: name.to_string(),
            count: h.count(),
            min: ns(h.min()),
            mean: h.mean() * 1e9 / hz as f64,
            p50: ns(h.quantile(0.50)),
            p90: ns(h.quantile(0.90)),
            p99: ns(h.quantile(0.99)),
            p99_9: ns(h.quantile(0.999)),
            p99_99: ns(h.quantile(0.9999)),
            max: ns(h.max()),
            p50_interp: interp(0.50),
            p90_interp: interp(0.90),
            p99_interp: interp(0.99),
            p99_9_interp: interp(0.999),
            p99_99_interp: interp(0.9999),
        }
    }
}

/// 完整报告（打印 + JSON）。
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub client: String,
    pub sessions: u16,
    pub delay_us: u64,
    pub duration_sec: u64,
    pub payload: usize,
    pub timeout_us: u64,
    pub elapsed_sec: f64,
    pub tsc_hz: u64,
    pub counters: Counters,
    pub metrics: Vec<MetricRow>,
    pub burst_sizes: Vec<(usize, u64)>,
    pub port: PortSummary,
    pub mbuf: LeakReport,
    pub exit_reason: String,
    /// 异常包明细（最多 16 条）
    pub anomalies: Vec<String>,
    /// `probe` 构建的附加诊断（普通构建为空）
    pub probe_notes: Vec<String>,
    /// 主循环空转停顿（空轮询间隔超过阈值）：次数、累计时长、最长一次
    pub stalls: StallSummary,
    /// 段① ≥ [`SEG1_SLOW_NS`] 的样本占比（%）。段①的分布是两段式的：绝大多数约 50 ns，少数（等网卡）在 150 ns 以上，
    /// 中间几乎没有样本。所以段①的 p99 落在哪一段，只取决于这个占比在 1% 的哪一边
    pub seg1_slow_percent: f64,
    /// 启用的诊断开关（非空 = 这份结果**不参与排名**）
    pub diag: Vec<String>,
    /// `--samples`：原始样本文件
    pub samples: Option<SamplesInfo>,
    /// 构建信息与运行环境
    pub env: EnvInfo,
}

#[derive(Debug, Clone, Serialize)]
pub struct SamplesInfo {
    pub path: String,
    pub count: u64,
    pub capacity: u64,
    /// 缓冲区写满之后没有记录下来的样本数
    pub dropped: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct StallSummary {
    /// 空轮询停顿
    pub threshold_ns: u64,
    pub count: u64,
    pub total_ns: u64,
    pub max_ns: u64,
    /// 取包前停顿
    pub rx_threshold_ns: u64,
    pub rx_count: u64,
    pub rx_total_ns: u64,
    pub rx_max_ns: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PortSummary {
    pub ipackets: u64,
    pub opackets: u64,
    pub imissed: u64,
    pub ierrors: u64,
    pub oerrors: u64,
    pub rx_nombuf: u64,
    /// ENA 的 AWS 限额计数器（超限会静默丢包）
    pub allowance_exceeded: Vec<(String, u64)>,
}

impl Report {
    pub fn print(&self) {
        let c = &self.counters;
        println!();
        println!("==================== {} ====================", self.client);
        if !self.diag.is_empty() {
            println!("★ 诊断运行（{}）：这份结果不参与 A − B 排名", self.diag.join("、"));
        }
        println!(
            "sessions {}  delay {} µs  duration {} s (实际 {:.2} s)  payload {} B  timeout {} µs  TSC {:.3} GHz",
            self.sessions, self.delay_us, self.duration_sec, self.elapsed_sec, self.payload, self.timeout_us,
            self.tsc_hz as f64 / 1e9
        );
        println!(
            "sent {}  received {}  timeouts {}  late {}  in-flight-at-end {}  unexpected {}  foreign {}  tsc-mismatch {}  other {}  arp-replied {}  tx-full {}  no-mbuf {}",
            c.sent, c.received, c.timeouts, c.late, c.in_flight_at_end, c.unexpected, c.foreign, c.tsc_mismatch,
            c.other_rx, c.arp_replies, c.tx_full, c.no_mbuf
        );
        let lost = c.sent as i64 - c.received as i64 - c.timeouts as i64 - c.in_flight_at_end as i64;
        println!(
            "对账：sent − received − timeouts − in-flight = {lost}（应为 0）；丢包 = timeouts = {}（其中 {} 个迟到收到）",
            c.timeouts, c.late
        );
        let samples = self.metrics.first().map_or(0, |m| m.count);
        if samples != c.received - c.tsc_mismatch {
            println!("样本对账：延迟样本 {samples} ≠ received − tsc-mismatch = {}（提前退出时，尚在 sleep 的 session 手里的回复不会被记录）", c.received - c.tsc_mismatch);
        }
        // 收到的每个包必须恰好落入一类
        let rx_diff = c.rx_pkts as i64
            - (c.received + c.late + c.unexpected + c.foreign + c.other_rx + c.arp_replies) as i64;
        println!(
            "收包对账：rx {} − (received + late + unexpected + foreign + other + arp) = {rx_diff}（应为 0）",
            c.rx_pkts
        );
        println!();
        println!(
            "{:<28} {:>10} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>10}",
            "metric (ns)", "count", "min", "mean", "p50", "p90", "p99", "p99.9", "p99.99", "max"
        );
        for r in &self.metrics {
            println!(
                "{:<28} {:>10} {:>8} {:>8.0} {:>8} {:>8} {:>8} {:>8} {:>8} {:>10}",
                r.name, r.count, r.min, r.mean, r.p50, r.p90, r.p99, r.p99_9, r.p99_99, r.max
            );
        }
        println!();
        println!(
            "段① ≥ {SEG1_SLOW_NS} ns 的样本占 {:.3}%（段①是两段式分布，它的 p99 落在哪一段只看这个占比在 1% 的哪一边）",
            self.seg1_slow_percent
        );
        let total: u64 = self.burst_sizes.iter().map(|(_, n)| n).sum();
        let pct = |n: u64| 100.0 * n as f64 / total.max(1) as f64;
        let bs: Vec<String> =
            self.burst_sizes.iter().filter(|(_, n)| pct(*n) >= 0.01).map(|(k, n)| format!("{k}:{:.2}%", pct(*n))).collect();
        let rare: u64 = self.burst_sizes.iter().filter(|(_, n)| pct(*n) < 0.01).map(|(_, n)| n).sum();
        let biggest = self.burst_sizes.iter().map(|(k, _)| *k).max().unwrap_or(0);
        println!(
            "rx_burst 包数分布（非空 burst）：{}  其余更大的 burst 共 {rare} 次（最大一次 {biggest} 个包）",
            bs.join("  ")
        );
        let s = &self.stalls;
        println!(
            "主循环被外部打断：空轮询 > {} µs 共 {} 次（累计 {:.1} ms，最长 {:.1} µs）；取包前 > {} µs 共 {} 次（累计 {:.1} ms，最长 {:.1} µs）",
            s.threshold_ns / 1000,
            s.count,
            s.total_ns as f64 / 1e6,
            s.max_ns as f64 / 1e3,
            s.rx_threshold_ns / 1000,
            s.rx_count,
            s.rx_total_ns as f64 / 1e6,
            s.rx_max_ns as f64 / 1e3
        );
        let p = &self.port;
        println!(
            "port: ipackets {}  opackets {}  imissed {}  ierrors {}  oerrors {}  rx_nombuf {}",
            p.ipackets, p.opackets, p.imissed, p.ierrors, p.oerrors, p.rx_nombuf
        );
        let ax: Vec<String> = p.allowance_exceeded.iter().map(|(k, v)| format!("{k}={v}")).collect();
        println!("AWS allowance（本次运行增量）：{}", ax.join("  "));
        let m = &self.mbuf;
        let leak = m.avail_initial as i64 - m.avail_final as i64;
        println!(
            "mbuf：pool {}，初值 avail {}（端口启动后 {}），关停后 avail {} → 泄漏 {} {}",
            m.pool_size,
            m.avail_initial,
            m.avail_after_start,
            m.avail_final,
            leak,
            if leak == 0 { "✔" } else { "✘" }
        );
        println!("退出原因：{}", self.exit_reason);
        self.env.print();
        if let Some(s) = &self.samples {
            println!(
                "原始样本：{} 个 → {}（容量 {}{}）",
                s.count,
                s.path,
                s.capacity,
                if s.dropped > 0 { format!("，写满后有 {} 个未记录", s.dropped) } else { String::new() }
            );
        }
        for n in &self.probe_notes {
            println!("{n}");
        }
        if !self.anomalies.is_empty() {
            println!("异常包明细（最多 16 条）：");
            for a in &self.anomalies {
                println!("  {a}");
            }
        }
    }
}

/// 读取端口计数器，并计算 AWS allowance 计数器在本次运行中的增量（超限 = 被 AWS 静默丢包）。
pub fn port_summary(port: &Port, xstats_before: &[(String, u64)]) -> PortSummary {
    let st = port.stats();
    let after = port.xstats();
    let allowance_exceeded = after
        .iter()
        .filter(|(k, _)| k.contains("allowance_exceeded"))
        .map(|(k, v)| {
            let b = xstats_before.iter().find(|(kb, _)| kb == k).map_or(0, |(_, v)| *v);
            (k.clone(), v.saturating_sub(b))
        })
        .collect();
    PortSummary {
        ipackets: st.ipackets,
        opackets: st.opackets,
        imissed: st.imissed,
        ierrors: st.ierrors,
        oerrors: st.oerrors,
        rx_nombuf: st.rx_nombuf,
        allowance_exceeded,
    }
}

impl Report {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: &str,
        args: &Args,
        elapsed_sec: f64,
        stats: &Stats,
        tsc_hz: u64,
        port: PortSummary,
        mbuf: LeakReport,
        exit_reason: String,
        mut env: EnvInfo,
    ) -> Report {
        env.finish(tsc_hz);
        let samples = args.samples.as_ref().map(|p| {
            let n = stats.samples.len() as u64;
            SamplesInfo {
                path: p.display().to_string(),
                count: n,
                capacity: stats.samples.capacity() as u64,
                dropped: stats.inproc.count().saturating_sub(n),
            }
        });
        Report {
            client: client.to_string(),
            sessions: args.sessions,
            delay_us: args.delay_us,
            duration_sec: args.duration_sec,
            payload: args.payload,
            timeout_us: args.timeout_us,
            elapsed_sec,
            tsc_hz,
            counters: stats.c,
            metrics: stats.rows(tsc_hz, env.tsc_step_cycles),
            seg1_slow_percent: 100.0 * stats.seg1.fraction_at_or_above(dpdk::tsc::ns_to_cycles(SEG1_SLOW_NS, tsc_hz)),
            burst_sizes: stats.burst_sizes.iter().enumerate().skip(1).filter(|(_, n)| **n > 0).map(|(k, n)| (k, *n)).collect(),
            port,
            mbuf,
            exit_reason,
            anomalies: stats.anomalies.clone(),
            probe_notes: stats.probe_notes(),
            stalls: StallSummary {
                threshold_ns: STALL_THRESHOLD_NS,
                count: stats.stalls.count,
                total_ns: cycles_to_ns(stats.stalls.total, tsc_hz),
                max_ns: cycles_to_ns(stats.stalls.max, tsc_hz),
                rx_threshold_ns: STALL_RX_THRESHOLD_NS,
                rx_count: stats.stalls.rx_count,
                rx_total_ns: cycles_to_ns(stats.stalls.rx_total, tsc_hz),
                rx_max_ns: cycles_to_ns(stats.stalls.rx_max, tsc_hz),
            },
            diag: args.diag().iter().map(|d| d.name()).collect(),
            samples,
            env,
        }
    }

    /// 打印报告，并按需写 JSON、原始样本文件。
    pub fn emit(&self, json: Option<&std::path::Path>, stats: &Stats) {
        self.print();
        if let Some(s) = &self.samples {
            if let Err(e) = stats.samples.write_to(std::path::Path::new(&s.path), self.tsc_hz) {
                eprintln!("写原始样本失败：{e}");
            }
        }
        if let Some(p) = json {
            if let Err(e) = std::fs::write(p, serde_json::to_string_pretty(self).unwrap()) {
                eprintln!("写 JSON 失败：{e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples::{unpack, SampleLog};

    fn stamp(t0: u64, t1: u64) -> Stamp {
        Stamp { t0, t1, ..Stamp::default() }
    }

    #[test]
    fn on_reply_records_all_segments_and_the_raw_sample() {
        let mut st = Stats { samples: SampleLog::with_capacity(8), ..Stats::default() };
        st.on_reply(stamp(1_000, 1_130), 200_000, 200_338);
        assert_eq!((st.seg1.max(), st.seg2.max(), st.inproc.max()), (130, 338, 468));
        assert_eq!(st.e2e.max(), 199_338);
        assert_eq!(st.samples.as_slice().iter().map(|&v| unpack(v)).collect::<Vec<_>>(), vec![(130, 338, 200_000)]);
    }

    /// 回复带回的时间戳对不上：计数、留下明细、并且告诉调用方不要把它记进延迟分布。
    #[test]
    fn mismatched_echo_is_counted_and_excluded() {
        let mut st = Stats::default();
        let s = stamp(1_000, 1_130);
        assert!(st.verify_echo(s, 1_000, 3, 7));
        assert!(!st.verify_echo(s, 999, 3, 7));
        assert_eq!(st.c.tsc_mismatch, 1);
        assert_eq!(st.anomalies.len(), 1);
        assert!(st.anomalies[0].contains("id=3 seq=7"));
    }

    #[test]
    fn anomaly_log_is_bounded() {
        let mut st = Stats::default();
        for i in 0..1000 {
            st.note_anomaly(format_args!("#{i}"));
        }
        assert_eq!(st.anomalies.len(), ANOMALY_LOG_MAX);
    }

    #[test]
    fn wake_splits_into_sleep_error_and_seg3() {
        let mut st = Stats::default();
        st.on_wake(10_000, 10_040, 10_400);
        assert_eq!((st.sleep_err.max(), st.seg3.max(), st.wake_total.max()), (40, 360, 400));
    }
}
