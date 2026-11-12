//! Parallelism and liveness stress for the executor wake-up chain.
//!
//! The executor releases at most one sleeper per `State::notify` and relies on
//! the woken ticker passing the baton on (`notify_next`). Two failure modes
//! follow from that, and each needs its own shape of load:
//!
//! * **serialization** -- a dropped baton leaves one thread running a burst that
//!   should have spread across cores. Results stay correct; the machine is just
//!   underused. Detected by comparing CPU time against wall time.
//! * **stranding** -- a missed wake-up leaves a queued task with every worker
//!   parked. Detected by the workload failing to finish inside a generous limit.
//!
//! Every scenario reports wall time, CPU time and the implied parallelism
//! (`cpu / wall`), which is the quantity that actually distinguishes the two.
//!
//! ```sh
//! cargo run --release --example wake_chain
//! ```
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{AsyncReadExt, AsyncWriteExt};

fn cpu_ms() -> u64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap();
    let f: Vec<&str> = s.rsplit(')').next().unwrap().split_whitespace().collect();
    (f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap()) * 10
}

fn burn(ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        std::hint::spin_loop();
    }
}

struct Row {
    name: &'static str,
    wall: u128,
    cpu: u64,
    note: &'static str,
}

fn measure<F: FnOnce()>(name: &'static str, note: &'static str, f: F) -> Row {
    let c0 = cpu_ms();
    let t = Instant::now();
    f();
    Row {
        name,
        wall: t.elapsed().as_millis(),
        cpu: cpu_ms() - c0,
        note,
    }
}

fn report(rows: &[Row], cores: usize) {
    println!(
        "{:<32} {:>9} {:>10} {:>7} {:>8}  expect",
        "scenario", "wall(ms)", "cpu(ms)", "par", "cpu%"
    );
    println!("{}", "-".repeat(88));
    for r in rows {
        let par = r.cpu as f64 / r.wall.max(1) as f64;
        println!(
            "{:<32} {:>9} {:>10} {:>7.1} {:>7.0}%  {}",
            r.name,
            r.wall,
            r.cpu,
            par,
            par * 100.0,
            r.note
        );
    }
    println!("\ncores = {cores}; `par` = cpu/wall, the effective thread count");
}

// --- scenarios ---------------------------------------------------------

/// Spawned from the `block_on` thread: every push goes to the global queue and
/// notifies on its own.
fn global_burst(n: usize, work_ms: u64) -> Row {
    measure("global burst", "par ~= n capped by cores", || {
        awak::block_on(async move {
            let done = Arc::new(AtomicUsize::new(0));
            let mut ts = Vec::new();
            for _ in 0..n {
                let d = Arc::clone(&done);
                ts.push(awak::spawn(async move {
                    burn(work_ms);
                    d.fetch_add(1, Ordering::Relaxed);
                }));
            }
            for t in ts {
                t.await;
            }
            assert_eq!(done.load(Ordering::Relaxed), n);
        })
    })
}

/// Spawned from inside a task: the tasks land in that worker's shard, which is
/// the case the baton has to cover.
fn nested_burst(n: usize, work_ms: u64) -> Row {
    measure("nested burst", "par ~= n capped by cores", || {
        awak::block_on(async move {
            let done = Arc::new(AtomicUsize::new(0));
            let d = Arc::clone(&done);
            let outer = awak::spawn(async move {
                let mut kids = Vec::new();
                for _ in 0..n {
                    let d = Arc::clone(&d);
                    kids.push(awak::spawn(async move {
                        burn(work_ms);
                        d.fetch_add(1, Ordering::Relaxed);
                    }));
                }
                for k in kids {
                    k.await;
                }
            });
            outer.await;
            assert_eq!(done.load(Ordering::Relaxed), n);
        })
    })
}

/// Two levels: the inner burst is spawned from shards that are already busy.
fn two_level(n: usize, work_ms: u64) -> Row {
    measure("two-level burst", "par ~= n capped by cores", || {
        awak::block_on(async move {
            let done = Arc::new(AtomicUsize::new(0));
            let d = Arc::clone(&done);
            let outer = awak::spawn(async move {
                let mut mids = Vec::new();
                for _ in 0..4 {
                    let d = Arc::clone(&d);
                    mids.push(awak::spawn(async move {
                        let mut kids = Vec::new();
                        for _ in 0..(n / 4) {
                            let d = Arc::clone(&d);
                            kids.push(awak::spawn(async move {
                                burn(work_ms);
                                d.fetch_add(1, Ordering::Relaxed);
                            }));
                        }
                        for k in kids {
                            k.await;
                        }
                    }));
                }
                for m in mids {
                    m.await;
                }
            });
            outer.await;
            assert_eq!(done.load(Ordering::Relaxed), n);
        })
    })
}

/// Long chain of dependents: each task spawns the next, so the executor has
/// exactly one runnable at a time. A missed wake-up strands the chain.
fn serial_chain(n: usize) -> Row {
    measure("serial chain", "must finish (liveness)", || {
        let finished = awak::block_on(awak::time::timeout(Duration::from_secs(20), async move {
            let mut task = awak::spawn(async move { 0usize });
            for _ in 0..n {
                let prev = task.await;
                let next = prev + 1;
                task = awak::spawn(async move { next });
            }
            task.await
        }));
        assert_eq!(
            finished.expect("serial chain stalled: a wake-up was lost"),
            n
        );
    })
}

