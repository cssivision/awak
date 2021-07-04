use std::collections::VecDeque;
use std::sync::Mutex;

#[derive(Debug)]
pub struct ConcurrentQueue<T> {
    inner: Mutex<VecDeque<T>>,
}

impl<T> ConcurrentQueue<T> {
    pub fn new() -> ConcurrentQueue<T> {
        ConcurrentQueue::with_capacity(0)
    }

    pub fn with_capacity(n: usize) -> ConcurrentQueue<T> {
        ConcurrentQueue {
            inner: Mutex::new(VecDeque::with_capacity(n)),
        }
    }

    pub fn pop(&self) -> Option<T> {
        self.inner.lock().unwrap().pop_front()
    }

    pub fn push(&self, value: T) {
        self.inner.lock().unwrap().push_back(value)
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn capacity(&self) -> usize {
        self.inner.lock().unwrap().capacity()
    }
}
