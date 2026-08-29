//! The database-debugger iocsh commands from `dbIocRegister.c` (@`R7.0.10`):
//! `dbb`, `dbd`, `dbc`, `dbs`, `dbstat`, `dbp` and `dbap`.
//!
//! All seven are a thin shell over `dbBkpt.c`, and so are these: the machinery
//! lives in [`crate::server::database::breakpoint`], which decides and reports
//! as data, and this module does the printing and the two things only the
//! shell can do — spawn a lock set's continuation thread, and hand the
//! debugger a `dbpr` to auto-print with.
//!
//! **`dbprc` is deliberately absent.** `dbBkpt.c:942-965` defines it and
//! `dbIocRegister.c` never registers it, so it is not one of this file's
//! commands; the port has no reachable C caller for it either.
//!
//! Every one of the seven is registered through `iocshSetError` in C
//! (`dbIocRegister.c:78`, `:85`, `:91`, `:99`, `:105`, `:121-124`, `:131`), so
//! a refusal fails the shell line as well as printing. That is
//! [`CommandOutcome::Failed`] after the message, not a bare `Continue`.

use std::sync::Arc;

use super::registry::*;
use crate::runtime::task::{StackSizeClass, ThreadPriority};
use crate::server::database::breakpoint::{self, BkptError, BreakpointTable};

/// Register the breakpoint command set on `registry`.
pub(crate) fn register(registry: &mut CommandRegistry) {
    registry.register(cmd_dbb());
    registry.register(cmd_dbd());
    registry.register(cmd_dbc());
    registry.register(cmd_dbs());
    registry.register(cmd_dbstat());
    registry.register(cmd_dbp());
    registry.register(cmd_dbap());
}

/// C `iocshArgStringRecord` reaches the handler as `args[0].sval`, which is
/// `NULL` when the argument was omitted. Every command but `dbc`/`dbs` treats
/// that as its usage error; those two treat it as "the default lock set".
fn sval(args: &[ArgValue], index: usize) -> Option<&str> {
    match args.get(index) {
        Some(ArgValue::String(s)) if !s.is_empty() => Some(s.as_str()),
        _ => None,
    }
}

/// The first positional argument as C's `args[1].ival`: an omitted
/// `iocshArgInt` reaches the handler as 0.
fn ival(args: &[ArgValue], index: usize) -> i32 {
    match args.get(index) {
        Some(ArgValue::Int(n)) => *n as i32,
        _ => 0,
    }
}

/// C's `if (!record_name) { printf("Usage: ..."); return -1; }` preamble,
/// shared by the five commands that require a name (`dbBkpt.c:285-288`,
/// `:410-413`, `:858-861`).
fn usage(ctx: &CommandContext, command: &str) -> CommandResult {
    ctx.println(&format!("Usage: {command} \"record_name\""));
    Ok(CommandOutcome::Failed)
}

/// Print the message and fail the line — every `BkptError` path in C.
fn refuse(ctx: &CommandContext, err: &BkptError) -> CommandResult {
    ctx.println(&err.message());
    Ok(CommandOutcome::Failed)
}

/// The debugger, installing it on first use, with this shell's `dbpr` wired in
/// as what `dbPrint` prints with.
///
/// The printer is re-installed on every `dbb` because a `CommandContext` is
/// built per call and the bridge it captures belongs to a runtime that a later
/// shell may have replaced; the table keeps the newest.
fn table_with_printer(ctx: &CommandContext) -> Arc<BreakpointTable> {
    let table = ctx.db().breakpoints_or_install();
    let db = ctx.db().clone();
    let bridge = ctx.bridge().clone();
    table.set_printer(Arc::new(move |name: &str| {
        // C `dbPrint` (`dbBkpt.c:818-821`): a blank line, the level-2 dump,
        // then the hanging `-> ` prompt. Its own context, because the one the
        // `dbb` line ran on is long gone by the time a record processes — and
        // `CommandContext::new` writes to stdout, which is where C's `printf`
        // goes.
        let ctx = CommandContext::new(db.clone(), bridge.clone());
        ctx.println("");
        super::commands::dbpr_report(&ctx, name, 2);
        ctx.print_fmt(format_args!("-> "));
    }));
    table
}

