use std::cell::Cell;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use concurrent_queue::ConcurrentQueue;
use rand::Rng;

/// A multi-threaded executor.
#[derive(Debug)]
pub struct Executor {
    global: Arc<Global>,
}

#[derive(Debug)]
pub struct Task<T>(Option<async_task::JoinHandle<T, ()>>);

impl Executor {
    pub fn new() -> Executor {
        Executor {
            global: Arc::new(Global {
                queue: ConcurrentQueue::unbounded(),
                notified: AtomicBool::new(false),
                shards: RwLock::new(Vec::new()),
                sleepers: Mutex::new(Sleepers {
                    count: 0,
                    callbacks: Vec::new(),
                }),
            }),
        }
    }

    pub fn spawn<T: Send + 'static>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Task<T> {
        let global = self.global.clone();

        let schedule = move |runnable| {
            global.queue.push(runnable).unwrap();
            global.notify();
        };

        let (runnable, handle) = async_task::spawn(future, schedule, ());
        runnable.schedule();

        Task(Some(handle))
    }

    pub fn ticker(&self, notify: impl Fn() + Send + Sync + 'static) -> Ticker {
        let ticker = Ticker {
            global: Arc::new(self.global.clone()),
            shard: Arc::new(ConcurrentQueue::bounded(512)),
            callback: Callback::new(notify),
            sleeping: Cell::new(false),
            ticks: Cell::new(0),
        };

        self.global
            .shards
            .write()
            .unwrap()
            .push(ticker.shard.clone());

        ticker
    }
}

/// A cloneable callback function.
#[derive(Clone)]
struct Callback(Arc<Box<dyn Fn() + Send + Sync>>);

impl Callback {
    fn new(f: impl Fn() + Send + Sync + 'static) -> Callback {
        Callback(Arc::new(Box::new(f)))
    }

    fn call(&self) {
        (self.0)();
    }
}

impl PartialEq for Callback {
    fn eq(&self, other: &Callback) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Callback {}

impl fmt::Debug for Callback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("<callback>").finish()
    }
}

type Runnable = async_task::Task<()>;

#[derive(Debug)]
struct Global {
    /// The global queue.
    queue: ConcurrentQueue<Runnable>,

    /// Set to `true` when a sleeping ticker is notified or no tickers are sleeping.
    notified: AtomicBool,

    /// Shards of the global queue created by tickers.
    shards: RwLock<Vec<Arc<ConcurrentQueue<Runnable>>>>,

    /// A list of sleeping tickers.
    sleepers: Mutex<Sleepers>,
}

/// A list of sleeping tickers.
#[derive(Debug)]
struct Sleepers {
    /// Number of sleeping tickers (both notified and unnotified).
    count: usize,

    /// Callbacks of sleeping unnotified tickers.
    ///
    /// A sleeping ticker is notified when its callback is missing from this list.
    callbacks: Vec<Callback>,
}

impl Sleepers {
    /// Returns notification callback for a sleeping ticker.
    ///
    /// If a ticker was notified already or there are no tickers, `None` will be returned.
    fn notify(&mut self) -> Option<Callback> {
        if self.callbacks.len() == self.count {
            self.callbacks.pop()
        } else {
            None
        }
    }

    /// Re-inserts a sleeping ticker's callback if it was notified.
    ///
    /// Returns `true` if the ticker was notified.
    fn update(&mut self, callback: &Callback) -> bool {
        if self.callbacks.iter().all(|cb| cb != callback) {
            self.callbacks.push(callback.clone());
            true
        } else {
            false
        }
    }

    /// Returns `true` if a sleeping ticker is notified or no tickers are sleeping.
    fn is_notified(&self) -> bool {
        self.count == 0 || self.count > self.callbacks.len()
    }

    /// Removes a previously inserted sleeping ticker.
    fn remove(&mut self, callback: &Callback) {
        self.count -= 1;
        for i in (0..self.callbacks.len()).rev() {
            if &self.callbacks[i] == callback {
                self.callbacks.remove(i);
                return;
            }
        }
    }

    /// Inserts a new sleeping ticker.
    fn insert(&mut self, callback: &Callback) {
        self.count += 1;
        self.callbacks.push(callback.clone());
    }
}

