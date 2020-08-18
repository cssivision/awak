#[macro_use]
mod pin;
mod async_io;
mod blocking;
mod executor;
mod parking;
mod task;
mod waker_fn;

use std::thread;

use async_io::reactor::Reactor;
pub use blocking::block_on;
pub use executor::Executor;

use once_cell::sync::Lazy;

pub static EXECUTOR: Lazy<Executor> = Lazy::new(|| {
    for _ in 0..num_cpus::get().max(1) {
        thread::spawn(|| {
            let (p, u) = parking::pair();
            let ticker = EXECUTOR.ticker(move || u.unpark());

            loop {
                for _ in 0..200 {
                    if !ticker.tick() {
                        p.park();
                        break;
                    }
                }

                Reactor::try_react();
            }
        });
    }

    Executor::new()
});
