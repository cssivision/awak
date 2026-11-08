use std::io;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use futures_io::{AsyncRead, AsyncWrite};

/// The buffer size used by [`CopyBuffer::new`], and therefore by
/// [`copy_bidirectional`](super::copy_bidirectional).
pub const DEFAULT_COPY_BUFFER_SIZE: usize = 1024 * 2;

pub struct CopyBuffer {
    read_done: bool,
    pos: usize,
    cap: usize,
    amt: u64,
    buf: Box<[u8]>,
    need_flush: bool,
}

impl Default for CopyBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl CopyBuffer {
    pub fn new() -> CopyBuffer {
        Self::with_buffer_size(DEFAULT_COPY_BUFFER_SIZE)
    }

    /// Creates a `CopyBuffer` that reads at most `buffer_size` bytes per
    /// `poll_read` call.
    ///
    /// A `buffer_size` of zero is clamped to one: reading into an empty buffer
    /// would be reported as EOF by most readers, ending the copy before any
    /// data is moved.
    pub fn with_buffer_size(buffer_size: usize) -> CopyBuffer {
        CopyBuffer {
            read_done: false,
            amt: 0,
            pos: 0,
            cap: 0,
            buf: vec![0; buffer_size.max(1)].into_boxed_slice(),
            need_flush: false,
        }
    }

    pub fn poll_copy<'a, R, W>(
        &mut self,
        cx: &mut Context,
        mut reader: &'a mut R,
        mut writer: &'a mut W,
    ) -> Poll<io::Result<u64>>
    where
        R: AsyncRead + Unpin + ?Sized,
        W: AsyncWrite + Unpin + ?Sized,
    {
        loop {
            // If our buffer is empty, then we need to read some data to
            // continue.
            if self.pos == self.cap && !self.read_done {
                let n = match Pin::new(&mut reader).poll_read(cx, &mut self.buf) {
                    Poll::Ready(Ok(n)) => n,
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                    Poll::Pending => {
                        // Try flushing when the reader has no progress to avoid deadlock
                        // when the reader depends on buffered writer.
                        if self.need_flush {
                            ready!(Pin::new(&mut writer).poll_flush(cx))?;
                            self.need_flush = false;
                        }
                        return Poll::Pending;
                    }
                };
                if n == 0 {
                    self.read_done = true;
                } else {
                    self.pos = 0;
                    self.cap = n;
                }
            }

            // If our buffer has some data, let's write it out!
            while self.pos < self.cap {
                let i =
                    ready!(Pin::new(&mut writer).poll_write(cx, &self.buf[self.pos..self.cap]))?;
                if i == 0 {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write zero byte into writer",
                    )));
                } else {
                    self.pos += i;
                    self.amt += i as u64;
                    self.need_flush = true;
                }
            }

            // If we've written all the data and we've seen EOF, flush out the
            // data and finish the transfer.
            if self.pos == self.cap && self.read_done {
                ready!(Pin::new(&mut writer).poll_flush(cx))?;
                return Poll::Ready(Ok(self.amt));
            }
        }
    }
}
