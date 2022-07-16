use std::collections::BTreeMap;
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

use super::poller::{Event, Poller};
use crate::future::poll_fn;
use crate::parking;
use crate::queue::Queue;

const DEFAULT_TIME_OP_SIZE: usize = 1000;
const READ: usize = 0;
const WRITE: usize = 1;

pub(crate) struct Reactor {
    pub block_on_count: AtomicUsize,
    pub unparker: parking::Unparker,
    ticker: AtomicUsize,
    poller: Poller,
    sources: Mutex<Slab<Arc<Source>>>,
    events: Mutex<Vec<Event>>,
    timer_ops: Queue<TimerOp>,
    timers: Mutex<BTreeMap<(Instant, usize), Waker>>,
}

/// A single timer operation.
enum TimerOp {
    Insert(Instant, usize, Waker),
    Remove(Instant, usize),
}

impl Drop for Reactor {
    fn drop(&mut self) {
        self.unparker.unpark();
    }
}

/// A lock on the reactor.
pub(crate) struct ReactorLock<'a> {
    reactor: &'a Reactor,
    events: MutexGuard<'a, Vec<Event>>,
}

impl ReactorLock<'_> {
    pub(crate) fn react(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        let mut wakers = Vec::new();
        let next_timer = self.reactor.process_timers(&mut wakers);

        // compute the timeout for blocking on I/O events.
        let timeout = match (next_timer, timeout) {
            (None, None) => None,
            (Some(t), None) | (None, Some(t)) => Some(t),
            (Some(a), Some(b)) => Some(a.min(b)),
        };

        // Bump the ticker before polling I/O.
        let tick = self
            .reactor
            .ticker
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);

        self.events.clear();

        let res = match self.reactor.poller.wait(&mut self.events, timeout) {
            Ok(0) => {
                if timeout != Some(Duration::from_secs(0)) {
                    // The non-zero timeout was hit so fire ready timers.
                    self.reactor.process_timers(&mut wakers);
                }
                Ok(())
            }
            Ok(_) => {
                let sources = self.reactor.sources.lock().unwrap();

                for ev in self.events.iter() {
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
                            self.reactor.poller.interest(
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
}

impl Reactor {
    pub fn get() -> &'static Reactor {
        static REACTOR: Lazy<Reactor> = Lazy::new(|| {
            let (parker, unparker) = parking::pair();
            thread::spawn(move || {
                let reactor = Reactor::get();

                // The last observed reactor tick.
                let mut last_tick = 0;
                // Number of sleeps since this thread has called `react()`.
                let mut sleeps = 0u64;

                loop {
                    let tick = reactor.ticker.load(Ordering::SeqCst);
                    if last_tick == tick {
                        let reactor_lock = if sleeps >= 10 {
                            // If no new ticks have occurred for a while, stop sleeping and
                            // spinning in this loop and just block on the reactor lock.
                            Some(Reactor::get().lock())
                        } else {
                            Reactor::get().try_lock()
                        };
                        if let Some(mut reactor_lock) = reactor_lock {
                            let _ = reactor_lock.react(None);
                            last_tick = Reactor::get().ticker.load(Ordering::SeqCst);
                            sleeps = 0;
                        }
                    } else {
                        last_tick = tick;
                    }

                    if Reactor::get().block_on_count.load(Ordering::SeqCst) > 0 {
                        // Exponential backoff from 50us to 10ms.
                        let delay_us = [50, 75, 100, 250, 500, 750, 1000, 2500, 5000]
                            .get(sleeps as usize)
                            .unwrap_or(&10_000);
                        if !parker.park_timeout(Some(Duration::from_nanos(*delay_us))) {
                            sleeps += 1;
                        } else {
                            sleeps = 0;
                            last_tick = Reactor::get().ticker.load(Ordering::SeqCst);
                        }
                    }
                }
            });
            Reactor {
                unparker,
                block_on_count: AtomicUsize::new(0),
                ticker: AtomicUsize::new(0),
                poller: Poller::new(),
                sources: Mutex::new(Slab::new()),
                events: Mutex::new(Vec::new()),
                timer_ops: Queue::with_capacity(DEFAULT_TIME_OP_SIZE),
                timers: Mutex::new(BTreeMap::new()),
            }
        });
        &REACTOR
    }

    fn lock(&self) -> ReactorLock<'_> {
        let reactor = self;
        let events = self.events.lock().unwrap();
        ReactorLock { reactor, events }
    }

    /// Attempts to lock the reactor.
    pub fn try_lock(&self) -> Option<ReactorLock<'_>> {
        self.events
            .try_lock()
            .map(|events| {
                let reactor = self;
                ReactorLock { reactor, events }
            })
            .ok()
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
