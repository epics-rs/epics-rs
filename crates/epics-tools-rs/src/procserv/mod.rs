//! procServ — PTY-based process supervisor.
//!
//! See crate-level docs ([`crate`]) for architectural rationale.
//! This module is gated on `procserv_host_platform` — see `build.rs`,
//! which owns the decision — because it depends on `forkpty(3)`,
//! `execvp(3)` and a controlling terminal.

pub mod child;
pub mod client;
pub mod config;
pub mod console;
pub mod daemon;
pub mod endpoint;
pub mod error;
pub mod listener;
pub mod menu;
pub mod messages;
pub mod restart;
pub mod sidecar;
pub mod supervisor;
pub mod telnet;

pub use config::ProcServConfig;
pub use error::{ProcServError, ProcServResult};
pub use restart::{RestartMode, RestartPolicy};
pub use supervisor::ProcServ;

#[cfg(test)]
mod tests {
    /// The platform rule has one owner and it is an allowlist that refuses.
    ///
    /// Both halves matter and neither is visible from a host test run, which
    /// is why they are pinned from source rather than exercised:
    ///
    /// * **Allowlist, not exclusion list.** Naming `rtems`/`vxworks` in the
    ///   predicate answers "is this one of the two targets we know about",
    ///   and the next unix-family RTOS triple inherits the host arm — the
    ///   defect this gate replaced, which is how `console.rs` came to reach
    ///   `libc::termios` on a target whose binding for it is wrong.
    /// * **Refusal, not an empty crate.** Compiling `procserv` away on a
    ///   unix target that cannot run it would leave an embedded consumer an
    ///   empty API and imply the port is merely missing.
    ///
    /// Modelled on `epics-pva-rs`'s
    /// `the_capability_is_owned_by_an_allowlist_in_the_build_script`, which
    /// guards the same shape for `local_account_db`.
    #[test]
    fn the_platform_rule_is_an_allowlist_that_refuses() {
        // Comment lines are stripped: the build script's docs quote the
        // rejected `not(target_os = "rtems")` predicate in order to explain
        // why it is rejected. The guard is about what the code does.
        let build: String = include_str!("../../build.rs")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(build.contains("cargo::rustc-check-cfg=cfg(procserv_host_platform)"));
        assert!(build.contains("    \"linux\","));
        // `main` must not decide the capability itself — it asks the predicate.
        assert!(
            build.contains("if procserv_host_platform(unix, &os) {"),
            "the capability emission must go through `fn procserv_host_platform`"
        );
        // A unix target that fails the predicate must be refused, and refused
        // for every configuration: no feature selection of this crate builds
        // for such a target anyway (`tokio` is an unconditional dependency),
        // so a narrower condition would only swap this message for the
        // dependency's own errors.
        assert!(
            build.contains("cargo::error=epics-tools-rs:"),
            "a unix target off the allowlist must fail the build, not compile away"
        );
        assert!(
            build.contains("    if unix {\n") && !build.contains("CARGO_FEATURE_"),
            "the refusal must be unconditional on a unix target off the allowlist"
        );

        let decl = "fn procserv_host_platform(unix: bool, os: &str) -> bool {";
        let start = build
            .find(decl)
            .expect("the capability predicate must be a named function");
        let body = &build[start + decl.len()..];
        let body = &body[..body.find("\n}").expect("predicate body must close")];

        assert!(
            body.contains("PROCSERV_HOST_TARGETS.contains("),
            "the capability must be decided by consulting the allowlist"
        );
        // What separates an allowlist from an exclusion list: an exclusion
        // list has to NAME what it excludes. An allowlist never mentions it.
        for excluded in ["rtems", "vxworks"] {
            assert!(
                !body.contains(&format!("{excluded:?}")),
                "the predicate names {excluded}, so it is an exclusion list; \
                 a target that is not on the allowlist must simply not be on it"
            );
        }
    }
}
