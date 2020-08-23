pub mod reactor;
pub mod sys;

use std::io::{self, Read, Write};
use std::mem::ManuallyDrop;
use std::net::{Shutdown, TcpStream};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use reactor::{Reactor, Source};

use futures::future::Future;
use futures::io::{AsyncRead, AsyncWrite, IoSlice, IoSliceMut};

#[derive(Debug)]
pub struct Async<T> {
    /// A source registered in the reactor.
    source: Arc<Source>,

    /// The inner I/O handle.
    io: Box<T>,
}

impl<T: AsRawFd> Async<T> {
    pub(crate) fn new(io: T) -> io::Result<Async<T>> {
        Ok(Async {
            source: Reactor::get().insert_io(io.as_raw_fd())?,
            io: Box::new(io),
        })
    }
}

impl<T> Async<T> {
    pub fn get_ref(&self) -> &T {
        &self.io
    }

    pub fn get_mut(&mut self) -> &mut T {
        &mut self.io
    }

    pub async fn readable(&self) -> io::Result<()> {
        self.source.readable().await
    }

    pub async fn writable(&self) -> io::Result<()> {
        self.source.writable().await
    }

    pub async fn read_with<R>(&self, op: impl FnMut(&T) -> io::Result<R>) -> io::Result<R> {
        let mut op = op;
        loop {
            match op(self.get_ref()) {
                Err(err) => if err.kind() == io::ErrorKind::WouldBlock {},
                res => return res,
            }

            self.source.readable().await?;
        }
    }

    pub async fn read_with_mut<R>(
        &mut self,
        op: impl FnMut(&mut T) -> io::Result<R>,
    ) -> io::Result<R> {
        let mut op = op;
        loop {
            match op(self.get_mut()) {
                Err(err) => if err.kind() == io::ErrorKind::WouldBlock {},
                res => return res,
            }

            self.source.readable().await?;
        }
    }

    pub async fn write_with<R>(&self, op: impl FnMut(&T) -> io::Result<R>) -> io::Result<R> {
        let mut op = op;

        loop {
            match op(self.get_ref()) {
                Err(err) => if err.kind() == io::ErrorKind::WouldBlock {},
                res => return res,
            }

            self.source.writable().await?;
        }
    }

    pub async fn write_with_mut<R>(
        &mut self,
        op: impl FnMut(&mut T) -> io::Result<R>,
    ) -> io::Result<R> {
        let mut op = op;

        loop {
            match op(self.get_mut()) {
                Err(err) => if err.kind() == io::ErrorKind::WouldBlock {},
                res => return res,
            }

            self.source.writable().await?;
        }
    }
}

impl<T> Drop for Async<T> {
    fn drop(&mut self) {
        let _ = Reactor::get().remove_io(&self.source);
    }
}

impl<T: Read> AsyncRead for Async<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.read_with_mut(|io| io.read(buf)))
    }

    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.read_with_mut(|io| io.read_vectored(bufs)))
    }
}

impl<T: Write> AsyncWrite for Async<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.write_with_mut(|io| io.write(buf)))
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.write_with_mut(|io| io.write_vectored(bufs)))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        poll_future(cx, self.write_with_mut(|io| io.flush()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(shutdown_write(self.source.raw))
    }
}

fn shutdown_write(raw: RawFd) -> io::Result<()> {
    // This may not be a TCP stream, but that's okay. All we do is attempt a `shutdown()` on the
    // raw descriptor and ignore errors.
    let stream = unsafe { ManuallyDrop::new(TcpStream::from_raw_fd(raw)) };

    // If the socket is a TCP stream, the only actual error can be ENOTCONN.
    match stream.shutdown(Shutdown::Write) {
        Err(err) if err.kind() == io::ErrorKind::NotConnected => Err(err),
        _ => Ok(()),
    }
}

fn poll_future<T>(cx: &mut Context<'_>, fut: impl Future<Output = T>) -> Poll<T> {
    pin_mut!(fut);
    fut.poll(cx)
}
