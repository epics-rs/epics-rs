//! Names the one capability `auth::plain` needs and `cfg(unix)` cannot express.
//!
//! This script used to also call `epics_rtems_boot::contract::emit_link_args()`
//! for `realtime-pva-ioc`. That binary now lives in `epics-bridge-rs`
//! (doc/qsrv-rtems-design.md §9.7), and link arguments are emitted by the
//! package that owns the binary, so the call moved with it. This package
//! produces no RTEMS binary, so emitting them here would have decorated a link
//! that never happens.
//!
//! # The dual meaning this exists to split
//!
//! `cfg(unix)` was being used in `auth/plain.rs` to mean two different things
//! at once:
//!
//! 1. **"`libc` is linked."** That is what `Cargo.toml`'s
//!    `[target.'cfg(unix)'.dependencies] libc` means, and it is *true* on
//!    RTEMS — `server_native::blocking` and `search_engine` both call libc on
//!    target.
//! 2. **"there is a passwd/group database behind `getpwnam`/`getgrgid`."**
//!    That is false on RTEMS: newlib has no `getgrouplist(3)` at all, and an
//!    image whose IMFS carries no `/etc/passwd` has nothing for the other two
//!    to read.
//!
//! RTEMS satisfies (1) and not (2), so one predicate could not carry both. The
//! arms that need (2) now select on `local_account_db`, emitted here;
//! `Cargo.toml` keeps `cfg(unix)` for (1), where it is correct.
//!
//! # Why an allowlist, not `all(unix, not(target_os = "rtems"))`
//!
//! Excluding RTEMS by name fixes the target we know about and silently
//! mishandles the next one: VxWorks is `cfg(unix)` with no passwd database
//! either, and any future non-hosted unix target would inherit the hosted arm
//! by default — the same defect with a different `target_os`. An allowlist
//! inverts that default. An unrecognised target gets the conservative arm, and
//! conservative is the safe direction here: the fallback reports an account's
//! own name as its only role, which can only *deny* a `member group:` ACF rule
//! that would otherwise have matched, never grant one.
//!
//! Adding a target is a one-line change here, and it is a decision someone has
//! to make deliberately rather than one that happens by inheritance.

// # `pva_blocking_client` — forcing the blocking client transport on a host
//
// The PVA client dials through one seam with two implementations: the tokio
// `TcpStream` on `tokio_backend`, and `runtime::blocking_io`'s two-thread pump
// on `exec_backend`, which gives a spawned future no reactor. This `--cfg`
// forces the second one on a build that would otherwise take the first; it is
// the only way to reach that arm without also moving the whole crate onto the
// exec backend. Showing that the second one leaves the frame
// pipeline untouched means running the *whole* host client suite against it,
// including the integration tests in `tests/`, which are separate crates and so
// cannot see anything `#[cfg(test)]`.
//
// It is a bare `--cfg`, checked below and emitted by nobody:
//
//     RUSTFLAGS="--cfg pva_blocking_client" cargo nextest run -p epics-pva-rs
//
// A cargo feature was the obvious alternative and is the wrong tool: features
// unify across the graph, so any crate in a workspace build enabling it would
// silently move every other crate's PVA client onto the blocking transport. A
// runtime env var would ship the switch in release binaries, where an operator
// setting it would change the transport of a production IOC. A `--cfg` that no
// manifest can turn on cannot reach either place — it exists only for a build
// someone typed the flag for, which is the same mechanism
// `scripts/rtems-check.sh` uses for `rtems_boot_linked`.

/// Targets whose libc is backed by a real passwd/group database.
///
/// Every entry is a hosted unix with `getpwnam(3)`, `getgrgid(3)` and
/// `getgrouplist(3)`. Deliberately absent: `rtems` and `vxworks` (unix-family,
/// no account database), and anything not listed.
const LOCAL_ACCOUNT_DB_TARGETS: &[&str] = &[
    "linux",
    "android",
    "macos",
    "ios",
    "tvos",
    "watchos",
    "freebsd",
    "netbsd",
    "openbsd",
    "dragonfly",
    "solaris",
    "illumos",
];

// # `exec_backend` / `tokio_backend` — does a spawned future get a reactor?
//
// `epics-base-rs`'s `build.rs` defines this rule and this script repeats it,
// for this crate's own compilation:
//
//     exec_backend  ⟺  epics_embedded_target (target_os in {"rtems", "vxworks"})
//                   ||  feature "rtems-exec-model"
//     tokio_backend ⟺  otherwise
//
// Why the PVA client needs it. Every client task is started through
// `runtime::task::spawn`. On `exec_backend` that lands on a callback-pool
// worker with **no tokio reactor entered**, so any `tokio::net` socket the
// task opens panics — including in a hosted process that has a tokio runtime
// elsewhere, because the runtime is not entered on that worker. The client's
// UDP SEARCH transport was gated on `not(target_os = "rtems")`, which names
// the target when the fact it needs is the *backend*; a host build with
// `--features rtems-exec-model` compiled the UDP transport in and panicked on
// it at `realtime-pva-ioc`'s first search (measured, `doc/calink-rtems-design.md`
// §10.10 item 2). `tokio_backend` is the predicate that means "a reactor
// exists", so the transport takes that one and `SearchTransport` has the
// single `NameServersOnly` variant on `exec_backend` — the target's shape,
// now reached by the host build that models the target.
//
// This is a third copy of a four-line rule, so it is pinned rather than
// trusted: a `const` assertion in `src/lib.rs` checks it against
// `epics_base_rs::runtime::task::HAS_TOKIO_REACTOR` at compile time. A build
// that enables `epics-base-rs/rtems-exec-model` without this crate's own
// `rtems-exec-model` fails to compile instead of panicking at boot.

/// The `local_account_db` capability decision, as one named predicate.
///
/// Named, rather than written inline in `main`, because it is the thing
/// `auth::plain`'s `the_capability_is_owned_by_an_allowlist_in_the_build_script`
/// guard is about: the guard reads this function's body and nothing else, so it
/// tests the decision rather than the file. `unix` is a precondition rather
/// than a synonym — it is what puts `libc` in the dependency graph at all
/// (`Cargo.toml`, meaning 1 above) — so it is stated separately from the
/// account-database question the allowlist answers.
fn local_account_db(unix: bool, os: &str) -> bool {
    unix && LOCAL_ACCOUNT_DB_TARGETS.contains(&os)
}

fn main() {
    println!("cargo::rustc-check-cfg=cfg(local_account_db)");
    // Declared, never emitted here. See the note above `PVA_BLOCKING_CLIENT`.
    println!("cargo::rustc-check-cfg=cfg(pva_blocking_client)");
    println!("cargo::rustc-check-cfg=cfg(exec_backend)");
    println!("cargo::rustc-check-cfg=cfg(tokio_backend)");
    println!("cargo::rustc-check-cfg=cfg(epics_embedded_target)");

    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let embedded_target = matches!(os.as_str(), "rtems" | "vxworks");
    if embedded_target {
        println!("cargo::rustc-cfg=epics_embedded_target");
    }

    let host_exec_model = std::env::var_os("CARGO_FEATURE_RTEMS_EXEC_MODEL").is_some();
    if embedded_target || host_exec_model {
        println!("cargo::rustc-cfg=exec_backend");
    } else {
        println!("cargo::rustc-cfg=tokio_backend");
    }

    let unix = std::env::var_os("CARGO_CFG_UNIX").is_some();

    if local_account_db(unix, &os) {
        println!("cargo::rustc-cfg=local_account_db");
    }
}
