//! DPDK 裸绑定。所有函数都是 `unsafe` 的；安全封装在 `dpdk` crate 里。
#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code, clippy::all)]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
