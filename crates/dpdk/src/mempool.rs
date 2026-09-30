use crate::{Error, Mbuf, Result};
use dpdk_sys::rte_mempool;
use std::ffi::CString;
use std::ptr::NonNull;

/// mbuf 内存池。创建后泄漏为 `&'static Mempool`：
/// mbuf 可以在任何地方被持有，而池永远不会先于它们被释放。
pub struct Mempool {
    raw: NonNull<rte_mempool>,
    size: u32,
}

impl Mempool {
    /// `rte_pktmbuf_pool_create`。`n` 最好取 2^k − 1（DPDK 文档建议）。
    pub fn create_pktmbuf_pool(name: &str, n: u32, cache_size: u32, data_room: u16, socket: i32) -> Result<&'static Mempool> {
        let cname = CString::new(name).expect("name");
        // SAFETY: EAL 已初始化（能拿到 Eal 才能走到这里的调用方保证）；参数都是值类型。
        let p = unsafe { dpdk_sys::rte_pktmbuf_pool_create(cname.as_ptr(), n, cache_size, 0, data_room, socket) };
        let raw = NonNull::new(p).ok_or_else(|| Error::from_rte_errno("rte_pktmbuf_pool_create"))?;
        Ok(Box::leak(Box::new(Mempool { raw, size: n })))
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    /// 可用对象数 = 公共 ring + 所有 lcore cache。要遍历所有 lcore，不要在热路径上调用。
    pub fn avail_count(&self) -> u32 {
        // SAFETY: 有效 mempool。
        unsafe { dpdk_sys::rte_mempool_avail_count(self.raw.as_ptr()) }
    }

    pub fn in_use_count(&self) -> u32 {
        // SAFETY: 有效 mempool。
        unsafe { dpdk_sys::rte_mempool_in_use_count(self.raw.as_ptr()) }
    }

    /// 取一个空 mbuf（已 reset：data_off = headroom，长度 0）。池空时返回 None。
    #[inline]
    pub fn alloc(&self) -> Option<Mbuf> {
        // SAFETY: 有效 mempool；返回的 mbuf 由我们独占。
        let p = unsafe { dpdk_sys::shim_pktmbuf_alloc(self.raw.as_ptr()) };
        if p.is_null() { None } else { Some(unsafe { Mbuf::from_raw(p) }) }
    }

    pub(crate) fn as_ptr(&self) -> *mut rte_mempool {
        self.raw.as_ptr()
    }
}
