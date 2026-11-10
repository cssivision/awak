#![allow(clippy::len_without_is_empty)]
use std::cell::UnsafeCell;
use std::error::Error;
use std::fmt;
use std::future::{poll_fn, Future};
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use async_task::{Runnable, Task};
use rand::RngExt;
use thread_local::ThreadLocal;

use crate::queue::Queue;

/// A multi-threaded executor.
pub struct Executor {
    state: Arc<State>,
}

impl Default for Executor {
    fn default() -> Executor {
        Executor::new()
    }
}

impl Executor {
    pub fn new() -> Executor {
        Executor {
            state: Arc::new(State {
                queue: Queue::unbound(),
                notified: AtomicBool::new(false),
                local_queue: ThreadLocal::new(),
                sleepers: Mutex::new(Sleepers {
                    count: 0,
                    wakers: Vec::new(),
                    id_gen: 1,
                }),
            }),
        }
    }

    pub fn spawn<T: Send + 'static>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Task<T> {
        let state = self.state.clone();

        let schedule = move |runnable| {
            let mut runnable = Some(runnable);
            // Try to push into the local queue.
            if let Some(local_queue) = state.local_queue.get() {
                if let Err(e) = local_queue.local.push(runnable.take().unwrap()) {
                    runnable = Some(e.into_inner());
                } else {
                    local_queue.waker.wake_by_ref();
                }
            }

            if let Some(runnable) = runnable {
                state.queue.push(runnable).unwrap();
                state.notify();
            }
        };

        let (runnable, task) = async_task::spawn(future, schedule);
        runnable.schedule();
        task
    }

    pub fn ticker(&self) -> Ticker {
        Ticker {
            state: self.state.clone(),
            sleeping: AtomicUsize::new(0),
            ticks: AtomicUsize::new(0),
        }
    }
}

struct State {
    /// The global queue.
    queue: Queue<Runnable>,

    /// Set to `true` when a sleeping ticker is notified or no tickers are sleeping.
    notified: AtomicBool,

    /// Shards of the global queue created by tickers.
    local_queue: ThreadLocal<LocalQueue>,

    /// A list of sleeping tickers.
    sleepers: Mutex<Sleepers>,
}

/// A list of sleeping tickers.
#[derive(Debug)]
struct Sleepers {
    /// Number of sleeping tickers (both notified and unnotified).
    count: usize,

    /// Wakers of sleeping unnotified tickers.
    ///
    /// A sleeping ticker is notified when its callback is missing from this list.
    wakers: Vec<(usize, Waker)>,

    /// ID generator for sleepers.
    id_gen: usize,
}

impl Sleepers {
    /// Returns notification callback for a sleeping ticker.
    ///
    /// If a ticker was notified already or there are no tickers, `None` will be returned.
    fn notify(&mut self) -> Option<Waker> {
        if self.wakers.len() == self.count {
            self.wakers.pop().map(|item| item.1)
        } else {
            None
        }
    }

    /// Re-inserts a sleeping ticker's waker if it was notified.
    ///
    /// Returns `true` if the ticker was notified.
    fn update(&mut self, id: usize, waker: &Waker) -> bool {
        for item in &mut self.wakers {
            if item.0 == id {
                if !item.1.will_wake(waker) {
                    item.1.clone_from(waker);
                }
                return false;
            }
        }

        self.wakers.push((id, waker.clone()));
        true
    }

    /// Returns `true` if a sleeping ticker is notified or no tickers are sleeping.
    fn is_notified(&self) -> bool {
        self.count == 0 || self.count > self.wakers.len()
    }

    /// Removes a previously inserted sleeping ticker.
    fn remove(&mut self, id: usize) {
        self.count -= 1;
        for i in (0..self.wakers.len()).rev() {
            if self.wakers[i].0 == id {
                self.wakers.remove(i);
                return;
            }
        }
    }

    /// Inserts a new sleeping ticker.
    fn insert(&mut self, waker: &Waker) -> usize {
        let id = self.id_gen;
        self.id_gen += 1;
        self.count += 1;
        self.wakers.push((id, waker.clone()));
        id
    }
}

