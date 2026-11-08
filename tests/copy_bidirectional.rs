use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use awak::block_on;
use awak::util::{
    copy_bidirectional, copy_bidirectional_with_buffer_size, CopyBidirectional,
    DEFAULT_COPY_BUFFER_SIZE,
};
use futures_io::{AsyncRead, AsyncWrite};

/// In-memory stand-in for a socket: reads out of `to_read`, appends writes to
/// `written`, and records the size of the buffer handed to `poll_read`.
struct MockIo {
    to_read: Vec<u8>,
    pos: usize,
    written: Vec<u8>,
    max_read_buf: usize,
}

impl MockIo {
    fn new(to_read: Vec<u8>) -> Self {
        MockIo {
            to_read,
            pos: 0,
            written: Vec::new(),
            max_read_buf: 0,
        }
    }

    fn remaining(&self) -> usize {
        self.to_read.len() - self.pos
    }
}

impl AsyncRead for MockIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.max_read_buf = self.max_read_buf.max(buf.len());

        let remaining = self.to_read.len() - self.pos;
        if remaining == 0 || buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let n = remaining.min(buf.len());
        buf[..n].copy_from_slice(&self.to_read[self.pos..self.pos + n]);
        self.pos += n;
        Poll::Ready(Ok(n))
    }
}

impl AsyncWrite for MockIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.written.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// `awaka_to_b` bytes move `a -> b` and `b_to_a` bytes move `b -> a`.
fn run_copy(copy: impl std::future::Future<Output = io::Result<(u64, u64)>>) -> (u64, u64) {
    block_on(copy).expect("copy failed")
}

#[test]
fn copy_bidirectional_uses_default_buffer_size() {
    let a_data = payload(5000);
    let b_data = payload(3);

    let mut a = MockIo::new(a_data.clone());
    let mut b = MockIo::new(b_data.clone());

    let counts = run_copy(copy_bidirectional(&mut a, &mut b));

    assert_eq!(counts, (a_data.len() as u64, b_data.len() as u64));
    assert_eq!(a.written, b_data);
    assert_eq!(b.written, a_data);
    assert_eq!(a.max_read_buf, DEFAULT_COPY_BUFFER_SIZE);
    assert_eq!(b.max_read_buf, DEFAULT_COPY_BUFFER_SIZE);
    assert_eq!(a.remaining(), 0);
    assert_eq!(b.remaining(), 0);
}

#[test]
fn copy_bidirectional_honours_explicit_buffer_size() {
    let a_data = payload(100_000);
    let b_data = payload(100_001);

    let mut a = MockIo::new(a_data.clone());
    let mut b = MockIo::new(b_data.clone());

    let counts = run_copy(copy_bidirectional_with_buffer_size(
        &mut a,
        &mut b,
        32 * 1024,
    ));

    assert_eq!(counts, (a_data.len() as u64, b_data.len() as u64));
    assert_eq!(a.written, b_data);
    assert_eq!(b.written, a_data);
    assert_eq!(a.max_read_buf, 32 * 1024);
    assert_eq!(b.max_read_buf, 32 * 1024);
}

#[test]
fn copy_bidirectional_builder_copies_all_data() {
    let a_data = payload(64 * 1024 + 7);
    let b_data = payload(0);

    let mut a = MockIo::new(a_data.clone());
    let mut b = MockIo::new(b_data);

    let counts = run_copy(CopyBidirectional::with_buffer_size(&mut a, &mut b, 4096));

    assert_eq!(counts, (a_data.len() as u64, 0));
    assert_eq!(b.written, a_data);
    assert_eq!(a.max_read_buf, 4096);
}

/// A zero-sized buffer must not be mistaken for EOF.
#[test]
fn copy_bidirectional_clamps_zero_buffer_size() {
    let a_data = payload(1000);
    let mut a = MockIo::new(a_data.clone());
    let mut b = MockIo::new(Vec::new());

    let counts = run_copy(copy_bidirectional_with_buffer_size(&mut a, &mut b, 0));

    assert_eq!(counts, (a_data.len() as u64, 0));
    assert_eq!(b.written, a_data);
    assert_eq!(a.max_read_buf, 1);
}
