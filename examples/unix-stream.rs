use std::io;
use std::time::Duration;

use awak::io::AsyncReadExt;
use awak::net::UnixStream;
use awak::time::delay_for;

fn main() -> io::Result<()> {
    awak::block_on(async {
        let mut stream = UnixStream::connect("temp.sock").await?;
        loop {
            let mut buf = vec![0; 10];
            stream.read_exact(&mut buf).await?;
            println!("read bytes: {:?}", buf);
            delay_for(Duration::from_secs(1)).await;
        }
    })
}
