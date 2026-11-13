//! Measures how accurately a timer actually fires.
//!
//! With a timerfd or `epoll_pwait2` the reactor honours nanosecond deadlines;
//! with a plain millisecond `epoll_wait` timeout a 200us timer cannot fire
//! earlier than 1ms. The difference is what this prints.
use std::time::{Duration, Instant};

fn one(delay_us: u64) -> Duration {
    awak::block_on(async move {
        let t = Instant::now();
        awak::time::delay_for(Duration::from_micros(delay_us)).await;
        t.elapsed()
    })
}

fn stats(name: &str, delay_us: u64, n: usize) {
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(one(delay_us).as_micros() as u64);
    }
    v.sort();
    let mut late: Vec<u64> = v.iter().map(|x| x.saturating_sub(delay_us)).collect();
    late.sort();
    println!(
        "RESULT name={} requested_us={} median_us={} p95_us={} max_us={} median_late_us={} max_late_us={}",
        name,
        delay_us,
        v[n / 2],
        v[(n * 95) / 100],
        v[n - 1],
        late[n / 2],
        late[n - 1]
    );
}

fn main() {
    // Warm up the reactor and executor.
    let _ = one(1000);
    stats("us200", 200, 200);
    stats("us500", 500, 200);
    stats("us1500", 1500, 200);
    stats("ms5", 5000, 100);
    stats("ms25", 25000, 50);
}
