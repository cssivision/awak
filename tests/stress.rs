//! Runtime stress and regression tests.
//!
//! These run as part of `cargo test`. Each scenario is wrapped in a [`timeout`]
//! so that a lost task or a missed wake-up surfaces as a failing assertion
//! instead of a hung test binary. A hang is how the scheduler bug presented, so
//! the timeouts here are load-bearing.
//!
//! # What a failure means
//!
//! A failure is a bug report, but the tests are not equally sensitive, and it is
//! worth knowing which is which before reading a red run:
//!
//! * **Regression tests** (`spawn_await_waves`, `spawn_await_many_tasks`) are
//!   tuned to reproduce the lost-wake-up race in `Ticker::tick`. Both drive the
//!   executor through many park/unpark cycles, which is what the race needs.
//!   `spawn_await_waves` fails deterministically against the buggy scheduler.
//! * **Smoke tests** (`tcp_echo_*`, `timers_all_fire`,
//!   `connect_churn_does_not_hang`) exercise real I/O and timers end to end.
//!   They were measured *not* to reproduce the scheduling race on their own, so
//!   passing them says "the basics still work", not "the scheduler is correct".
//!
//! Every test owns the tasks it starts and drops them before returning. The
//! reactor and the global executor are process-wide singletons, so a test that
//! left an accept loop detached would keep registering I/O interest while the
//! *next* test runs and make failures depend on test order.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use async_task::Task;
use awak::block_on;
use awak::time::timeout;
use futures_util::{AsyncReadExt, AsyncWriteExt};

const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// An echo server that shuts down when the returned handle is dropped.
struct EchoServer {
    addr: SocketAddr,
    /// Dropping a `Task` cancels it, which ends the accept loop and releases the
    /// listener's registration in the reactor.
    handle: Option<Task<()>>,
}

impl EchoServer {
    fn start() -> EchoServer {
        let listener = block_on(awak::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = awak::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };
                awak::spawn(async move {
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if stream.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                })
                .detach();
            }
        });
        EchoServer {
            addr,
            handle: Some(handle),
        }
    }
}

impl Drop for EchoServer {
    fn drop(&mut self) {
        drop(self.handle.take());
    }
}

/// One client session doing `iters` request/response round trips.
async fn echo_session(addr: SocketAddr, iters: usize) {
    let mut client = awak::net::TcpStream::connect(addr).await.unwrap();
    let payload = vec![0x55u8; 1024];
    let mut rbuf = vec![0u8; payload.len()];
    for _ in 0..iters {
        client.write_all(&payload).await.unwrap();
        client.read_exact(&mut rbuf).await.unwrap();
    }
}

/// Smoke test: sequential echo sessions.
///
/// This used to be considered a regression test for the task-loss bug, but with
/// the leaked accept loop removed it no longer reproduces that race (measured:
/// 0/6 against the buggy scheduler). It still verifies that I/O, the reactor and
/// the executor cooperate end to end. For the actual regression coverage see
/// `spawn_await_waves`.
#[test]
fn tcp_echo_sessions_never_hang() {
    let server = EchoServer::start();
    block_on(timeout(TEST_TIMEOUT, async {
        for _ in 0..12 {
            echo_session(server.addr, 300).await;
        }
    }))
    .expect("scheduler stalled: an echo session never completed");
}

/// Smoke test: concurrent sessions.
///
/// Exercises the stealing path across several sessions, but was measured *not*
/// to reproduce the scheduling race (0/6 buggy, and raising the concurrency to
/// 32 or 64 did not help reliably). Treat a pass as "concurrent I/O works".
#[test]
fn tcp_echo_concurrent_sessions() {
    let server = EchoServer::start();
    block_on(timeout(TEST_TIMEOUT, async {
        let mut sessions = Vec::new();
        for _ in 0..4 {
            sessions.push(awak::spawn(echo_session(server.addr, 200)));
        }
        for session in sessions {
            session.await;
        }
    }))
    .expect("scheduler stalled with concurrent echo sessions");
}

/// Smoke test: many timers firing together.
///
/// Verified end to end that timers all fire; not tuned to reproduce the
/// scheduling race.
#[test]
fn timers_all_fire() {
    block_on(timeout(TEST_TIMEOUT, async {
        let mut tasks = Vec::new();
        for _ in 0..500 {
            tasks.push(awak::spawn(async {
                awak::time::delay_for(Duration::from_micros(200)).await;
            }));
        }
        for task in tasks {
            task.await;
        }
    }))
    .expect("timers did not all fire");
}

/// Regression test: spawning many tasks in one burst must not lose any.
///
/// A single burst is a weaker reproducer than [`spawn_await_waves`] (measured:
/// roughly 1 in 12 runs against the buggy scheduler, versus deterministic for
/// the wave test), but it covers the "queue fills up, tasks spill to the global
/// queue" path that the wave test does not.
#[test]
fn spawn_await_many_tasks() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const N: usize = 20_000;

    let ran = Arc::new(AtomicUsize::new(0));
    block_on(timeout(TEST_TIMEOUT, async {
        let mut tasks = Vec::new();
        for _ in 0..N {
            let ran = Arc::clone(&ran);
            tasks.push(awak::spawn(async move {
                ran.fetch_add(1, Ordering::Relaxed);
            }));
        }
        for task in tasks {
            task.await;
        }
    }))
    .expect("spawned tasks did not all complete");

    assert_eq!(ran.load(Ordering::Relaxed), N);
}

/// Regression test: the wake-up race in `Ticker::tick`.
///
/// `tick` used to search for work, then register as a sleeper. A task pushed
/// into that window found no sleeper to wake, and `sleep` then overwrote the
/// `notified` flag, so the task sat in the global queue with every worker
/// parked. The process hung instead of failing.
///
/// The race needs workers to repeatedly go to sleep *and* have work appear
/// right then, so this test drains the queue many times in small waves rather
/// than pushing one big burst. Against the buggy scheduler it failed in every
/// measured run; a single 20k burst (see [`spawn_await_many_tasks`]) only
/// reproduced it about once in twelve.
#[test]
fn spawn_await_waves() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const WAVES: usize = 100;
    const PER_WAVE: usize = 16;

    let ran = Arc::new(AtomicUsize::new(0));
    block_on(timeout(TEST_TIMEOUT, async {
        for _ in 0..WAVES {
            let mut tasks = Vec::new();
            for _ in 0..PER_WAVE {
                let ran = Arc::clone(&ran);
                tasks.push(awak::spawn(async move {
                    ran.fetch_add(1, Ordering::Relaxed);
                }));
            }
            for task in tasks {
                task.await;
            }
        }
    }))
    .expect("wave-based spawn stalled: a wave never completed");

    assert_eq!(ran.load(Ordering::Relaxed), WAVES * PER_WAVE);
}

/// Smoke test: repeated connect/disconnect cycles.
///
/// This shape used to strand the executor before the local-queue race was
/// fixed, but with its own listener it was measured *not* to reproduce the
/// scheduling race. It guards the source register/deregister path.
#[test]
fn connect_churn_does_not_hang() -> io::Result<()> {
    let listener = block_on(awak::net::TcpListener::bind("127.0.0.1:0"))?;
    let addr = listener.local_addr()?;

    block_on(timeout(TEST_TIMEOUT, async {
        for _ in 0..200 {
            let stream = awak::net::TcpStream::connect(addr).await.unwrap();
            drop(stream);
            // Drain the connection so the listener's backlog cannot fill up and
            // leave `connect` blocked.
            let _ = timeout(Duration::from_millis(50), listener.accept()).await;
        }
    }))
    .expect("connection churn stalled");
    Ok(())
}
