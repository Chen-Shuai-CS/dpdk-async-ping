use dpdk_sys::rte_mbuf;
use std::ptr::NonNull;

/// 一个 DPDK 包缓冲区的**唯一所有者**。Drop 时归还 mempool。
///
/// - 不实现 `Clone`：两个所有者意味着 double free。
/// - 内含裸指针，因此自动 `!Send + !Sync`：mbuf 只在创建它的 lcore 上流转。
/// - 只支持单段 mbuf（本项目的帧只有 106 B，远小于 2048 B 的数据区）。
pub struct Mbuf {
    raw: NonNull<rte_mbuf>,
}

impl Mbuf {
    /// # Safety
    /// `raw` 必须是一个有效的、由调用者独占的单段 mbuf（例如刚从 rx_burst / alloc 得到）。
    #[inline]
    pub(crate) unsafe fn from_raw(raw: *mut rte_mbuf) -> Mbuf {
        Mbuf { raw: NonNull::new_unchecked(raw) }
    }

    /// 放弃所有权，交出裸指针（交给网卡发送时用）。
    #[inline]
    pub(crate) fn into_raw(self) -> *mut rte_mbuf {
        let p = self.raw.as_ptr();
        std::mem::forget(self);
        p
    }

    #[inline]
    fn m(&self) -> &rte_mbuf {
        // SAFETY: self 独占一个有效 mbuf。
        unsafe { self.raw.as_ref() }
    }

    /// 数据区起始偏移（headroom）。
    #[inline]
    fn data_off(&self) -> u16 {
        // SAFETY: union 的两个视图都是 POD，读 data_off 总是有效的。
        unsafe { self.m().__bindgen_anon_1.__bindgen_anon_1.data_off }
    }

    /// 当前包长度（单段：data_len == pkt_len）。
    #[inline]
    pub fn len(&self) -> usize {
        // SAFETY: 同上，POD 字段。
        unsafe { self.m().__bindgen_anon_2.__bindgen_anon_1.data_len as usize }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 数据区从 data_off 开始还能容纳多少字节。
    #[inline]
    pub fn capacity(&self) -> usize {
        // SAFETY: POD 字段。
        let buf_len = unsafe { self.m().__bindgen_anon_2.__bindgen_anon_1.buf_len };
        (buf_len - self.data_off()) as usize
    }

    /// 包数据（只读）。
    #[inline]
    pub fn data(&self) -> &[u8] {
        // SAFETY: buf_addr + data_off 起的 data_len 字节位于该 mbuf 的数据区内，且由 self 独占；
        // 返回的切片借用 self，不会比 mbuf 活得久。
        unsafe {
            let base = (self.m().buf_addr as *const u8).add(self.data_off() as usize);
            std::slice::from_raw_parts(base, self.len())
        }
    }

    /// 包数据（可写）。
    #[inline]
    pub fn data_mut(&mut self) -> &mut [u8] {
        let len = self.len();
        // SAFETY: 同 data()，且 &mut self 保证独占。
        unsafe {
            let m = self.raw.as_mut();
            let base = (m.buf_addr as *mut u8).add(m.__bindgen_anon_1.__bindgen_anon_1.data_off as usize);
            std::slice::from_raw_parts_mut(base, len)
        }
    }

    /// 设置包长度（单段：pkt_len = data_len = len）。超过容量则 panic。
    #[inline]
    pub fn set_len(&mut self, len: usize) {
        assert!(len <= self.capacity(), "mbuf 长度 {len} 超过容量 {}", self.capacity());
        // SAFETY: &mut self 独占；写 POD 字段。
        unsafe {
            let f = &mut self.raw.as_mut().__bindgen_anon_2.__bindgen_anon_1;
            f.pkt_len = len as u32;
            f.data_len = len as u16;
        }
    }

    /// 引用计数（调试 / 断言用）。
    pub fn refcnt(&self) -> u16 {
        // SAFETY: 读取有效 mbuf 的引用计数。
        unsafe { dpdk_sys::shim_mbuf_refcnt_read(self.raw.as_ptr()) }
    }
}

impl Drop for Mbuf {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: self 是这个 mbuf 唯一的所有者，且它尚未被释放（into_raw 会 forget self，不会走到这里）。
        unsafe { dpdk_sys::shim_pktmbuf_free(self.raw.as_ptr()) }
    }
}