impl State {
    fn notify(&self) {
        if self
            .notified
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let waker = self.sleepers.lock().unwrap().notify();
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
}

struct LocalQueue {
    local: Local<Runnable>,
    waker: Waker,
}

/// Runs tasks in a multi-threaded executor.
pub struct Ticker {
    /// The global state.
    state: Arc<State>,

    /// Set to `sleeper's id` when in sleeping state.
    ///
    /// States a ticker can be in:
    /// 1) Woken.
    ///    2a) Sleeping and unnotified.
    ///    2b) Sleeping and notified.
    sleeping: AtomicUsize,

    /// Bumped every time a task is run.
    ticks: AtomicUsize,
}

impl Ticker {
    /// Moves the ticker into sleeping and unnotified state.
    ///
    /// Returns `false` if the ticker was already sleeping and unnotified.
    fn sleep(&self, waker: &Waker) -> bool {
        let mut sleepers = self.state.sleepers.lock().unwrap();
        match self.sleeping.load(Ordering::SeqCst) {
            // Move to sleeping state.
            0 => self
                .sleeping
                .store(sleepers.insert(waker), Ordering::SeqCst),
            id => {
                // Already sleeping, check if notified.
                if !sleepers.update(id, waker) {
                    return false;
                }
            }
        }
        self.state
            .notified
            .swap(sleepers.is_notified(), Ordering::SeqCst);
        true
    }

    fn wake(&self) {
        let id = self.sleeping.swap(0, Ordering::SeqCst);
        if id == 0 {
            return;
        }
        let mut sleepers = self.state.sleepers.lock().unwrap();
        sleepers.remove(id);
        self.state
            .notified
            .swap(sleepers.is_notified(), Ordering::SeqCst);
    }

    pub async fn run(&self) {
        let local_queue = poll_fn(|cx| {
            Poll::Ready(self.state.local_queue.get_or(|| LocalQueue {
                local: Local::new(512),
                waker: cx.waker().clone(),
            }))
        })
        .await;

        loop {
            for _ in 0..200 {
                let runnable = self.tick(local_queue).await;
                runnable.run();
            }
            yield_now().await;
        }
    }

    /// Return a single task and returns `None` if one was found.
    async fn tick(&self, local_queue: &LocalQueue) -> Runnable {
        poll_fn(|cx| {
            match self.search(local_queue) {
                None => {
                    self.sleep(cx.waker());

                    // A task pushed between the `search` above and the point
                    // where this ticker became a registered sleeper could not
                    // wake anyone: `notify` found no sleeper and only set
                    // `notified`, which `sleep` then overwrote. Re-check now
                    // that we are registered so such a task cannot be stranded.
                    if let Some(r) = self.search(local_queue) {
                        self.wake();
                        return Poll::Ready(r);
                    }

                    Poll::Pending
                }
                Some(r) => {
                    self.wake();
                    // Notify another ticker to pick up leftover work, but only when there is
                    // any: a wake-up per task is wasted work on the common single-task path.
                    if !local_queue.local.is_empty() {
                        self.state.notify();
                    }
                    // Bump the ticker.
                    let ticks = self.ticks.fetch_add(1, Ordering::SeqCst);
                    // Steal tasks from the global queue to ensure fair task scheduling.
                    if ticks.is_multiple_of(64) {
                        local_queue.local.steal(
                            self.state.queue.len(),
                            || self.state.queue.pop().ok(),
                            |r| self.put_back(r),
                        );
                    }
                    Poll::Ready(r)
                }
            }
        })
        .await
    }

