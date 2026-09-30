use crate::dataplane::LeakReport;
use crate::Args;
use dpdk::Port;
use crate::hist::Hist;
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
    /// 诊断：段① 按"距上一次发送多久"分档：<100 ns / 100–250 / 250–500 / 500 ns–2 µs / ≥2 µs。
    /// ENA 每次发送前有一次 sfence，要等上一个包的写合并缓冲排空，所以间隔越短段①越长。
    pub seg1_by_gap: [Hist; 5],
    /// 分档边界（TSC 周期），见 [`Stats::with_hz`]
    pub gap_edges: [u64; 4],
    /// 非空 rx_burst 的包数分布（下标 = 一次收到几个包）
    pub burst_sizes: [u64; RX_BURST_MAX + 1],
    pub c: Counters,
    /// 异常包明细（unexpected 等），最多记录 [`ANOMALY_LOG_MAX`] 条，冷路径
    pub anomalies: Vec<String>,
}

pub const ANOMALY_LOG_MAX: usize = 16;

impl Default for Stats {
    fn default() -> Self {
        Stats {
            seg1: Hist::default(),
            seg2: Hist::default(),
            inproc: Hist::default(),
            e2e: Hist::default(),
            seg3: Hist::default(),
            sleep_err: Hist::default(),
            seg1_by_gap: Default::default(),
            gap_edges: [260, 650, 1_300, 5_200], // 100/250/500/2000 ns @ 2.6 GHz；with_hz 按实际频率重设
            burst_sizes: [0; RX_BURST_MAX + 1],
            c: Counters::default(),
            anomalies: Vec::new(),
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
        self.seg2.record(s2);
        self.inproc.record(s1 + s2);
        self.e2e.record(t3.saturating_sub(s.t0));
    }

    /// 一次 sleep 结束：deadline、timer 发现到期的时刻、随后的下一个 T0。
    #[inline]
    pub fn on_wake(&mut self, deadline: u64, detected: u64, t0_next: u64) {
        self.sleep_err.record(detected.saturating_sub(deadline));
        self.seg3.record(t0_next.saturating_sub(detected));
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

    pub fn rows(&self, hz: u64) -> Vec<MetricRow> {
        [
            ("in-process ①+②  [排名指标]", &self.inproc),
            ("seg① send  T1−T0", &self.seg1),
            ("seg② recv  T3−T2", &self.seg2),
            ("end-to-end T3−T0", &self.e2e),
            ("seg③ wake→next T0", &self.seg3),
            ("sleep error", &self.sleep_err),
            ("  (诊断) seg① 距上次发送<100ns", &self.seg1_by_gap[0]),
            ("  (诊断) seg① 100–250ns", &self.seg1_by_gap[1]),
            ("  (诊断) seg① 250–500ns", &self.seg1_by_gap[2]),
            ("  (诊断) seg① 500ns–2µs", &self.seg1_by_gap[3]),
            ("  (诊断) seg① ≥2µs", &self.seg1_by_gap[4]),
        ]
        .into_iter()
        .map(|(name, h)| MetricRow::from_hist(name, h, hz))
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
}

impl MetricRow {
    fn from_hist(name: &str, h: &Hist, hz: u64) -> MetricRow {
        let ns = |c: u64| cycles_to_ns(c, hz);
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
        println!(
            "sessions {}  delay {} µs  duration {} s (实际 {:.2} s)  payload {} B  timeout {} µs  TSC {:.3} GHz",
            self.sessions, self.delay_us, self.duration_sec, self.elapsed_sec, self.payload, self.timeout_us,
            self.tsc_hz as f64 / 1e9
        );
        println!(
            "sent {}  received {}  timeouts {}  late {}  in-flight-at-end {}  unexpected {}  other {}  arp-replied {}  tx-full {}  no-mbuf {}",
            c.sent, c.received, c.timeouts, c.late, c.in_flight_at_end, c.unexpected, c.other_rx, c.arp_replies,
            c.tx_full, c.no_mbuf
        );
        let lost = c.sent as i64 - c.received as i64 - c.timeouts as i64 - c.in_flight_at_end as i64;
        println!(
            "对账：sent − received − timeouts − in-flight = {lost}（应为 0）；丢包 = timeouts = {}（其中 {} 个迟到收到）",
            c.timeouts, c.late
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
        let total: u64 = self.burst_sizes.iter().map(|(_, n)| n).sum();
        let bs: Vec<String> = self
            .burst_sizes
            .iter()
            .map(|(k, n)| format!("{k}:{:.2}%", 100.0 * *n as f64 / total.max(1) as f64))
            .collect();
        println!("rx_burst 包数分布（非空 burst）：{}", bs.join("  "));
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
    ) -> Report {
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
            metrics: stats.rows(tsc_hz),
            burst_sizes: stats.burst_sizes.iter().enumerate().skip(1).filter(|(_, n)| **n > 0).map(|(k, n)| (k, *n)).collect(),
            port,
            mbuf,
            exit_reason,
            anomalies: stats.anomalies.clone(),
        }
    }

    /// 打印报告，并按需写 JSON。
    pub fn emit(&self, json: Option<&std::path::Path>) {
        self.print();
        if let Some(p) = json {
            if let Err(e) = std::fs::write(p, serde_json::to_string_pretty(self).unwrap()) {
                eprintln!("写 JSON 失败：{e}");
            }
        }
    }
}
