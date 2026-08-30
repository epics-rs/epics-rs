//! The operator commands EPICS base registers from `iocshRegisterRTEMS`
//! (`libcom/RTEMS/posix/rtems_init.c:692-705` @R7.0.10).
//!
//! Five of base's six: `netstat`, `heapSpace`, `zoneset`, `rt` and
//! `setlogmask`. The sixth, `nfsMount`, names an API that does not exist on
//! the network stack this port targets — see
//! [`epics_rtems_boot::shell`] for the argument, which is where the rest of
//! the reasoning about these commands lives too.
//!
//! # Why this file is not reached from `register_builtins`
//!
//! C registers these from `iocshRegisterRTEMS`, which the RTEMS boot path
//! calls — not from a `*IocRegister.c` that every IOC runs. An IOC has them
//! because it booted on RTEMS, so the port hooks them the same way:
//! [`register_rtems_commands`] is called by the RTEMS IOC binaries' `main`
//! and by nothing else. A hosted `softioc-rs` does not grow the names, exactly
//! as a Linux `softIoc` does not.
//!
//! That is a different rule from `ClockTime_Init`/`ClockTime_Shutdown` in
//! `time_commands.rs`, which C registers from libCom's own initialisation
//! under `#if defined(vxWorks) || defined(__rtems__)` — a compile-time OS
//! condition, ported as one. Two C mechanisms, two seats.
//!
//! # Where the work happens
//!
//! The behaviour is `epics_rtems_boot`'s: it owns the RTEMS and libbsd calls,
//! and it is the crate the RTEMS image already links. This file is the shell
//! face — argument descriptors, C's usage text, C's diagnostics — and it lives
//! here because [`CommandDef`] is this crate's type and `epics-rtems-boot` is
//! one of its dependencies. **Deviation, forced:** the brief's literal reading
//! puts the whole command in `epics-rtems-boot`; a `CommandDef` there would be
//! a dependency cycle.
//!
//! # Output routing
//!
//! `heapSpace` and `setlogmask` print through [`CommandContext`], so a `>`
//! redirect in the shell captures them. C's callbacks use `printf`, which
//! writes to the process's stdout and is *not* what `epicsGetThreadStdout`
//! redirects — so this port's output is redirectable where base's is not.
//! `netstat` and `rt` are not: their text is written by C code inside libbsd
//! and the RTEMS shell, straight to descriptor 1, on both this port and base.

use epics_rtems_boot::shell::{self, ShellError};
use epics_rtems_boot::stats::{self, HeapSpace};

use super::registry::*;

/// Register the RTEMS operator commands on the process command table — C
/// `iocshRegisterRTEMS` (`rtems_init.c:692-705`).
///
/// Call it from an RTEMS IOC's `main`, before the shell starts. C calls
/// `rtems_shell_init_environment()` here as its last act (`:704`); this port
/// does that inside the `rt` lookup instead, so the ordering holds by
/// construction rather than by remembering to register in the right order.
pub fn register_rtems_commands() {
    for def in rtems_command_defs() {
        super::register_command(def);
    }
}

/// The definitions, in C's registration order.
///
/// Split out from [`register_rtems_commands`] so the set can be tested on a
/// host, where the process command table is a hosted IOC's and must not grow
/// these names.
fn rtems_command_defs() -> Vec<CommandDef> {
    vec![
        cmd_netstat(),
        cmd_heap_space(),
        cmd_zoneset(),
        cmd_rt(),
        cmd_setlogmask(),
    ]
}

