//! Small, dependency-light primitives inspired by `claude-agent-acp/src/utils.ts`.
//!
//! The original implementation bridges push-based producers and async
//! iterators. In Rust, `Pushable<T>` provides the same semantics using Tokio's
//! synchronization primitives, without a worker task or an intermediate
//! channel allocation per consumer.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};
use tokio::time::{sleep as tokio_sleep, Duration};

struct State<T> {
    queue: VecDeque<T>,
    closed: bool,
}

/// A push-based asynchronous queue with explicit close semantics.
///
/// `push()` is safe to call from a producer task while one or more consumers
/// wait in `next()`. Values are FIFO. Closing drains buffered values before
/// returning `None`.
#[derive(Clone)]
pub struct Pushable<T> {
    state: Arc<Mutex<State<T>>>,
    notify: Arc<Notify>,
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
                closed: false,
            })),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Push an item unless the stream has already been closed.
    pub async fn push(&self, item: T) -> bool {
        let mut state = self.state.lock().await;
        if state.closed {
            return false;
        }
        state.queue.push_back(item);
        drop(state);
        self.notify.notify_waiters();
        true
    }

    /// Close the queue. Existing buffered values remain available.
    pub async fn close(&self) {
        let mut state = self.state.lock().await;
        if !state.closed {
            state.closed = true;
        }
        drop(state);
        self.notify.notify_waiters();
    }

    pub async fn is_closed(&self) -> bool {
        self.state.lock().await.closed
    }

    /// Receive the next value, or `None` after the queue is closed and drained.
    pub async fn next(&self) -> Option<T> {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self.state.lock().await;
                if let Some(item) = state.queue.pop_front() {
                    return Some(item);
                }
                if state.closed {
                    return None;
                }
            }
            notified.await;
        }
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

    #[tokio::test]
    async fn pushable_preserves_order_and_closes_cleanly() {
        let queue = Pushable::new();
        queue.push(1).await;
        queue.push(2).await;
        queue.close().await;

        assert_eq!(queue.next().await, Some(1));
        assert_eq!(queue.next().await, Some(2));
        assert_eq!(queue.next().await, None);
    }

    #[tokio::test]
    async fn push_after_close_is_rejected() {
        let queue = Pushable::<u8>::new();
        queue.close().await;
        assert!(!queue.push(1).await);
    }

    #[tokio::test]
    async fn consumer_waits_until_producer_pushes() {
        let queue = Pushable::new();
        let producer = queue.clone();

        let task = tokio::spawn(async move {
            tokio::task::yield_now().await;
            producer.push("ready").await
        });

        assert_eq!(queue.next().await, Some("ready"));
        assert!(task.await.unwrap());
    }
}
