use std::io;
use std::net::{self, SocketAddr, ToSocketAddrs};

use super::stream::TcpStream;
use crate::io::Async;

pub struct TcpListener {
    inner: Async<net::TcpListener>,
}

impl TcpListener {
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        let listener = net::TcpListener::bind(addr)?;

        Ok(TcpListener {
            inner: Async::new(listener)?,
        })
    }

    pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        let (stream, addr) = self.inner.read_with(|io| io.accept()).await?;

        Ok((TcpStream::new(stream)?, addr))
    }
}
