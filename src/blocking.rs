use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use crate::io::reactor::Reactor;
use crate::parking;
use crate::waker_fn::waker_fn;

/// Runs a future to completion on the current thread.
pub fn block_on<T>(future: impl Future<Output = T>) -> T {
    let (p, u) = parking::pair();
    let waker = waker_fn(move || {
        u.unpark();
    });

    let cx = &mut Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(t) = future.as_mut().poll(cx) {
            return t;
        }

        // Check if a notification has been received.
        if p.park_timeout(Some(Duration::from_secs(0))) {
            // Try grabbing a lock on the reactor to process I/O events.
            if let Some(reactor_lock) = Reactor::get().try_lock() {
                // Process available I/O events.
                reactor_lock.react(Some(Duration::from_secs(0))).ok();
            }
            continue;
        }

        // Try grabbing a lock on the reactor to process I/O events.
        if let Some(reactor_lock) = Reactor::get().try_lock() {
            // Record the instant at which the lock was grabbed.
            let start = Instant::now();

            loop {
                // Check if a notification has been received.
                if p.park_timeout(Some(Duration::from_secs(0))) {
                    break;
                }

                // Wait on I/O Events
                reactor_lock.react(None).ok();

                // Check if a notification has been received.
                if p.park_timeout(Some(Duration::from_secs(0))) {
                    break;
                }

                // Check if this thread been handling I/O events for a long time.
                if start.elapsed() > Duration::from_micros(500) {
                    // This thread is clearly processing I/O events for some other threads
                    // because it didn't get a notification yet. It's best to stop hogging the
                    // reactor and give other threads a chance to process I/O events for
                    // themselves.
                    drop(reactor_lock);

                    // Wait for a notification.
                    p.park();
                    break;
                }
            }
        } else {
            p.park();
        }
    }
}
