use std::io;
use std::net::{self, SocketAddr, ToSocketAddrs};

use crate::io::Async;

#[derive(Debug)]
pub struct UdpSocket {
    inner: Async<net::UdpSocket>,
}

impl UdpSocket {
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<UdpSocket> {
        let addrs = addr.to_socket_addrs()?;
        let mut last_err = None;

        for addr in addrs {
            match UdpSocket::bind_addr(addr).await {
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

    async fn bind_addr(addr: SocketAddr) -> io::Result<UdpSocket> {
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

    pub async fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
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

    pub async fn send(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write_with_mut(|io| io.send(buf)).await
    }

    pub async fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read_with_mut(|io| io.recv(buf)).await
    }

    pub async fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.inner.read_with_mut(|io| io.recv_from(buf)).await
    }

    pub async fn send_to<A: ToSocketAddrs>(&mut self, buf: &[u8], target: A) -> io::Result<usize> {
        let addrs = target.to_socket_addrs()?;

        for addr in addrs {
            return self.inner.write_with_mut(|io| io.send_to(buf, addr)).await;
        }

        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no addresses to send data to",
        ))
    }
}
