use std::collections::BTreeMap;
use std::future::poll_fn;
use std::io;
use std::mem;
use std::os::unix::io::RawFd;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex, MutexGuard, OnceLock,
};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use slab::Slab;

use super::poller::Poller;
use crate::parking;
use crate::queue::Queue;

const DEFAULT_TIME_OP_SIZE: usize = 1000;
const READ: usize = 0;
const WRITE: usize = 1;

pub(crate) struct Reactor {
    block_on_count: AtomicUsize,
    unparker: parking::Unparker,
    ticker: AtomicUsize,
    poller: Poller,
    sources: Mutex<Slab<Arc<Source>>>,
    lock: Mutex<()>,
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
        self.unpark();
    }
}

impl Reactor {
    fn ticker(&self) -> usize {
        self.ticker.load(Ordering::SeqCst)
    }

    pub fn unpark(&self) -> bool {
        self.unparker.unpark()
    }

    pub fn add_block_on_count(&self) {
        self.block_on_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn sub_block_on_count(&self) {
        self.block_on_count.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn get() -> &'static Reactor {
        static REACTOR: OnceLock<Reactor> = OnceLock::new();
        REACTOR.get_or_init(|| {
            let (parker, unparker) = parking::pair();

            thread::spawn(move || {
                // The last observed reactor tick.
                let mut last_tick = 0;
                // Number of sleeps since this thread has called `react()`.
                let mut sleeps = 0u64;

                loop {
                    let tick = Reactor::get().ticker();

                    if last_tick == tick {
                        let reactor_lock = if sleeps >= 10 {
                            // If no new ticks have occurred for a while, stop sleeping and spinning in
                            // this loop and just block on the reactor lock.
                            Some(Reactor::get().lock())
                        } else {
                            Reactor::get().try_lock()
                        };

                        if let Some(reactor_lock) = reactor_lock {
                            reactor_lock.react(None).ok();
                            last_tick = Reactor::get().ticker();
                            sleeps = 0;
                        }
                    } else {
                        last_tick = tick;
                    }

                    if Reactor::get().block_on_count.load(Ordering::Relaxed) > 0 {
                        // Exponential backoff from 50us to 10ms.
                        let delay_us = [50, 75, 100, 250, 500, 750, 1000, 2500, 5000]
                            .get(sleeps as usize)
                            .unwrap_or(&10_000);

                        if parker.park_timeout(Some(Duration::from_micros(*delay_us))) {
                            // If notified before timeout, reset the last tick and the sleep counter.
                            last_tick = Reactor::get().ticker();
                            sleeps = 0;
                        } else {
                            sleeps += 1;
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
                lock: Mutex::new(()),
                timer_ops: Queue::bound(DEFAULT_TIME_OP_SIZE),
                timers: Mutex::new(BTreeMap::new()),
            }
        })
    }

    pub fn try_lock(&self) -> Option<ReactorLock<'_>> {
        self.lock.try_lock().ok().map(|_guard| ReactorLock {
            reactor: self,
            _guard,
        })
    }

    pub fn lock(&self) -> ReactorLock<'_> {
        let _guard = self.lock.lock().unwrap();
        ReactorLock {
            reactor: self,
            _guard,
        }
    }

    fn react(&self, timeout: Option<Duration>) -> io::Result<()> {
        let mut wakers = Vec::new();
        // compute the timeout for blocking on I/O events.
        let next_timer = self.process_timers(&mut wakers);

        // compute the timeout for blocking on I/O events.
        let timeout = match (next_timer, timeout) {
            (None, None) => None,
            (Some(t), None) | (None, Some(t)) => Some(t),
            (Some(a), Some(b)) => Some(a.min(b)),
        };

        // Bump the ticker before polling I/O.
        let tick = self.ticker.fetch_add(1, Ordering::SeqCst).wrapping_add(1);

        let res = match self.poller.wait(timeout) {
            Ok(events) if events.is_empty() => {
                if timeout != Some(Duration::from_secs(0)) {
                    // The non-zero timeout was hit so fire ready timers.
                    self.process_timers(&mut wakers);
                }
                Ok(())
            }
            Ok(events) => {
                let sources = self.sources.lock().unwrap();

                for ev in events.iter() {
                    if let Some(source) = sources.get(ev.key) {
                        let mut states = source.states.lock().unwrap();

                        // Wake readers if a readability event was emitted.
                        //
                        // `closed` is a latch: once a hangup or error is seen
                        // the direction is finished and nothing clears it, which
                        // matches Tokio's `READ_CLOSED` (its `clear_readiness`
                        // preserves closed states as final). It deliberately does
                        // *not* let `poll_ready` short-circuit -- see the comment
                        // there and `bugs/closed-latch-read-spin.md`.
                        if ev.readable {
                            states.dirs[READ].tick = tick;
                            states.dirs[READ].ready = true;
                            states.dirs[READ].closed |= ev.read_closed;
                            wakers.append(&mut states.dirs[READ].wakers);
                        }

                        // Wake writers if a writability event was emitted.
                        if ev.writable {
                            states.dirs[WRITE].tick = tick;
                            states.dirs[WRITE].ready = true;
                            states.dirs[WRITE].closed |= ev.write_closed;
                            wakers.append(&mut states.dirs[WRITE].wakers);
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
        let mut sources = self.sources.lock().unwrap();
        // Reserve the slab key before registering: the poller needs it, and a
        // descriptor registered under a placeholder key would have its events
        // attributed to whichever source owns key 0 until the first re-arm.
        let entry = sources.vacant_entry();
        let key = entry.key();
        self.poller.insert(raw, key)?;
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
        while self
            .timer_ops
            .push(TimerOp::Insert(when, id, waker.clone()))
            .is_err()
        {
            let mut timers = self.timers.lock().unwrap();
            self.process_timer_ops(&mut timers);
        }
        self.notify();
        id
    }

    pub fn remove_timer(&self, when: Instant, id: usize) {
        while self.timer_ops.push(TimerOp::Remove(when, id)).is_err() {
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
                Ok(TimerOp::Insert(when, id, waker)) => {
                    timers.insert((when, id), waker);
                }
                Ok(TimerOp::Remove(when, id)) => {
                    timers.remove(&(when, id));
                }
                Err(_) => break,
            }
        }
    }

    pub fn notify(&self) {
        self.poller.notify().expect("failed to notify reactor");
    }
}

/// A lock on the reactor.
pub(crate) struct ReactorLock<'a> {
    reactor: &'a Reactor,
    _guard: MutexGuard<'a, ()>,
}

impl<'a> ReactorLock<'a> {
    pub fn react(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.reactor.react(timeout)
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
    states: Mutex<States>,
}

#[derive(Default, Debug)]
struct States {
    /// Readiness state for the read and write directions.
    dirs: [State; 2],
    /// Whether an edge-triggered registration is already armed.
    #[cfg(feature = "edge-triggered")]
    armed: bool,
}

#[derive(Default, Debug)]
struct State {
    /// Tasks waiting for the next event.
    wakers: Vec<Waker>,
    /// Last reactor tick that delivered an event.
    tick: usize,
    /// Whether an event has been delivered and not consumed yet.
    ready: bool,
    /// Whether the source has been closed in this direction.
    closed: bool,
}

impl Source {
    pub async fn readable(&self) -> io::Result<()> {
        self.readable_tick().await.map(|_| ())
    }

    pub async fn writable(&self) -> io::Result<()> {
        self.writable_tick().await.map(|_| ())
    }

    /// Waits until the source is readable and returns the readiness tick.
    pub async fn readable_tick(&self) -> io::Result<usize> {
        poll_fn(|cx| self.poll_ready(READ, cx)).await
    }

    /// Waits until the source is writable and returns the readiness tick.
    pub async fn writable_tick(&self) -> io::Result<usize> {
        poll_fn(|cx| self.poll_ready(WRITE, cx)).await
    }

    pub fn poll_readable_tick(&self, cx: &Context) -> Poll<io::Result<usize>> {
        self.poll_ready(READ, cx)
    }

    pub fn poll_writable_tick(&self, cx: &Context) -> Poll<io::Result<usize>> {
        self.poll_ready(WRITE, cx)
    }

    /// Clears the readiness bit for `op` unless a newer event has arrived.
    ///
    /// The `tick` comparison is what makes this safe under edge-triggered
    /// polling: the caller clears the bit it consumed, and if a newer event has
    /// landed in the meantime the tick no longer matches and the fresh
    /// readiness survives. Without it a late clear would erase an edge that
    /// nobody has read yet, and the source would go quiet with data pending.
    ///
    /// `closed` is deliberately left alone: a finished direction stays finished.
    /// It does not make `poll_ready` report ready on its own, so clearing it
    /// would only lose the information that the direction is done.
    pub fn clear_ready(&self, op: usize, tick: usize) {
        let mut states = self.states.lock().unwrap();
        let state = &mut states.dirs[op];
        if state.tick == tick {
            state.ready = false;
        }
    }

    pub fn clear_readable(&self, tick: usize) {
        self.clear_ready(READ, tick);
    }

    pub fn clear_writable(&self, tick: usize) {
        self.clear_ready(WRITE, tick);
    }

    pub fn poll_ready(&self, op: usize, cx: &Context) -> Poll<io::Result<usize>> {
        let mut states = self.states.lock().unwrap();
        // Only `ready` short-circuits. `closed` must not: it is a latch that
        // nothing clears, so returning `Ready` for it would report a hung-up
        // direction as readable forever, even with nothing to read. The caller
        // would then loop on `recv` -> `WouldBlock` -> clear `ready` ->
        // `poll_ready` -> `Ready`, without ever issuing a syscall. See
        // `bugs/closed-latch-read-spin.md`.
        if states.dirs[op].ready {
            return Poll::Ready(Ok(states.dirs[op].tick));
        }

        #[cfg(not(feature = "edge-triggered"))]
        let was_empty = states.dirs[op].wakers.is_empty();

        if states.dirs[op]
            .wakers
            .iter()
            .all(|w| !w.will_wake(cx.waker()))
        {
            states.dirs[op].wakers.push(cx.waker().clone());
        }

        #[cfg(feature = "edge-triggered")]
        if !states.armed {
            // Edge-triggered registrations stay armed for the lifetime of the
            // source, so events keep flowing without further `epoll_ctl` calls.
            //
            // This is the only place the key is written to the kernel for an
            // edge-triggered source, which is why `Poller::insert` must get it
            // right up front: between `insert` and this call the descriptor is
            // addressed with whatever `insert` used, and a peer reset in that
            // window reports `HUP`/`ERR` against that key.
            Reactor::get().interest(self.raw, self.key, true, true)?;
            states.armed = true;
        }

        #[cfg(not(feature = "edge-triggered"))]
        if was_empty {
            // One-shot registrations must be re-armed on every wait.
            Reactor::get().interest(
                self.raw,
                self.key,
                !states.dirs[READ].wakers.is_empty(),
                !states.dirs[WRITE].wakers.is_empty(),
            )?;
        }

        Poll::Pending
    }
}
