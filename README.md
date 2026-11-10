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
registrations (`EPOLLET` / `EV_CLEAR`). Descriptors stay registered and are only
re-armed (with `EPOLL_CTL_MOD` / `EV_ADD`) when a task polls for readiness again,
the same model Tokio uses. The runtime always attempts the I/O operation before
waiting for readiness, so readiness is never lost; code that waits with
`Async::readable`/`Async::writable` directly and then performs a single syscall
should still be prepared to retry on `WouldBlock`.

```toml
[dependencies]
awak = { version = "0.2", features = ["edge-triggered"] }
```
