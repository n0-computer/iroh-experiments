use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use n0_future::Stream;
use tokio::sync::Notify;

pub mod connection_pool;
pub mod connection_pool_0rtt;

#[cfg(test)]
mod tests;

#[derive(Debug)]
struct ConnectionCounterInner {
    count: AtomicUsize,
    notify: Notify,
}

#[derive(Debug, Clone)]
struct ConnectionCounter {
    inner: Arc<ConnectionCounterInner>,
}

impl ConnectionCounter {
    fn new() -> Self {
        Self {
            inner: Arc::new(ConnectionCounterInner {
                count: Default::default(),
                notify: Notify::new(),
            }),
        }
    }

    /// Increase the connection count and return a guard for the new connection
    fn get_one(&self) -> OneConnection {
        self.inner.count.fetch_add(1, Ordering::SeqCst);
        OneConnection {
            inner: self.inner.clone(),
        }
    }

    fn is_idle(&self) -> bool {
        self.inner.count.load(Ordering::SeqCst) == 0
    }

    /// Infinite stream that yields when the connection is briefly idle.
    ///
    /// Note that you still have to check if the connection is still idle when
    /// you get the notification.
    ///
    /// Also note that this stream is triggered on [OneConnection::drop], so it
    /// won't trigger initially even though a [ConnectionCounter] starts up as
    /// idle.
    fn idle_stream(self) -> impl Stream<Item = ()> {
        n0_future::stream::unfold(self, |c| async move {
            c.inner.notify.notified().await;
            Some(((), c))
        })
    }
}

/// Guard for one connection
#[derive(Debug)]
struct OneConnection {
    inner: Arc<ConnectionCounterInner>,
}

impl Drop for OneConnection {
    fn drop(&mut self) {
        if self.inner.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.inner.notify.notify_waiters();
        }
    }
}
