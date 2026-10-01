use std::{env, path::PathBuf};

const FUNCTIONS: &[&str] = &[
    // EAL
    "rte_eal_init", "rte_eal_cleanup", "rte_strerror", "rte_socket_id", "rte_get_tsc_hz",
    // mempool / mbuf
    "rte_pktmbuf_pool_create", "rte_mempool_avail_count", "rte_mempool_in_use_count", "rte_mempool_free",
    // ethdev
    "rte_eth_dev_count_avail", "rte_eth_dev_info_get", "rte_eth_dev_configure", "rte_eth_rx_queue_setup",
    "rte_eth_tx_queue_setup", "rte_eth_dev_start", "rte_eth_dev_stop", "rte_eth_dev_close",
    "rte_eth_dev_socket_id", "rte_eth_macaddr_get", "rte_eth_link_get_nowait", "rte_eth_stats_get",
    "rte_eth_xstats_get", "rte_eth_xstats_get_names", "rte_eth_xstats_reset", "rte_eth_stats_reset",
    "rte_eth_dev_callback_register", "rte_eth_tx_done_cleanup",
    // timer（ENA 的 watchdog 需要应用驱动 rte_timer）
    "rte_timer_subsystem_init", "rte_timer_manage",
    // 本项目的 shim
    "shim_.*",
];

fn main() {
    let dpdk = pkg_config::Config::new()
        .atleast_version("25.11")
        .probe("libdpdk")
        .expect("找不到 libdpdk（先运行 scripts/setup.sh dpdk，并确认 PKG_CONFIG_PATH）");

    // 1. 编译 C shim，参数与 DPDK 自己的 cflags 一致（-include rte_config.h -march=native）
    let mut cc = cc::Build::new();
    cc.file("src/shim.c")
        .opt_level(3)
        .flag("-march=native")
        .flag("-include")
        .flag("rte_config.h")
        .warnings(false);
    for p in &dpdk.include_paths {
        cc.include(p);
    }
    cc.compile("dpdk_shim");

    // 2. bindgen 生成裸声明（只要 rte_* / shim_*，其余系统头文件里的东西不要）
    let bindings = bindgen::Builder::default()
        .header("src/wrapper.h")
        .clang_args(dpdk.include_paths.iter().map(|p| format!("-I{}", p.display())))
        .clang_arg("-Isrc")
        .clang_args(["-march=native", "-include", "rte_config.h"])
        // 只列出本项目真正调用的函数；需要的类型由 bindgen 顺着依赖带出来。
        // （用 "rte_.*" 会把 GTP 等 packed 协议头也拉进来，Rust 不接受那种布局）
        .allowlist_function(FUNCTIONS.join("|"))
        .allowlist_type("rte_mbuf|rte_mempool|rte_eth_conf|rte_eth_dev_info|rte_eth_stats|rte_eth_link|rte_eth_xstat|rte_eth_xstat_name|rte_ether_addr|rte_eth_rxconf|rte_eth_txconf")
        .allowlist_var("RTE_MAX_ETHPORTS|RTE_PKTMBUF_HEADROOM|RTE_MBUF_DEFAULT_BUF_SIZE|RTE_ETH_.*|RTE_MBUF_F_.*")
        .derive_default(true)
        .derive_debug(false)
        .layout_tests(false)
        .generate_comments(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindgen 失败");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindings.rs");
    bindings.write_to_file(out).expect("写 bindings.rs 失败");

    // DPDK 版本号（来自 pkg-config），写进运行报告的环境信息里
    println!("cargo:rustc-env=DPDK_PKG_VERSION={}", dpdk.version);
    println!("cargo:rerun-if-changed=src/shim.c");
    println!("cargo:rerun-if-changed=src/shim.h");
    println!("cargo:rerun-if-changed=src/wrapper.h");
}
