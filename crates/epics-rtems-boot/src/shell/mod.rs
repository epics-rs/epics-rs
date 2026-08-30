//! The RTEMS operator commands EPICS base registers from `iocshRegisterRTEMS`.
//!
//! C base registers six iocsh commands from its RTEMS boot path —
//! `netstat`, `heapSpace`, `nfsMount`, `zoneset`, `rt` and `setlogmask`
//! (`libcom/RTEMS/posix/rtems_init.c:692-705` @R7.0.10) — and four from its
//! `score/` twin (`:523-528`, the same set without `rt` and `setlogmask`).
//! They are registered from the boot path rather than from a `*IocRegister.c`,
//! which is exactly why they belong to this crate: an IOC gets them because it
//! booted on RTEMS, not because it loaded a database.
//!
//! This module owns their *behaviour*. The iocsh surface — argument
//! descriptors, usage text, the registry entries — is
//! `epics_base_rs::server::iocsh::register_rtems_commands`, because a
//! `CommandDef` is that crate's type and this crate is one of its
//! dependencies. Nothing here is reachable from the hosted `softioc-rs`.
//!
//! # Why this was never caught
//!
//! `iocsh/commands.rs`' `ABSENT_DATABASE_COMMANDS` census is measured by
//! diffing `help` on a C `softIoc` against `softioc-rs`, and a Linux `softIoc`
//! never reaches `iocshRegisterRTEMS`. The census is true and this gap was
//! real at the same time; no measurement taken on a host can close it.
//!
//! # The five, and the sixth
//!
//! | command | where the work happens |
//! | --- | --- |
//! | `netstat` | [`netstat`] → libbsd's own `netstat`, see below |
//! | `heapSpace` | [`crate::stats::heap_space`], base's three `Stats` inputs |
//! | `zoneset` | [`zoneset`], pure libc, host-tested |
//! | `rt` | [`run_shell_command`] → `rtems_shell_lookup_cmd` |
//! | `setlogmask` | [`set_log_priority`], [`log_priority_names`] |
//!
//! `nfsMount` is the sixth and is NOT ported, because the API it names does
//! not exist on this stack. Base's posix arm includes `<librtemsNfs.h>` only
//! under `#ifdef RTEMS_LEGACY_STACK` (`rtems_init.c:60-62`) while calling
//! `nfsMount()` under `#ifndef OMIT_NFS_SUPPORT` (`:593-604`, `:696`), so a
//! base build on the libbsd stack either defines `OMIT_NFS_SUPPORT` — and has
//! no `nfsMount` command — or calls an undeclared function. And the API is
//! gone rather than moved: rtems-libbsd's `librtemsNfs.h` declares
//! `rpcUdpInit`, `nfsInit`, `nfsMountsShow` and `rtems_nfs_initialize`, and no
//! `nfsMount(char *, char *, char *)`, at both pins this workspace builds
//! against. Mounting NFS on libbsd goes through `mount()` with
//! `RTEMS_FILESYSTEM_TYPE_NFS`, which is a different command with a different
//! argument grammar, and `csrc/rtems_config.c` configures no NFS filesystem
//! for it to reach. A registered `nfsMount` that could only print a refusal
//! would be worse than its absence: `help` would list it.
//!
//! # `netstat` is the one deliberate behavioural deviation
//!
//! Base's `rtems_netstat` (`:531-547`) puts every reading inside
//! `#ifdef RTEMS_LEGACY_STACK`; on libbsd its whole body is
//! `printf("***** Sorry not implemented yet with the new network stack
//! (bsdlib)\n")`. So a C IOC on RTEMS 6/7 has a `netstat` command that reports
//! nothing at all. `csrc/rtems_shell_cmds.c` produces the readings base wanted
//! from libbsd's own `netstat`, mapping base's level ladder onto its flags.
//! See that file for the mapping.
//!
//! # The OS fork happens once
//!
//! Same shape as [`crate::stats`], for the same reason: one backend module
//! selected by `#[cfg]` below, and no `#[cfg]` anywhere else in this file —
//! `the_os_fork_happens_only_at_the_backend_selection` below holds that.
//! [`zoneset`] is in the funnel rather than in a backend because it calls no
//! RTEMS API: `setenv`/`unsetenv`/`tzset` is the whole of base's
//! implementation (`:611-627`), and putting it here is what makes it testable
//! on a machine that cannot boot the target.