/// `dbb <record name>` — C `dbb()` (`dbBkpt.c:274-386`).
///
/// The continuation thread is spawned here rather than in the mechanism
/// because it is the shell that owns a handle to the runtime and to stdout.
/// It is a dedicated OS thread, as C's `epicsThreadCreate("bkptCont", ...)` is:
/// it exists to be parked at a breakpoint, and parking a runtime worker would
/// stall every other record in the IOC.
fn cmd_dbb() -> CommandDef {
    CommandDef::new(
        "dbb",
        vec![ArgDesc {
            name: "record name",
            arg_type: ArgType::Record,
        }],
        "dbb <record name> — Set Breakpoint on a record\n\
         This command spawns one breakpoint continuation task per lockset, \
         in which further record execution is run",
        |args: &[ArgValue], ctx: &CommandContext| {
            let Some(name) = sval(args, 0) else {
                return usage(ctx, "dbb");
            };
            let table = table_with_printer(ctx);
            let db = ctx.db().clone();
            let spawn = |id: u64, ex: Arc<breakpoint::BinarySemaphoreHandle>| {
                let db = (*db).clone();
                // C: `epicsThreadCreate("bkptCont", epicsThreadPriorityScanLow-1,
                // epicsThreadGetStackSize(epicsThreadStackBig), dbBkptCont,
                // precord)` (`dbBkpt.c:373-376`) — one band below the scan
                // threads, so a stopped lock set never outranks the ones still
                // driving. `epicsThreadCreate` also puts the thread on
                // `pthreadList`, which is what lets `epicsThreadShowAll` and
                // `epicsThreadResume` name the one thread a C IOC can actually
                // have suspended. A bare `thread::Builder` reached neither, so
                // the port's only suspendable thread was the only one invisible
                // to the commands that exist to find it; the registry-entering
                // spawn every other IOC thread already uses is what carries the
                // band, the stack class and the registration together.
                let spawned = crate::runtime::task::spawn_dedicated_thread(
                    "bkptCont".to_string(),
                    // C's own derivation, not the number it evaluates to.
                    ThreadPriority::Custom(ThreadPriority::ScanLow.value() - 1),
                    StackSizeClass::Big,
                    move || {
                        let closing = breakpoint::continuation_loop(db, id, ex);
                        // C `:628` prints this and leaves the prompt hanging.
                        print!("{closing}");
                        use std::io::Write;
                        let _ = std::io::stdout().flush();
                    },
                );
                if spawned.is_err() {
                    // C `:379-386`.
                    println!("   BKPT> Cannot spawn task to process record");
                }
            };
            match table.set(&db, name, spawn) {
                Ok(()) => Ok(CommandOutcome::Continue),
                Err(e) => refuse(ctx, &e),
            }
        },
    )
}

/// `dbd <record name>` — C `dbd()` (`dbBkpt.c:399-479`).
fn cmd_dbd() -> CommandDef {
    CommandDef::new(
        "dbd",
        vec![ArgDesc {
            name: "record name",
            arg_type: ArgType::Record,
        }],
        "dbd <record name> — Remove breakpoint from a record.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let Some(name) = sval(args, 0) else {
                return usage(ctx, "dbd");
            };
            // No `breakpoints_or_install` here: with nothing being debugged
            // there is no breakpoint to remove, and installing a table to say
            // so would put the hooks on the hot path for a failed command.
            let Some(table) = ctx.db().breakpoints() else {
                return refuse(ctx, &BkptError::NotSet);
            };
            match table.clear(ctx.db(), name) {
                Ok(()) => {
                    ctx.db().retire_breakpoints_if_idle();
                    Ok(CommandOutcome::Continue)
                }
                Err(e) => refuse(ctx, &e),
            }
        },
    )
}

