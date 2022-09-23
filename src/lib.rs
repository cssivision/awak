#[macro_export]
macro_rules! pin_mut {
    ($($x:ident),* $(,)?) => { $(
        // Move the value to ensure that it is owned
        let mut $x = $x;
        // Shadow the original binding so that it can't be directly accessed
        // ever again.
        #[allow(unused_mut)]
        let mut $x = unsafe {
            std::pin::Pin::new_unchecked(&mut $x)
        };
    )* }
}

pub mod blocking;
pub mod executor;
pub mod io;
pub mod net;
mod parking;
mod queue;
pub mod time;
mod waker_fn;

use std::future::Future;
use std::thread;

pub use blocking::block_on;
pub use executor::Executor;

use async_task::Task;
use once_cell::sync::Lazy;

pub static EXECUTOR: Lazy<Executor> = Lazy::new(|| {
    for _ in 0..num_cpus::get().max(1) {
        thread::spawn(|| {
            let ticker = EXECUTOR.ticker();
            block_on(ticker.run());
        });
    }
    Executor::new()
});

pub fn spawn<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> Task<T> {
    EXECUTOR.spawn(future)
}
