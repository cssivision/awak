use std::time::Duration;

use awak::time::delay_for;

fn main() {
    awak::block_on(async {
        delay_for(Duration::from_secs(1)).await;
    });
}
