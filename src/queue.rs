use std::collections::VecDeque;
use std::sync::Mutex;

#[derive(Debug)]
pub struct Queue<T> {
    inner: Mutex<VecDeque<T>>,
}

impl<T> Queue<T> {
    pub fn new() -> Queue<T> {
        Queue::with_capacity(0)
    }

    pub fn with_capacity(n: usize) -> Queue<T> {
        Queue {
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