// The one place this module knows what OS it is on — see the module docs.
#[cfg(all(target_os = "rtems", rtems_boot_linked))]
#[path = "rtems.rs"]
mod backend;
#[cfg(not(all(target_os = "rtems", rtems_boot_linked)))]
#[path = "unsupported.rs"]
mod backend;

use std::fmt;

/// Why a command could not do what it was asked.
///
/// One type for the module, one meaning per variant: a caller that wants to
/// print a diagnostic never has to infer which failure it is holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellError {
    /// This build has no RTEMS backend — a host build, or a portability build
    /// where no BSP prefix was set so the C shim was never compiled.
    ///
    /// Distinct from every other variant because it is a property of the
    /// *image*, not of the argument: nothing the operator types can fix it.
    Unsupported,
    /// `rtems_shell_lookup_cmd` found no command of that name in the set
    /// `csrc/rtems_config.c` configured. Base's `rtshellCallFunc` prints
    /// `ERR: No such command` here (`:511`).
    NoSuchCommand,
    /// The syslog level name is not in `prioritynames`. Base prints
    /// `Error: unknown log level.` here (`:684`).
    UnknownLevel,
    /// The argument cannot be handed to a C API — it contains an interior NUL,
    /// so no NUL-terminated form of it exists.
    ///
    /// The iocsh tokeniser cannot produce one, so this is unreachable from a
    /// typed line; it exists so the conversion has an answer other than a
    /// panic for a caller that built the string some other way.
    NotRepresentable,
}

impl fmt::Display for ShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("not available: this image has no RTEMS boot shim"),
            Self::NoSuchCommand => f.write_str("No such command"),
            Self::UnknownLevel => f.write_str("unknown log level."),
            Self::NotRepresentable => f.write_str("argument contains an embedded NUL"),
        }
    }
}

/// `netstat <level>` — base `netStatCallFunc` (`:548-551`).
///
/// Prints; the reading itself goes to the console, as base's does. Base has no
/// failure path here at all, so the only `Err` is [`ShellError::Unsupported`].
pub fn netstat(level: i32) -> Result<(), ShellError> {
    backend::netstat(level)
}

/// `rt <cmd> <args...>` — base `rtshellCallFunc` (`:506-524`).
///
/// `argv` is handed to the RTEMS shell command verbatim, so **`argv[0]` must
/// be the command name**: base passes `iocshArgArgv` from position 1, whose
/// `av[0]` is the token that named the command (`iocsh.cpp:1282-1285`), and
/// every shell command's own `getopt` starts at index 1. A caller that passed
/// the arguments alone would silently lose the first one.
///
/// Returns the command's exit status. A non-zero status is `Ok`, not `Err`:
/// the command ran and said so, which is a different fact from not being able
/// to run it.
pub fn run_shell_command(argv: &[String]) -> Result<i32, ShellError> {
    let Some(name) = argv.first() else {
        return Err(ShellError::NoSuchCommand);
    };
    backend::run_shell_command(name, argv)
}

/// `setlogmask <level>` — base `setlogmaskCallFunc` (`:660-686`).
pub fn set_log_priority(name: &str) -> Result<(), ShellError> {
    backend::set_log_priority(name)
}

/// The syslog level names, in the C library's own order — what base's
/// `setlogmask` with no argument lists (`:665-671`).
///
/// Empty on a build with no backend. That is the same claim
/// [`ShellError::Unsupported`] makes and it cannot be confused with "this
/// system has no levels", because the command that prints it is not registered
/// on such a build in the first place.
pub fn log_priority_names() -> Vec<String> {
    backend::log_priority_names()
}

