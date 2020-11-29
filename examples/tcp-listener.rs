use std::io;
use std::time::Duration;

use awak::io::AsyncWriteExt;
use awak::net::TcpListener;
use awak::time::delay_for;
use awak::StreamExt;

fn main() -> io::Result<()> {
    awak::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:8080").await?;
        println!("server start at 127.0.0.1:8080");

        loop {
            while let Some(stream) = listener.incoming().next().await {
                match stream {
                    Ok(mut stream) => {
                        let task = awak::spawn(async move {
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
                        });

                        task.detach();
                    }
                    Err(e) => {
                        println!("err: {}", e);
                    }
                }
            }
        }
    })
}
