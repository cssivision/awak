use std::io;
use std::os::unix::io::RawFd;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};
use std::task::{Poll, Waker};

use super::sys;

use futures::future::poll_fn;

pub struct Reactor {
    ticker: AtomicUsize,
    sys: sys::Reactor,
}

impl Reactor {
    pub fn get() -> Reactor {
        Reactor {
            ticker: AtomicUsize::new(0),
            sys: sys::Reactor::new().expect("init reactor fail"),
        }
    }

    fn interest(&self, raw: RawFd, key: usize, read: bool, write: bool) -> io::Result<()> {
        self.sys.interest(raw, key, read, write)
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
}
