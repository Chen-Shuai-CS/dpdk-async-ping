use clap::Parser;
use std::path::PathBuf;

/// A 与 B 的命令行参数完全相同（SPEC §6.1 / §6.2）。
/// 网卡相关参数默认从环境变量读取（scripts/run.sh 会 source config/nic.env）。
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
    #[arg(long, env = "DPDK_PCI")]
    pub pci: String,

    /// 本端 IP（必须是这张 ENI 的地址，AWS 会做源地址检查）
    #[arg(long, env = "DPDK_IP")]
    pub src_ip: String,

    /// 对端 IP
    #[arg(long, env = "PEER_IP")]
    pub dst_ip: String,

    /// 对端 MAC（SPEC §3.1 给定，因此不需要发 ARP 请求）
    #[arg(long, env = "PEER_MAC")]
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
