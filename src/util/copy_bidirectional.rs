use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use futures_io::{AsyncRead, AsyncWrite};

use super::copy_buffer::{CopyBuffer, DEFAULT_COPY_BUFFER_SIZE};

enum TransferState {
    Running(CopyBuffer),
    Close(u64),
    Done(u64),
}

/// A future copying data in both directions between `a` and `b`.
///
/// Each direction reads from its source into a buffer of
/// [`DEFAULT_COPY_BUFFER_SIZE`] bytes and writes that buffer out before reading
/// again. Use [`CopyBidirectional::with_buffer_size`] (or
/// [`copy_bidirectional_with_buffer_size`]) to pick another size.
///
/// ```
/// # use std::io;
/// # async fn example<A, B>(a: &mut A, b: &mut B) -> io::Result<()>
/// # where
/// #     A: futures_io::AsyncRead + futures_io::AsyncWrite + Unpin + ?Sized,
/// #     B: futures_io::AsyncRead + futures_io::AsyncWrite + Unpin + ?Sized,
/// # {
/// use awak::util::CopyBidirectional;
///
/// CopyBidirectional::with_buffer_size(a, b, 64 * 1024).await?;
/// # Ok(())
/// # }
/// ```
pub struct CopyBidirectional<'a, A: ?Sized, B: ?Sized> {
    a: &'a mut A,
    b: &'a mut B,
    a_to_b: TransferState,
    b_to_a: TransferState,
}

impl<'a, A, B> CopyBidirectional<'a, A, B>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    /// Copies with the default buffer size of [`DEFAULT_COPY_BUFFER_SIZE`].
    pub fn new(a: &'a mut A, b: &'a mut B) -> Self {
        Self::with_buffer_size(a, b, DEFAULT_COPY_BUFFER_SIZE)
    }

    /// Copies with a buffer of `buffer_size` bytes in each direction.
    ///
    /// Larger buffers mean fewer reads, writes and (for wrapped transports)
    /// fewer framing records per byte copied. A `buffer_size` of zero is
    /// clamped to one.
    pub fn with_buffer_size(a: &'a mut A, b: &'a mut B, buffer_size: usize) -> Self {
        CopyBidirectional {
            a,
            b,
            a_to_b: TransferState::Running(CopyBuffer::with_buffer_size(buffer_size)),
            b_to_a: TransferState::Running(CopyBuffer::with_buffer_size(buffer_size)),
        }
    }
}

fn transfer_one_direction<A, B>(
    cx: &mut Context<'_>,
    state: &mut TransferState,
    r: &mut A,
    mut w: &mut B,
) -> Poll<io::Result<u64>>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    loop {
        match state {
            TransferState::Running(buf) => {
                let count = ready!(buf.poll_copy(cx, r, w))?;
                *state = TransferState::Close(count);
            }
            TransferState::Close(count) => {
                ready!(Pin::new(&mut w).poll_close(cx))?;
                *state = TransferState::Done(*count);
            }
            TransferState::Done(count) => return Poll::Ready(Ok(*count)),
        }
    }
}

impl<'a, A, B> Future for CopyBidirectional<'a, A, B>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    type Output = io::Result<(u64, u64)>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Unpack self into mut refs to each field to avoid borrow check issues.
        let CopyBidirectional {
            a,
            b,
            a_to_b,
            b_to_a,
        } = &mut *self;

        let a_to_b = transfer_one_direction(cx, a_to_b, &mut *a, &mut *b)?;
        let b_to_a = transfer_one_direction(cx, b_to_a, &mut *b, &mut *a)?;

        // It is not a problem if ready! returns early because transfer_one_direction for the
        // other direction will keep returning TransferState::Done(count) in future calls to poll
        let a_to_b = ready!(a_to_b);
        let b_to_a = ready!(b_to_a);

        Poll::Ready(Ok((a_to_b, b_to_a)))
    }
}

/// Copies data in both directions between `a` and `b` using the default buffer
/// size of [`DEFAULT_COPY_BUFFER_SIZE`].
pub async fn copy_bidirectional<A, B>(a: &mut A, b: &mut B) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    CopyBidirectional::new(a, b).await
}

/// Same as [`copy_bidirectional`], but each direction uses a buffer of
/// `buffer_size` bytes.
pub async fn copy_bidirectional_with_buffer_size<A, B>(
    a: &mut A,
    b: &mut B,
    buffer_size: usize,
) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    CopyBidirectional::with_buffer_size(a, b, buffer_size).await
}
