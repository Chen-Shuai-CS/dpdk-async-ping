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

impl Args {
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
        let need = self.rxd as u32 + self.txd as u32 + 2 * self.sessions as u32 + 512;
        if self.mbufs < need {
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
