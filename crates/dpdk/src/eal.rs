use crate::{Error, Result};
use std::ffi::{c_char, CString};
use std::sync::atomic::{AtomicBool, Ordering};

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// EAL 初始化的凭证。整个进程只能有一个。
pub struct Eal {
    _private: (),
}

impl Eal {
    /// 调用 `rte_eal_init`。`args` 就是 EAL 命令行参数（不含程序名）。
    pub fn init<S: AsRef<str>>(args: &[S]) -> Result<Eal> {
        if INITIALIZED.swap(true, Ordering::SeqCst) {
            return Err(Error { what: "rte_eal_init（重复初始化）", errno: libc_ealready() });
        }
        let mut owned: Vec<CString> = vec![CString::new("bq").unwrap()];
        owned.extend(args.iter().map(|a| CString::new(a.as_ref()).expect("EAL 参数里不能有 NUL")));
        // EAL 可能保留 argv 里的指针，所以这些字符串故意泄漏，与进程同寿命。
        let argv: &'static mut [*mut c_char] = Box::leak(
            owned.into_iter().map(|c| c.into_raw()).collect::<Vec<_>>().into_boxed_slice(),
        );
        // SAFETY: argv 指向 argc 个有效且永不释放的 C 字符串。
        let ret = unsafe { dpdk_sys::rte_eal_init(argv.len() as i32, argv.as_mut_ptr()) };
        if ret < 0 {
            return Err(Error::from_rte_errno("rte_eal_init"));
        }
        // ENA 驱动的 watchdog 用 rte_timer 实现，需要应用初始化 timer 子系统并周期性调用 rte_timer_manage。
        // SAFETY: EAL 已初始化；只调用一次。
        let ret = unsafe { dpdk_sys::rte_timer_subsystem_init() };
        if ret < 0 && ret != -114 {
            // -EALREADY 表示已初始化，可以接受
            return Err(Error::from_ret("rte_timer_subsystem_init", ret));
        }
        Ok(Eal { _private: () })
    }

    /// 驱动 DPDK 的 rte_timer（ENA watchdog、TX 完成超时检查）。需在启动端口的那个 lcore 上周期性调用。
    #[inline]
    pub fn timer_manage(&self) {
        // SAFETY: EAL 与 timer 子系统已初始化（由 &self 的存在保证）。
        unsafe { dpdk_sys::rte_timer_manage() };
    }

    /// 当前线程的 lcore id。
    #[inline]
    pub fn lcore_id(&self) -> u32 {
        // SAFETY: 读取线程局部变量，无前置条件。
        unsafe { dpdk_sys::shim_lcore_id() }
    }

    /// 释放 EAL 资源（大页文件等）。
    ///
    /// # Safety
    /// 调用后所有 DPDK 对象（mempool、mbuf、端口）都失效；调用者必须保证此后不再使用任何 [`crate::Mbuf`]、
    /// [`crate::Mempool`]、[`crate::Port`]。
    pub unsafe fn cleanup(self) {
        dpdk_sys::rte_eal_cleanup();
    }
}

fn libc_ealready() -> i32 {
    114 // EALREADY
}
