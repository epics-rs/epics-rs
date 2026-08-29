// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that hand-builds its own tokio runtime; runs and passes in the exec-backend suite.
//! The callback- and scanOnce-queue iocsh commands from
//! `dbIocRegister.c` (@R7.0.10): `scanOnceSetQueueSize`,
//! `scanOnceQueueShow`, `callbackSetQueueSize`, `callbackQueueShow`
//! and `callbackParallelThreads`.
//!
//! All five reach the same two facilities the port already runs — the
//! priority-banded callback pool and the `scanOnce` ring in
//! `epics_libcom_rs::runtime::background` — through the process-global
//! executor in `runtime::task`. The two sizing commands and
//! `callbackParallelThreads` write module state that the pool reads
//! once, when it is built, which is why C refuses them after
//! `callbackInit` and why this port refuses them after
//! `background_started()`.
//!
//! Two C error paths in `callbackParallelThreads` (`callback.c:181-190`)
//! are not reproduced because they cannot arise here: `pdbbase not set`
//! and `No Priority menu` both report a database that has not loaded
//! `menuPriority`, and the port's `menuPriority` is the compile-time
//! `dbd_generated::MENU_PRIORITY`, which always exists.

use super::registry::*;
use crate::runtime::background::{CallbackPriority, CallbackQueueStats};

/// Register the queue-facility command set on `registry`.
pub(crate) fn register(registry: &mut CommandRegistry) {
    registry.register(cmd_scan_once_set_queue_size());
    registry.register(cmd_scan_once_queue_show());
    registry.register(cmd_callback_set_queue_size());
    registry.register(cmd_callback_queue_show());
    registry.register(cmd_callback_parallel_threads());
}

/// C `callbackQueueShow`'s refusal (`callback.c:145-146`), one line,
/// verbatim.
const CALLBACK_NOT_INIT: &str =
    "Callback system not initialized, yet. Please run iocInit before using this command.";

/// C `scanOnceQueueShow`'s refusal (`dbScan.c:762-763`), one line,
/// verbatim.
const SCAN_ONCE_NOT_INIT: &str =
    "scanOnce system not initialized, yet. Please run iocInit before using this command.";

/// C `callbackSetQueueSize`'s two diagnostics (`callback.c:103`, `:107`).
const QUEUE_SIZE_MUST_BE_POSITIVE: &str = "Queue size must be positive";
const CALLBACK_ALREADY_INIT: &str = "Callback system already initialized";

/// C `threadNamePrefix[]` (`callback.c:86-88`) — what
/// `callbackQueueShow` puts in the PRIORITY column. Not the
/// `menuPriority` choices `callbackParallelThreads` matches against;
/// the two spellings belong to different C tables.
const BAND_NAMES: [&str; 3] = ["cbLow", "cbMedium", "cbHigh"];

/// The table both `callbackQueueShow` and `scanOnceQueueShow` print
/// (`callback.c:149-158`, `dbScan.c:765-771`) — the same header and the
/// same `%8s  %15d  %10d  %6d  %6.1f  %11d` row in both, which is why
/// one formatter serves both.
fn queue_stats_table(rows: &[(&str, CallbackQueueStats)]) -> Vec<String> {
    let mut out =
        vec!["PRIORITY  HIGH-WATER MARK  ITEMS IN Q  Q SIZE  % USED  Q OVERFLOWS".to_string()];
    for (name, st) in rows {
        let qusage = 100.0 * st.num_used as f64 / st.size as f64;
        out.push(format!(
            "{:>8}  {:>15}  {:>10}  {:>6}  {:>6.1}  {:>11}",
            name, st.max_used, st.num_used, st.size, qusage, st.num_overflow
        ));
    }
    out
}

/// The first positional argument as C's `args[0].ival`: an omitted
/// `iocshArgInt` reaches the handler as 0.
fn ival(args: &[ArgValue], index: usize) -> i64 {
    match args.get(index) {
        Some(ArgValue::Int(n)) => *n,
        _ => 0,
    }
}

/// C `callbackParallelThreads`'s count arithmetic (`callback.c:167-171`):
/// a negative count is relative to the CPU count, zero means
/// `callbackParallelThreadsDefault`, and the result floors at 1.
fn resolve_thread_count(count: i64, cpus: i64, default: i64) -> usize {
    let n = if count < 0 {
        cpus + count
    } else if count == 0 {
        default
    } else {
        count
    };
    n.max(1) as usize
}

