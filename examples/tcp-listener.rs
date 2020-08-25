use awak::net::TcpListener;
use awak::{AsyncReadExt, AsyncWriteExt, StreamExt};

fn main() {
    awak::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
        println!("server start at 127.0.0.1:8080");

        loop {
            while let Some(stream) = listener.incoming().next().await {
                match stream {
                    Ok(mut stream) => {
                        awak::spawn(async move {
                            let mut output = [0u8; 11];
                            let _ = stream.read_exact(&mut output).await;
                            println!("output: {:?}", String::from_utf8(output.to_vec()));
                            let _ = stream.write_all(b"hello world").await;
                        });
                    }
                    Err(e) => {
                        println!("err: {}", e);
                    }
                }
            }
        }
    })
}
