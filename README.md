[![Build](https://github.com/cssivision/awak/workflows/build/badge.svg)](
https://github.com/cssivision/awak/actions)
[![crate](https://img.shields.io/crates/v/awak.svg)](https://crates.io/crates/awak)
[![License](http://img.shields.io/badge/license-mit-blue.svg)](https://github.com/cssivision/awak/blob/master/LICENSE)

# awak
A small async runtime for Rust.

## Features

### `edge-triggered`

By default the reactor registers one-shot interest (`EPOLLONESHOT` on Linux,
`EV_ONESHOT` on macOS/BSD): the kernel reports each readiness event once and the
reactor re-arms the descriptor when a task polls for readiness again.

Enabling the `edge-triggered` feature switches every descriptor -- including
the reactor's notification and timer descriptors -- to persistent edge-triggered
registrations (`EPOLLET` / `EV_CLEAR`). A descriptor is armed once, when a task
first waits for it, and stays registered afterwards, so the reactor performs no
`epoll_ctl` / `kevent` calls for it in the steady state.

Readiness is tracked as a bit per direction, the same model Tokio uses: once an
event has been reported the bit stays set until an I/O operation returns
`WouldBlock`, at which point it is cleared and the task waits for the next edge.
The runtime always attempts the I/O operation before waiting for readiness, so
readiness is never lost. Code that waits with `Async::readable`/`Async::writable`
directly and then performs a single syscall should still be prepared to retry on
`WouldBlock`.

```toml
[dependencies]
awak = { version = "0.2", features = ["edge-triggered"] }
```
