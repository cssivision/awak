# Bug report: `closed` short-circuits `poll_ready`, so a hung-up socket spins

## Summary

`Source::poll_ready` returns `Ready` whenever `closed` is set, regardless of
whether anything is actually readable. `closed` is a latch that nothing clears.
A socket that has been hung up but still sits in a read loop therefore spins: the
I/O call returns `WouldBlock`, the caller clears `ready`, and `poll_ready` hands
back `Ready` again because `closed` is still true.

With `--features edge-triggered` this is reachable from ordinary workloads. A
proxy that accepts many short-lived connections degrades by an order of
magnitude. Ten consecutive runs per build, interleaved on the same machine:

| metric | one-shot (default) | edge-triggered | edge-triggered + fix |
| --- | --- | --- | --- |
| wall time, 6400 conns | 1.59 – 1.65 s (median 1.62) | **15.45 – 16.45 s (median 16.13)** | 1.59 – 1.68 s (median 1.61) |
| CPU seconds | 1.9 – 2.0 (median 2.0) | **47 – 154 (median 109)** | 1.9 – 2.1 (median 2.0) |
| CPU utilisation | 120 – 124 % | **304 – 973 %** | 120 – 125 % |

Reproduction rate: **10 of 10** for the unfixed build, 0 of 10 for both other
builds.

Two caveats on the numbers:

- The unfixed build **does not finish the 6400 connections** within the harness
  timeout; across the runs recorded it got through 4450 – 6092 of them before the
  client gave up, at 277 – 372 connections/s. So its wall time is a timeout
  rather than a completion time, and the real slowdown is worse than the ~10x the
  medians imply.
- CPU seconds vary widely (47 – 154) because the spin lasts until each connection
  is dropped, so the total depends on how many connections happen to be spinning
  at once. Wall time is the tighter metric.

## Environment

- Linux 6.1.11-2302.1.1 (x86_64), 32 cores
- awak 0.2.41 (`e42fbe0`, the merge of `feature/edge-triggered`)
- Built with `--features edge-triggered`
- Workload: 16 concurrent SOCKS5 clients, each opening 400 short-lived
  connections (connect → 8 bytes → echo → close) through a relay built on awak

## Root cause

### The fast path ignores what `closed` means

```rust
pub fn poll_ready(&self, op: usize, cx: &Context) -> Poll<io::Result<usize>> {
    let mut states = self.states.lock().unwrap();
    if states.dirs[op].closed || states.dirs[op].ready {
        return Poll::Ready(Ok(states.dirs[op].tick));
    }
    // ... register the waker, arm the registration if needed, return Pending
}
```

`ready` is a real readiness bit: something happened, and the caller is expected
to consume it. `closed` is different — it only says the direction is finished.
Treating them alike means a closed direction reports `Ready` forever, even when
the I/O call has nothing to report yet.

### Nothing clears `closed`

```rust
pub fn clear_ready(&self, op: usize, tick: usize) {
    let mut states = self.states.lock().unwrap();
    let state = &mut states.dirs[op];
    if state.tick == tick {
        state.ready = false;      // `closed` deliberately untouched
    }
}
```

The comment in the source explains the intent: a closed direction stays closed,
and the caller's loop is expected to exit on the EOF or error that follows. That
holds for a fully closed socket. It does not hold when the socket is hung up but
the read loop has not yet observed it through a normal return.

### The loop that results

`src/io/mod.rs`:

```rust
pub fn read_with_mut<R>(&mut self, op: impl FnMut(&mut T) -> io::Result<R>) -> io::Result<R> {
    let mut op = op;
    let mut tick = None;
    loop {
        match op(self.get_mut()) {                       // recv() -> WouldBlock
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if let Some(tick) = tick.take() {
                    self.source.clear_readable(tick);     // ready = false, closed stays
                }
            }
            res => return res,
        }
        tick = Some(self.source.readable_tick().await?);  // poll_ready -> Ready
    }
}
```

`poll_ready` returns `Ready` immediately, `recv` returns `WouldBlock` again, and
the loop never yields. No syscall is issued by the reactor during this, which is
why the threads show `state=R`, `wchan=0` in `/proc` — a pure userspace spin.

## Evidence

### Stack at the spin

`std::backtrace::force_capture()` inside `poll_ready`, taken after 3 000 000
calls in a run that reproduced the slowdown:

```text
0: <awak::io::reactor::Source>::poll_ready
1: awak::io::poll_future::<..., <...Async<std::net::tcp::TcpStream>>::read_with_mut<...>::{closure#0}>
2: <awak::net::tcp::stream::TcpStream as futures_io::AsyncRead>::poll_read
3: <awak::util::copy_buffer::CopyBuffer>::poll_copy::<&mut TcpStream, &mut CipherStream<...>>
4: <awak::util::idle_timeout::IdleTimeout<...> as core::future::Future>::poll
5: <relay>::stream_relay::{closure#0}::{closure#0}
```

### Call counts

