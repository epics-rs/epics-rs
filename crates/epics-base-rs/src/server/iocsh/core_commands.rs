// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that hand-builds its own tokio runtime; runs and passes in the feature-ON suite.
//! Core iocsh commands beyond the database/record family.
//!
//! a stock `st.cmd` relies on a set of core commands
//! (`iocsh.cpp` / `libComRegister.c`) that the Rust port did not
//! register, so an unmodified startup script errored on the first
//! unknown command. This module registers the ones that are
//! implementable in-process; `iocshCmd` / `iocshRun` / `on` are
//! handled directly in `IocShell::execute_line` because they need
//! the shell itself, not just a `CommandContext`.

use super::registry::*;

/// Register the core iocsh command set on `registry`.
pub(crate) fn register(registry: &mut CommandRegistry) {
    registry.register(cmd_comment());
    registry.register(cmd_echo());
    registry.register(cmd_date());
    registry.register(cmd_chdir());
    registry.register(cmd_cd());
    registry.register(cmd_pwd());
    registry.register(cmd_epics_env_unset());
    registry.register(cmd_epics_env_show());
    registry.register(cmd_epics_prt_env_params());
    registry.register(cmd_epics_param_show());
    registry.register(cmd_epics_thread_sleep());
    registry.register(cmd_install_last_resort_event_provider());
    // C `libComRegister.c:504` calls `updatePWD()` right after
    // registering `cd`/`pwd`, so `$(PWD)` is already correct on a
    // fresh shell that has not run a `cd`.
    update_pwd();
}

/// The single writer of `PWD` — C `updatePWD` (`libComRegister.c:36-43`),
/// which is `epicsEnvSet("PWD", getcwd(...))` and therefore also clears
/// a shell macro shadowing the name.
fn update_pwd() {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    super::iocsh_env_clear("PWD");
    // SAFETY: same single-threaded-shell rationale as `epicsEnvSet`.
    unsafe { std::env::set_var("PWD", cwd) };
}

/// The only way the shell changes its working directory. Routing every
/// caller through here is what keeps `$(PWD)` equal to the process cwd
/// by construction rather than by each command remembering to update it.
pub(super) fn set_working_dir(dir: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    std::env::set_current_dir(dir)?;
    update_pwd();
    Ok(())
}

