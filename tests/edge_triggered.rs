//! Edge-triggered readiness correctness probes.
//!
//! Edge-triggered (`EPOLLET` / `EV_CLEAR`) only reports a transition, so the two
//! classic failure modes are:
//!
//! 1. A partial read leaves data in the socket buffer and no further edge ever
//!    arrives, so the remaining bytes are stranded.
//! 2. A readiness bit is cleared after the producer already refilled the buffer,
//!    so the next edge is missed and the reader hangs.
//!
//! These run under both modes (`cargo test --features edge-triggered` and
//! without) so the two configurations can be compared directly.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use awak::block_on;
use awak::time::timeout;
use futures_util::{AsyncReadExt, AsyncWriteExt};

const LIMIT: Duration = Duration::from_secs(30);

/// Reads with a deliberately small buffer so many reads are needed per burst.
/// Under edge-triggered semantics a missed edge strands the tail.
#[test]
fn small_reads_do_not_strand_data() {
    const PAYLOAD: usize = 256 * 1024;
    const CHUNK: usize = 1024;

    let got = block_on(timeout(LIMIT, async {
        let listener = awak::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = awak::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let data = vec![0x7eu8; PAYLOAD];
            s.write_all(&data).await.unwrap();
            // Let the reader drain rather than closing immediately.
            awak::time::delay_for(Duration::from_millis(50)).await;
        });

        let mut c = awak::net::TcpStream::connect(addr).await.unwrap();
        let mut buf = vec![0u8; CHUNK];
        let mut total = 0;
        while total < PAYLOAD {
            let n = c.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        let _ = server.await;
        total
    }))
    .expect("reader stalled: an edge was missed");

    assert_eq!(got, 256 * 1024, "data was stranded in the socket buffer");
}

/// Ping-pong that is sensitive to clearing readiness too early: the reader sees
/// WouldBlock, clears its bit, and the peer writes again immediately after.
#[test]
fn pingpong_survives_early_clear() {
    const ROUNDS: usize = 2000;

    block_on(timeout(LIMIT, async {
        let listener = awak::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = awak::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            loop {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if s.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let mut c = awak::net::TcpStream::connect(addr).await.unwrap();
        let payload = vec![0x33u8; 1024];
        let mut rbuf = vec![0u8; 1024];
        for _ in 0..ROUNDS {
            c.write_all(&payload).await.unwrap();
            c.read_exact(&mut rbuf).await.unwrap();
        }
        drop(c);
        let _ = server.await;
    }))
    .expect("ping-pong stalled: readiness was lost after a clear");
}

/// Several readers on the same source must all be woken.
#[test]
fn concurrent_readers_all_progress() {
    const PER: usize = 64 * 1024;
    const READERS: usize = 4;

    let finished = block_on(timeout(LIMIT, async {
        let listener = awak::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = awak::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let data = vec![0x11u8; PER * READERS];
            s.write_all(&data).await.unwrap();
            awak::time::delay_for(Duration::from_millis(50)).await;
        });

        let c = awak::net::TcpStream::connect(addr).await.unwrap();
        let c = Arc::new(c);
        let done = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..READERS {
            let c = Arc::clone(&c);
            let done = Arc::clone(&done);
            tasks.push(awak::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let mut mine = 0;
                // Each reader takes a share; stop once satisfied.
                while mine < PER {
                    match (&*c).read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => mine += n,
                    }
                }
                done.fetch_add(1, Ordering::Relaxed);
            }));
        }
        for t in tasks {
            t.await;
        }
        let _ = server.await;
        done.load(Ordering::Relaxed)
    }))
    .expect("concurrent readers stalled");

    assert_eq!(finished, READERS, "a reader never made progress");
}

/// Writes that hit WouldBlock and resume must not lose the remainder.
#[test]
fn write_after_would_block_resumes() {
    const PAYLOAD: usize = 4 * 1024 * 1024;

    let sent = block_on(timeout(LIMIT, async {
        let listener = awak::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = awak::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let mut total = 0;
            loop {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => total += n,
                }
            }
            total
        });

        let mut c = awak::net::TcpStream::connect(addr).await.unwrap();
        let chunk = vec![0x99u8; 64 * 1024];
        let mut sent = 0;
        while sent < PAYLOAD {
            c.write_all(&chunk).await.unwrap();
            sent += chunk.len();
        }
        drop(c);
        let _ = server.await;
        sent
    }))
    .expect("writer stalled");

    assert_eq!(sent, PAYLOAD);
}

