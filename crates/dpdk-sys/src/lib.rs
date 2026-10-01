//! DPDK 裸绑定。所有函数都是 `unsafe` 的；安全封装在 `dpdk` crate 里。
#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code, clippy::all)]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// 构建时链接的 DPDK 版本（pkg-config 报告的版本号）。
pub const DPDK_VERSION: &str = env!("DPDK_PKG_VERSION");
