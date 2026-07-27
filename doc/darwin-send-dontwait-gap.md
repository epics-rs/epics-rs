# Darwin ignores `MSG_DONTWAIT` on `send`, so the write deadline does not hold there

**Status: open.** Two tests are excluded on `target_vendor = "apple"` to keep
the rest of the matrix readable. The exclusions mark this defect; they are not
a statement that macOS is out of scope.

## What breaks

`epics-libcom-rs::runtime::blocking_io::write_frame_deadline` and
`asyn-rs::drivers::ip_port::write_with_retry` both bound a frame write with the
same two-part contract:

1. `poll(POLLOUT)` against the caller's deadline, and
2. a send that cannot park, via `MSG_DONTWAIT`.

Part 2 does not hold on Darwin. XNU's `sosend` decides whether to sleep from
`so->so_state & SS_NBIO` and its own internal `MSG_NBIO`; the `MSG_DONTWAIT`
a caller passes reaches it only as the sockbuf-lock wait hint. The send
therefore blocks until the whole buffer is queued, exactly as a plain `write`
would, and part 1 cannot help — `poll` already said the socket was writable.

Both entry points are ungated `pub` API (`runtime::blocking_io`,
`server_native::blocking`), so this reaches any macOS consumer that selects the
blocking drivers, not only the embedded builds they were written for.

## Measurement

macOS CI, 2026-07-27, runs 30278102379 (`macos-arm64`, `macos-x86_64`), both
arches identical:

| case | `SO_SNDTIMEO` | result |
| --- | --- | --- |
| `blocking_io::the_deadline_loop_ends_a_trickling_peer` | armed, 50 ms | pass — ends at the 200 ms deadline |
| `blocking_io::the_deadline_holds_with_no_socket_send_timeout` | none | **fail** — outlived a 20 s bounded `recv`, 3/3 tries |
| `ip_port::a_stalled_write_bounds_itself_without_arming_so_sndtimeo` | none | **fail** — 240 s nextest timeout, 3/3 tries |

The pair is the discriminator: identical code, identical deadline, and the only
difference is whether an option was armed for the send to fall back on. Nothing
else fits — `poll` is bounded by the deadline in milliseconds, a send error
would have returned through the loop, and a panicking writer would have shown
as a disconnected channel rather than a timeout.

Linux is unaffected (`tcp_sendmsg` honours `MSG_DONTWAIT`), and Windows takes
the `not(unix)` arm, which arms `SO_SNDTIMEO` from inside the module.

## Why the port is exposed and C is not

C sets the socket non-blocking once at connect — `setNonBlock(fd, 1)`,
`drvAsynIPPort.c:511`, under `USE_POLL` — and polls its reads as well as its
writes. It never asks a send flag to do this job, so no platform's treatment of
`MSG_DONTWAIT` can reach it.

This port deviated deliberately: a per-send flag leaves the shared file
description alone, which matters because the reader pump shares the exact
descriptor (`try_clone` is unavailable on RTEMS libbsd sockets) and because
reads are bounded with `SO_RCVTIMEO`, which VxWorks does implement and which a
permanently non-blocking socket would defeat. The deviation is what Darwin
finds.

## Closing it

The fix is C's shape: this module owns the socket's blocking mode, sets it
non-blocking once, and polls both directions — a `wait_readable` symmetric to
the existing `wait_writable`. That removes the dependency on any flag or
option, so the claim `wait_writable` already makes ("no socket option is
load-bearing, and a target that implements none of them is bounded exactly as
one that implements them all") becomes true everywhere instead of on three
platforms out of four.

The cost is that it changes the read path on the two embedded targets. The
write direction is safe ground — `libc::poll` is already what both RTEMS and
VxWorks run in production in this same module — but the read conversion
replaces the `SO_RCVTIMEO` bound and the teardown that `lib.rs:46` records as
measured ("shutdown wakes a parked read"). Both need re-measuring on target
before it lands, on the RTEMS/QEMU box and on the VxWorks guest, which is why
this is a follow-up change and not a hotfix inside the round-3 integration PR.
