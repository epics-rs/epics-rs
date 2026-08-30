// A `pub` type that no downstream path can name is a public API hole: a
// caller receives the value and cannot write its type, so they cannot store
// it in a struct, name it in a signature, or implement a trait over it. Three
// of these were live in this crate and one of them — `ExceptionSite` on the
// public `CaException` — was found only because a rustdoc link happened to
// point at it. This lint finds the population instead of the sample, and the
// crate's `clippy -D warnings` gate turns it into a build failure.
#![warn(unnameable_types)]
#![allow(
    clippy::collapsible_if,
    clippy::map_entry,
    clippy::io_other_error,
    clippy::new_without_default,
    clippy::redundant_closure,
    clippy::single_match,
    clippy::type_complexity,
    clippy::unnecessary_cast
)]

//! EPICS Channel Access protocol — client and server.
//!
//! This crate provides the CA wire protocol implementation,
//! separated from the core IOC infrastructure in `epics-base-rs`.
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
//! | `iocStats` | `4.0.1` |
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

// The CA client, the discovery stack, and the CA-link resolver.
//
// The client and everything over it (`channel`, `calink`, the `cli`/`copt`
// tool support) are selected by the `client-core` FEATURE rather than by the
// target, because which of them a build gets is a choice a build makes — a
// record link needs the circuit, the search engine and the resolver, and needs
// none of the UDP discovery stack. `default = ["client"]` keeps every hosted
// consumer on the full set; an RTEMS image selects `client-core`
// (`scripts/rtems-check.sh`).
//
// `hostname` stays gated on `epics_embedded_target` (RTEMS or VxWorks), not on
// a feature: it is host-only for a reason that is a fact about the platform —
// `getnameinfo` has no newlib backing on RTEMS and is absent from `libc`'s
// VxWorks module too — and it has a consumer outside the client. What the
// `client` feature owns is the client's *references* to it.
//
// `discovery` and `repeater` take `tokio_backend` for the reason spelled out
// at each: their backends and loops await, and the backend, not the triple,
// is what decides whether there is a reactor under them. Each also has a
// consumer outside the client — `discovery` backs the CA server's mDNS/DNS
// announce, `repeater` backs `ca-repeater-rs` — so the `client` feature gates
// the client's references, not the modules.
//
// `repeater` takes `tokio_backend` rather than the target, because its
// question is the backend's and not the triple's: both its loops await
// `epics_base_rs::net::recv_from_with_drop_count_socket` on a
// `tokio::net::UdpSocket`, which needs an ambient reactor, and `exec_backend`
// — no reactor — is selected on a *host* build too, by
// `EPICS_RS_BUILD_EXEC_BACKEND=thread`. Under the target gate alone the module
// compiled into that build and the
// only thing standing between it and a runtime panic was that nothing had
// started it yet.
pub mod audit;
/// CA links for record INP/OUT fields — resolves ` CA`-modified /
/// `ca://` record link fields to a live CA client (monitor-backed
/// cache). Mirrors C `dbCa.c` / `dbCaLink`. Compiled whenever the client
/// is: having `epics-ca-rs` with its default features is enough to
/// resolve CA links, no separate opt-in.
#[cfg(feature = "client-core")]
pub mod calink;
pub mod cap_token;
// CA client channel state (`AccessRights`, id allocators); used only by
// `client`.
#[cfg(feature = "client-core")]
pub(crate) mod channel;
pub mod chaos;
// CA client-tool option/format helpers. They name `client` items, so they
// follow it; still host-only on top of that, because their consumers are the
// host CLI binaries.
#[cfg(all(feature = "client-core", not(epics_embedded_target)))]
pub mod cli;
#[cfg(feature = "client-core")]
pub mod client;
// CA client-tool (`caget`/`caput`/`cainfo`) argument parsing; it references
// `cli::IntStyle` and backs only the host client binaries.
#[cfg(all(feature = "client-core", not(epics_embedded_target)))]
pub mod copt;
// Service discovery (mDNS browse, DNS-SD watch). Two predicates, because there
// are two different dependencies here and one gate cannot say both. The
// reactor-dependent half is `mdns`, `dnssd` and `dns_update`: their
// `discover()`/`subscribe()`/`register()` await on `mdns-sd`'s, `hickory`'s and
// `tokio::net`'s own sockets, and `exec_backend` — selected on a *host* build
// too, by `EPICS_RS_BUILD_EXEC_BACKEND=thread` — has no reactor to await on, so
// those three carry
// `tokio_backend` inside the module. The module's own surface —
// `DiscoveryConfig` and its `EPICS_CA_DISCOVERY` parsing, `StaticBackend`,
// `ZoneSnippet` — is plain `std` plus `tokio::sync`, which needs no reactor;
// what it needs is the optional dependencies to be buildable, and that is the
// target. A single `tokio_backend` here was the coarse version of both facts
// and took the parsing and the zone renderer down with the backends.
#[cfg(not(epics_embedded_target))]
pub mod discovery;
pub mod estdlib;
// Reverse-DNS (`getnameinfo` via `socket2::SockAddr`) for the CA client's
// peer-name cache. Its only consumer is the client's `peer_display_name` /
// `peer_resolved_name`, so it follows the `client` feature as well as the
// target: `client-core` names a peer by its dotted address, which is C's own
// answer for an address with no PTR record.
#[cfg(all(feature = "client", not(epics_embedded_target)))]
pub mod hostname;
pub(crate) mod iocinf;
pub mod observability;
pub mod protocol;
#[cfg(tokio_backend)]
pub mod repeater;
// The repeater's reactor-free half. Two clauses, and they answer different
// questions. `not(epics_embedded_target)` is what the code itself needs: the
// client table and the datagram rewrites want `socket2` (absent on RTEMS and
// VxWorks) and no reactor, so the gate is the target's, never whatever gate
// `repeater` happens to carry. `any(tokio_backend, test)` is the separate
// question of whether anything can consume it — the module is `pub(crate)`,
// so it is unreachable from outside, and `repeater` is its only caller. When
// `repeater` narrowed to `tokio_backend` this module kept compiling into
// reactor-free host builds with no caller at all, and every item in it became
// dead code under `-D warnings`. A `pub mod` would not need this clause; this
// one does, because its visibility is what makes the absence provable.
// The predicates are named rather than pointed at: another branch is free to
// change the neighbouring declaration's `cfg`, and a comment that says "the
// gate above" then merges cleanly into a false sentence.
#[cfg(all(not(epics_embedded_target), any(tokio_backend, test)))]
pub(crate) mod repeater_clients;
pub mod replay;
pub mod server;
pub mod tls;

