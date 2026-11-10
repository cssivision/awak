// The integer type behind the `EV_*` constants differs between platforms
// (`u16` on macOS, `u32` on some BSDs), so casts to `u16` are not always
// required and cannot be written portably without them.
#![allow(clippy::unnecessary_cast)]

use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::ptr;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use super::Event;

const NOTIFY_KEY: usize = usize::MAX;

/// How readiness is reported for descriptors registered by the reactor.
///
/// By default registrations are one-shot (`EV_ONESHOT`): the kernel reports an
/// event once and the reactor re-arms the descriptor when a task asks for
/// readiness again. With the `edge-triggered` feature the descriptor stays
/// registered (`EV_CLEAR`) and the kernel only reports state changes.
fn mode_flags() -> u16 {
    if cfg!(feature = "edge-triggered") {
        libc::EV_CLEAR as u16
    } else {
        libc::EV_ONESHOT as u16
    }
}

pub struct Reactor {
    kqueue_fd: RawFd,
    write_stream: UnixStream,
    read_stream: UnixStream,
    events: Mutex<Events>,
}

impl Reactor {
    pub fn new() -> io::Result<Reactor> {
        let kqueue_fd = syscall!(kqueue())?;
        syscall!(fcntl(kqueue_fd, libc::F_SETFD, libc::FD_CLOEXEC))?;
        let (read_stream, write_stream) = UnixStream::pair()?;
        read_stream.set_nonblocking(true)?;
        write_stream.set_nonblocking(true)?;
        let reactor = Reactor {
            kqueue_fd,
            write_stream,
            read_stream,
            events: Mutex::new(Events::new()),
        };
        reactor.interest(reactor.read_stream.as_raw_fd(), NOTIFY_KEY, true, false)?;
        Ok(reactor)
    }

    pub fn insert(&self, fd: RawFd) -> io::Result<()> {
        let flags = syscall!(fcntl(fd, libc::F_GETFL))?;
        syscall!(fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
        Ok(())
    }

    pub fn interest(
        &self,
        fd: RawFd,
        key: usize,
        readable: bool,
        writable: bool,
    ) -> io::Result<()> {
        let mut read_flags = mode_flags() | libc::EV_RECEIPT as u16;
        let mut write_flags = mode_flags() | libc::EV_RECEIPT as u16;
        if readable {
            read_flags |= libc::EV_ADD;
        } else {
            read_flags |= libc::EV_DELETE;
        }
        if writable {
            write_flags |= libc::EV_ADD;
        } else {
            write_flags |= libc::EV_DELETE;
        }
        let changelist = [
            libc::kevent {
                ident: fd as _,
                filter: libc::EVFILT_READ,
                flags: read_flags,
                fflags: 0,
                data: 0,
                udata: key as _,
            },
            libc::kevent {
                ident: fd as _,
                filter: libc::EVFILT_WRITE,
                flags: write_flags,
                fflags: 0,
                data: 0,
                udata: key as _,
            },
        ];
        let mut eventlist = changelist;
        syscall!(kevent(
            self.kqueue_fd,
            changelist.as_ptr(),
            changelist.len() as _,
            eventlist.as_mut_ptr(),
            eventlist.len() as _,
            ptr::null(),
        ))?;
        for ev in &eventlist {
            // Explanation for ignoring EPIPE: https://github.com/tokio-rs/mio/issues/582
            if (ev.flags & libc::EV_ERROR) != 0
                && ev.data != 0
                && ev.data != libc::ENOENT as _
                && ev.data != libc::EPIPE as _
            {
                return Err(io::Error::from_raw_os_error(ev.data as _));
            }
        }
        Ok(())
    }

    pub fn remove(&self, fd: RawFd) -> io::Result<()> {
        // A list of changes for kqueue.
        let changelist = [
            libc::kevent {
                ident: fd as _,
                filter: libc::EVFILT_READ,
                flags: libc::EV_DELETE | libc::EV_RECEIPT,
                fflags: 0,
                data: 0,
                udata: 0 as _,
            },
            libc::kevent {
                ident: fd as _,
                filter: libc::EVFILT_WRITE,
                flags: libc::EV_DELETE | libc::EV_RECEIPT,
                fflags: 0,
                data: 0,
                udata: 0 as _,
            },
        ];

        // Apply changes.
        let mut eventlist = changelist;
        syscall!(kevent(
            self.kqueue_fd,
            changelist.as_ptr(),
            changelist.len() as _,
            eventlist.as_mut_ptr(),
            eventlist.len() as _,
            ptr::null(),
        ))?;

        // Check for errors.
        for ev in &eventlist {
            if (ev.flags & libc::EV_ERROR) != 0 && ev.data != 0 && ev.data != libc::ENOENT as _ {
                return Err(io::Error::from_raw_os_error(ev.data as _));
            }
        }
        Ok(())
    }

    pub fn wait(&self, timeout: Option<Duration>) -> io::Result<EventsGuard<'_>> {
        let mut events = self.events.lock().unwrap();
        // Convert the `Duration` to `libc::timespec`.
        let timeout = timeout.map(|t| libc::timespec {
            tv_sec: t.as_secs() as libc::time_t,
            tv_nsec: t.subsec_nanos() as libc::c_long,
        });
        // Wait for I/O events.
        let changelist = [];
        let eventlist = &mut events.list;
        let res = syscall!(kevent(
            self.kqueue_fd,
            changelist.as_ptr(),
            changelist.len() as _,
            eventlist.as_mut_ptr(),
            eventlist.len() as _,
            match &timeout {
                None => ptr::null(),
                Some(t) => t,
            }
        ))?;
        events.len = res as usize;
        // Clear the notification (if received) and re-register interest in it.
        while (&self.read_stream).read(&mut [0; 64]).is_ok() {}
        #[cfg(not(feature = "edge-triggered"))]
        self.interest(self.read_stream.as_raw_fd(), NOTIFY_KEY, true, false)?;
        Ok(EventsGuard { events })
    }

    pub fn notify(&self) -> io::Result<()> {
        let _ = (&self.write_stream).write(&[1]);
        Ok(())
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

pub struct Events {
    list: Box<[libc::kevent]>,
    len: usize,
}

unsafe impl Send for Events {}

impl Events {
    /// Creates an empty list.
    pub fn new() -> Events {
        let ev = libc::kevent {
            ident: 0_usize,
            filter: 0,
            flags: 0,
            fflags: 0,
            data: 0,
            udata: 0 as _,
        };
        let list = vec![ev; 1000].into_boxed_slice();
        let len = 0;
        Events { list, len }
    }

    /// Iterates over I/O events.
    pub fn iter(&self) -> impl Iterator<Item = Event> + '_ {
        // On some platforms, closing the read end of a pipe wakes up writers, but the
        // event is reported as EVFILT_READ with the EV_EOF flag.
        //
        // https://github.com/golang/go/commit/23aad448b1e3f7c3b4ba2af90120bde91ac865b4
        self.list[..self.len].iter().map(|ev| Event {
            key: ev.udata as usize,
            readable: ev.filter == libc::EVFILT_READ,
            writable: ev.filter == libc::EVFILT_WRITE
                || (ev.filter == libc::EVFILT_READ && (ev.flags & libc::EV_EOF) != 0),
            read_closed: ev.filter == libc::EVFILT_READ && (ev.flags & libc::EV_EOF) != 0,
            write_closed: ev.filter == libc::EVFILT_WRITE && (ev.flags & libc::EV_EOF) != 0,
        })
    }
}
