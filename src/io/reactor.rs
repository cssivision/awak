use std::collections::BTreeMap;
use std::future::poll_fn;
use std::io;
use std::mem;
use std::os::unix::io::RawFd;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex, MutexGuard,
};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use slab::Slab;

use super::poller::Poller;
use crate::queue::Queue;

const DEFAULT_TIME_OP_SIZE: usize = 1000;
const READ: usize = 0;
const WRITE: usize = 1;

pub(crate) struct Reactor {
    ticker: AtomicUsize,
    poller: Poller,
    sources: Mutex<Slab<Arc<Source>>>,
    timer_ops: Queue<TimerOp>,
    timers: Mutex<BTreeMap<(Instant, usize), Waker>>,
}

/// A single timer operation.
enum TimerOp {
    Insert(Instant, usize, Waker),
    Remove(Instant, usize),
}

impl Reactor {
    pub fn get() -> &'static Reactor {
        static REACTOR: Lazy<Reactor> = Lazy::new(|| {
            thread::spawn(move || {
                let reactor = Reactor::get();
                loop {
                    // spinning in this loop and just block on the reactor lock.
                    let _ = reactor.react();
                }
            });
            Reactor {
                ticker: AtomicUsize::new(0),
                poller: Poller::new(),
                sources: Mutex::new(Slab::new()),
                timer_ops: Queue::with_capacity(DEFAULT_TIME_OP_SIZE),
                timers: Mutex::new(BTreeMap::new()),
            }
        });
        &REACTOR
    }

    fn react(&self) -> io::Result<()> {
        let mut wakers = Vec::new();
        // compute the timeout for blocking on I/O events.
        let timeout = self.process_timers(&mut wakers);

        // Bump the ticker before polling I/O.
        let tick = self.ticker.fetch_add(1, Ordering::SeqCst).wrapping_add(1);

        let mut events = Vec::new();
        let res = match self.poller.wait(&mut events, timeout) {
            Ok(0) => {
                if timeout != Some(Duration::from_secs(0)) {
                    // The non-zero timeout was hit so fire ready timers.
                    self.process_timers(&mut wakers);
                }
                Ok(())
            }
            Ok(_) => {
                let sources = self.sources.lock().unwrap();

                for ev in events.into_iter() {
                    if let Some(source) = sources.get(ev.key) {
                        let mut states = source.states.lock().unwrap();

                        // Wake readers if a readability event was emitted.
                        if ev.readable {
                            states[READ].tick = tick + 1;
                            wakers.append(&mut states[READ].wakers);
                        }

                        // Wake writers if a writability event was emitted.
                        if ev.writable {
                            states[WRITE].tick = tick + 1;
                            wakers.append(&mut states[WRITE].wakers);
                        }
                        if !(states[WRITE].wakers.is_empty() && states[READ].wakers.is_empty()) {
                            self.poller.interest(
                                source.raw,
                                source.key,
                                !states[READ].wakers.is_empty(),
                                !states[WRITE].wakers.is_empty(),
                            )?;
                        }
                    }
                }
                Ok(())
            }
            // The syscall was interrupted.
            Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(()),
            // An actual error occureed.
            Err(err) => Err(err),
        };

        // Wake up ready tasks.
        for waker in wakers {
            waker.wake();
        }
        res
    }

    fn interest(&self, raw: RawFd, key: usize, read: bool, write: bool) -> io::Result<()> {
        self.poller.interest(raw, key, read, write)
    }

    pub fn insert_io(&self, raw: RawFd) -> io::Result<Arc<Source>> {
        self.poller.insert(raw)?;
        let mut sources = self.sources.lock().unwrap();
        let entry = sources.vacant_entry();
        let key = entry.key();
        let source = Arc::new(Source {
            raw,
            key,
            states: Default::default(),
        });
        entry.insert(source.clone());
        Ok(source)
    }

    pub fn remove_io(&self, source: &Source) -> io::Result<()> {
        let mut sources = self.sources.lock().unwrap();
        sources.remove(source.key);
        self.poller.remove(source.raw)
    }

    pub fn insert_timer(&self, when: Instant, waker: &Waker) -> usize {
        // Generate a new timer ID.
        static ID_GENERATOR: AtomicUsize = AtomicUsize::new(1);
        let id = ID_GENERATOR.fetch_add(1, Ordering::Relaxed);
        self.timer_ops
            .push(TimerOp::Insert(when, id, waker.clone()));
        if self.timer_ops.len() >= DEFAULT_TIME_OP_SIZE {
            let mut timers = self.timers.lock().unwrap();
            self.process_timer_ops(&mut timers);
        }
        self.notify();
        id
    }

    pub fn remove_timer(&self, when: Instant, id: usize) {
        self.timer_ops.push(TimerOp::Remove(when, id));
        if self.timer_ops.len() >= DEFAULT_TIME_OP_SIZE {
            let mut timers = self.timers.lock().unwrap();
            self.process_timer_ops(&mut timers);
        }
    }

    fn process_timers(&self, wakers: &mut Vec<Waker>) -> Option<Duration> {
        let mut timers = self.timers.lock().unwrap();
        self.process_timer_ops(&mut timers);
        let now = Instant::now();

        // Split timers into ready and pending timers.
        let pending = timers.split_off(&(now, 0));
        let ready = mem::replace(&mut *timers, pending);
        let dur = if ready.is_empty() {
            timers
                .keys()
                .next()
                .map(|(when, _)| when.saturating_duration_since(now))
        } else {
            Some(Duration::from_secs(0))
        };
        drop(timers);
        for (_, waker) in ready {
            wakers.push(waker);
        }
        dur
    }

    fn process_timer_ops(&self, timers: &mut MutexGuard<'_, BTreeMap<(Instant, usize), Waker>>) {
        for _ in 0..self.timer_ops.capacity() {
            match self.timer_ops.pop() {
                Some(TimerOp::Insert(when, id, waker)) => {
                    timers.insert((when, id), waker);
                }
                Some(TimerOp::Remove(when, id)) => {
                    timers.remove(&(when, id));
                }
                None => break,
            }
        }
    }

    pub fn notify(&self) {
        self.poller.notify().expect("failed to notify reactor");
    }
}

