//! A：async-ping —— 跑在自研 `rt` runtime 上。每个 session 是一个独立的 async task：
//!
//! ```text
//! loop {
//!     send(seq).await;                     // T0 → T1，段①
//!     let reply = wait_reply(seq).await;   // T2 → T3，段②；超时也从这里返回
//!     sleep(delay).await;                  // 样本之外；期间持有 reply 的 mbuf
//!     record(reply);
//! }
//! ```
//!
//! 与 B 共用：数据面、发送函数（段①）、协议解析、直方图、维护动作。

mod driver;

use clap::Parser;
use dpdk::tsc::ns_to_cycles;
use driver::{IcmpDriver, Reply, Shared};
use pingkit::live::{self, Live};
use pingkit::stats::{port_summary, Report};
use pingkit::{rdtsc, Args, Dataplane, SendError, Stamp};
use rt::{sleep, sleep_until, with_port, Runtime, RuntimeConfig, SleepInfo};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TICK_NS: u64 = 100_000;
const TX_RETRY_NS: u64 = 1_000;

/// 段①：调用与 B 相同的发送函数。成功后登记"在等 seq"。TX 环满等罕见情况下 sleep 1 µs 重试。
async fn send(sh: &Shared, id: u16, seq: u16) -> Stamp {
    loop {
        match with_port(|p| sh.sender.send(p, id, seq)) {
            Ok(stamp) => {
                sh.flows[id as usize].arm(seq, stamp.t0 + sh.timeout);
                sh.stats.borrow_mut().c.sent += 1;
                return stamp;
            }
            Err(e) => {
                let mut st = sh.stats.borrow_mut();
                match e {
                    SendError::NoMbuf => st.c.no_mbuf += 1,
                    SendError::TxFull => st.c.tx_full += 1,
                }
                drop(st);
                sleep(sh.tx_retry).await;
            }
        }
    }
}

/// 一个 session。`first_deadline` 是初始相位（64 个 session 均匀错开在一个 delay 周期内）。
async fn session(sh: Rc<Shared>, id: u16, first_deadline: u64) {
    let flow = &sh.flows[id as usize];
    let mut woke: SleepInfo = sleep_until(first_deadline).await;
    let mut held: Option<(Reply, Stamp, u64)> = None;
    let mut seq: u16 = 0;
    loop {
        // record(reply)：sleep 结束后才记录上一个样本并释放它的 mbuf（SPEC 的 loop 形状，与 B 相同）
        if let Some((reply, stamp, t3)) = held.take() {
            sh.stats.borrow_mut().on_reply(stamp, reply.t2, t3);
            drop(reply);
        }
        if sh.stopping.get() {
            break;
        }
        let stamp = send(&sh, id, seq).await;
        sh.stats.borrow_mut().on_wake(woke.deadline, woke.fired_at, stamp.t0);

        let r = flow.mailbox.recv().await; // wait_reply：reactor 把 reply 放进信箱并唤醒本 task
        let t3 = rdtsc(); // T3：本 task 从 wait_reply().await 恢复执行
        if let Ok(reply) = r {
            #[cfg(feature = "probe")]
            {
                let (put, poll) = (sh.probe_put_done.get(), rt::probe::poll_start());
                let mut p = sh.probe.borrow_mut();
                p[0].record(put.saturating_sub(reply.t2));
                p[1].record(poll.saturating_sub(put));
                p[2].record(t3.saturating_sub(poll));
            }
            sh.stats.borrow_mut().c.received += 1;
            held = Some((reply, stamp, t3));
        } // Err(Timeout)：已由 driver 计数；照常 delay 后发下一个

        woke = sleep_until(t3 + sh.delay).await; // 期间 `held` 持有 reply 的 mbuf
        seq = seq.wrapping_add(1);
    }
}

fn main() {
    let args = Args::parse();
    live::install_signal_handlers();
    let dp = Dataplane::open(&args).unwrap_or_else(|e| {
        eprintln!("初始化失败：{e}");
        std::process::exit(2);
    });
    let hz = dp.hz;
    let xstats_before = dp.port.xstats();
    let live = Arc::new(Live::default());
    let reporter = live::spawn_reporter(live.clone(), args.report_core, Duration::from_secs(args.progress_sec), "A async");

    let n = args.sessions as usize;
    let delay = ns_to_cycles(args.delay_us * 1000, hz);
    let start = rdtsc();
    let sh = Rc::new(Shared::new(
        n,
        pingkit::Sender::new(dp.pool, dp.tmpl.clone()),
        delay,
        ns_to_cycles(args.timeout_us * 1000, hz),
        ns_to_cycles(TX_RETRY_NS, hz),
        start + ns_to_cycles(args.duration_sec * 1_000_000_000, hz),
        dp.endpoints.src_ip,
        dp.endpoints.src_mac,
        live.clone(),
    ));

    *sh.stats.borrow_mut() = pingkit::Stats::with_hz(hz);
    let rt = Runtime::new(RuntimeConfig { max_tasks: n + 8, max_timers: 2 * n + 8, tick_cycles: ns_to_cycles(TICK_NS, hz) });
    for i in 0..n {
        rt.spawn(session(sh.clone(), i as u16, start + delay * i as u64 / n as u64));
    }
    let driver = IcmpDriver { sh: sh.clone(), eal: &dp.eal };
    let wall = Instant::now();
    let exit = rt.run(&dp.port, &driver);
    let elapsed = wall.elapsed().as_secs_f64();

    // ---------------- 收尾 ----------------
    live.publish(&sh.stats.borrow());
    live.finish();
    if let Some(r) = reporter {
        let _ = r.join();
    }
    let exit_reason = sh.exit_reason.borrow().clone();
    let exit_reason = if exit == rt::RunExit::AllTasksDone { exit_reason } else { format!("{exit_reason}（runtime 提前退出）") };
    sh.stats.borrow_mut().c.in_flight_at_end = sh.flows.iter().filter(|f| f.expect.get().is_some()).count() as u64;
    // 释放程序持有的所有 mbuf：drop runtime（丢弃所有未完成 task 及其持有的 reply）→ 清空信箱
    drop(driver);
    drop(rt);
    for f in sh.flows.iter() {
        drop(f.mailbox.take());
    }
    #[cfg(feature = "probe")]
    {
        let names = ["T2→put 返回（分类+投递+wake）", "put 返回→开始 poll（回主循环+出队）", "开始 poll→T3（poll 到恢复）"];
        println!("\n[probe] 段②子段（ns）：");
        for (name, h) in names.iter().zip(sh.probe.borrow().iter()) {
            let ns = |c| dpdk::tsc::cycles_to_ns(c, hz);
            println!(
                "  {:<40} p50 {:>6}  p90 {:>6}  p99 {:>6}  p99.9 {:>6}",
                name,
                ns(h.quantile(0.5)),
                ns(h.quantile(0.9)),
                ns(h.quantile(0.99)),
                ns(h.quantile(0.999))
            );
        }
    }
    let port = port_summary(&dp.port, &xstats_before);
    let stats = std::mem::take(&mut *sh.stats.borrow_mut());
    drop(sh);
    let (eal, mbuf) = dp.shutdown();
    Report::new("A · async-ping（自研 rt runtime）", &args, elapsed, &stats, hz, port, mbuf, exit_reason)
        .emit(args.json.as_deref());
    // SAFETY: runtime、task、信箱都已释放，端口已关闭；此后不再访问任何 DPDK 对象。
    unsafe { eal.cleanup() };
    std::process::exit(if mbuf.leaked() == 0 { 0 } else { 3 });
}
