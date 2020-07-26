use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

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
}

impl Ticker {
    /// Runs a single task and returns `true` if one was found.
    pub fn tick(&self) -> bool {
        loop {}
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
