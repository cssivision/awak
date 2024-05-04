use std::future::Future;
use std::io;
use std::ops::{Add, Sub};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use crate::time::{delay_for, Delay};
use pin_project_lite::pin_project;

pin_project! {
    /// A future with timeout time set
    pub struct IdleTimeout<S: Future> {
        #[pin]
        inner: S,
        #[pin]
        sleep: Delay,
        idle_timeout: Duration,
        check_interval: Duration,
        last_visited: Instant,
    }
}

impl<S: Future> IdleTimeout<S> {
    pub fn new(inner: S, idle_timeout: Duration, check_interval: Duration) -> Self {
        let sleep = delay_for(idle_timeout);

        Self {
            inner,
            sleep,
            idle_timeout,
            check_interval,
            last_visited: Instant::now(),
        }
    }
}

impl<S: Future> Future for IdleTimeout<S> {
    type Output = io::Result<S::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        match this.inner.poll(cx) {
            Poll::Ready(v) => Poll::Ready(Ok(v)),
            Poll::Pending => match Pin::new(&mut this.sleep).poll(cx) {
                Poll::Ready(_) => Poll::Ready(Err(io::ErrorKind::TimedOut.into())),
                Poll::Pending => {
                    let now = Instant::now();
                    if now.sub(*this.last_visited) >= *this.check_interval {
                        *this.last_visited = now;
                        this.sleep.reset(now.add(*this.idle_timeout));
                    }
                    Poll::Pending
                }
            },
        }
    }
}
