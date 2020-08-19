use std::io;
use std::net::{self, SocketAddr};
use std::sync::Arc;

use crate::io::reactor::Source;

use socket2::{Domain, Socket};

pub struct TcpStream {
    source: Arc<Source>,
    io: net::TcpStream,
}

impl TcpStream {
    pub async fn connect<A: Into<SocketAddr>>(addr: A) -> io::Result<TcpStream> {
        let addr = addr.into();

        let domain = if addr.is_ipv6() {
            Domain::ipv6()
        } else {
            Domain::ipv4()
        };
    }
}
