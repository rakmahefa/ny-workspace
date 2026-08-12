//! Small, dependency-light primitives inspired by `claude-agent-acp/src/utils.ts`.
//!
//! The original implementation bridges push-based producers and async
//! iterators. In Rust, `Pushable<T>` provides the same semantics without
//! spawning a worker thread or allocating an intermediate task queue.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use tokio::sync::Mutex;
use tokio::time::{sleep as tokio_sleep, Duration};

struct State<T> {
    queue: VecDeque<T>,
    wakers: Vec<Waker>,
    closed: bool,
}

/// A push-based asynchronous stream.
///
/// `push()` is non-blocking and wakes exactly the consumers that may be
/// waiting. `close()` is idempotent and causes all pending/future polls to
/// terminate once the buffered values have been drained.
#[derive(Clone)]
pub struct Pushable<T> {
    state: Arc<Mutex<State<T>>>,
}

impl<T> Default for Pushable<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Pushable<T> {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                queue: VecDeque::new(),
                wakers: Vec::new(),
                closed: false,
            })),
        }
    }

    /// Push an item unless the stream has already been closed.
    pub async fn push(&self, item: T) -> bool {
        let mut state = self.state.lock().await;
        if state.closed {
            return false;
        }
        state.queue.push_back(item);
        let wakers = std::mem::take(&mut state.wakers);
        drop(state);
        for waker in wakers {
            waker.wake();
        }
        true
    }

    /// Close the stream. Buffered items remain observable before `None`.
    pub async fn close(&self) {
        let mut state = self.state.lock().await;
        state.closed = true;
        let wakers = std::mem::take(&mut state.wakers);
        drop(state);
        for waker in wakers {
            waker.wake();
        }
    }

    pub async fn is_closed(&self) -> bool {
        self.state.lock().await.closed
    }

    pub fn stream(&self) -> PushableStream<T> {
        PushableStream {
            state: Arc::clone(&self.state),
        }
    }
}

/// Consumer side of [`Pushable`].
pub struct PushableStream<T> {
    state: Arc<Mutex<State<T>>>,
}

impl<T: Unpin> futures_core::Stream for PushableStream<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let state = self.state.clone();
        let mut lock = match state.try_lock() {
            Ok(lock) => lock,
            Err(_) => return Poll::Pending,
        };

        if let Some(item) = lock.queue.pop_front() {
            return Poll::Ready(Some(item));
        }
        if lock.closed {
            return Poll::Ready(None);
        }

        lock.wakers.push(cx.waker().clone());
        Poll::Pending
    }
}

/// Async equivalent of the small `sleep()` helper from the TypeScript ACP
/// implementation.
pub async fn sleep(duration: Duration) {
    tokio_sleep(duration).await;
}

/// Box a future when a callback must be stored behind a trait object.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[tokio::test]
    async fn pushable_preserves_order_and_closes_cleanly() {
        let queue = Pushable::new();
        queue.push(1).await;
        queue.push(2).await;
        queue.close().await;

        let mut stream = queue.stream();
        assert_eq!(stream.next().await, Some(1));
        assert_eq!(stream.next().await, Some(2));
        assert_eq!(stream.next().await, None);
    }

    #[tokio::test]
    async fn push_after_close_is_rejected() {
        let queue = Pushable::<u8>::new();
        queue.close().await;
        assert!(!queue.push(1).await);
    }
}
