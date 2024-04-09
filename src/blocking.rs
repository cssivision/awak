use std::cell::Cell;
use std::future::Future;
use std::pin::pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use crate::io::reactor::Reactor;
use crate::parking;
use crate::waker_fn::waker_fn;

/// Runs a closure when dropped.
struct CallOnDrop<F: Fn()>(F);

impl<F: Fn()> Drop for CallOnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// Runs a future to completion on the current thread.
pub fn block_on<T>(future: impl Future<Output = T>) -> T {
    Reactor::get().add_block_on_count();

    let _guard = CallOnDrop(|| {
        Reactor::get().sub_block_on_count();
        Reactor::get().unpark();
    });

    let (p, u) = parking::pair();
    let io_blocked = Arc::new(AtomicBool::new(false));
    thread_local! {
        static IO_POLLING: Cell<bool> = const { Cell::new(false) };
    }

    let waker = waker_fn({
        let io_blocked = io_blocked.clone();
        move || {
            if u.unpark() && !IO_POLLING.with(Cell::get) && io_blocked.load(Ordering::Acquire) {
                Reactor::get().notify();
            }
        }
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
                IO_POLLING.with(|io| io.set(true));
                let _guard = CallOnDrop(|| {
                    IO_POLLING.with(|io| io.set(false));
                });
                // Process available I/O events.
                reactor_lock.react(Some(Duration::from_secs(0))).ok();
            }
            continue;
        }

        // Try grabbing a lock on the reactor to process I/O events.
        if let Some(reactor_lock) = Reactor::get().try_lock() {
            // Hold the lock means all I/O events just handled.

            // Record the instant at which the lock was grabbed.
            let start = Instant::now();

            loop {
                IO_POLLING.with(|io| io.set(true));
                io_blocked.store(true, Ordering::Release);
                let _guard = CallOnDrop(|| {
                    IO_POLLING.with(|io| io.set(false));
                    io_blocked.store(false, Ordering::Release);
                });

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

                    // Unpark the epoll thread in case no other thread is ready to start
                    // processing I/O events. This way we prevent a potential latency spike.
                    Reactor::get().unpark();

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
