use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{self, SocketAddr};
use std::path::Path;
use std::task::{Context, Poll};

use crate::io::Async;

#[derive(Debug)]
pub struct UnixDatagram {
    inner: Async<net::UnixDatagram>,
}

impl UnixDatagram {
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixDatagram> {
        Ok(UnixDatagram {
            inner: Async::new(net::UnixDatagram::bind(path)?)?,
        })
    }

    pub fn pair() -> io::Result<(UnixDatagram, UnixDatagram)> {
        let (sock1, sock2) = net::UnixDatagram::pair()?;
        Ok((
            UnixDatagram::from_std(sock1)?,
            UnixDatagram::from_std(sock2)?,
        ))
    }

    pub fn from_std(socket: net::UnixDatagram) -> io::Result<UnixDatagram> {
        Ok(UnixDatagram {
            inner: Async::new(socket)?,
        })
    }

    pub fn connect<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        self.inner.get_ref().connect(path)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.get_ref().local_addr()
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.inner.get_ref().peer_addr()
    }

    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read_with(|io| io.recv(buf)).await
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.inner.read_with(|io| io.recv_from(buf)).await
    }

    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write_with(|io| io.send(buf)).await
    }

    pub async fn send_to<P: AsRef<Path>>(&self, buf: &[u8], path: P) -> io::Result<usize> {
        self.inner
            .write_with(|io| io.send_to(buf, path.as_ref()))
            .await
    }

    pub fn poll_recv(&self, cx: &Context, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        self.inner.poll_read_with(cx, |io| io.recv(buf))
    }

    pub fn poll_recv_from(
        &self,
        cx: &Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr)>> {
        self.inner.poll_read_with(cx, |io| io.recv_from(buf))
    }

    pub fn poll_send(&self, cx: &Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.inner.poll_write_with(cx, |io| io.send(buf))
    }

    pub fn poll_send_to<P: AsRef<Path>>(
        &self,
        cx: &Context,
        buf: &[u8],
        path: P,
    ) -> Poll<io::Result<usize>> {
        self.inner
            .poll_write_with(cx, |io| io.send_to(buf, path.as_ref()))
    }
}

impl AsRawFd for UnixDatagram {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.get_ref().as_raw_fd()
    }
}
