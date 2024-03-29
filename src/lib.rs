pub mod blocking;
pub mod executor;
pub mod io;
pub mod net;
mod parking;
mod queue;
pub mod time;
pub mod util;
mod waker_fn;

use std::future::Future;
use std::panic::catch_unwind;
use std::sync::OnceLock;
use std::thread;

pub use blocking::block_on;
pub use executor::Executor;

use async_task::Task;

pub fn spawn<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> Task<T> {
    fn global() -> &'static Executor {
        static EXECUTOR: OnceLock<Executor> = OnceLock::new();

        EXECUTOR.get_or_init(|| {
            for _ in 0..thread::available_parallelism().unwrap().get().max(1) {
                thread::spawn(|| loop {
                    let _ = catch_unwind(|| {
                        block_on(global().ticker().run());
                    });
                });
            }
            Executor::new()
        })
    }
    global().spawn(future)
}
