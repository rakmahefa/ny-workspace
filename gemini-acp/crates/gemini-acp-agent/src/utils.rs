//! Small, dependency-light primitives inspired by `claude-agent-acp/src/utils.ts`.
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tokio::time::{sleep as tokio_sleep, Duration};

struct State<T> { queue: VecDeque<T>, closed: bool }

#[derive(Clone)]
pub struct Pushable<T> { state: Arc<Mutex<State<T>>>, notify: Arc<Notify> }

impl<T> Default for Pushable<T> { fn default() -> Self { Self::new() } }
impl<T> Pushable<T> {
    pub fn new() -> Self { Self { state: Arc::new(Mutex::new(State { queue: VecDeque::new(), closed: false })), notify: Arc::new(Notify::new()) } }
    pub async fn push(&self, item: T) -> bool { let mut state = self.state.lock().await; if state.closed { return false; } state.queue.push_back(item); drop(state); self.notify.notify_waiters(); true }
    pub async fn close(&self) { let mut state = self.state.lock().await; state.closed = true; drop(state); self.notify.notify_waiters(); }
    pub async fn is_closed(&self) -> bool { self.state.lock().await.closed }
    pub async fn next(&self) -> Option<T> { loop { let notified = self.notify.notified(); { let mut state = self.state.lock().await; if let Some(item) = state.queue.pop_front() { return Some(item); } if state.closed { return None; } } notified.await; } }
}

pub async fn sleep(duration: Duration) { tokio_sleep(duration).await }
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[cfg(test)]
#[path = "test/utils.rs"]
mod tests;