    /// Finds the next task to run.
    fn search(&self, local_queue: &LocalQueue) -> Option<Runnable> {
        if let Ok(r) = local_queue.local.pop() {
            return Some(r);
        }

        // Try stealing from the global queue.
        if let Ok(r) = self.state.queue.pop() {
            local_queue.local.steal(
                self.state.queue.len(),
                || self.state.queue.pop().ok(),
                |r| self.put_back(r),
            );
            return Some(r);
        }

        // Try stealing from other shards.
        let local_queues = &self.state.local_queue;

        // Pick a random starting point in the iterator list and rotate the list.
        let n = local_queues.iter().count();
        let start = rand::rng().random_range(0..n);
        let iter = local_queues
            .iter()
            .chain(local_queues.iter())
            .skip(start)
            .take(n)
            .filter(|shard| !std::ptr::eq(*shard, local_queue));

        // Try stealing from each shard in the list.
        for shard in iter {
            local_queue.local.steal(
                shard.local.len(),
                || shard.local.pop().ok(),
                |r| self.put_back(r),
            );
            if let Ok(r) = local_queue.local.pop() {
                return Some(r);
            }
        }
        None
    }

    /// Returns a task that could not fit into a local queue to the global one.
    ///
    /// The global queue is unbounded, so this never loses the task. Without it a
    /// stolen `Runnable` would be dropped, which cancels the task and makes the
    /// awaiting `Task` panic with `polled after completion`.
    fn put_back(&self, runnable: Runnable) {
        if self.state.queue.push(runnable).is_ok() {
            self.state.notify();
        }
    }
}

#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
struct YieldNow(bool);

fn yield_now() -> YieldNow {
    YieldNow(false)
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.0 {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

#[derive(Debug)]
struct Slot<T> {
    /// Sequence number coordinating producers and consumers.
    ///
    /// The slot at position `pos` is free when `sequence == pos`, filled when
    /// `sequence == pos + 1`, and reusable once the consumer that took it
    /// advances `sequence` to `pos + capacity`.
    sequence: AtomicUsize,

    /// Stores the value.
    value: UnsafeCell<MaybeUninit<T>>,
}

#[derive(Debug)]
pub struct Local<T> {
    /// Concurrently updated by many threads.
    head: AtomicUsize,

    /// Only updated by current thread but read by many threads.
    tail: AtomicUsize,

    /// Masks the head / tail position value to obtain the index in the buffer.
    mask: usize,

    /// Stores the values, one sequence number per slot.
    buffer: Box<[Slot<T>]>,

    /// capacity.
    capacity: usize,
}

impl<T> Local<T> {
    /// Steals some items from another into local.
    ///
    /// `on_full` receives any value that could not be pushed because this queue
    /// filled up in the meantime. `steal` must never drop a stolen value: for
    /// `Runnable` that cancels the task, and awaiting the cancelled task panics.
    /// The budget below is computed from a snapshot of `len()`, so a concurrent
    /// writer targeting the same queue can invalidate it.
    fn steal(&self, length: usize, mut f: impl FnMut() -> Option<T>, mut on_full: impl FnMut(T)) {
        // Half of `src`'s length rounded up.
        let mut count = length.div_ceil(2);
        if count > 0 {
            // Don't steal more than fits into local. `saturating_sub` because
            // `len` is a snapshot and may be stale by the time we use it.
            count = count.min(self.capacity().saturating_sub(self.len()));
            for _ in 0..count {
                match f() {
                    Some(t) => {
                        if let Err(e) = self.push(t) {
                            on_full(e.into_inner());
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }
}

unsafe impl<T> Send for Local<T> {}
unsafe impl<T> Sync for Local<T> {}

impl<T> Local<T> {
    fn new(n: usize) -> Local<T> {
        let capacity = n.next_power_of_two();
        let mask = capacity - 1;

        let mut buffer = Vec::with_capacity(capacity);
        for i in 0..capacity {
            buffer.push(Slot {
                sequence: AtomicUsize::new(i),
                value: UnsafeCell::new(MaybeUninit::uninit()),
            });
        }

        Local {
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            mask,
            buffer: buffer.into_boxed_slice(),
            capacity,
        }
    }

    fn pop(&self) -> Result<T, ErrorEmpty> {
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            let slot = &self.buffer[head & self.mask];
            let sequence = slot.sequence.load(Ordering::Acquire);

            // `sequence == head + 1` means the value has been fully published.
            match sequence.wrapping_sub(head.wrapping_add(1)) as isize {
                0 => match self.head.compare_exchange_weak(
                    head,
                    head.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // Safety: we claimed this slot, so no producer can
                        // write to it until we publish the new sequence.
                        let value = unsafe { slot.value.get().read().assume_init() };

                        // Make the slot available for a future lap.
                        slot.sequence
                            .store(head.wrapping_add(self.capacity), Ordering::Release);
                        return Ok(value);
                    }
                    Err(current) => head = current,
                },
                // The slot has not been filled yet: the queue is empty.
                diff if diff < 0 => return Err(ErrorEmpty),
                // Another consumer took this slot; retry with the new head.
                _ => head = self.head.load(Ordering::Relaxed),
            }
        }
    }

    fn push(&self, value: T) -> Result<(), ErrorFull<T>> {
        let mut tail = self.tail.load(Ordering::Relaxed);
        loop {
            let slot = &self.buffer[tail & self.mask];
            let sequence = slot.sequence.load(Ordering::Acquire);

            // `sequence == tail` means the slot is free and can be written.
            match sequence.wrapping_sub(tail) as isize {
                0 => match self.tail.compare_exchange_weak(
                    tail,
                    tail.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // Safety: this slot is reserved for us, and no consumer
                        // will read it until `sequence` is published.
                        unsafe {
                            slot.value.get().write(MaybeUninit::new(value));
                        }

                        // Publish the value to consumers.
                        slot.sequence.store(tail.wrapping_add(1), Ordering::Release);
                        return Ok(());
                    }
                    Err(current) => tail = current,
                },
                // The slot has not been consumed yet: the queue is full.
                diff if diff < 0 => return Err(ErrorFull { inner: value }),
                // Another producer reserved this slot; retry with the new tail.
                _ => tail = self.tail.load(Ordering::Relaxed),
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);

        let length = tail.wrapping_sub(head);
        if length <= self.buffer.len() {
            length
        } else {
            self.capacity
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[derive(Debug)]
struct ErrorFull<T> {
    inner: T,
}

impl<T> fmt::Display for ErrorFull<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "queue full")
    }
}

impl<T: std::fmt::Debug> Error for ErrorFull<T> {}

impl<T> ErrorFull<T> {
    fn into_inner(self) -> T {
        self.inner
    }
}

#[derive(Debug)]
struct ErrorEmpty;

impl fmt::Display for ErrorEmpty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "queue empty")
    }
}

impl Error for ErrorEmpty {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn local_queue_concurrent_push_pop() {
        const N: usize = 200_000;

        let queue = Arc::new(Local::new(64));
        let done = Arc::new(AtomicBool::new(false));

        let mut consumers = Vec::new();
        for _ in 0..4 {
            let queue = Arc::clone(&queue);
            let done = Arc::clone(&done);
            consumers.push(std::thread::spawn(move || {
                let mut seen = Vec::new();
                loop {
                    match queue.pop() {
                        Ok(value) => seen.push(value),
                        Err(_) if done.load(Ordering::Acquire) => {
                            while let Ok(value) = queue.pop() {
                                seen.push(value);
                            }
                            return seen;
                        }
                        Err(_) => std::thread::yield_now(),
                    }
                }
            }));
        }

        let producer = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                for i in 0..N {
                    let mut value = i;
                    loop {
                        match queue.push(value) {
                            Ok(()) => break,
                            Err(err) => {
                                value = err.into_inner();
                                std::thread::yield_now();
                            }
                        }
                    }
                }
                done.store(true, Ordering::Release);
            })
        };
        producer.join().unwrap();

        let mut seen = vec![false; N];
        let mut total = 0;
        for consumer in consumers {
            for value in consumer.join().unwrap() {
                assert!(!seen[value], "value {} was popped twice", value);
                seen[value] = true;
                total += 1;
            }
        }
        assert_eq!(total, N);
        assert!(seen.iter().all(|&seen| seen));
    }