/// `zoneset <zone string>` — base `zoneset` (`:611-637`).
///
/// Sets `TZ` and re-reads it, or clears `TZ` when `zone` is `None`. No RTEMS
/// API is involved: base's own body is `setenv`/`unsetenv` then `tzset`, so
/// this is the whole command and it runs — and is tested — on the host.
///
/// `tzset` is declared directly rather than through the `libc` crate, which
/// this package takes only on the embedded targets. It is POSIX and is defined
/// by the C library of every target this workspace builds, so the declaration
/// leaves no undefined symbol anywhere — unlike an `extern` for an RTEMS
/// symbol, which is why those live behind the backend.
///
/// # Safety
///
/// Calls [`std::env::set_var`] / [`std::env::remove_var`], which are unsound
/// while another thread may be reading the environment. C has the same hazard
/// and ships it: `zoneset` is an operator command typed into a running IOC.
/// The caller must be able to say that no other thread is touching the
/// environment for the duration.
pub unsafe fn zoneset(zone: Option<&str>) -> Result<(), ShellError> {
    unsafe extern "C" {
        fn tzset();
    }

    match zone {
        Some(zone) => {
            // `set_var` panics on an interior NUL rather than returning; C's
            // `setenv` would return -1. Refuse it with the module's own error
            // so the two agree that this is a failed command, not a crash.
            if zone.contains('\0') {
                return Err(ShellError::NotRepresentable);
            }
            // SAFETY: the caller's contract above.
            unsafe { std::env::set_var("TZ", zone) };
        }
        // SAFETY: as above. Base's newlib arm calls `unsetenv` and takes its
        // return; `remove_var` is that call and cannot report failure, which
        // matches base's pre-newlib-2.2.0 arm (`:620-625`) exactly.
        None => unsafe { std::env::remove_var("TZ") },
    }

    // SAFETY: `tzset` takes no argument and reads only the environment, which
    // the caller's contract has just made exclusive to this thread.
    unsafe { tzset() };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The funnel's own production lines, comments stripped — see
    /// [`crate::stats`]' twin of this helper for why both halves matter.
    fn funnel_code() -> impl Iterator<Item = &'static str> {
        source_guard::production(include_str!("mod.rs"), source_guard::Comments::Strip).lines()
    }

    /// A host build has no RTEMS shell, and every entry point must say so
    /// rather than pretend it did the work. A `netstat` that returned `Ok`
    /// having printed nothing is the shape that makes a boot log read like a
    /// working IOC.
    #[test]
    fn a_host_build_refuses_every_rtems_command_rather_than_faking_it() {
        assert_eq!(netstat(0), Err(ShellError::Unsupported));
        assert_eq!(netstat(2), Err(ShellError::Unsupported));
        assert_eq!(
            run_shell_command(&["stackuse".to_string()]),
            Err(ShellError::Unsupported)
        );
        assert_eq!(set_log_priority("debug"), Err(ShellError::Unsupported));
        assert!(log_priority_names().is_empty());
    }

    /// An empty argv is refused before the backend sees it: there is no name to
    /// look up, and `NoSuchCommand` is the answer C reaches for the same input
    /// (`rtems_shell_lookup_cmd(NULL)`'s caller prints `ERR: No such command`).
    #[test]
    fn an_empty_command_line_is_no_such_command_not_a_panic() {
        assert_eq!(run_shell_command(&[]), Err(ShellError::NoSuchCommand));
    }

    /// `zoneset` is the one command that works off-target, so it is the one
    /// whose behaviour can be held to base's line by line.
    ///
    /// Each assertion is a boundary of base's `zoneset` (`:611-627`): a zone
    /// sets `TZ`, a second zone replaces it (base passes `overwrite = 1`), and
    /// no zone unsets it.
    #[test]
    fn zoneset_sets_replaces_and_clears_tz() {
        // SAFETY: nextest runs each test in its own process, so this thread is
        // the only one that can be touching the environment.
        unsafe {
            zoneset(Some("UTC")).expect("a plain zone name is representable");
            assert_eq!(std::env::var("TZ").as_deref(), Ok("UTC"));

            zoneset(Some("EST5EDT")).expect("the second zone replaces the first");
            assert_eq!(
                std::env::var("TZ").as_deref(),
                Ok("EST5EDT"),
                "base passes overwrite = 1 to setenv, so a second zoneset wins"
            );

            zoneset(None).expect("no zone clears TZ");
            assert!(std::env::var("TZ").is_err(), "TZ is unset, not empty");
        }
    }

    /// An empty zone string is a zone, not an absent argument. C stores `""`
    /// for an empty token and passes it to `setenv`, which sets `TZ` to the
    /// empty string — UTC — rather than unsetting it. Only a missing argument
    /// takes the `unsetenv` arm.
    #[test]
    fn an_empty_zone_string_sets_tz_rather_than_clearing_it() {
        // SAFETY: as above.
        unsafe {
            zoneset(Some("")).expect("an empty zone is representable");
            assert_eq!(std::env::var("TZ").as_deref(), Ok(""));
        }
    }

    /// A string with an interior NUL has no NUL-terminated form, and
    /// `std::env::set_var` panics on one. An operator command must not be able
    /// to abort the IOC.
    #[test]
    fn an_embedded_nul_is_refused_rather_than_panicking() {
        // SAFETY: as above; this call must not reach `set_var` at all.
        let refused = unsafe { zoneset(Some("UTC\0EST")) };
        assert_eq!(refused, Err(ShellError::NotRepresentable));
    }

    /// Every variant prints something an operator can act on, and no two print
    /// the same thing — a `Display` that collapsed two failures would put the
    /// port back where a single error string had it.
    #[test]
    fn every_failure_has_its_own_message() {
        let messages: Vec<String> = [
            ShellError::Unsupported,
            ShellError::NoSuchCommand,
            ShellError::UnknownLevel,
            ShellError::NotRepresentable,
        ]
        .iter()
        .map(|e| e.to_string())
        .collect();
        for m in &messages {
            assert!(!m.is_empty());
        }
        let mut unique = messages.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), messages.len(), "two variants print the same");
    }

    /// The C is compiled on exactly one configuration, and the backend that
    /// declares its symbols must not outrun it — a declaration on a wider cfg
    /// leaves an undefined symbol in the toolchain-free portability build.
    #[test]
    fn the_rtems_backend_is_scoped_to_the_configuration_that_compiles_the_c() {
        let src = include_str!("mod.rs");
        let cfg = concat!("#[cfg(all(target_os = \"rtems\", ", "rtems_boot_linked))]");
        assert!(
            src.contains(&format!("{cfg}\n#[path = \"rtems.rs\"]\nmod backend;")),
            "the RTEMS backend must sit directly under the linked-image cfg"
        );
    }

    /// The commands' C must stay in the build script's file list and its change
    /// list; dropping either leaves the RTEMS backend pointing at nothing, or
    /// leaves a stale object behind after an edit.
    #[test]
    fn the_build_script_compiles_and_watches_the_command_shim() {
        let build = include_str!("../../build.rs");
        assert!(build.contains(".file(\"csrc/rtems_shell_cmds.c\")"));
        assert!(build.contains("cargo::rerun-if-changed=csrc/rtems_shell_cmds.c"));
    }

    /// The funnel's whole claim is that the OS fork happens once, at the
    /// `backend` selection. A `#[cfg]` anywhere else is a second fork — see
    /// [`crate::stats`], where the same guard exists for the same reason.
    #[test]
    fn the_os_fork_happens_only_at_the_backend_selection() {
        assert_eq!(
            funnel_code().filter(|l| l.contains("#[cfg(")).count(),
            funnel_code().filter(|l| l.contains("mod backend;")).count(),
            "every `#[cfg]` in the funnel must be a backend selection"
        );
    }

    /// Every backend file, paired with its contents — the census below is only
    /// as good as this list, so the list is checked against `mod.rs` too.
    const BACKENDS: &[(&str, &str)] = &[
        ("rtems.rs", include_str!("rtems.rs")),
        ("unsupported.rs", include_str!("unsupported.rs")),
    ];

    /// A `#[cfg]`ed-out backend is not compiled, so nothing about it is checked
    /// by building — and on this workspace the RTEMS arm builds only on the
    /// bring-up box. The surface is a census instead, in both directions.
    #[test]
    fn every_backend_implements_the_whole_funnel_surface() {
        let declared: Vec<&str> = funnel_code()
            .filter_map(|l| l.trim().strip_prefix("#[path = \""))
            .map(|r| &r[..r.find('"').expect("the path attribute is terminated")])
            .collect();
        let listed: Vec<&str> = BACKENDS.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            declared, listed,
            "every backend `mod.rs` selects must be listed in BACKENDS, in order"
        );

        let required: Vec<&str> = funnel_code()
            .filter_map(|l| l.trim().strip_prefix("backend::"))
            .map(|r| &r[..r.find('(').expect("a backend call is a call")])
            .collect();
        assert!(
            !required.is_empty(),
            "the funnel must delegate to the backend"
        );

        for (name, body) in BACKENDS {
            for f in &required {
                assert!(
                    body.contains(&format!("fn {f}(")),
                    "backend {name} does not implement {f}"
                );
            }
        }
    }
}
