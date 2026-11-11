//! A hung-up socket must not make the read loop spin.
//!
//! `Source::poll_ready` used to return `Ready` whenever `closed` was set, even
//! when `ready` was false. `closed` is a latch that nothing clears, so a socket
//! that had been hung up but still had a read pending reported ready forever:
//! `recv` returned `WouldBlock`, the caller cleared `ready`, and `poll_ready`
//! handed back `Ready` again because `closed` was still true. No syscall is
//! issued in that loop, so it is a pure userspace spin and the only outward sign
//! is CPU burn.
//!
//! The latch is reachable from ordinary workloads under `--features
//! edge-triggered`: a relay handling short-lived connections degrades by orders
//! of magnitude. Long-lived connections only hit it at the very end of their
//! life, and a plain echo test usually sees `recv` return `Ok(0)` on the first
//! iteration and exits before spinning -- which is why the existing tests missed
//! it.
//!
//! See `bugs/closed-latch-read-spin.md`.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use awak::block_on;
use awak::time::timeout;
use awak::util::copy_bidirectional;
use futures_util::{AsyncReadExt, AsyncWriteExt};

const LIMIT: Duration = Duration::from_secs(30);

/// Short-lived connections through a relay must sustain throughput.
///
/// Before the fix this workload spun: the relay's downstream socket was hung up
/// by the client while `copy_bidirectional` still had a read pending, and the
/// resulting loop burned CPU without moving bytes. Under one-shot it passed, so
/// the assertion is on elapsed time rather than on the spin directly -- a
/// spinning build needs orders of magnitude longer for the same work.
#[test]
fn short_lived_connections_do_not_spin() {
    const PER_CLIENT: usize = 200;
    const CLIENTS: usize = 8;

    let elapsed = block_on(timeout(LIMIT, async {
        let start = Instant::now();

        // Upstream echo that mirrors bytes and closes with the peer.
        let up = awak::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up.local_addr().unwrap();
        awak::spawn(async move {
            while let Ok((mut s, _)) = up.accept().await {
                awak::spawn(async move {
                    let mut b = [0u8; 64];
                    while let Ok(k) = s.read(&mut b).await {
                        if k == 0 || s.write_all(&b[..k]).await.is_err() {
                            break;
                        }
                    }
                })
                .detach();
            }
        })
        .detach();

        // Relay: accept, then shovel bytes both ways.
        let rl = awak::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = rl.local_addr().unwrap();
        awak::spawn(async move {
            while let Ok((mut down, _)) = rl.accept().await {
                awak::spawn(async move {
                    if let Ok(mut up) = awak::net::TcpStream::connect(up_addr).await {
                        let _ = copy_bidirectional(&mut down, &mut up).await;
                    }
                })
                .detach();
            }
        })
        .detach();

        let done = Arc::new(AtomicUsize::new(0));
        let mut clients = Vec::new();
        for _ in 0..CLIENTS {
            let done = Arc::clone(&done);
            clients.push(awak::spawn(async move {
                for _ in 0..PER_CLIENT {
                    let mut c = awak::net::TcpStream::connect(relay_addr).await.unwrap();
                    c.write_all(b"12345678").await.unwrap();
                    let mut b = [0u8; 8];
                    let _ = c.read_exact(&mut b).await;
                    // Dropping the client hangs up its side while the relay may
                    // still have a read pending: the exact window that spun.
                    drop(c);
                    done.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for c in clients {
            c.await;
        }
        assert_eq!(done.load(Ordering::Relaxed), PER_CLIENT * CLIENTS);
        start.elapsed()
    }))
    .expect("relay workload did not finish: the read loop is spinning");

    // A correct build does this in well under a second; a spinning one needs
    // orders of magnitude more. Keep the bound loose so slow CI does not fail.
    assert!(
        elapsed < Duration::from_secs(10),
        "{:?} for {} short connections suggests a spin",
        elapsed,
        PER_CLIENT * CLIENTS
    );
}

/// A socket the peer has hung up must not report readable indefinitely.
///
/// This drives the same condition directly: reset the peer while the reader is
/// parked, then check that the reader reaches a terminal state quickly instead
/// of looping.
#[test]
fn reset_peer_does_not_make_the_reader_loop() {
    block_on(timeout(LIMIT, async {
        let l = awak::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();

        let reader = awak::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            // Bound the loop so a spin becomes an assertion, not a hang.
            for _ in 0..200_000 {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => return "done",
                    Ok(_) => continue,
                }
            }
            "spun"
        });

        // Connect, let the reader park, then abort so the socket is hung up
        // with nothing buffered.
        let mut peer = std::net::TcpStream::connect(addr).unwrap();
        {
            use std::io::Write;
            let _ = peer.write_all(b"x");
        }
        awak::time::delay_for(Duration::from_millis(80)).await;
        {
            use std::os::unix::io::AsRawFd;
            let linger = libc::linger {
                l_onoff: 1,
                l_linger: 0,
            };
            unsafe {
                libc::setsockopt(
                    peer.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_LINGER,
                    &linger as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::linger>() as u32,
                );
            }
        }
        peer.shutdown(std::net::Shutdown::Both).ok();
        drop(peer);

        let outcome = reader.await;
        assert_eq!(outcome, "done", "reader looped after the peer hung up");
    }))
    .expect("reader never reached a terminal state");
}
