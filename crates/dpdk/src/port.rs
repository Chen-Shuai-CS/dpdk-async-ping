use crate::{Eal, Error, Mbuf, Mempool, Result};
use dpdk_sys::rte_mbuf;
use std::ffi::{c_int, c_void, CStr};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};

/// 一次 rx_burst 最多取多少个包。
pub const RX_BURST_MAX: usize = 32;

/// 网卡报告"需要 reset"（ENA watchdog 触发等）时由 EAL 中断线程置位，主循环轮询它。
static RESET_REQUESTED: AtomicBool = AtomicBool::new(false);

/// 一个以太网端口（固定使用 RX/TX 队列 0）。
///
/// 故意做成 `!Send + !Sync`：同一队列上的 rx_burst / tx_burst 不是线程安全的，
/// 只允许在创建它的那个 lcore 线程上使用。
pub struct Port {
    id: u16,
    socket: i32,
    _not_send: PhantomData<*const ()>,
}

/// 端口的基础计数器（`rte_eth_stats` 的子集）。
#[derive(Debug, Clone, Copy, Default)]
pub struct PortStats {
    pub ipackets: u64,
    pub opackets: u64,
    pub imissed: u64,
    pub ierrors: u64,
    pub oerrors: u64,
    pub rx_nombuf: u64,
}

impl Port {
    /// 配置端口 `id`：1 个 RX 队列（`nb_rxd` 个描述符，mbuf 取自 `pool`）+ 1 个 TX 队列。
    pub fn configure(_eal: &Eal, id: u16, pool: &'static Mempool, nb_rxd: u16, nb_txd: u16) -> Result<Port> {
        // SAFETY: 纯查询。
        if unsafe { dpdk_sys::rte_eth_dev_count_avail() } <= id {
            return Err(Error { what: "找不到 DPDK 端口（网卡是否已 bind？）", errno: 19 });
        }
        // SAFETY: 有效端口号。
        let socket = unsafe { dpdk_sys::rte_eth_dev_socket_id(id) }.max(0);
        let conf = dpdk_sys::rte_eth_conf::default(); // 不开任何 offload：IPv4 头是常量，ICMP 校验和网卡不支持
        // SAFETY: conf 在调用期间有效；DPDK 会拷贝它。
        let ret = unsafe { dpdk_sys::rte_eth_dev_configure(id, 1, 1, &conf) };
        if ret < 0 {
            return Err(Error::from_ret("rte_eth_dev_configure", ret));
        }
        // SAFETY: 端口已配置；rxconf/txconf 传 NULL 表示使用驱动默认值；pool 为 'static。
        let ret = unsafe {
            dpdk_sys::rte_eth_rx_queue_setup(id, 0, nb_rxd, socket as u32, std::ptr::null(), pool.as_ptr())
        };
        if ret < 0 {
            return Err(Error::from_ret("rte_eth_rx_queue_setup", ret));
        }
        // SAFETY: 同上。
        let ret = unsafe { dpdk_sys::rte_eth_tx_queue_setup(id, 0, nb_txd, socket as u32, std::ptr::null()) };
        if ret < 0 {
            return Err(Error::from_ret("rte_eth_tx_queue_setup", ret));
        }
        // SAFETY: 回调是一个只写原子变量的 extern "C" 函数，参数不使用。
        unsafe {
            dpdk_sys::rte_eth_dev_callback_register(
                id,
                dpdk_sys::rte_eth_event_type_RTE_ETH_EVENT_INTR_RESET,
                Some(on_reset_event),
                std::ptr::null_mut(),
            );
        }
        Ok(Port { id, socket, _not_send: PhantomData })
    }

    pub fn id(&self) -> u16 {
        self.id
    }

    pub fn socket(&self) -> i32 {
        self.socket
    }

    /// 启动端口：PMD 此时从 mempool 取一个 RX 环的 mbuf 把 RX 环填满。
    pub fn start(&self) -> Result<()> {
        // SAFETY: 端口已配置。
        let ret = unsafe { dpdk_sys::rte_eth_dev_start(self.id) };
        if ret < 0 { Err(Error::from_ret("rte_eth_dev_start", ret)) } else { Ok(()) }
    }

    /// 停止端口：PMD 把 RX 环、TX 环上的 mbuf 全部还给 mempool。
    pub fn stop(&self) -> Result<()> {
        // SAFETY: 有效端口；之后不会再 rx/tx 直到再次 start。
        let ret = unsafe { dpdk_sys::rte_eth_dev_stop(self.id) };
        if ret < 0 { Err(Error::from_ret("rte_eth_dev_stop", ret)) } else { Ok(()) }
    }

    pub fn close(self) -> Result<()> {
        // SAFETY: 端口已停止；self 被消耗，之后无法再使用。
        let ret = unsafe { dpdk_sys::rte_eth_dev_close(self.id) };
        if ret < 0 { Err(Error::from_ret("rte_eth_dev_close", ret)) } else { Ok(()) }
    }

    pub fn mac(&self) -> [u8; 6] {
        let mut a = dpdk_sys::rte_ether_addr::default();
        // SAFETY: a 是有效的输出缓冲区。
        unsafe { dpdk_sys::rte_eth_macaddr_get(self.id, &mut a) };
        a.addr_bytes
    }

    pub fn link_up(&self) -> bool {
        let mut l = dpdk_sys::rte_eth_link::default();
        // SAFETY: l 是有效的输出缓冲区。
        unsafe { dpdk_sys::rte_eth_link_get_nowait(self.id, &mut l) };
        // SAFETY: union 的 bitfield 视图是 POD。
        unsafe { l.__bindgen_anon_1.__bindgen_anon_1.link_status() == 1 }
    }

