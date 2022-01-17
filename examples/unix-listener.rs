use std::io;
use std::time::Duration;

use awak::io::AsyncWriteExt;
use awak::net::UnixListener;
use awak::time::delay_for;

fn main() -> io::Result<()> {
    awak::block_on(async {
        let listener = UnixListener::bind("temp.sock")?;
        println!("server start at temp.sock");
        loop {
            let (mut stream, _) = listener.accept().await?;
            awak::spawn(async move {
                loop {
                    delay_for(Duration::from_secs(1)).await;
                    match stream.write_all(b"helloworld").await {
                        Ok(_) => {
                            println!("write bytes succ");
                        }
                        Err(e) => {
                            println!("write bytes err: {:?}", e);
                            break;
                        }
                    }
                }
            })
            .detach();
        }
    })
}
