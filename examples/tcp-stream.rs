use std::io;
use std::time::Duration;

use awak::net::TcpStream;
use awak::time::delay_for;
use futures_util::AsyncReadExt;

fn main() -> io::Result<()> {
    awak::block_on(async {
        let mut stream = TcpStream::connect("127.0.0.1:8080").await?;
        loop {
            let mut buf = vec![0; 10];
            stream.read_exact(&mut buf).await?;
            println!("read bytes: {:?}", buf);
            delay_for(Duration::from_secs(1)).await;
        }
    })
}