/// `installLastResortEventProvider` — C `libComRegister.c:483-489`,
/// a zero-argument command wrapping
/// `installLastResortEventProvider()` (`epicsGeneralTime.c:521-525`).
///
/// It is the only way an event stamp falls back to the wall clock, and C
/// exposes it as an operator opt-in rather than an init step: without it
/// a `TSE` no provider serves leaves `prec->time` alone and
/// `recGblGetTimeStampSimm` errlogs. Registering the command is what
/// makes that opt-in reachable at all.
fn cmd_install_last_resort_event_provider() -> CommandDef {
    CommandDef::new(
        "installLastResortEventProvider",
        vec![],
        "installLastResortEventProvider — Install the Last Resort event \
         provider at priority 999, which returns the current time for every \
         event number.",
        |_args: &[ArgValue], _ctx: &CommandContext| {
            crate::runtime::general_time::install_last_resort_event_provider();
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `#` — the comment command. C `iocsh.cpp` registers `#` so it
/// appears in `help`; a line starting with `#` is already treated as
/// a comment by `execute_line`, but a script may invoke it as a
/// no-output command.
fn cmd_comment() -> CommandDef {
    CommandDef::new(
        "#",
        vec![ArgDesc {
            name: "text",
            arg_type: ArgType::String,
            optional: true,
        }],
        "# [text] — Comment; ignores its arguments.",
        |_args: &[ArgValue], _ctx: &CommandContext| Ok(CommandOutcome::Continue),
    )
}

/// `echo [text]` — print the argument, with its escape sequences translated.
/// C `libComRegister.c:84-91` runs `dbTranslateEscape(str, str)` before the
/// `printf`, so `echo "a\tb"` prints a real tab — the same translation the
/// `.db` loader owes its field values (R18-91), through the same owner.
fn cmd_echo() -> CommandDef {
    CommandDef::new(
        "echo",
        vec![ArgDesc {
            name: "text",
            arg_type: ArgType::String,
            optional: true,
        }],
        "echo [text] — Print text to the console.",
        |args: &[ArgValue], ctx: &CommandContext| {
            match &args[0] {
                ArgValue::String(s) => {
                    ctx.println_bytes(&crate::runtime::epics_string::raw_from_escaped(s))
                }
                _ => ctx.println(""),
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `date` — print the current local date and time. Mirrors C `date`.
/// Translate an `epicsTimeToStrftime` format into chrono's. The two
/// agree except on the fractional-second conversions EPICS adds on top
/// of the C library's (`epicsTime.cpp`): `%f` is nanoseconds and `%0Nf`
/// is N digits, which chrono spells `%9f` and `%Nf`.
fn epics_strftime_to_chrono(fmt: &str) -> String {
    let c: Vec<char> = fmt.chars().collect();
    let mut out = String::with_capacity(fmt.len());
    let mut i = 0;
    while i < c.len() {
        if c[i] != '%' {
            out.push(c[i]);
            i += 1;
            continue;
        }
        let mut j = i + 1;
        if j < c.len() && c[j] == '%' {
            out.push_str("%%");
            i = j + 1;
            continue;
        }
        if j < c.len() && c[j] == '0' {
            j += 1;
        }
        let digits_at = j;
        while j < c.len() && c[j].is_ascii_digit() {
            j += 1;
        }
        if j < c.len() && c[j] == 'f' {
            let digits: String = c[digits_at..j].iter().collect();
            let n = digits.parse::<usize>().unwrap_or(9).clamp(1, 9);
            out.push_str(&format!("%{n}f"));
            i = j + 1;
            continue;
        }
        out.push('%');
        i += 1;
    }
    out
}

/// `date [format]` — C `date` (`libComRegister.c:59-72`) takes an
/// optional `strftime` format and falls back to
/// `"%Y/%m/%d %H:%M:%S.%06f"`; the port declared no argument at all, so
/// a format was silently discarded and the default did not match.
fn cmd_date() -> CommandDef {
    CommandDef::new(
        "date",
        vec![ArgDesc {
            name: "format",
            arg_type: ArgType::String,
            optional: true,
        }],
        "date [format] — Print the current date and time.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let fmt = match &args[0] {
                ArgValue::String(f) if !f.is_empty() => f.as_str(),
                _ => "%Y/%m/%d %H:%M:%S.%06f",
            };
            let now = chrono::Local::now();
            ctx.println(&now.format(&epics_strftime_to_chrono(fmt)).to_string());
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `chdir <dir>` — change the working directory. Mirrors C `chdir`.
fn cmd_chdir() -> CommandDef {
    CommandDef::new(
        "chdir",
        vec![ArgDesc {
            name: "dir",
            arg_type: ArgType::String,
            optional: false,
        }],
        "chdir <dir> — Change the current working directory.",
        chdir_handler,
    )
}

/// `cd <dir>` — alias of `chdir` (the spelling many `st.cmd` use).
fn cmd_cd() -> CommandDef {
    CommandDef::new(
        "cd",
        vec![ArgDesc {
            name: "dir",
            arg_type: ArgType::String,
            optional: false,
        }],
        "cd <dir> — Change the current working directory.",
        chdir_handler,
    )
}

/// C `chdirCallFunc` (`libComRegister.c:104-116`) prints nothing on
/// success — it only calls `updatePWD()`. The cwd line the port used to
/// echo here has no counterpart in C.
fn chdir_handler(args: &[ArgValue], _ctx: &CommandContext) -> CommandResult {
    let dir = match &args[0] {
        ArgValue::String(s) => s,
        _ => return Err("chdir: missing directory".into()),
    };
    set_working_dir(dir).map_err(|e| format!("chdir: {dir}: {e}"))?;
    Ok(CommandOutcome::Continue)
}

/// `pwd` — print the current working directory. Mirrors C `pwd`.
fn cmd_pwd() -> CommandDef {
    CommandDef::new(
        "pwd",
        vec![],
        "pwd — Print the current working directory.",
        |_args: &[ArgValue], ctx: &CommandContext| {
            match std::env::current_dir() {
                Ok(cwd) => ctx.println(&cwd.display().to_string()),
                Err(e) => return Err(format!("pwd: {e}")),
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `epicsEnvUnset <name>` — remove an environment variable. Mirrors
/// C `epicsEnvUnset`.
fn cmd_epics_env_unset() -> CommandDef {
    CommandDef::new(
        "epicsEnvUnset",
        vec![ArgDesc {
            name: "name",
            arg_type: ArgType::String,
            optional: false,
        }],
        "epicsEnvUnset <name> — Remove an environment variable.",
        |args: &[ArgValue], _ctx: &CommandContext| {
            let name = match &args[0] {
                ArgValue::String(s) => s,
                _ => return Err("epicsEnvUnset: missing name".into()),
            };
            // C `epicsEnvUnset` (`osdEnv.c:58-63`) clears the shell
            // macro first, exactly as `epicsEnvSet` does — otherwise an
            // unset variable stays readable through the macro that was
            // shadowing it.
            super::iocsh_env_clear(name);
            // SAFETY: matches C iocsh behaviour; mutated only from the
            // single-threaded REPL thread.
            unsafe { std::env::remove_var(name) };
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `epicsEnvShow [name]` — show one or all environment variables.
///
/// C (`osdEnv.c:66-77`) walks `environ` and prints every entry whose
/// NAME half `epicsStrnGlobMatch`es the argument, so the argument is a
/// PATTERN and not a key: `epicsEnvShow "EPICS_CA_*"` is the documented
/// use. An entry that matches nothing prints nothing — C has no
/// "is not set" line — and the listing keeps `environ` order rather
/// than being sorted.
fn cmd_epics_env_show() -> CommandDef {
    CommandDef::new(
        "epicsEnvShow",
        vec![ArgDesc {
            name: "[name]",
            arg_type: ArgType::String,
            optional: true,
        }],
        "epicsEnvShow [name] — Show environment variables matching a pattern.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let pattern = match &args[0] {
                ArgValue::String(p) => Some(p.as_str()),
                _ => None,
            };
            for (k, v) in std::env::vars() {
                if let Some(pattern) = pattern
                    && !super::commands::epics_strn_glob_match(
                        k.as_bytes(),
                        k.len(),
                        pattern.as_bytes(),
                    )
                {
                    continue;
                }
                ctx.println(&format!("{k}={v}"));
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// C `epicsPrtEnvParams` (`envSubr.c:383-392`): walk `env_param_list[]` and
/// print each parameter's *effective* value — the environment string, else the
/// compiled default, else "is undefined". Shared by `epicsPrtEnvParams` and
/// `epicsParamShow` (C registers both names for the same report).
///
/// This is NOT a filtered dump of the process environment. On a clean shell C
/// prints all 34 parameters with their compiled defaults; a `std::env::vars()`
/// scan prints nothing, and can never reach the `IOCSH_*` trio at all.
fn print_epics_params(ctx: &CommandContext) {
    for line in crate::runtime::env::prt_env_params() {
        ctx.println(&line);
    }
}

/// `epicsPrtEnvParams` — print the EPICS environment parameters.
fn cmd_epics_prt_env_params() -> CommandDef {
    CommandDef::new(
        "epicsPrtEnvParams",
        vec![],
        "epicsPrtEnvParams — Print the EPICS environment parameters.",
        |_args: &[ArgValue], ctx: &CommandContext| {
            print_epics_params(ctx);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `epicsParamShow` — alias of `epicsPrtEnvParams`.
fn cmd_epics_param_show() -> CommandDef {
    CommandDef::new(
        "epicsParamShow",
        vec![],
        "epicsParamShow — Print the EPICS environment parameters.",
        |_args: &[ArgValue], ctx: &CommandContext| {
            print_epics_params(ctx);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `epicsThreadSleep <seconds>` — block the shell for `seconds`.
/// Mirrors C `epicsThreadSleep`.
fn cmd_epics_thread_sleep() -> CommandDef {
    CommandDef::new(
        "epicsThreadSleep",
        vec![ArgDesc {
            name: "seconds",
            arg_type: ArgType::Double,
            optional: false,
        }],
        "epicsThreadSleep <seconds> — Sleep for the given number of seconds.",
        |args: &[ArgValue], _ctx: &CommandContext| {
            let secs = match &args[0] {
                ArgValue::Double(d) => *d,
                ArgValue::Int(n) => *n as f64,
                _ => return Err("epicsThreadSleep: missing seconds".into()),
            };
            if secs > 0.0 {
                std::thread::sleep(std::time::Duration::from_secs_f64(secs));
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::database::PvDatabase;
    use std::sync::Arc;

    fn make_ctx() -> CommandContext {
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

    fn run_env_show(ctx: &CommandContext, tokens: &[&str]) -> String {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg.get("epicsEnvShow").unwrap();
        let tokens: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
        let args = parse_args(&tokens, &cmd.args).unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        ctx.with_output(std::fs::File::create(&path).unwrap(), || {
            cmd.handler.call(&args, ctx).unwrap();
        });
        std::fs::read_to_string(&path).unwrap()
    }

    /// C `epicsEnvShow` (`osdEnv.c:66-77`): the argument is an
    /// `epicsStrnGlobMatch` pattern over the NAME half, and an entry
    /// that matches nothing prints nothing at all.
    #[test]
    #[serial_test::serial(epics_env)]
    fn epics_env_show_glob_matches_and_is_silent_on_no_match() {
        let ctx = make_ctx();
        // SAFETY: single-threaded under the `epics_env` serial group.
        unsafe {
            std::env::set_var("EPICS_RS_SHOW_ONE", "1");
            std::env::set_var("EPICS_RS_SHOW_TWO", "2");
        }

        let globbed = run_env_show(&ctx, &["EPICS_RS_SHOW_*"]);
        let mut lines: Vec<&str> = globbed.lines().collect();
        lines.sort();
        assert_eq!(lines, ["EPICS_RS_SHOW_ONE=1", "EPICS_RS_SHOW_TWO=2"]);

        assert_eq!(
            run_env_show(&ctx, &["EPICS_RS_SHOW_ONE"]),
            "EPICS_RS_SHOW_ONE=1\n"
        );
        // No match, no output — C invents no "is not set" line.
        assert_eq!(run_env_show(&ctx, &["EPICS_RS_NO_SUCH_VAR"]), "");

        // SAFETY: same serial group.
        unsafe {
            std::env::remove_var("EPICS_RS_SHOW_ONE");
            std::env::remove_var("EPICS_RS_SHOW_TWO");
        }
    }

    /// C `date` takes a format (`libComRegister.c:74`,
    /// `dateArg0 = {"format", iocshArgString}`) and defaults to
    /// `%Y/%m/%d %H:%M:%S.%06f`.
    #[test]
    fn date_takes_a_format_argument() {
        let ctx = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg.get("date").unwrap();
        assert_eq!(cmd.args.len(), 1, "C declares one argument");

        let args = parse_args(&["%Y".to_string()], &cmd.args).unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        ctx.with_output(std::fs::File::create(&path).unwrap(), || {
            cmd.handler.call(&args, &ctx).unwrap();
        });
        let out = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            out.trim().len(),
            4,
            "a %Y-only format prints just a year: {out:?}"
        );

        // The default carries the EPICS `%06f` fraction, which chrono
        // spells `%6f`.
        assert_eq!(
            epics_strftime_to_chrono("%Y/%m/%d %H:%M:%S.%06f"),
            "%Y/%m/%d %H:%M:%S.%6f"
        );
        assert_eq!(epics_strftime_to_chrono("%f"), "%9f");
        assert_eq!(epics_strftime_to_chrono("%d%%%H"), "%d%%%H");
    }

    #[test]
    fn echo_and_pwd_run() {
        let ctx = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        for name in ["echo", "pwd", "date", "#"] {
            let cmd = reg.get(name).unwrap();
            let args = parse_args(&[], &cmd.args).unwrap();
            assert!(
                cmd.handler.call(&args, &ctx).is_ok(),
                "{name} must run cleanly"
            );
        }
    }

    /// C `chdirCallFunc` (`libComRegister.c:104-116`) prints nothing on
    /// success and calls `updatePWD()`, and `libComRegister.c:504` calls
    /// `updatePWD()` at registration, so `$(PWD)` is already right on a
    /// shell that has not run a `cd` yet.
    #[test]
    #[serial_test::serial(epics_env)]
    fn cd_updates_pwd_and_prints_nothing() {
        use std::path::PathBuf;

        let start = std::env::current_dir().unwrap();
        let ctx = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().canonicalize().unwrap();
        // SAFETY: single-threaded under the `epics_env` serial group.
        unsafe { std::env::remove_var("PWD") };

        let mut reg = CommandRegistry::new();
        register(&mut reg);
        assert_eq!(
            std::env::var("PWD").ok().map(PathBuf::from),
            Some(start.clone()),
            "registration must set PWD before any cd"
        );

        let cmd = reg.get("cd").unwrap();
        let args = parse_args(&[target.to_str().unwrap().to_string()], &cmd.args).unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let out_path = tmp.path().to_path_buf();
        ctx.with_output(std::fs::File::create(&out_path).unwrap(), || {
            cmd.handler.call(&args, &ctx).unwrap();
        });

        assert_eq!(
            std::fs::read_to_string(&out_path).unwrap(),
            "",
            "C `cd` prints nothing on success"
        );
        assert_eq!(
            PathBuf::from(std::env::var("PWD").unwrap()),
            target,
            "`cd` must update PWD like C `updatePWD`"
        );

        std::env::set_current_dir(&start).unwrap();
    }

    /// C registers `installLastResortEventProvider`
    /// (`libComRegister.c:483-489`, `:531`) as the operator's opt-in into
    /// the wall-clock event fallback. Without the command the provider
    /// cannot be installed at all, so `epicsTimeGetEvent` fails forever
    /// on an IOC that has no event provider.
    #[test]
    fn install_last_resort_event_provider_is_reachable_from_the_shell() {
        let ctx = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg
            .get("installLastResortEventProvider")
            .expect("C registers this command");
        assert!(cmd.args.is_empty(), "C declares zero arguments");

        // No event provider is registered in this process, so C's
        // `S_time_noProvider` is the answer.
        assert!(
            crate::runtime::general_time::get_event(1).is_none(),
            "an IOC with no event provider must fail the event stamp"
        );

        let args = parse_args(&[], &cmd.args).unwrap();
        cmd.handler.call(&args, &ctx).unwrap();

        assert!(
            crate::runtime::general_time::get_event(1).is_some(),
            "the last-resort provider answers every event number"
        );
        // C's report names the EVENT provider "Last Resort Event"; "OS
        // Clock" is the current-time provider in a different table.
        assert!(
            crate::runtime::general_time::report(1).contains("Last Resort Event"),
            "generalTimeReport must name the provider as C does"
        );
    }

    #[test]
    fn epics_env_unset_removes_var() {
        let ctx = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        unsafe { std::env::set_var("_CORECMD_TEST", "x") };
        let cmd = reg.get("epicsEnvUnset").unwrap();
        let args = parse_args(&["_CORECMD_TEST".to_string()], &cmd.args).unwrap();
        cmd.handler.call(&args, &ctx).unwrap();
        assert!(std::env::var("_CORECMD_TEST").is_err());
    }
}
