mod poller;
pub mod reactor;

use std::future::Future;
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::pin::{pin, Pin};
use std::sync::Arc;
use std::task::{ready, Context, Poll, Waker};
use std::time::Instant;

use futures_io::{AsyncRead, AsyncWrite, IoSlice, IoSliceMut};

pub(crate) use reactor::{Reactor, Source};

#[derive(Debug)]
pub struct Async<T> {
    /// A source registered in the reactor.
    source: Arc<Source>,

    /// The inner I/O handle.
    io: Option<T>,
}

impl<T> Unpin for Async<T> {}

impl<T: AsRawFd> Async<T> {
    pub(crate) fn new(io: T) -> io::Result<Async<T>> {
        Ok(Async {
            source: Reactor::get().insert_io(io.as_raw_fd())?,
            io: Some(io),
        })
    }
}

impl<T> Async<T> {
    pub fn get_ref(&self) -> &T {
        self.io.as_ref().unwrap()
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.io.as_mut().unwrap()
    }

    pub fn into_inner(mut self) -> io::Result<T> {
        let io = self.io.take().unwrap();
        Reactor::get().remove_io(&self.source)?;
        Ok(io)
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
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                res => return res,
            }
            self.source.readable().await?;
        }
    }

    pub fn poll_read_with<R>(
        &self,
        cx: &Context,
        op: impl FnMut(&T) -> io::Result<R>,
    ) -> Poll<io::Result<R>> {
        let mut op = op;
        loop {
            match op(self.get_ref()) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                res => return Poll::Ready(res),
            }
            ready!(self.source.poll_readable(cx))?;
        }
    }

    pub async fn read_with_mut<R>(
        &mut self,
        op: impl FnMut(&mut T) -> io::Result<R>,
    ) -> io::Result<R> {
        let mut op = op;
        loop {
            match op(self.get_mut()) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                res => return res,
            }
            self.source.readable().await?;
        }
    }

    pub fn poll_read_with_mut<R>(
        &mut self,
        cx: &Context,
        op: impl FnMut(&mut T) -> io::Result<R>,
    ) -> Poll<io::Result<R>> {
        let mut op = op;
        loop {
            match op(self.get_mut()) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                res => return Poll::Ready(res),
            }
            ready!(self.source.poll_readable(cx))?;
        }
    }

    pub async fn write_with<R>(&self, op: impl FnMut(&T) -> io::Result<R>) -> io::Result<R> {
        let mut op = op;
        loop {
            match op(self.get_ref()) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                res => return res,
            }
            self.source.writable().await?;
        }
    }

    pub fn poll_write_with<R>(
        &self,
        cx: &Context,
        op: impl FnMut(&T) -> io::Result<R>,
    ) -> Poll<io::Result<R>> {
        let mut op = op;
        loop {
            match op(self.get_ref()) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                res => return Poll::Ready(res),
            }
            ready!(self.source.poll_writable(cx))?;
        }
    }

    pub async fn write_with_mut<R>(
        &mut self,
        op: impl FnMut(&mut T) -> io::Result<R>,
    ) -> io::Result<R> {
        let mut op = op;
        loop {
            match op(self.get_mut()) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                res => return res,
            }
            self.source.writable().await?;
        }
    }

    pub fn poll_write_with_mut<R>(
        &mut self,
        cx: &Context,
        op: impl FnMut(&mut T) -> io::Result<R>,
    ) -> Poll<io::Result<R>> {
        let mut op = op;
        loop {
            match op(self.get_mut()) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                res => return Poll::Ready(res),
            }
            ready!(self.source.poll_writable(cx))?;
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
        poll_future(cx, self.read_with(|mut io| io.read(buf)))
    }

    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.read_with(|mut io| io.read_vectored(bufs)))
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
        Poll::Ready(Ok(()))
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
        poll_future(cx, self.write_with(|mut io| io.write(buf)))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        poll_future(cx, self.write_with(|mut io| io.write_vectored(bufs)))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        poll_future(cx, self.write_with(|mut io| io.flush()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn poll_future<T>(cx: &mut Context<'_>, fut: impl Future<Output = T>) -> Poll<T> {
    pin!(fut).poll(cx)
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

    pub fn poll_timeout(&mut self, cx: &mut Context) -> Poll<Instant> {
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
        if self.waker.take().is_some() {
            Reactor::get().remove_timer(self.deadline, self.key);
        }
    }
}
