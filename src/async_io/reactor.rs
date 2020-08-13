use std::io;
use std::os::unix::io::RawFd;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::task::{Poll, Waker};
use std::thread;

use super::{parking, sys};

use futures::future::poll_fn;
use once_cell::sync::Lazy;
use slab::Slab;

pub struct Reactor {
    parker_count: AtomicUsize,
    unparker: parking::Unparker,
    ticker: AtomicUsize,
    sys: sys::Reactor,
    sources: Mutex<Slab<Arc<Source>>>,
    events: Mutex<sys::Events>,
}

impl Reactor {
    pub fn get() -> &'static Reactor {
        static REACTOR: Lazy<Reactor> = Lazy::new(|| {
            let (parker, unparker) = parking::pair();

            thread::spawn(move || {
                let reactor = Reactor::get();

                loop {
                    let tick = reactor.ticker.load(Ordering::SeqCst);

                    if reactor.parker_count.load(Ordering::SeqCst) > 0 {}
                }
            });

            Reactor {
                parker_count: AtomicUsize::new(0),
                unparker,
                ticker: AtomicUsize::new(0),
                sys: sys::Reactor::new().expect("init reactor fail"),
                sources: Mutex::new(Slab::new()),
                events: Mutex::new(sys::Events::new()),
            }
        });

        &REACTOR
    }

    pub fn increment_parkers(&self) {
        self.parker_count.fetch_add(1, Ordering::SeqCst);
    }

    pub fn decrement_parkers(&self) {
        self.parker_count.fetch_sub(1, Ordering::SeqCst);
        self.unparker.unpark();
    }

    fn interest(&self, raw: RawFd, key: usize, read: bool, write: bool) -> io::Result<()> {
        self.sys.interest(raw, key, read, write)
    }

    pub fn insert_io(&self, raw: RawFd) -> io::Result<Arc<Source>> {
        self.sys.insert(raw)?;

        let mut sources = self.sources.lock().unwrap();
        let entry = sources.vacant_entry();
        let key = entry.key();

        let source = Arc::new(Source {
            raw,
            key,
            wakers: Mutex::new(Wakers {
                readers: Vec::new(),
                writers: Vec::new(),
                tick_readable: 0,
                tick_writeable: 0,
            }),
        });

        entry.insert(source.clone());

        Ok(source)
    }

    pub fn remove_io(&self, source: &Source) -> io::Result<()> {
        let mut sources = self.sources.lock().unwrap();
        sources.remove(source.key);
        self.sys.remove(source.raw)
    }
}

/// A registered source of I/O events.
#[derive(Debug)]
pub struct Source {
    /// Raw file descriptor on Unix platforms.
    pub raw: RawFd,
    /// The key of this source obtained during registration.
    key: usize,
    /// Tasks interested in events on this source.
    wakers: Mutex<Wakers>,
}

#[derive(Debug)]
pub struct Wakers {
    /// Tasks waiting for the next readability event.
    readers: Vec<Waker>,
    /// Tasks waiting for the next writability event.
    writers: Vec<Waker>,

    tick_readable: usize,
    tick_writeable: usize,
}

impl Source {
    pub async fn readable(&self) -> io::Result<()> {
        let mut ticks = None;

        poll_fn(|cx| {
            let mut w = self.wakers.lock().unwrap();

            if let Some((a, b)) = ticks {
                if w.tick_readable != a && w.tick_readable != b {
                    return Poll::Ready(Ok(()));
                }
            }

            if w.readers.is_empty() {
                // no readers, register in reactor
                Reactor::get().interest(self.raw, self.key, true, !w.writers.is_empty())?;
            }

            if w.readers.iter().all(|w| !w.will_wake(cx.waker())) {
                w.readers.push(cx.waker().clone());
            }

            if ticks.is_none() {
                ticks = Some((
                    Reactor::get().ticker.load(Ordering::SeqCst),
                    w.tick_readable,
                ));
            }
            Poll::Pending
        })
        .await
    }

    pub async fn writeable(&self) -> io::Result<()> {
        let mut ticks = None;

        poll_fn(|cx| {
            let mut w = self.wakers.lock().unwrap();

            if let Some((a, b)) = ticks {
                if w.tick_writeable != a && w.tick_writeable != b {
                    return Poll::Ready(Ok(()));
                }
            }

            if w.writers.is_empty() {
                // no writer, register in reactor
                Reactor::get().interest(self.raw, self.key, !w.readers.is_empty(), true)?;
            }

            if w.writers.iter().all(|w| !w.will_wake(cx.waker())) {
                w.writers.push(cx.waker().clone());
            }

            if ticks.is_none() {
                ticks = Some((
                    Reactor::get().ticker.load(Ordering::SeqCst),
                    w.tick_writeable,
                ));
            }
            Poll::Pending
        })
        .await
    }
}
