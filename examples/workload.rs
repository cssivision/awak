//! Broad workload sweep, for comparing two builds across every path a runtime
//! exercises rather than one narrow case.
//!
//! Scenarios are grouped so that a regression shows up attributed to a path:
//!
//! * **executor** -- spawn/await at scale (`burst`, `chain`), which covers the
//!   local queue, the global overflow, stealing, and the park/unpark path.
//! * **timers** -- many live timers (`timers`), plus mixed timers-and-tasks
//!   (`timers_tasks`), which is what stresses the reactor's timeout
//!   computation rather than the run queue.
//! * **io** -- loopback echo at four shapes (`io_pp_small`, `io_pp_many`,
//!   `io_bulk`, `io_churn`), covering readiness transition frequency from
//!   hundreds of thousands down to a handful.
//! * **mixed** -- I/O and timers concurrently (`io_with_timers`), the realistic
//!   case where a reactor wake-up must serve both.
//!
//! Each scenario asserts the expected amount of work completed, so a build
//! cannot look fast by dropping tasks.
//!
//! ```sh
//! cargo run --release --example workload all 200000
//! cargo run --release --example workload io_pp_small
//! ```
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{AsyncReadExt, AsyncWriteExt};

fn cpu_ms() -> u64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap();
    let f: Vec<&str> = s.rsplit(')').next().unwrap().split_whitespace().collect();
    (f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap()) * 10
}

fn peak_rss_kb() -> u64 {
    let s = match std::fs::read_to_string("/proc/self/status") {
        Ok(s) => s,
        Err(_) => return 0,
    };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest
                .split_whitespace()
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
        }
    }
    0
}

fn emit(name: &str, ops: usize, bytes: usize, wall: Duration, cpu: u64) {
    println!(
        "RESULT name={} ops={} bytes={} wall_ms={} cpu_ms={} peak_rss_kb={}",
        name,
        ops,
        bytes,
        wall.as_millis(),
        cpu,
        peak_rss_kb()
    );
}

fn measure<F, G>(name: &str, ops: usize, f: F, bytes: G)
where
    F: FnOnce(),
    G: FnOnce() -> usize,
{
    let c0 = cpu_ms();
    let t = Instant::now();
    f();
    let wall = t.elapsed();
    let cpu = cpu_ms() - c0;
    emit(name, ops, bytes(), wall, cpu);
}

// --- executor paths ----------------------------------------------------

fn burst(n: usize) {
    measure(
        &format!("burst_{n}"),
        n,
        || {
            awak::block_on(async move {
                let done = Arc::new(AtomicUsize::new(0));
                let mut ts = Vec::with_capacity(n);
                for _ in 0..n {
                    let d = Arc::clone(&done);
                    ts.push(awak::spawn(async move {
                        d.fetch_add(1, Ordering::Relaxed);
                    }));
                }
                for x in ts {
                    x.await;
                }
                assert_eq!(done.load(Ordering::Relaxed), n, "lost tasks");
            });
        },
        || 0,
    );
}

fn chain(n: usize) {
    measure(
        &format!("chain_{n}"),
        n,
        || {
            let reached = awak::block_on(async move {
                let mut task = awak::spawn(async { 0usize });
                for _ in 0..n {
                    let prev = task.await;
                    let next = prev + 1;
                    task = awak::spawn(async move { next });
                }
                task.await
            });
            assert_eq!(reached, n, "chain stopped short");
        },
        || 0,
    );
}

// --- timer paths -------------------------------------------------------

fn timers(n: usize) {
    measure(
        &format!("timers_{n}"),
        n,
        || {
            awak::block_on(async move {
                let done = Arc::new(AtomicUsize::new(0));
                let mut ts = Vec::with_capacity(n);
                for i in 0..n {
                    let d = Arc::clone(&done);
                    let delay = Duration::from_micros(300 + (i % 7) as u64);
                    ts.push(awak::spawn(async move {
                        awak::time::delay_for(delay).await;
                        d.fetch_add(1, Ordering::Relaxed);
                    }));
                }
                for x in ts {
                    x.await;
                }
                assert_eq!(done.load(Ordering::Relaxed), n, "lost timers");
            });
        },
        || 0,
    );
}

/// Timers and CPU-bound tasks interleaved: the reactor must keep waking for
/// deadlines while workers are busy, which is where a coarse timeout shows up
/// as late timers rather than as idle CPU.
fn timers_tasks(n: usize) {
    measure(
        &format!("timers_tasks_{n}"),
        n,
        || {
            awak::block_on(async move {
                let fired = Arc::new(AtomicUsize::new(0));
                let mut ts = Vec::with_capacity(n);
                for i in 0..n {
                    let f = Arc::clone(&fired);
                    let delay = Duration::from_micros(200 + (i % 5) as u64);
                    ts.push(awak::spawn(async move {
                        awak::time::delay_for(delay).await;
                        f.fetch_add(1, Ordering::Relaxed);
                    }));
                }
                let mut workers = Vec::with_capacity(n);
                for _ in 0..n {
                    workers.push(awak::spawn(async {
                        for _ in 0..64 {
                            std::hint::black_box(0u64.wrapping_mul(3));
                        }
                    }));
                }
                for w in workers {
                    w.await;
                }
                for t in ts {
                    t.await;
                }
                assert_eq!(fired.load(Ordering::Relaxed), n, "lost timers");
            });
        },
        || 0,
    );
}

// --- io paths ----------------------------------------------------------

static SERVERS: Mutex<Vec<async_task::Task<()>>> = Mutex::new(Vec::new());