// The timing contract for this crate's tests — `FACT_BUDGET` and the
// `barrier` primitive — lives in `tests/common/budget.rs`, because the
// integration suites reach it by `#[path]` and a test rule with two homes is
// two rules. The unit tests inside `src/` cannot use `#[path]` from their own
// (often inline, variably nested) module, so the crate root declares it once
// and they reach it as `crate::test_budget`.
#[cfg(test)]
#[path = "../tests/common/budget.rs"]
mod test_budget;

// Pins this crate's `exec_backend`/`tokio_backend` decision (`build.rs`) to
// `epics-base-rs`'s.
//
// Both scripts compute the same rule from the same two inputs — the target OS
// and `EPICS_RS_BUILD_EXEC_BACKEND` — but they compute it independently, so a
// build in which one of the two scripts did not see the variable would give
// `runtime::task::spawn` a reactor-free backend while this crate still compiled
// the reactor-backed UDP SEARCH transport in and selected it. That is exactly the configuration
// measured as a boot panic, and it is the one state the two-variant
// `search::SearchTransport` cannot rule out on its own.
//
// So it is ruled out here instead: the two views must agree or the crate does
// not compile.
const _: () = assert!(
    epics_base_rs::runtime::task::HAS_TOKIO_REACTOR == cfg!(tokio_backend),
    "epics-ca-rs and epics-base-rs disagree about the runtime::task backend. \
     Both derive it from EPICS_RS_BUILD_EXEC_BACKEND, so they cannot disagree \
     over what was asked for: one of the two build scripts did not see the \
     variable. Check that both carry \
     `rtems_exec_gate::CANONICAL_DERIVATION`, whose \
     `cargo::rerun-if-env-changed` line is what makes a changed value rebuild \
     this crate"
);

// Re-export commonly used types from epics-base-rs for convenience
pub use epics_base_rs::error::{CaError, CaOp, CaResult};
pub use epics_base_rs::runtime;
pub use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};
