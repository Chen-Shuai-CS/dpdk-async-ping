//! runtime 的单元测试：用 `Runtime::run_offline()`（不驱动网卡），不需要 DPDK / 网卡 / root。
//!
//! 覆盖：timer 顺序与精度、Mailbox 交接、poll 期间自唤醒不丢、过期 waker 不误伤新任务、
//! 丢弃 runtime 时释放未完成 task 持有的资源、取消的 sleep、死锁检测。

use rt::sync::Mailbox;
use rt::{sleep, Runtime, RuntimeConfig};
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::OnceLock;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

/// 测试进程没有初始化 EAL，所以自己标定一次 TSC 频率。
fn hz() -> u64 {
    static HZ: OnceLock<u64> = OnceLock::new();
    *HZ.get_or_init(|| {
        let (c0, t0) = (dpdk::tsc::rdtsc(), Instant::now());
        while t0.elapsed() < Duration::from_millis(20) {}
        ((dpdk::tsc::rdtsc() - c0) as f64 / t0.elapsed().as_secs_f64()) as u64
    })
}

fn us(n: u64) -> u64 {
    n * hz() / 1_000_000
}

fn runtime(max_tasks: usize) -> Runtime {
    Runtime::new(RuntimeConfig { max_tasks, max_timers: 64, tick_cycles: us(100), stall_threshold_cycles: us(1), stall_rx_threshold_cycles: us(10) })
}

#[test]
fn sleeps_wake_in_deadline_order_never_early() {
    let r = runtime(8);
    let order = Rc::new(RefCell::new(Vec::new()));
    let late = Rc::new(RefCell::new(Vec::new()));
    for (id, d) in [(0u32, 300u64), (1, 100), (2, 200), (3, 0)] {
        let (order, late) = (order.clone(), late.clone());
        r.spawn(async move {
            let info = sleep(us(d)).await;
            assert!(info.fired_at >= info.deadline, "timer 提前触发");
            order.borrow_mut().push(id);
            late.borrow_mut().push(info.fired_at - info.deadline);
        });
    }
    r.run_offline();
    assert_eq!(*order.borrow(), vec![3, 1, 2, 0]);
    let worst = *late.borrow().iter().max().unwrap();
    assert!(worst < us(50), "sleep 误差过大：{} 周期", worst);
}

#[test]
fn mailbox_hands_over_values_in_order() {
    let r = runtime(4);
    let mb = Rc::new(Mailbox::<u32>::new());
    let got = Rc::new(RefCell::new(Vec::new()));
    {
        let mb = mb.clone();
        r.spawn(async move {
            for mut v in 0..100u32 {
                sleep(us(1)).await;
                while let Err(back) = mb.put(v) {
                    v = back; // 信箱满：等消费者取走再放
                    sleep(us(1)).await;
                }
            }
        });
    }
    {
        let (mb, got) = (mb.clone(), got.clone());
        r.spawn(async move {
            for _ in 0..100 {
                let v = mb.recv().await;
                got.borrow_mut().push(v);
            }
        });
    }
    r.run_offline();
    assert_eq!(*got.borrow(), (0..100).collect::<Vec<_>>());
}

/// 每次 poll 都先唤醒自己再返回 Pending：唤醒发生在 poll 期间，不能被"已入队"标记吞掉。
struct YieldN(u32);
impl Future for YieldN {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 == 0 {
            return Poll::Ready(());
        }
        self.0 -= 1;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

#[test]
fn self_wake_during_poll_is_not_lost() {
    let r = runtime(2);
    let done = Rc::new(Cell::new(false));
    let d = done.clone();
    r.spawn(async move {
        YieldN(1000).await;
        d.set(true);
    });
    r.run_offline(); // 若丢了唤醒，这里会因"死锁"而 panic
    assert!(done.get());
}

/// 记下自己的 waker，并统计被 poll 的次数。
struct Recorder {
    slot: Rc<RefCell<Option<Waker>>>,
}
impl Future for Recorder {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        *self.slot.borrow_mut() = Some(cx.waker().clone());
        Poll::Ready(())
    }
}

#[test]
fn stale_waker_does_not_wake_new_task_in_same_slot() {
    let r = runtime(1); // 只有一个任务槽：第二个任务必然复用第一个的槽位
    let old = Rc::new(RefCell::new(None::<Waker>));
    r.spawn(Recorder { slot: old.clone() });
    r.run_offline();
    let stale = old.borrow_mut().take().expect("应已记录 waker");

    // 新任务：第一次 poll 时用旧 waker "唤醒"一下，然后 sleep 200 µs 再结束。
    // 正确的 runtime 只会 poll 它 2 次（首次 + sleep 到期）；若旧 waker 叫醒了它，会多出一次无用的 poll。
    let polls = Rc::new(Cell::new(0u32));
    let body = async move {
        stale.wake(); // 代数不匹配，必须被忽略
        sleep(us(200)).await;
    };
    r.spawn(CountTaskPolls { inner: Box::pin(body), polls: polls.clone() });
    r.run_offline();
    assert_eq!(polls.get(), 2, "过期 waker 唤醒了复用同一槽位的新任务");
}

/// 统计整个 task 被 executor poll 了几次。
struct CountTaskPolls<F> {
    inner: Pin<Box<F>>,
    polls: Rc<Cell<u32>>,
}
impl<F: Future> Future for CountTaskPolls<F> {
    type Output = F::Output;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        self.polls.set(self.polls.get() + 1);
        self.inner.as_mut().poll(cx)
    }
}

struct DropGuard(Rc<Cell<u32>>);
impl Drop for DropGuard {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[test]
fn dropping_runtime_releases_resources_held_by_unfinished_tasks() {
    // 对应真实场景：关停时 task 还拿着 reply 的 mbuf，drop runtime 必须把它们归还
    let released = Rc::new(Cell::new(0));
    {
        let r = runtime(8);
        for _ in 0..5 {
            let g = DropGuard(released.clone());
            r.spawn(async move {
                let _g = g;
                std::future::pending::<()>().await;
            });
        }
        assert_eq!(released.get(), 0);
    }
    assert_eq!(released.get(), 5);
}

/// 只 poll 一次内部 future，然后丢弃它（模拟被 select / 超时取消）。
struct PollOnceThenDrop<F>(Option<Pin<Box<F>>>);
impl<F: Future> Future for PollOnceThenDrop<F> {
    type Output = bool;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
        let mut f = self.0.take().expect("只能 poll 一次");
        let ready = f.as_mut().poll(cx).is_ready();
        drop(f);
        Poll::Ready(ready)
    }
}

#[test]
fn cancelled_sleep_does_not_fire_or_leak() {
    let r = runtime(4);
    let finished = Rc::new(Cell::new(0));
    let f = finished.clone();
    r.spawn(async move {
        let ready = PollOnceThenDrop(Some(Box::pin(sleep(us(1000))))).await;
        assert!(!ready);
        // 取消后继续做别的事；被取消的 timer 到期时不应唤醒任何人
        sleep(us(1500)).await;
        f.set(f.get() + 1);
    });
    r.run_offline();
    assert_eq!(finished.get(), 1);
}

#[test]
#[should_panic(expected = "死锁")]
fn deadlock_is_detected() {
    let r = runtime(2);
    r.spawn(std::future::pending::<()>());
    r.run_offline();
}
