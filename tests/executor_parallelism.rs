//! Executor liveness.
//!
//! The executor releases at most one sleeper per `State::notify` and relies on
//! the woken ticker passing the baton on (`Ticker::notify_next`). When the baton
//! is dropped, queued work can end up with every worker parked: the task is
//! never picked up and the workload hangs. That is what these tests watch for.
//!
//! They deliberately assert on completion, not on speed. Whether the executor
//! actually *spreads* a burst across cores has to be measured, and a measurement
//! cannot be made reliable here: with only two cores a broken chain and a working
//! one both reach a 2x speedup, so the parallelism check was blind on the very
//! machines CI runs on. `examples/wake_chain.rs` reports the numbers for a
//! manual run on a machine with cores to spare; the deterministic guarantees
//! live in the unit tests in `src/executor.rs`, which pin the state machine that
//! the chain is built from.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use awak::block_on;
use awak::time::timeout;

/// Generous: a correct build finishes every scenario in well under a second.
/// A stranding bug means it never finishes.
const LIMIT: Duration = Duration::from_secs(30);

/// Occupies the thread for roughly `ms`, so a task actually has to be scheduled
/// rather than finishing inside the spawn call.
fn burn(ms: u64) {
    let deadline = std::time::Instant::now() + Duration::from_millis(ms);
    while std::time::Instant::now() < deadline {
        std::hint::spin_loop();
    }
}

/// A long chain where each task awaits the next: exactly one runnable exists at
/// a time, so a lost wake-up strands the chain rather than slowing it.
#[test]
fn serial_chain_never_strands() {
    const N: usize = 300;

    let reached = block_on(timeout(LIMIT, async move {
        let mut task = awak::spawn(async { 0usize });
        for _ in 0..N {
            let prev = task.await;
            let next = prev + 1;
            task = awak::spawn(async move { next });
        }
        task.await
    }))
    .expect("serial chain stalled: a wake-up was lost");

    assert_eq!(reached, N);
}

/// Timers and tasks together: the executor has to keep a ticker free to drive
/// the reactor while others run.
#[test]
fn timers_and_tasks_make_progress_together() {
    const TASKS: usize = 200;

    block_on(timeout(LIMIT, async {
        let fired = Arc::new(AtomicUsize::new(0));
        let ran = Arc::new(AtomicUsize::new(0));

        let mut timers = Vec::new();
        for _ in 0..TASKS {
            let f = Arc::clone(&fired);
            timers.push(awak::spawn(async move {
                awak::time::delay_for(Duration::from_micros(200)).await;
                f.fetch_add(1, Ordering::Relaxed);
            }));
        }
        let mut workers = Vec::new();
        for _ in 0..TASKS {
            let r = Arc::clone(&ran);
            workers.push(awak::spawn(async move {
                burn(1);
                r.fetch_add(1, Ordering::Relaxed);
            }));
        }
        for t in timers {
            t.await;
        }
        for w in workers {
            w.await;
        }
        assert_eq!(fired.load(Ordering::Relaxed), TASKS, "a timer never fired");
        assert_eq!(ran.load(Ordering::Relaxed), TASKS, "a task never ran");
    }))
    .expect("timers and tasks did not all complete");
}
