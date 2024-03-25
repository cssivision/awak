use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use async_task::{Runnable, Task};
use rand::Rng;
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
                queue: Queue::new(512),
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
                if let Err(e) = local_queue.queue.push(runnable.take().unwrap()) {
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
                    item.1 = waker.clone();
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
    queue: Queue<Runnable>,
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
    /// 2a) Sleeping and unnotified.
    /// 2b) Sleeping and notified.
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
                queue: Queue::new(512),
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
                    Poll::Pending
                }
                Some(r) => {
                    self.wake();
                    // Notify another ticker now to pick up where this ticker left off, just in
                    // case running the task takes a long time.
                    self.state.notify();
                    // Bump the ticker.
                    let ticks = self.ticks.fetch_add(1, Ordering::SeqCst);
                    // Steal tasks from the global queue to ensure fair task scheduling.
                    if ticks % 64 == 0 {
                        steal(&self.state.queue, &local_queue.queue);
                    }
                    Poll::Ready(r)
                }
            }
        })
        .await
    }

    /// Finds the next task to run.
    fn search(&self, local_queue: &LocalQueue) -> Option<Runnable> {
        if let Ok(r) = local_queue.queue.pop() {
            return Some(r);
        }

        // Try stealing from the global queue.
        if let Ok(r) = self.state.queue.pop() {
            steal(&self.state.queue, &local_queue.queue);
            return Some(r);
        }

        // Try stealing from other shards.
        let local_queues = &self.state.local_queue;

        // Pick a random starting point in the iterator list and rotate the list.
        let n = local_queues.iter().count();
        let start = rand::thread_rng().gen_range(0..n);
        let iter = local_queues
            .iter()
            .chain(local_queues.iter())
            .skip(start)
            .take(n)
            .filter(|shard| !std::ptr::eq(*shard, local_queue));

        // Try stealing from each shard in the list.
        for shard in iter {
            steal(&shard.queue, &local_queue.queue);
            if let Ok(r) = local_queue.queue.pop() {
                return Some(r);
            }
        }
        None
    }
}

/// Steals some items from one queue into another.
fn steal<T>(src: &Queue<T>, dest: &Queue<T>) {
    // Half of `src`'s length rounded up.
    let mut count = (src.len() + 1) / 2;
    if count > 0 {
        // Don't steal more than fits into the queue.
        count = count.min(dest.capacity() - dest.len());
        for _ in 0..count {
            if let Ok(t) = src.pop() {
                let _ = dest.push(t);
            } else {
                break;
            }
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