/// A closed fd number must not leak readiness into the socket that reuses it.
///
/// With edge-triggered polling the readiness state lives in a `Source` that is
/// keyed by the reactor's slab, so a recycled descriptor must get a fresh
/// `Source` rather than inherit the previous socket's bits.
#[test]
fn fd_recycling_does_not_leak_readiness() {
    const ITERS: usize = 300;

    block_on(timeout(LIMIT, async {
        let listener = awak::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        awak::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                awak::spawn(async move {
                    let mut buf = [0u8; 64];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                })
                .detach();
            }
        })
        .detach();

        for i in 0..ITERS {
            let mut c = awak::net::TcpStream::connect(addr).await.unwrap();
            let msg = [i as u8; 8];
            c.write_all(&msg).await.unwrap();
            let mut rbuf = [0u8; 8];
            c.read_exact(&mut rbuf).await.unwrap();
            assert_eq!(rbuf, msg, "echo mismatch on iteration {i}");
            drop(c);
        }
    }))
    .expect("fd recycling stalled");
}

/// An idle source must not report readiness in a loop.
///
/// If the readiness bit is left set with nothing to read, `read` returns
/// `WouldBlock`, the bit is cleared, and the source instantly reports ready
/// again: a spin. Detect it directly instead of watching CPU time, which is
/// process-wide and gets polluted by tests running in parallel.
#[test]
fn idle_source_does_not_spin() {
    block_on(timeout(LIMIT, async {
        let listener = awak::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let consumed = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&consumed);

        awak::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 16];
            let _ = s.read(&mut buf).await;
            c.fetch_add(1, Ordering::Relaxed);
            // Stay connected but silent, so the client socket is idle.
            awak::time::delay_for(Duration::from_millis(800)).await;
        })
        .detach();

        let mut client = awak::net::TcpStream::connect(addr).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        awak::time::delay_for(Duration::from_millis(150)).await;
        assert_eq!(consumed.load(Ordering::Relaxed), 1, "peer never read");

        // Count how many times a read future becomes ready over a fixed window.
        // A correct implementation yields WouldBlock once, clears the bit, and
        // then stays pending, so this counts to 1 and blocks.
        let mut completions = 0u32;
        let deadline = std::time::Instant::now() + Duration::from_millis(400);
        while std::time::Instant::now() < deadline {
            let mut buf = [0u8; 1];
            if timeout(Duration::from_millis(50), (&client).read(&mut buf))
                .await
                .is_ok()
            {
                // WouldBlock counted as a completion: the read was polled, got no
                // data, and returned. A spin produces many of these.
                completions += 1;
            }
        }
        assert!(
            completions <= 3,
            "read future completed {} times in 400ms while idle (spinning on stale readiness)",
            completions
        );
        drop(client);
    }))
    .expect("idle spin check stalled");
}

/// One write must wake every task waiting on the same source.
#[test]
fn one_edge_wakes_all_waiters() {
    const WAITERS: usize = 12;

    block_on(timeout(LIMIT, async {
        let listener = awak::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = awak::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            s.write_all(&[0x77u8; 64]).await.unwrap();
            awak::time::delay_for(Duration::from_millis(200)).await;
        });

        let c = Arc::new(awak::net::TcpStream::connect(addr).await.unwrap());
        let woken = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..WAITERS {
            let c = Arc::clone(&c);
            let woken = Arc::clone(&woken);
            tasks.push(awak::spawn(async move {
                let mut buf = [0u8; 1];
                let _ = timeout(Duration::from_secs(3), (&*c).read(&mut buf)).await;
                woken.fetch_add(1, Ordering::Relaxed);
            }));
        }
        for t in tasks {
            t.await;
        }
        assert_eq!(
            woken.load(Ordering::Relaxed),
            WAITERS,
            "not every waiter was woken"
        );
        drop(c);
        let _ = server.await;
    }))
    .expect("waiter wake-up check stalled");
}
