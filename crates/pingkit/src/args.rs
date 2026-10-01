use clap::Parser;
use std::path::PathBuf;

/// A 与 B 的命令行参数完全相同（SPEC §6.1 / §6.2）。
/// 网卡相关参数的来源依次是：命令行 → 环境变量 → `config/nic.env`（由 scripts/detect-nic.sh 生成）。
/// 所以在仓库根目录直接运行 `sudo target/release/async-ping --delay-us 500 --duration-sec 60` 也可以。
#[derive(Parser, Debug, Clone)]
#[command(version, about)]
pub struct Args {
    /// 每个 session 收到 reply 后 sleep 多少微秒再发下一个（推荐 500）
    #[arg(long)]
    pub delay_us: u64,

    /// 运行多少秒后自动停止、输出完整统计、干净退出
    #[arg(long)]
    pub duration_sec: u64,

    /// 并发 session 数（ICMP id = 0..sessions）
    #[arg(long, default_value_t = 64)]
    pub sessions: u16,

    /// ICMP payload 字节数（前 8 字节是发送时刻 TSC）
    #[arg(long, default_value_t = 64)]
    pub payload: usize,

    /// reply 超时（微秒）。超时的请求计为丢失，不进入延迟分布；之后到达的 reply 计为"迟到"
    #[arg(long, default_value_t = 10_000)]
    pub timeout_us: u64,

    /// DPDK 网卡 PCI 地址
    #[arg(long, env = "DPDK_PCI", default_value = "")]
    pub pci: String,

    /// 本端 IP（必须是这张 ENI 的地址，AWS 会做源地址检查）
    #[arg(long, env = "DPDK_IP", default_value = "")]
    pub src_ip: String,

    /// 对端 IP
    #[arg(long, env = "PEER_IP", default_value = "")]
    pub dst_ip: String,

    /// 对端 MAC（SPEC §3.1 给定，因此不需要发 ARP 请求）
    #[arg(long, env = "PEER_MAC", default_value = "")]
    pub dst_mac: String,

    /// runtime / 主循环所在的核（已被 isolcpus 隔离）
    #[arg(long, env = "DPDK_LCORE", default_value_t = 3)]
    pub lcore: u32,

    /// 进度上报线程所在的核
    #[arg(long, default_value_t = 1)]
    pub report_core: usize,

    /// 进度输出间隔（秒），0 = 不输出
    #[arg(long, default_value_t = 5)]
    pub progress_sec: u64,

    /// 把最终报告另存为 JSON
    #[arg(long)]
    pub json: Option<PathBuf>,

    /// 把每个样本的原始值（段①、段②、T2）另存为二进制文件，供 scripts/ci.py 做置信区间等离线分析。
    /// 缓冲区启动时一次分配好，运行中不分配；默认不存
    #[arg(long)]
    pub samples: Option<PathBuf>,

    /// 诊断，结果不参与排名：读 T0 之前先做一件事。用来查明"紧跟在上一次发送之后的发送为什么慢"、
    /// 以及把这段等待移到段①之外后 A − B 是多少。
    /// sfence：只约束写入顺序；mfence：等此前所有写入真正完成；stores：往栈上写 --diag-stores 个字（不等任何东西）
    #[arg(long, value_enum)]
    pub diag_pre_t0: Option<PreT0>,

    /// `--diag-pre-t0 stores` 写多少个 8 字节的字
    #[arg(long, default_value_t = 128)]
    pub diag_stores: usize,

    /// RX / TX 描述符数量
    #[arg(long, default_value_t = 1024)]
    pub rxd: u16,
    #[arg(long, default_value_t = 1024)]
    pub txd: u16,

    /// mempool 大小（2^k − 1）
    #[arg(long, default_value_t = 8191)]
    pub mbufs: u32,

    /// 追加给 EAL 的参数（空格分隔）
    #[arg(long, default_value = "")]
    pub eal_extra: String,
}

/// `--diag-pre-t0` 的取值。
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreT0 {
    Sfence,
    Mfence,
    Stores,
}

/// `--diag-pre-t0` 解析后的形式（`stores` 带上要写的字数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Diag {
    Sfence,
    Mfence,
    Stores(usize),
}

/// `stores` 模式最多写这么多个字（一个固定大小的栈上数组）。
pub const DIAG_STORES_MAX: usize = 512;

