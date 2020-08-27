use std::io;
use std::net::{self, SocketAddr, ToSocketAddrs};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use crate::io::Async;

use futures::io::{AsyncRead, AsyncWrite, IoSlice, IoSliceMut};
use socket2::{Domain, Protocol, Socket, Type};

pub struct TcpStream {
    inner: Async<net::TcpStream>,
}

impl TcpStream {
    pub fn from_std(stream: net::TcpStream) -> io::Result<TcpStream> {
        Ok(TcpStream {
            inner: Async::new(stream)?,
        })
    }

    async fn connect_addr(addr: SocketAddr) -> io::Result<TcpStream> {
        let domain = if addr.is_ipv6() {
            Domain::ipv6()
        } else {
            Domain::ipv4()
        };

        let socket = Socket::new(domain, Type::stream(), Some(Protocol::tcp()))?;

        socket.set_nonblocking(true)?;

        socket.connect(&addr.into()).or_else(|err| {
            let in_progress = err.raw_os_error() == Some(libc::EINPROGRESS);

            if in_progress {
                Ok(())
            } else {
                Err(err)
            }
        })?;

        let stream = Async::new(socket.into_tcp_stream())?;

        stream.writable().await?;

        match stream.get_ref().take_error()? {
            None => Ok(TcpStream { inner: stream }),
            Some(err) => Err(err),
        }
    }

    pub async fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<TcpStream> {
        let addrs = addr.to_socket_addrs()?;

        let mut last_err = None;

        for addr in addrs {
            match TcpStream::connect_addr(addr).await {
                Ok(stream) => return Ok(stream),
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

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.get_ref().local_addr()
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.inner.get_ref().peer_addr()
    }

    pub fn shutdown(&self, how: net::Shutdown) -> std::io::Result<()> {
        self.inner.get_ref().shutdown(how)
    }

    pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read_with(|io| io.peek(buf)).await
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        self.inner.get_ref().nodelay()
    }

    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        self.inner.get_ref().set_nodelay(nodelay)
    }

    pub fn set_keepalive(&self, keepalive: Option<Duration>) -> io::Result<()> {
        self.as_socket().set_keepalive(keepalive)
    }

    pub fn keepalive(&self) -> io::Result<Option<Duration>> {
        self.as_socket().keepalive()
    }

    fn as_socket(&self) -> Socket {
        let raw_fd = self.inner.get_ref().as_raw_fd();
        unsafe { Socket::from_raw_fd(raw_fd) }
    }
}

impl AsyncRead for TcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }

    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_read_vectored(cx, bufs)
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}