/// Many short sleeps: exercises the chain on every timer wake-up.
fn timer_churn(n: usize, us: u64) -> Row {
    measure("timer churn", "must finish; par low (sleeps)", || {
        awak::block_on(awak::time::timeout(Duration::from_secs(20), async move {
            let done = Arc::new(AtomicUsize::new(0));
            let mut ts = Vec::new();
            for _ in 0..n {
                let d = Arc::clone(&done);
                ts.push(awak::spawn(async move {
                    awak::time::delay_for(Duration::from_micros(us)).await;
                    d.fetch_add(1, Ordering::Relaxed);
                }));
            }
            for t in ts {
                t.await;
            }
            assert_eq!(done.load(Ordering::Relaxed), n);
        }))
        .expect("timer churn stalled");
    })
}

/// I/O tasks across many connections: the reactor and the executor both have to
/// stay awake, which is what the spare ticker is for.
fn io_load(conns: usize, iters: usize) -> Row {
    measure("tcp ping-pong", "par ~= conns (capped)", || {
        awak::block_on(awak::time::timeout(Duration::from_secs(30), async move {
            let l = awak::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            awak::spawn(async move {
                while let Ok((mut s, _)) = l.accept().await {
                    awak::spawn(async move {
                        let mut b = vec![0u8; 4096];
                        loop {
                            match s.read(&mut b).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    if s.write_all(&b[..n]).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    })
                    .detach();
                }
            })
            .detach();

            let mut cs = Vec::new();
            for _ in 0..conns {
                cs.push(awak::spawn(async move {
                    let mut c = awak::net::TcpStream::connect(addr).await.unwrap();
                    let p = vec![0x5au8; 512];
                    let mut r = vec![0u8; 512];
                    for _ in 0..iters {
                        c.write_all(&p).await.unwrap();
                        c.read_exact(&mut r).await.unwrap();
                    }
                    drop(c);
                }));
            }
            for c in cs {
                c.await;
            }
        }))
        .expect("tcp ping-pong stalled");
    })
}

/// Mixed: CPU work plus I/O in flight, so the executor must both run tasks and
/// keep a ticker available to drive the reactor.
fn mixed(cpu_tasks: usize, conns: usize, iters: usize) -> Row {
    measure("mixed cpu + io", "must finish; par > 1", || {
        awak::block_on(awak::time::timeout(Duration::from_secs(30), async move {
            let l = awak::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            awak::spawn(async move {
                while let Ok((mut s, _)) = l.accept().await {
                    awak::spawn(async move {
                        let mut b = vec![0u8; 4096];
                        loop {
                            match s.read(&mut b).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    if s.write_all(&b[..n]).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    })
                    .detach();
                }
            })
            .detach();

            let mut all = Vec::new();
            for _ in 0..cpu_tasks {
                all.push(awak::spawn(async move { burn(60) }));
            }
            for _ in 0..conns {
                all.push(awak::spawn(async move {
                    let mut c = awak::net::TcpStream::connect(addr).await.unwrap();
                    let p = vec![0x11u8; 256];
                    let mut r = vec![0u8; 256];
                    for _ in 0..iters {
                        c.write_all(&p).await.unwrap();
                        c.read_exact(&mut r).await.unwrap();
                    }
                }));
            }
            for t in all {
                t.await;
            }
        }))
        .expect("mixed workload stalled");
    })
}

/// Repeated small bursts: the chain is restarted every time, so a baton that
/// only works on the first wake-up shows up here.
fn burst_train(bursts: usize, per_burst: usize, work_ms: u64) -> Row {
    measure("burst train", "must finish; par >= 4", || {
        awak::block_on(awak::time::timeout(Duration::from_secs(30), async move {
            for _ in 0..bursts {
                let done = Arc::new(AtomicUsize::new(0));
                let mut ts = Vec::new();
                for _ in 0..per_burst {
                    let d = Arc::clone(&done);
                    ts.push(awak::spawn(async move {
                        burn(work_ms);
                        d.fetch_add(1, Ordering::Relaxed);
                    }));
                }
                for t in ts {
                    t.await;
                }
                assert_eq!(done.load(Ordering::Relaxed), per_burst);
            }
        }))
        .expect("burst train stalled");
    })
}

fn main() {
    let cores = std::thread::available_parallelism().unwrap().get();
    let n = cores.min(32);

    println!("cores: {cores}\n");

    // Warm up: first-touch allocation and lazy thread startup.
    let _ = nested_burst(4, 5);

    let rows = vec![
        global_burst(n, 100),
        nested_burst(n, 100),
        two_level(n, 100),
        burst_train(20, n, 20),
        serial_chain(500),
        timer_churn(500, 200),
        io_load(16, 200),
        mixed(16, 8, 200),
    ];
    report(&rows, cores);
}
