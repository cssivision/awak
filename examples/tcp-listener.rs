use awak::io::AsyncWriteExt;
use awak::net::TcpListener;
use awak::StreamExt;

fn main() {
    awak::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
        println!("server start at 127.0.0.1:8080");

        loop {
            while let Some(stream) = listener.incoming().next().await {
                match stream {
                    Ok(mut stream) => {
                        awak::spawn(async move {
                            loop {
                                awak::time::delay_for(std::time::Duration::from_secs(1)).await;
                                let _ = stream.write_all(b"helloworld").await;
                            }
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
