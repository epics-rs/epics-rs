//! Names the one capability `auth::plain` needs and `cfg(unix)` cannot express.
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

fn main() {
    println!("cargo::rustc-check-cfg=cfg(local_account_db)");

    // `unix` is a precondition rather than a synonym: it is what puts `libc`
    // in the dependency graph at all (`Cargo.toml`, meaning 1 above). Stating
    // it explicitly keeps the two meanings visibly separate.
    let unix = std::env::var_os("CARGO_CFG_UNIX").is_some();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    if unix && LOCAL_ACCOUNT_DB_TARGETS.contains(&os.as_str()) {
        println!("cargo::rustc-cfg=local_account_db");
    }
}
