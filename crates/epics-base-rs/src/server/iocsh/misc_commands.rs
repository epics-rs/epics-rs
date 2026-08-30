//! The `modules/database/src/ioc/misc` iocsh commands.
//!
//! C registers five names from `miscIocRegister.c:66-72` @R7.0.10:
//! `iocInit`, `iocBuild`, `iocRun`, `iocPause` and `coreRelease`, plus
//! `system` from the `iocshSystemCommand` registrar that `softIoc.dbd`
//! pulls in (`modules/database/src/std/softIoc/Makefile:21`
//! `softIoc_DBD += system.dbd`), so a stock `softIoc` does have it.
//! `dlload` is a seventh name from the same directory: its own file
//! `dlload.c` registers it through `epicsExportRegistrar`, which
//! `softIoc.dbd` also pulls in.
//!
//! `iocInit` is registered by [`super::commands`]. `iocRun` and
//! `iocPause` are registered here and drive the real transitions —
//! [`crate::server::ioc_app::ioc_run`] and
//! [`crate::server::ioc_app::ioc_pause`] over the same `iocState` cell
//! C keeps (`iocInit.h:17-19`).
//!
//! All five are registered. `iocBuild` is the last to arrive: it needs a
//! point at which the IOC is built and quiescent, which the lifecycle
//! owner only grew when it was split the way C splits it —
//! `iocInit() = iocBuild() || iocRun()` (`iocInit.c:111-113`).
//! `IocBuild::perform_build` (`server::ioc_app`) stops at
//! `IocState::Built`, exactly where `iocBuild_3` does (`:201-207`), and
//! `BuiltIoc::run` is the transition out of it. So the three commands
//! here are three views of one owner: `iocBuild` takes the first half,
//! `iocRun` the second, `iocInit` both.

use super::registry::*;

/// Register the `miscIocRegister.c` command set on `registry`.
pub(crate) fn register(registry: &mut CommandRegistry) {
    registry.register(cmd_ioc_build());
    registry.register(cmd_ioc_run());
    registry.register(cmd_ioc_pause());
    registry.register(cmd_core_release());
    registry.register(cmd_system());
    registry.register(cmd_dlload());
}

/// `iocBuild` — C `iocBuildCallFunc` (`miscIocRegister.c:36-39`),
/// `iocshSetError(iocBuild())`.
///
/// Build the IOC and stop, leaving it in C's `iocBuilt` state: device support
/// wired, PINI=YES processed, access security loaded, scanning still paused and
/// no server running. The lines between this and `iocRun` are the reason the
/// command exists — they see a database that is fully initialised and not yet
/// processing.
fn cmd_ioc_build() -> CommandDef {
    CommandDef::new(
        "iocBuild",
        vec![],
        "iocBuild — Initialize the IOC and leave it in a quiescent state.",
        |_args: &[ArgValue], ctx: &CommandContext| {
            use crate::server::ioc_app::ShellTransition;
            match crate::server::ioc_app::build_from_shell(ctx.bridge()) {
                ShellTransition::Done => Ok(CommandOutcome::Continue),
                ShellTransition::Failed => Ok(CommandOutcome::Failed),
                // C `iocBuild_1` (`iocInit.c:116-121`) refuses from any state
                // but `iocVoid`.
                ShellTransition::Refused => {
                    crate::runtime::log::errlog_printf(&crate::server::ioc_app::build_refusal());
                    Ok(CommandOutcome::Failed)
                }
                // No `IocApplication` lifecycle to drive — a bare
                // `PvDatabase` shell or a `CaServerBuilder` binary. C runs
                // `iocBuild_1` for those too, so this arm does, and the
                // record-load close is this shell's contribution to it.
                ShellTransition::NotOurs => {
                    if crate::server::ioc_app::build_without_application(|| {
                        ctx.block_on(async { ctx.db().ioc_init().await });
                    }) {
                        Ok(CommandOutcome::Continue)
                    } else {
                        Ok(CommandOutcome::Failed)
                    }
                }
            }
        },
    )
}

