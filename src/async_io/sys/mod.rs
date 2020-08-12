macro_rules! syscall {
    ($fn:ident $args:tt) => {{
        let res = unsafe { libc::$fn $args };
        if res == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

/// An event reported by epoll/kqueue/wepoll.
pub struct Event {
    /// Key passed when registering interest in the I/O handle.
    pub key: usize,
    /// Is the I/O handle readable?
    pub readable: bool,
    /// Is the I/O handle writable?
    pub writable: bool,
}

mod epoll;

pub use epoll::*;
