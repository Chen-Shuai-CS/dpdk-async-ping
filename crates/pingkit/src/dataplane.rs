use crate::Args;
use dpdk::{Eal, Mempool, Port};
use pingproto::{parse_ipv4, parse_mac, EchoTemplate, Endpoints};
use std::time::{Duration, Instant};

/// mempool 每个 lcore 的 cache 大小。
const MEMPOOL_CACHE: u32 = 256;
/// mbuf 数据区大小（DPDK 默认 2048 + 128 headroom）。
const DATA_ROOM: u16 = 2048 + 128;

/// 初始化好的数据面：EAL、mbuf 池、已启动的端口、帧模板。A 和 B 用同一套初始化代码。
pub struct Dataplane {
    pub eal: Eal,
    pub pool: &'static Mempool,
    pub port: Port,
    pub endpoints: Endpoints,
    pub tmpl: EchoTemplate,
    /// TSC 频率（Hz）
    pub hz: u64,
    /// mempool 刚创建、端口尚未启动时的可用数 —— 零泄漏核对的"初值"
    pub avail_initial: u32,
    /// 端口启动后（RX 环已预投递）的可用数，仅供报告参考
    pub avail_after_start: u32,
    /// 单实例锁：持有到进程结束
    _lock: Option<std::fs::File>,
}

/// 同一张网卡同一时刻只允许一个进程驱动。
///
/// igb_uio 不阻止第二个进程再次打开同一个设备；两个进程各自初始化队列、各自敲 doorbell，
/// 会互相破坏对方的收发环（表现为丢包、收到别人的包，甚至网卡 reset）。所以启动时先拿一把文件锁，拿不到就拒绝启动。
/// 锁随进程结束自动释放（包括崩溃、被 kill），不会留下需要手工清理的残留。
fn instance_lock(pci: &str) -> Result<Option<std::fs::File>, String> {
    if !std::path::Path::new(&format!("/sys/bus/pci/devices/{pci}")).exists() {
        return Ok(None); // 设备不存在：不留下无意义的锁文件，后面的 EAL 初始化自会报错
    }
    let path = format!("/run/bqping-{pci}.lock");
    let Ok(f) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path) else {
        return Ok(None); // 没有权限创建锁文件（非 root）：后面的 EAL 初始化自会报错
    };
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(std::fs::TryLockError::WouldBlock) => Err(format!(
            "网卡 {pci} 正被另一个 async-ping / raw-ping 进程使用（锁文件 {path}）。两个进程同时驱动同一张网卡会互相破坏收发队列，已拒绝启动"
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("无法锁定 {path}：{e}")),
    }
}

/// 零 mbuf 泄漏核对结果。
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct LeakReport {
    pub pool_size: u32,
    pub avail_initial: u32,
    pub avail_after_start: u32,
    pub avail_final: u32,
    pub in_use_final: u32,
}

impl LeakReport {
    pub fn leaked(&self) -> i64 {
        self.avail_initial as i64 - self.avail_final as i64
    }
}

impl Dataplane {
    pub fn open(args: &Args) -> Result<Dataplane, String> {
        let dst_mac = parse_mac(&args.dst_mac).ok_or("对端 MAC 格式错误")?;
        let src_ip = parse_ipv4(&args.src_ip).ok_or("本端 IP 格式错误")?;
        let dst_ip = parse_ipv4(&args.dst_ip).ok_or("对端 IP 格式错误")?;

        let lock = instance_lock(&args.pci)?;
        let eal = Eal::init(&args.eal_args()).map_err(|e| e.to_string())?;
        let pool = Mempool::create_pktmbuf_pool("bq_pool", args.mbufs, MEMPOOL_CACHE, DATA_ROOM, 0)
            .map_err(|e| e.to_string())?;
        let avail_initial = pool.avail_count();
        let port = Port::configure(&eal, 0, pool, args.rxd, args.txd).map_err(|e| e.to_string())?;
        port.start().map_err(|e| e.to_string())?;
        let t = Instant::now();
        while !port.link_up() {
            if t.elapsed() > Duration::from_secs(10) {
                return Err("10 秒内端口 link 未 up".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let avail_after_start = pool.avail_count();
        let endpoints = Endpoints { src_mac: port.mac(), dst_mac, src_ip, dst_ip };
        let tmpl = EchoTemplate::new(&endpoints, args.payload);
        Ok(Dataplane { eal, pool, port, endpoints, tmpl, hz: dpdk::tsc::hz(), avail_initial, avail_after_start, _lock: lock })
    }

    /// 关停端口并核对 mbuf。**调用前必须已经 drop 掉程序持有的所有 Mbuf**（task、信箱、held reply）。
    ///
    /// 顺序：回收 TX 环上已完成的 mbuf → stop（PMD 归还 RX 环与 TX 环上剩余的 mbuf）→ 读 avail → close。
    /// 返回 EAL 凭证，由调用者在一切结束后决定是否 cleanup。
    pub fn shutdown(self) -> (Eal, LeakReport) {
        let _ = self.port.tx_done_cleanup(0);
        if let Err(e) = self.port.stop() {
            eprintln!("警告：{e}");
        }
        let rep = LeakReport {
            pool_size: self.pool.size(),
            avail_initial: self.avail_initial,
            avail_after_start: self.avail_after_start,
            avail_final: self.pool.avail_count(),
            in_use_final: self.pool.in_use_count(),
        };
        if let Err(e) = self.port.close() {
            eprintln!("警告：{e}");
        }
        (self.eal, rep)
    }
}
