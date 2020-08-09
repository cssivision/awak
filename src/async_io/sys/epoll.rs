use std::io;
use std::os::unix::io::RawFd;

pub struct Reactor {}

impl Reactor {
    pub fn new() -> io::Result<Reactor> {
        unimplemented!();
    }

    pub fn interest(&self, fd: RawFd, key: usize, read: bool, write: bool) -> io::Result<()> {
        unimplemented!();
    }

    pub fn insert(&self, fd: RawFd) -> io::Result<()> {
        unimplemented!();
    }

    pub fn remove(&self, fd: RawFd) -> io::Result<()> {
        unimplemented!();
    }
}

/// A list of reported I/O events.
pub struct Events {
    list: Box<[libc::epoll_event]>,
    len: usize,
}

impl Events {
    /// Creates an empty list.
    pub fn new() -> Events {
        let ev = libc::epoll_event { events: 0, u64: 0 };
        let list = vec![ev; 1000].into_boxed_slice();
        let len = 0;
        Events { list, len }
    }
}
