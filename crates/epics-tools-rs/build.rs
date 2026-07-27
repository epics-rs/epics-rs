//! Names the one capability `procserv` needs and `cfg(unix)` cannot express,
//! and refuses the build where a unix target does not have it.
//!
//! # The dual meaning this exists to split
//!
//! `cfg(unix)` was the crate's gate, used to mean two things at once:
//!
//! 1. **"`libc` and `nix` are linked."** That is what `Cargo.toml`'s
//!    `[target.'cfg(unix)'.dependencies]` means, and it is true on RTEMS and
//!    VxWorks — both are `target_family = "unix"`.
//! 2. **"there is a second process to supervise, reached through
//!    `forkpty(3)`/`execvp(3)`, with a controlling terminal behind fd 0."**
//!    That is false on both.
//!
//! Measured 2026-07-27. RTEMS's `fork()` is an inoperable stub that sets
//! `ENOSYS` (`rtems/cpukit/posix/src/fork.c:48-50`), and its BSP archive
//! `librtemscpu.a` defines `forkpty`, `openpty`, `execvp` and `posix_spawn`
//! zero times each (control: `fork` and `tcgetattr` once each, 2360 `T`
//! symbols in the archive). The whole VxWorks 7 SDK header tree
//! (`wrsdk-vxworks7-qemu-1.17.0`) names `forkpty`/`openpty` in zero files
//! (control: `tcsetattr` in two). C states the same restriction in its build
//! system rather than in code — `PROD_HOST = procServ`
//! (`epics-modules/procServ/Makefile.Epics.in:12`), which EPICS base defines
//! as `product_only_for_host_type_systems`
//! (`epics-base/configure/Sample.Makefile:190`) — and procServ's README names
//! Linux, Solaris, MacOS and Cygwin as the platforms it runs on.
//!
//! # Why an allowlist, not `all(unix, not(target_os = "rtems"), ...)`
//!
//! Excluding the two targets by name answers "is this RTEMS or VxWorks", which
//! goes stale the next time a unix-family RTOS triple appears — that target
//! would inherit the hosted arm by default, which is the same defect with a
//! different `target_os`. The allowlist answers "were this platform's process
//! facilities verified", and an unrecognised target is simply not on it. Same
//! shape, and for the same reason, as `epics-pva-rs`'s `local_account_db`.
//!
//! # Why a build refusal here and an absent module on Windows
//!
//! The two outcomes differ because the two situations do:
//!
//! * Non-unix (Windows) — the module is **unported**, not unportable. C ships
//!   a Cygwin build and a ConPTY backend is possible. It compiles away, the
//!   `procserv-rs` bin says why at startup, and `cargo build --workspace
//!   --tests` keeps passing on the Windows CI cells.
//! * unix that is not a host platform — the module is **unportable**. An image
//!   where the IOC *is* the system has no second process to supervise. Left to
//!   compile away it would offer an embedded consumer an empty API and imply a
//!   port is merely missing; worse, until this script existed the only thing
//!   stopping such a build was that `nix` happens not to compile for those
//!   triples (30 errors for `armv7-rtems-eabihf`, 33 for `x86_64-wrs-vxworks`,
//!   measured on this tree), all of them inside `nix::unistd` and none of them
//!   saying anything about procServ. The refusal states the reason ourselves
//!   instead of borrowing a dependency's accident.
//!
//! The refusal is unconditional on such a target — not conditioned on the
//! `procserv` feature, which would be the narrower and more tempting rule.
//! There is no configuration of this crate that builds for one of those
//! triples: `tokio` with `features = ["full"]` is an unconditional dependency,
//! and `--no-default-features -p epics-tools-rs` for `armv7-rtems-eabihf`
//! fails with 89 errors inside `mio`/`nix` (measured on this tree). A
//! feature-conditioned refusal would therefore not preserve a working
//! configuration; it would only replace this message with those 89 errors.

/// Targets whose libc supervises a separate process the way `procserv` needs.
///
/// Every entry has `fork(2)`, `forkpty(3)` (or procServ's own `forkpty.c`
/// fallback over `openpty`), `execvp(3)` and a controlling terminal on fd 0.
/// The list is C procServ's own supported set plus the BSDs that share the
/// same `forkpty(3)`. Deliberately absent: anything not verified — which is
/// the point of a list nobody has to remember to update.
const PROCSERV_HOST_TARGETS: &[&str] = &[
    "linux",
    "macos",
    "freebsd",
    "netbsd",
    "openbsd",
    "dragonfly",
    "solaris",
    "illumos",
    "cygwin",
];

/// The capability decision, as one named predicate.
///
/// Named rather than written inline in `main` for the reason
/// `epics-pva-rs`'s `local_account_db` is: the guard test in
/// `procserv::tests` reads this function's body and nothing else, so it tests
/// the decision rather than the file. `unix` is a precondition rather than a
/// synonym — it is what puts `libc` and `nix` in the dependency graph at all
/// (`Cargo.toml`, meaning 1 above) — so it is stated separately from the
/// process-facility question the allowlist answers.
fn procserv_host_platform(unix: bool, os: &str) -> bool {
    unix && PROCSERV_HOST_TARGETS.contains(&os)
}

fn main() {
    println!("cargo::rustc-check-cfg=cfg(procserv_host_platform)");

    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let unix = std::env::var_os("CARGO_CFG_UNIX").is_some();

    if procserv_host_platform(unix, &os) {
        println!("cargo::rustc-cfg=procserv_host_platform");
        return;
    }

    if unix {
        println!(
            "cargo::error=epics-tools-rs: this crate supervises a separate process and needs \
             fork(2), forkpty(3) and a controlling terminal; target_os = \"{os}\" is not a \
             platform where those were verified, so it is not built for this target — C ships \
             procServ as PROD_HOST for the same reason. Add the OS to PROCSERV_HOST_TARGETS in \
             crates/epics-tools-rs/build.rs once all three are confirmed there."
        );
    }
}
