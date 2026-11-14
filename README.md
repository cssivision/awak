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

## IPv6

TCP and UDP sockets use `std::net`'s `ToSocketAddrs` uniformly, so IPv6 works
the same way it does in `std`:

- `TcpListener::bind` and `TcpStream::connect` accept IPv6 literals, either as
  `SocketAddr` values or as bracketed strings such as `"[::1]:8080"`.
- `TcpStream::connect` tries every address returned by resolution, so a
  hostname with both `A` and `AAAA` records falls back to the other family if
  one is unreachable.
- Binding a listener to `[::]` uses the platform default for `IPV6_V6ONLY`
  (dual-stack on Linux and macOS, IPv6-only on Windows); bind `0.0.0.0` for an
  IPv4-only listener.

`tests/ipv6.rs` covers a TCP echo round trip on `::1`, connecting through a
bracketed address string, and a UDP round trip on `::1`.
