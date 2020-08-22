use cfg_if::cfg_if;

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

cfg_if! {
    if #[cfg(any(target_os = "linux", target_os = "android"))] {
        mod epoll;
        pub use epoll::*;
    } else if #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
    ))] {
        mod kqueue;
        pub use kqueue::*;
    } else if #[cfg(target_os = "windows")] {
        mod wepoll;
        use wepoll as sys;
    } else {
        compile_error!("does not support this target OS");
    }
}
