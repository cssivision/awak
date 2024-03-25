use std::sync::{Arc, Mutex};
use std::time::Duration;

use awak::time::delay_for;

fn main() {
    awak::block_on(async {
        chain(100);
        delay_for(Duration::from_secs(1)).await;
    });
}

fn chain(count: isize) {
    let sum = Arc::new(Mutex::new(0));
    for _ in 0..count {
        let sum = sum.clone();
        awak::spawn(async move {
            let result = *sum.lock().unwrap() + 1;
            let sum = std::mem::replace(&mut *sum.lock().unwrap(), result);
            println!("sum: {}", sum);
        })
        .detach();
    }
}
