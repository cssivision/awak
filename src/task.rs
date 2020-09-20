use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

#[derive(Debug)]
pub struct Task<T>(pub Option<async_task::JoinHandle<T, ()>>);

impl<T> Task<T> {
    pub async fn cancel(mut self) -> Option<T> {
        let handle = self.0.take().unwrap();
        handle.cancel();
        handle.await
    }
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.0.as_mut().unwrap()).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(output) => Poll::Ready(output.expect("task has failed")),
        }
    }
}
