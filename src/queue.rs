use std::collections::VecDeque;
use std::sync::RwLock;

#[derive(Debug)]
pub struct Queue<T> {
    inner: RwLock<VecDeque<T>>,
}

impl<T> Queue<T> {
    pub fn new() -> Queue<T> {
        Queue::with_capacity(0)
    }

    pub fn with_capacity(n: usize) -> Queue<T> {
        Queue {
            inner: RwLock::new(VecDeque::with_capacity(n)),
        }
    }

    pub fn pop(&self) -> Option<T> {
        self.inner.write().unwrap().pop_front()
    }

    pub fn push(&self, value: T) {
        self.inner.write().unwrap().push_back(value)
    }

    pub fn len(&self) -> usize {
        self.inner.read().unwrap().len()
    }

    pub fn capacity(&self) -> usize {
        self.inner.read().unwrap().capacity()
    }
}