Temporary counters in `poll_ready` and `read_with_mut`, in a single 15-second run
that completed only 2 798 connections (181 conn/s):

```text
[closed-hit] n=90000000 raw=13 op=0 ready=false
[closed-hit] n=92000000 raw=22 op=0 ready=false
[spin] read_with_mut 200k iterations, no progress
[spin] WouldBlock repeated; tick_present=true
```

- `poll_ready` reached **116 000 000 calls**, every sample taking the `closed`
  branch with `ready == false`. So the loop is not "readiness never clears"; it
  is "`closed` forces `Ready` regardless of readiness".
- `op` was `0` (`READ`) in every sample; the write direction is not involved.
- `tick_present=true` at the spin point confirms `clear_readable` did run — the
  bit really was cleared and `closed` alone kept the loop alive.

### What sets `closed`

Instrumenting the `react` branch that latches the flag shows every occurrence as
one of these:

```text
[closed-set] raw=10 readable=true read_closed=true writable=true
[closed-set] raw=14 readable=true read_closed=true writable=true
```

`readable` *and* `writable` *and* `read_closed` together is the signature of
`EPOLLHUP`, which sits in both `read_flags()` and `write_flags()`. Removing
`EPOLLRDHUP` alone from `read_flags()` changes nothing (I measured it), because
`EPOLLHUP` still sets the flag.

### Why only some workloads trigger it

| scenario | reproduces |
| --- | --- |
| one 256 MiB transfer | no — the connection never hangs up mid-life |
| 200 connections ping-ponging 30 messages | no — peers stay open while active |
| short-lived connections, 16 in parallel | **yes** |

The spin lasts from the moment the socket is hung up until the connection is
dropped, so short-lived connections feel it for their whole remaining lifetime
while long-lived ones only hit it at the end.

### Why a minimal awak-only echo test misses it

In a plain echo test the `recv` after a hangup returns `Ok(0)` (EOF) and
`read_with_mut` returns on the first iteration. The spin needs `recv` to return
`WouldBlock` while `closed` is already set, which happens when the hangup arrives
before any data is queued or after the buffer has been drained.

## Fix

Do not let `closed` alone short-circuit `poll_ready`. `closed` still stops the
direction from ever waiting again on its own, but only `ready` makes
`poll_ready` return immediately:

```rust
-        if states.dirs[op].closed || states.dirs[op].ready {
+        if states.dirs[op].ready {
             return Poll::Ready(Ok(states.dirs[op].tick));
         }
```

With that, a hung-up socket behaves the way it does in one-shot mode: the read
loop drains what is buffered, `recv` returns `0`, and the caller exits.

Measured effect, ten runs each, same machine, same workload — the same numbers
as the table at the top of the report:

| build | wall (6400 conns, median) | CPU s (median) | CPU % (range) |
| --- | --- | --- | --- |
| one-shot (default) | 1.62 s | 2.0 | 120 – 124 |
| edge-triggered, unfixed | **16.13 s** (timeout) | **109** | **304 – 973** |
| **edge-triggered, fixed** | **1.61 s** | **2.0** | 120 – 125 |

The full suite passes in both configurations after the change:

```text
cargo test                          -> 4+4+7+2+6+1 passed
cargo test --features edge-triggered -> 4+4+7+2+6+1 passed
cargo clippy --all-targets --features edge-triggered -- -D warnings -> clean
```

### Alternatives I considered and why they are worse

- **Drop `EPOLLRDHUP` from `read_flags()`.** Measured, no effect: the latch comes
  from `EPOLLHUP`, which has to stay in the mask for the reasons the existing
  comment gives. This also loses the distinction between "half closed, still
  readable" and "gone".
- **Drop `EPOLLHUP`/`EPOLLERR` from `read_flags()`.** The source comment argues
  for keeping them (matching `smol-rs/polling`): a hung-up socket does return
  immediately from `read`. Removing them would change behaviour for every caller,
  not just the spinning one.
- **Clear `closed` in `clear_ready`.** Would fix the spin but erase the fact that
  the direction is finished. `EPOLLHUP`-only sockets rely on that to stop being
  polled.

## A regression test worth adding

A future that loops 200 000 times without making progress is unambiguously
spinning, and counting iterations does not depend on timing:

```rust
// in Async::read_with_mut, inside the loop
if iterations == 200_000 {
    panic!("read_with_mut made no progress in 200k iterations");
}
```

The existing `tests/edge_triggered.rs::idle_source_does_not_spin` checks the
analogous case for an idle socket but never hangs one up mid-life, which is why
this path is uncovered. A test that accepts a connection, lets the peer call
`shutdown(SHUT_WR)`, and then keeps reading would exercise it — under one-shot it
passes today, under edge-triggered it spins.

## Impact

Any server built on this runtime that handles connections which the peer closes
while the local side still has a read pending is affected. Before the fix,
edge-triggered took 10x the wall time (and did not finish the workload) with
~50x the CPU on the test above; the feature also provided no benefit in that
scenario, so I would keep it off until this is resolved.
