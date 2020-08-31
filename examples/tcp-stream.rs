use awak::io::AsyncReadExt;
use awak::net::TcpStream;

fn main() {
    awak::block_on(async {
        let mut stream = TcpStream::connect("127.0.0.1:8080").await.unwrap();

        loop {
            let mut buf = vec![0; 10];
            stream.read_exact(&mut buf).await.unwrap();
            println!("{:?}", buf);
            awak::time::delay_for(std::time::Duration::from_secs(1)).await;
        }
    });
}
