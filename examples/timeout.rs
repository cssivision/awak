use std::io;
use std::time::Duration;

use awak::net::TcpStream;
use awak::time::timeout;
use futures_util::AsyncReadExt;

fn main() -> io::Result<()> {
    awak::block_on(async {
        match timeout(Duration::from_secs(1), TcpStream::connect("127.0.0.1:8080")).await {
            Ok(stream) => match stream {
                Ok(mut stream) => {
                    let mut buf = vec![0; 10];
                    // `read_exact` fills the whole buffer or fails; it does not
                    // report how many bytes it read, so report the buffer size.
                    stream.read_exact(&mut buf).await?;
                    println!("read {:?} bytes: {:?}", buf.len(), buf);
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
