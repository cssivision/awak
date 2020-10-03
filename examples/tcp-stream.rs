use std::time::Duration;

use awak::io::AsyncReadExt;
use awak::net::TcpStream;
use awak::time::delay_for;

fn main() {
    awak::block_on(async {
        let mut stream = TcpStream::connect("127.0.0.1:8080").await.unwrap();

        loop {
            let mut buf = vec![0; 10];
            let n = stream.read_exact(&mut buf).await.unwrap();
            println!("read {:?} bytes", n);

            delay_for(Duration::from_secs(1)).await;
        }
    });
}