impl Global {
    fn notify(&self) {
        if !self
            .notified
            .compare_and_swap(false, true, Ordering::SeqCst)
        {
            let callback = self.sleepers.lock().unwrap().notify();
            if let Some(cb) = callback {
                cb.call();
            }
        }
    }
}

/// Runs tasks in a multi-threaded executor.
#[derive(Debug)]
pub struct Ticker {
    /// The global queue.
    global: Arc<Arc<Global>>,

    /// A shard of the global queue.
    shard: Arc<ConcurrentQueue<Runnable>>,

    /// Callback invoked to wake this ticker up.
    callback: Callback,

    /// Set to `true` when in sleeping state.
    ///
    /// States a ticker can be in:
    /// 1) Woken.
    /// 2a) Sleeping and unnotified.
    /// 2b) Sleeping and notified.
    sleeping: Cell<bool>,

    /// Bumped every time a task is run.
    ticks: Cell<usize>,
}

impl Ticker {
    /// Moves the ticker into sleeping and unnotified state.
    ///
    /// Returns `false` if the ticker was already sleeping and unnotified.
    fn sleep(&self) -> bool {
        let mut sleepers = self.global.sleepers.lock().unwrap();

        if self.sleeping.get() {
            // Already sleeping, check if notified.
            if !sleepers.update(&self.callback) {
                return false;
            }
        } else {
            // Move to sleeping state.
            sleepers.insert(&self.callback);
        }

        self.global
            .notified
            .swap(sleepers.is_notified(), Ordering::SeqCst);

        self.sleeping.set(true);
        true
    }

    fn wake(&self) -> bool {
        if self.sleeping.get() {
            let mut sleepers = self.global.sleepers.lock().unwrap();
            sleepers.remove(&self.callback);

            self.global
                .notified
                .swap(sleepers.is_notified(), Ordering::SeqCst);
        }

        self.sleeping.replace(false)
    }

    /// Runs a single task and returns `true` if one was found.
    pub fn tick(&self) -> bool {
        loop {
            match self.search() {
                None => {
                    if !self.sleep() {
                        return false;
                    }
                }
                Some(r) => {
                    self.wake();

                    // Notify another ticker now to pick up where this ticker left off, just in
                    // case running the task takes a long time.
                    self.global.notify();

                    // Bump the ticker.
                    let ticks = self.ticks.get();
                    self.ticks.set(ticks.wrapping_add(1));

                    // Steal tasks from the global queue to ensure fair task scheduling.
                    if ticks % 64 == 0 {
                        steal(&self.global.queue, &self.shard);
                    }

                    r.run();

                    return true;
                }
            }
        }
    }

    /// Finds the next task to run.
    fn search(&self) -> Option<Runnable> {
        if let Ok(r) = self.shard.pop() {
            return Some(r);
        }

        // Try stealing from the global queue.
        if let Ok(r) = self.global.queue.pop() {
            return Some(r);
        }

        // Try stealing from other shards.
        let shards = self.global.shards.read().unwrap();

        // Pick a random starting point in the iterator list and rotate the list.
        let n = shards.len();
        let start = rand::thread_rng().gen_range(0, n);
        let iter = shards
            .iter()
            .chain(shards.iter())
            .skip(start)
            .take(n)
            .filter(|shard| !Arc::ptr_eq(shard, &self.shard));

        // Try stealing from each shard in the list.
        for shard in iter {
            steal(shard, &self.shard);
            if let Ok(r) = self.shard.pop() {
                return Some(r);
            }
        }

        None
    }
}

/// Steals some items from one queue into another.
fn steal<T>(src: &ConcurrentQueue<T>, dest: &ConcurrentQueue<T>) {
    // Half of `src`'s length rounded up.
    let mut count = (src.len() + 1) / 2;

    if count > 0 {
        // Don't steal more than fits into the queue.
        if let Some(cap) = dest.capacity() {
            count = count.min(cap - dest.len());
        }

        // Steal tasks.
        for _ in 0..count {
            if let Ok(t) = src.pop() {
                assert!(dest.push(t).is_ok());
            } else {
                break;
            }
        }
    }
}