impl Diag {
    /// 冷路径：只有打开诊断开关时才会走到。
    #[cold]
    #[inline(never)]
    pub fn run(self) {
        match self {
            Diag::Sfence => dpdk::tsc::sfence(),
            Diag::Mfence => dpdk::tsc::mfence(),
            Diag::Stores(n) => {
                // 连续写 n 个互不相同的地址：每次写入在 CPU 的写入队列（store queue）里占一个位置。
                // 这里不等待任何东西；如果队首被一次很慢的设备写入堵着，队列被填满后 CPU 才不得不停下来等。
                // 数组不做初始化（否则清零本身就是几十上百次写入，n 就不是唯一的变量了）。
                let mut buf = [const { std::mem::MaybeUninit::<u64>::uninit() }; DIAG_STORES_MAX];
                for (i, slot) in buf.iter_mut().take(n).enumerate() {
                    // SAFETY: slot 指向数组里一个有效、独占的位置；用 volatile 写是为了不让编译器把这些写入合并或删掉。
                    unsafe { std::ptr::write_volatile(slot.as_mut_ptr(), i as u64) };
                }
                std::hint::black_box(&buf);
            }
        }
    }

    pub fn name(self) -> String {
        match self {
            Diag::Sfence => "sfence-before-t0".into(),
            Diag::Mfence => "mfence-before-t0".into(),
            Diag::Stores(n) => format!("{n}-stores-before-t0"),
        }
    }
}

impl Args {
    pub fn eal_args(&self) -> Vec<String> {
        let mut v: Vec<String> = [
            "-l", &self.lcore.to_string(),
            "-a", &self.pci,
            "--file-prefix", "bqping",
            "--in-memory", // 大页用 memfd，不在 /dev/hugepages 留文件；进程退出即全部释放
            "--log-level", "*:notice",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        v.extend(self.eal_extra.split_whitespace().map(String::from));
        v
    }
}

impl Args {
    /// 诊断开关（默认 None）。
    pub fn diag(&self) -> Option<Diag> {
        self.diag_pre_t0.map(|d| match d {
            PreT0::Sfence => Diag::Sfence,
            PreT0::Mfence => Diag::Mfence,
            PreT0::Stores => Diag::Stores(self.diag_stores.min(DIAG_STORES_MAX)),
        })
    }

    /// `--samples` 的缓冲区容量（样本数）；没开则为 0。
    /// 按"每个 session 每 (delay + 20 µs) 完成一个请求"估上限（实际往返至少 60 µs），再留 5% 余量。
    pub fn sample_capacity(&self) -> usize {
        if self.samples.is_none() {
            return 0;
        }
        let per_sec = self.sessions as u64 * 1_000_000 / (self.delay_us + 20);
        let cap = (per_sec * self.duration_sec) as usize / 20 * 21 + 4096;
        cap.min(crate::samples::MAX_SAMPLES)
    }

    /// 解析命令行，补全网卡参数，检查合法性；出错则打印原因并以退出码 2 结束。
    pub fn load() -> Args {
        let mut a = Args::parse();
        if let Err(e) = a.resolve() {
            eprintln!("参数错误：{e}");
            std::process::exit(2);
        }
        a
    }

