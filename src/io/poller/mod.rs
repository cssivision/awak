#![allow(clippy::non_send_fields_in_send_ty)]
use std::io;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cfg_if::cfg_if;

macro_rules! syscall {
    ($fn:ident $args:tt) => {{
        let res = unsafe { libc::$fn $args };
        if res == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

cfg_if! {
    if #[cfg(any(target_os = "linux", target_os = "android"))] {
        mod epoll;
        use epoll as sys;
    } else if #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
    ))] {
        mod kqueue;
        use kqueue as sys;
    } else {
        compile_error!("does not support this target OS");
    }
}

/// An event reported by epoll/kqueue/wepoll.
#[derive(Debug)]
pub struct Event {
    /// Key passed when registering interest in the I/O handle.
    pub key: usize,
    /// Is the I/O handle readable?
    pub readable: bool,
    /// Is the I/O handle writable?
    pub writable: bool,
}

pub struct Poller {
    notified: AtomicBool,
    reactor: sys::Reactor,
}

impl Poller {
    pub fn new() -> Poller {
        Poller {
            notified: AtomicBool::new(false),
            reactor: sys::Reactor::new().expect("init reactor fail"),
        }
    }

    pub fn wait(&self, timeout: Option<Duration>) -> io::Result<Vec<Event>> {
        let sys_events = self.reactor.wait(timeout)?;
        self.notified.swap(false, Ordering::SeqCst);
        let events = sys_events
            .iter()
            .filter(|ev| ev.key != usize::MAX)
            .collect();
        Ok(events)
    }

    pub fn notify(&self) -> io::Result<()> {
        if self
            .notified
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.reactor.notify()?;
        }
        Ok(())
    }

    pub fn remove(&self, fd: RawFd) -> io::Result<()> {
        self.reactor.remove(fd)
    }

    pub fn insert(&self, fd: RawFd) -> io::Result<()> {
        self.reactor.insert(fd)
    }

    pub fn interest(&self, fd: RawFd, key: usize, read: bool, write: bool) -> io::Result<()> {
        if key == usize::MAX {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the key is not allowed to be `usize::MAX`",
            ))
        } else {
            self.reactor.interest(fd, key, read, write)
        }
    }
}
