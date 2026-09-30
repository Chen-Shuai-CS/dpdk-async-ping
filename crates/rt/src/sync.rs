//! Mailbox：reactor 与 task 之间交接所有权的单槽信箱（单线程，无锁）。

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

struct Inner<T> {
    item: Option<T>,
    waker: Option<Waker>,
}

/// 单槽信箱。`put` 放入一个值并唤醒等待者；`recv().await` 取出它。
///
/// 在本项目里，reactor 把属于 session `id` 的 reply（mbuf 的所有权）放进 `mailbox[id]`，
/// session task 在 `recv().await` 处被唤醒并拿到它。
pub struct Mailbox<T> {
    inner: RefCell<Inner<T>>,
}

impl<T> Default for Mailbox<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Mailbox<T> {
    pub const fn new() -> Self {
        Mailbox { inner: RefCell::new(Inner { item: None, waker: None }) }
    }

    /// 放入一个值；信箱已满则原样退回。若有 task 在等，唤醒它（先释放借用再 wake，避免重入冲突）。
    #[inline]
    pub fn put(&self, v: T) -> Result<(), T> {
        let mut i = self.inner.borrow_mut();
        if i.item.is_some() {
            return Err(v);
        }
        i.item = Some(v);
        let w = i.waker.take();
        drop(i);
        if let Some(w) = w {
            w.wake();
        }
        Ok(())
    }

    /// 等待并取出一个值。
    #[inline]
    pub fn recv(&self) -> Recv<'_, T> {
        Recv { mb: self }
    }

    /// 不等待，直接取出（关停清理用）。
    pub fn take(&self) -> Option<T> {
        self.inner.borrow_mut().item.take()
    }
}

pub struct Recv<'a, T> {
    mb: &'a Mailbox<T>,
}

impl<T> Future for Recv<'_, T> {
    type Output = T;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut i = self.mb.inner.borrow_mut();
        if let Some(v) = i.item.take() {
            return Poll::Ready(v);
        }
        match &i.waker {
            Some(w) if w.will_wake(cx.waker()) => {}
            _ => i.waker = Some(cx.waker().clone()),
        }
        Poll::Pending
    }
}

impl<T> Drop for Recv<'_, T> {
    /// 被取消（drop）的 recv 撤销登记，避免之后的 put 去叫醒一个已不在等的 task。
    fn drop(&mut self) {
        if let Ok(mut i) = self.mb.inner.try_borrow_mut() {
            i.waker = None;
        }
    }
}