/// `iocRun` — C `iocRunCallFunc` (`miscIocRegister.c:42-45`),
/// `iocshSetError(iocRun())`.
///
/// C's help text, verbatim in substance: bring the IOC out of its
/// quiescent state to the running state.
fn cmd_ioc_run() -> CommandDef {
    CommandDef::new(
        "iocRun",
        vec![],
        "iocRun — Bring the IOC out of its initial quiescent state to the \
         running state.",
        |_args: &[ArgValue], ctx: &CommandContext| {
            use crate::server::ioc_app::ShellTransition;
            // Two ways to reach `iocRunning`, and C has both: out of
            // `iocBuilt`, which is the IOC this `run` built and still owns,
            // and out of `iocPaused`, which owns nothing because the build
            // already happened. `Refused`/`NotOurs` is the second case — no
            // built IOC to consume — so the plain transition is what runs,
            // and it is also the only one a `CaServerBuilder` IOC has.
            match crate::server::ioc_app::run_from_shell(ctx.bridge()) {
                ShellTransition::Done => return Ok(CommandOutcome::Continue),
                ShellTransition::Failed => return Ok(CommandOutcome::Failed),
                ShellTransition::Refused | ShellTransition::NotOurs => {}
            }
            // C `iocshSetError(iocRun())`: a non-zero status fails the
            // line, and `iocRun` has already printed its own diagnostic.
            if crate::server::ioc_app::ioc_run() == 0 {
                Ok(CommandOutcome::Continue)
            } else {
                Ok(CommandOutcome::Failed)
            }
        },
    )
}

/// `iocPause` — C `iocPauseCallFunc` (`miscIocRegister.c:51-54`),
/// `iocshSetError(iocPause())`.
fn cmd_ioc_pause() -> CommandDef {
    CommandDef::new(
        "iocPause",
        vec![],
        "iocPause — Bring a running IOC to a quiescent state with record \
         processing frozen.",
        |_args: &[ArgValue], _ctx: &CommandContext| {
            if crate::server::ioc_app::ioc_pause() == 0 {
                Ok(CommandOutcome::Continue)
            } else {
                Ok(CommandOutcome::Failed)
            }
        },
    )
}

