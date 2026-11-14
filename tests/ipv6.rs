use std::io;
use std::net::{Ipv6Addr, SocketAddr};

use awak::net::{TcpListener, TcpStream, UdpSocket};
use futures_util::{AsyncReadExt, AsyncWriteExt};

fn v6_loopback(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv6Addr::LOCALHOST, port))
}

#[test]
fn tcp_ipv6_echo() -> io::Result<()> {
    awak::block_on(async {
        let listener = TcpListener::bind(v6_loopback(0)).await?;
        let addr = listener.local_addr()?;
        assert!(addr.is_ipv6(), "listener bound to {}", addr);

        awak::spawn(async move {
            let (mut stream, peer) = listener.accept().await.unwrap();
            assert!(peer.is_ipv6());
            let mut buf = [0u8; 16];
            let n = stream.read(&mut buf).await.unwrap();
            stream.write_all(&buf[..n]).await.unwrap();
            stream.flush().await.unwrap();
        })
        .detach();

        let mut stream = TcpStream::connect(addr).await?;
        assert!(stream.local_addr()?.is_ipv6());
        assert!(stream.peer_addr()?.is_ipv6());
        stream.write_all(b"hello ipv6").await?;
        let mut buf = [0u8; 16];
        let n = stream.read(&mut buf).await?;
        assert_eq!(&buf[..n], b"hello ipv6");
        Ok(())
    })
}

#[test]
fn tcp_ipv6_connect_by_bracketed_string() -> io::Result<()> {
    awak::block_on(async {
        let listener = TcpListener::bind(v6_loopback(0)).await?;
        let port = listener.local_addr()?.port();
        awak::spawn(async move {
            let _ = listener.accept().await;
        })
        .detach();

        // Config files spell IPv6 literals with brackets; make sure the
        // `ToSocketAddrs` path used by `connect` accepts that form.
        let addr = format!("[::1]:{port}");
        let stream = TcpStream::connect(addr).await?;
        assert!(stream.peer_addr()?.is_ipv6());
        Ok(())
    })
}

#[test]
fn udp_ipv6_roundtrip() -> io::Result<()> {
    awak::block_on(async {
        let server = UdpSocket::bind(v6_loopback(0))?;
        let client = UdpSocket::bind(v6_loopback(0))?;
        let client_addr = client.local_addr()?;
        let server_addr = server.local_addr()?;
        assert!(server_addr.is_ipv6());

        client.send_to(b"ping", server_addr).await?;

        let mut buf = [0u8; 16];
        let (n, from) = server.recv_from(&mut buf).await?;
        assert!(from.is_ipv6());
        server.send_to(&buf[..n], client_addr).await?;

        let n = client.recv(&mut buf).await?;
        assert_eq!(&buf[..n], b"ping");
        Ok(())
    })
}
