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
                let should_notify = !local_queue.local.is_empty();
                if let Err(e) = local_queue.local.push(runnable.take().unwrap()) {
                    runnable = Some(e.into_inner());
                } else {
                    local_queue.waker.wake_by_ref();
                    if should_notify {
                        state.notify();
                    }
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
    ///
    /// Returns `true` if this ticker had been selected by `notify`.
    fn remove(&mut self, id: usize) -> bool {
        self.count -= 1;
        for i in (0..self.wakers.len()).rev() {
            if self.wakers[i].0 == id {
                self.wakers.remove(i);
                return false;
            }
        }
        true
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

    fn wake(&self) -> bool {
        let id = self.sleeping.swap(0, Ordering::SeqCst);
        if id == 0 {
            return false;
        }
        let mut sleepers = self.state.sleepers.lock().unwrap();
        let was_notified = sleepers.remove(id);
        self.state
            .notified
            .swap(sleepers.is_notified(), Ordering::SeqCst);
        was_notified
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
                        let was_notified = self.wake();
                        self.notify_next(was_notified, local_queue);
                        return Poll::Ready(r);
                    }

                    Poll::Pending
                }
                Some(r) => {
                    let was_notified = self.wake();
                    self.notify_next(was_notified, local_queue);
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

    /// Continues a wake-up chain, or starts one when work remains queued.
    fn notify_next(&self, was_notified: bool, local_queue: &LocalQueue) {
        if was_notified || !local_queue.local.is_empty() || self.state.queue.len() != 0 {
            self.state.notify();
        }
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

    // --- wake-up chain ---------------------------------------------------
    //
    // The executor wakes at most one sleeper per `State::notify`, and relied on
    // the woken ticker to keep the chain going. `was_notified` carries that
    // hand-off. These tests pin the two halves of the mechanism: how a sleeper
    // is classified when it wakes, and when a ticker decides to pass the
    // wake-up on.

    /// A ticker that `notify` selected must be reported as notified when it
    /// wakes, and one that was not must not be.
    ///
    /// `Sleepers` stores only *unnotified* wakers, so "absent from the list"
    /// means "already notified". `remove` returns that distinction; the chain
    /// is broken if it reports the wrong way round, because a ticker that
    /// consumes a hand-off without passing one on strands the queue.
    fn waker_for_test() -> Waker {
        use std::task::{RawWaker, RawWakerVTable};
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }

    #[test]
    fn sleepers_remove_reports_whether_notify_selected_it() {
        let mut sleepers = Sleepers {
            count: 0,
            wakers: Vec::new(),
            id_gen: 1,
        };
        let waker = waker_for_test();

        let a = sleepers.insert(&waker);
        let b = sleepers.insert(&waker);
        assert_eq!((sleepers.count, sleepers.wakers.len()), (2, 2));

        // `notify` pops one waker; that ticker is now "notified".
        let popped = sleepers.notify();
        assert!(popped.is_some(), "notify should pick one sleeper");
        // Both were registered under the same no-op waker, so we cannot tell
        // which id came out; drive it through `remove` for each and assert that
        // exactly one of them reports "was notified".
        let a_notified = sleepers.remove(a);
        let b_notified = sleepers.remove(b);
        assert!(
            a_notified != b_notified,
            "exactly one ticker should report being selected by notify, got \
             a={} b={}",
            a_notified,
            b_notified
        );
        assert_eq!(sleepers.count, 0, "every sleeper should be accounted for");
        assert!(sleepers.wakers.is_empty(), "no waker should be left behind");
    }

    /// Removing a ticker that was never notified reports `false`.
    #[test]
    fn sleepers_remove_reports_false_for_unnotified() {
        let mut sleepers = Sleepers {
            count: 0,
            wakers: Vec::new(),
            id_gen: 1,
        };
        let id = sleepers.insert(&waker_for_test());
        assert!(
            !sleepers.remove(id),
            "a ticker still present in the waker list was not notified"
        );
        assert_eq!(sleepers.count, 0);
    }

    /// `notify_next` must call `notify` exactly when work remains.
    ///
    /// The predicate is observed through its effect rather than by restating it:
    /// a sleeper is parked, `notify_next` runs, and the test checks whether that
    /// sleeper was released. Restating the condition would pass for any
    /// implementation, including a wrong one.
    #[test]
    fn notify_next_passes_the_baton_only_when_work_remains() {
        use std::sync::mpsc;

        /// Seeds the queues first, then parks one ticker, then runs
        /// `notify_next`, and reports whether the parked sleeper was released.
        ///
        /// The order matters: `spawn` notifies on its own, so a sleeper parked
        /// before the seed would be woken by the seeding rather than by
        /// `notify_next`.
        fn released(was_notified: bool, local_items: usize, global_items: usize) -> bool {
            let executor = Arc::new(Executor::new());

            // A shard must exist on this thread for `spawn` to use it.
            let local_shard = executor.state.local_queue.get_or(|| LocalQueue {
                local: Local::new(64),
                waker: waker_for_test(),
            });

            // Fill the local shard via `spawn`, which pushes into it.
            for _ in 0..local_items {
                executor.spawn(async {}).detach();
            }
            // Fill the global queue from a thread that has no shard.
            for _ in 0..global_items {
                let global = Arc::clone(&executor);
                std::thread::scope(|sc| {
                    sc.spawn(|| {
                        global.spawn(async {}).detach();
                    });
                });
            }

            // Park the only sleeper *after* seeding, so any wake-up observed
            // below can only come from `notify_next`.
            let (tx, rx) = mpsc::channel::<()>();
            let watch = crate::waker_fn::waker_fn(move || {
                let _ = tx.send(());
            });
            let ticker = executor.ticker();
            assert!(ticker.sleep(&watch), "park the sleeper under observation");

            ticker.notify_next(was_notified, local_shard);
            let fired = rx.try_recv().is_ok();

            // Drain so the shard does not leak into the next case.
            while local_shard.local.pop().is_ok() {}
            executor.state.queue.pop().ok();
            fired
        }

        // Nothing queued anywhere and no baton to pass: stay quiet, otherwise
        // every tick would issue a wake-up.
        assert!(
            !released(false, 0, 0),
            "must not wake when there is no work and no baton"
        );

        // A consumed hand-off is passed on.
        assert!(
            released(true, 0, 0),
            "a consumed hand-off must be passed on"
        );

        // A local backlog wakes a spare.
        assert!(released(false, 2, 0), "a local backlog must wake a spare");

        // A global backlog wakes a spare. This is the branch that the original
        // `is_empty()` check missed: tasks pushed from another thread sit in the
        // global queue, so a ticker whose own shard is empty still has work to
        // hand off.
        assert!(released(false, 0, 2), "a global backlog must wake a spare");
    }

    /// The baton must survive a long chain of hand-offs.
    ///
    /// `Sleepers::notify` releases exactly one sleeper and only when every
    /// sleeper is unnotified (`wakers.len() == count`). The executor therefore
    /// wakes one thread per `notify` call, and the only thing keeping a burst
    /// spread across cores is that each woken ticker passes the baton on via
    /// `notify_next(was_notified = true, ..)`.
    ///
    /// Walk that chain: release one sleeper, mark it as having been notified
    /// (what `Ticker::wake` reports as `was_notified`), and assert the next
    /// `notify` still succeeds. If a released ticker reported `false` instead,
    /// the chain would stop and the remaining sleepers would never be woken
    /// while work was still queued.
    #[test]
    fn wake_up_chain_reaches_every_sleeper() {
        use std::sync::mpsc;

        const N: usize = 16;

        let mut sleepers = Sleepers {
            count: 0,
            wakers: Vec::new(),
            id_gen: 1,
        };
        let mut receivers = Vec::new();
        for _ in 0..N {
            let (tx, rx) = mpsc::channel::<()>();
            let waker = crate::waker_fn::waker_fn(move || {
                let _ = tx.send(());
            });
            sleepers.insert(&waker);
            receivers.push(rx);
        }
        assert_eq!(sleepers.wakers.len(), sleepers.count);

        let mut released = 0;
        for round in 0..N {
            // Every remaining sleeper is unnotified, so `notify` can pick one.
            assert!(
                sleepers.wakers.len() == sleepers.count,
                "round {}: notify would be throttled with {} wakers for {} sleepers",
                round,
                sleepers.wakers.len(),
                sleepers.count
            );

            let before: Vec<usize> = sleepers.wakers.iter().map(|(id, _)| *id).collect();
            let waker = sleepers
                .notify()
                .unwrap_or_else(|| panic!("round {}: notify returned no waker", round));
            waker.wake();
            let after: Vec<usize> = sleepers.wakers.iter().map(|(id, _)| *id).collect();
            let id = before
                .into_iter()
                .find(|id| !after.contains(id))
                .expect("notify must have removed exactly one waker");

            // This is what `Ticker::wake` computes for the ticker that took the
            // baton: absent from the waker list means `notify` had selected it.
            assert!(
                sleepers.remove(id),
                "round {}: a ticker released by notify reported was_notified = \
                 false, so the chain would stop here",
                round
            );
            released += 1;
        }

        assert_eq!(
            released, N,
            "the chain released {} of {} sleepers",
            released, N
        );
        assert_eq!(sleepers.count, 0, "every sleeper should have been reached");
        let fired = receivers.iter().filter(|rx| rx.try_recv().is_ok()).count();
        assert_eq!(fired, N, "only {} of {} wakers actually fired", fired, N);
    }

    /// `Ticker::wake` returns `false` when the ticker was not sleeping.
    ///
    /// A ticker that never called `sleep` has `sleeping == 0`; waking it must be
    /// a no-op and must not report `was_notified`, otherwise `notify_next` would
    /// keep firing on a ticker that has nothing to hand off.
    #[test]
    fn wake_reports_false_when_not_sleeping() {
        let executor = Executor::new();
        let ticker = executor.ticker();
        assert!(
            !ticker.wake(),
            "waking a ticker that never slept must not report was_notified"
        );
        // Calling it again must stay false and not corrupt the sleeper count.
        assert!(!ticker.wake());
        assert_eq!(
            executor.state.sleepers.lock().unwrap().count,
            0,
            "no sleeper should have been registered"
        );
    }

    /// A ticker woken normally reports `was_notified = false`; one released by
    /// `notify` reports `true`. This is the value `notify_next` keys off.
    #[test]
    fn wake_reports_the_sleeper_state_it_left() {
        let executor = Executor::new();
        let waker = waker_for_test();

        // Case 1: plain sleep, woken by its own waker.
        let t1 = executor.ticker();
        assert!(t1.sleep(&waker), "first sleep should register the ticker");
        assert_eq!(executor.state.sleepers.lock().unwrap().count, 1);
        assert!(
            !t1.wake(),
            "a ticker woken without being selected by notify must report false"
        );
        assert_eq!(
            executor.state.sleepers.lock().unwrap().count,
            0,
            "wake must remove the sleeper"
        );

        // Case 2: sleep, then let `notify` select it, then wake.
        let t2 = executor.ticker();
        assert!(t2.sleep(&waker));
        executor.state.notify();
        assert!(
            t2.wake(),
            "a ticker selected by notify must report was_notified = true"
        );
        assert_eq!(executor.state.sleepers.lock().unwrap().count, 0);
    }

    /// `State::notify` releases at most one sleeper per call.
    ///
    /// This is the property the chain has to work around: with several sleepers
    /// registered, a single `notify` must leave the rest waiting, so the batch
    /// of wake-ups only happens because each woken ticker passes the baton on.
    #[test]
    fn notify_releases_one_sleeper_at_a_time() {
        use std::sync::mpsc;

        let executor = Executor::new();
        let mut tickers = Vec::new();
        let mut receivers = Vec::new();
        for _ in 0..8 {
            let (tx, rx) = mpsc::channel::<()>();
            let waker = crate::waker_fn::waker_fn(move || {
                let _ = tx.send(());
            });
            let t = executor.ticker();
            assert!(t.sleep(&waker));
            tickers.push(t);
            receivers.push(rx);
        }
        assert_eq!(executor.state.sleepers.lock().unwrap().count, 8);

        executor.state.notify();

        // Exactly one waker fires, and exactly one ticker reports having been
        // selected -- that is the baton the chain is built on.
        let fired = receivers.iter().filter(|rx| rx.try_recv().is_ok()).count();
        assert_eq!(
            fired, 1,
            "one notify must release exactly one sleeper, released {}",
            fired
        );

        {
            let sleepers = executor.state.sleepers.lock().unwrap();
            assert_eq!(sleepers.count, 8, "all eight are still sleeping");
            assert_eq!(
                sleepers.wakers.len(),
                7,
                "notify pops one waker, so seven remain unnotified"
            );
            assert!(
                sleepers.is_notified(),
                "the executor should consider itself notified while one ticker                  is released and the rest are waiting"
            );
        }

        // Waking all of them is fine; what matters is that only the selected one
        // reports `true`, so only it would pass the baton on.
        let flagged: Vec<bool> = tickers.iter().map(|t| t.wake()).collect();
        assert_eq!(
            flagged.iter().filter(|w| **w).count(),
            1,
            "exactly one ticker should report having been selected by notify"
        );
        assert_eq!(
            executor.state.sleepers.lock().unwrap().count,
            0,
            "every ticker should be out of the sleeper set after wake"
        );
    }

    /// `spawn` from a thread without a shard wakes a sleeper.
    ///
    /// That path pushes straight to the global queue, which is the case the
    /// `notify` in `Executor::spawn` has to cover. Without it the task would sit
    /// in the queue with every worker parked.
    #[test]
    fn spawn_without_a_shard_wakes_a_sleeper() {
        let executor = std::sync::Arc::new(Executor::new());
        let waker = waker_for_test();

        // Park one ticker so there is a sleeper to wake.
        let ticker = executor.ticker();
        assert!(ticker.sleep(&waker));
        assert_eq!(executor.state.sleepers.lock().unwrap().count, 1);

        // Spawn from this thread, which has no shard.
        let task = executor.spawn(async {});
        assert_eq!(
            executor.state.queue.len(),
            1,
            "without a shard the task must land in the global queue"
        );
        assert!(
            ticker.wake(),
            "the spawn must have notified the parked ticker"
        );
        task.detach();
    }

    /// `spawn` only wakes a spare when the shard already had a backlog.
    ///
    /// The first task pushed into an empty shard is picked up by the shard's
    /// owner through `waker.wake_by_ref()`, so waking a spare as well would be
    /// wasted work on the common single-task path. Once the shard is non-empty
    /// there is work for a second thread, and `spawn` has to say so -- the
    /// owner alone would serialise the backlog.
    ///
    /// Observed through a parked sleeper: if `spawn` notifies, the sleeper is
    /// released; otherwise it stays parked.
    #[test]
    fn spawn_wakes_a_spare_only_when_the_shard_has_a_backlog() {
        use std::sync::mpsc;

        /// Fills `prefill` tasks into a fresh shard, then spawns one more while a
        /// sleeper is parked, and reports whether that sleeper was released.
        fn spare_woken(prefill: usize) -> bool {
            let executor = Arc::new(Executor::new());
            // Create the shard on this thread so `spawn` below targets it.
            executor.state.local_queue.get_or(|| LocalQueue {
                local: Local::new(64),
                waker: waker_for_test(),
            });

            if prefill > 0 {
                executor.spawn(async {}).detach();
            }

            // Park the only sleeper after the prefill, so the next `spawn` is the
            // only thing that could wake it.
            let (tx, rx) = mpsc::channel::<()>();
            let watch = crate::waker_fn::waker_fn(move || {
                let _ = tx.send(());
            });
            let ticker = executor.ticker();
            assert!(ticker.sleep(&watch));

            executor.spawn(async {}).detach();
            let fired = rx.try_recv().is_ok();
            let _ = ticker.wake();
            fired
        }

        // Empty shard: the owner's own waker is enough, no spare needed.
        assert!(
            !spare_woken(0),
            "the first task in a shard must not wake a spare"
        );

        // Non-empty shard: there is work for a second thread.
        assert!(spare_woken(1), "a backlog in the shard must wake a spare");
    }

    /// `State::notify` is throttled: once a wake-up is outstanding, further
    /// calls do nothing until a ticker consumes it.
    ///
    /// This is what keeps the executor from waking every sleeper on every push.
    /// Dropping the check does not break correctness, so no functional test
    /// notices -- the only symptom is a storm of redundant wake-ups. Pin it
    /// directly.
    #[test]
    fn notify_is_throttled_while_a_wake_up_is_outstanding() {
        use std::sync::mpsc;

        let executor = Executor::new();

        // Two sleepers, each with an observable waker.
        let (tx1, rx1) = mpsc::channel::<()>();
        let (tx2, rx2) = mpsc::channel::<()>();
        let w1 = crate::waker_fn::waker_fn(move || {
            let _ = tx1.send(());
        });
        let w2 = crate::waker_fn::waker_fn(move || {
            let _ = tx2.send(());
        });
        let t1 = executor.ticker();
        let t2 = executor.ticker();
        assert!(t1.sleep(&w1));
        assert!(t2.sleep(&w2));
        assert_eq!(executor.state.sleepers.lock().unwrap().count, 2);

        // First notify releases one sleeper and sets `notified`.
        executor.state.notify();
        let fired_after_first = [rx1.try_recv().is_ok(), rx2.try_recv().is_ok()]
            .iter()
            .filter(|f| **f)
            .count();
        assert_eq!(
            fired_after_first, 1,
            "the first notify releases one sleeper"
        );
        assert!(
            executor.state.notified.load(Ordering::SeqCst),
            "a wake-up is now outstanding"
        );

        // Second notify must be swallowed: the outstanding wake-up has not been
        // consumed yet. Without the throttle this would release the other
        // sleeper too, and every push would wake the whole pool.
        executor.state.notify();
        let fired_after_second = [rx1.try_recv().is_ok(), rx2.try_recv().is_ok()]
            .iter()
            .filter(|f| **f)
            .count();
        assert_eq!(
            fired_after_second, 0,
            "a second notify must be throttled while a wake-up is outstanding"
        );

        // Consuming the wake-up (a ticker waking) re-arms the throttle. Both are
        // woken so the one that `notify` actually selected is covered -- `||`
        // would short-circuit on the first and leave the selected ticker asleep.
        let woken1 = t1.wake();
        let woken2 = t2.wake();
        assert!(
            woken1 || woken2,
            "one of the tickers must report having been released"
        );
        // Both tickers left the sleeping set (waking is unconditional), so there
        // is nobody left to wake: `is_notified` reports true for "no sleepers",
        // which re-arms the throttle rather than leaving it stuck.
        assert_eq!(
            executor.state.sleepers.lock().unwrap().count,
            0,
            "both tickers should have left the sleeping set"
        );
        assert!(
            executor.state.notified.load(Ordering::SeqCst),
            "with no sleepers left, the next notify must be free to fire"
        );

        let fired_after_rearm = [rx1.try_recv().is_ok(), rx2.try_recv().is_ok()]
            .iter()
            .filter(|f| **f)
            .count();
        assert_eq!(fired_after_rearm, 0, "no further sleeper should have fired");
    }
}
