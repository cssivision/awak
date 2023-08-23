use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::sync::RwLock;

#[derive(Debug)]
pub struct Queue<T> {
    inner: RwLock<Inner<T>>,
    capacity: usize,
}

#[derive(Debug)]
struct Inner<T> {
    queue: VecDeque<T>,
    length: usize,
}

#[derive(Debug)]
pub struct ErrorFull<T> {
    inner: T,
}

impl<T> fmt::Display for ErrorFull<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "queue full")
    }
}

impl<T: std::fmt::Debug> Error for ErrorFull<T> {}

impl<T> ErrorFull<T> {
    pub fn into_inner(self) -> T {
        self.inner
    }
}

#[derive(Debug)]
pub struct ErrorEmpty;

impl fmt::Display for ErrorEmpty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "queue empty")
    }
}

impl Error for ErrorEmpty {}

impl<T> Queue<T> {
    pub fn new(n: usize) -> Queue<T> {
        Queue {
            inner: RwLock::new(Inner {
                queue: VecDeque::with_capacity(n),
                length: 0,
            }),
            capacity: n,
        }
    }

    pub fn pop(&self) -> Result<T, ErrorEmpty> {
        let mut inner = self.inner.write().unwrap();
        if inner.length == 0 {
            return Err(ErrorEmpty);
        }
        inner.length -= 1;
        inner.queue.pop_front().ok_or(ErrorEmpty)
    }

    pub fn push(&self, value: T) -> Result<(), ErrorFull<T>> {
        let mut inner = self.inner.write().unwrap();
        if self.capacity == inner.length {
            return Err(ErrorFull { inner: value });
        }
        inner.length += 1;
        inner.queue.push_back(value);
        Ok(())
    }

    pub fn len(&self) -> usize {
        let inner = self.inner.write().unwrap();
        inner.length
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}
