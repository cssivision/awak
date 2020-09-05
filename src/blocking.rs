use std::cell::Cell;
use std::future::Future;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use crate::io::Reactor;
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
    Reactor::get().block_on_count.fetch_add(1, Ordering::SeqCst);

    let _guard = CallOnDrop(|| {
        Reactor::get().block_on_count.fetch_sub(1, Ordering::SeqCst);
        Reactor::get().unparker.unpark();
    });

    let (p, u) = parking::pair();

    let io_blocked = Arc::new(AtomicBool::new(false));

    thread_local! {
        static IO_POLLING: Cell<bool> = Cell::new(false);
    }

    let waker = waker_fn({
        let io_blocked = io_blocked.clone();
        move || {
            if u.unpark() && !IO_POLLING.with(Cell::get) && io_blocked.load(Ordering::SeqCst) {
                Reactor::get().notify();
            }
        }
    });

    let cx = &mut Context::from_waker(&waker);

    pin_mut!(future);

    loop {
        if let Poll::Ready(t) = future.as_mut().poll(cx) {
            return t;
        }

        if p.park_timeout(Some(Duration::from_secs(0))) {
            if let Some(mut reactor_lock) = Reactor::get().try_lock() {
                IO_POLLING.with(|io| io.set(true));
                let _guard = CallOnDrop(|| {
                    IO_POLLING.with(|io| io.set(false));
                });
                let _ = reactor_lock.react(Some(Duration::from_secs(0)));
            }
            continue;
        }

        if let Some(mut reactor_lock) = Reactor::get().try_lock() {
            let start = Instant::now();

            loop {
                IO_POLLING.with(|io| io.set(true));
                io_blocked.store(true, Ordering::SeqCst);
                let _guard = CallOnDrop(|| {
                    IO_POLLING.with(|io| io.set(false));
                    io_blocked.store(false, Ordering::SeqCst);
                });

                if p.park_timeout(Some(Duration::from_secs(0))) {
                    break;
                }

                let _ = reactor_lock.react(None);

                if p.park_timeout(Some(Duration::from_secs(0))) {
                    break;
                }

                if start.elapsed() > Duration::from_micros(500) {
                    drop(reactor_lock);
                    Reactor::get().unparker.unpark();
                    p.park();
                    break;
                }
            }
        } else {
            p.park();
        }
    }
}
