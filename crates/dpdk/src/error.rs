use std::ffi::CStr;
use std::fmt;

/// DPDK 调用失败：记录是哪个调用、返回的 errno。
#[derive(Debug, Clone)]
pub struct Error {
    pub what: &'static str,
    pub errno: i32,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// DPDK 惯例：返回负的 errno。
    pub(crate) fn from_ret(what: &'static str, ret: i32) -> Self {
        Error { what, errno: ret.abs() }
    }
    /// 通过 rte_errno 报告错误的调用（返回 NULL 的那些）。
    pub(crate) fn from_rte_errno(what: &'static str) -> Self {
        // SAFETY: 读取当前 lcore 的 rte_errno，无前置条件。
        Error { what, errno: unsafe { dpdk_sys::shim_rte_errno() } }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: rte_strerror 返回指向静态或线程局部缓冲区的 C 字符串。
        let msg = unsafe { CStr::from_ptr(dpdk_sys::rte_strerror(self.errno)) };
        write!(f, "{} 失败: {} (errno {})", self.what, msg.to_string_lossy(), self.errno)
    }
}

impl std::error::Error for Error {}
