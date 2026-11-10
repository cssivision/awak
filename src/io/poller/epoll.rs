use std::io;
use std::os::unix::io::RawFd;
use std::ptr;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use super::Event;

const NOTIFY_KEY: usize = usize::MAX;

pub struct Reactor {
    epoll_fd: RawFd,
    event_fd: RawFd,
    /// File descriptor for the timerfd that produces timeouts.
    timer_fd: RawFd,
    events: Mutex<Events>,
}

/// `timespec` value that equals zero.
const TS_ZERO: libc::timespec = libc::timespec {
    tv_sec: 0,
    tv_nsec: 0,
};

/// Epoll flags for all possible readability events.
fn read_flags() -> libc::c_int {
    libc::EPOLLIN | libc::EPOLLRDHUP | libc::EPOLLHUP | libc::EPOLLERR | libc::EPOLLPRI
}

/// Epoll flags for all possible writability events.
fn write_flags() -> libc::c_int {
    libc::EPOLLOUT | libc::EPOLLHUP | libc::EPOLLERR
}

/// How readiness is reported for descriptors registered by the reactor.
///
/// By default registrations are one-shot: the kernel reports an event once and
/// the reactor re-arms the descriptor when a task asks for readiness again.
/// With the `edge-triggered` feature the descriptor stays registered and the
/// kernel only reports transitions from "not ready" to "ready".
fn mode_flags() -> libc::c_int {
    if cfg!(feature = "edge-triggered") {
        libc::EPOLLET
    } else {
        libc::EPOLLONESHOT
    }
}

impl Reactor {
    pub fn new() -> io::Result<Reactor> {
        let epoll_fd = syscall!(epoll_create(1024))?;
        let flags = syscall!(fcntl(epoll_fd, libc::F_GETFD))?;
        syscall!(fcntl(epoll_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC))?;
        let event_fd = syscall!(eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK))?;
        let timer_fd = syscall!(timerfd_create(
            libc::CLOCK_MONOTONIC,
            libc::TFD_CLOEXEC | libc::TFD_NONBLOCK
        ))?;
        let reactor = Reactor {
            epoll_fd,
            event_fd,
            timer_fd,
            events: Mutex::new(Events::new()),
        };
        reactor.insert(event_fd)?;
        reactor.insert(timer_fd)?;
        reactor.interest(event_fd, NOTIFY_KEY, true, false)?;
        reactor.interest(timer_fd, NOTIFY_KEY, true, false)?;
        Ok(reactor)
    }

    pub fn interest(&self, fd: RawFd, key: usize, read: bool, write: bool) -> io::Result<()> {
        let mut flags = mode_flags();
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
        let flags = syscall!(fcntl(fd, libc::F_GETFL))?;
        syscall!(fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
        let mut ev = libc::epoll_event {
            events: mode_flags() as _,
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

    pub fn wait(&self, timeout: Option<Duration>) -> io::Result<EventsGuard<'_>> {
        let mut events = self.events.lock().unwrap();
        let new_val = libc::itimerspec {
            it_interval: TS_ZERO,
            it_value: match timeout {
                None => TS_ZERO,
                Some(t) => libc::timespec {
                    tv_sec: t.as_secs() as libc::time_t,
                    tv_nsec: t.subsec_nanos() as libc::c_long,
                },
            },
        };
        syscall!(timerfd_settime(self.timer_fd, 0, &new_val, ptr::null_mut()))?;
        #[cfg(not(feature = "edge-triggered"))]
        self.interest(self.timer_fd, NOTIFY_KEY, true, false)?;
        // Timeout in milliseconds for epoll.
        let timeout_ms = if timeout == Some(Duration::from_secs(0)) {
            // This is a non-blocking call - use zero as the timeout.
            0
        } else {
            // This is a blocking call - rely on timerfd to trigger the timeout.
            -1
        };
        let res = syscall!(epoll_wait(
            self.epoll_fd,
            events.list.as_mut_ptr(),
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
        // Drain the timer as well: edge-triggered readiness only fires when
        // the expiration counter goes from zero to non-zero again.
        #[cfg(feature = "edge-triggered")]
        let _ = syscall!(read(
            self.timer_fd,
            &mut buf[0] as *mut u8 as *mut libc::c_void,
            buf.len()
        ));
        #[cfg(not(feature = "edge-triggered"))]
        self.interest(self.event_fd, NOTIFY_KEY, true, false)?;
        Ok(EventsGuard { events })
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
        let _ = self.remove(self.timer_fd);
        let _ = syscall!(close(self.event_fd));
        let _ = syscall!(close(self.epoll_fd));
        let _ = syscall!(close(self.timer_fd));
    }
}

/// A borrowed, reusable list of reported I/O events.
pub struct EventsGuard<'a> {
    events: MutexGuard<'a, Events>,
}

impl EventsGuard<'_> {
    pub fn is_empty(&self) -> bool {
        // The notification event does not count as I/O activity.
        self.iter().next().is_none()
    }

    pub fn iter(&self) -> impl Iterator<Item = Event> + '_ {
        self.events.iter().filter(|ev| ev.key != NOTIFY_KEY)
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
