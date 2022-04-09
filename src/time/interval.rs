use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_core::stream::Stream;

use super::{delay_until, Delay};
use crate::future::poll_fn;

pub fn interval(period: Duration) -> Interval {
    assert!(period > Duration::new(0, 0), "`period` must be non-zero.");
    interval_at(Instant::now(), period)
}

pub fn interval_at(start: Instant, period: Duration) -> Interval {
    assert!(period > Duration::new(0, 0), "`period` must be non-zero.");
    Interval {
        delay: delay_until(start),
        period,
    }
}

pub struct Interval {
    delay: Delay,
    period: Duration,
}

impl Interval {
    pub fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<Instant> {
        ready!(Pin::new(&mut self.delay).poll(cx));
        let now = self.delay.deadline();
        let next = now + self.period;
        self.delay.reset(next);
        Poll::Ready(now)
    }

    pub async fn tick(&mut self) -> Instant {
        poll_fn(|cx| self.poll_tick(cx)).await
    }
}

impl Stream for Interval {
    type Item = Instant;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Instant>> {
        Poll::Ready(Some(ready!(self.poll_tick(cx))))
    }
}
