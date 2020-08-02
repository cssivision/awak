use std::cell::RefCell;
use std::future::Future;
use std::task::{Context, Poll, Waker};

use crate::parking::{parker_and_waker, Parker};

/// Runs a future to completion on the current thread.
pub fn block_on<T>(future: impl Future<Output = T>) -> T {
    pin_mut!(future);

    thread_local! {
        // Cached parker and waker for efficiency.
        static CACHE: RefCell<(Parker, Waker)> = RefCell::new(parker_and_waker());
    }

    CACHE.with(|cache| match cache.try_borrow_mut() {
        Ok(cache) => {
            let (parker, waker) = &*cache;

            let cx = &mut Context::from_waker(&waker);
            loop {
                match future.as_mut().poll(cx) {
                    Poll::Ready(output) => return output,
                    Poll::Pending => parker.park(),
                }
            }
        }
        Err(_) => {
            let (parker, waker) = parker_and_waker();

            let cx = &mut Context::from_waker(&waker);
            loop {
                match future.as_mut().poll(cx) {
                    Poll::Ready(output) => return output,
                    Poll::Pending => parker.park(),
                }
            }
        }
    })
}
