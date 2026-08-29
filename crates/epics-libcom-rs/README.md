# epics-libcom-rs

EPICS `libCom` for Rust: the layer an IOC is built *on*, with no record system
above it. Two modules, named for the two halves of C's libCom they port:

| module | C counterpart | what it is |
|---|---|---|
| `runtime` | `epicsThread`, `epicsTime`, `errlog`, `envDefs`, `epicsString` | the task spawn/sleep/interval seam and its two backends, EPICS priority bands, the general-time provider, the environment-parameter table, `errlog` |
| `net` | `osiSock` | the EPICS protocols' shared socket layer — per-NIC async UDP with TX/RX accuracy, interface enumeration, loopback multicast |

It exists so a consumer that wants the concurrency and socket primitives — a
protocol client, a gateway, `pvxs-rs` — does not have to take the database with
them (issue #55).

**You usually do not depend on this crate directly.** `epics-base-rs` re-exports
both modules at their original paths, so `epics_base_rs::runtime::…` and
`epics_base_rs::net::…` still resolve exactly as before; the split changed no
call site anywhere in the workspace. Depend on `epics-libcom-rs` only when you
want the layer *without* `epics-base-rs`.

## The two task backends

`runtime::task`'s spawn/sleep/interval seam has two mutually exclusive
backends, selected by `build.rs` into one `cfg` so the ~two dozen seam sites
gate on a single uniform condition:

- **`tokio_backend`** — the tokio runtime. The hosted default; you get this
  with no flags.
- **`exec_backend`** — the std-thread background executor (callback pool +
  delayed timer + scanOnce worker). Unconditional on `armv7-rtems-eabihf`,
  which has no tokio reactor.

`EXEC_BACKEND` states which one a given build got, so a crate above can pin its
own copy of the predicate to it with a `const _: () = assert!(…)` —
`epics-base-rs` does exactly that.

## Feature levers

| feature | what it does |
|---|---|
| `linux-rt` | back `runtime::sync::PriorityInheritanceMutex` with a `pthread_mutex_t` carrying `PTHREAD_PRIO_INHERIT` (epics-base 7-E `5a8b6e41`). No-op off Linux. Only enable where threads run `SCHED_FIFO`/`SCHED_RR` and unbounded priority inversion is a real risk. |

`epics-base-rs` forwards it, so `--features epics-base-rs/linux-rt` means what
it always did.

## Backend lever

`EPICS_RS_BUILD_EXEC_BACKEND=thread` selects `exec_backend` on a **hosted**
target: a Linux (e.g. PREEMPT_RT) blocking-front-end deployment that wants the
same runtime-free spawn/timer backend the RTEMS build uses, driving async
record-processing completion on dedicated OS threads instead of a tokio
runtime. Unset or `tokio` is the hosted default. It is a variable and not a
cargo feature because a feature that flips a backend is not additive: while it
was one, `--all-features` turned the reactor *off* and no single invocation
meant "everything on". Every `build.rs` that reads it also emits
`cargo::rerun-if-env-changed`, so changing the value rebuilds what depends on
it.

## RTEMS

The `net` module's socket-bearing submodules (`async_udp_v4`, `iface_map`,
`loopback_mcast`) are host-only — `socket2`/`if-addrs` do not build for
`armv7-rtems-eabihf`, and the RTEMS CA server uses the separate raw-libc socket
driver. The wire constants beside them stay on every target, because the PVA
SEARCH decoder has to embed them on RTEMS too.

`./scripts/rtems-check.sh` compiles this crate for the target in both build
configurations.
