use crate::stats::Stats;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// lcore 与上报线程之间唯一的共享状态：几个原子计数器。
/// lcore 只在维护节拍里做 Relaxed store（x86 上就是普通 mov），不在热路径上写。
#[derive(Default)]
pub struct Live {
    sent: AtomicU64,
    received: AtomicU64,
    timeouts: AtomicU64,
    late: AtomicU64,
    done: AtomicBool,
}

impl Live {
    #[inline]
    pub fn publish(&self, s: &Stats) {
        self.sent.store(s.c.sent, Relaxed);
        self.received.store(s.c.received, Relaxed);
        self.timeouts.store(s.c.timeouts, Relaxed);
        self.late.store(s.c.late, Relaxed);
    }

    pub fn finish(&self) {
        self.done.store(true, Relaxed);
    }
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Relaxed);
}

/// SIGINT / SIGTERM → 请求干净退出（主循环在维护节拍里检查）。正常情况下靠 --duration-sec 自动停止。
pub fn install_signal_handlers() {
    // SAFETY: 处理函数只写一个原子变量，是 async-signal-safe 的。
    unsafe {
        libc::signal(libc::SIGINT, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
    }
}

#[inline]
pub fn stop_requested() -> bool {
    STOP.load(Relaxed)
}

/// 把当前线程绑到 `core`。
pub fn pin_current_thread(core: usize) -> bool {
    // SAFETY: cpu_set_t 是 POD；sched_setaffinity(0, …) 作用于当前线程。
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(core, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0
    }
}

/// 在 `core` 上启动进度上报线程，每 `every` 打印一行。
pub fn spawn_reporter(live: Arc<Live>, core: usize, every: Duration, label: &'static str) -> Option<JoinHandle<()>> {
    if every.is_zero() {
        return None;
    }
    Some(std::thread::spawn(move || {
        pin_current_thread(core);
        let start = Instant::now();
        let (mut last_t, mut last_sent) = (start, 0u64);
        while !live.done.load(Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            if last_t.elapsed() < every {
                continue;
            }
            let sent = live.sent.load(Relaxed);
            let rate = (sent - last_sent) as f64 / last_t.elapsed().as_secs_f64();
            eprintln!(
                "[{label} {:>6.1}s] sent {:>10}  recv {:>10}  timeouts {}  late {}  {:.0} req/s",
                start.elapsed().as_secs_f64(),
                sent,
                live.received.load(Relaxed),
                live.timeouts.load(Relaxed),
                live.late.load(Relaxed),
                rate
            );
            (last_t, last_sent) = (Instant::now(), sent);
        }
    }))
}
