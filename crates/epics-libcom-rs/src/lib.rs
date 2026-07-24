//! EPICS `libCom` for Rust: the layer an IOC is built *on*, with no record
//! system above it.
//!
//! This is `epics_base_rs::runtime` and `epics_base_rs::net` lifted out of
//! `epics-base-rs` (issue #55) so a consumer — a protocol client, a gateway,
//! `pvxs-rs` — can take the concurrency and socket primitives without taking
//! the database with them. The name is C's: `libCom` is where upstream EPICS
//! keeps `epicsThread`, `epicsTime`, `errlog`, `envDefs` *and* `osiSock`, which
//! is exactly this crate's two modules.
//!
//! `epics-base-rs` re-exports both modules at their original paths, so
//! `epics_base_rs::runtime::…` and `epics_base_rs::net::…` still resolve and
//! nothing downstream had to change.
//!
//! * [`runtime`] — the task seam and its two backends, `epicsThread`-parity
//!   priority bands, `errlog`, the EPICS string/environment types, the
//!   general-time provider.
//! * [`net`] — the EPICS protocols' shared socket layer: per-NIC async UDP,
//!   interface enumeration, loopback multicast. Host-only; the wire constants
//!   beside it compile for every target, RTEMS included.
//! * [`walltime`] — [`WallTime`](walltime::WallTime), the wall-clock instant
//!   `runtime::time` returns. It lived in `epics_base_rs::types` and moved down
//!   with its producer; `epics-base-rs` re-exports it at `types::WallTime`.
//!
//! # Features
//!
//! * `rtems-exec-model` — select the reactor-free `exec_backend` on a hosted
//!   target. See [`EXEC_BACKEND`].
//! * `linux-rt` — back [`runtime::sync::PriorityInheritanceMutex`] with a
//!   `PTHREAD_PRIO_INHERIT` `pthread_mutex_t` on Linux.

// The three `epics-base-rs` crate-level allows this code was written under and
// still needs — `collapsible_if` and `manual_range_contains` in `runtime`,
// `io_other_error` in `net`. Narrowed to those three rather than inherited
// wholesale: the extraction is a move, so the code is byte-identical and a
// lint it does not trip has no business being silenced here.
#![allow(
    clippy::collapsible_if,
    clippy::io_other_error,
    clippy::manual_range_contains
)]

// The exec backend's blocking pumps end a parked reader with a local
// `shutdown(Shutdown::Both)` and bound a stuck writer through loopback
// send-backpressure (`runtime::blocking_io`). Both are POSIX blocking-socket
// semantics; Windows provides neither (measured, PR #56 CI 2026-07-24: a
// parked `recv` outlived shutdown by the full 120 s test bound, and an
// 8 MiB frame to a never-reading peer was swallowed in 12 ms), so a Windows
// build selecting this backend would hang on connection teardown instead of
// failing visibly. Refuse it at compile time rather than ship that.
#[cfg(all(windows, exec_backend))]
compile_error!(
    "the exec backend (`rtems-exec-model`) relies on POSIX blocking-socket \
     semantics (shutdown wakes a parked read; loopback sends see \
     backpressure) that Windows does not provide; build the default tokio \
     backend on Windows instead"
);

pub mod net;
pub mod runtime;
pub mod walltime;

/// Which [`runtime::task`] backend this build selected — `true` for the
/// reactor-free std-thread [`runtime::background`] executor, `false` for tokio.
///
/// The predicate is computed once, in this crate's `build.rs`, from the target
/// OS and the `rtems-exec-model` feature. A crate above that derives the same
/// `cfg` from its own `build.rs` (`epics-base-rs` does, for `server::scan`)
/// can pin the two together with a `const _: () = assert!(...)`, so a feature
/// forward that stops being wired fails to compile instead of splitting the
/// workspace across two backends.
pub const EXEC_BACKEND: bool = cfg!(exec_backend);
