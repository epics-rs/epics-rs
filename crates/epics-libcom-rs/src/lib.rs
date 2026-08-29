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
//! The task backend is not one: it is chosen by the
//! `EPICS_RS_BUILD_EXEC_BACKEND` environment variable, read by `build.rs`. See
//! [`EXEC_BACKEND`].
//!
//! * `linux-rt` — back [`runtime::sync::PriorityInheritanceMutex`] with a
//!   `PTHREAD_PRIO_INHERIT` `pthread_mutex_t` on Linux.
//!
//! # C reference pins
//!
//! Every `file.c:NNN` citation in this crate resolves at the tree and revision
//! below, not at whatever that tree's working copy holds today. These trees are
//! checked out on local branches here and run ahead of their pins.
//!
//! | tree | pinned revision |
//! | --- | --- |
//! | `epics-base` | `R7.0.10` |
//! | `pvxs` | `1.5.1-42-gb568e93` |
//! | `ca-gateway` | `R2-1-3-0-54-g0666f21` |
//! | `asyn` | `R4-45-19-ge2a281e2` |
//! | `calc` | `R3-7-5-49-gf207871` |
//!
//! **Resolve by symbol at the pin; the line is a hint.** Find the named
//! function, struct, macro or field first, and treat the line number as a hint
//! that has to land inside that construct. Three cases follow:
//!
//! 1. Construct at the pin, line lands in it — the citation is exact. A
//!    reference checkout ahead of the pin will disagree; that disagreement is
//!    the checkout's, not the citation's.
//! 2. Construct at the pin, line lands outside it — line drift. Keep the
//!    symbol and move the line to the pin's.
//! 3. Construct absent at the pin — the citation means code added after it,
//!    and is NOT moved onto the pin, where it would point at lines that do not
//!    exist. It names the revision it means inline, beside the line span: the
//!    upstream PR and commit, and that both are later than the pin this table
//!    gives. `epics-libcom-rs` already carries that form.
//!
//! Every pin above passes `git merge-base --is-ancestor <pin> origin/<default>`
//! in its own tree, which is the test a pin has to meet. A `git describe`
//! string names an exact commit and is worth as much as a tag; what
//! disqualifies a revision is being reachable only from a fork branch or an
//! unmerged PR, because then it names nothing a reader outside this workspace
//! can fetch.
//!
//! Resolve each citation on its own. One sentence can cite two lines that are
//! right at different revisions, and a check run at either revision then
//! reports a single tidy error while vouching for the very citation the other
//! condemns.
//!
//! A row reading *no settled pin* means no revision has been agreed for that
//! tree: say which revision you read, and do not take its `HEAD` for the pin.
//! Citations into non-EPICS sources (libc, RTEMS, `rtems-libbsd`, VxWorks,
//! vendored third-party) are outside this table and carry no pin.

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
    "the exec backend (EPICS_RS_BUILD_EXEC_BACKEND=thread) relies on POSIX \
     blocking-socket semantics (shutdown wakes a parked read; loopback sends \
     see backpressure) that Windows does not provide; build the default tokio \
     backend on Windows instead"
);

// Lets `#[epics_macros_rs::epics_test]` expansions — which name the runtime
// crate by its external path — resolve inside this crate's own unit tests,
// where proc-macro-crate reports `FoundCrate::Itself`. Same device as
// `epics-base-rs`'s alias for the same macro.
extern crate self as epics_libcom_rs;

pub mod net;
pub mod runtime;
pub mod walltime;

/// Which [`runtime::task`] backend this build selected — `true` for the
/// reactor-free std-thread [`runtime::background`] executor, `false` for tokio.
///
/// The predicate is computed once, in this crate's `build.rs`, from the target
/// OS and `EPICS_RS_BUILD_EXEC_BACKEND`. A crate above that derives the same
/// `cfg` from its own `build.rs` (`epics-base-rs` does, for `server::scan`)
/// can pin the two together with a `const _: () = assert!(...)`, so a build
/// script that did not see the variable fails to compile instead of splitting
/// the workspace across two backends.
pub const EXEC_BACKEND: bool = cfg!(exec_backend);
