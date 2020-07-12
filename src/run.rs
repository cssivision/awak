use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Context;
use std::thread;

use crossbeam::channel;
use futures::channel::oneshot;
use once_cell::sync::Lazy;

use crate::waker_fn::waker_fn;

/// A queue that holds scheduled tasks.
static QUEUE: Lazy<channel::Sender<Arc<Task>>> = Lazy::new(|| {
    // Create a queue.
    let (sender, receiver) = channel::unbounded::<Arc<Task>>();

    // Spawn executor threads the first time the queue is created.
    for _ in 0..num_cpus::get().max(1) {
        let receiver = receiver.clone();
        thread::spawn(move || receiver.iter().for_each(|task| task.run()));
    }

    sender
});

/// Awaits the output of a spawned future.
type JoinHandle<R> = Pin<Box<dyn Future<Output = R> + Send>>;

/// Spawns a future on the executor.
pub fn spawn<F, R>(future: F) -> JoinHandle<R>
where
    F: Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    // Wrap the future into one that sends the output into a channel.
    let (s, r) = oneshot::channel();
    let future = async move {
        let _ = s.send(future.await);
    };

    // Create a task and schedule it for execution.
    let task = Arc::new(Task {
        state: AtomicUsize::new(0),
        future: Mutex::new(Box::pin(future)),
    });
    QUEUE.send(task).unwrap();

    // Return a join handle that retrieves the output of the future.
    Box::pin(async { r.await.unwrap() })
}

/// A spawned future and its current state.
struct Task {
    state: AtomicUsize,
    future: Mutex<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl Task {
    /// Runs the task.
    fn run(self: Arc<Task>) {
        // Set if the task has been woken.
        const WOKEN: usize = 0b01;
        // Set if the task is currently running.
        const RUNNING: usize = 0b10;

        // Create a waker that schedules the task.
        let task = self.clone();
        let waker = waker_fn(move || {
            if task.state.fetch_or(WOKEN, Ordering::SeqCst) == 0 {
                QUEUE.send(task.clone()).unwrap();
            }
        });

        // The state is now "not woken" and "running".
        self.state.store(RUNNING, Ordering::SeqCst);

        // Poll the future.
        let cx = &mut Context::from_waker(&waker);
        let poll = self.future.try_lock().unwrap().as_mut().poll(cx);

        // If the future hasn't completed and was woken while running, then reschedule it.
        if poll.is_pending() {
            if self.state.fetch_and(!RUNNING, Ordering::SeqCst) == WOKEN | RUNNING {
                QUEUE.send(self).unwrap();
            }
        }
    }
}