/// `netstat <level>` — C `netStatFuncDef`/`netStatCallFunc`
/// (`rtems_init.c:545-551`).
///
/// C's callback has no failure path: `rtems_netstat` returns nothing and the
/// command is registered without `iocshSetError`. The only failure here is a
/// build with no RTEMS behind it, which the registration above cannot produce.
fn cmd_netstat() -> CommandDef {
    CommandDef::new(
        "netstat",
        vec![ArgDesc {
            name: "level",
            arg_type: ArgType::Int,
        }],
        "show network status",
        |args: &[ArgValue], _ctx: &CommandContext| {
            let level = match args.first() {
                Some(ArgValue::Int(n)) => *n as i32,
                _ => 0,
            };
            shell::netstat(level).map_err(|e| e.to_string())?;
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `heapSpace` — C `heapSpaceFuncDef`/`heapSpaceCallFunc`
/// (`rtems_init.c:562-590`).
fn cmd_heap_space() -> CommandDef {
    CommandDef::new(
        "heapSpace",
        vec![],
        "show malloc statistic",
        |_args: &[ArgValue], ctx: &CommandContext| {
            let Some(heap) = stats::heap_space() else {
                return Err("heapSpace: no malloc statistics on this build".to_string());
            };
            ctx.print_fmt(format_args!("{}", heap_space_line(heap)));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// C's one line of `heapSpace` output, byte for byte
/// (`rtems_init.c:573-577`).
///
/// Pure, so the two branches and their boundary are testable on a host that
/// has no heap to read.
///
/// The number is [`HeapSpace::free`] — C's
/// `size - (lifetime_allocated - lifetime_freed)`, computed there in `double`
/// after an unsigned subtraction and here saturating, which is the deviation
/// `HeapSpace::free` documents.
fn heap_space_line(heap: HeapSpace) -> String {
    let free = heap.free() as f64;
    // C's threshold and its two units. `%.1f` in both arms.
    if free >= (1024 * 1024) as f64 {
        format!("Heap space: {:.1} MB", free / (1024.0 * 1024.0))
    } else {
        format!("Heap space: {:.1} kB", free / 1024.0)
    }
}

/// `zoneset <zone string>` — C `zonesetFuncDef`/`zonesetCallFunc`
/// (`rtems_init.c:637-647`).
///
/// C's `zoneset` returns non-zero only when `setenv`/`unsetenv` fails, which
/// `iocshSetError` then fails the line on.
fn cmd_zoneset() -> CommandDef {
    CommandDef::new(
        "zoneset",
        vec![ArgDesc {
            name: "zone string",
            arg_type: ArgType::String,
        }],
        "set timezone (obsolete?)",
        |args: &[ArgValue], _ctx: &CommandContext| {
            // C reads `args[0].sval`, which is NULL when the line ended
            // before this argument — that is the branch that unsets TZ.
            let zone = match args.first() {
                Some(ArgValue::String(s)) => Some(s.as_str()),
                _ => None,
            };
            // SAFETY: `zoneset` writes the environment, which is unsound while
            // another thread reads it. This is C's hazard unchanged: base's
            // `zonesetCallFunc` calls `setenv` from the shell thread of a
            // running IOC. The operator typing it is asserting the same thing
            // base's operator asserts.
            unsafe { shell::zoneset(zone) }.map_err(|e| e.to_string())?;
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `rt <cmd> <args...>` — C `rtshellFuncDef`/`rtshellCallFunc`
/// (`rtems_init.c:495-523`).
///
/// C's `args[1].aval` starts at the token that named the shell command, so
/// `av[0]` is that name and `ac` counts it. This port's [`ArgValue::Argv`]
/// carries the tokens *after* the descriptors that precede it, so the name has
/// to be put back at the front — see
/// [`epics_rtems_boot::shell::run_shell_command`], whose contract is that
/// `argv[0]` is the command name.
fn cmd_rt() -> CommandDef {
    CommandDef::new(
        "rt",
        vec![
            ArgDesc {
                name: "cmd",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "args",
                arg_type: ArgType::Argv,
            },
        ],
        "run rtems shell command",
        |args: &[ArgValue], _ctx: &CommandContext| {
            let argv = rt_argv(args);
            match shell::run_shell_command(&argv) {
                // C `iocshSetError(ret)` plus `if(ret) fprintf(stderr, ...)`.
                Ok(0) => Ok(CommandOutcome::Continue),
                Ok(status) => Err(format!("ERR: {status}")),
                // C `rtems_shell_lookup_cmd` returning NULL, including the
                // NULL name a bare `rt` hands it.
                Err(ShellError::NoSuchCommand) => Err("ERR: No such command".to_string()),
                Err(other) => Err(other.to_string()),
            }
        },
    )
}

/// C's `argv` for the shell command: the name first, then the rest of the
/// line.
///
/// Its own function so the reconstruction is testable without a shell — this
/// is the one place the two argv conventions meet, and a handler that got it
/// wrong would silently drop the shell command's first argument.
fn rt_argv(args: &[ArgValue]) -> Vec<String> {
    let mut argv = Vec::new();
    if let Some(ArgValue::String(cmd)) = args.first() {
        argv.push(cmd.clone());
    }
    if let Some(ArgValue::Argv(rest)) = args.get(1) {
        argv.extend(rest.iter().cloned());
    }
    argv
}

/// `setlogmask <level name>` — C `setlogmaskFuncDef`/`setlogmaskCallFunc`
/// (`rtems_init.c:655-686`).
///
/// With no argument C prints the usage and the level names and does *not*
/// fail the line; with an unknown name it prints one line and fails.
fn cmd_setlogmask() -> CommandDef {
    CommandDef::new(
        "setlogmask",
        vec![ArgDesc {
            name: "level name",
            arg_type: ArgType::String,
        }],
        "Set syslog() threshold level",
        |args: &[ArgValue], ctx: &CommandContext| {
            let Some(ArgValue::String(name)) = args.first() else {
                ctx.print_fmt(format_args!(
                    "{}",
                    setlogmask_usage(&shell::log_priority_names())
                ));
                return Ok(CommandOutcome::Continue);
            };
            match shell::set_log_priority(name) {
                Ok(()) => Ok(CommandOutcome::Continue),
                // C prints this and calls `iocshSetError(-1)`.
                Err(ShellError::UnknownLevel) => Err("Error: unknown log level.".to_string()),
                Err(other) => Err(other.to_string()),
            }
        },
    )
}

/// C's no-argument `setlogmask` output (`rtems_init.c:665-671`), without the
/// trailing newline [`CommandContext::print_fmt`] adds.
fn setlogmask_usage(levels: &[String]) -> String {
    let mut out = String::from("Usage: setlogmask <level>\n\n  Level names:");
    for level in levels {
        out.push_str("\n    ");
        out.push_str(level);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C's six minus `nfsMount`, under C's names and C's arities. A name or an
    /// arity that drifts here is a command an operator's existing script stops
    /// finding.
    #[test]
    fn the_set_is_c_s_names_and_arities() {
        let defs = rtems_command_defs();
        let shape: Vec<(String, usize)> = defs
            .iter()
            .map(|d| (d.name.clone(), d.args.len()))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("netstat".to_string(), 1),
                ("heapSpace".to_string(), 0),
                ("zoneset".to_string(), 1),
                ("rt".to_string(), 2),
                ("setlogmask".to_string(), 1),
            ],
            "C `iocshRegisterRTEMS` order, names and argument counts"
        );
    }

    /// `nfsMount` must stay out. Registering a name whose implementation
    /// cannot exist would put it in `help`, which is worse than its absence —
    /// see `epics_rtems_boot::shell` for why the API is gone.
    #[test]
    fn nfs_mount_is_not_registered() {
        assert!(!rtems_command_defs().iter().any(|d| d.name == "nfsMount"));
    }

    /// C's usage strings verbatim, because `help` prints them.
    #[test]
    fn the_usage_text_is_c_s() {
        let defs = rtems_command_defs();
        let usage = |name: &str| {
            defs.iter()
                .find(|d| d.name == name)
                .expect("registered above")
                .usage
                .clone()
        };
        assert_eq!(usage("netstat"), "show network status");
        assert_eq!(usage("heapSpace"), "show malloc statistic");
        assert_eq!(usage("zoneset"), "set timezone (obsolete?)");
        assert_eq!(usage("rt"), "run rtems shell command");
        assert_eq!(usage("setlogmask"), "Set syslog() threshold level");
    }

    /// C's argument names and types, which `help` renders into the synopsis
    /// line an operator reads.
    #[test]
    fn the_argument_descriptors_are_c_s() {
        let defs = rtems_command_defs();
        let arg = |name: &str, i: usize| {
            let d = defs.iter().find(|d| d.name == name).expect("registered");
            (
                d.args[i].name,
                format!("{:?}", d.args[i].arg_type).to_string(),
            )
        };
        assert_eq!(arg("netstat", 0), ("level", "Int".to_string()));
        assert_eq!(arg("zoneset", 0), ("zone string", "String".to_string()));
        assert_eq!(arg("rt", 0), ("cmd", "String".to_string()));
        assert_eq!(arg("rt", 1), ("args", "Argv".to_string()));
        assert_eq!(arg("setlogmask", 0), ("level name", "String".to_string()));
    }

    /// The unit boundary is C's `x >= 1024*1024`, and the number is C's
    /// `size - (allocated - freed)`.
    #[test]
    fn the_heap_space_line_is_c_s_two_branches() {
        let heap = |free: u64| HeapSpace {
            size: free,
            lifetime_allocated: 0,
            lifetime_freed: 0,
        };
        assert_eq!(
            heap_space_line(heap(1024 * 1024)),
            "Heap space: 1.0 MB",
            "the boundary itself is the MB branch in C"
        );
        assert_eq!(
            heap_space_line(heap(1024 * 1024 - 1)),
            "Heap space: 1024.0 kB",
            "one byte below it is still kB, as in C"
        );
        assert_eq!(heap_space_line(heap(0)), "Heap space: 0.0 kB");
        assert_eq!(
            heap_space_line(HeapSpace {
                size: 8 * 1024 * 1024,
                lifetime_allocated: 6 * 1024 * 1024,
                lifetime_freed: 2 * 1024 * 1024,
            }),
            "Heap space: 4.0 MB",
            "C subtracts the live bytes from the heap size"
        );
    }

    /// The shell command's own `argv[0]` is its name — the convention this
    /// port's `Argv` does not carry, and the one every `getopt` in the RTEMS
    /// shell assumes.
    #[test]
    fn rt_puts_the_command_name_back_at_argv_zero() {
        let args = vec![
            ArgValue::String("stackuse".to_string()),
            ArgValue::Argv(vec!["-v".to_string(), "2".to_string()]),
        ];
        assert_eq!(rt_argv(&args), vec!["stackuse", "-v", "2"]);
    }

    /// A shell command with no arguments still gets its own name, so `ac` is
    /// 1 and not 0 — C's `aval.ac` counts the name too.
    #[test]
    fn rt_with_no_arguments_still_passes_the_name() {
        let args = vec![
            ArgValue::String("stackuse".to_string()),
            ArgValue::Argv(vec![]),
        ];
        assert_eq!(rt_argv(&args), vec!["stackuse"]);
    }

    /// A bare `rt` has no name to look up. C hands `rtems_shell_lookup_cmd` a
    /// NULL and prints `ERR: No such command`; an empty argv is how that
    /// reaches the same answer here.
    #[test]
    fn a_bare_rt_produces_an_empty_argv() {
        assert!(rt_argv(&[ArgValue::Missing, ArgValue::Argv(vec![])]).is_empty());
    }

    /// C's usage block, indentation included (`rtems_init.c:665-671`).
    #[test]
    fn the_setlogmask_usage_is_c_s_block() {
        let levels = vec!["emerg".to_string(), "alert".to_string()];
        assert_eq!(
            setlogmask_usage(&levels),
            "Usage: setlogmask <level>\n\n  Level names:\n    emerg\n    alert"
        );
    }

    /// A build with no RTEMS behind it has no level names, and the block must
    /// still be the block — C prints the header before the loop.
    #[test]
    fn the_setlogmask_usage_survives_an_empty_level_list() {
        assert_eq!(
            setlogmask_usage(&[]),
            "Usage: setlogmask <level>\n\n  Level names:"
        );
    }
}
