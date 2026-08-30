// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that hand-builds its own tokio runtime; runs and passes in the exec-backend suite.
//! Core iocsh commands beyond the database/record family.
//!
//! a stock `st.cmd` relies on a set of core commands
//! (`iocsh.cpp` / `libComRegister.c`) that the Rust port did not
//! register, so an unmodified startup script errored on the first
//! unknown command. This module registers the ones that are
//! implementable in-process. `iocshCmd` / `iocshRun` / `iocshLoad` /
//! `on` are *executed* directly in `IocShell::execute_line` because they
//! need the shell itself, not just a `CommandContext` — but they are
//! registered here all the same, which is what C does with its own
//! shell-internal commands: "Dummy internal commands -- register and
//! install in command table so they show up in the help display"
//! (`iocsh.cpp:1577-1580` at `R7.0.10`). Without the table entry `help`
//! cannot print their usage and a command lookup cannot see them, which
//! is the whole of the gap those four had.
//!
//! The usage text of each command here is C's `iocshFuncDef.usage`
//! verbatim, because `help` prints it and an operator comparing the two
//! shells reads the same words.

use super::registry::*;

/// Register the core iocsh command set on `registry`.
pub(crate) fn register(registry: &mut CommandRegistry) {
    registry.register(cmd_comment());
    registry.register(cmd_echo());
    registry.register(cmd_date());
    registry.register(cmd_cd());
    registry.register(cmd_pwd());
    registry.register(cmd_epics_env_unset());
    registry.register(cmd_epics_env_show());
    registry.register(cmd_epics_prt_env_params());
    registry.register(cmd_epics_param_show());
    registry.register(cmd_epics_thread_sleep());
    registry.register(cmd_epics_thread_show_all());
    registry.register(cmd_epics_thread_show());
    registry.register(cmd_epics_thread_resume());
    registry.register(cmd_taskwd_show());
    registry.register(cmd_epics_mutex_show_all());
    registry.register(cmd_install_last_resort_event_provider());
    registry.register(cmd_general_time_report());
    // The four the shell executes itself. Registered for `help` and for
    // command lookup, never called — `execute_expanded_line` intercepts
    // each name before the registry is consulted.
    registry.register(cmd_iocsh_cmd());
    registry.register(cmd_iocsh_run());
    registry.register(cmd_iocsh_load());
    registry.register(cmd_on());

    // The errlog / IOC-log-client surface (`libComRegister.c:252-317`,
    // `:498-506`). Without these a startup script cannot turn on forwarding
    // to a site's log server, which is the whole point of the facility.
    registry.register(cmd_eltc());
    registry.register(cmd_errlog_init());
    registry.register(cmd_errlog_init2());
    registry.register(cmd_errlog_show());
    registry.register(cmd_errlog());
    registry.register(cmd_ioc_log_init());
    registry.register(cmd_ioc_log_prefix());
    registry.register(cmd_ioc_log_show());
    registry.register(cmd_set_ioc_log_disable());
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

/// `generalTimeReport <interest_level>` — C `libComRegister.c:454-464`,
/// `iocshSetError(generalTimeReport(args[0].ival))`.
///
/// The report itself is C `generalTimeReport`
/// (`epicsGeneralTime.c:530-618`) and the port already owns it, byte
/// shape included, in [`crate::runtime::general_time::report`]; this is
/// only the shell entry point it never had. Level 0 lists the providers
/// and their priorities, level 1 also samples each current-time
/// provider. C returns `epicsTimeOK` on every path that prints, so the
/// line never fails.
fn cmd_general_time_report() -> CommandDef {
    CommandDef::new(
        "generalTimeReport",
        vec![ArgDesc {
            // C `generalTimeReportArg0` (`libComRegister.c:455`).
            name: "interest_level",
            arg_type: ArgType::Int,
        }],
        concat!(
            "Display time providers information for given interest level.\n",
            "interest level 0 - List providers and their priorities.\n",
            "               1 - Additionally show current time obtained from each provider.",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            let level = match args.first() {
                Some(ArgValue::Int(n)) => *n as i32,
                _ => 0,
            };
            let report = crate::runtime::general_time::report(level);
            // `report` is already newline-terminated exactly as C's
            // `printf`/`puts` sequence leaves it — including the blank
            // line `puts(message)` adds after a non-empty provider
            // block. `print_fmt` re-adds the final newline, so strip
            // exactly one rather than trimming the run.
            let body = report.strip_suffix('\n').unwrap_or(&report);
            ctx.print_fmt(format_args!("{body}"));
            Ok(CommandOutcome::Continue)
        },
    )
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
        concat!(
            "Installs the optional Last Resort event provider at priority 999,\n",
            "which returns the current time for every event number",
        ),
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
/// The handler every shell-executed command carries. `execute_expanded_line`
/// matches these names before it consults the registry, so reaching this is
/// a routing bug, not a user error — say so instead of failing quietly.
fn shell_owned(name: &'static str) -> impl Fn(&[ArgValue], &CommandContext) -> CommandResult {
    move |_args: &[ArgValue], _ctx: &CommandContext| {
        Err(format!(
            "{name} must be run by the shell, not through the command table"
        ))
    }
}

/// `iocshCmd(command)` — C `iocsh.cpp:1473-1483`, `iocshCmd(cmd)` being
/// `iocshRun(cmd, NULL)` (`:1335-1338`). Executed by the shell.
fn cmd_iocsh_cmd() -> CommandDef {
    CommandDef::new(
        "iocshCmd",
        vec![ArgDesc {
            name: "command",
            arg_type: ArgType::String,
        }],
        concat!(
            "Takes a single IOC shell command and executes it\n",
            "  * This function is most useful to execute a single IOC shell command\n",
            "    from vxWorks or RTEMS startup script (or command line)",
        ),
        shell_owned("iocshCmd"),
    )
}

/// `iocshRun(command, macros)` — C `iocsh.cpp:1497-1508`. Executed by the
/// shell.
fn cmd_iocsh_run() -> CommandDef {
    CommandDef::new(
        "iocshRun",
        vec![
            ArgDesc {
                name: "command",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "macros",
                arg_type: ArgType::String,
            },
        ],
        concat!(
            "Takes a single IOC shell command, replaces macros and executes it\n",
            "  * This function is most useful to execute a single IOC shell command\n",
            "    from vxWorks or RTEMS startup script (or command line)",
        ),
        shell_owned("iocshRun"),
    )
}

/// `iocshLoad(pathname, macros)` — C `iocsh.cpp:1485-1495`. Executed by the
/// shell.
fn cmd_iocsh_load() -> CommandDef {
    CommandDef::new(
        "iocshLoad",
        vec![
            ArgDesc {
                name: "pathname",
                arg_type: ArgType::Path,
            },
            ArgDesc {
                name: "macros",
                arg_type: ArgType::String,
            },
        ],
        concat!(
            "Execute IOC shell commands provided in file from first parameter\n",
            "  * (optional) replace macros within the file with provided values",
        ),
        shell_owned("iocshLoad"),
    )
}

/// `on error continue|break|halt|wait <delay>` — C `iocsh.cpp:1510-1518`.
/// The single C argument is `iocshArgArgv`, so the whole tail of the line
/// reaches the handler; the shell parses it in `handle_on_command`.
fn cmd_on() -> CommandDef {
    CommandDef::new(
        "on",
        vec![ArgDesc {
            // C `onArg0` (`iocsh.cpp:1511`), an `iocshArgArgv` so that
            // `help` prints the phrase bare — the quoting rule only
            // applies to a fixed argument whose name has a space in it.
            // C bolds the `error` word inside the name; this port leaves
            // the escape out so a `NO_COLOR` shell stays clean.
            name: "error [continue | break | halt | wait <delay>]",
            arg_type: ArgType::Argv,
        }],
        concat!(
            "Change IOC shell error handling.\n",
            "  continue (default) - Ignores error and continue with next commands.\n",
            "  break - Return to caller without executing further commands.\n",
            "  halt - Suspend process.\n",
            "  wait - stall process for <delay> seconds, then continue.",
        ),
        shell_owned("on"),
    )
}

/// `eltc <(0,1)>` — C `libComRegister.c:252-262`. "Error log to console".
fn cmd_eltc() -> CommandDef {
    CommandDef::new(
        "eltc",
        vec![ArgDesc {
            name: "(0,1)=>(false,true)",
            arg_type: ArgType::Int,
        }],
        concat!(
            "Control display of error log messages on console\n",
            "  0 - no\n",
            "  1 - yes (default)",
        ),
        |args: &[ArgValue], _ctx: &CommandContext| {
            let yes = matches!(args.first(), Some(ArgValue::Int(v)) if *v != 0);
            crate::runtime::log::eltc(yes);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `errlogInit <bufSize>` — C `libComRegister.c:264-276`. The size is a
/// request: `errlogInit` raises anything below `MIN_BUFFER_SIZE` and the
/// once-init means only the first call decides.
fn cmd_errlog_init() -> CommandDef {
    CommandDef::new(
        "errlogInit",
        vec![ArgDesc {
            name: "bufSize",
            arg_type: ArgType::Int,
        }],
        concat!(
            "Initialize error log client buffer size\n",
            "  bufSize - size of circular buffer (default = 1280 bytes)",
        ),
        |args: &[ArgValue], _ctx: &CommandContext| {
            let bufsize = int_arg(args.first()).max(0) as usize;
            crate::runtime::log::errlog_init(bufsize);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `errlogInit2 <bufSize> <maxMsgSize>` — C `libComRegister.c:278-291`.
fn cmd_errlog_init2() -> CommandDef {
    CommandDef::new(
        "errlogInit2",
        vec![
            ArgDesc {
                name: "bufSize",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "maxMsgSize",
                arg_type: ArgType::Int,
            },
        ],
        concat!(
            "Initialize error log client buffer size and maximum message size\n",
            "  bufSize    - size of circular buffer       (default = 1280 bytes)\n",
            "  maxMsgSize - maximum size of error message (default =  256 bytes)",
        ),
        |args: &[ArgValue], _ctx: &CommandContext| {
            let bufsize = int_arg(args.first()).max(0) as usize;
            let max_msg = int_arg(args.get(1)).max(0) as usize;
            crate::runtime::log::errlog_init2(bufsize, max_msg);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `errlogShow <level>` — C `libComRegister.c:294-307`, registered at `:519`.
/// It was in the expected-name tables and nowhere else, so a `st.cmd` line
/// asking an IOC what its error log is doing got `Command 'errlogShow' not
/// registered.`
///
/// The three level bands and their help text are C's;
/// [`crate::runtime::log::errlog_show`] states where the level-2 dump departs
/// from C's arena bytes and why.
fn cmd_errlog_show() -> CommandDef {
    CommandDef::new(
        "errlogShow",
        vec![ArgDesc {
            name: "level",
            arg_type: ArgType::Int,
        }],
        concat!(
            "Show the contents of the error log private data\n",
            "level 0 - Show the size of the buffers and max message size\n",
            "      1 - Show the number of listeners\n",
            "      2 - Show the contents of the buffer",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            let level = int_arg(args.first()).max(0) as u32;
            for line in crate::runtime::log::errlog_show(level) {
                ctx.println(&line);
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `errlog <message>` — C `libComRegister.c:293-307`. Note the two details
/// the C body carries: the message goes through `errlogPrintfNoConsole` with
/// a newline appended, and the call func flushes afterwards, so the line has
/// reached every listener by the time the shell prompts again.
fn cmd_errlog() -> CommandDef {
    CommandDef::new(
        "errlog",
        vec![ArgDesc {
            name: "message",
            arg_type: ArgType::String,
        }],
        "Send message to errlog",
        |args: &[ArgValue], _ctx: &CommandContext| {
            let message = match args.first() {
                Some(ArgValue::String(s)) => s.clone(),
                _ => String::new(),
            };
            crate::runtime::log::errlog_printf_no_console(&format!("{message}\n"));
            crate::runtime::log::errlog_flush();
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `iocLogInit` — C `libComRegister.c:214-223`. `iocshSetError(iocLogInit())`
/// marks the line failed with no diagnostic of its own, because the
/// `iocLog: EPICS environment variable …` line has already been printed.
fn cmd_ioc_log_init() -> CommandDef {
    CommandDef::new(
        "iocLogInit",
        vec![],
        concat!(
            "Initialize IOC logging\n",
            "  * EPICS environment variable 'EPICS_IOC_LOG_INET' has to be defined\n",
            "  * Logging controlled via 'iocLogDisable' variable\n",
            "       see 'setIocLogDisable' command",
        ),
        |_args: &[ArgValue], _ctx: &CommandContext| match crate::runtime::log_client::ioc_log_init()
        {
            Ok(()) => Ok(CommandOutcome::Continue),
            Err(_) => Ok(CommandOutcome::Failed),
        },
    )
}

/// `iocLogPrefix <prefix>` — C `libComRegister.c:309-317`.
fn cmd_ioc_log_prefix() -> CommandDef {
    CommandDef::new(
        "iocLogPrefix",
        vec![ArgDesc {
            name: "prefix",
            arg_type: ArgType::String,
        }],
        "Create the prefix for all messages going into IOC log",
        |args: &[ArgValue], _ctx: &CommandContext| {
            if let Some(ArgValue::String(prefix)) = args.first() {
                crate::runtime::log_client::ioc_log_prefix(prefix);
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `iocLogShow <level>` — C `libComRegister.c:242-250`. The lines go through
/// the command context so a redirect captures them, where C writes straight
/// to `stdout`.
fn cmd_ioc_log_show() -> CommandDef {
    CommandDef::new(
        "iocLogShow",
        vec![ArgDesc {
            name: "level",
            arg_type: ArgType::Int,
        }],
        "Determine if a IOC Log Prefix has been set",
        |args: &[ArgValue], ctx: &CommandContext| {
            let level = int_arg(args.first()).max(0) as u32;
            for line in crate::runtime::log_client::ioc_log_show(level) {
                ctx.println(&line);
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `setIocLogDisable <(0,1)>` — C `libComRegister.c:225-240`. Read per
/// message, so it silences a client that is already connected.
fn cmd_set_ioc_log_disable() -> CommandDef {
    CommandDef::new(
        "setIocLogDisable",
        vec![ArgDesc {
            name: "(0,1)=>(false,true)",
            arg_type: ArgType::Int,
        }],
        concat!(
            "Controls the 'iocLogDisable' variable\n",
            "  0 - enable logging\n",
            "  1 - disable logging",
        ),
        |args: &[ArgValue], _ctx: &CommandContext| {
            crate::runtime::log_client::set_ioc_log_disable(int_arg(args.first()) != 0);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// The integer an `ArgType::Int` slot carries, or 0 — C's `args[n].ival` is
/// zero for a missing or unparsable argument.
fn int_arg(arg: Option<&ArgValue>) -> i64 {
    match arg {
        Some(ArgValue::Int(v)) => *v,
        _ => 0,
    }
}

fn cmd_comment() -> CommandDef {
    CommandDef::new(
        "#",
        vec![ArgDesc {
            name: "text",
            arg_type: ArgType::String,
        }],
        "",
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
            name: "string",
            arg_type: ArgType::String,
        }],
        "Print string after expanding macros and environment variables",
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
///
/// C's `strftime` cannot fail: an unknown conversion is copied out
/// literally, so `date "%Q"` prints `%Q`. chrono's `DelayedFormat` instead
/// makes its `Display` return `Err`, and `to_string()` on that panics — the
/// measured result of `date "%Q"` was `a Display implementation returned an
/// error unexpectedly: Error` and a dead shell thread, against C's `%Q`.
///
/// A runtime guard around the render cannot fix that, because there is no
/// answer to fall back TO: the whole line is lost for one bad conversion.
/// So no specifier chrono rejects is allowed into the format string in the
/// first place. Each one is rendered on its own against the very timestamp
/// the line will print, and the ones that fail come out as `%%X` — chrono's
/// literal per cent — which is glibc's pass-through and makes the result
/// render by construction. `now` is a parameter for exactly that reason:
/// the probe has to be the real render, not a guess about one.
fn epics_strftime_to_chrono(fmt: &str, now: &chrono::DateTime<chrono::Local>) -> String {
    use std::fmt::Write as _;
    // chrono renders this specifier for this timestamp without erroring.
    let renders = |spec: &str| {
        let mut probe = String::new();
        write!(probe, "{}", now.format(spec)).is_ok()
    };
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
        // Not one of EPICS' own conversions, so it is the C library's, and
        // the only question is whether chrono knows it. A trailing bare `%`
        // has no following character to ask about and is a literal in C.
        match c.get(i + 1) {
            Some(&ch) if renders(&format!("%{ch}")) => {
                out.push('%');
                out.push(ch);
            }
            Some(&ch) => {
                out.push_str("%%");
                out.push(ch);
            }
            None => out.push_str("%%"),
        }
        i += if i + 1 < c.len() { 2 } else { 1 };
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
        }],
        concat!(
            "Print current date and time\n",
            "  (default) - '%Y/%m/%d %H:%M:%S.%06f'",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            let fmt = match &args[0] {
                ArgValue::String(f) if !f.is_empty() => f.as_str(),
                _ => "%Y/%m/%d %H:%M:%S.%06f",
            };
            let now = chrono::Local::now();
            ctx.println(&now.format(&epics_strftime_to_chrono(fmt, &now)).to_string());
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `cd <dir>` — C's only spelling of the directory change: the func def
/// is `chdirFuncDef` but the name it registers is `cd`
/// (`libComRegister.c:105`).
fn cmd_cd() -> CommandDef {
    CommandDef::new(
        "cd",
        vec![ArgDesc {
            // C `chdirArg0` (`libComRegister.c:104`), whose func def is
            // named `cd` (`:106`).
            name: "directory name",
            arg_type: ArgType::Path,
        }],
        "Change directory to new directory provided as parameter",
        chdir_handler,
    )
}

/// C `chdirCallFunc` (`libComRegister.c:104-116`) prints nothing on
/// success — it only calls `updatePWD()`. The cwd line the port used to
/// echo here has no counterpart in C.
fn chdir_handler(args: &[ArgValue], ctx: &CommandContext) -> CommandResult {
    // C's whole body is one `||` (`libComRegister.c:108-116`):
    //
    // ```c
    // if (args[0].sval == NULL ||
    //     iocshSetError(chdir(args[0].sval))) {
    //     fprintf(stderr, "Invalid directory path, ignored\n");
    // } else {
    //     updatePWD();
    // }
    // ```
    //
    // `None` is the NULL arm. It SHORT-CIRCUITS, so `iocshSetError` never
    // runs and the line stays successful; refusing it made `on error break`
    // abandon the rest of a script C finishes. `Some(false)` is a `chdir()`
    // that failed, and that return value IS what `iocshSetError` is handed,
    // so that line — and only that line — is errored.
    let changed = match &args[0] {
        ArgValue::String(dir) => Some(set_working_dir(dir).is_ok()),
        _ => None,
    };
    if changed == Some(true) {
        // C prints nothing on success; `updatePWD` is inside
        // `set_working_dir`, the one owner of the cwd.
        return Ok(CommandOutcome::Continue);
    }
    // ONE sentence for both failing arms, because C's `||` writes it once.
    // It does not name the directory and it does not spell the errno: this
    // site used to return `Err(format!("chdir: {dir}: {e}"))`, which the
    // shell then framed, so an operator who typed a missing directory read
    // `ERROR st.cmd line 11: chdir: topbin: No such file or directory (os
    // error 2)` where C says six words and no `os error`.
    ctx.eprintln("Invalid directory path, ignored");
    Ok(if changed.is_none() {
        CommandOutcome::Continue
    } else {
        CommandOutcome::Failed
    })
}

/// `pwd` — print the current working directory. Mirrors C `pwd`.
fn cmd_pwd() -> CommandDef {
    CommandDef::new(
        "pwd",
        vec![],
        "Print name of current/working directory",
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
        }],
        "Remove variable name from the environment",
        |args: &[ArgValue], ctx: &CommandContext| {
            // Same shape and the same sentence as `epicsEnvSet`'s missing
            // name (`libComRegister.c:164-168`): written by the body, so
            // unframed, and failing the line through `iocshSetError(-1)`
            // rather than through a diagnostic of the shell's own.
            let ArgValue::String(name) = &args[0] else {
                ctx.eprintln("Missing environment variable name argument.");
                return Ok(CommandOutcome::Failed);
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
        }],
        concat!(
            "Show environment variables on your system\n",
            "  (default) - show all environment variables\n",
            "   name     - show value of specific environment variable\n",
            "Example: epicsEnvShow\n",
            "Example: epicsEnvShow PATH",
        ),
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
        "Show the environment variable parameters used by iocCore",
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
        "Show the environment variable parameters used by iocCore",
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
        }],
        "Pause execution of IOC shell for <seconds> seconds",
        |args: &[ArgValue], _ctx: &CommandContext| {
            let secs = match &args[0] {
                ArgValue::Double(d) => *d,
                ArgValue::Int(n) => *n as f64,
                // C `epicsThreadSleepCallFunc` (`libComRegister.c:419-422`)
                // sleeps `args[0].dval`, and `cvtArg` leaves that at 0.0 when
                // the line carried no token for it. An argument-less
                // `epicsThreadSleep` is a real line in base's own
                // `libcom/test/iocshTestSuccess.cmd:8`, so refusing it stopped
                // a script C runs to the end.
                _ => 0.0,
            };
            // C's own `epicsThreadSleep`, through the one owner:
            // `Duration::from_secs_f64` panicked here on `1e300` / `inf`
            // / `nan`, all of which C accepts and returns from at once.
            crate::runtime::time::sleep_secs(secs);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `epicsThreadShowAll [level]` — C `libComRegister.c:319-327`,
/// `epicsThreadShowAll(args[0].ival)`.
///
/// The header, one line per live thread in creation order, then a trailer on
/// **stderr** — C's own three-part shape (`osdThread.c:1009-1031`). The
/// listing comes from [`crate::runtime::task::thread_report`], the port's
/// `pthreadList`, so a thread appears here for exactly as long as it exists.
///
/// `level` is accepted and ignored, as it is in C: `epicsThreadShowInfo` on
/// every POSIX target takes the argument and never reads it
/// (`os/Linux/osdThreadExtra.c:31-55`). Rejecting it, or printing more at a
/// higher level, would both be departures from what a stock `st.cmd` sees.
fn cmd_epics_thread_show_all() -> CommandDef {
    CommandDef::new(
        "epicsThreadShowAll",
        vec![ArgDesc {
            name: "level",
            arg_type: ArgType::Int,
        }],
        "Display info about all threads",
        |_args: &[ArgValue], ctx: &CommandContext| {
            show_all_threads(ctx);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// C `epicsThreadShowAll` itself (`osdThread.c:1009-1031`): the header row is
/// `epicsThreadShow(0, level)`, and the priority-range trailer goes to stderr
/// so a script redirecting the listing to a file still sees it on the console.
fn show_all_threads(ctx: &CommandContext) {
    ctx.println(crate::runtime::task::THREAD_SHOW_HEADER);
    for thread in crate::runtime::task::thread_report() {
        ctx.println(&thread.show_line());
    }
    ctx.eprintln(&crate::runtime::task::osd_priority_range_line());
}

/// `epicsThreadShow [-level] [thread ...]` — C `libComRegister.c:329-370`.
///
/// One `iocshArgArgv` parameter, so the command sees the whole tail of the
/// line rather than a fixed number of slots: `epicsThreadShow -2 errlog CAS`
/// is three arguments to one declared parameter. With no thread named it is
/// `epicsThreadShowAll`; otherwise the header is printed once, before the
/// first argument that resolves, and then one entry per argument.
///
/// An argument that does not parse as a number is looked up as a thread name.
/// A name that matches nothing is C's only failing path here: the diagnostic
/// goes to stderr, `iocshSetError(-1)` marks the line failed, and the loop
/// carries on with the remaining arguments rather than stopping.
fn cmd_epics_thread_show() -> CommandDef {
    CommandDef::new(
        "epicsThreadShow",
        vec![ArgDesc {
            name: "[-level] [thread ...]",
            arg_type: ArgType::Argv,
        }],
        "Display info about the specified thread",
        |args: &[ArgValue], ctx: &CommandContext| {
            let ArgValue::Argv(argv) = &args[0] else {
                return Err("epicsThreadShow: expected the argument vector".into());
            };
            let mut rest = argv.as_slice();
            // C `if (*(cp = argv[i]) == '-') { level = atoi(cp + 1); i++; }`.
            // The level is consumed but never used: `epicsThreadShowInfo`
            // takes it and reads it on no POSIX target
            // (`os/Linux/osdThreadExtra.c:31-55`). Consuming it is what keeps
            // `-2` from being read as a thread handle.
            if rest.first().is_some_and(|token| token.starts_with('-')) {
                rest = &rest[1..];
            }
            if rest.is_empty() {
                show_all_threads(ctx);
                return Ok(CommandOutcome::Continue);
            }

            let mut header_printed = false;
            let mut failed = false;
            for token in rest {
                // C `strtoull(cp, &endp, 0)`; a non-empty remainder means the
                // argument was a name, not a handle.
                let id = match super::registry::parse_iocsh_int(token) {
                    Ok(value) => value as u64,
                    Err(_) => match crate::runtime::task::thread_by_name(token) {
                        Some(thread) => thread.id(),
                        None => {
                            ctx.eprintln(&format!("\t'{token}' is not a known thread name"));
                            failed = true;
                            continue;
                        }
                    },
                };
                if !header_printed {
                    show_thread(ctx, 0);
                    header_printed = true;
                }
                show_thread(ctx, id);
            }

            if failed {
                Ok(CommandOutcome::Failed)
            } else {
                Ok(CommandOutcome::Continue)
            }
        },
    )
}

/// `epicsThreadResume [thread ...]` — C `libComRegister.c:408-452`,
/// registered at `:511`.
///
/// One `iocshArgArgv`, like `epicsThreadShow`, and the same
/// number-or-name resolution per argument: `strtoull` with a remainder
/// means the token was a thread name, an exact parse means it was a
/// handle. Each of C's three failure arms writes to stderr, marks the
/// line failed and moves on to the next argument rather than abandoning
/// the rest of the line.
///
/// C validates a *handle* by calling `epicsThreadGetName` on it and then
/// testing whether anything was written (`:438-441`), which dereferences
/// the number before it can be rejected: R7.0.10 `softIoc` takes SIGSEGV
/// on both `epicsThreadResume 0` and `epicsThreadResume
/// 18446744073709551615`, so C's own `is not a valid thread id` arm is
/// unreachable on Linux. Resolving the id through the thread registry
/// instead is what lets this port print the line C meant to print.
///
/// C tests `epicsThreadIsSuspended(tid)` and then calls
/// `epicsThreadResume(tid)` (`:445-450`); `ThreadInfo::resume` does both
/// under the thread's own suspension lock and reports which arm it took,
/// so the answer this prints cannot be stale by the time it acts. The one
/// thread that genuinely parks itself is a breakpoint continuation
/// (`dbBkpt.c:797`), and resuming it here is the same call `dbc` makes
/// (`:518`) — the shell and the debugger reach one park.
fn cmd_epics_thread_resume() -> CommandDef {
    CommandDef::new(
        "epicsThreadResume",
        vec![ArgDesc {
            name: "[thread ...]",
            arg_type: ArgType::Argv,
        }],
        concat!(
            "Resume a suspended thread.\n",
            "Only do this if you know that it is safe to resume a suspended thread",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            let ArgValue::Argv(argv) = &args[0] else {
                return Err("epicsThreadResume: expected the argument vector".into());
            };
            let mut failed = false;
            for token in argv {
                let (kind, found) = match super::registry::parse_iocsh_int(token) {
                    Ok(id) => ("thread id", crate::runtime::task::thread_by_id(id as u64)),
                    Err(_) => ("thread name", crate::runtime::task::thread_by_name(token)),
                };
                let Some(thread) = found else {
                    ctx.eprintln(&format!("'{token}' is not a valid {kind}"));
                    failed = true;
                    continue;
                };
                // C `if (!epicsThreadIsSuspended(tid)) { ... continue; }
                // epicsThreadResume(tid);` (`:445-450`), as one step.
                if !thread.resume() {
                    ctx.eprintln(&format!("Thread {token} is not suspended"));
                    failed = true;
                }
            }
            if failed {
                Ok(CommandOutcome::Failed)
            } else {
                Ok(CommandOutcome::Continue)
            }
        },
    )
}

/// C `epicsThreadShow` itself (`osdThread.c:1033-1062`).
///
/// A zero handle is the header and nothing else — the call C's own
/// `epicsThreadShowAll` makes to print it. A handle that matches no live
/// thread reports so on **stdout** (`epicsStdoutPrintf`), unlike the unknown
/// *name* case, which is a stderr diagnostic: C treats an unresolvable name
/// as a bad command line and an unresolvable number as a thread that has
/// since exited.
fn show_thread(ctx: &CommandContext, id: u64) {
    if id == 0 {
        ctx.println(crate::runtime::task::THREAD_SHOW_HEADER);
        return;
    }
    match crate::runtime::task::thread_by_id(id) {
        Some(thread) => ctx.println(&thread.show_line()),
        None => ctx.println(&format!("Thread {id:#x} ({id}) not found.")),
    }
}

/// `taskwdShow <level>` — C `libComRegister.c:372-380`, registered at `:508`,
/// over `taskwdShow(int level)` (`taskwd.c:359-390`).
///
/// The report belongs to the watchdog and is
/// [`crate::runtime::taskwd::taskwd_show`]'s; this is the registration and the
/// argument. C tests `level` for truth (`taskwd.c:377`), so any non-zero value
/// — negative included, which is why this casts rather than clamps — adds the
/// per-task table, and a missing argument is 0.
///
/// C's detail row is `THREAD NAME STATE EPICS TID epicsCallback USR ARG`
/// (`taskwd.c:378-386`). The last three are C pointers, and half the tasks
/// this port watches are futures with no thread of their own, so they have no
/// value here and none is invented; C's `%d free nodes` on the summary line is
/// absent for the same kind of reason, being a count of a recycling pool that
/// a Rust drop replaces. `STATE` keeps C's wording over the port's meaning: a
/// task that stopped checking in, since no thread here can be suspended.
fn cmd_taskwd_show() -> CommandDef {
    CommandDef::new(
        "taskwdShow",
        vec![ArgDesc {
            name: "level",
            arg_type: ArgType::Int,
        }],
        "Show number of tasks and monitors registered",
        |args: &[ArgValue], ctx: &CommandContext| {
            let level = match &args[0] {
                ArgValue::Int(n) => *n as u32,
                _ => 0,
            };
            crate::runtime::taskwd::taskwd_show(level, &|line| ctx.println(line));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `epicsMutexShowAll <onlyLocked> <level>` — C `libComRegister.c:383-397`,
/// `epicsMutexShowAll(args[0].ival, args[1].ival)`.
///
/// Three parts, all on stdout: the whole list's length, the priority-protocol
/// line, then one entry per mutex that passed the filter
/// (`epicsMutex.cpp:129-146`). `ellCount(&mutexList)` is C's literal wording,
/// kept because the point of the command is that its output matches C's.
///
/// `onlyLocked` is C's `int` — any non-zero value turns the try-lock filter
/// on. `level` is C's `unsigned int`, so a negative argument converts to a
/// large positive one and *does* select the extra `uaddr` line, which is why
/// this casts rather than clamping.
fn cmd_epics_mutex_show_all() -> CommandDef {
    CommandDef::new(
        "epicsMutexShowAll",
        vec![
            ArgDesc {
                name: "onlyLocked",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "level",
                arg_type: ArgType::Int,
            },
        ],
        concat!(
            "Display information about all epicsMutex semaphores\n",
            "  onlyLocked - non-zero to show only locked semaphores\n",
            "  level      - desired information level to report",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            let only_locked = matches!(&args[0], ArgValue::Int(n) if *n != 0);
            let level = match &args[1] {
                ArgValue::Int(n) => *n as u32,
                _ => 0,
            };
            let report = crate::runtime::sync::mutex_report(only_locked);
            ctx.println(&format!("ellCount(&mutexList) {}", report.total));
            ctx.println(crate::runtime::sync::osd_show_all_line());
            for entry in &report.shown {
                for line in entry.show_lines(level) {
                    ctx.println(&line);
                }
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

    /// Both streams a command writes, kept apart: C sends the thread listing
    /// to stdout and the priority-range trailer to stderr, and a `>` redirect
    /// of the listing must not swallow the trailer.
    fn run_split(ctx: &CommandContext, name: &str, tokens: &[&str]) -> (String, String, bool) {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg.get(name).unwrap();
        let tokens: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
        let args = parse_args(&tokens, &cmd.args).unwrap();
        let out_file = tempfile::NamedTempFile::new().unwrap();
        let err_file = tempfile::NamedTempFile::new().unwrap();
        let (out_path, err_path) = (out_file.path().to_path_buf(), err_file.path().to_path_buf());
        let mut failed = false;
        ctx.with_error(std::fs::File::create(&err_path).unwrap(), || {
            ctx.with_output(std::fs::File::create(&out_path).unwrap(), || {
                failed = matches!(
                    cmd.handler.call(&args, ctx).unwrap(),
                    CommandOutcome::Failed
                );
            });
        });
        (
            std::fs::read_to_string(&out_path).unwrap(),
            std::fs::read_to_string(&err_path).unwrap(),
            failed,
        )
    }

    /// C `taskwdShowFuncDef` is one `iocshArgInt` named `level`
    /// (`libComRegister.c:373-376`), so a C `st.cmd` line parses unchanged.
    /// Every `iocshArg.name` C's `libComRegister.c` gives a command, as of
    /// R7.0.10.
    ///
    /// These were decoration until `help` learned C's synopsis line
    /// (`iocsh.cpp:956-969`); now they are the first thing an operator
    /// reads about a command, so they have to be C's words and not a
    /// paraphrase.
    const C_LIBCOM_ARG_NAMES: &[(&str, &[&str])] = &[
        ("cd", &["directory name"]),
        ("date", &["format"]),
        ("echo", &["string"]),
        ("eltc", &["(0,1)=>(false,true)"]),
        ("epicsEnvSet", &["name", "value"]),
        ("epicsEnvShow", &["[name]"]),
        ("epicsEnvUnset", &["name"]),
        ("epicsMutexShowAll", &["onlyLocked", "level"]),
        ("epicsParamShow", &[]),
        ("epicsPrtEnvParams", &[]),
        ("epicsThreadResume", &["[thread ...]"]),
        ("epicsThreadShow", &["[-level] [thread ...]"]),
        ("epicsThreadShowAll", &["level"]),
        ("epicsThreadSleep", &["seconds"]),
        ("errlog", &["message"]),
        ("errlogInit", &["bufSize"]),
        ("errlogInit2", &["bufSize", "maxMsgSize"]),
        ("errlogShow", &["level"]),
        ("generalTimeReport", &["interest_level"]),
        ("installLastResortEventProvider", &[]),
        ("iocLogInit", &[]),
        ("iocLogPrefix", &["prefix"]),
        ("iocLogShow", &["level"]),
        ("pwd", &[]),
        ("registryDump", &[]),
        ("setIocLogDisable", &["(0,1)=>(false,true)"]),
        ("taskwdShow", &["level"]),
    ];

    /// C's argument names, verbatim, for every `libComRegister.c` command
    /// this module registers.
    ///
    /// The count is pinned so that a command drifting out of this module
    /// fails here rather than quietly dropping out of the comparison.
    #[test]
    fn the_argument_names_are_cs_words() {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let mut checked = 0;
        for (name, want) in C_LIBCOM_ARG_NAMES {
            let Some(def) = reg.get(name) else { continue };
            let got: Vec<&str> = def.args.iter().map(|a| a.name).collect();
            assert_eq!(&got, want, "`{name}` argument names");
            checked += 1;
        }
        // `epicsEnvSet` and `registryDump` are the two C registers here
        // that this module does not.
        assert_eq!(checked, 25);
    }

    #[test]
    fn taskwd_show_is_registered_with_cs_arity() {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg
            .get("taskwdShow")
            .expect("taskwdShow must be registered");
        assert_eq!(cmd.args.len(), 1);
        assert_eq!(cmd.args[0].name, "level");
        assert!(matches!(cmd.args[0].arg_type, ArgType::Int));
    }

    /// C prints the summary line always and the per-task table only when
    /// `level` is true (`taskwd.c:375-388`). A missing argument is 0 there, and
    /// a negative one is true, because the test is `if (level)` and not a
    /// comparison.
    #[test]
    fn the_task_table_appears_exactly_when_cs_level_is_true() {
        let ctx = make_ctx();
        let watched = crate::runtime::taskwd::taskwd_insert(
            "cbTaskwdCmd",
            crate::runtime::taskwd::CheckIn::Every(std::time::Duration::from_secs(30)),
            None,
        );

        let (summary, err, failed) = run_split(&ctx, "taskwdShow", &[]);
        assert!(!failed);
        assert_eq!(err, "", "the whole report is stdout, as C's `printf` is");
        assert_eq!(
            summary.lines().count(),
            1,
            "a missing level is C's 0 — summary only, got {summary:?}"
        );
        assert!(
            summary.starts_with("0 monitors, 1 tasks registered"),
            "{summary:?}"
        );

        let (detailed, _, _) = run_split(&ctx, "taskwdShow", &["1"]);
        let lines: Vec<&str> = detailed.lines().collect();
        assert_eq!(
            lines.len(),
            3,
            "summary, header, one task — got {detailed:?}"
        );
        assert!(lines[1].starts_with("TASK NAME"), "{:?}", lines[1]);
        assert!(lines[2].starts_with("cbTaskwdCmd"), "{:?}", lines[2]);
        assert!(
            lines[2].contains("Ok"),
            "a task that is checking in is not reported — {:?}",
            lines[2]
        );

        let (negative, _, _) = run_split(&ctx, "taskwdShow", &["-1"]);
        assert_eq!(
            negative.lines().count(),
            3,
            "C's `if (level)` makes a negative level true — got {negative:?}"
        );

        drop(watched);
        let (empty, _, _) = run_split(&ctx, "taskwdShow", &["1"]);
        assert_eq!(
            empty.lines().count(),
            2,
            "dropping the handle is C's `taskwdRemove` — got {empty:?}"
        );
    }

    /// Start a thread that has run the IOC prologue and is parked, so the
    /// listing has a row whose contents are known. The returned sender ends it.
    fn parked_ioc_thread(
        name: &'static str,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let join = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let _ = crate::runtime::task::enter_ioc_thread(
                    crate::runtime::task::ThreadPriority::ScanLow,
                );
                started_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
            })
            .unwrap();
        started_rx.recv().unwrap();
        (finish_tx, join)
    }

    /// C `libComRegister.c:319-327`: one `iocshArgInt` named `level`.
    #[test]
    fn epics_thread_show_all_is_registered_with_cs_arity() {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg
            .get("epicsThreadShowAll")
            .expect("epicsThreadShowAll must be registered");
        assert_eq!(cmd.args.len(), 1);
        assert_eq!(cmd.args[0].name, "level");
        assert!(matches!(cmd.args[0].arg_type, ArgType::Int));
    }

    /// The three parts of C's `epicsThreadShowAll`, and which stream each
    /// lands on.
    #[test]
    fn epics_thread_show_all_prints_the_header_the_rows_and_a_stderr_trailer() {
        let ctx = make_ctx();
        let (finish, join) = parked_ioc_thread("cbShowAllRow");

        let (out, err, failed) = run_split(&ctx, "epicsThreadShowAll", &[]);
        assert!(!failed);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], crate::runtime::task::THREAD_SHOW_HEADER);
        let row = lines
            .iter()
            .find(|l| l.contains("cbShowAllRow"))
            .unwrap_or_else(|| panic!("the running thread must be listed:\n{out}"));
        // `%16.16s` right-aligns the name, then `%3d` carries the EPICS
        // priority the thread took.
        assert!(row.starts_with("    cbShowAllRow "), "{row}");
        assert!(
            row.ends_with(&format!(
                "{:3}{:8} {:>8.8}",
                crate::runtime::task::ThreadPriority::ScanLow.value(),
                0,
                "OK"
            )),
            "{row}"
        );

        assert!(
            !out.contains("OSD priority range"),
            "the trailer belongs on stderr, not in the redirected listing"
        );
        assert!(err.starts_with("OSD priority range min: "), "{err}");
        assert!(err.trim_end().ends_with(", memory not locked"), "{err}");

        finish.send(()).unwrap();
        join.join().unwrap();
    }

    /// C `libComRegister.c:329-332`: one `iocshArgArgv` whose declared name is
    /// the usage line itself, which is what `help epicsThreadShow` shows.
    #[test]
    fn epics_thread_show_is_registered_with_cs_arity() {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg
            .get("epicsThreadShow")
            .expect("epicsThreadShow must be registered");
        assert_eq!(cmd.args.len(), 1);
        assert_eq!(cmd.args[0].name, "[-level] [thread ...]");
        assert!(matches!(cmd.args[0].arg_type, ArgType::Argv));
    }

    /// Measured against C `softIoc` R7.0.10. `epicsThreadShow`, and
    /// `epicsThreadShow -2`, both print exactly what `epicsThreadShowAll`
    /// prints, trailer included — C reaches `epicsThreadShowAll(level)` once
    /// the argument list is empty, whether or not a level was consumed.
    #[test]
    fn epics_thread_show_without_a_thread_is_show_all() {
        let ctx = make_ctx();
        let (finish, join) = parked_ioc_thread("cbShowBare");

        let (all, all_err, _) = run_split(&ctx, "epicsThreadShowAll", &[]);
        for tokens in [vec![], vec!["-2"]] {
            let (out, err, failed) = run_split(&ctx, "epicsThreadShow", &tokens);
            assert!(!failed, "{tokens:?}");
            assert_eq!(out, all, "{tokens:?}");
            assert_eq!(err, all_err, "{tokens:?}");
        }

        finish.send(()).unwrap();
        join.join().unwrap();
    }

    /// A named thread: the header once, then that row, and no trailer — the
    /// trailer belongs to `epicsThreadShowAll`, which this path never reaches.
    #[test]
    fn epics_thread_show_by_name_prints_the_header_then_the_one_row() {
        let ctx = make_ctx();
        let (finish, join) = parked_ioc_thread("cbShowNamed");

        let (out, err, failed) = run_split(&ctx, "epicsThreadShow", &["cbShowNamed"]);
        assert!(!failed);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert_eq!(lines[0], crate::runtime::task::THREAD_SHOW_HEADER);
        assert!(lines[1].contains("cbShowNamed"), "{out}");
        assert_eq!(err, "", "no trailer on this path");

        // The `-level` prefix is consumed, not read as a thread.
        let (levelled, _, _) = run_split(&ctx, "epicsThreadShow", &["-2", "cbShowNamed"]);
        assert_eq!(levelled, out);

        finish.send(()).unwrap();
        join.join().unwrap();
    }

    /// C's only failing path: the diagnostic is a stderr line beginning with a
    /// tab, `iocshSetError(-1)` fails the line, and the remaining arguments are
    /// still processed — so the header appears because a *later* argument
    /// resolved.
    #[test]
    fn an_unknown_thread_name_fails_the_line_without_ending_it() {
        let ctx = make_ctx();
        let (finish, join) = parked_ioc_thread("cbShowAfterBad");

        let (out, err, failed) =
            run_split(&ctx, "epicsThreadShow", &["nosuchthread", "cbShowAfterBad"]);
        assert!(failed, "an unknown name is C's `iocshSetError(-1)`");
        assert_eq!(err, "\t'nosuchthread' is not a known thread name\n");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert_eq!(lines[0], crate::runtime::task::THREAD_SHOW_HEADER);
        assert!(lines[1].contains("cbShowAfterBad"), "{out}");

        // The same name alone prints nothing at all on stdout: nothing
        // resolved, so the header was never reached.
        let (only, only_err, only_failed) = run_split(&ctx, "epicsThreadShow", &["nosuchthread"]);
        assert!(only_failed);
        assert_eq!(only, "");
        assert_eq!(only_err, "\t'nosuchthread' is not a known thread name\n");

        finish.send(()).unwrap();
        join.join().unwrap();
    }

    /// The two numeric boundaries, both measured against C: a handle of zero
    /// is the header and nothing else — so it prints twice, once for C's
    /// `first` header and once for the entry itself — and an empty argument is
    /// zero, because `strtoull` converts nothing and leaves no remainder.
    #[test]
    fn a_zero_handle_is_the_header_and_an_empty_argument_is_zero() {
        let ctx = make_ctx();
        let header = crate::runtime::task::THREAD_SHOW_HEADER;
        for token in ["0", ""] {
            let (out, err, failed) = run_split(&ctx, "epicsThreadShow", &[token]);
            assert!(!failed, "{token:?}");
            assert_eq!(out, format!("{header}\n{header}\n"), "{token:?}");
            assert_eq!(err, "", "{token:?}");
        }
    }

    /// A number that resolves to no live thread is a stdout report, not a
    /// stderr diagnostic, and does not fail the line: C reads it as a thread
    /// that has since exited rather than as a bad command line.
    #[test]
    fn an_unresolvable_handle_reports_on_stdout_without_failing() {
        let ctx = make_ctx();
        let (out, err, failed) = run_split(&ctx, "epicsThreadShow", &["18446744073709551615"]);
        assert!(!failed);
        assert_eq!(err, "");
        assert_eq!(
            out,
            format!(
                "{}\nThread 0xffffffffffffffff (18446744073709551615) not found.\n",
                crate::runtime::task::THREAD_SHOW_HEADER
            )
        );
    }

    /// A handle from the listing resolves back to the row it came from — the
    /// only use a shell user has for the `EPICS ID` column.
    #[test]
    fn a_handle_from_the_listing_resolves_back_to_its_row() {
        let ctx = make_ctx();
        let (finish, join) = parked_ioc_thread("cbShowByHandle");
        let thread = crate::runtime::task::thread_by_name("cbShowByHandle").unwrap();

        for token in [format!("{:#x}", thread.id()), thread.os_id().to_string()] {
            let (out, _, failed) = run_split(&ctx, "epicsThreadShow", &[&token]);
            assert!(!failed, "{token}");
            let lines: Vec<&str> = out.lines().collect();
            assert_eq!(lines.len(), 2, "{token}: {out}");
            assert_eq!(lines[1], thread.show_line(), "{token}");
        }

        finish.send(()).unwrap();
        join.join().unwrap();
    }

    /// C `epicsThreadResumeArg0` is a single `iocshArgArgv` named
    /// `[thread ...]` (`libComRegister.c:409`), so `epicsThreadResume a b c`
    /// is three arguments to one declared parameter.
    #[test]
    fn epics_thread_resume_is_registered_with_cs_arity() {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg
            .get("epicsThreadResume")
            .expect("epicsThreadResume must be registered");
        assert_eq!(cmd.args.len(), 1);
        assert_eq!(cmd.args[0].name, "[thread ...]");
        assert!(matches!(cmd.args[0].arg_type, ArgType::Argv));
    }

    /// C `libComRegister.c:432-434`. Unlike `epicsThreadShow`, which prints
    /// its unknown-name line with a leading tab, this one has none.
    #[test]
    fn an_unknown_thread_name_fails_the_line_on_stderr() {
        let ctx = make_ctx();
        let (out, err, failed) = run_split(&ctx, "epicsThreadResume", &["noSuchThread"]);
        assert!(failed, "C `iocshSetError(-1)`");
        assert_eq!(out, "");
        assert_eq!(err, "'noSuchThread' is not a valid thread name\n");
    }

    /// The other half of C's `*endp` test (`:438-441`). A token that parses
    /// clean is a handle, so an unresolvable one is the *id* diagnostic —
    /// and unlike `epicsThreadShow`, which treats the same token as a thread
    /// that has since exited and reports it on stdout without failing, here
    /// it is a bad command line.
    #[test]
    fn a_handle_that_names_no_thread_is_the_id_diagnostic() {
        let ctx = make_ctx();
        for token in ["0", "18446744073709551615"] {
            let (out, err, failed) = run_split(&ctx, "epicsThreadResume", &[token]);
            assert!(failed, "{token}");
            assert_eq!(out, "", "{token}");
            assert_eq!(err, format!("'{token}' is not a valid thread id\n"));
        }
    }

    /// C `:450` — the resume itself, by either spelling. A thread that
    /// really is parked in `epicsThreadSuspendSelf` comes back, and the row
    /// reads `SUSPEND` up to the moment it does.
    ///
    /// This is the same call `dbc` makes on a lock set's continuation thread
    /// (`dbBkpt.c:518`), so `epicsThreadResume bkptCont` continues a stopped
    /// record exactly as `dbc` does — measured against softIoc @`R7.0.10`.
    #[test]
    fn a_suspended_thread_is_resumed_and_the_line_succeeds() {
        let ctx = make_ctx();
        for by_id in [false, true] {
            let name = if by_id {
                "cbResumeById"
            } else {
                "cbResumeByName"
            };
            let (id_tx, id_rx) = std::sync::mpsc::channel();
            let (woke_tx, woke_rx) = std::sync::mpsc::channel();
            let join = std::thread::Builder::new()
                .name(name.to_string())
                .spawn(move || {
                    let _ = crate::runtime::task::enter_ioc_thread(
                        crate::runtime::task::ThreadPriority::ScanLow,
                    );
                    id_tx
                        .send(crate::runtime::task::current_thread_id())
                        .unwrap();
                    crate::runtime::task::suspend_self();
                    woke_tx.send(()).unwrap();
                })
                .unwrap();
            let id = id_rx.recv().unwrap();
            for _ in 0..500 {
                if crate::runtime::task::thread_by_id(id).is_some_and(|t| t.is_suspended()) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let row = crate::runtime::task::thread_by_id(id).expect("row");
            assert!(row.is_suspended(), "{name} must reach suspend_self");
            assert!(
                row.show_line().ends_with(" SUSPEND"),
                "{:?}",
                row.show_line()
            );

            let token = if by_id {
                format!("{:#x}", id)
            } else {
                name.to_string()
            };
            let (out, err, failed) = run_split(&ctx, "epicsThreadResume", &[&token]);
            assert!(
                !failed,
                "a resume that acts does not fail the line: {token}"
            );
            assert_eq!(out, "", "{token}");
            assert_eq!(err, "", "{token}");
            woke_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap_or_else(|e| panic!("{token} must wake the thread: {e}"));
            join.join().unwrap();
        }
    }

    /// The arm a running thread reaches, by either spelling: C tests
    /// `epicsThreadIsSuspended` first and reports `:446-449` instead of
    /// resuming. C echoes the token as typed, not the resolved name.
    #[test]
    fn a_live_thread_resolves_and_reports_not_suspended() {
        let ctx = make_ctx();
        let (finish, join) = parked_ioc_thread("cbResumeTarget");
        let thread = crate::runtime::task::thread_by_name("cbResumeTarget").unwrap();

        for token in ["cbResumeTarget".to_string(), format!("{:#x}", thread.id())] {
            let (out, err, failed) = run_split(&ctx, "epicsThreadResume", &[&token]);
            assert!(failed, "{token}");
            assert_eq!(out, "", "{token}");
            assert_eq!(err, format!("Thread {token} is not suspended\n"));
        }

        finish.send(()).unwrap();
        join.join().unwrap();
    }

    /// C's loop starts at `argv[1]`, so a bare `epicsThreadResume` visits
    /// nothing at all — no usage message, no error status.
    #[test]
    fn no_arguments_resume_nothing_and_do_not_fail() {
        let ctx = make_ctx();
        let (out, err, failed) = run_split(&ctx, "epicsThreadResume", &[]);
        assert!(!failed);
        assert_eq!(out, "");
        assert_eq!(err, "");
    }

    /// Each of C's three arms ends in `continue`, not a return: one bad
    /// argument must not cost the operator the rest of the line.
    #[test]
    fn a_failing_argument_does_not_abandon_the_rest_of_the_line() {
        let ctx = make_ctx();
        let (finish, join) = parked_ioc_thread("cbResumeRest");
        let (out, err, failed) = run_split(
            &ctx,
            "epicsThreadResume",
            &["noSuchThread", "0", "cbResumeRest"],
        );
        assert!(failed);
        assert_eq!(out, "");
        assert_eq!(
            err,
            concat!(
                "'noSuchThread' is not a valid thread name\n",
                "'0' is not a valid thread id\n",
                "Thread cbResumeRest is not suspended\n",
            )
        );
        finish.send(()).unwrap();
        join.join().unwrap();
    }

    /// C `libComRegister.c:383-392`: two `iocshArgInt`s, in this order.
    #[test]
    fn epics_mutex_show_all_is_registered_with_cs_arity() {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg
            .get("epicsMutexShowAll")
            .expect("epicsMutexShowAll must be registered");
        assert_eq!(cmd.args.len(), 2);
        assert_eq!(cmd.args[0].name, "onlyLocked");
        assert_eq!(cmd.args[1].name, "level");
        assert!(matches!(cmd.args[0].arg_type, ArgType::Int));
        assert!(matches!(cmd.args[1].arg_type, ArgType::Int));
    }

    /// The three parts of C's output, in C's order, all on stdout. Measured
    /// against `softIoc` R7.0.10, whose idle listing is
    /// `ellCount(&mutexList) 21` / `PI is enabled` / 21 `epicsMutexId` rows.
    #[test]
    fn epics_mutex_show_all_prints_the_count_the_protocol_and_the_rows() {
        let ctx = make_ctx();
        let gate: crate::runtime::sync::PriorityInheritanceMutex<()> =
            crate::runtime::sync::PriorityInheritanceMutex::new(());

        let (out, err, failed) = run_split(&ctx, "epicsMutexShowAll", &["0", "0"]);
        assert!(!failed);
        assert_eq!(err, "", "C sends the whole report to stdout");
        let lines: Vec<&str> = out.lines().collect();
        let total = crate::runtime::sync::mutex_report(false).total;
        assert_eq!(lines[0], format!("ellCount(&mutexList) {total}"));
        assert_eq!(lines[1], crate::runtime::sync::osd_show_all_line());
        assert_eq!(
            lines.len(),
            2 + total,
            "one row per mutex when nothing is filtered:\n{out}"
        );
        assert!(
            lines[2..].iter().all(|l| l.starts_with("epicsMutexId 0x")),
            "{out}"
        );
        drop(gate);
    }

    /// `level` above 0 adds C's `epicsMutexOsdShow` line under each row, and
    /// C's `unsigned int` conversion means a negative argument selects it too.
    #[test]
    fn a_level_above_zero_adds_the_osd_line_under_every_row() {
        let ctx = make_ctx();
        let gate: crate::runtime::sync::PriorityInheritanceMutex<()> =
            crate::runtime::sync::PriorityInheritanceMutex::new(());

        let (plain, _, _) = run_split(&ctx, "epicsMutexShowAll", &["0", "0"]);
        for level in ["1", "-1"] {
            let (detailed, _, _) = run_split(&ctx, "epicsMutexShowAll", &["0", level]);
            let rows = plain.lines().count() - 2;
            assert_eq!(
                detailed.lines().count(),
                plain.lines().count() + rows,
                "level {level} must add one line per row:\n{detailed}"
            );
            assert!(
                detailed.lines().any(|l| l.starts_with(&format!(
                    "    {} uaddr=0x",
                    crate::runtime::sync::MUTEX_OSD_LABEL
                ))),
                "{detailed}"
            );
        }
        drop(gate);
    }

    /// `onlyLocked` is C's try-lock filter: the count stays the whole list, the
    /// rows shrink to what is held. On an idle IOC C prints the two header
    /// lines and nothing else, which is the state this asserts first.
    #[test]
    fn only_locked_filters_the_rows_but_not_the_count() {
        let ctx = make_ctx();
        let gate: crate::runtime::sync::PriorityInheritanceMutex<()> =
            crate::runtime::sync::PriorityInheritanceMutex::new(());

        let (idle, _, _) = run_split(&ctx, "epicsMutexShowAll", &["1", "0"]);
        let total = crate::runtime::sync::mutex_report(false).total;
        assert_eq!(
            idle,
            format!(
                "ellCount(&mutexList) {total}\n{}\n",
                crate::runtime::sync::osd_show_all_line()
            ),
            "nothing is held, so C prints only the two header lines"
        );

        let held = gate.lock();
        let (locked, _, _) = run_split(&ctx, "epicsMutexShowAll", &["1", "0"]);
        let lines: Vec<&str> = locked.lines().collect();
        assert_eq!(lines[0], format!("ellCount(&mutexList) {total}"));
        assert_eq!(lines.len(), 3, "exactly the one held mutex:\n{locked}");
        assert!(lines[2].starts_with("epicsMutexId 0x"), "{locked}");
        assert!(lines[2].contains("core_commands.rs"), "{locked}");
        drop(held);
    }

    /// The errlog / IOC-log surface a startup script needs. Without a table
    /// entry an `st.cmd` that says `iocLogInit` errors with "unknown
    /// command" and the IOC never forwards anything to the site's log
    /// server — which was the whole of 02 L1's observable.
    #[test]
    fn the_errlog_and_ioc_log_commands_are_registered_with_cs_arities() {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        for (name, args) in [
            ("eltc", 1usize),
            ("errlogInit", 1),
            ("errlogInit2", 2),
            ("errlogShow", 1),
            ("errlog", 1),
            ("iocLogInit", 0),
            ("iocLogPrefix", 1),
            ("iocLogShow", 1),
            ("setIocLogDisable", 1),
        ] {
            let cmd = reg
                .get(name)
                .unwrap_or_else(|| panic!("{name} must be registered"));
            assert_eq!(cmd.args.len(), args, "{name}: C's argument count");
        }
    }

    /// `eltc` reaches the setting the console gate reads, and restores it.
    #[test]
    fn eltc_from_the_shell_flips_the_console_setting() {
        let ctx = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg.get("eltc").unwrap();
        let args = parse_args(&["0".to_string()], &cmd.args).unwrap();
        cmd.handler.call(&args, &ctx).unwrap();
        assert!(!crate::runtime::log::errlog_to_console());
        let args = parse_args(&["1".to_string()], &cmd.args).unwrap();
        cmd.handler.call(&args, &ctx).unwrap();
        assert!(crate::runtime::log::errlog_to_console());
    }

    /// C's three level bands, measured on `softIoc` 7.0.10.1-DEV:
    ///
    /// ```text
    /// errlogShow 0 -> Error log: / buffer size: N / max message size: M
    /// errlogShow 1 -> ... + "  number of listeners: 0"
    /// errlogShow 2 -> ... + "  buffer(log) contents:" and the print pair
    /// ```
    ///
    /// The two SIZES differ from that binary — it is built from the 7.0
    /// branch, where `MIN_BUFFER_SIZE` is 2560 and the default max message
    /// size 512 (`errlog.c:44-45`, unreleased); released R7.0.10, which this
    /// port carries, has 1280 and 256. So the numbers here are the port's own
    /// constants and the LAYOUT is C's.
    #[test]
    fn errlog_show_reports_the_three_level_bands() {
        let ctx = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let show = |level: &str| {
            let cmd = reg
                .get("errlogShow")
                .expect("errlogShow must be registered");
            let args = parse_args(&[level.to_string()], &cmd.args).unwrap();
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let path = tmp.path().to_path_buf();
            ctx.with_output(std::fs::File::create(&path).unwrap(), || {
                cmd.handler.call(&args, &ctx).unwrap();
            });
            std::fs::read_to_string(&path).unwrap()
        };

        let sizes = format!(
            "Error log:\n  buffer size: {}\n  max message size: {}\n",
            crate::runtime::log::MIN_BUFFER_SIZE,
            crate::runtime::log::MIN_MESSAGE_SIZE
        );
        assert_eq!(show("0"), sizes);

        // One listener of our own, so the count is a fact this test set and
        // not whatever the process happened to hold.
        let id = crate::runtime::log::errlog_add_listener(|_| {});
        assert_eq!(
            show("1"),
            format!("{sizes}  number of listeners: 1\n"),
            "level 1 adds C's listener count"
        );
        assert!(crate::runtime::log::errlog_remove_listener(id));

        // Level 2 adds both buffers. The port names the write position
        // instead of drawing C's caret into an arena it does not keep.
        assert_eq!(
            show("2"),
            format!(
                "{sizes}  number of listeners: 0\n\
                 \x20 buffer(log) contents:\n\
                 \x20 buffer(log) position: 0 of {size} bytes\n\
                 \x20 buffer(print) contents:\n\
                 \x20 buffer(print) position: 0 of {size} bytes\n",
                size = crate::runtime::log::MIN_BUFFER_SIZE
            )
        );
    }

    /// `errlog <msg>` goes through `errlogPrintfNoConsole` with a newline
    /// appended and flushes before returning (`libComRegister.c:293-307`), so
    /// a listener has already seen the line when the command comes back.
    #[test]
    fn errlog_from_the_shell_reaches_a_listener_with_a_trailing_newline() {
        let ctx = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&seen);
        let id = crate::runtime::log::errlog_add_listener(move |m| {
            sink.lock().expect("sink").push(m.to_string());
        });
        let cmd = reg.get("errlog").unwrap();
        let args = parse_args(&["disk nearly full".to_string()], &cmd.args).unwrap();
        cmd.handler.call(&args, &ctx).unwrap();
        let lines = seen.lock().expect("sink").clone();
        crate::runtime::log::errlog_remove_listener(id);
        assert!(
            lines.iter().any(|l| l == "disk nearly full\n"),
            "the command must have flushed before returning: {lines:?}"
        );
    }

    /// The four commands the shell executes itself must still appear in the
    /// command table. C registers its own shell-internal commands for exactly
    /// this reason — "Dummy internal commands -- register and install in
    /// command table so they show up in the help display"
    /// (`iocsh.cpp:1577-1580` @R7.0.10) — and the observable is `help`: with no
    /// table entry `help iocshLoad` prints nothing and `help` omits the name,
    /// so a startup script author cannot discover the usage from the shell.
    #[test]
    fn the_shell_executed_commands_are_still_in_the_command_table() {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        for (name, args) in [
            ("iocshCmd", 1usize),
            ("iocshRun", 2),
            ("iocshLoad", 2),
            ("on", 1),
        ] {
            let cmd = reg
                .get(name)
                .unwrap_or_else(|| panic!("{name} must be in the command table for help"));
            assert_eq!(cmd.args.len(), args, "{name}: C's argument count");
            // The name comes from the synopsis line `help` renders, not
            // from the usage text, which is C's description alone.
            assert!(
                super::super::format_help_entry(cmd, false, true).contains(name),
                "`help {name}` must name the command"
            );
        }
    }

    /// And the table entry must never be the thing that runs them. Reaching a
    /// registered handler means `execute_expanded_line` stopped intercepting
    /// the name — a routing bug that would silently change the semantics
    /// (`iocshCmd` runs a line in a scope of its own), so it fails loudly
    /// rather than doing nothing.
    #[test]
    fn a_shell_executed_command_refuses_to_run_from_the_table() {
        let ctx = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        for name in ["iocshCmd", "iocshRun", "iocshLoad", "on"] {
            let cmd = reg.get(name).unwrap();
            let Err(err) = cmd.handler.call(&[], &ctx) else {
                panic!("{name}: the table entry must not execute");
            };
            assert!(err.contains(name), "{name}: {err}");
        }
    }

    /// `epicsThreadSleep` takes an `iocshArgDouble`
    /// (`libComRegister.c:398-405,510` @R7.0.10), so a startup script
    /// can hand it any double C's `nanosleep` will refuse. C returns
    /// from each of those at once — measured against `softIoc`, `1e300`
    /// / `inf` / `nan` / `-5` all completed inside the 0.33 s startup
    /// baseline — where this command used to panic in
    /// `Duration::from_secs_f64`.
    #[test]
    fn epics_thread_sleep_survives_every_delay_c_refuses() {
        let ctx = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg.get("epicsThreadSleep").unwrap();
        for arg in ["1e300", "inf", "nan", "-5", "0"] {
            let tokens = vec![arg.to_string()];
            let args = parse_args(&tokens, &cmd.args).unwrap();
            let t = std::time::Instant::now();
            cmd.handler.call(&args, &ctx).unwrap();
            assert!(
                t.elapsed() < std::time::Duration::from_millis(50),
                "epicsThreadSleep {arg} must return at once, as C does"
            );
        }
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
        let now = chrono::Local::now();
        assert_eq!(
            epics_strftime_to_chrono("%Y/%m/%d %H:%M:%S.%06f", &now),
            "%Y/%m/%d %H:%M:%S.%6f"
        );
        assert_eq!(epics_strftime_to_chrono("%f", &now), "%9f");
        assert_eq!(epics_strftime_to_chrono("%d%%%H", &now), "%d%%%H");
        // C's strftime copies an unknown conversion out literally; chrono
        // would error on it, so it becomes chrono's literal per cent.
        assert_eq!(epics_strftime_to_chrono("%Q", &now), "%%Q");
        assert_eq!(epics_strftime_to_chrono("a%", &now), "a%%");
    }

    /// C `date "%Q"` prints `%Q`: `strftime` copies an unknown conversion
    /// out and cannot fail. Measured against
    /// `~/work/epics-base/bin/linux-x86_64/softIoc`; the port panicked the
    /// shell thread instead.
    #[test]
    fn an_unknown_conversion_prints_itself_instead_of_panicking() {
        let ctx = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg.get("date").unwrap();
        let args = parse_args(&["%Q".to_string()], &cmd.args).unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        ctx.with_output(std::fs::File::create(&path).unwrap(), || {
            cmd.handler.call(&args, &ctx).unwrap();
        });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "%Q\n");
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
    /// (`libComRegister.c:467-473`, `:514` at the review's `R7.0.10` pin;
    /// this machine's checkout is +17 lines there, so the same code reads
    /// `:483-489`/`:531` against the working tree) as the operator's opt-in into
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

    /// C registers `generalTimeReport` with one `iocshArgInt`
    /// (`libComRegister.c:455-457,513`) and prints
    /// `generalTimeReport(level)` verbatim. The port already owned the
    /// report; only the shell entry point was missing, so the command's
    /// job is to emit that string with nothing added and nothing lost.
    #[test]
    fn general_time_report_prints_the_report_verbatim() {
        let ctx = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg.get("generalTimeReport").expect("C registers this");
        assert_eq!(cmd.args.len(), 1, "C declares one argument");

        for level in ["0", "1"] {
            let args = parse_args(&[level.to_string()], &cmd.args).unwrap();
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let path = tmp.path().to_path_buf();
            ctx.with_output(std::fs::File::create(&path).unwrap(), || {
                cmd.handler.call(&args, &ctx).unwrap();
            });
            let out = std::fs::read_to_string(&path).unwrap();
            assert!(
                out.starts_with("Backwards time errors prevented "),
                "C's first line is the error count: {out:?}"
            );
            assert!(
                out.contains("Current Time Providers:") && out.contains("Event Time Providers:"),
                "C prints both headers: {out:?}"
            );
            // Nothing added: the command re-emits the report's own
            // trailing newline rather than a second one.
            assert!(!out.ends_with("\n\n\n"), "no newline added: {out:?}");
        }

        // A missing argument is C's zeroed `argBuf` — level 0, not an error.
        let args = parse_args(&[], &cmd.args).unwrap();
        assert!(cmd.handler.call(&args, &ctx).is_ok());
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
