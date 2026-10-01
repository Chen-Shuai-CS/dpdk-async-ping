//! 构建信息与运行环境，写进每份报告（JSON 的 `env` 字段）：任何一个数字都能追溯到
//! 确切的代码版本、编译器、DPDK、内核启动参数、网卡驱动，以及这次运行里时钟本身的质量。
//! 全部是冷路径：启动时采集一次，结束时补上时钟漂移。

use crate::Args;
use dpdk::tsc::{clock_read_cost, rdtsc};
use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct EnvInfo {
    /// 构建时的 git 提交；`git_dirty` = 构建时 crates/ 等源码路径有未提交的改动
    pub git_commit: String,
    pub git_dirty: bool,
    pub rustc: String,
    pub profile: String,
    pub features: Vec<String>,
    pub dpdk: String,
    pub kernel: String,
    pub kernel_cmdline: String,
    pub cpu_model: String,
    /// CPU 标志里与 TSC 有关的几项（constant_tsc / nonstop_tsc / rdtscp …）
    pub tsc_flags: Vec<String>,
    pub clocksource: String,
    pub nic_pci: String,
    pub nic_driver: String,
    /// 网卡的某个 BAR 是否以写合并（write-combining）方式映射 —— ENA 的 LLQ 发送路径依赖它
    pub nic_write_combining: bool,
    pub argv: Vec<String>,
    pub started_unix: u64,
    /// 读一次时钟（rdtscp）的平均成本：每个被测段都恰好包含一次
    pub clock_read_mean_ns: f64,
    /// 相邻两次读数之差的最小值、中位数（只能取步长的整数倍）
    pub clock_read_min_ns: f64,
    pub clock_read_median_ns: f64,
    /// TSC 读数的步长（周期 / 纳秒）= 时间戳的分辨率：所有时间差都是它的整数倍
    pub tsc_step_cycles: u64,
    pub tsc_step_ns: f64,
    /// 整个运行期间，"TSC 周期数 ÷ 标定频率"与内核 CLOCK_MONOTONIC_RAW 的相对偏差（ppm）：
    /// 周期 → 纳秒的换算有多准
    pub tsc_vs_monotonic_ppm: f64,
    #[serde(skip)]
    t0: (u64, u64),
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn monotonic_raw_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: 只写入传入的 timespec。
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}

/// 网卡的任一 BAR 是否出现在内核 PAT 表的 write-combining 条目里（需要 root 读 debugfs；读不到则为 false）。
fn write_combining(pci: &str) -> bool {
    let bars: Vec<(u64, u64)> = read(&format!("/sys/bus/pci/devices/{pci}/resource"))
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (a, b) = (hex(it.next()?)?, hex(it.next()?)?);
            (b > a).then_some((a, b))
        })
        .collect();
    read("/sys/kernel/debug/x86/pat_memtype_list").lines().filter(|l| l.contains("write-combining")).any(|l| {
        let start = l.split("[mem ").nth(1).and_then(|r| r.split('-').next()).and_then(hex);
        start.is_some_and(|s| bars.iter().any(|&(a, b)| a <= s && s <= b))
    })
}

impl EnvInfo {
    /// 在数据面初始化之后、运行开始之前调用（此时已在 lcore 上，时钟标定反映的就是被测的那个核）。
    pub fn collect(args: &Args, hz: u64) -> EnvInfo {
        let cpuinfo = read("/proc/cpuinfo");
        let field = |name: &str| {
            cpuinfo
                .lines()
                .find(|l| l.starts_with(name))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default()
        };
        let flags = field("flags");
        let cost = clock_read_cost(200_000);
        let ns = |c: f64| c * 1e9 / hz as f64;
        EnvInfo {
            git_commit: env!("BQ_GIT_COMMIT").into(),
            git_dirty: env!("BQ_GIT_DIRTY") == "true",
            rustc: env!("BQ_RUSTC").into(),
            profile: env!("BQ_PROFILE").into(),
            features: if cfg!(feature = "probe") { vec!["probe".into()] } else { Vec::new() },
            dpdk: dpdk::DPDK_VERSION.into(),
            kernel: read("/proc/sys/kernel/osrelease"),
            kernel_cmdline: read("/proc/cmdline"),
            cpu_model: field("model name"),
            tsc_flags: flags
                .split_whitespace()
                .filter(|f| matches!(*f, "constant_tsc" | "nonstop_tsc" | "rdtscp" | "tsc_reliable" | "tsc_known_freq"))
                .map(String::from)
                .collect(),
            clocksource: read("/sys/devices/system/clocksource/clocksource0/current_clocksource"),
            nic_pci: args.pci.clone(),
            nic_driver: std::fs::read_link(format!("/sys/bus/pci/devices/{}/driver", args.pci))
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default(),
            nic_write_combining: write_combining(&args.pci),
            argv: std::env::args().collect(),
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            clock_read_mean_ns: ns(cost.mean),
            clock_read_min_ns: ns(cost.min as f64),
            clock_read_median_ns: ns(cost.median as f64),
            tsc_step_cycles: cost.step,
            tsc_step_ns: ns(cost.step as f64),
            tsc_vs_monotonic_ppm: 0.0,
            t0: (rdtsc(), monotonic_raw_ns()),
        }
    }

    /// 运行结束后调用：补上 TSC 相对内核单调时钟的偏差。
    pub fn finish(&mut self, hz: u64) {
        let (c1, m1) = (rdtsc(), monotonic_raw_ns());
        let by_tsc = (c1 - self.t0.0) as f64 * 1e9 / hz as f64;
        let by_mono = (m1 - self.t0.1) as f64;
        if by_mono > 0.0 {
            self.tsc_vs_monotonic_ppm = (by_tsc - by_mono) / by_mono * 1e6;
        }
    }

    pub fn print(&self) {
        println!(
            "构建：commit {}{} · {} · DPDK {} · {}{}",
            self.git_commit,
            if self.git_dirty { "（构建时源码有未提交改动）" } else { "" },
            self.rustc,
            self.dpdk,
            self.profile,
            if self.features.is_empty() { String::new() } else { format!(" · features: {}", self.features.join(",")) }
        );
        println!(
            "环境：{} · 内核 {} · 网卡 {} 驱动 {}（写合并映射：{}）· clocksource {} · {}",
            self.cpu_model,
            self.kernel,
            self.nic_pci,
            self.nic_driver,
            if self.nic_write_combining { "是" } else { "未检测到" },
            self.clocksource,
            self.tsc_flags.join(" ")
        );
        println!(
            "时钟：读一次 rdtscp 平均 {:.1} ns（每个被测段恰好含一次）；TSC 读数步长 {} 周期 = {:.1} ns（时间戳分辨率，所有时间差都是它的整数倍）；周期→纳秒换算与 CLOCK_MONOTONIC_RAW 相差 {:+.1} ppm",
            self.clock_read_mean_ns, self.tsc_step_cycles, self.tsc_step_ns, self.tsc_vs_monotonic_ppm
        );
    }
}