/// `coreRelease` — C `coreRelease()` (`misc/epicsRelease.c:21-29`),
/// a five-line banner around `epicsReleaseVersion` and the two
/// `epicsVCS.h` macros.
///
/// C generates that header per build with `genVersionHeader.pl`, and it
/// has two arms with no empty outcome between them: a VCS checkout gives
/// `git describe --always --tags --dirty --abbrev=20` for the version and
/// `Git: ` + `git show -s --format=%ci HEAD` for the date (`:88-101`), and
/// no VCS directory gives `build date/time` plus the build timestamp
/// (`:130-139`, the arm reached because `RULES_BUILD:453` passes an empty
/// `-V`). This crate's `build.rs` stamps the same pair the same two ways.
fn cmd_core_release() -> CommandDef {
    CommandDef::new(
        "coreRelease",
        vec![],
        "coreRelease — Print release information for iocCore.",
        |_args: &[ArgValue], ctx: &CommandContext| {
            for line in core_release_block() {
                ctx.println(&line);
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// The five lines themselves, so the shell command and the build print one
/// text.
///
/// C has one `coreRelease()` and calls it from both places — the iocsh
/// command and `iocBuild_1` (`iocInit.c:147`) — and the port had the wording
/// only inside the command, which is why the build could not print it
/// without a second copy. The command still renders through the iocsh
/// context so a redirection applies to it; the build writes stdout, as C's
/// `printf` does.
pub(crate) fn core_release_block() -> [String; 5] {
    let rule = "#".repeat(76);
    [
        rule.clone(),
        format!("## {}", crate::runtime::version::EPICS_RELEASE_VERSION),
        format!("## Rev. {VCS_VERSION}"),
        format!("## Rev. Date {VCS_VERSION_DATE}"),
        rule,
    ]
}

/// C `EPICS_VCS_VERSION` and `EPICS_VCS_VERSION_DATE`, stamped by this
/// crate's `build.rs` exactly as C stamps `epicsVCS.h`, and read here with
/// `env!` as C's banner reads the macros.
const VCS_VERSION: &str = env!("EPICS_VCS_VERSION");
const VCS_VERSION_DATE: &str = env!("EPICS_VCS_VERSION_DATE");
/// Neither field may render as a bare label. Asserted at the reader as well
/// as at the writer because the banner is no longer something someone has to
/// type: it prints on every IOC start.
const _: () = assert!(!VCS_VERSION.is_empty() && !VCS_VERSION_DATE.is_empty());

/// `dlload <path/library.so>` — C `dlloadCallFunc`
/// (`ioc/misc/dlload.c:32-35`), `iocshSetError(dlload(args[0].sval))`.
///
/// C's body is four lines: `epicsLoadLibrary(name)`, and on false
/// `printf("epicsLoadLibrary failed: %s\n", epicsLoadError())` then
/// return -1. This port has no dynamic loader — `epicsLoadLibrary`,
/// `epicsFindSymbol` and `dlopen` appear nowhere in `crates/` — so the
/// load can never succeed and the command takes C's failure path on
/// every input.
///
/// That is the honest implementation rather than the absent one. The
/// alternative considered and rejected was a real `dlopen`: it would
/// load the library and run its constructors, but a shared library
/// registers record types, device support and iocsh commands by
/// calling into C's global registries through `epicsExportRegistrar`,
/// and this port's registries are Rust-side and populated at compile
/// time with no C-ABI door into them. `dlopen` would therefore report
/// success having changed nothing — a command that lies. Reporting
/// C's own failure line reports the truth, and the reason travels in
/// the message where the operator reads it.
fn cmd_dlload() -> CommandDef {
    CommandDef::new(
        "dlload",
        vec![ArgDesc {
            // C `dlloadArg0` (`dlload.c:22`), `iocshArgStringPath`.
            name: "path/library.so",
            arg_type: ArgType::Path,
        }],
        "dlload <path/library.so> — Load the given shared library. \
         Example: dlload myLibrary.so",
        |args: &[ArgValue], ctx: &CommandContext| {
            // A bare `dlload` is C's `dlopen(NULL, ...)`
            // (`osi/os/posix/osdFindSymbol.c:21-24`), which hands back a handle
            // on the running program and loads nothing, so C's `dlload` returns
            // 0 and the line succeeds with no output at all — measured. The
            // program this asks for is already loaded here too, so reporting
            // that success is accurate rather than the lie the loaded-library
            // case would be.
            let ArgValue::String(name) = &args[0] else {
                return Ok(CommandOutcome::Continue);
            };
            // C's line verbatim; the text after the colon is what
            // `epicsLoadError()` would carry, which here is a fixed
            // property of the binary rather than a per-call `dlerror`.
            ctx.println(&format!(
                "epicsLoadLibrary failed: {name}: this IOC is statically \
                 linked and has no dynamic loader, and a shared library has \
                 no registrar path into its compile-time registries"
            ));
            Ok(CommandOutcome::Failed)
        },
    )
}

/// `system <command string>` — C `systemCallFunc`
/// (`miscIocRegister.c:91-94`), `iocshSetError(system(args[0].sval))`.
///
/// C's `system()` returns the wait status, so a non-zero exit makes
/// the iocsh line fail without any diagnostic of its own — that pair
/// (failed, nothing printed) is [`CommandOutcome::Failed`]. A shell
/// that cannot be spawned at all is C's `system()` returning -1, which
/// is also just a failed line.
fn cmd_system() -> CommandDef {
    CommandDef::new(
        "system",
        vec![ArgDesc {
            // C `systemArg0` (`miscIocRegister.c:84`).
            name: "command string",
            arg_type: ArgType::String,
        }],
        "system <command string> — Send command string to the system \
         command interpreter for execution.",
        |args: &[ArgValue], _ctx: &CommandContext| {
            let cmd = match &args[0] {
                ArgValue::String(s) => s.as_str(),
                _ => return Ok(CommandOutcome::Failed),
            };
            Ok(run_system(cmd))
        },
    )
}

/// The one place the shell escapes to the system command interpreter.
/// C's `system()` hands the string to `/bin/sh -c` on POSIX and to
/// `cmd.exe /C` on Windows, so the argument keeps its shell grammar —
/// `system "ls | wc -l"` is a pipeline, not an argv.
fn run_system(cmd: &str) -> CommandOutcome {
    #[cfg(windows)]
    let mut c = {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    };
    #[cfg(not(windows))]
    let mut c = {
        let mut c = std::process::Command::new("/bin/sh");
        c.arg("-c").arg(cmd);
        c
    };
    match c.status() {
        Ok(st) if st.success() => CommandOutcome::Continue,
        _ => CommandOutcome::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::database::PvDatabase;
    use std::sync::Arc;

    fn make_ctx() -> CommandContext {
        // RTEMS-EXEC-MODEL-ALLOW(1): the runtime is built here only to capture
        // a `BlockingBridge`; the commands under test then run synchronously,
        // so this site does not need the ambient reactor the feature withholds
        // and the tests pass in the exec-backend suite.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(PvDatabase::new());
        let bridge = {
            let _guard = rt.enter();
            crate::runtime::task::BlockingBridge::capture()
        };
        let ctx = CommandContext::new(db, bridge);
        std::mem::forget(rt);
        ctx
    }

    fn run(ctx: &CommandContext, name: &str, tokens: &[&str]) -> (String, bool) {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg.get(name).unwrap();
        let tokens: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
        let args = parse_args(&tokens, &cmd.args).unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let outcome = ctx.with_output(std::fs::File::create(&path).unwrap(), || {
            cmd.handler.call(&args, ctx)
        });
        let failed = matches!(outcome, Ok(CommandOutcome::Failed));
        (std::fs::read_to_string(&path).unwrap(), failed)
    }

    /// `dlload` is registered, takes C's one path argument, and reports
    /// C's failure line with a failed status — C's `dlload` returning -1
    /// through `iocshSetError` (`dlload.c:32-35`). The two halves are one
    /// test because the command has exactly one path: there is no success
    /// branch to separate them.
    #[test]
    fn dlload_takes_cs_path_argument_and_reports_cs_failure_line() {
        let ctx = make_ctx();
        {
            let mut reg = CommandRegistry::new();
            register(&mut reg);
            let def = reg.get("dlload").unwrap();
            assert_eq!(def.args.len(), 1);
            assert_eq!(def.args[0].name, "path/library.so");
            assert!(matches!(def.args[0].arg_type, ArgType::Path));
        }
        let (out, failed) = run(&ctx, "dlload", &["myLibrary.so"]);
        assert!(failed, "C returns -1, so the shell line fails");
        assert!(
            out.starts_with("epicsLoadLibrary failed: myLibrary.so"),
            "C's own prefix and the name it was given, got: {out:?}"
        );
        assert!(
            out.contains("statically"),
            "the reason travels in the message, got: {out:?}"
        );
    }

    /// The two commands C registers from `miscIocRegister.c:69-70` exist,
    /// take no arguments, and carry `iocshSetError`'s status through to the
    /// shell: refused from the wrong state, accepted from the right one.
    #[test]
    fn ioc_run_and_ioc_pause_carry_the_transition_status() {
        use crate::server::ioc_app::{IocState, get_ioc_state, note_scan_owner_started};

        let ctx = make_ctx();
        {
            let mut reg = CommandRegistry::new();
            register(&mut reg);
            assert!(reg.get("iocRun").unwrap().args.is_empty());
            assert!(reg.get("iocPause").unwrap().args.is_empty());
        }

        // iocVoid: C refuses both, and `iocshSetError` fails the line.
        assert!(run(&ctx, "iocRun", &[]).1, "iocRun from iocVoid fails");
        assert!(run(&ctx, "iocPause", &[]).1, "iocPause from iocVoid fails");

        note_scan_owner_started();
        assert!(!run(&ctx, "iocPause", &[]).1, "a running IOC pauses");
        assert_eq!(get_ioc_state(), IocState::Paused);
        assert!(!run(&ctx, "iocRun", &[]).1, "a paused IOC runs again");
        assert_eq!(get_ioc_state(), IocState::Running);
    }

    /// C `coreRelease` prints exactly five lines: a 76-`#` rule, three
    /// `## ` lines, and the rule again (`misc/epicsRelease.c:23-27`).
    #[test]
    fn core_release_prints_c_s_five_line_banner() {
        let ctx = make_ctx();
        let (out, failed) = run(&ctx, "coreRelease", &[]);
        assert!(!failed, "coreRelease never fails in C");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5, "C prints five lines: {out:?}");
        assert_eq!(lines[0], "#".repeat(76));
        assert_eq!(lines[4], "#".repeat(76));
        assert_eq!(
            lines[1],
            format!("## {}", crate::runtime::version::EPICS_RELEASE_VERSION)
        );
        assert_eq!(lines[2], format!("## Rev. {VCS_VERSION}"));
        assert_eq!(lines[3], format!("## Rev. Date {VCS_VERSION_DATE}"));
        // The stamp reaches the line: neither renders as a bare label the
        // way `## Rev. Date ` did before `build.rs` filled the pair.
        assert!(lines[2].len() > "## Rev. ".len(), "empty revision: {out:?}");
        assert!(
            lines[3].len() > "## Rev. Date ".len(),
            "empty date: {out:?}"
        );
    }

    /// C declares one `iocshArgString`, and the whole string reaches the
    /// interpreter with its shell grammar intact.
    #[test]
    #[cfg(unix)]
    fn system_runs_a_shell_pipeline_and_reports_the_exit_status() {
        let ctx = make_ctx();
        {
            let mut reg = CommandRegistry::new();
            register(&mut reg);
            assert_eq!(
                reg.get("system").unwrap().args.len(),
                1,
                "C declares one argument"
            );
        }

        // A pipeline is one string to `sh -c`, not two argv words, and a
        // non-zero exit fails the line with no diagnostic of its own.
        let (out, failed) = run(&ctx, "system", &["true | true"]);
        assert!(!failed, "a 0 exit is a clean line");
        assert!(out.is_empty(), "C prints nothing on success: {out:?}");
        let (out, failed) = run(&ctx, "system", &["exit 3"]);
        assert!(failed, "a non-zero exit must fail the line");
        assert!(out.is_empty(), "C prints no diagnostic of its own: {out:?}");
    }
}
