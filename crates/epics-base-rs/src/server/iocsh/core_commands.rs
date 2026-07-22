//! Core iocsh commands beyond the database/record family.
//!
//! a stock `st.cmd` relies on a set of core commands
//! (`iocsh.cpp` / `libComRegister.c`) that the Rust port did not
//! register, so an unmodified startup script errored on the first
//! unknown command. This module registers the ones that are
//! implementable in-process; `iocshCmd` / `iocshRun` / `on` are
//! handled directly in `IocShell::execute_line` because they need
//! the shell itself, not just a `CommandContext`.

// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the feature-ON suite.

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
fn cmd_date() -> CommandDef {
    CommandDef::new(
        "date",
        vec![],
        "date — Print the current date and time.",
        |_args: &[ArgValue], ctx: &CommandContext| {
            let now = chrono::Local::now();
            ctx.println(&now.format("%Y-%m-%d %H:%M:%S%.6f %z").to_string());
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

fn chdir_handler(args: &[ArgValue], ctx: &CommandContext) -> CommandResult {
    let dir = match &args[0] {
        ArgValue::String(s) => s,
        _ => return Err("chdir: missing directory".into()),
    };
    std::env::set_current_dir(dir).map_err(|e| format!("chdir: {dir}: {e}"))?;
    if let Ok(cwd) = std::env::current_dir() {
        ctx.println(&cwd.display().to_string());
    }
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
            // SAFETY: matches C iocsh behaviour; mutated only from the
            // single-threaded REPL thread.
            unsafe { std::env::remove_var(name) };
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `epicsEnvShow [name]` — show one or all environment variables.
/// Mirrors C `epicsEnvShow`.
fn cmd_epics_env_show() -> CommandDef {
    CommandDef::new(
        "epicsEnvShow",
        vec![ArgDesc {
            name: "name",
            arg_type: ArgType::String,
            optional: true,
        }],
        "epicsEnvShow [name] — Show one or all environment variables.",
        |args: &[ArgValue], ctx: &CommandContext| {
            match &args[0] {
                ArgValue::String(name) => match std::env::var(name) {
                    Ok(v) => ctx.println(&format!("{name}={v}")),
                    Err(_) => ctx.println(&format!("{name} is not set")),
                },
                _ => {
                    let mut vars: Vec<(String, String)> = std::env::vars().collect();
                    vars.sort();
                    for (k, v) in vars {
                        ctx.println(&format!("{k}={v}"));
                    }
                }
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
        let handle = rt.handle().clone();
        let ctx = CommandContext::new(db, handle);
        std::mem::forget(rt);
        ctx
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