fn keep(h: async_task::Task<()>) {
    SERVERS.lock().unwrap().push(h);
}

struct Echo {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    turns: Arc<AtomicUsize>,
}

impl Echo {
    fn start() -> Echo {
        let stop = Arc::new(AtomicBool::new(false));
        let turns = Arc::new(AtomicUsize::new(0));
        let (s, t) = (Arc::clone(&stop), Arc::clone(&turns));
        let addr = awak::block_on(async move {
            let listener = awak::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            keep(awak::spawn(async move {
                while !s.load(Ordering::Relaxed) {
                    let (mut stream, _) = match listener.accept().await {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let t = Arc::clone(&t);
                    keep(awak::spawn(async move {
                        let mut buf = vec![0u8; 16 * 1024];
                        loop {
                            let n = match stream.read(&mut buf).await {
                                Ok(0) => break,
                                Ok(n) => n,
                                Err(_) => break,
                            };
                            if stream.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                            t.fetch_add(1, Ordering::Relaxed);
                        }
                    }));
                }
            }));
            addr
        });
        Echo { addr, stop, turns }
    }

    fn turns(&self) -> usize {
        self.turns.load(Ordering::Relaxed)
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn io_ping_pong(name: &str, conns: usize, rounds: usize, msg: usize) {
    let echo = Echo::start();
    let addr = echo.addr;
    measure(
        name,
        conns * rounds,
        || {
            awak::block_on(async move {
                let mut clients = Vec::with_capacity(conns);
                for _ in 0..conns {
                    clients.push(awak::spawn(async move {
                        let mut s = awak::net::TcpStream::connect(addr).await.unwrap();
                        s.set_nodelay(true).unwrap();
                        let req = vec![b'x'; msg];
                        let mut resp = vec![0u8; msg];
                        for _ in 0..rounds {
                            s.write_all(&req).await.unwrap();
                            s.read_exact(&mut resp).await.unwrap();
                        }
                    }));
                }
                for c in clients {
                    c.await;
                }
            });
        },
        || conns * rounds * msg * 2,
    );
    echo.stop();
    let _ = echo.turns();
}

fn io_churn(conns: usize, msg: usize, window: usize) {
    let echo = Echo::start();
    let addr = echo.addr;
    measure(
        "io_churn",
        conns,
        || {
            awak::block_on(async move {
                let mut done = 0;
                while done < conns {
                    let batch = window.min(conns - done);
                    let mut clients = Vec::with_capacity(batch);
                    for _ in 0..batch {
                        clients.push(awak::spawn(async move {
                            let mut s = awak::net::TcpStream::connect(addr).await.unwrap();
                            s.set_nodelay(true).unwrap();
                            let req = vec![b'x'; msg];
                            let mut resp = vec![0u8; msg];
                            s.write_all(&req).await.unwrap();
                            s.read_exact(&mut resp).await.unwrap();
                        }));
                    }
                    for c in clients {
                        c.await;
                    }
                    done += batch;
                }
            });
        },
        || conns * msg * 2,
    );
    echo.stop();
    let _ = echo.turns();
}

/// I/O traffic and timers at the same time: every reactor wake-up has to
/// consider both an I/O event and the nearest deadline.
fn io_with_timers(n: usize) {
    let echo = Echo::start();
    let addr = echo.addr;
    measure(
        &format!("io_with_timers_{n}"),
        n,
        || {
            awak::block_on(async move {
                let fired = Arc::new(AtomicUsize::new(0));

                let mut timers = Vec::with_capacity(n);
                for i in 0..n {
                    let f = Arc::clone(&fired);
                    let d = Duration::from_micros(200 + (i % 9) as u64);
                    timers.push(awak::spawn(async move {
                        awak::time::delay_for(d).await;
                        f.fetch_add(1, Ordering::Relaxed);
                    }));
                }

                let mut clients = Vec::with_capacity(8);
                for _ in 0..8 {
                    clients.push(awak::spawn(async move {
                        let mut s = awak::net::TcpStream::connect(addr).await.unwrap();
                        s.set_nodelay(true).unwrap();
                        let req = vec![b'x'; 256];
                        let mut resp = vec![0u8; 256];
                        for _ in 0..50 {
                            s.write_all(&req).await.unwrap();
                            s.read_exact(&mut resp).await.unwrap();
                        }
                    }));
                }

                for c in clients {
                    c.await;
                }
                for t in timers {
                    t.await;
                }
                assert_eq!(fired.load(Ordering::Relaxed), n, "lost timers");
            });
        },
        || n,
    );
    echo.stop();
    let _ = echo.turns();
}

fn main() {
    let what = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    let n: usize = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000);

    awak::block_on(async { awak::time::delay_for(Duration::from_millis(10)).await });

    let run = |name: &str| what == "all" || what == name;

    if run("burst") {
        burst(n);
    }
    if run("chain") {
        chain(n / 4);
    }
    if run("timers") {
        timers(n);
    }
    if run("timers_tasks") {
        timers_tasks(n / 4);
    }
    if run("io_pp_small") {
        io_ping_pong("io_pp_small", 64, 100, 64);
    }
    if run("io_pp_many") {
        io_ping_pong("io_pp_many", 32, 200, 1024);
    }
    if run("io_bulk") {
        io_ping_pong("io_bulk", 4, 20, 64 * 1024);
    }
    if run("io_churn") {
        io_churn(2000, 1024, 64);
    }
    if run("io_with_timers") {
        io_with_timers(n / 4);
    }
}
