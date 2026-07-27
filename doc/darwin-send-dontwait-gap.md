# `MSG_DONTWAIT` is not a bound: Darwin ignores it on `send`

**Status: closed.** Both write paths now own the socket's blocking mode, which
is what C does. The measurement below is kept because it is the evidence for
why a send flag cannot be what a deadline rests on.

## What broke

`epics-libcom-rs::runtime::blocking_io::write_frame_deadline` and
`asyn-rs::drivers::ip_port::write_with_retry` both bounded a frame write with
the same two-part contract:

1. `poll(POLLOUT)` against the caller's deadline, and
2. a send that cannot park, via `MSG_DONTWAIT`.

Part 2 does not hold on Darwin. XNU's `sosend` decides whether to sleep from
`so->so_state & SS_NBIO` and its own internal `MSG_NBIO`; the `MSG_DONTWAIT` a
caller passes reaches it only as the sockbuf-lock wait hint. The send therefore
blocked until the whole buffer was queued, exactly as a plain `write` would, and
part 1 could not help — `poll` had already said the socket was writable.

Both entry points are ungated `pub` API (`runtime::blocking_io`,
`server_native::blocking`), so this reached any macOS consumer that selects the
blocking drivers, not only the embedded builds they were written for.

## Measurement

macOS CI, 2026-07-27, run 30278102379 (`macos-arm64`, `macos-x86_64`), both
arches identical:

| case | `SO_SNDTIMEO` | result |
| --- | --- | --- |
| `blocking_io::the_deadline_loop_ends_a_trickling_peer` | armed, 50 ms | pass — ended at the 200 ms deadline |
| `blocking_io::the_deadline_holds_with_no_socket_send_timeout` | none | **fail** — outlived a 20 s bounded `recv`, 3/3 tries |
| `ip_port::a_stalled_write_bounds_itself_without_arming_so_sndtimeo` | none | **fail** — 240 s nextest timeout, 3/3 tries |

The pair is the discriminator: identical code, identical deadline, and the only
difference is whether an option was armed for the send to fall back on. Nothing
else fits — `poll` is bounded by the deadline in milliseconds, a send error
would have returned through the loop, and a panicking writer would have shown as
a disconnected channel rather than a timeout.

Linux was unaffected (`tcp_sendmsg` honours `MSG_DONTWAIT`), and Windows takes
the `not(unix)` arm, which arms `SO_SNDTIMEO` from inside the module.

## VxWorks was the same defect from the other side

Darwin is where CI could see it, but it is not the worst case. VxWorks 7
implements no `SO_SNDTIMEO` at all — `setsockopt` answers `ENOPROTOOPT`, errno
42, measured on target (`doc/vxworks-circuit-wedge-on-target-measurement.md`
§5) — so there the flag was not one of two mechanisms but the only one, and
whether VxWorks honours `MSG_DONTWAIT` on `send` had never been measured. The
bound on the target this code was written for rested on an untested assumption.

## The fix

C's shape, in both crates: own the socket's blocking mode once, and poll both
directions.

- `blocking_io::own_blocking_mode`, called by `write_frame_deadline` (so no
  caller can be the one that forgot) and by `drive_socket_blocking` (so the mode
  is right from the first read). `wait_readable` is the new twin of
  `wait_writable`; `reader_pump` waits on it instead of parking in `read`.
- `ip_port::OWNED_NONBLOCKING`, applied by `IpIoState::own_blocking_mode` at the
  end of `connect` for all four transports, with `BoundNextRead` polling ahead
  of each read.

`MSG_DONTWAIT` stays in both send paths. It is no longer load-bearing; where a
target honours it, it saves the loop a `poll` when the socket has room.

`SO_RCVTIMEO` stops being the read mechanism on unix and stays as the value:
`drive_socket_blocking` passes `PumpConfig::read_timeout` to the pump directly,
and `spawn_reader_pump` reads it back off the socket, so no public signature
changed and no target needs the getter on the path that matters.

Windows keeps blocking sockets and both socket options, which it implements; the
`not(unix)` arms of both waits are built on them.

## What is owed

The read-path conversion has not been re-measured on the embedded targets. The
write direction is safe ground — `libc::poll` is already what both RTEMS and
VxWorks run in production in this same module — but the reader now reaches EOF
through `poll` returning `POLLIN`/`POLLHUP` rather than through a parked `read`
being interrupted, and that teardown is what `epics-libcom-rs/src/lib.rs:46`
records as measured. The hosted suites cover it (`the_reader_guard_returns_a_pump_parked_in_read`
and the exec-backend suite pass), but a target run on the RTEMS/QEMU box and the
VxWorks guest is still owed before the next embedded release.