/// `dbc <record name>` — C `dbc()` (`dbBkpt.c:489-518`). With no argument the
/// lock set on top of the stack, which is the one that stopped last.
fn cmd_dbc() -> CommandDef {
    CommandDef::new(
        "dbc",
        vec![ArgDesc {
            name: "record name",
            arg_type: ArgType::Record,
        }],
        "dbc <record name> — Continue processing in a lockset until next breakpoint is found.",
        |args: &[ArgValue], ctx: &CommandContext| resume(ctx, sval(args, 0), false),
    )
}

/// `dbs <record name>` — C `dbs()` (`dbBkpt.c:528-556`).
fn cmd_dbs() -> CommandDef {
    CommandDef::new(
        "dbs",
        vec![ArgDesc {
            name: "record name",
            arg_type: ArgType::Record,
        }],
        "dbs <record name> — Step through record processing within a lockset.\n\
         If called without an argument, automatically steps with the last breakpoint.",
        |args: &[ArgValue], ctx: &CommandContext| resume(ctx, sval(args, 0), true),
    )
}

fn resume(ctx: &CommandContext, name: Option<&str>, stepping: bool) -> CommandResult {
    let Some(table) = ctx.db().breakpoints() else {
        return refuse(ctx, &BkptError::NoneStopped);
    };
    let outcome = if stepping {
        table.step(ctx.db(), name)
    } else {
        table.cont(ctx.db(), name)
    };
    match outcome {
        Ok(announcement) => {
            if let Some(line) = announcement {
                ctx.println(&line);
            }
            Ok(CommandOutcome::Continue)
        }
        Err(e) => refuse(ctx, &e),
    }
}

