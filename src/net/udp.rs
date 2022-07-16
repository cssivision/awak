use std::io;
use std::net::{self, SocketAddr, ToSocketAddrs};
use std::task::{Context, Poll};

use crate::io::Async;

#[derive(Debug)]
pub struct UdpSocket {
    inner: Async<net::UdpSocket>,
}

impl UdpSocket {
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<UdpSocket> {
        let addrs = addr.to_socket_addrs()?;
        let mut last_err = None;
        for addr in addrs {
            match UdpSocket::bind_addr(addr) {
                Ok(socket) => return Ok(socket),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "could not resolve to any address",
            )
        }))
    }

    fn bind_addr(addr: SocketAddr) -> io::Result<UdpSocket> {
        Ok(UdpSocket {
            inner: Async::new(net::UdpSocket::bind(addr)?)?,
        })
    }

    pub fn from_std(socket: net::UdpSocket) -> io::Result<UdpSocket> {
        Ok(UdpSocket {
            inner: Async::new(socket)?,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.get_ref().local_addr()
    }

    pub fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        let addrs = addr.to_socket_addrs()?;
        let mut last_err = None;
        for addr in addrs {
            match self.inner.get_ref().connect(addr) {
                Ok(_) => return Ok(()),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "could not resolve to any address",
            )
        }))
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

    pub async fn send_to<A: Into<SocketAddr>>(&self, buf: &[u8], target: A) -> io::Result<usize> {
        let addr = target.into();
        self.inner.write_with(|io| io.send_to(buf, addr)).await
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

    pub fn poll_send_to<A: Into<SocketAddr>>(
        &self,
        cx: &Context,
        buf: &[u8],
        target: A,
    ) -> Poll<io::Result<usize>> {
        let addr = target.into();
        self.inner.poll_write_with(cx, |io| io.send_to(buf, addr))
    }
}