    fn resolve(&mut self) -> Result<(), String> {
        if [&self.pci, &self.src_ip, &self.dst_ip, &self.dst_mac].iter().any(|s| s.is_empty()) {
            if let Some(kv) = read_nic_env() {
                let get = |k: &str| kv.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
                for (field, key) in [
                    (&mut self.pci, "DPDK_PCI"),
                    (&mut self.src_ip, "DPDK_IP"),
                    (&mut self.dst_ip, "PEER_IP"),
                    (&mut self.dst_mac, "PEER_MAC"),
                ] {
                    if field.is_empty() {
                        *field = get(key).unwrap_or_default();
                    }
                }
            }
        }
        for (v, flag) in [(&self.pci, "--pci"), (&self.src_ip, "--src-ip"), (&self.dst_ip, "--dst-ip"), (&self.dst_mac, "--dst-mac")] {
            if v.is_empty() {
                return Err(format!("缺少 {flag}（也没有找到 config/nic.env；可先运行 scripts/detect-nic.sh）"));
            }
        }
        if self.sessions == 0 {
            return Err("--sessions 至少为 1".into());
        }
        if !(8..=1472).contains(&self.payload) {
            return Err("--payload 需在 8..=1472 之间（前 8 字节放发送时刻 TSC；上限由 1500 的 MTU 决定）".into());
        }
        if self.duration_sec == 0 || self.timeout_us == 0 {
            return Err("--duration-sec 与 --timeout-us 必须大于 0".into());
        }
        // RX 环预投递 + TX 环在途 + 每个 session 同时最多占 2 个（持有的 reply + 在途的 request）+ lcore cache
        // （故障注入脚本用 BQ_FAULT_SKIP_MBUF_CHECK=1 跳过这项检查，故意制造 mbuf 耗尽）
        let need = self.rxd as u32 + self.txd as u32 + 2 * self.sessions as u32 + 512;
        if self.mbufs < need && std::env::var_os("BQ_FAULT_SKIP_MBUF_CHECK").is_none() {
            return Err(format!("--mbufs {} 太小：{} 个 session 至少需要 {need}", self.mbufs, self.sessions));
        }
        Ok(())
    }
}

/// 读取 nic.env：`KEY=VALUE  # 注释`。查找顺序：$BQ_NIC_ENV → ./config/nic.env → <可执行文件>/../../config/nic.env。
fn read_nic_env() -> Option<Vec<(String, String)>> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("BQ_NIC_ENV") {
        candidates.push(p.into());
    }
    candidates.push("config/nic.env".into());
    if let Ok(exe) = std::env::current_exe() {
        if let Some(root) = exe.ancestors().nth(3) {
            candidates.push(root.join("config/nic.env"));
        }
    }
    let text = candidates.iter().find_map(|p| std::fs::read_to_string(p).ok())?;
    Some(
        text.lines()
            .filter_map(|l| {
                let l = l.split('#').next()?.trim();
                let (k, v) = l.split_once('=')?;
                Some((k.trim().to_string(), v.trim().to_string()))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(extra: &[&str]) -> Result<Args, String> {
        let mut argv = vec!["x", "--pci", "0000:00:00.0", "--src-ip", "10.0.0.1", "--dst-ip", "10.0.0.2", "--dst-mac", "02:00:00:00:00:01"];
        for (flag, default) in [("--delay-us", "500"), ("--duration-sec", "60")] {
            if !extra.contains(&flag) {
                argv.extend([flag, default]);
            }
        }
        argv.extend(extra);
        let mut a = Args::try_parse_from(argv).map_err(|e| e.to_string())?;
        a.resolve()?;
        Ok(a)
    }

    #[test]
    fn defaults_are_valid() {
        let a = args(&[]).unwrap();
        assert_eq!((a.sessions, a.payload, a.timeout_us), (64, 64, 10_000));
        assert!(a.diag_pre_t0.is_none() && a.samples.is_none() && a.sample_capacity() == 0);
        assert_eq!(a.diag(), None);
        assert_eq!(args(&["--diag-pre-t0", "mfence"]).unwrap().diag(), Some(Diag::Mfence));
        assert_eq!(args(&["--diag-pre-t0", "stores"]).unwrap().diag(), Some(Diag::Stores(128)));
        assert_eq!(args(&["--diag-pre-t0", "stores", "--diag-stores", "9999"]).unwrap().diag(), Some(Diag::Stores(DIAG_STORES_MAX)));
        Diag::Stores(64).run();
        assert!(args(&["--diag-pre-t0", "bogus"]).is_err());
    }

    #[test]
    fn boundary_values() {
        assert!(args(&["--sessions", "0"]).is_err());
        assert!(args(&["--sessions", "1"]).is_ok());
        assert!(args(&["--payload", "7"]).is_err());
        assert!(args(&["--payload", "8"]).is_ok());
        assert!(args(&["--payload", "1472"]).is_ok());
        assert!(args(&["--payload", "1473"]).is_err());
        assert!(args(&["--timeout-us", "0"]).is_err());
        assert!(args(&["--delay-us", "0"]).is_ok());
        // mempool 至少要装得下 RX 环 + TX 环 + 每个 session 2 个 + lcore cache
        assert!(args(&["--mbufs", "2687"]).is_err());
        assert!(args(&["--mbufs", "2688"]).is_ok());
    }

    #[test]
    fn sample_buffer_is_sized_from_the_request_rate_and_capped() {
        let a = args(&["--samples", "/tmp/x.bin"]).unwrap();
        // 64 session、每个最多每 520 µs 一个请求、60 秒 → 约 738 万，再加 5% 余量
        let cap = a.sample_capacity();
        assert!((7_700_000..7_800_000).contains(&cap), "{cap}");
        let a = args(&["--samples", "/tmp/x.bin", "--delay-us", "0", "--duration-sec", "100000"]).unwrap();
        assert_eq!(a.sample_capacity(), crate::samples::MAX_SAMPLES);
    }
}