/// `dbstat` — C `dbstat()` (`dbBkpt.c:884-940`). Takes no argument and cannot
/// fail: with nothing being debugged it prints nothing, as C's empty stack
/// does.
fn cmd_dbstat() -> CommandDef {
    CommandDef::new(
        "dbstat",
        vec![],
        "dbstat — Print list of suspended records, and breakpoints set in locksets.",
        |_args: &[ArgValue], ctx: &CommandContext| {
            if let Some(table) = ctx.db().breakpoints() {
                for line in table.status(ctx.db()) {
                    ctx.println(&line.render());
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbp <record name> <interest level>` — C `dbp()` (`dbBkpt.c:825-845`).
///
/// C's `(interest_level == 0) ? 2 : interest_level` (`:841`) is why an omitted
/// level prints at 2 and not at 0: `dbp` exists to dump a stopped record, and
/// level 0 would show less than the auto-print does.
fn cmd_dbp() -> CommandDef {
    CommandDef::new(
        "dbp",
        vec![
            ArgDesc {
                name: "record name",
                arg_type: ArgType::Record,
            },
            ArgDesc {
                name: "interest level",
                arg_type: ArgType::Int,
            },
        ],
        "dbp <record name> <interest level> — Print Fields of a currently suspended \
         record by a breakpoint.\n\
         interest level 0 - Fields of interest to an Application developer and\n\
         \x20                    that can be changed as a result of record processing.\n\
         \x20              1 - Fields of interest to an Application developer and\n\
         \x20                    that do not change during record processing.\n\
         \x20              2 - Fields of major interest to a System developer.\n\
         \x20              3 - Fields of minor interest to a System developer.\n\
         \x20              4 - Internal record fields.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let Some(table) = ctx.db().breakpoints() else {
                return refuse(ctx, &BkptError::NoneStopped);
            };
            let target = match table.print_target(ctx.db(), sval(args, 0)) {
                Ok(name) => name,
                Err(e) => return refuse(ctx, &e),
            };
            let level = ival(args, 1);
            let level = if level == 0 { 2 } else { level };
            super::commands::dbpr_report(ctx, &target, level);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbap <record name>` — C `dbap()` (`dbBkpt.c:848-881`). Toggles the BKPT
/// print bit whether or not the record has a breakpoint; `dbPrint` is what
/// makes the bit conditional on the lock set holding one.
fn cmd_dbap() -> CommandDef {
    CommandDef::new(
        "dbap",
        vec![ArgDesc {
            name: "record name",
            arg_type: ArgType::Record,
        }],
        "dbap <record name> — Auto Print.\n\
         Toggle automatic printing after processing a record that has a breakpoint.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let Some(name) = sval(args, 0) else {
                return usage(ctx, "dbap");
            };
            match breakpoint::toggle_autoprint(ctx.db(), name) {
                Ok(line) => {
                    ctx.println(&line);
                    Ok(CommandOutcome::Continue)
                }
                Err(e) => refuse(ctx, &e),
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::database::PvDatabase;
    use crate::server::records::ai::AiRecord;

    /// `dbb` puts its continuation thread on the thread list, as C's
    /// `epicsThreadCreate` does, with C's name and C's band.
    ///
    /// The defect this pins: the thread was started with a bare
    /// `std::thread::Builder`, so the one thread a C IOC can actually have
    /// suspended was the one thread `epicsThreadShowAll` and
    /// `epicsThreadResume` could not name. Measured on softIoc @`R7.0.10`
    /// with `A:ONE` stopped at a breakpoint: `epicsThreadShowAll` lists
    /// `bkptCont` at OSIPRI 59.
    ///
    /// The thread stays parked on its execution semaphore when the test ends,
    /// which is what it does in a running IOC between stops; nothing joins it,
    /// exactly as C never joins `bkptCont`.
    #[test]
    fn dbb_puts_its_continuation_thread_on_the_thread_list() {
        // RTEMS-EXEC-MODEL-ALLOW(1): a sync test that hand-builds its own tokio runtime; runs and passes in the exec-backend suite.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(PvDatabase::new());
        rt.block_on(async {
            db.add_record("BKPT:ONE", Box::new(AiRecord::new(0.0)))
                .await
                .expect("add_record");
        });
        db.build_lock_sets();
        let bridge = {
            let _guard = rt.enter();
            crate::runtime::task::BlockingBridge::capture()
        };
        let ctx = CommandContext::new(db.clone(), bridge);
        let mut reg = CommandRegistry::new();
        register(&mut reg);

        assert!(
            crate::runtime::task::thread_by_name("bkptCont").is_none(),
            "no continuation thread before dbb"
        );

        let cmd = reg.get("dbb").unwrap();
        let args = parse_args(&["BKPT:ONE".to_string()], &cmd.args).unwrap();
        assert!(
            matches!(cmd.handler.call(&args, &ctx), Ok(CommandOutcome::Continue)),
            "dbb on a record with a lock set must succeed"
        );

        // The thread registers itself in its own prologue, so the row appears
        // once it is scheduled rather than at `spawn` return.
        let listed = (0..200)
            .find_map(|_| {
                crate::runtime::task::thread_by_name("bkptCont").or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    None
                })
            })
            .expect("bkptCont must reach the thread list epicsThreadShowAll reads");
        assert_eq!(
            listed.epics_priority(),
            ThreadPriority::ScanLow.value() - 1,
            "C spawns bkptCont at epicsThreadPriorityScanLow-1 (dbBkpt.c:373)"
        );
        assert!(
            crate::runtime::task::thread_by_id(listed.id()).is_some(),
            "the row must also resolve by the id epicsThreadResume parses"
        );

        // C's `dbstat` prints `pnode->taskid` with `%p` and that is the same
        // pointer `epicsThreadShowAll` shows in its EPICS ID column — measured
        // on softIoc @`R7.0.10`, `T: 0x6311a8111d00` against the `bkptCont`
        // row's `0x6311a8111d00`. One thread, one handle.
        let stat = breakpoint::BreakpointTable::status(
            &db.breakpoints().expect("dbb installs the table"),
            &db,
        );
        assert!(
            stat[0]
                .render()
                .ends_with(&format!("T: {:#x}", listed.id())),
            "dbstat's T: column must be the thread's EPICS ID, got {:?}",
            stat[0].render()
        );
        std::mem::forget(rt);
    }
}
