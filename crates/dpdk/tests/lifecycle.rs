//! 端口生命周期：安全接口在"非法顺序"下的行为（审查意见 R4）。
//!
//! 用 DPDK 自带的虚拟网卡 `net_null`（收：凭空产生包；发：直接丢弃），不需要真网卡、大页和 root，
//! 所以 `cargo test` 就能跑。EAL 一个进程只能初始化一次，所以所有步骤放在同一个测试里按顺序做。

use dpdk::{Eal, Mempool, Port, RxBurst};

#[test]
fn illegal_orders_are_refused_or_harmless() {
    let eal = Eal::init(&["--no-huge", "--no-pci", "--in-memory", "-m", "64", "--vdev=net_null0", "--log-level", "*:emerg"])
        .expect("EAL（--no-huge + net_null）应当能在没有特权的情况下初始化");
    let pool = Mempool::create_pktmbuf_pool(&eal, "lifecycle", 1023, 0, 2048, 0).expect("mempool");
    let total = pool.avail_count();
    let port = Port::configure(&eal, 0, pool, 128, 128).expect("configure");

    // 1. 重复领取同一个端口：拒绝
    assert!(Port::configure(&eal, 0, pool, 128, 128).is_err(), "同一个端口号不能领出第二个句柄");

    // 2. 还没启动就收发：收到 0 个；发送被拒绝，mbuf 原样还回来（不泄漏、不崩溃）
    let mut burst = RxBurst::new();
    assert_eq!(port.rx_burst(&mut burst), 0, "未启动的端口收不到包");
    let m = pool.alloc().expect("alloc");
    let m = port.tx(m).expect_err("未启动的端口不接收发送");
    drop(m);
    assert_eq!(pool.avail_count(), total);

    // 3. 启动后正常收发
    port.start().expect("start");
    let n = port.rx_burst(&mut burst);
    assert!(n > 0, "net_null 每次 rx_burst 都会给包");
    while burst.next().is_some() {}
    assert!(port.tx(pool.alloc().expect("alloc")).is_ok());

    // 4. 停止后再收发：与未启动时相同
    port.stop().expect("stop");
    assert_eq!(port.rx_burst(&mut burst), 0, "停止后的端口收不到包");
    let m = port.tx(pool.alloc().expect("alloc")).expect_err("停止后的端口不接收发送");
    drop(m);
    assert_eq!(pool.avail_count(), total, "停止后所有 mbuf 都回到了池里");

    // 5. 没有先 stop 就 close：close 自己先停止
    port.start().expect("restart");
    assert!(port.rx_burst(&mut burst) > 0);
    drop(burst); // 归还手里的包
    port.close().expect("close（内部先 stop）");
    assert_eq!(pool.avail_count(), total, "关闭后 mbuf 零泄漏");

    // SAFETY: 端口已关闭，所有 Mbuf / RxBurst 已释放，此后不再使用任何 DPDK 对象。
    unsafe { eal.cleanup() };
}
