use std::io;
use std::os::unix::net::{self, SocketAddr};
use std::path::Path;

use crate::io::Async;
use crate::net::unix::UnixStream;

#[derive(Debug)]
pub struct UnixListener {
    inner: Async<net::UnixListener>,
}

impl UnixListener {
    pub fn bind<P>(path: P) -> io::Result<UnixListener>
    where
        P: AsRef<Path>,
    {
        let listener = net::UnixListener::bind(path)?;
        let io = Async::new(listener)?;
        Ok(UnixListener { inner: io })
    }

    pub fn from_std(listener: net::UnixListener) -> io::Result<UnixListener> {
        Ok(UnixListener {
            inner: Async::new(listener)?,
        })
    }

    pub async fn accept(&self) -> io::Result<(UnixStream, SocketAddr)> {
        let (stream, addr) = self.inner.read_with(|io| io.accept()).await?;
        Ok((UnixStream::from_std(stream)?, addr))
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.get_ref().local_addr()
    }
}
