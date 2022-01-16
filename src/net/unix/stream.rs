use std::io;
use std::net::Shutdown;
use std::os::unix::net::{self, SocketAddr};
use std::path::Path;

use socket2::{Domain, SockAddr, Socket, Type};

use crate::io::Async;

#[derive(Debug)]
pub struct UnixStream {
    inner: Async<net::UnixStream>,
}

impl UnixStream {
    pub async fn connect<P>(path: P) -> io::Result<UnixStream>
    where
        P: AsRef<Path>,
    {
        let addr = SockAddr::unix(path)?;
        let sock_type = Type::STREAM;
        #[cfg(target_os = "linux")]
        // If we can, set nonblocking at socket creation for unix
        let sock_type = sock_type.nonblocking();
        // This automatically handles cloexec on unix, no_inherit on windows and nosigpipe on macos
        let socket = Socket::new(Domain::UNIX, sock_type, None)?;
        #[cfg(not(target_os = "linux"))]
        // If the current platform doesn't support nonblocking at creation, enable it after creation
        socket.set_nonblocking(true)?;
        match socket.connect(&addr) {
            Ok(_) => {}
            #[cfg(unix)]
            Err(err) if err.raw_os_error() == Some(libc::EINPROGRESS) => {}
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
            Err(err) => return Err(err),
        }
        let stream = Async::new(net::UnixStream::from(socket))?;
        stream.writable().await?;
        stream.get_ref().peer_addr()?;
        Ok(UnixStream { inner: stream })
    }

    pub fn from_std(stream: net::UnixStream) -> io::Result<UnixStream> {
        Ok(UnixStream {
            inner: Async::new(stream)?,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.get_ref().local_addr()
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.inner.get_ref().peer_addr()
    }

    pub fn shutdown(&self, how: Shutdown) -> std::io::Result<()> {
        self.inner.get_ref().shutdown(how)
    }
}
