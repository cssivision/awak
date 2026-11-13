//! Task-scale stress: how far does the executor hold up?
//!
//! The local run queue holds 512 runnables (`Local::new(512)` in
//! `Executor::new`) and the global queue is unbounded, so any burst above 512
//! already exercises the overflow path. What this measures is whether that path
//! keeps scaling into the hundreds of thousands, and where the cost bends.
//!
//! Three shapes, because they stress different things:
//!
//! * **burst** -- every task is spawned before any is awaited. All of them are
//!   live at once, so this is the peak-memory and queue-pressure case.
//! * **chain** -- each task spawns the next only after being polled, so exactly
//!   one task is live at a time. Isolates wake-up latency from queue pressure;
//!   it is the shape that reveals cost in the park/unpark path.
//! * **timers** -- many timers alive at once, stressing the timer map and the
//!   reactor's wake-up path rather than the run queue.
//!
//! ```sh
//! cargo run --release --example task_scale            # all, 200k tasks
//! cargo run --release --example task_scale burst 500000
//! cargo run --release --example task_scale chain 1000000
//! ```
//!
//! Every scenario asserts that the expected number of tasks ran, so a timing
//! that looks good because tasks were dropped cannot pass unnoticed.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Wall-clock CPU time of the whole process, in milliseconds.
fn cpu_ms() -> u64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap();
    let f: Vec<&str> = s.rsplit(')').next().unwrap().split_whitespace().collect();
    (f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap()) * 10
}

/// Peak resident set size in KiB (`VmHWM`), which is what a burst of live tasks
/// actually costs.
fn peak_rss_kb() -> u64 {
    let s = match std::fs::read_to_string("/proc/self/status") {
        Ok(s) => s,
        Err(_) => return 0,
    };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest
                .split_whitespace()
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
        }
    }
    0
}

fn emit(name: &str, ops: usize, wall: Duration, cpu: u64) {
    println!(
        "RESULT name={} ops={} wall_ms={} cpu_ms={} peak_rss_kb={}",
        name,
        ops,
        wall.as_millis(),
        cpu,
        peak_rss_kb()
    );
}

/// Spawns all `n` tasks in one burst, then awaits them.
fn burst(n: usize) {
    let c0 = cpu_ms();
    let t = Instant::now();
    awak::block_on(async move {
        let done = Arc::new(AtomicUsize::new(0));
        let mut ts = Vec::with_capacity(n);
        for _ in 0..n {
            let d = Arc::clone(&done);
            ts.push(awak::spawn(async move {
                d.fetch_add(1, Ordering::Relaxed);
            }));
        }
        for x in ts {
            x.await;
        }
        assert_eq!(done.load(Ordering::Relaxed), n, "lost tasks at n = {n}");
    });
    emit(&format!("burst_{n}"), n, t.elapsed(), cpu_ms() - c0);
}

/// Each task spawns the next after being polled, so one task is live at a time.
fn chain(n: usize) {
    let c0 = cpu_ms();
    let t = Instant::now();
    let reached = awak::block_on(async move {
        let mut task = awak::spawn(async { 0usize });
        for _ in 0..n {
            let prev = task.await;
            let next = prev + 1;
            task = awak::spawn(async move { next });
        }
        task.await
    });
    assert_eq!(reached, n, "chain stopped short at {reached}");
    emit(&format!("chain_{n}"), n, t.elapsed(), cpu_ms() - c0);
}

/// Many timers alive at once.
fn timers(n: usize) {
    let c0 = cpu_ms();
    let t = Instant::now();
    awak::block_on(async move {
        let done = Arc::new(AtomicUsize::new(0));
        let mut ts = Vec::with_capacity(n);
        for i in 0..n {
            let d = Arc::clone(&done);
            // Spread the deadlines so they do not all land on one tick.
            let delay = Duration::from_micros(300 + (i % 7) as u64);
            ts.push(awak::spawn(async move {
                awak::time::delay_for(delay).await;
                d.fetch_add(1, Ordering::Relaxed);
            }));
        }
        for x in ts {
            x.await;
        }
        assert_eq!(done.load(Ordering::Relaxed), n, "lost timers at n = {n}");
    });
    emit(&format!("timers_{n}"), n, t.elapsed(), cpu_ms() - c0);
}

fn main() {
    let what = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    let n: usize = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000);

    // Warm the reactor and executor: the first scenario otherwise absorbs
    // thread spawn and lazy initialisation.
    awak::block_on(async { awak::time::delay_for(Duration::from_millis(10)).await });

    if what == "all" || what == "burst" {
        burst(n);
    }
    if what == "all" || what == "chain" {
        chain(n);
    }
    if what == "all" || what == "timers" {
        timers(n);
    }
}
