//! The OS-clock time-provider iocsh commands from `osiClockTime.c`
//! (@R7.0.10).
//!
//! C's file declares three — `ClockTime_Init`, `ClockTime_Shutdown` and
//! `ClockTime_Report` — but registers the first two inside
//! `#if defined(vxWorks) || defined(__rtems__)` (`osiClockTime.c:106-110`),
//! so a hosted C IOC answers only `ClockTime_Report`. Both are the shell face
//! of a `ClockTimeSync` thread this port starts on no target, so only the
//! report has a subject here.
//!
//! `osiNTPTime.c`'s `NTPTime_Report` and `NTPTime_Shutdown` are not in this
//! file for the same reason one step further out: that source is compiled only
//! for vxWorks and RTEMS (`libcom/src/osi/Makefile:84-85`), so they are not
//! commands a hosted C IOC has at all.

use super::registry::*;

pub(crate) fn register(registry: &mut CommandRegistry) {
    registry.register(cmd_clock_time_report());
}

/// `ClockTime_Report` — C `osiClockTime.c:60-93,266-312`.
///
/// The subject is [`crate::runtime::general_time::clock_time_report`], which
/// owns the byte shape; this is the shell entry point. C's `ClockTime_Report`
/// returns 0 on every path and is registered without `iocshSetError`, so the
/// command cannot fail.
fn cmd_clock_time_report() -> CommandDef {
    CommandDef::new(
        "ClockTime_Report",
        vec![ArgDesc {
            // C `ReportArg0` (`osiClockTime.c:80`). Declared `iocshArgArgv`
            // there and read back as `.ival`, which is the deviation
            // `clock_time_report` documents; the port declares the integer C
            // meant.
            name: "interest_level",
            arg_type: ArgType::Int,
        }],
        "ClockTime_Report <interest_level> — Report the IOC's OS clock \
         synchronization status.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let level = match args.first() {
                Some(ArgValue::Int(n)) => *n as i32,
                _ => 0,
            };
            let report = crate::runtime::general_time::clock_time_report(level);
            let body = report.strip_suffix('\n').unwrap_or(&report);
            ctx.print_fmt(format_args!("{body}"));
            Ok(CommandOutcome::Continue)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_is_registered_with_c_s_name_and_arity() {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg.get("ClockTime_Report").expect("C registers this");
        assert_eq!(cmd.args.len(), 1, "C `ReportFuncDef` declares 1 arg");
        assert_eq!(cmd.args[0].name, "interest_level");
        assert!(matches!(cmd.args[0].arg_type, ArgType::Int));
    }

    /// The hosted shape is C's `else` branch and nothing else: no
    /// synchronization lines, because no `ClockTimeSync` thread exists here.
    #[test]
    fn the_report_is_c_s_unsynchronized_branch() {
        let out = crate::runtime::general_time::clock_time_report(0);
        let mut lines = out.lines();
        let first = lines.next().expect("one line at least");
        assert!(
            first.starts_with("Program started at "),
            "C prints this verbatim: {first:?}"
        );
        // `%Y-%m-%d %H:%M:%S.%06f` — 26 characters.
        assert_eq!(
            first.len(),
            "Program started at ".len() + 26,
            "C's epicsTimeToStrftime width: {first:?}"
        );
        let rest: Vec<&str> = lines.collect();
        if cfg!(any(target_os = "vxworks", target_os = "rtems")) {
            assert_eq!(
                rest,
                ["IOC's OS Clock synchronization thread is not running."]
            );
        } else {
            assert!(rest.is_empty(), "C guards that line out here: {rest:?}");
        }
        // The level argument changes nothing on this branch, as in C.
        assert_eq!(
            crate::runtime::general_time::clock_time_report(1)
                .lines()
                .count(),
            out.lines().count()
        );
    }
}