/// A registered source of I/O events.
#[derive(Debug)]
pub(crate) struct Source {
    /// Raw file descriptor on Unix platforms.
    pub raw: RawFd,
    /// The key of this source obtained during registration.
    key: usize,
    /// Tasks interested in events on this source.
    states: Mutex<[State; 2]>,
}

#[derive(Default, Debug)]
struct State {
    /// Tasks waiting for the next event.
    wakers: Vec<Waker>,
    /// Last reactor tick that delivered an event.
    tick: usize,
    /// Ticks remembered by `poll_readable()` or `poll_writable()`.
    ticks: Option<(usize, usize)>,
}

impl Source {
    pub async fn readable(&self) -> io::Result<()> {
        poll_fn(|cx| self.poll_ready(READ, cx)).await
    }

    pub async fn writable(&self) -> io::Result<()> {
        poll_fn(|cx| self.poll_ready(WRITE, cx)).await
    }

    pub fn poll_readable(&self, cx: &Context) -> Poll<io::Result<()>> {
        self.poll_ready(READ, cx)
    }

    pub fn poll_writable(&self, cx: &Context) -> Poll<io::Result<()>> {
        self.poll_ready(WRITE, cx)
    }

    pub fn poll_ready(&self, op: usize, cx: &Context) -> Poll<io::Result<()>> {
        let mut states = self.states.lock().unwrap();
        if let Some((a, b)) = states[op].ticks {
            if states[op].tick != a && states[op].tick != b {
                states[op].ticks = None;
                return Poll::Ready(Ok(()));
            }
        }
        let was_empty = states[op].wakers.is_empty();
        if states[op].wakers.iter().all(|w| !w.will_wake(cx.waker())) {
            states[op].wakers.push(cx.waker().clone());
        }
        if states[op].ticks.is_none() {
            states[op].ticks = Some((
                Reactor::get().ticker.load(Ordering::SeqCst),
                states[op].tick,
            ));
        }
        if was_empty {
            // no wakers, register in reactor
            Reactor::get().interest(
                self.raw,
                self.key,
                !states[READ].wakers.is_empty(),
                !states[WRITE].wakers.is_empty(),
            )?;
        }
        Poll::Pending
    }
}
