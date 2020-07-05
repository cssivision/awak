use std::cell::RefCell;
use std::future::Future;
use std::task::{Context, Poll, Waker};

use crossbeam::sync::Parker;

/// Runs a future to completion on the current thread.
pub fn block_on<F: Future>(future: F) -> F::Output {
    pin_utils::pin_mut!(future);

    thread_local! {
        static CACHE: RefCell<(Parker, Waker)> = {
            let parker = Parker::new();
            let unparker = parker.unparker().clone();
            let waker = waker_fn::waker_fn(move || unparker.unpark());
            RefCell::new((parker, waker))
        };
    }

    CACHE.with(|cache| {
        let (parker, waker) = &mut *cache.try_borrow_mut().ok().expect("recursive `block_on`");

        let cx = &mut Context::from_waker(&waker);
        loop {
            match future.as_mut().poll(cx) {
                Poll::Ready(output) => return output,
                Poll::Pending => parker.park(),
            }
        }
    })
}
