fn main() {
    awak::block_on(async {
        // Spawn a future.
        let handle = awak::spawn(async {
            println!("Running task...");
            1 + 2
        });
        // Await its output.
        assert_eq!(handle.await, 3);
    });
}
