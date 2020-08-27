pub mod reactor;
pub mod sys;

use std::future::Future;
use std::io::{self, Read, Write};
use std::mem::ManuallyDrop;
use std::net::{Shutdown, TcpStream};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use reactor::{Reactor, Source};

pub use futures::io::{copy, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use futures::io::{IoSlice, IoSliceMut};

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

impl<T> AsyncRead for &Async<T>
where
    for<'a> &'a T: Read,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.read_with(|io| (&*io).read(buf)))
    }

    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.read_with(|io| (&*io).read_vectored(bufs)))
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

impl<T> AsyncWrite for &Async<T>
where
    for<'a> &'a T: Write,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.write_with(|io| (&*io).write(buf)))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.write_with(|io| (&*io).write_vectored(bufs)))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        poll_future(cx, self.write_with(|io| (&*io).flush()))
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

pub(crate) struct Timer {
    deadline: Instant,
    key: usize,
    waker: Option<Waker>,
}

impl Timer {
    pub fn new(deadline: Instant) -> Timer {
        Timer {
            deadline,
            key: 0,
            waker: None,
        }
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn is_elapsed(&self) -> bool {
        self.deadline < Instant::now()
    }

    pub fn reset(&mut self, when: Instant) {
        if let Some(waker) = self.waker.as_ref() {
            Reactor::get().remove_timer(self.deadline, self.key);
            self.key = Reactor::get().insert_timer(when, waker);
        }

        self.deadline = when;
    }
}

impl Future for Timer {
    type Output = Instant;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.deadline <= Instant::now() {
            if self.key > 0 {
                Reactor::get().remove_timer(self.deadline, self.key);
            }

            Poll::Ready(self.deadline)
        } else {
            match self.waker {
                None => {
                    self.key = Reactor::get().insert_timer(self.deadline, cx.waker());
                    self.waker = Some(cx.waker().clone());
                }
                Some(ref w) if !w.will_wake(cx.waker()) => {
                    Reactor::get().remove_timer(self.deadline, self.key);

                    self.key = Reactor::get().insert_timer(self.deadline, cx.waker());
                    self.waker = Some(cx.waker().clone());
                }
                _ => {}
            }

            Poll::Pending
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some(_) = self.waker.take() {
            Reactor::get().remove_timer(self.deadline, self.key);
        }
    }
}