    /// 收一批包到 `burst`（最多 [`RX_BURST_MAX`] 个），返回收到的数量。非阻塞：没有包就返回 0。
    /// 如果 `burst` 里还有上一批没取走的包，会先释放它们（不会泄漏）。
    #[inline]
    pub fn rx_burst(&self, burst: &mut RxBurst) -> usize {
        burst.release_remaining();
        // SAFETY: ptrs 有 RX_BURST_MAX 个槽位；返回的 n 个指针由我们独占，交给 RxBurst 管理。
        let n = unsafe { dpdk_sys::shim_eth_rx_burst(self.id, 0, burst.ptrs.as_mut_ptr(), RX_BURST_MAX as u16) };
        burst.len = n;
        burst.pos = 0;
        n as usize
    }

    /// 发送一个包（tx_burst，1 个包，敲一次 doorbell）。
    /// 成功：所有权交给网卡（PMD 在发送完成后释放）。失败（TX 环满）：原样还给调用者。
    #[inline]
    pub fn tx(&self, m: Mbuf) -> std::result::Result<(), Mbuf> {
        let mut p = m.into_raw();
        // SAFETY: p 是有效且独占的 mbuf；tx_burst 接收它则所有权转移给 PMD。
        let n = unsafe { dpdk_sys::shim_eth_tx_burst(self.id, 0, &mut p, 1) };
        if n == 1 {
            Ok(())
        } else {
            // SAFETY: PMD 没有接收，所有权仍在我们手里。
            Err(unsafe { Mbuf::from_raw(p) })
        }
    }

    /// 主动回收已发送完成的 TX mbuf。返回回收数量；驱动不支持时返回 None。
    pub fn tx_done_cleanup(&self, max: u32) -> Option<u32> {
        // SAFETY: 有效端口/队列。
        let r = unsafe { dpdk_sys::rte_eth_tx_done_cleanup(self.id, 0, max) };
        if r < 0 { None } else { Some(r as u32) }
    }

    pub fn stats(&self) -> PortStats {
        let mut s = dpdk_sys::rte_eth_stats::default();
        // SAFETY: s 是有效输出缓冲区。
        unsafe { dpdk_sys::rte_eth_stats_get(self.id, &mut s) };
        PortStats {
            ipackets: s.ipackets,
            opackets: s.opackets,
            imissed: s.imissed,
            ierrors: s.ierrors,
            oerrors: s.oerrors,
            rx_nombuf: s.rx_nombuf,
        }
    }

    /// 扩展计数器（ENA 的 bw/pps allowance exceeded 等都在这里）。
    pub fn xstats(&self) -> Vec<(String, u64)> {
        // SAFETY: 先用空缓冲区查询数量，再按数量分配；缓冲区在调用期间有效。
        unsafe {
            let n = dpdk_sys::rte_eth_xstats_get_names(self.id, std::ptr::null_mut(), 0);
            if n <= 0 {
                return Vec::new();
            }
            let mut names = vec![dpdk_sys::rte_eth_xstat_name::default(); n as usize];
            let mut vals = vec![dpdk_sys::rte_eth_xstat::default(); n as usize];
            let n1 = dpdk_sys::rte_eth_xstats_get_names(self.id, names.as_mut_ptr(), n as u32);
            let n2 = dpdk_sys::rte_eth_xstats_get(self.id, vals.as_mut_ptr(), n as u32);
            let k = n1.min(n2).max(0) as usize;
            (0..k)
                .map(|i| {
                    let name = CStr::from_ptr(names[vals[i].id as usize].name.as_ptr()).to_string_lossy().into_owned();
                    (name, vals[i].value)
                })
                .collect()
        }
    }

    /// 网卡是否请求过 reset（ENA watchdog 检测到保活超时 / TX 卡死等）。
    #[inline]
    pub fn reset_requested(&self) -> bool {
        RESET_REQUESTED.load(Ordering::Relaxed)
    }
}

unsafe extern "C" fn on_reset_event(_port: u16, _ev: dpdk_sys::rte_eth_event_type, _arg: *mut c_void, _ret: *mut c_void) -> c_int {
    RESET_REQUESTED.store(true, Ordering::Relaxed);
    0
}

/// 一次 rx_burst 的结果：按顺序以 [`Mbuf`]（所有权）的形式交出。
/// 没被取走的包在下一次 rx_burst 或 Drop 时自动释放。
pub struct RxBurst {
    ptrs: [*mut rte_mbuf; RX_BURST_MAX],
    len: u16,
    pos: u16,
}

impl RxBurst {
    pub fn new() -> Self {
        RxBurst { ptrs: [std::ptr::null_mut(); RX_BURST_MAX], len: 0, pos: 0 }
    }

    #[inline]
    fn release_remaining(&mut self) {
        while self.next().is_some() {}
    }
}

impl Default for RxBurst {
    fn default() -> Self {
        Self::new()
    }
}

impl Iterator for RxBurst {
    type Item = Mbuf;
    #[inline]
    fn next(&mut self) -> Option<Mbuf> {
        if self.pos < self.len {
            let p = self.ptrs[self.pos as usize];
            self.pos += 1;
            // SAFETY: [pos, len) 区间内的指针来自 rx_burst，尚未交出过所有权。
            Some(unsafe { Mbuf::from_raw(p) })
        } else {
            None
        }
    }
}

impl Drop for RxBurst {
    fn drop(&mut self) {
        self.release_remaining();
    }
}