    /// `Ticker::search` lets several tickers run `steal` against the *same*
    /// `Local` shard, so `push` has more than one writer. When `push` assumed a
    /// single writer, concurrent steals overwrote each other's slot and values
    /// were lost, which stranded the executor.
    ///
    /// The queue is sized so that every value fits: `steal` silently drops a
    /// value when `push` reports the queue full, which would otherwise make this
    /// test fail for an unrelated reason.
    #[test]
    fn local_queue_concurrent_steal_loses_nothing() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 500;
        const TOTAL: usize = THREADS * PER_THREAD;
        const ROUNDS: usize = 30;

        for _ in 0..ROUNDS {
            let victim = Arc::new(Local::new(TOTAL.next_power_of_two()));
            let barrier = Arc::new(std::sync::Barrier::new(THREADS));

            let stealers: Vec<_> = (0..THREADS)
                .map(|thread| {
                    let victim = Arc::clone(&victim);
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        // Release all writers at the same instant so their
                        // `tail` updates actually overlap.
                        barrier.wait();
                        for i in 0..PER_THREAD {
                            victim.steal(
                                1,
                                || Some(thread * PER_THREAD + i),
                                |_| panic!("test queue is sized to hold every value"),
                            );
                        }
                    })
                })
                .collect();
            for stealer in stealers {
                stealer.join().unwrap();
            }

            let mut values = Vec::new();
            while let Ok(value) = victim.pop() {
                values.push(value);
            }
            values.sort_unstable();

            // Report a compact summary rather than dumping both vectors.
            if values.len() != TOTAL {
                let unique: std::collections::HashSet<_> = values.iter().copied().collect();
                panic!(
                    "concurrent steals lost items: drained {} of {} values ({} unique)",
                    values.len(),
                    TOTAL,
                    unique.len()
                );
            }
            let expected: Vec<usize> = (0..TOTAL).collect();
            if values != expected {
                let missing = expected.iter().filter(|v| !values.contains(v)).count();
                panic!(
                    "concurrent steals corrupted order/content: {} values missing",
                    missing
                );
            }
        }
    }

    /// A queue that fills up while `steal` is running must hand the leftover
    /// values to `on_full` instead of dropping them.
    ///
    /// `steal` budgets from a `len()` snapshot, so a concurrent writer targeting
    /// the same queue can fill it after the budget was computed. Dropping a
    /// `Runnable` cancels the task and awaiting it panics with `polled after
    /// completion`, so a lost value here is a user-visible failure.
    ///
    /// The race is simulated by having `f` fill the destination itself, which is
    /// exactly what a second writer would do.
    #[test]
    fn steal_returns_overflow_instead_of_dropping() {
        const ROUNDS: usize = 20;

        for _ in 0..ROUNDS {
            let dest = Local::<usize>::new(8);
            let source_items = 100;
            let mut spilled = Vec::new();
            let mut next = 0;
            let mut filled_by_other_writer = false;

            dest.steal(
                source_items,
                || {
                    if next >= source_items {
                        return None;
                    }
                    // A concurrent writer grabs the remaining space right after
                    // `steal` computed its budget.
                    if !filled_by_other_writer {
                        filled_by_other_writer = true;
                        while dest.push(9999).is_ok() {}
                    }
                    let v = next;
                    next += 1;
                    Some(v)
                },
                |v| spilled.push(v),
            );

            assert!(
                filled_by_other_writer,
                "test setup did not simulate a concurrent writer"
            );
            assert!(
                !spilled.is_empty(),
                "steal dropped values instead of reporting them via on_full"
            );
            // Every value pulled from the source is either in the queue or was
            // handed back.
            let mut drained = Vec::new();
            while let Ok(v) = dest.pop() {
                drained.push(v);
            }
            let stolen_total = drained.iter().filter(|v| **v != 9999).count() + spilled.len();
            assert_eq!(
                stolen_total, next,
                "steal lost values: pulled {} from source but accounted for {}",
                next, stolen_total
            );
        }
    }
}
