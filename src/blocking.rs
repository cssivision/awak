use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll};

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
        p.park();
    }
}
