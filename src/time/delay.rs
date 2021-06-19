use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use crate::io::Timer;

pub struct Delay {
    timer: Timer,
}

pub fn delay_until(deadline: Instant) -> Delay {
    Delay {
        timer: Timer::new(deadline),
    }
}

pub fn delay_for(duration: Duration) -> Delay {
    delay_until(Instant::now() + duration)
}

impl Delay {
    pub fn deadline(&self) -> Instant {
        self.timer.deadline()
    }

    pub fn is_elapsed(&self) -> bool {
        self.timer.is_elapsed()
    }

    pub fn reset(&mut self, deadline: Instant) {
        self.timer.reset(deadline);
    }
}

impl Future for Delay {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.timer.poll_timeout(cx) {
            Poll::Ready(_) => Poll::Ready(()),
            Poll::Pending => Poll::Pending,
        }
    }
}
