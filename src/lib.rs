#[macro_use]
mod pin;
mod blocking;
mod executor;
mod io;
mod parking;
mod task;
mod waker_fn;

use std::thread;

pub use blocking::block_on;
pub use executor::Executor;
use io::reactor::Reactor;

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
