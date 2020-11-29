use std::io;
use std::time::Duration;

use awak::io::AsyncReadExt;
use awak::net::TcpStream;
use awak::time::timeout;

fn main() -> io::Result<()> {
    awak::block_on(async {
        match timeout(Duration::from_secs(1), TcpStream::connect("127.0.0.1:8080")).await {
            Ok(stream) => match stream {
                Ok(mut stream) => {
                    let mut buf = vec![0; 10];
                    let n = stream.read_exact(&mut buf).await?;
                    println!("read {:?} bytes", n);
                }
                Err(e) => {
                    println!("connect err: {:?}", e);
                }
            },
            Err(e) => {
                println!("timeout err: {:?}", e);
            }
        }

        Ok(())
    })
}
