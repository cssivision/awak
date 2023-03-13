use std::fmt;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use pin_project_lite::pin_project;

use super::{delay_until, Delay};

pub fn timeout<T>(duration: Duration, future: T) -> Timeout<T>
where
    T: Future,
{
    timeout_at(Instant::now() + duration, future)
}

pub fn timeout_at<T>(deadline: Instant, future: T) -> Timeout<T>
where
    T: Future,
{
    let delay = delay_until(deadline);

    Timeout {
        value: future,
        delay,
    }
}

pin_project! {
    pub struct Timeout<T> {
        #[pin]
        value: T,
        #[pin]
        delay: Delay,
    }
}

impl<T> Timeout<T> {
    pub fn into_inner(self) -> T {
        self.value
    }

    pub fn get_ref(&self) -> &T {
        &self.value
    }

    pub fn get_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

#[derive(Debug)]
pub struct Elapsed(());

impl std::error::Error for Elapsed {}

impl fmt::Display for Elapsed {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        "deadline has elapsed".fmt(fmt)
    }
}

impl From<Elapsed> for io::Error {
    fn from(_err: Elapsed) -> io::Error {
        io::ErrorKind::TimedOut.into()
    }
}

impl<T> Future for Timeout<T>
where
    T: Future,
{
    type Output = Result<T::Output, Elapsed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        if let Poll::Ready(v) = this.value.poll(cx) {
            return Poll::Ready(Ok(v));
        }
        match this.delay.poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(Elapsed(()))),
            Poll::Pending => Poll::Pending,
        }
    }
}
