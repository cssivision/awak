use std::convert::TryInto;
use std::io;
use std::os::unix::io::RawFd;
use std::ptr;
use std::time::Duration;

use super::Event;

const NOTIFY_KEY: usize = usize::MAX;

pub struct Reactor {
    epoll_fd: RawFd,
    event_fd: RawFd,
}

/// Epoll flags for all possible readability events.
fn read_flags() -> libc::c_int {
    libc::EPOLLIN | libc::EPOLLRDHUP | libc::EPOLLHUP | libc::EPOLLERR | libc::EPOLLPRI
}

/// Epoll flags for all possible writability events.
fn write_flags() -> libc::c_int {
    libc::EPOLLOUT | libc::EPOLLHUP | libc::EPOLLERR
}

impl Reactor {
    pub fn new() -> io::Result<Reactor> {
        let epoll_fd = syscall!(epoll_create(1024))?;
        let flags = syscall!(fcntl(epoll_fd, libc::F_GETFD))?;
        syscall!(fcntl(epoll_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC))?;

        let event_fd = syscall!(eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK))?;

        let reactor = Reactor { epoll_fd, event_fd };

        reactor.insert(event_fd)?;
        Ok(reactor)
    }

    pub fn interest(&self, fd: RawFd, key: usize, read: bool, write: bool) -> io::Result<()> {
        let mut flags = libc::EPOLLONESHOT;
        if read {
            flags |= read_flags();
        }
        if write {
            flags |= write_flags();
        }

        let mut ev = libc::epoll_event {
            events: flags as _,
            u64: key as u64,
        };

        syscall!(epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_MOD, fd, &mut ev))?;

        Ok(())
    }

    pub fn insert(&self, fd: RawFd) -> io::Result<()> {
        let flags = syscall!(fcntl(fd, libc::F_GETFD))?;
        syscall!(fcntl(fd, libc::F_SETFD, flags | libc::O_NONBLOCK))?;

        let mut ev = libc::epoll_event {
            events: 0,
            u64: 0u64,
        };

        syscall!(epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut ev))?;
        Ok(())
    }

    pub fn remove(&self, fd: RawFd) -> io::Result<()> {
        syscall!(epoll_ctl(
            self.epoll_fd,
            libc::EPOLL_CTL_DEL,
            fd,
            ptr::null_mut()
        ))?;

        Ok(())
    }

    pub fn wait(&self, events: &mut Events, timeout: Option<Duration>) -> io::Result<usize> {
        let timeout_ms = timeout
            .map(|t| {
                if t == Duration::from_millis(0) {
                    t
                } else {
                    // Non-zero duration must be at least 1ms.
                    t.max(Duration::from_millis(1))
                }
            })
            .and_then(|t| t.as_millis().try_into().ok())
            .unwrap_or(-1);

        let res = syscall!(epoll_wait(
            self.epoll_fd,
            events.list.as_mut_ptr() as *mut libc::epoll_event,
            events.list.len() as libc::c_int,
            timeout_ms as libc::c_int,
        ))?;

        events.len = res as usize;

        let mut buf = [0u8; 8];
        let _ = syscall!(read(
            self.event_fd,
            &mut buf[0] as *mut u8 as *mut libc::c_void,
            buf.len()
        ));
        self.interest(self.event_fd, NOTIFY_KEY, true, false)?;

        Ok(events.len)
    }

    pub fn notify(&self) -> io::Result<()> {
        let buf: [u8; 8] = 1u64.to_ne_bytes();
        let _ = syscall!(write(
            self.event_fd,
            &buf[0] as *const u8 as *const libc::c_void,
            buf.len()
        ));
        Ok(())
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        let _ = self.remove(self.event_fd);
        let _ = syscall!(close(self.event_fd));
        let _ = syscall!(close(self.epoll_fd));
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

    pub fn iter(&self) -> impl Iterator<Item = Event> + '_ {
        self.list[..self.len].iter().map(|ev| Event {
            key: ev.u64 as usize,
            readable: (ev.events as libc::c_int & read_flags()) != 0,
            writable: (ev.events as libc::c_int & write_flags()) != 0,
        })
    }
}
