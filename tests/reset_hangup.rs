//! A reset connection must not deliver its hangup to another descriptor.
//!
//! `insert` used to register a descriptor with `data.u64 = 0` and only learn its
//! slab key afterwards, so until the first `interest` the descriptor was
//! addressed as key 0. `EPOLLHUP`/`EPOLLERR` are not masked by the subscription
//! bits -- an abortive close (RST) reports them even for a descriptor registered
//! with nothing but `EPOLLET`, whereas an orderly FIN does not -- so the reset
//! arrives under key 0. `react` maps that to `readable` and sets both `ready`
//! and `closed` on whichever `Source` owns key 0, normally the listener.
//!
//! `closed` is sticky, so `poll_ready` then reports ready forever and `accept`
//! spins on `EAGAIN`.
//!
//! To keep the window open, the accepted sockets are held rather than dropped:
//! the placeholder key is only corrected when a task first waits on the source,
//! so an accepted-but-unpolled socket is still addressed as key 0.
use std::net::{Shutdown, TcpStream};
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

use awak::block_on;
use awak::time::timeout;

const LIMIT: Duration = Duration::from_secs(30);

/// Aborts the connection so the kernel sends RST rather than FIN.
fn reset(mut s: TcpStream) {
    use std::io::Write;
    let _ = s.write_all(b"x");
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            &linger as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::linger>() as u32,
        );
    }
    drop(s);
    let _ = Shutdown::Both;
}

/// One reset connection must not leave the listener permanently ready.
///
/// Accepted sockets are held, so the placeholder key stays in effect and the
/// reset is delivered to key 0. Afterwards the listener has no pending
/// connection, so `accept` must park rather than complete in a loop.
#[test]
fn reset_connections_do_not_wedge_the_listener() {
    block_on(timeout(LIMIT, async {
        let l = awak::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();

        // Hold every accepted stream: dropping them would deregister the source
        // and there would be nothing left to misattribute.
        let mut held = Vec::new();
        for _ in 0..6 {
            let peer = TcpStream::connect(addr).unwrap();
            if let Ok(Ok((s, _))) = timeout(Duration::from_millis(200), l.accept()).await {
                held.push(s);
            }
            reset(peer);
            awak::time::delay_for(Duration::from_millis(30)).await;
        }

        // Nothing is pending now. Count how often `accept` reports completion; a
        // sticky `closed`/`ready` on the listener shows up as a tight loop.
        let mut completions = 0u32;
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            if timeout(Duration::from_millis(40), l.accept()).await.is_ok() {
                completions += 1;
            }
        }
        assert!(
            completions <= 3,
            "accept completed {} times in 300ms with no connection pending: the \
             listener is stuck ready after a reset connection",
            completions
        );

        drop(held);
    }))
    .expect("listener wedge check stalled");
}

/// The listener must still serve a real connection after resets.
#[test]
fn listener_still_accepts_after_resets() {
    block_on(timeout(LIMIT, async {
        let l = awak::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();

        let mut held = Vec::new();
        for _ in 0..6 {
            let peer = TcpStream::connect(addr).unwrap();
            if let Ok(Ok((s, _))) = timeout(Duration::from_millis(200), l.accept()).await {
                held.push(s);
            }
            reset(peer);
            awak::time::delay_for(Duration::from_millis(20)).await;
        }

        // A real connection must be accepted, not lost in the churn.
        let mut client = awak::net::TcpStream::connect(addr).await.unwrap();
        match timeout(Duration::from_secs(5), l.accept()).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => panic!("accept failed after resets: {}", e),
            Err(_) => panic!("accept never completed after resets"),
        }
        use futures_util::AsyncWriteExt;
        let _ = client.write_all(b"ping").await;
        drop(client);
        drop(held);
    }))
    .expect("post-reset accept check stalled");
}