/// C's priority-name lookup (`callback.c:191-203`): `NULL`, `""` and
/// `"*"` mean every band, anything else is matched case-insensitively
/// against the `menuPriority` choice values with `epicsStrCaseCmp`.
/// `Err(name)` is C's "Unknown priority" fall-through.
fn priority_from_name(prio: Option<&str>) -> Result<Option<CallbackPriority>, ()> {
    let Some(name) = prio else {
        return Ok(None);
    };
    if name.is_empty() || name == "*" {
        return Ok(None);
    }
    for (i, choice) in crate::server::record::dbd_generated::MENU_PRIORITY
        .iter()
        .enumerate()
    {
        if choice.eq_ignore_ascii_case(name) {
            return Ok(Some(CallbackPriority::ALL[i]));
        }
    }
    Err(())
}

/// `scanOnceSetQueueSize <size>` — C `scanOnceSetQueueSize`
/// (`dbScan.c:728-732`), the bare assignment `onceQueueSize = size`. It
/// validates nothing, prints nothing, and always returns 0, so the line
/// never fails however absurd the size.
fn cmd_scan_once_set_queue_size() -> CommandDef {
    CommandDef::new(
        "scanOnceSetQueueSize",
        vec![ArgDesc {
            name: "size",
            arg_type: ArgType::Int,
        }],
        "scanOnceSetQueueSize <size> — Change size of Scan once queue. \
         Must be called before iocInit().",
        |args: &[ArgValue], _ctx: &CommandContext| {
            let size = ival(args, 0);
            crate::runtime::background::scan_once::set_queue_size(size.max(1) as usize);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `scanOnceQueueShow <reset>` — C `scanOnceQueueShow`
/// (`dbScan.c:759-773`). `void` in C and registered without
/// `iocshSetError` (`dbIocRegister.c:444-447`), so even the refusal
/// leaves the line successful.
fn cmd_scan_once_queue_show() -> CommandDef {
    CommandDef::new(
        "scanOnceQueueShow",
        vec![ArgDesc {
            name: "reset",
            arg_type: ArgType::Int,
        }],
        "scanOnceQueueShow <reset> — Show details and statistics of scan once queue processing.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let reset = ival(args, 0) != 0;
            match crate::runtime::task::background_scan_once_stats(reset) {
                None => ctx.eprintln(SCAN_ONCE_NOT_INIT),
                Some(st) => {
                    let row = CallbackQueueStats {
                        size: st.size,
                        num_used: st.num_used,
                        max_used: st.max_used,
                        num_overflow: st.num_overflow,
                    };
                    for line in queue_stats_table(&[("scanOnce", row)]) {
                        ctx.println(&line);
                    }
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `callbackSetQueueSize <bufsize>` — C `callbackSetQueueSize`
/// (`callback.c:101-113`). The size check comes before the
/// already-initialised guard in C, so a non-positive size is reported
/// as such even on a running IOC.
fn cmd_callback_set_queue_size() -> CommandDef {
    CommandDef::new(
        "callbackSetQueueSize",
        vec![ArgDesc {
            name: "bufsize",
            arg_type: ArgType::Int,
        }],
        "callbackSetQueueSize <bufsize> — Change depth of queue for callback workers. \
         Must be called before iocInit().",
        |args: &[ArgValue], ctx: &CommandContext| {
            let size = ival(args, 0);
            if size <= 0 {
                ctx.eprintln(QUEUE_SIZE_MUST_BE_POSITIVE);
                return Ok(CommandOutcome::Failed);
            }
            if crate::runtime::task::background_started() {
                ctx.eprintln(CALLBACK_ALREADY_INIT);
                return Ok(CommandOutcome::Failed);
            }
            crate::runtime::background::callback_executor::set_queue_size(size as usize);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `callbackQueueShow <reset>` — C `callbackQueueShow`
/// (`callback.c:143-159`). Like `scanOnceQueueShow` it is `void` and
/// registered without `iocshSetError` (`dbIocRegister.c:500-503`).
fn cmd_callback_queue_show() -> CommandDef {
    CommandDef::new(
        "callbackQueueShow",
        vec![ArgDesc {
            name: "reset",
            arg_type: ArgType::Int,
        }],
        "callbackQueueShow <reset> — Show status of callback thread processing queue.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let reset = ival(args, 0) != 0;
            match crate::runtime::task::background_callback_stats(reset) {
                None => ctx.eprintln(CALLBACK_NOT_INIT),
                Some(bands) => {
                    let rows: Vec<(&str, CallbackQueueStats)> = BAND_NAMES
                        .iter()
                        .copied()
                        .zip(bands.iter().copied())
                        .collect();
                    for line in queue_stats_table(&rows) {
                        ctx.println(&line);
                    }
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `callbackParallelThreads <no of threads> <priority>` — C
/// `callbackParallelThreads` (`callback.c:160-208`). The
/// already-initialised guard fires first, before the count arithmetic
/// and before the priority is looked at.
fn cmd_callback_parallel_threads() -> CommandDef {
    CommandDef::new(
        "callbackParallelThreads",
        vec![
            ArgDesc {
                name: "no of threads",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "priority",
                arg_type: ArgType::String,
            },
        ],
        "callbackParallelThreads <no of threads> <priority> — Configure multiple workers for a \
         given callback queue priority level. priority may be omitted or \"*\" to act on all \
         priorities or one of LOW, MEDIUM, or HIGH.",
        |args: &[ArgValue], ctx: &CommandContext| {
            if crate::runtime::task::background_started() {
                ctx.eprintln(CALLBACK_ALREADY_INIT);
                return Ok(CommandOutcome::Failed);
            }
            let prio = match args.get(1) {
                Some(ArgValue::String(s)) => Some(s.as_str()),
                _ => None,
            };
            let band = match priority_from_name(prio) {
                Ok(band) => band,
                Err(()) => {
                    ctx.eprintln(&format!(
                        "callbackParallelThreads: Unknown priority \"{}\"",
                        prio.unwrap_or("")
                    ));
                    return Ok(CommandOutcome::Failed);
                }
            };
            // Two different C globals, and only their startup values
            // coincide: `epicsThreadGetCPUs()` for the negative arm,
            // `callbackParallelThreadsDefault` for the zero arm.
            use crate::runtime::background::callback_executor as cb;
            let count = resolve_thread_count(
                ival(args, 0),
                cb::cpu_count() as i64,
                cb::parallel_threads_default() as i64,
            );
            crate::runtime::background::callback_executor::set_parallel_threads(count, band);
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

    /// Both streams, because which one a line lands on is the assertion:
    /// C prints these tables with `printf` and every refusal with
    /// `fprintf(stderr, ...)`, so a helper that captured stdout alone let a
    /// refusal on the wrong stream pass its test.
    fn run(ctx: &CommandContext, name: &str, tokens: &[&str]) -> (String, String, bool) {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg.get(name).unwrap();
        let tokens: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
        let args = parse_args(&tokens, &cmd.args).unwrap();
        let out_tmp = tempfile::NamedTempFile::new().unwrap();
        let err_tmp = tempfile::NamedTempFile::new().unwrap();
        let out_path = out_tmp.path().to_path_buf();
        let err_path = err_tmp.path().to_path_buf();
        let outcome = ctx.with_output(std::fs::File::create(&out_path).unwrap(), || {
            ctx.with_error(std::fs::File::create(&err_path).unwrap(), || {
                cmd.handler.call(&args, ctx)
            })
        });
        let failed = matches!(outcome, Ok(CommandOutcome::Failed));
        (
            std::fs::read_to_string(&out_path).unwrap(),
            std::fs::read_to_string(&err_path).unwrap(),
            failed,
        )
    }

    /// C declares the arity and types in `dbIocRegister.c:426-441`,
    /// `:482-510`; a wrong arity silently drops the argument the
    /// handler reads.
    #[test]
    fn arity_and_types_match_c() {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        for (name, arity) in [
            ("scanOnceSetQueueSize", 1),
            ("scanOnceQueueShow", 1),
            ("callbackSetQueueSize", 1),
            ("callbackQueueShow", 1),
            ("callbackParallelThreads", 2),
        ] {
            assert_eq!(reg.get(name).unwrap().args.len(), arity, "{name}");
        }
        let cpt = reg.get("callbackParallelThreads").unwrap();
        assert!(matches!(cpt.args[0].arg_type, ArgType::Int));
        assert!(matches!(cpt.args[1].arg_type, ArgType::String));
    }

    /// C's `%8s  %15d  %10d  %6d  %6.1f  %11d` with a percentage the
    /// test can compute: 3 of 8 used is 37.5%.
    #[test]
    fn the_table_reproduces_c_s_column_layout() {
        let st = CallbackQueueStats {
            size: 8,
            num_used: 3,
            max_used: 5,
            num_overflow: 12,
        };
        let out = queue_stats_table(&[("cbLow", st)]);
        assert_eq!(
            out[0],
            "PRIORITY  HIGH-WATER MARK  ITEMS IN Q  Q SIZE  % USED  Q OVERFLOWS"
        );
        assert_eq!(
            out[1],
            "   cbLow                5           3       8    37.5           12"
        );
    }

    /// `callbackQueueShow` bands the three queues in `threadNamePrefix`
    /// order and `scanOnceQueueShow` prints one row called `scanOnce`
    /// (`callback.c:151`, `dbScan.c:768`).
    #[test]
    fn both_show_commands_print_c_s_row_names_once_the_pool_is_up() {
        crate::runtime::task::background_init();
        let ctx = make_ctx();

        let (out, err, failed) = run(&ctx, "callbackQueueShow", &["0"]);
        assert!(!failed, "C never sets an error status for this command");
        assert!(err.is_empty(), "C prints the table with printf: {err:?}");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "header plus one row per band: {out:?}");
        for (line, name) in lines[1..].iter().zip(BAND_NAMES) {
            assert_eq!(line.split_whitespace().next(), Some(name));
        }

        let (out, err, failed) = run(&ctx, "scanOnceQueueShow", &["0"]);
        assert!(!failed, "C never sets an error status for this command");
        assert!(err.is_empty(), "C prints the table with printf: {err:?}");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "header plus one row: {out:?}");
        assert_eq!(lines[1].split_whitespace().next(), Some("scanOnce"));
    }

    /// C checks the size before the initialised guard
    /// (`callback.c:102-109`), so this holds whether or not the pool is
    /// up — which is what makes it the one command test that needs no
    /// `background_init` of its own.
    #[test]
    fn callback_set_queue_size_rejects_a_non_positive_size_in_either_state() {
        let ctx = make_ctx();
        let (out, err, failed) = run(&ctx, "callbackSetQueueSize", &["0"]);
        assert!(failed, "C returns -1 through iocshSetError");
        assert_eq!(err.trim_end(), QUEUE_SIZE_MUST_BE_POSITIVE);
        assert!(
            out.is_empty(),
            "C writes this one with fprintf(stderr): {out:?}"
        );
        let (out, err, failed) = run(&ctx, "callbackSetQueueSize", &["-1"]);
        assert!(failed);
        assert_eq!(err.trim_end(), QUEUE_SIZE_MUST_BE_POSITIVE);
        assert!(
            out.is_empty(),
            "C writes this one with fprintf(stderr): {out:?}"
        );
    }

    /// Both sizing commands refuse once the pool exists, because the
    /// pool read the knob when it was built (`callback.c:106-109`,
    /// `:162-165`).
    #[test]
    fn the_sizing_commands_refuse_once_the_pool_is_up() {
        crate::runtime::task::background_init();
        let ctx = make_ctx();
        let (out, err, failed) = run(&ctx, "callbackSetQueueSize", &["4000"]);
        assert!(failed);
        assert_eq!(err.trim_end(), CALLBACK_ALREADY_INIT);
        assert!(
            out.is_empty(),
            "C writes this one with fprintf(stderr): {out:?}"
        );
        let (out, err, failed) = run(&ctx, "callbackParallelThreads", &["2", "LOW"]);
        assert!(failed);
        assert_eq!(err.trim_end(), CALLBACK_ALREADY_INIT);
        assert!(
            out.is_empty(),
            "C writes this one with fprintf(stderr): {out:?}"
        );
    }

    /// The refusals the two show commands print when the facility is
    /// absent (`callback.c:145-146`, `dbScan.c:762-763`). The branch is
    /// `background_*_stats(..) == None`; the process-global executor
    /// cannot be torn down, so the text is asserted here and the
    /// mapping is the one `match` arm above it.
    #[test]
    fn the_not_initialized_refusals_are_c_s_sentences() {
        assert_eq!(
            CALLBACK_NOT_INIT,
            "Callback system not initialized, yet. Please run iocInit before using this command."
        );
        assert_eq!(
            SCAN_ONCE_NOT_INIT,
            "scanOnce system not initialized, yet. Please run iocInit before using this command."
        );
    }

    /// C matches `menuPriority`'s choice values with `epicsStrCaseCmp`,
    /// and treats `NULL`, `""` and `"*"` alike (`callback.c:173`).
    #[test]
    fn priority_names_follow_menu_priority_case_insensitively() {
        assert_eq!(priority_from_name(None), Ok(None));
        assert_eq!(priority_from_name(Some("")), Ok(None));
        assert_eq!(priority_from_name(Some("*")), Ok(None));
        assert_eq!(
            priority_from_name(Some("low")),
            Ok(Some(CallbackPriority::Low))
        );
        assert_eq!(
            priority_from_name(Some("Medium")),
            Ok(Some(CallbackPriority::Medium))
        );
        assert_eq!(
            priority_from_name(Some("HIGH")),
            Ok(Some(CallbackPriority::High))
        );
        assert_eq!(priority_from_name(Some("URGENT")), Err(()));
    }

    /// `count < 0` is relative to the CPU count, `count == 0` takes the
    /// default, and the result floors at 1 (`callback.c:167-171`).
    #[test]
    fn thread_count_follows_c_s_arithmetic() {
        assert_eq!(resolve_thread_count(3, 8, 8), 3);
        assert_eq!(resolve_thread_count(0, 8, 8), 8);
        assert_eq!(resolve_thread_count(-2, 8, 8), 6);
        assert_eq!(resolve_thread_count(-8, 8, 8), 1, "floors at 1");
        assert_eq!(resolve_thread_count(-99, 8, 8), 1, "floors at 1");
    }
}
