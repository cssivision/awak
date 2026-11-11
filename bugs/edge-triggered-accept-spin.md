# Bug report: `EPOLL_CTL_ADD` registers every fd under key 0, so events reach the wrong `Source`

## Summary

`Reactor::insert` registered a new descriptor with `data.u64 = 0` instead of the
slab key it would be stored under. Key `0` is already owned by the first
`Source` in the slab — in a server that is the listening socket — so an event
the kernel delivers for a freshly accepted descriptor before it is first polled
is attributed to the listener.

The visible symptom is an `accept` future that spins. On Linux, `accept4` on a
descriptor with nothing pending returns `EAGAIN`; the runtime clears the read
readiness bit, but a misdelivered `EPOLLHUP` keeps setting it again, so the loop
never parks.

## Environment

- OS: Linux 6.1.11-2302.1.1 (x86_64)
- Branch: `feature/edge-triggered`, commit `c917e4d`
- Built with `--features edge-triggered`

## Root cause

`src/io/poller/epoll.rs` before the fix:

```rust
pub fn insert(&self, fd: RawFd) -> io::Result<()> {
    let flags = syscall!(fcntl(fd, libc::F_GETFL))?;
    syscall!(fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
    let mut ev = libc::epoll_event {
        events: mode_flags() as _,
        u64: 0u64,                    // <-- every fd is added as key 0
    };
    syscall!(epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut ev))?;
    Ok(())
}
```

The real key was allocated afterwards, in `src/io/reactor.rs`:

```rust
pub fn insert_io(&self, raw: RawFd) -> io::Result<Arc<Source>> {
    self.poller.insert(raw)?;                       // registers with key 0
    let mut sources = self.sources.lock().unwrap();
    let entry = sources.vacant_entry();
    let key = entry.key();                          // real key, too late
    let source = Arc::new(Source { raw, key, states: Default::default() });
    entry.insert(source.clone());
    Ok(source)
}
```

The reactor dispatches purely by key:

```rust
for ev in events.iter() {
    if let Some(source) = sources.get(ev.key) {
        // ... sets ready / closed on that source
    }
}
```

So an event for the new descriptor is applied to whichever `Source` owns key 0.

### Deterministic check

Two listeners are enough to show the wrong key, with no timing involved:

```console
$ strace -f -e trace=epoll_ctl ./keyprobe
epoll_ctl(5, EPOLL_CTL_ADD, 8, {events=EPOLLET, data={u64=0}}) = 0
epoll_ctl(5, EPOLL_CTL_ADD, 9, {events=EPOLLET, data={u64=0}}) = 0   <-- wrong
epoll_ctl(5, EPOLL_CTL_MOD, 8, {events=..., data={u64=0}}) = 0
epoll_ctl(5, EPOLL_CTL_MOD, 9, {events=..., data={u64=1}}) = 0       <-- corrected here
```

Descriptor 9's real slab key is 1, but it is added as 0 and only corrected when
a task first polls it.

### Why one-shot mode hides it

In the default configuration `poll_ready` re-arms when a direction gains its
first waiter:

```rust
#[cfg(not(feature = "edge-triggered"))]
if was_empty {
    Reactor::get().interest(
        self.raw,
        self.key,
        !states.dirs[READ].wakers.is_empty(),
        !states.dirs[WRITE].wakers.is_empty(),
    )?;
}
```

`interest` uses `EPOLL_CTL_MOD` with the correct key, so the placeholder is
replaced before the descriptor is waited on.

### Why edge-triggered does not

With the feature enabled the descriptor is armed exactly once:

```rust
#[cfg(feature = "edge-triggered")]
if !states.armed {
    Reactor::get().interest(self.raw, self.key, true, true)?;
    states.armed = true;
}
```

That call uses the right key, but it only runs when a task first waits for
readiness. Anything the kernel reports before then carries key 0.

## Observed sequence

`strace -f -tt -e trace=accept4,epoll_wait,epoll_ctl` on a spinning run. Key 0
is the listening socket; 9 and 10 are accepted connections.

```text
epoll_wait(5, [{events=EPOLLIN, data={u32=0, u64=0}}], 1000, -1) = 1
accept4(8, ...) = 9
epoll_ctl(5, EPOLL_CTL_ADD, 9,  {events=EPOLLET, data={u32=0, u64=0}}) = 0   key 0!
epoll_ctl(5, EPOLL_CTL_ADD, 10, {events=EPOLLET, data={u32=0, u64=0}}) = 0   key 0!
epoll_ctl(5, EPOLL_CTL_MOD, 10, {events=..., data={u32=2, u64=2}}) = 0

... 320 ms later, the connection is torn down ...

epoll_wait(5, [{events=EPOLLIN|EPOLLOUT|EPOLLHUP|EPOLLRDHUP, data={u32=2}}]) = 1
epoll_ctl(5, EPOLL_CTL_DEL, 10, NULL) = 0
epoll_wait(5, [{events=EPOLLHUP, data={u32=0, u64=0}}], 1000, -1) = 1        HUP -> key 0
epoll_ctl(5, EPOLL_CTL_DEL, 9, NULL) = 0
```

The hangup at key 0 belongs to fd 9, which was never re-registered under its
real key. `react` therefore sets `ready = true` and `closed = true` on the
**listener's** `Source`.

`poll_ready` short-circuits:

```rust
if states.dirs[op].closed || states.dirs[op].ready {
    return Poll::Ready(Ok(states.dirs[op].tick));
}
```

`accept` is then retried immediately after every `EAGAIN`, forever.

## Two things combine

1. `insert` registers under key 0, delivering another socket's hangup to the
   listener. This is the bug.
2. `closed` is a latch that is never reset, so a single misdelivered hangup
   turns the listener into a permanently-"ready" source.

Fixing (1) removes the wrong-key window and the spin with it, and that is what
was done. The latch is not itself a defect — see the follow-up section.

## Fix

Pass the key to `insert`, and allocate the slab key before registering:

```rust
// src/io/reactor.rs
pub fn insert_io(&self, raw: RawFd) -> io::Result<Arc<Source>> {
    let mut sources = self.sources.lock().unwrap();
    let entry = sources.vacant_entry();
    let key = entry.key();
    self.poller.insert(raw, key)?;      // register under the right key
    let source = Arc::new(Source { raw, key, states: Default::default() });
    entry.insert(source.clone());
    Ok(source)
}
```

```rust
// src/io/poller/epoll.rs
pub fn insert(&self, fd: RawFd, key: usize) -> io::Result<()> {
    // ...
    let mut ev = libc::epoll_event {
        events: mode_flags() as _,
        u64: key as u64,
    };
    // ...
}
```

The kqueue backend does not have this problem: its `insert` only sets
`O_NONBLOCK` and registers nothing, so the descriptor's first `kevent` is the
`EV_ADD` in `interest`, which already carries the correct `udata`. Its signature
takes the key anyway, to keep the two backends in step.

## Impact

- One core spins in `accept4` until a real connection arrives or the `Source` is
  dropped.
- When the race does not fire the feature does what it was meant to: `epoll_ctl`
  for a 32 MiB transfer dropped from 17–21 calls to a constant 5.

---

## Follow-up: mechanism confirmed

The missing piece was *why* a descriptor registered with nothing but `EPOLLET`
ever reports anything. It does, but only for abortive closes.

Measured directly against the kernel, with a peer that closes:

| registration | peer FIN | peer RST |
| --- | --- | --- |
| `EPOLLET` only | no event | **`HUP\|ERR` fires** |
| `EPOLLONESHOT` only | no event | **`HUP\|ERR` fires** |
| `EPOLLIN\|EPOLLET` | `IN` | `IN\|HUP\|ERR` |

`EPOLLHUP`/`EPOLLERR` are not masked by the subscription bits, so they are the
one kind of event a placeholder registration can still surface. An orderly FIN
does not, which is why earlier probes (which only closed cleanly) missed it.

### End-to-end trace

Accepting sockets and holding them unpolled, so the placeholder key is still in
effect when the peer resets.

Before the fix — every hangup lands on key 0, the listener:

```text
[EVT] key=0 rd=true wr=true rclosed=true wclosed=true -> fd=8    (x6)
```

After the fix — each connection's hangup lands on its own source:

```text
[EVT] key=1 ... -> fd=10
[EVT] key=2 ... -> fd=11
[EVT] key=3 ... -> fd=12
...
```

With key 0 correct, `react` no longer marks the listener ready/closed, and the
spin disappears: `accept4` goes from thousands of `EAGAIN` calls to a constant 8
across 10 runs.

### Why the placeholder was invisible in every other test

Two conditions have to hold at once:

1. The connection must be **accepted but not yet polled**, otherwise the first
   `poll_ready` calls `interest` and rewrites the key before anything arrives.
2. The peer must **abort with RST**, not close cleanly — a FIN produces no event
   for a registration without `EPOLLIN`.

That is why `tests/edge_triggered.rs` and every ad-hoc probe missed it, and why
holding the accepted sockets open is what finally reproduced it.

### Regression test

`tests/reset_hangup.rs::reset_connections_do_not_wedge_the_listener` keeps the
accepted sockets alive, resets the peers, then asserts that `accept` parks
instead of completing in a loop.

- Against the unfixed code: **detected 5/5** (each run wedges until the test's
  30 s timeout).
- Against the fixed code: passes in 0.50 s, 0/10 failures across ten runs.

It is a behavioural test rather than a `strace` assertion, so it runs in CI on
both platforms.

### Notes from comparing implementations

- `smol-rs/polling` and `mio` both require the key at registration time
  (`epoll::EventData::new_u64(ev.key as u64)` / `u64: usize::from(token)`).
  Neither has a placeholder, so neither can have this bug.
- `polling`'s `read_flags()` is `IN | HUP | ERR | PRI` and its `Events::iter`
  treats `HUP|ERR` as readable — the same choice awak makes. `mio` excludes
  `HUP` from `is_readable` and exposes `is_read_closed` separately.
- Sticky "closed" readiness is not a defect in itself: Tokio's
  `clear_readiness` deliberately leaves `READ_CLOSED`/`WRITE_CLOSED` set
  ("Closed states are excluded because they are final states"). The bug was
  never the latch; it was delivering another descriptor's hangup to the
  listener, which the latch then made permanent.
- `polling` has no per-source "already armed" flag: it does not wrap the poller,
  so arming is the caller's responsibility. awak's `State::armed` exists because
  `poll_ready` may be called repeatedly for one source by several tasks, and
  re-issuing `epoll_ctl(MOD)` each time would cancel the feature's benefit.
