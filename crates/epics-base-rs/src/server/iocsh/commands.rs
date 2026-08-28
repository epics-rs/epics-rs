use std::collections::HashMap;

use super::registry::*;
use crate::error::{CaError, CaResult};
use crate::runtime::log::{ERL_ERROR, ERL_WARNING};
use crate::server::database::filters::sync::DbState;
use crate::server::database::{DbNode, RecordLoad, parse_pv_name};
use crate::server::db_loader;
use crate::server::record::{Base, FieldDeclaration, FieldDesc, Special};
use crate::types::{DbFieldType, DbfCode, EpicsValue};
use std::sync::Arc;

/// C `dbRecordsOnceOnly` (`dbLexRoutines.c:52-53`), the exported `int` a
/// startup script sets with `var dbRecordsOnceOnly 1`.
///
/// Clear (C's default) a second `record(type, "name")` for a name already
/// loaded MERGES its fields into the existing instance, which is how a
/// template overrides a value its own include declared. Set, that second
/// block is refused and its whole body dropped, so a `.db` set that
/// accidentally declares one record twice is caught instead of silently
/// taking the last writer.
///
/// C tests the flag with a bare `if`, so any non-zero value sets it.
static DB_RECORDS_ONCE_ONLY: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

fn db_records_once_only() -> bool {
    DB_RECORDS_ONCE_ONLY.load(std::sync::atomic::Ordering::Relaxed) != 0
}

fn db_records_once_only_var() -> super::vars::VarDef {
    super::vars::VarDef {
        name: "dbRecordsOnceOnly",
        access: super::vars::VarAccess::Int {
            get: || DB_RECORDS_ONCE_ONLY.load(std::sync::atomic::Ordering::Relaxed),
            set: |v| DB_RECORDS_ONCE_ONLY.store(v, std::sync::atomic::Ordering::Relaxed),
        },
    }
}

/// C `dbQuietMacroWarnings` (`dbLexRoutines.c:58`), the exported `int` a
/// startup script sets with `var dbQuietMacroWarnings 1`.
///
/// `dbReadCOM` hands it straight to `macSuppressWarning` (`:273`), which
/// silences macLib's per-reference notice AND changes the placeholder an
/// unresolved reference leaves behind from `$(name,undefined)` to
/// `$(name)` (`macCore.c:911-928`) — so it is not a pure logging switch
/// and the loader reads it at both places.
///
/// C tests the flag with a bare `if`, so any non-zero value sets it.
fn db_quiet_macro_warnings_var() -> super::vars::VarDef {
    super::vars::VarDef {
        name: "dbQuietMacroWarnings",
        access: super::vars::VarAccess::Int {
            get: || i64::from(db_loader::db_quiet_macro_warnings()),
            set: |v| db_loader::set_db_quiet_macro_warnings(v != 0),
        },
    }
}

/// C `createAlias` (`dbLexRoutines.c:1450-1481`) — the one place an
/// `alias(...)` reaches the database, from a record body or from the file
/// scope.
///
/// An alias name that ALREADY names this same record is not an error and
/// not a re-creation: C compares `precnode->aliasedRecnode` after
/// de-aliasing the target and, when they match, leaves `status` at 0 and
/// creates nothing. Only `dbRecordsOnceOnly` turns that into an error. The
/// port used to hand every repeat to `add_alias`, whose name-free check
/// rejected it — a diagnostic C does not print at its own default.
async fn install_alias(
    ctx: &CommandContext,
    alias: &str,
    target: &str,
    faults: &mut db_loader::DbFaults,
) {
    let canonical = ctx
        .db()
        .resolve_alias(target)
        .unwrap_or_else(|| target.to_string());
    if ctx
        .db()
        .resolve_alias(alias)
        .is_some_and(|already| already == canonical)
    {
        if db_records_once_only() {
            faults.recoverable(format!(
                "{ERL_ERROR}: Alias '{alias}' already defined; dbRecordsOnceOnly is set."
            ));
        }
        return;
    }
    if let Err(e) = ctx.db().add_alias(alias, target).await {
        faults.recoverable(format!(
            "dbLoadRecords: alias '{alias}' for '{target}' rejected: {e}"
        ));
    }
}

/// Register all built-in iocsh commands.
pub(crate) fn register_builtins(registry: &mut CommandRegistry) {
    registry.register(cmd_help());
    registry.register(cmd_dbl());
    registry.register(cmd_dba());
    registry.register(cmd_dbgf());
    registry.register(cmd_dbpf());
    registry.register(cmd_dbpr());
    registry.register(cmd_dbsr());
    registry.register(cmd_dbglob());
    registry.register(cmd_dbgrep());
    registry.register(cmd_scanppl());
    registry.register(cmd_scanpel());
    registry.register(cmd_scanpiol());
    registry.register(cmd_post_event());
    registry.register(cmd_ioc_stats());
    registry.register(cmd_db_load_database());
    registry.register(cmd_db_load_records());
    registry.register(cmd_db_load_template());
    registry.register(cmd_db_create_record());
    registry.register(cmd_db_delete_record());
    // The `db*` report and state commands C registers from
    // `dbIocRegister.c:587-645` and `dbStaticIocRegister.c:265-280` — the
    // spans at `R7.0.10`, where the latter file is 281 lines. What this IOC
    // still does not reach is `ABSENT_DATABASE_COMMANDS`, measured against
    // this function rather than against any one module.
    registry.register(cmd_dbtgf());
    registry.register(cmd_dbtpf());
    registry.register(cmd_gft());
    registry.register(cmd_pft());
    registry.register(cmd_dbtr());
    registry.register(cmd_dbjlr());
    registry.register(cmd_dbior());
    registry.register(cmd_dbel());
    registry.register(cmd_dbtpn());
    registry.register(cmd_tpn());
    registry.register(cmd_dbnr());
    registry.register(cmd_dblsr());
    registry.register(cmd_db_lock_show_locked());
    registry.register(cmd_db_put_attribute());
    registry.register(cmd_db_notify_dump());
    registry.register(cmd_dbcar());
    registry.register(cmd_dbla());
    registry.register(cmd_dbli());
    registry.register(cmd_db_create_alias());
    registry.register(cmd_db_state_create());
    registry.register(cmd_db_state_set());
    registry.register(cmd_db_state_clear());
    registry.register(cmd_db_state_show());
    registry.register(cmd_db_state_show_all());
    registry.register(cmd_db_dump_breaktable());
    registry.register(cmd_db_dump_path());
    registry.register(cmd_epics_env_set());
    registry.register(cmd_pushd());
    registry.register(cmd_popd());
    registry.register(cmd_dirs());
    registry.register(cmd_ioc_init());
    registry.register(cmd_after_ioc_running());
    registry.register(cmd_exit());

    // core iocsh commands (echo, date, cd/pwd, epicsEnv*, ...)
    // and the access-security `as*` family. Without these a stock
    // `st.cmd` errors on the first unknown command and access
    // security cannot be loaded from the shell.
    super::core_commands::register(registry);
    super::dbstatic_commands::register(registry);
    super::misc_commands::register(registry);
    super::queue_commands::register(registry);
    super::registry_commands::register(registry);
    super::time_commands::register(registry);
    super::access_commands::register(registry);
    super::breakpoint_commands::register(registry);
    // Last: C registers `var` from `iocshRegisterVariable`, so it must
    // come after everything that contributes to the variable table.
    super::vars::register_variable(db_records_once_only_var());
    super::vars::register_variable(db_quiet_macro_warnings_var());
    super::vars::register(registry);

    // The built-in table is assembled from one file per C registrar and
    // no two of them may claim a name: `register` replaces silently, as
    // C's `iocshRegister` does, so a collision would otherwise leave one
    // family's command simply absent with nothing to say so. Asserted at
    // the owner rather than only in a test, because the table grows by a
    // one-line append per family and two such appends merge cleanly.
    debug_assert!(
        registry.displaced().is_empty(),
        "two built-in families claim the same iocsh name(s): {:?}",
        registry.displaced()
    );
}

/// Every iocsh command name the three database `*Register.c` files register at
/// `R7.0.10`, read from the `iocshFuncDef` NAME STRINGS and not from the
/// `iocshRegister` symbols, because the two disagree: the FuncDef variable is
/// `dbPutAttrFuncDef` where the registered name is `dbPutAttribute`
/// (`dbIocRegister.c:384`). Fifty come from `dbIocRegister.c:587-645`,
/// including `gft` and `pft`, which belong to that file and to the same
/// `dbTest.c` as the `db*` set; sixteen from `dbStaticIocRegister.c:265-280`;
/// and `dbLoadTemplate` from `dbtemplate/dbtoolsIocRegister.c:41`.
///
/// C also has `dbCreateRecord` and `dbDeleteRecord`, which are `dbStaticLib`
/// API and not iocsh commands there, so they are not on this list even though
/// [`register_builtins`] registers both.
#[cfg(test)]
const C_DATABASE_COMMANDS: &[&str] = &[
    "callbackParallelThreads",
    "callbackQueueShow",
    "callbackSetQueueSize",
    "dbCreateAlias",
    "dbDumpBreaktable",
    "dbDumpDevice",
    "dbDumpDriver",
    "dbDumpField",
    "dbDumpFunction",
    "dbDumpLink",
    "dbDumpMenu",
    "dbDumpPath",
    "dbDumpRecord",
    "dbDumpRecordType",
    "dbDumpRegistrar",
    "dbDumpVariable",
    "dbLoadDatabase",
    "dbLoadRecords",
    "dbLoadTemplate",
    "dbLockShowLocked",
    "dbNotifyDump",
    "dbPutAttribute",
    "dbPvdDump",
    "dbPvdTableSize",
    "dbReportDeviceConfig",
    "dbStateClear",
    "dbStateCreate",
    "dbStateSet",
    "dbStateShow",
    "dbStateShowAll",
    "dba",
    "dbap",
    "dbb",
    "dbc",
    "dbcar",
    "dbd",
    "dbel",
    "dbgf",
    "dbglob",
    "dbgrep",
    "dbhcr",
    "dbior",
    "dbjlr",
    "dbl",
    "dbla",
    "dbli",
    "dblsr",
    "dbnr",
    "dbp",
    "dbpf",
    "dbpr",
    "dbs",
    "dbsr",
    "dbstat",
    "dbtgf",
    "dbtpf",
    "dbtpn",
    "dbtr",
    "gft",
    "pft",
    "postEvent",
    "scanOnceQueueShow",
    "scanOnceSetQueueSize",
    "scanpel",
    "scanpiol",
    "scanppl",
    "tpn",
];

/// The subset of [`C_DATABASE_COMMANDS`] this IOC cannot reach, and what each
/// one would need.
///
/// RESOLUTION RULE — the reason the census is a constant and a test rather than
/// prose: a name is PRESENT when [`register_builtins`] puts it in the
/// `CommandRegistry` the shell serves, whichever module registered it.
/// Reachability is a property of the IOC, not of this file, so `dbDumpMenu` in
/// `dbstatic_commands.rs` and `dbstat` in `breakpoint_commands.rs` are present.
/// Counting `CommandDef::new` in this file alone — the measurement this
/// constant replaces — reported nine registered commands as absent.
///
/// `the_database_command_census_matches_the_registry` builds the real registry
/// and asserts this list is exactly what is missing from it, in both
/// directions, so the census cannot go stale: porting one of these fails the
/// test until the name is deleted from here, and losing a registration fails it
/// until the name is added.
///
/// Currently empty: every name `dbIocRegister.c` registers is reachable.
#[cfg(test)]
const ABSENT_DATABASE_COMMANDS: &[&str] = &[];

/// Every command this shell registers that no `*IocRegister.c` in EPICS base
/// R7.0.10 registers under any name, paired with the reason it exists here.
///
/// A port-only command is allowed; an UNDOCUMENTED one is not. `help` shows a
/// name and a usage line and nothing else, so a reader cannot tell "we added
/// this deliberately" from "C has this and we got the spelling wrong" — which
/// is exactly what `post_event` was until it was removed.
///
/// The population is MEASURED, not grepped: `help` on C `softIoc` R7.0.10-146
/// and on `softioc-rs`, same `.db`, differ by these five names in this
/// direction and nothing else. The C side of that comparison is every name
/// reachable in a `softIoc`, not just `dbIocRegister.c`'s, which is why the
/// test below can only assert the weaker half — that none of these is a
/// database-command name — against the census this file carries.
#[cfg(test)]
const PORT_ONLY_COMMANDS: &[(&str, &str)] = &[
    (
        "dbDeleteRecord",
        "C has the call — `dbDeleteRecord` (`dbStaticLib.h:139`) — but reaches \
         it only from the `.db` grammar `record(\"#\",\"name\")` \
         (`dbLexRoutines.c:1146-1159`), which this port also accepts. The \
         command is that same deletion with an iocsh surface.",
    ),
    (
        "dirs",
        "C registers `cd` and `pwd` (`libComRegister.c`) and keeps no \
         directory stack. See `pushd`.",
    ),
    (
        "iocStats",
        "Base has no runtime-statistics verb at any spelling; the `devIocStats` \
         module publishes the same numbers as records, not as a command.",
    ),
    (
        "popd",
        "The pop half of the stack `cd` alone cannot express. See `pushd`.",
    ),
    (
        "pushd",
        "An `st.cmd` that `iocshLoad`s a script which `cd`s has no way back in \
         C except to know its own absolute path; the stack makes the return \
         local to the caller.",
    ),
];

/// `afterIocRunning <command>` — queue an iocsh command line to run
/// after iocInit completes. Mirrors epics-base PR #558.
fn cmd_after_ioc_running() -> CommandDef {
    CommandDef::new(
        "afterIocRunning",
        vec![ArgDesc {
            name: "command",
            arg_type: ArgType::String,
        }],
        "afterIocRunning <command> — schedule a command for post-iocInit execution",
        |args: &[ArgValue], ctx: &CommandContext| {
            let line = match &args[0] {
                ArgValue::String(s) => s.clone(),
                _ => {
                    ctx.println("afterIocRunning: missing command");
                    return Ok(CommandOutcome::Continue);
                }
            };
            ctx.db().queue_after_ioc_running(line);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbDeleteRecord <name>` — remove a record from the live database.
///
/// Port-only, and listed as such in `PORT_ONLY_COMMANDS`: no
/// `*IocRegister.c` in R7.0.10 registers this name or any synonym. C does have
/// the operation — `dbDeleteRecord` (`dbStaticLib.h:139`) — but exposes it
/// only through the `.db` grammar `record("#","name")`
/// (`dbLexRoutines.c:1146-1159`), the form epics-base PR #505 added and this
/// port also accepts. This command is that deletion with an iocsh surface.
fn cmd_db_delete_record() -> CommandDef {
    CommandDef::new(
        "dbDeleteRecord",
        vec![ArgDesc {
            // Port-only command; C hints the record argument of its
            // nearest equivalent, `dbCreateAliasArg0`
            // (`dbStaticIocRegister.c:230`).
            name: "recordName",
            arg_type: ArgType::Record,
        }],
        "dbDeleteRecord <name> — remove a record from the live database",
        |args: &[ArgValue], ctx: &CommandContext| {
            let name = match &args[0] {
                ArgValue::String(s) => s.clone(),
                _ => {
                    ctx.println("dbDeleteRecord: missing recordName");
                    return Ok(CommandOutcome::Continue);
                }
            };
            if ctx.block_on(ctx.db().remove_record(&name)) {
                ctx.println(&format!("dbDeleteRecord: removed '{name}'"));
            } else {
                return Err(format!("dbDeleteRecord: no record named '{name}'"));
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Process-global directory stack for `pushd`/`popd`/`dirs`.
/// Mirrors epics-base PR #497 — bash-style directory navigation in
/// iocsh. Stack is shared across IocShell instances within one
/// process; iocsh is by convention single-instance.
fn dir_stack() -> &'static std::sync::Mutex<Vec<std::path::PathBuf>> {
    static STACK: std::sync::OnceLock<std::sync::Mutex<Vec<std::path::PathBuf>>> =
        std::sync::OnceLock::new();
    STACK.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn print_stack(ctx: &CommandContext) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let stack = dir_stack().lock().unwrap();
    // Bash convention: top-of-stack is leftmost, cwd is shown first.
    let parts: Vec<String> = std::iter::once(cwd.display().to_string())
        .chain(stack.iter().rev().map(|p| p.display().to_string()))
        .collect();
    ctx.println(&parts.join(" "));
}

fn cmd_help() -> CommandDef {
    CommandDef::new(
        "help",
        vec![ArgDesc {
            name: "command",
            arg_type: ArgType::String,
        }],
        "help [command] - List commands or show usage for a specific command",
        |args: &[ArgValue], _ctx: &CommandContext| {
            // help needs access to the registry, which we handle specially in execute_line
            // This handler is a placeholder; the real logic is in IocShell::execute_line
            match &args[0] {
                ArgValue::String(name) => {
                    _ctx.println("Use 'help' without arguments to list all commands, or 'help <command>' for details.");
                    _ctx.println(&format!("(Looking for help on '{name}')"));
                }
                ArgValue::Missing => {
                    _ctx.println("Use 'help' to list all commands.");
                }
                _ => {}
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// C `splitFieldsList` (`dbTest.c:108-125`): the field list is split on
/// SPACES, not commas — `epicsStrtok_r(fieldnames, " ", ...)` — so
/// `dbl("ai","VAL DESC")` asks for two fields and `dbl("ai","VAL,DESC")`
/// asks for one field literally named `VAL,DESC`.
fn split_fields_list(fields: &str) -> Vec<String> {
    fields
        .split(' ')
        .filter(|f| !f.is_empty())
        .map(str::to_string)
        .collect()
}

/// C `printFieldsList` (`dbTest.c:127-148`) — the per-record field dump
/// `dbl`, `dbgrep` and `dbglob` share. The caller has already written
/// the record name with no newline; this appends `, "value"` for each
/// requested field and terminates the line, so one record is one line.
/// A field the record does not have contributes a bare `, ` (C prints
/// the separator and `continue`s), except the pseudo-field
/// `recordType`, which C answers from `dbGetRecordTypeName` when
/// `dbFindField` fails.
fn print_fields_list(ctx: &CommandContext, name: &str, fields: &[String]) {
    let mut line = String::from(name);
    let record = ctx.db().get_record(name);
    for field in fields {
        let value = record.as_ref().and_then(|rec| {
            let inst = rec.read();
            inst.record
                .get_field(field)
                .map(|v| v.to_string())
                .or_else(|| (field == "recordType").then(|| inst.record.record_type().to_string()))
        });
        match value {
            Some(v) => line.push_str(&format!(", \"{v}\"")),
            None => line.push_str(", "),
        }
    }
    ctx.println(&line);
}

/// C `dbl` and `dbglob` walk the database record-type MAJOR
/// (`dbTest.c:174-193`, `:322-341`): `dbFirstRecordType` selects a
/// record type and `dbFirstRecord`/`dbNextRecord` then walk that
/// type's own list — the order its nodes were loaded — before the
/// next type is touched. Sorting every name once interleaves the
/// types, which C never does, and throws away the load order
/// [`PvDatabase::all_db_nodes`](crate::server::database::PvDatabase::all_db_nodes) preserves.
///
/// The list holds ALIAS NODES as well as records ([`DbNode`]), so this
/// is the walk and [`record_names_type_major`] is C's
/// `if (dbIsAlias(pdbentry)) continue`. Answering "records or both?"
/// here, once, is what keeps a command from answering it by accident:
/// `dbl` listed no alias at all for as long as the walk had no alias
/// in it to skip.
///
/// The type sequence is `dbd_generated::RECORD_TYPES`, which the
/// generator emits in name order; C's is `recordTypeList`, the order
/// the loaded `.dbd` declared them in. The port has no per-database
/// declaration order to read, so the grouping is C's and the sequence
/// of the groups is the table's. A type registered at runtime is not
/// in the table and follows those that are, by name. An alias ranks by
/// the type of the record it names, which is the type list C's
/// `dbCreateAlias` put its node in.
fn db_nodes_type_major(ctx: &CommandContext) -> Vec<DbNode> {
    use crate::server::record::dbd_generated::RECORD_TYPES;

    let mut nodes = ctx.block_on(ctx.db().all_db_nodes());
    let rank = |node: &DbNode| {
        let record_type = ctx
            .db()
            .get_record(&node.name)
            .map(|rec| rec.read().record.record_type().to_string())
            .unwrap_or_default();
        match RECORD_TYPES.iter().position(|t| *t == record_type) {
            Some(i) => (0usize, i, String::new()),
            None => (1usize, 0, record_type),
        }
    };
    // Stable: the nodes of one type keep the load order they arrived in.
    nodes.sort_by_cached_key(rank);
    nodes
}

/// The record half of [`db_nodes_type_major`] — C's
/// `if (dbIsAlias(pdbentry)) continue`, which `dbjlr` (`dbJLink.c:520`),
/// `dbcar` (`dbCaTest.c:85`) and `dbWriteRecordFP` (`dbStaticLib.c:826`)
/// each spell for themselves.
fn record_names_type_major(ctx: &CommandContext) -> Vec<String> {
    db_nodes_type_major(ctx)
        .into_iter()
        .filter(|node| node.alias_of.is_none())
        .map(|node| node.name)
        .collect()
}

fn cmd_dbl() -> CommandDef {
    CommandDef::new(
        "dbl",
        vec![
            ArgDesc {
                name: "record type",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "fields",
                arg_type: ArgType::String,
            },
        ],
        "dbl [record type] [fields] - List record names, optionally filtered by type",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `dbl` (`dbTest.c:164-166`): an empty type and the
            // literal `*` are both the all-types sentinel, collapsed to
            // `precordTypename = NULL`; an empty field list is likewise
            // no field list (`:167-168`).
            let type_filter = match &args[0] {
                ArgValue::String(s) if !s.is_empty() && s != "*" => Some(s.as_str()),
                _ => None,
            };
            let fields = match &args[1] {
                ArgValue::String(s) => split_fields_list(s),
                _ => Vec::new(),
            };

            // C's `dbl` names no `dbIsAlias` filter, so the alias nodes in
            // the type's list are listed with the records (`dbTest.c:180-185`)
            // — a site that builds its PV inventory with `dbl > pvlist` gets
            // the alias names its clients use.
            let nodes = db_nodes_type_major(ctx);

            // C walks the record types and reports an unknown one
            // before listing anything (`dbTest.c:174-180`).
            if let Some(filter) = type_filter {
                let known = nodes.iter().any(|node| {
                    ctx.db()
                        .get_record(&node.name)
                        .is_some_and(|rec| rec.read().record.record_type() == filter)
                });
                if !known {
                    ctx.println("No record type");
                    return Ok(CommandOutcome::Continue);
                }
            }

            for node in &nodes {
                if let Some(filter) = type_filter {
                    let rec = ctx.db().get_record(&node.name);
                    if let Some(rec) = rec {
                        let inst = rec.read();
                        if inst.record.record_type() != filter {
                            continue;
                        }
                    }
                }
                print_fields_list(ctx, &node.name, &fields);
            }

            Ok(CommandOutcome::Continue)
        },
    )
}

/// C `dbpr_msgOut`'s `TAB_BUFFER` (`dbTest.c:1279-1367`) — the layout
/// every `dbgf` and `dbpr` message goes through. Each inserted message
/// is padded out to the next `tab` stop, and the buffer is flushed as
/// one line when the next message would carry it past `MAXLINE`.
/// Reproducing the buffer rather than formatting a line directly is
/// what keeps a multi-element array wrapping where C wraps it.
///
/// `tab` is a parameter for the same reason it is one in C: `dbgf`
/// passes 10 (`dbTest.c:512`) and `dbpr` passes 20 (`dbTest.c:444`),
/// and those two are the only values C accepts (`dbTest.c:1287-1290`).
struct TabBuffer {
    out: String,
    tab: usize,
    next_tab: usize,
    lines: Vec<String>,
}

impl TabBuffer {
    const MAXLINE: usize = 80;

    fn new(tab: usize) -> Self {
        Self {
            out: String::new(),
            tab,
            next_tab: tab,
            lines: Vec::new(),
        }
    }

    /// C reads `out_buff` as a zero-filled fixed buffer, so a position
    /// past the written text is NUL.
    fn at(&self, i: usize) -> u8 {
        self.out.as_bytes().get(i).copied().unwrap_or(0)
    }

    fn insert(&mut self, msg: &str) {
        if self.out.len() + msg.len() > Self::MAXLINE {
            self.flush();
        }
        for b in msg.bytes() {
            self.out.push(b as char);
            if self.at(self.next_tab - 1) != 0 {
                self.next_tab += self.tab;
            }
        }
        while self.at(self.next_tab - 1) != b' ' && self.out.len() < Self::MAXLINE {
            self.out.push(' ');
        }
    }

    fn flush(&mut self) {
        if !self.out.is_empty() {
            self.lines.push(std::mem::take(&mut self.out));
        }
        self.next_tab = self.tab;
    }

    fn finish(mut self) -> Vec<String> {
        self.flush();
        self.lines
    }
}

/// C `dbEntryToAddr`'s two type words for one field (`dbAccess.c:627-649`):
/// `paddr->field_type`, the `DBF_*` token the `.dbd` declared except on a
/// `special(SPC_DBADDR)` row where the record's `cvt_dbaddr` overwrites it,
/// and `paddr->dbr_field_type`, that token through `mapDBFToDBR`.
///
/// **The single owner of "what type is this field read and labelled as".**
/// Every `dbTest.c` command that prints a `DBF_<T>:` header — `dbgf`, `dbpf`,
/// `dbtgf`, `dbtpf` — and `dba`, which prints both words, asks here, so the
/// answer cannot be read off the [`EpicsValue`] the record happens to hold.
/// It cannot be, in either direction: `DBF_ENUM`, `DBF_MENU` and `DBF_DEVICE`
/// all collapse onto one served variant and one `mapDBFToDBR` code, so the
/// variant cannot name the token, while a `DBF_MENU` index is stored in a
/// short and the variant names the wrong code entirely. Reading it off the
/// value is what made `dbgf REC.PRIO` answer `DBF_SHORT: 0 = 0x0` where C
/// answers `DBF_STRING: "LOW"`.
///
/// The one place the stored value IS the answer is a
/// [`runtime_typed`](FieldDesc::runtime_typed) row, where C's `cvt_dbaddr`
/// re-types the address from the record's own state — `waveform.VAL` to `FTVL`
/// (`waveformRecord.c::cvt_dbaddr`), `mbbo.VAL` down to `DBF_USHORT` when no
/// state string is defined (`mbboRecord.c::cvt_dbaddr`) — and the port's
/// answer to that same question is the variant the record holds. `value` is
/// therefore consulted for those rows and only those; `None` leaves the
/// declaration standing, which is what a caller with no value in hand wants.
fn field_addr_types(fd: &FieldDesc, value: Option<&EpicsValue>) -> (DbfCode, DbfCode) {
    let field_type = match value {
        Some(v) if fd.runtime_typed => v.db_field_type().dbf_code(),
        // C tests `paddr->special`, which `dbEntryToAddr` has just copied from
        // `pflddes->special` (`dbAccess.c:638-641`) — the DECLARED code, not
        // the one `cvt_dbaddr` then raises. `lsi.VAL` is the case in hand: it
        // declares `SPC_DBADDR` and resolves to `SPC_MOD`, so reading the
        // resolved code skipped the arm and labelled it `DBF_NOACCESS` where C
        // says `DBF_STRING` (`lsiRecord.c::cvt_dbaddr`).
        _ if fd.declared_special == Special::DbAddr => fd.dbf_type.dbf_code(),
        _ => fd.declared_dbf,
    };
    (field_type, field_type.dbr_code())
}

/// C's `printBuffer` label and the port's served type for one DBR code — the
/// inverse of [`DbfCode::dbr_code`]'s codomain, resolved through the one table
/// that already pairs the two ([`DBTGF_REQUEST_TYPES`], C's `dbr[]` at
/// `dbTest.c:82-85`).
///
/// `None` for `DBR_NOACCESS`, the one code `mapDBFToDBR` produces that no
/// request type answers. C has no defined answer here either: `DBR_NOACCESS`
/// is `DBF_NOACCESS`, i.e. 17 (`dbFldTypes.h:90`), while `dbr[]` is declared
/// `[DBR_ENUM+2]` — thirteen entries — so `printBuffer`'s own label lookup
/// reads out of bounds. A live `dbgf REC.TIME` at R7.0.10 prints
/// `DBF_CHAR: failed.`, the adjacent `dbf[1]`. The port answers with the code
/// number instead of reproducing one build's out-of-bounds read.
fn dbr_request(dbr: DbfCode) -> Option<(&'static str, crate::types::DbFieldType)> {
    DBTGF_REQUEST_TYPES
        .iter()
        .find(|(name, _)| *name == dbr.name())
        .copied()
}

/// C `printBuffer`'s element switch (`dbTest.c:999-1148`), keyed by the DBR
/// type the CALLER asked for.
///
/// The DBR code is an input and never a property of `val`: by the time C's
/// `printBuffer` sees the buffer it has already been through
/// `dbFastGetConvertRoutine[field_type][dbrType]`, so the switch is on
/// `dbr_type` alone. Callers convert first ([`native_readback_lines`]), which
/// is what makes the pairs below exhaustive over what can actually arrive —
/// the fallback is C's own `default:` arm (`dbTest.c:1144-1147`) and not a
/// guard against a shape this port can construct.
fn printbuffer_elements(dbr: DbfCode, val: &EpicsValue) -> Vec<String> {
    use crate::calc::engine::cvt::fmt_g;

    fn quoted(bytes: &[u8]) -> String {
        format!("\"{}\"", escape_char_array_for_dbgf(bytes))
    }
    fn i32_hex(v: i32) -> String {
        format!("{v} = 0x{:x}", v as u32)
    }
    fn i16_hex(v: i16) -> String {
        format!("{v} = 0x{:x}", v as u16)
    }
    /// `dbTest.c:1014-1021` — the byte is read through an `epicsInt8 *` and
    /// widened to `epicsInt32`, so 0xc8 prints as -56; the hex is masked back
    /// to a byte (`val & 0xff`), and the character itself is appended when
    /// `isprint`. `EpicsValue::Char` holds the same byte in a `u8`.
    fn char_scalar(byte: u8) -> String {
        let val = byte as i8 as i32;
        if byte.is_ascii_graphic() || byte == b' ' {
            format!("{val} = 0x{byte:x} = '{}'", byte as char)
        } else {
            format!("{val} = 0x{byte:x}")
        }
    }
    /// C `cvtInt64ToHexString` (`cvtFast.c:483-507`) — alone among the integer
    /// rows it prints a SIGNED hex, `-0x5` and not `0xfffffffffffffffb`,
    /// because `printBuffer`'s 64-bit arms call it instead of `sprintf("%x")`
    /// (`dbTest.c:1092-1104`). `i64::MIN` has no positive magnitude and C
    /// spells its digits out by hand.
    fn i64_hex(v: i64) -> String {
        match v {
            i64::MIN => "-0x8000000000000000".to_string(),
            v if v < 0 => format!("-0x{:x}", v.unsigned_abs()),
            v => format!("0x{v:x}"),
        }
    }

    match (dbr, val) {
        (DbfCode::String, EpicsValue::String(v)) => vec![quoted(v.as_bytes())],
        (DbfCode::String, EpicsValue::StringArray(v)) => {
            v.iter().map(|s| quoted(s.as_bytes())).collect()
        }
        // `%d = 0x%x`, plus the printable character (`dbTest.c:1013-1021`).
        (DbfCode::Char, EpicsValue::Char(v)) => vec![char_scalar(*v)],
        // C's discriminator here is `no_elements`, not how the value happens
        // to be stored: a ONE-byte buffer takes the numeric row above, which
        // is why `dbpf B:WC ""` — one NUL — reads back `DBF_CHAR: 0 = 0x0`
        // and not a quoted empty string.
        (DbfCode::Char, EpicsValue::CharArray(v)) if v.len() == 1 => vec![char_scalar(v[0])],
        // A longer CHAR buffer is ONE escaped, quoted string (`:1022-1039`)
        // that stops at `epicsStrnLen(pbuffer, no_elements)` (`:1023`) — so
        // the NUL `dbpf` appends is a terminator, not a `\x00` in the text.
        // A buffer that STARTS with one gives `len == 0`, and C's
        // `while (len > 0)` then prints nothing under the header.
        (DbfCode::Char, EpicsValue::CharArray(v)) => {
            let len = v.iter().position(|&b| b == 0).unwrap_or(v.len());
            match len {
                0 => vec![],
                len => vec![quoted(&v[..len])],
            }
        }
        (DbfCode::UChar, EpicsValue::UChar(v)) => vec![format!("{v} = 0x{v:x}")],
        (DbfCode::UChar, EpicsValue::UCharArray(v)) => {
            v.iter().map(|e| format!("{e} = 0x{e:x}")).collect()
        }
        (DbfCode::Short, EpicsValue::Short(v)) => vec![i16_hex(*v)],
        (DbfCode::Short, EpicsValue::ShortArray(v)) => v.iter().map(|e| i16_hex(*e)).collect(),
        (DbfCode::UShort, EpicsValue::UShort(v)) => vec![format!("{v} = 0x{v:x}")],
        (DbfCode::UShort, EpicsValue::UShortArray(v)) => {
            v.iter().map(|e| format!("{e} = 0x{e:x}")).collect()
        }
        (DbfCode::Long, EpicsValue::Long(v)) => vec![i32_hex(*v)],
        (DbfCode::Long, EpicsValue::LongArray(v)) => v.iter().map(|e| i32_hex(*e)).collect(),
        (DbfCode::ULong, EpicsValue::ULong(v)) => vec![format!("{v} = 0x{v:x}")],
        (DbfCode::ULong, EpicsValue::ULongArray(v)) => {
            v.iter().map(|e| format!("{e} = 0x{e:x}")).collect()
        }
        (DbfCode::Int64, EpicsValue::Int64(v)) => vec![format!("{v} = {}", i64_hex(*v))],
        (DbfCode::Int64, EpicsValue::Int64Array(v)) => {
            v.iter().map(|e| format!("{e} = {}", i64_hex(*e))).collect()
        }
        (DbfCode::UInt64, EpicsValue::UInt64(v)) => vec![format!("{v} = 0x{v:x}")],
        (DbfCode::UInt64, EpicsValue::UInt64Array(v)) => {
            v.iter().map(|e| format!("{e} = 0x{e:x}")).collect()
        }
        (DbfCode::Float, EpicsValue::Float(v)) => vec![fmt_g(*v as f64, 6, false, false)],
        (DbfCode::Float, EpicsValue::FloatArray(v)) => v
            .iter()
            .map(|e| fmt_g(*e as f64, 6, false, false))
            .collect(),
        (DbfCode::Double, EpicsValue::Double(v)) => vec![fmt_g(*v, 12, false, false)],
        (DbfCode::Double, EpicsValue::DoubleArray(v)) => {
            v.iter().map(|e| fmt_g(*e, 12, false, false)).collect()
        }
        // `%u` — the bare index (`dbTest.c:1136-1141`). `dbgf` never reaches
        // this row because it substitutes `DBR_STRING` for a `DBR_ENUM` field
        // before it prints; `dbtgf` and `dbtpf` print the native code and do.
        (DbfCode::Enum, EpicsValue::Enum(v)) => vec![v.to_string()],
        (DbfCode::Enum, EpicsValue::EnumWithChoices { index, .. }) => vec![index.to_string()],
        (DbfCode::Enum, EpicsValue::EnumArray(v)) => v.iter().map(|e| e.to_string()).collect(),
        _ => vec![format!("Bad DBR type {}", dbr as i16)],
    }
}

/// One `printBuffer` block for a field read in a DBR type (`dbTest.c:984-1151`)
/// — the shape `dbgf`, `dbtgf`'s native line and `dbtpf`'s read-back share.
///
/// The value is converted to `dbr` first, exactly where C's
/// `dbFastGetConvertRoutine[field_type][dbrType]` runs, so the label and the
/// rendering are two readings of ONE decision rather than two decisions that
/// can disagree. A conversion C has no row for is its non-zero `status`, which
/// prints `failed.` under the header (`:994-997`).
fn native_readback_lines(dbr: DbfCode, val: &EpicsValue) -> Vec<String> {
    let Some((label, target)) = dbr_request(dbr) else {
        return printbuffer_lines(dbr.name(), 1, Some(&printbuffer_elements(dbr, val)));
    };
    match val.get_convert(target) {
        Ok(v) => printbuffer_block(label, dbr, &v),
        Err(_) => printbuffer_lines(label, 1, None),
    }
}

/// One `printBuffer` call (`dbTest.c:984-1151`) over an already-converted
/// value: `no_elements` reaches the header and the element switch as ONE
/// number, exactly as C passes one argument to both.
///
/// Deriving it twice is what let a `DBF_CHAR` buffer be labelled by its byte
/// count and rendered by its variant — a one-byte array printed
/// `"\x00"` where C prints the scalar row `0 = 0x0`.
fn printbuffer_block(label: &str, dbr: DbfCode, val: &EpicsValue) -> Vec<String> {
    printbuffer_lines(
        label,
        val.count() as usize,
        Some(&printbuffer_elements(dbr, val)),
    )
}

/// One `printBuffer` value block (`dbTest.c:1140-1151`): the
/// `DBF_<T>:` header, then either `failed.` or the element renderings,
/// laid out through one tab buffer and flushed as one line.
///
/// `count` is C's `no_elements`, which is NOT always the number of
/// renderings — a `DBF_CHAR` array is one escaped string yet reports
/// its byte count. Two rules come straight from C and are uniform for
/// every type: the zero-element header is built as ONE message with
/// `(empty)` appended (`strcat(pmsg, "(empty)")`, `dbTest.c:1144`) so
/// the pair shares a single tab stop rather than being padded apart,
/// and every element loop in the switch is bounded by `no_elements`,
/// so `count == 0` prints no element text whatever `elements` holds.
fn printbuffer_lines(dbr: &str, count: usize, elements: Option<&[String]>) -> Vec<String> {
    let mut buf = TabBuffer::new(10);
    match count {
        1 => buf.insert(&format!("DBF_{dbr}: ")),
        0 => buf.insert(&format!("DBF_{dbr}[0]: (empty)")),
        n => buf.insert(&format!("DBF_{dbr}[{n}]: ")),
    }
    match elements {
        // `status != 0` prints `failed.` regardless of the count.
        None => buf.insert("failed."),
        Some(values) if count > 0 => {
            for v in values {
                buf.insert(v);
            }
        }
        Some(_) => {}
    }
    buf.finish()
}

/// C `dbgf` (`dbTest.c:363-389`) once `nameToAddr` has resolved the name.
///
/// The field is read and labelled in its `DBADDR`'s DBR type
/// ([`field_addr_types`]) — except that a `DBR_ENUM` field is re-read as
/// `DBR_STRING` and prints `DBF_STRING` carrying its choice text (`:371-380`).
///
/// That substitution is `dbgf`'s alone. `dbtgf` (`dbTest.c:529-532`) and
/// `dbtpf` (`:693-696`) print `addr.dbr_field_type` unchanged, so the same
/// menu field's native line reads `DBF_ENUM` there and `DBF_STRING` here, and
/// both are C. `dbpf` ends by calling `dbgf` (`:433`) whatever the put
/// returned, so its read-back is this printer and not a second rendering.
///
/// A name with no `dbFldDes` behind it is C's record ATTRIBUTE, for which
/// `dbGetAttributePart` synthesises a `DBF_STRING` descriptor
/// (`dbStaticLib.c:1265-1272`); `RTYP` is the only such name this port serves.
fn dbgf_lines(ctx: &CommandContext, pname: &str, val: &EpicsValue) -> Vec<String> {
    let (rec_name, field) = parse_pv_name(pname);
    let field = if field.is_empty() { "VAL" } else { field };
    let Some(rec) = ctx.db().get_record(rec_name) else {
        return native_readback_lines(DbfCode::String, val);
    };
    let inst = rec.read();
    let Some((_, dbr)) = inst
        .field_desc(field)
        .map(|fd| field_addr_types(fd, Some(val)))
    else {
        return native_readback_lines(DbfCode::String, val);
    };
    if dbr != DbfCode::Enum {
        return native_readback_lines(dbr, val);
    }
    // C `dbGetField(&addr, DBR_STRING, ...)` on the enum field, which is
    // `getEnumString` / `getMenuString` / `getDeviceString`
    // (`dbConvert.c:833-911`) by DBF class. `field_as_dbr_string` is that
    // table's single owner here, so `dbgf` and a db-link read of one field
    // cannot disagree about its text.
    let text = inst.field_as_dbr_string(field).unwrap_or_default();
    native_readback_lines(DbfCode::String, &EpicsValue::String(text))
}

/// C `nameToAddr` (`dbTest.c:787-795`) — the one place every
/// `dbTest.c` command reports a name it cannot resolve. C prints this
/// line on stdout and the caller then returns -1 without printing
/// anything else, so the port must not route it through the shell's
/// error channel, which writes to stderr and prefixes `Error:`.
fn print_pv_not_found(ctx: &CommandContext, pname: &str) {
    ctx.println(&format!("PV '{pname}' not found"));
}

/// The byte width of one element of `code`, as C's `dbFldDes.size` carries
/// it — `sizeof(prec->field)`, stamped per field by the `GEN_SIZE_OFFSET`
/// block of the generated `xxxRecord.h`, except for `DBF_STRING` where it is
/// the `.dbd` `size(N)` attribute (`dbLexRoutines.c:663`, required non-zero
/// at `:755`).
///
/// `None` where the C number is a fact about a C struct this port does not
/// have: the three link types are `sizeof(DBLINK)` — measured as **80** on
/// `softIoc` linux-x86_64, and platform-dependent. Printing a width for those
/// would be inventing an ABI, so `dba` prints `(none)` instead.
///
/// A `DBF_NOACCESS` row reaches the last arm only when `field_addr_types` left
/// the declaration standing, i.e. when no `cvt_dbaddr` re-typed it — and those
/// are exactly the rows the generator carries a width for, taken from the C
/// type its `extra(...)` names. `dbCommon.TIME` is the case in hand: C prints
/// `Field Size: 8`, `sizeof(epicsTimeStamp)`. A width of zero still means the
/// port has no number, so it keeps `(none)`.
fn c_field_size(code: DbfCode, declared_size: u16) -> Option<u16> {
    Some(match code {
        DbfCode::String => declared_size,
        DbfCode::NoAccess if declared_size > 0 => declared_size,
        DbfCode::Char | DbfCode::UChar => 1,
        // `epicsEnum16` for all three (`epicsTypes.h`).
        DbfCode::Short | DbfCode::UShort | DbfCode::Enum | DbfCode::Menu | DbfCode::Device => 2,
        DbfCode::Long | DbfCode::ULong | DbfCode::Float => 4,
        DbfCode::Double | DbfCode::Int64 | DbfCode::UInt64 => 8,
        DbfCode::Inlink | DbfCode::Outlink | DbfCode::Fwdlink | DbfCode::NoAccess => return None,
    })
}

/// C `dbgf` when `dbGetField` fails after `nameToAddr` succeeded
/// (`dbTest.c:378-386`): `dbGet`'s validity gate reports through
/// `recGblDbaddrError` on the log, and `printBuffer`'s `status != 0` arm
/// prints the type header and `failed.` on stdout (`:994-997`).
///
/// The header is the field's own DBR word. C cannot print it — `dbr[]` is
/// declared `[DBR_ENUM+2]`, thirteen entries, while `DBR_NOACCESS` is
/// `DBF_NOACCESS` = 17 (`dbFldTypes.h:90`), so `dbr[17]` reads five past the
/// end and R7.0.10 prints the adjacent `dbf[1]`, `DBF_CHAR`. The string C
/// meant is in that table already, stranded at index 12. This prints it.
/// C's `sprintf` inside `dbGet`'s validity gate (`dbAccess.c:955-960`):
///
/// ```c
/// sprintf(message, "dbGet: dbrType = %d, field_type = %.12s (%d).",
///         dbrType, dbGetFieldTypeString(field_type), field_type);
/// ```
///
/// The two codes are separate inputs — `dbgf` passes `addr.dbr_field_type` as
/// `dbrType` (`dbTest.c:380`) while `field_type` is the declaration's — and
/// they coincide only because `mapDBFToDBR` is the identity on `DBF_NOACCESS`.
/// The `%.12s` is kept so the port truncates wherever C would, and there is no
/// trailing newline: `recGblDbaddrError`'s own format string owns the only one
/// (`recGbl.c:87-90`), so the report is one console line.
///
/// A function rather than a `format!` at the raise site so the exact bytes can
/// be pinned without capturing the log.
fn dbget_bad_dbrtype_message(dbr: DbfCode, field_type: DbfCode) -> String {
    let ft_name = format!("DBF_{}", field_type.name());
    format!(
        "dbGet: dbrType = {}, field_type = {ft_name:.12} ({}).",
        dbr as i16, field_type as i16
    )
}

fn dbgf_failed(ctx: &CommandContext, pname: &str) {
    let (rec_name, field) = parse_pv_name(pname);
    let field = if field.is_empty() { "VAL" } else { field };
    let (field_type, dbr, field_name) = match ctx.db().get_record(rec_name) {
        Some(rec) => {
            let inst = rec.read();
            match inst.field_desc(field) {
                Some(fd) => {
                    let (ft, dbr) = field_addr_types(fd, None);
                    (ft, dbr, fd.name)
                }
                None => (DbfCode::NoAccess, DbfCode::NoAccess, field),
            }
        }
        None => (DbfCode::NoAccess, DbfCode::NoAccess, field),
    };
    crate::server::recgbl::rec_gbl_dbaddr_error(
        "Illegal Database Request Type",
        rec_name,
        field_name,
        &dbget_bad_dbrtype_message(dbr, field_type),
    );
    for line in printbuffer_lines(dbr.name(), 1, None) {
        ctx.println(&line);
    }
}

/// C `printDbAddr` (`dbTest.c:795-818`), line for line.
///
/// Two of C's columns are C-storage facts with no counterpart here and print
/// `(none)` rather than a number:
///
/// * **Field Address** (`paddr->pfield`) — a C record is one struct and every
///   field sits at a fixed `dbFldDes.offset` inside it. This port reads a
///   field through [`Record::get_field`](crate::server::record::Record::get_field),
///   so there is no per-field address to print. The other two pointers ARE
///   real and carry C's meaning: the record address is the `Arc`'s pointee,
///   stable for the record's life and shared by every alias of it, and the
///   field description is the `&'static FieldDesc` in the generated table, so
///   comparing two `dba` outputs still answers "same record?" and "same
///   declaration?".
/// * **Field Size** for a link or `DBF_NOACCESS` field — see
///   [`c_field_size`].
///
/// The `????` C prints for an out-of-range type (`:807-808`, `:815-816`) is
/// unreachable here: [`DbfCode`] has no invalid value.
///
/// C's `dba` also resolves record ATTRIBUTES (`dbNameToAddr` falls back to
/// `dbGetAttributePart`, `dbAccess.c:670-672`), so `dba("rec.RTYP")` prints a
/// synthesised `DBF_STRING`/`SPC_ATTRIBUTE` descriptor (`dbStaticLib.c:1265-1272`).
/// This port has no attribute table — that is the subject `dbPutAttribute` is
/// still waiting on — so an attribute name reports not-found here.
fn print_db_addr(ctx: &CommandContext, pname: &str) -> bool {
    let (rec_name, field) = parse_pv_name(pname);
    let Some(rec) = ctx.db().get_record(rec_name) else {
        return false;
    };
    let field = if field.is_empty() { "VAL" } else { field };
    let inst = rec.read();
    let Some(fd) = inst.field_desc(field) else {
        return false;
    };

    let value = inst.resolve_field(field);
    let (field_type, dbr) = field_addr_types(fd, value.as_ref());
    // C: `no_elements = 1` (`dbAccess.c:635`), overwritten by `cvt_dbaddr`
    // with the CAPACITY for an array — `paddr->no_elements = prec->nelm`
    // (`waveformRecord.c::cvt_dbaddr`), never `NORD`. Reading the current
    // element count instead printed `No Elements: 0` for an unwritten
    // waveform where C prints its `NELM`. `field_native_count` is the same
    // NELM-first rule `CaChannel::create` already applies for `gft`/`pft`.
    let elements = inst
        .record
        .field_native_count(field)
        .or_else(|| inst.record.get_field(field).map(|v| v.count()))
        .unwrap_or(1);

    ctx.println(&format!(
        "Record Address: {:p} Field Address: (none) Field Description: {:p}",
        Arc::as_ptr(&rec),
        fd as *const FieldDesc,
    ));
    ctx.println(&format!("   No Elements: {elements}"));
    ctx.println(&format!("   Record Type: {}", inst.record.record_type()));
    ctx.println(&format!(
        "    Field Type: {} = DBF_{}",
        field_type as i16,
        field_type.name(),
    ));
    match c_field_size(field_type, fd.size) {
        Some(n) => ctx.println(&format!("    Field Size: {n}")),
        None => ctx.println("    Field Size: (none)"),
    }
    ctx.println(&format!("       Special: {}", fd.special as i16));
    ctx.println(&format!(
        "DBR Field Type: {} = DBR_{}",
        dbr as i16,
        dbr.name(),
    ));
    true
}

fn cmd_dba() -> CommandDef {
    CommandDef::new(
        "dba",
        vec![ArgDesc {
            // C `dbaArg0` (`dbIocRegister.c:185`).
            name: "record name",
            arg_type: ArgType::Record,
        }],
        "dba record name - Print the dbAddr structure for a field",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `dbTest.c:91-94`, returning 1.
            let name = match args.first() {
                Some(ArgValue::String(s)) if !s.is_empty() => s,
                _ => {
                    ctx.println("Usage: dba \"pv name\"");
                    return Ok(CommandOutcome::Failed);
                }
            };
            // C `dbTest.c:96-97` -> `nameToAddr` (`:785-793`), which prints
            // the not-found line itself and returns -1. `dba` has no
            // `dbt_requires_ioc_init` gate: C reads dbStatic only.
            if !print_db_addr(ctx, name) {
                print_pv_not_found(ctx, name);
                return Ok(CommandOutcome::Failed);
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

fn cmd_dbgf() -> CommandDef {
    CommandDef::new(
        "dbgf",
        vec![ArgDesc {
            // C `dbgfArg0` (`dbIocRegister.c:255`).
            name: "record name",
            arg_type: ArgType::Record,
        }],
        "dbgf record name - Get field value",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `dbTest.c:358-361`, returning 1.
            let name = match args.first() {
                Some(ArgValue::String(s)) if !s.is_empty() => s,
                _ => {
                    ctx.println("Usage: dbgf \"pv name\"");
                    return Ok(CommandOutcome::Failed);
                }
            };

            // C `dbTest.c:363-364` returns -1 — but only for a name
            // `nameToAddr` cannot resolve. A field the record type DECLARES
            // resolves, and its read then fails inside `dbGetField`, which is
            // a different outcome and prints differently.
            let got = ctx.db().get_pv(name);
            if matches!(got, Err(CaError::ChannelNotFound(_))) {
                print_pv_not_found(ctx, name);
                return Ok(CommandOutcome::Failed);
            }
            // C tests `lset` only AFTER `nameToAddr` (`dbTest.c:366-368`).
            if !dbt_requires_ioc_init(ctx, "dbgf") {
                return Ok(CommandOutcome::Failed);
            }
            match got {
                Ok(val) => {
                    for line in dbgf_lines(ctx, name, &val) {
                        ctx.println(&line);
                    }
                }
                // C `dbgf` returns 0 whatever `dbGetField` returned
                // (`dbTest.c:388`): the failure is a printed line, not a
                // failed command.
                Err(_) => dbgf_failed(ctx, name),
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

fn cmd_dbpf() -> CommandDef {
    CommandDef::new(
        "dbpf",
        vec![
            ArgDesc {
                // C `dbpfArg0` (`dbIocRegister.c:265`).
                name: "pvname",
                arg_type: ArgType::Record,
            },
            ArgDesc {
                name: "value",
                arg_type: ArgType::String,
            },
        ],
        "dbpf pvname value - Put field value",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `dbTest.c:400-403`: a missing or empty name, or a
            // missing value, is a usage line on stdout and a 1 return.
            let (name, value_str) = match (&args[0], &args[1]) {
                (ArgValue::String(n), ArgValue::String(v)) if !n.is_empty() => (n, v),
                _ => {
                    ctx.println("Usage: dbpf \"pv name\", \"value\"");
                    return Ok(CommandOutcome::Failed);
                }
            };

            // C resolves the name before it puts (`dbTest.c:405-406`),
            // so an unknown PV never reaches `dbPutField`; C returns -1.
            if ctx.db().get_pv(name).is_err() {
                print_pv_not_found(ctx, name);
                return Ok(CommandOutcome::Failed);
            }
            // C tests `lset` only AFTER `nameToAddr` (`dbTest.c:408-410`).
            if !dbt_requires_ioc_init(ctx, "dbpf") {
                return Ok(CommandOutcome::Failed);
            }

            let (base, field) = parse_pv_name(name);
            let field = field.to_ascii_uppercase();

            // Try to determine the field type for proper parsing
            let dbf_type = ctx.block_on(async {
                if let Some(rec) = ctx.db().get_record(base) {
                    let inst = rec.read();
                    // Check record-specific fields
                    if let Some(t) = crate::server::record::record_instance::declared_field_type_of(
                        inst.record.as_ref(),
                        &field,
                    ) {
                        return Some(t);
                    }
                    // Common field types
                    return common_field_dbf_type(&field);
                }
                None
            });

            // C `dbTest.c:413-429`: an ARRAY field is put through its OWN
            // `dbr_field_type`, never the `DBR_STRING` a scalar `dbpf` uses.
            // `no_elements` is the DBADDR capacity `cvt_dbaddr` wrote (`NELM`,
            // not `NORD`), and the resolved value already carries the type
            // that same call raised from `FTVL`.
            let array_addr = ctx.block_on(async {
                let rec = ctx.db().get_record(base)?;
                let inst = rec.read();
                let dbr = inst.resolve_field(&field)?.db_field_type();
                let capacity = inst
                    .record
                    .field_native_count(&field)
                    .or_else(|| inst.record.get_field(&field).map(|v| v.count()))
                    .unwrap_or(1);
                (capacity > 1).then_some((capacity as usize, dbr))
            });
            let array_value = match array_addr {
                Some((capacity, dbr @ (DbFieldType::Char | DbFieldType::UChar))) => {
                    // C `dbTest.c:415-416`: `n = strlen(pvalue) + 1` and the
                    // TEXT goes to `dbPutField` unparsed — a byte waveform
                    // takes the characters plus their NUL, so `dbpf B:WS 65`
                    // stores `'6','5',0` and reads back `DBF_CHAR[3]: "65"`,
                    // not the one byte 65 a JSON parse would have made.
                    let mut bytes = value_str.as_bytes().to_vec();
                    bytes.truncate(capacity.saturating_sub(1));
                    bytes.push(0);
                    Some(match dbr {
                        DbFieldType::UChar => EpicsValue::UCharArray(bytes),
                        _ => EpicsValue::CharArray(bytes),
                    })
                }
                Some((capacity, dbr)) => {
                    match crate::server::db_convert_json::db_put_convert_json(
                        value_str, dbr, capacity,
                    ) {
                        Ok(v) => Some(v),
                        // C `dbTest.c:425-426` returns the status BEFORE
                        // `dbPutField` and before the closing `dbgf`, so a
                        // refused literal writes nothing, prints nothing on
                        // stdout, and leaves the field as it was.
                        //
                        // The words are the CONVERTER's, not `dbpf`'s:
                        // `dbPutConvertJSON` errlogs `dbConvertJSON: %s`
                        // itself (`dbConvertJSON.c:170-176`) and hands its
                        // caller a bare status, so the operator sees an
                        // unframed stderr block and `dbpf` stays silent. This
                        // port's converter returns the text instead of
                        // printing it, so the errlog call sits at the call
                        // site until `dbConvertJSON.c`'s port takes it back.
                        //
                        // Two records, because C makes two `errlogPrintf`
                        // calls whenever a callback is what stopped the parse:
                        // its own refusal (`dbConvertJSON.c:31` and friends)
                        // and then yajl's `client cancelled parse` block.
                        Err(e) => {
                            // Both fields carry C's terminator, so each goes
                            // out as it stands — the errlog console appends
                            // nothing (`errlog.c:795`).
                            if let Some(refusal) = &e.refusal {
                                crate::runtime::log::errlog_printf(refusal);
                            }
                            crate::runtime::log::errlog_printf(&e.diagnostic);
                            return Ok(CommandOutcome::Failed);
                        }
                    }
                }
                None => None,
            };

            let value = if let Some(v) = array_value {
                v
            } else if field == "DTYP" {
                // DTYP is DBF_DEVICE: its choices are the record type's live
                // device menu (dynamic, per record type — device support names
                // registered at runtime), NOT the field-blind static table that
                // `EpicsValue::parse(Enum, _)` used to consult (the field-blind
                // menu table, removed with 03 L-7).
                // `declared_field_type_of(DTYP)` reports `Enum`, so the generic
                // path below parsed every device-support name as an enum/menu
                // string and rejected it ("invalid enum or menu string"), which
                // failed `dbpf <rec>.DTYP <device-support-name>` for every record
                // type. Feed the value straight to the put path, which already
                // resolves a numeric index against `device_menu` and stores a
                // device-support NAME as-is (tier-3, `put_common_field`'s "DTYP"
                // arm).
                match value_str.trim().parse::<i64>() {
                    Ok(i) => EpicsValue::Enum(i as u16),
                    Err(_) => EpicsValue::String(value_str.trim().into()),
                }
            } else if let Some(dbf) = dbf_type {
                // A `DBF_MENU`/`DBF_ENUM` label is menu-SPECIFIC — "Specified"
                // is index 1 of `menuFanout` but index 0 of `selSELM` — so its
                // one resolver is `resolve_menu_field_string`, which needs the
                // field's own menu. `dbpf` has only the field's declared TYPE
                // here, so it must not resolve the label itself: it hands the
                // token to the put path, exactly as the DTYP arm above does.
                // Resolving it here is what made `dbpf <sel>.SELM Specified`
                // store menuFanout's 1 instead of selSELM's 0.
                //
                // A numeric token still parses here, so a typed put keeps its
                // typed error, and a token that is neither a number nor a valid
                // choice is refused downstream by `put_string`/`bad_choice`
                // rather than silently landing as index 0.
                match dbf {
                    crate::types::DbFieldType::Short | crate::types::DbFieldType::Enum => {
                        EpicsValue::parse(dbf, value_str)
                            .unwrap_or_else(|_| EpicsValue::String(value_str.trim().into()))
                    }
                    _ => EpicsValue::parse(dbf, value_str)
                        .map_err(|e| format!("cannot parse '{value_str}' as {dbf:?}: {e}"))?,
                }
            } else {
                // No type info available, try as string
                EpicsValue::String(value_str.clone().into())
            };

            // C `dbpf` writes via `dbPutField` — processing put, but no
            // putNotify and no completion wait. Use the fire-and-forget
            // entry so the shell put never parks a notify wait-set on
            // the record. Fall back to put_pv for simple PVs.
            let put_result: CaResult<()> = ctx.block_on(async {
                let db = ctx.db();
                if db.get_record(base).is_some() {
                    db.put_record_field_from_ca_no_notify(base, &field, value)
                        .await
                } else {
                    db.put_pv(name, value).await
                }
            });
            // C hands `dbPutField`'s status straight to `iocshSetError`
            // (`dbIocRegister.c:272-273`), which only sets `scope.errored`
            // (`iocsh.cpp:1004-1018`) and prints nothing. Every word an
            // operator sees on a refused put therefore comes from INSIDE the
            // put — `recGblDbaddrError` for a bad request type, a record's
            // own `special()` for a bad CALC — and never from `dbpf`, so the
            // status may not be turned into a diagnostic here.
            let put_failed = put_result.is_err();

            // C `dbpf` ends with `dbgf(pname)` (`dbTest.c:433`) whatever
            // `dbPutField` returned, so the read-back is that one
            // printer rather than a second rendering, and a rejected
            // put still shows the value the record kept.
            if let Ok(val) = ctx.db().get_pv(name) {
                for line in dbgf_lines(ctx, name, &val) {
                    ctx.println(&line);
                }
            }

            if put_failed {
                Ok(CommandOutcome::Failed)
            } else {
                Ok(CommandOutcome::Continue)
            }
        },
    )
}

/// C `dbGetString` (`dbStaticLib.c:1888-1906`) for one non-link field — the
/// dbStatic-side renderer, and the ONLY one its callers may use.
///
/// C has two renderers for the same field and they disagree. This one answers
/// `dbpr` (`dbTest.c:1198-1203`) and `dbReportDeviceConfig` (`:3629`, `:3637`,
/// `:3641`, `:3645`); the other, `dbConvert`'s `DBR_STRING` path, answers
/// `dbgf` and every link read, and is
/// [`RecordInstance::field_as_dbr_string`](crate::server::record::RecordInstance::field_as_dbr_string).
/// They part on the two enum edges (see [`dbpr_choice_text`]) and on every
/// float: `realToString` here, `cvtDoubleToString` with the record's `PREC`
/// there — `dbReportDeviceConfig` asking the wrong one printed
/// `cvt(10000000,0.001)` where C prints `cvt(10000000,1.0e-03)`.
///
/// Link fields are not routed here: `dbGetString`'s link arms re-render from
/// the parsed link and `dbpr` prints the type alongside them, which is
/// [`render_dbpr_link`]'s job.
pub(super) fn db_get_string(
    inst: &crate::server::record::RecordInstance,
    desc: &crate::server::record::FieldDesc,
    value: &EpicsValue,
) -> String {
    // C `dbGetString` (`dbStaticLib.c:1888-1906`) switches on the DECLARED
    // `DBF_*` token and nothing else: `DBF_MENU` and `DBF_DEVICE` render their
    // choice string (`dbGetStringNum`, `:2131-2160`), while `DBF_ENUM` and
    // every numeric render the number (`:2094-2100`) — which is why C's `dbpr`
    // shows a `bo` as `VAL : 1` and its `SCAN` as `Passive`.
    //
    // The port used to ask a menu table keyed on the field NAME, a second
    // owner of "what choices does this field have" that had no entry for
    // `DTYP` — C's only `DBF_DEVICE` field — so every record's `dbpr` printed
    // its device index. Asking the declaration, then the ONE choice owner
    // (`RecordInstance::enum_string_form_for`, which is also what `dbgf` and
    // the CA encoders read), removes both the second table and the name key.
    let text = match desc.declared_dbf {
        DbfCode::Menu | DbfCode::Device => {
            // The index is an `epicsEnum16` in C whatever the port stores it
            // in; a value that is not an index at all leaves the choice list
            // unreachable and falls through to the plain rendering.
            let index = match value {
                EpicsValue::Enum(v) => Some(*v),
                EpicsValue::Short(v) => u16::try_from(*v).ok(),
                EpicsValue::UShort(v) => Some(*v),
                EpicsValue::UChar(v) => Some(u16::from(*v)),
                _ => None,
            };
            index.map(|i| dbpr_choice_text(inst, desc, i).unwrap_or_else(|| "<nil>".to_string()))
        }
        // C `dbGetStringNum`'s float arms are `floatToString` /
        // `doubleToString`, i.e. [`real_to_string`] — NOT the `%.*g` with the
        // record's `PREC` that a `DBR_STRING` read goes through
        // (`cvtDoubleToString`). Rust's own `Display` is a third rendering
        // again, and it is what printed `NaN` where C prints `nan`.
        // `base(HEX)` (`dbLexRoutines.c:652-661`) swaps the decimal converter
        // for a hex one on every integer arm of `dbGetStringNum`
        // (`dbStaticLib.c:2074-2124`) — measured on `softIoc`, an `mbbi`'s
        // `dbpr` reads `ZRVL: 0xa` where its `dbgf` still reads
        // `DBF_ULONG:          10 = 0xa`, because `dbgf` goes through
        // `dbConvert`, which has no base.
        _ if desc.base == Base::Hex => hex_string(value),
        _ => match value {
            EpicsValue::Float(v) => Some(real_to_string(f64::from(*v), false)),
            EpicsValue::Double(v) => Some(real_to_string(*v, true)),
            _ => None,
        },
    };
    text.unwrap_or_else(|| value.to_string())
}

/// C's two hex converters, chosen the way `dbGetStringNum` chooses them.
///
/// The 32-bit arms all call `ulongToHexString(epicsUInt32, ...)`
/// (`dbStaticLib.c:208-231`), so a SIGNED field is sign-extended to 32 bits
/// and printed unsigned — `LONG -1` reads `0xffffffff`, and there is no `-`
/// anywhere in that renderer. The 64-bit arms call `cvtInt64ToHexString` /
/// `cvtUInt64ToHexString` (`cvtFast.c:483-522`) instead, and those DO write a
/// leading `-` before the `0x`. Both drop leading zeros and print `0x0` for
/// zero.
///
/// C's `DBF_UINT64` arm reads its 64-bit field through an `epicsUInt32 *`
/// (`dbStaticLib.c:2123`), which truncates; no `.dbd` in base or the modules
/// declares `base(HEX)` on a `DBF_UINT64` field, so that arm is unreachable
/// and this port reads the whole width.
fn hex_string(value: &EpicsValue) -> Option<String> {
    fn u32_hex(v: u32) -> String {
        format!("0x{v:x}")
    }
    fn i64_hex(v: i64) -> String {
        match v {
            0 => "0x0".to_string(),
            v if v > 0 => format!("0x{v:x}"),
            i64::MIN => "-0x8000000000000000".to_string(),
            v => format!("-0x{:x}", v.unsigned_abs()),
        }
    }
    Some(match value {
        // C's `DBF_CHAR` arm dereferences a plain `char`, which is SIGNED on
        // every target this port builds for; `EpicsValue::Display` already
        // renders the same payload through `as i8` for that reason.
        EpicsValue::Char(v) => u32_hex(i32::from(*v as i8) as u32),
        EpicsValue::UChar(v) => u32_hex(u32::from(*v)),
        EpicsValue::Short(v) => u32_hex(i32::from(*v) as u32),
        EpicsValue::UShort(v) | EpicsValue::Enum(v) => u32_hex(u32::from(*v)),
        EpicsValue::Long(v) => u32_hex(*v as u32),
        EpicsValue::ULong(v) => u32_hex(*v),
        EpicsValue::Int64(v) => i64_hex(*v),
        EpicsValue::UInt64(v) => format!("0x{v:x}"),
        // A `base(HEX)` field that is not holding an integer — a string, a
        // link, an array — has no hex arm in C's switch either, so it falls
        // through to the caller's plain rendering.
        _ => return None,
    })
}

/// C `realToString` (`dbStaticLib.c:233-320`), the renderer every `DBF_FLOAT`
/// and `DBF_DOUBLE` field goes through on the dbStatic side — `dbpr`,
/// `dbDumpRecord`, `dbReportDeviceConfig` and the `.db` writer.
///
/// It is not `%g` and not Rust's `Display`. C prints the integer when the value
/// is within one `delta` of it, otherwise a fixed or exponential form trimmed
/// of trailing zeros with a half-up carry the trim performs itself, and an
/// exponential form always keeps a `.`, so `1e7` reads `1.0e+07` and `1.5`
/// reads `1.5`. `isdouble` picks the pair of constants C keys on: `1e-6`/`6`
/// digits for a `DBF_FLOAT`, `1e-15`/`14` for a `DBF_DOUBLE`.
fn real_to_string(value: f64, is_double: bool) -> String {
    const DELTA: [f64; 2] = [1e-6, 1e-15];
    const PRECISION: [i32; 2] = [6, 14];
    let i = usize::from(is_double);

    if value == 0.0 {
        return "0".to_string();
    }

    let absvalue = if value < 0.0 { -value } else { value };
    if absvalue < f64::from(i32::MAX) {
        let intval = value as i32;
        let diff = (value - f64::from(intval)).abs();
        if diff < absvalue * DELTA[i] {
            return intval.to_string();
        }
    }

    // C writes the sign into the buffer and advances past it, so everything
    // below formats the magnitude and the sign is prepended once at the end —
    // including in front of the `1` a full carry adds.
    let (sign, value) = if value < 0.0 {
        ("-", -value)
    } else {
        ("", value)
    };

    // C `(int)log10(value)`. A NaN or an infinity has no `log10`, and the cast
    // of the result is the integer-indefinite value on every target this port
    // runs on — outside `-2..=6` either way, so both take the exponential arm,
    // whose `%.*e` prints `nan`/`inf`: no `e` for C's `strchr` to find, and C
    // returns that text verbatim.
    let logval: i32 = if value.is_finite() {
        value.log10() as i32
    } else {
        i32::MIN
    };

    if logval > 6 || logval < -2 {
        let prec = PRECISION[i];
        let formatted = c_format_e(value, prec as usize);
        let Some(epos) = formatted.find('e') else {
            return format!("{sign}{formatted}");
        };
        let exponent = &formatted[epos..];
        let mut mantissa = real_trim(formatted.as_bytes()[..epos].to_vec(), prec, PRECISION[i]);
        if !mantissa.contains('.') {
            mantissa.push_str(".0");
        }
        format!("{sign}{mantissa}{exponent}")
    } else {
        let prec = (PRECISION[i] - logval).max(0);
        let formatted = format!("{value:.*}", prec as usize);
        let trimmed = real_trim(formatted.into_bytes(), prec, PRECISION[i]);
        format!("{sign}{trimmed}")
    }
}

/// C `realToString`'s trailing-zero trim and its carry (`dbStaticLib.c:293-317`).
///
/// The trim doubles as the rounding: it walks in from the last digit, and a run
/// of `9`s (or a digit `>= '8'` past `precision` places) sets `round`, after
/// which the second loop propagates a `+1` leftwards. A carry that runs off the
/// front produces a leading `1` — C writes it into the buffer ahead of the
/// digits, so it is returned here as part of the string.
fn real_trim(mut t: Vec<u8>, prec: i32, precision: i32) -> String {
    if prec <= 0 {
        return String::from_utf8_lossy(&t).into_owned();
    }
    let precision = precision as usize;
    let mut end = t.len() - 1;
    let mut round = false;
    while end > 0 {
        if t[end] == b'.' {
            end -= 1;
            break;
        }
        if t[end] == b'0' {
            end -= 1;
            continue;
        }
        if !round && end < precision {
            break;
        }
        if !round && t[end] < b'8' {
            break;
        }
        if t[end - 1] == b'.' {
            if round {
                end = end.saturating_sub(2);
            }
            break;
        }
        if t[end - 1] != b'9' {
            break;
        }
        round = true;
        end -= 1;
    }
    t.truncate(end + 1);
    // C's `while (round)` never clears `round` — every arm breaks — so the
    // loop is entered at most once and walks left until a digit takes the
    // increment.
    let mut carry = false;
    if round {
        loop {
            if t[end] < b'9' {
                t[end] += 1;
                break;
            }
            if end == 0 {
                carry = true;
                t[end] = b'0';
                break;
            }
            t[end] = b'0';
            end -= 1;
        }
    }
    let digits = String::from_utf8_lossy(&t);
    if carry {
        format!("1{digits}")
    } else {
        digits.into_owned()
    }
}

/// C `sprintf("%.*e", prec, value)`.
///
/// Rust's `{:e}` is not it: it writes `1.5e7` where C writes `1.5e+07`, and it
/// spells the non-finite values with capitals where glibc writes `inf` and
/// `nan`. Both differences are load-bearing — C's caller splits on the `e` and
/// pastes the exponent back verbatim, and its absence in `nan`/`inf` is what
/// makes those return early.
fn c_format_e(value: f64, prec: usize) -> String {
    if value.is_nan() {
        return if value.is_sign_negative() {
            "-nan"
        } else {
            "nan"
        }
        .to_string();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    let s = format!("{value:.*e}", prec);
    match s.split_once('e') {
        Some((mantissa, exp)) => {
            let (sign, digits) = match exp.strip_prefix('-') {
                Some(d) => ('-', d),
                None => ('+', exp.trim_start_matches('+')),
            };
            format!("{mantissa}e{sign}{digits:0>2}")
        }
        None => s,
    }
}

/// [`db_get_string`] addressed by field NAME, for a caller walking a record it
/// has no descriptor for (`dbReportDeviceConfig` does `dbFindField` then
/// `dbGetString` for each of its four columns).
///
/// `None` is C's `dbFindField` failure, which every caller treats as an empty
/// column rather than an error.
pub(super) fn db_get_string_field(
    inst: &crate::server::record::RecordInstance,
    field: &str,
) -> Option<String> {
    let desc = inst.field_desc(field)?;
    let value = inst.resolve_field(field)?;
    Some(db_get_string(inst, desc, &value))
}

/// C `dbGetStringNum`'s `DBF_MENU` and `DBF_DEVICE` arms
/// (`dbStaticLib.c:2131-2163`), which are NOT `dbConvert.c`'s `cvt_menu_st` /
/// `cvt_device_st` that a `DBR_STRING` read goes through. The two renderers
/// agree on every index that names a choice and disagree on both edges:
///
/// | | no choice list | index past the list |
/// |---|---|---|
/// | `dbGetStringNum` `DBF_MENU` | `NULL` | `"%u"` |
/// | `dbGetStringNum` `DBF_DEVICE` | `""` | `NULL` |
/// | `cvt_menu_st` / `cvt_device_st` | — | `"%u"` / `""` |
///
/// So only the edges are restated here; the choice LIST still comes from the
/// one owner (`RecordInstance::enum_string_form_for`), which is what keeps
/// `dbpr` and `dbgf` naming the same choice for the same index.
///
/// `None` is C's `NULL`, which `dbpr_report` prints as `<nil>`
/// (`dbTest.c:1198-1202`). The `DBF_DEVICE` empty string is the case in hand: a
/// record type with no `device()` declaration has no device menu, and C's
/// `dbpr` shows its `DTYP` as blank where the port showed the index `0`.
fn dbpr_choice_text(
    inst: &crate::server::record::RecordInstance,
    desc: &crate::server::record::FieldDesc,
    index: u16,
) -> Option<String> {
    let device = desc.declared_dbf == DbfCode::Device;
    let Some(form) = inst.enum_string_form_for(desc.name) else {
        return device.then(String::new);
    };
    match form.slots.get(index as usize) {
        Some(choice) => Some(choice.as_str_lossy().into_owned()),
        None if device => None,
        None => Some(index.to_string()),
    }
}

/// One link field's `dbpr` payload — C `dbpr_report`'s `"%s %s"` of the link
/// TYPE and `dbGetString`'s text (`dbTest.c:1205-1224`).
///
/// Both halves change at the same instant and for the same reason, so they
/// are decided together here. `dbInitRecordLinks` parses the link, writes
/// `plink->type` and then FREES `plink->text` (`dbStaticLib.c:2214-2231`).
/// Until that has run, C prints the literal word `LINK` and `dbGetString`
/// hands back `plink->text` verbatim (`dbStaticLib.c:1914-1915`); after it,
/// C prints the resolved type and `dbGetString` re-renders the link from the
/// parse, filling in the modifiers the `.db` left out. A link field the `.db`
/// never mentioned has no text on either side of that line and prints its
/// type — `CONSTANT` for every soft device support — with nothing after it.
///
/// `ioc_is_running` is this port's twin of "the text has been consumed".
/// `dbpr` is the only reader that can observe a link before that point:
/// `dbgf` and its siblings refuse to run pre-`iocInit`, and no CA or PVA
/// client can be connected yet — which is why [`crate::server::record::RecordInstance::resolve_field`]
/// renders unconditionally and the raw text is re-read here instead.
///
/// Measured on `bin/linux-x86_64/softIoc` (EPICS 7.0.10), `dbpr X 1`:
///
/// ```text
///                             before iocInit     after iocInit
/// field(INP,"5")              LINK 5             CONSTANT 5
/// field(INP,"[1,2,3]")                           CONSTANT [1,2,3]
/// field(INP,"L:B.VAL")        LINK L:B.VAL       DB_LINK L:B.VAL NPP NMS
/// field(INP,"L:B.VAL CPP MS")                    CA_LINK L:B.VAL CPP MS
/// field(INP,"OTHER:PV")       LINK OTHER:PV      CA_LINK OTHER:PV NPP NMS
/// field(INP,"OTHER:PV CA")                       CA_LINK OTHER:PV CA NMS
/// field(INP,{"const":[1,2,3]})                   JSON_LINK {"const":[1,2,3]}
/// field(INP,"@dev p1")        LINK @dev p1       CONSTANT                (*)
/// field(FLNK,"L:B")                              DB_LINK L:B
/// no INP at all               CONSTANT           CONSTANT
/// ```
///
/// (*) an `ai`'s Soft Channel support declares `CONSTANT`, so `dbCanSetLink`
/// rejects the `INST_IO` parse and the link keeps the devsup type with no
/// text at all. The word is `INST_IO` on a field whose device support
/// declares it.
fn render_dbpr_link(
    ctx: &CommandContext,
    class: crate::types::DbfLinkClass,
    raw: &str,
    rendered: &str,
    running: bool,
) -> String {
    if !running && !raw.is_empty() {
        return format!("LINK {raw}");
    }
    let ftype = crate::server::record::LinkFieldType::for_class(class);
    let parsed = ctx
        .db()
        .db_init_link_locality(crate::server::record::parse_link_field(raw, ftype));
    format!("{} {rendered}", parsed.link_type_after_init(raw).c_name())
}

fn cmd_dbpr() -> CommandDef {
    CommandDef::new(
        "dbpr",
        vec![
            ArgDesc {
                // C `dbprArg0` (`dbIocRegister.c:276`).
                name: "record",
                arg_type: ArgType::Record,
            },
            ArgDesc {
                name: "level",
                arg_type: ArgType::Int,
            },
        ],
        "dbpr record [level] - Print record fields (level 0-2)",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `dbTest.c:444-446`: a missing or empty name is a usage
            // line on stdout and a 1 return.
            let name = match &args[0] {
                ArgValue::String(s) if !s.is_empty() => s,
                _ => {
                    ctx.println("Usage: dbpr \"pv name\", level");
                    return Ok(CommandOutcome::Failed);
                }
            };
            let level = match &args[1] {
                ArgValue::Int(n) => *n as i32,
                ArgValue::Missing => 0,
                _ => 0,
            };

            // C `dbTest.c:449-450`: `nameToAddr` failing is a -1 return,
            // and `dbpr_report` failing a 1 (`:454-455`).
            if !dbpr_report(ctx, name, level) {
                return Ok(CommandOutcome::Failed);
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// C `dbpr_report` (`dbTest.c:1154-1277`) — everything `dbpr` does
/// once `nameToAddr` has resolved the name. `dbtr` runs the identical
/// walk at level 3 after it has processed the record
/// (`dbTest.c:494`), so the walk is a function here instead of the
/// tail of `cmd_dbpr`'s closure; both commands then print the same
/// field set for the same level by construction.
/// C's `DBF_NOACCESS` arm of `dbpr_report` (`dbTest.c:1225-1265`).
///
/// A `DBF_NOACCESS` row is never SERVED — its channel is refused at creation —
/// so there is no converted value to print and C does not ask for one: it
/// reads the C struct member directly and picks a renderer from the
/// DECLARATION. `TIME` is its one named case, keyed in C on the address
/// compare `pfield == &paddr->precord->time`; the port has no addresses to
/// compare, so it keys on the name the same declaration carries.
///
/// Everything else is `size` raw bytes as `%02x ` (`:1249-1262`), trailing
/// space included — C builds `"00 "` and lets the tab packer absorb it.
///
/// `None` for a declaration this port keeps no storage for. That is not a
/// silent hole: the row was absent before the descriptor existed too, and a
/// zero printed here would be a value C never read.
fn dbpr_no_access(
    inst: &crate::server::record::RecordInstance,
    d: &crate::server::record::FieldDesc,
) -> Option<String> {
    if d.name == "TIME" {
        // C `epicsTimeToStrftime(time_buf, 40, "%Y-%m-%d %H:%M:%S.%09f", ...)`
        // — the same routine, and the same 40-byte bound, as the soft
        // timestamp device support.
        return Some(
            crate::server::builtin_devices::timestamp::epics_time_to_strftime(
                "%Y-%m-%d %H:%M:%S.%09f",
                inst.common.time,
            ),
        );
    }
    let bytes: &[u8] = match d.name {
        "BKPT" => std::slice::from_ref(&inst.common.bkpt),
        _ => return None,
    };
    let mut out = String::new();
    for b in bytes.iter().take(usize::from(d.size)) {
        out.push_str(&format!("{b:02x} "));
    }
    Some(out)
}

pub(super) fn dbpr_report(ctx: &CommandContext, name: &str, level: i32) -> bool {
    // C's `nameToAddr` prints this line and the caller then returns
    // without printing anything else (`dbTest.c:449-450`, `:789-791`).
    // `false` is that failure: `dbpr` turns it into its -1, and `dbtr`
    // ignores it exactly as C does (`dbTest.c:494-495`).
    let rec = match ctx.db().get_record(name) {
        Some(rec) => rec,
        None => {
            print_pv_not_found(ctx, name);
            return false;
        }
    };

    // C `dbpr_report` (`dbTest.c:1156-1276`) walks the record
    // type's WHOLE field table — `dbCommon` first, then the
    // record-own fields — in `sortFldInd` order, which is the
    // field indices sorted by NAME (`dbLexRoutines.c:781-798`),
    // and skips every descriptor whose `interest` exceeds the
    // level (`dbTest.c:1181`). The port used to print a
    // hand-written per-level whitelist instead, so `interest`
    // was never read and a field C shows at level 0 could be
    // missing at level 2.
    let (fields, extras): (Vec<(String, String)>, Vec<(String, String)>) = ctx.block_on(async {
        let rec_name = { rec.read().name.clone() };
        let aliases = ctx.db().aliases_for_record(&rec_name);

        let inst = rec.read();
        let mut descs: Vec<&crate::server::record::FieldDesc> =
            crate::server::record::dbd_generated::DB_COMMON_FIELDS
                .iter()
                .chain(inst.record.field_list())
                .collect();
        descs.sort_by_key(|d| d.name);
        descs.dedup_by_key(|d| d.name);

        // A link field is `NAME: <type> <text>` where every other field is
        // `NAME: <text>` (`dbTest.c:1205-1224` against `:1198-1203`), and the
        // type is C's `plink->type`. Collect the stored text alongside the
        // rendered one here, while the record guard is held, and resolve the
        // type below: the locality half of `dbInitLink` reads the database's
        // record map, which nothing in this crate does under a record lock.
        let rows: Vec<(String, String, Option<(crate::types::DbfLinkClass, String)>)> = descs
            .iter()
            .filter(|d| i32::from(d.interest) <= level)
            .filter_map(|d| {
                // C's switch takes the `DBF_NOACCESS` arm FIRST
                // (`dbTest.c:1225`), before any of the arms that go through
                // `dbGetString`. Keeping that order here is what keeps
                // `dbf_type` — a "served as" answer these rows do not have —
                // out of the walk: the declaration alone renders them.
                if d.no_access() {
                    return dbpr_no_access(&inst, d).map(|t| (d.name.to_string(), t, None));
                }
                let value = inst.resolve_field(d.name)?;
                let link =
                    crate::types::dbf_link_class(inst.record.record_type(), d.name).map(|class| {
                        let raw = match inst.resolve_field_stored(d.name) {
                            Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
                            _ => String::new(),
                        };
                        (class, raw)
                    });
                Some((d.name.to_string(), db_get_string(&inst, d, &value), link))
            })
            .collect();

        // Port extensions, printed after C's block so the
        // parity half stays byte-for-byte C's: the alias
        // spellings that resolve to this record, and the
        // `info(...)` tags, which no base iocsh command
        // surfaces but a driver hint (`asyn:READBACK`,
        // `Q:group`) has to be checkable from the shell.
        let mut extras = Vec::new();
        if !aliases.is_empty() {
            extras.push(("ALIASES".to_string(), aliases.join(", ")));
        }
        if level >= 2 {
            let mut info_keys: Vec<&String> = inst.info.keys().collect();
            info_keys.sort();
            for key in info_keys {
                let val = inst.info.get(key).cloned().unwrap_or_default();
                extras.push((format!("info({key})"), val));
            }
        }
        drop(inst);

        let running = ctx.db().ioc_is_running();
        let fields = rows
            .into_iter()
            .map(|(name, text, link)| match link {
                None => (name, text),
                Some((class, raw)) => {
                    let text = render_dbpr_link(ctx, class, &raw, &text, running);
                    (name, text)
                }
            })
            .collect();
        (fields, extras)
    });

    // C `%-4s: %s` per field (`dbTest.c:1201-1203`), packed by
    // `dbpr_msgOut` at tab stop 20 (`dbTest.c:444`).
    let mut buf = TabBuffer::new(20);
    for (name, value) in &fields {
        buf.insert(&format!("{name:<4}: {value}"));
    }
    for line in buf.finish() {
        ctx.println(&line);
    }
    for (name, value) in &extras {
        ctx.println(&format!("{name:<4}: {value}"));
    }
    true
}

/// The 12 `DBR_*` types `dbtgf` requests, in C's order
/// (`dbTest.c:537-597`), each with the name `printBuffer` prints for
/// it (the `dbr` table indexed by request type).
const DBTGF_REQUEST_TYPES: [(&str, crate::types::DbFieldType); 12] = {
    use crate::types::DbFieldType as T;
    [
        ("STRING", T::String),
        ("CHAR", T::Char),
        ("UCHAR", T::UChar),
        ("SHORT", T::Short),
        ("USHORT", T::UShort),
        ("LONG", T::Long),
        ("ULONG", T::ULong),
        ("INT64", T::Int64),
        ("UINT64", T::UInt64),
        ("FLOAT", T::Float),
        ("DOUBLE", T::Double),
        ("ENUM", T::Enum),
    ]
};

/// The `DBR_STRING` row of a field whose text depends on the RECORD and not
/// only on the value.
///
/// Two C conversions live here and nowhere else on this path:
///
/// * `getDoubleString` (`dbConvert.c:772-799`) and `getFloatString`
///   (`:1601-1628`) format with `cvtDoubleToString(value, precision)`, taking
///   `precision` from the record's own `get_precision` slot and 6 when the
///   record type has none. This is why `dbtgf REC` on an `ai` with `PREC=3`
///   and `VAL=3.5` prints `"3.500"` where `dbgf REC` — which never leaves the
///   native type — prints `3.5`.
/// * `getEnumString`/`getMenuString`/`getDeviceString` render the CHOICE, not
///   the index, so C prints `"Passive"` for `REC.SCAN`, both from `dbtgf` and
///   from `gft` (measured on `softIoc` R7.0.10-146). The out-of-range answer
///   is the DBF class's, which is what [`EnumStringForm`] owns.
///
/// Converting value-to-value cannot reach either rule: both belong to the
/// record, and both are gone by the time an `EpicsValue::String` exists.
/// `None` for every other field type, whose `DBR_STRING` row is a plain get
/// conversion.
///
/// The quoting `dbtgf` shows is `printBuffer`'s, not the conversion's —
/// `gft` prints the same text bare — so it is applied by
/// [`dbtgf_string_elements`] and the row itself stays a pure conversion.
///
/// [`EnumStringForm`]: crate::server::snapshot::EnumStringForm
fn dbr_string_row(snap: &crate::server::snapshot::Snapshot, precision: i16) -> Option<Vec<String>> {
    use crate::calc::engine::cvt::cvt_double_to_string;

    let prec = precision.max(0) as u16;
    let text = |v: f64| cvt_double_to_string(v, prec);
    let label = |idx: u16| {
        snap.enums
            .as_ref()
            .map(|e| e.string_form.render(idx).as_str_lossy().into_owned())
            .unwrap_or_default()
    };
    match &snap.value {
        EpicsValue::Double(v) => Some(vec![text(*v)]),
        EpicsValue::Float(v) => Some(vec![text(*v as f64)]),
        EpicsValue::DoubleArray(v) => Some(v.iter().map(|e| text(*e)).collect()),
        EpicsValue::FloatArray(v) => Some(v.iter().map(|e| text(*e as f64)).collect()),
        EpicsValue::Enum(v) => Some(vec![label(*v)]),
        EpicsValue::EnumArray(v) => Some(v.iter().map(|e| label(*e)).collect()),
        _ => None,
    }
}

/// [`dbr_string_row`] through `printBuffer`'s quoting and escaping.
fn dbtgf_string_elements(
    snap: &crate::server::snapshot::Snapshot,
    precision: i16,
) -> Option<Vec<String>> {
    Some(
        dbr_string_row(snap, precision)?
            .into_iter()
            .map(|t| format!("\"{}\"", escape_char_array_for_dbgf(t.as_bytes())))
            .collect(),
    )
}

/// C `epicsTimeToStrftime(.., "%Y-%m-%d %H:%M:%S.%09f", ..)` as
/// `printBuffer` calls it (`dbTest.c:1053-1057`). A stamp still at the
/// EPICS epoch is the uninitialized one and renders `<undefined>`
/// (`epicsTime.cpp::strftime`).
fn dbtgf_time_text(ts: crate::types::WallTime) -> String {
    let epics_epoch_unix = crate::runtime::general_time::EPICS_EPOCH_UNIX_SECS;
    if ts.unix_secs() == epics_epoch_unix && ts.subsec_nanos() == 0 {
        return "<undefined>".to_string();
    }
    let secs = ts.unix_secs() as i64;
    let nanos = ts.subsec_nanos();
    match chrono::DateTime::from_timestamp(secs, 0) {
        Some(dt) => format!(
            "{}.{nanos:09}",
            dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S")
        ),
        None => "<undefined>".to_string(),
    }
}

/// The option block `dbtgf` prints before the values: C asks for every
/// option bit at once (`req_options = 0xffffffff`, `dbTest.c:527-534`)
/// and `printBuffer` renders each one it got back, or a
/// "not returned" line for each the record type does not supply.
///
/// # Two C defects this does NOT reproduce
///
/// The values C prints here are garbage on every base since 7.0.6, and
/// matching them byte for byte would mean reproducing two memory bugs
/// rather than a format:
///
/// * `dbAccessDefs.h:35-47` renumbered the option bits to insert
///   `DBR_AMSG` (0x2) and `DBR_UTAG` (0x20), and `getOptions`
///   (`dbAccess.c:336-360`) writes both payloads into the buffer —
///   `DB_AMSG_SIZE` is 40 bytes. `printBuffer` (`dbTest.c:1000-1090`)
///   was never given a step for either, so it reads `units`,
///   `precision`, `time` and every gr/ctrl/al block from the wrong
///   offset. Measured on `bin/linux-x86_64/softIoc`
///   (R7.0.10-146-g8f5015b663d764ad75df) against an `ai` with
///   `EGU="mm"`, `PREC=3`, `HOPR=100`, `LOPR=-100`, processed:
///   `units = ""`, `precision = 0`, `time = <undefined>`,
///   `alLong: 1079574528 < 0 .. -100 < 100`,
///   `alDouble: -100 < 100 .. -nan < -100`.
/// * `get_alarm` (`dbAccess.c:422-448`) writes the `DBR_AL_DOUBLE`
///   payload through `pbuffer`, a local copy it never advances — only
///   `*ppbuffer` moves — so a `dbGet` asking for both alarm forms gets
///   the long block overwritten by the double one. `dbtgf` is the only
///   caller that asks for both.
///
/// The line shapes below are C's; the values are the ones C intends.
/// Which lines appear is C's answer either way, because it is decided
/// by the returned option mask ([`PropertySupport`]), not by the
/// buffer walk.
///
/// [`PropertySupport`]: crate::server::snapshot::PropertySupport
fn dbtgf_option_lines(
    field_type: DbfCode,
    snap: &crate::server::snapshot::Snapshot,
) -> Vec<String> {
    use crate::calc::engine::cvt::fmt_g;

    let g = |v: f64| fmt_g(v, 6, false, false);
    // C `(epicsInt32)` on the double limit (`dbAccess.c:373-374`), and
    // `finite(x) ? (epicsInt32)x : 0` for the alarm limits (`:428-435`).
    let l = |v: f64| -> i32 { if v.is_finite() { v as i32 } else { 0 } };

    let mut out = Vec::new();
    out.push(format!(
        "status = {}, severity = {}",
        snap.alarm.status, snap.alarm.severity
    ));
    match snap.units() {
        Some(u) => out.push(format!("units = \"{}\"", u.as_str_lossy())),
        None => out.push("units not returned".to_string()),
    }
    match snap.precision() {
        Some(p) => out.push(format!("precision = {p}")),
        None => out.push("precision not returned".to_string()),
    }
    // C prints `time = <undefined>` for EVERY field here, whatever the
    // record's stamp is, because `printBuffer` walks past the `DBR_AMSG` and
    // `DBR_UTAG` payloads it was never given a step for and reads `time` from
    // the wrong offset. That is output C itself calls undefined, so this port
    // prints the record's real stamp rather than reproduce the misread — the
    // one line of this block that is deliberately not C's value.
    out.push(format!("time = {}", dbtgf_time_text(snap.timestamp)));
    // C `get_enum_strs` (`dbAccess.c:160-180`) reaches the record's
    // `get_enum_strs` / the field's menu ONLY for `paddr->field_type` in
    // {`DBF_ENUM`, `DBF_MENU`, `DBF_DEVICE`}; every other class falls straight
    // through to `nostrs`, which clears the option bit. The class is the
    // address's, so an `mbbo` whose `cvt_dbaddr` demoted it to `DBF_USHORT` for
    // want of a state string reports the strings as not returned even though
    // the record still owns a choice table.
    let enums = matches!(field_type, DbfCode::Enum | DbfCode::Menu | DbfCode::Device)
        .then_some(snap.enums.as_ref())
        .flatten();
    match enums {
        Some(e) => {
            out.push(format!("no_strs = {}:", e.strings.len()));
            for s in &e.strings {
                out.push(format!("\t\"{}\"", s.as_str_lossy()));
            }
        }
        None => out.push("enum strings not returned".to_string()),
    }
    match snap.graphic_limits() {
        Some((lo, hi)) => {
            out.push(format!("grLong: {} .. {}", l(lo), l(hi)));
            out.push(format!("grDouble: {} .. {}", g(lo), g(hi)));
        }
        None => {
            out.push("DBRgrLong not returned".to_string());
            out.push("DBRgrDouble not returned".to_string());
        }
    }
    match snap.control_limits() {
        Some((lo, hi)) => {
            out.push(format!("ctrlLong: {} .. {}", l(lo), l(hi)));
            out.push(format!("ctrlDouble: {} .. {}", g(lo), g(hi)));
        }
        None => {
            out.push("DBRctrlLong not returned".to_string());
            out.push("DBRctrlDouble not returned".to_string());
        }
    }
    match snap.alarm_limits() {
        Some((lolo, low, high, hihi)) => {
            out.push(format!(
                "alLong: {} < {} .. {} < {}",
                l(lolo),
                l(low),
                l(high),
                l(hihi)
            ));
            out.push(format!(
                "alDouble: {} < {} .. {} < {}",
                g(lolo),
                g(low),
                g(high),
                g(hihi)
            ));
        }
        None => {
            out.push("DBRalLong not returned".to_string());
            out.push("DBRalDouble not returned".to_string());
        }
    }
    out
}

/// The `<cmd> only works after iocInit` gate five commands share —
/// `dbgf` (`dbTest.c:366-368`), `dbpf` (`:408-410`), `dbtr` (`:476-478`),
/// `dbtgf` (`:520-522`) and `dbtpf` (`:621-623`). C tests
/// `addr.precord->lset == NULL`, the field `iocInit` fills; the port's
/// twin is the database's own init phase.
///
/// `dbpr` deliberately has NO such gate in C and must not gain one here:
/// measured on `softIoc` R7.0.10-146, `dbpr "R:SRC"` before `iocInit`
/// prints the record's fields, while `dbgf` and `dbpf` on the same name in
/// the same session print the refusal.
fn dbt_requires_ioc_init(ctx: &CommandContext, cmd: &str) -> bool {
    if ctx.db().ioc_is_running() {
        return true;
    }
    ctx.println(&format!("{cmd} only works after iocInit"));
    false
}

/// `dbtgf "pv name"` — Database Test Get Field.
///
/// C `dbtgf` (`dbTest.c:500-604`): print the record's option block,
/// then read the field once per `DBR_*` request type and print each
/// answer or `failed.`.
fn cmd_dbtgf() -> CommandDef {
    CommandDef::new(
        "dbtgf",
        vec![ArgDesc {
            // C `dbtgfArg0` (`dbIocRegister.c:301`).
            name: "record name",
            arg_type: ArgType::Record,
        }],
        "dbtgf record name - Get field with all DBR_* request types",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C returns 1 here and -1 below, both non-zero, so
            // `iocshSetError` fails the line after the command has
            // already said everything it is going to say.
            let name = match args.first() {
                Some(ArgValue::String(s)) if !s.is_empty() => s,
                _ => {
                    ctx.println("Usage: dbtgf \"pv name\"");
                    return Ok(CommandOutcome::Failed);
                }
            };
            let (base, field) = parse_pv_name(name);
            let field = field.to_ascii_uppercase();
            let Some(rec) = ctx.db().get_record(base) else {
                print_pv_not_found(ctx, name);
                return Ok(CommandOutcome::Failed);
            };
            if !dbt_requires_ioc_init(ctx, "dbtgf") {
                return Ok(CommandOutcome::Failed);
            }
            let Some((snap, class, native)) = ({
                let inst = rec.read();
                inst.snapshot_for_field(&field).map(|snap| {
                    // C prints `addr.dbr_field_type` here with NO `DBR_STRING`
                    // substitution — that is `dbgf`'s alone — so a menu field's
                    // native line reads `DBF_ENUM`. The option block below is
                    // decided by the other word, `addr.field_type`.
                    let (class, dbr) = inst
                        .field_desc(&field)
                        .map_or((DbfCode::String, DbfCode::String), |fd| {
                            field_addr_types(fd, Some(&snap.value))
                        });
                    (snap, class, dbr)
                })
            }) else {
                print_pv_not_found(ctx, name);
                return Ok(CommandOutcome::Failed);
            };

            for line in dbtgf_option_lines(class, &snap) {
                ctx.println(&line);
            }
            // C's first `dbGetField` asks for the native type with
            // `no_elements = 0` (`dbTest.c:529-532`), so the native
            // block is always the empty-array header.
            for line in printbuffer_lines(native.name(), 0, Some(&[])) {
                ctx.println(&line);
            }
            // C `long precision = 6` before it asks the record
            // (`dbConvert.c:778-783`).
            let precision = snap.precision().unwrap_or(6);
            for (dbr, target) in DBTGF_REQUEST_TYPES {
                let float_string = (target == crate::types::DbFieldType::String)
                    .then(|| dbtgf_string_elements(&snap, precision))
                    .flatten();
                let lines = match float_string {
                    Some(elements) => printbuffer_lines(dbr, elements.len(), Some(&elements)),
                    None => match snap.value.get_convert(target) {
                        Ok(v) => printbuffer_block(dbr, target.dbf_code(), &v),
                        Err(_) => printbuffer_lines(dbr, 1, None),
                    },
                };
                for line in lines {
                    ctx.println(&line);
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbtpf "pv name", "value"` — Database Test Put Field.
///
/// C `dbtpf` (`dbTest.c:604-709`): put the text as every `DBR_*` type
/// in turn and read the record's native value back after each put that
/// C's `epicsParse*` accepted.
fn cmd_dbtpf() -> CommandDef {
    CommandDef::new(
        "dbtpf",
        vec![
            ArgDesc {
                // C `dbtpfArg0`/`Arg1` (`dbIocRegister.c:311-312`).
                name: "record name",
                arg_type: ArgType::Record,
            },
            ArgDesc {
                name: "value",
                arg_type: ArgType::String,
            },
        ],
        "dbtpf record name, value - Put field with all DBR_* request types",
        |args: &[ArgValue], ctx: &CommandContext| {
            let (name, value_str) = match (&args[0], &args[1]) {
                (ArgValue::String(n), ArgValue::String(v)) if !n.is_empty() => (n, v),
                _ => {
                    ctx.println("Usage: dbtpf \"pv name\", \"value\"");
                    return Ok(CommandOutcome::Failed);
                }
            };
            let (base, field) = parse_pv_name(name);
            let field = field.to_ascii_uppercase();
            if ctx.db().get_record(base).is_none() {
                print_pv_not_found(ctx, name);
                return Ok(CommandOutcome::Failed);
            }
            if !dbt_requires_ioc_init(ctx, "dbtpf") {
                return Ok(CommandOutcome::Failed);
            }

            for (dbr, target) in DBTGF_REQUEST_TYPES {
                // C parses the text ITSELF before the put, with
                // `epicsParse*(pvalue, &val, 10, NULL)` — base 10 and a
                // NULL `units`, so a trailing tail is
                // `S_stdlib_extraneous` (`dbTest.c:645-679`). That is a
                // stricter parse than the `dbConvert` row a `dbpf` of the
                // same text would take, which is why `9.25` fails every
                // integer request type here and stores 9 there.
                // `DBR_STRING` has no parse at all — C passes `pvalue`
                // straight through — and `DBR_ENUM` is `epicsParseUInt16`.
                let parsed = match target {
                    crate::types::DbFieldType::String => {
                        Some(EpicsValue::String(value_str.as_str().into()))
                    }
                    crate::types::DbFieldType::Enum => {
                        match crate::types::c_parse::parse_base10_units_null(
                            crate::types::c_parse::NumericField::UShort,
                            value_str,
                        ) {
                            Some(EpicsValue::UShort(v)) => Some(EpicsValue::Enum(v)),
                            _ => None,
                        }
                    }
                    t => crate::types::c_parse::NumericField::of(t).and_then(|nf| {
                        crate::types::c_parse::parse_base10_units_null(nf, value_str)
                    }),
                };
                let Some(value) = parsed else {
                    ctx.println(&format!("Cvt to DBR_{dbr} failed."));
                    continue;
                };
                let put: CaResult<()> = ctx.block_on(
                    ctx.db()
                        .put_record_field_from_ca_no_notify(base, &field, value),
                );
                if put.is_err() {
                    ctx.println(&format!("Put as DBR_{dbr:<6} Failed."));
                    continue;
                }
                // C re-reads in the record's NATIVE type and prints it
                // through the same tab buffer, whose column count
                // restarts after the un-tabbed prefix (`dbTest.c:690-696`).
                let readback = ctx.db().get_record(base).and_then(|r| {
                    let inst = r.read();
                    let snap = inst.snapshot_for_field(&field)?;
                    let dbr = inst.field_desc(&field).map_or(DbfCode::String, |fd| {
                        field_addr_types(fd, Some(&snap.value)).1
                    });
                    Some((snap, dbr))
                });
                let lines = match &readback {
                    // C re-reads with `addr.dbr_field_type` and does NOT
                    // substitute `DBR_STRING` for an enum field the way `dbgf`
                    // does, so a `bo` read-back is `DBF_ENUM: 1`.
                    Some((snap, dbr)) => native_readback_lines(*dbr, &snap.value),
                    None => Vec::new(),
                };
                let mut lines = lines.into_iter();
                let head = lines.next().unwrap_or_default();
                ctx.println(&format!("Put as DBR_{dbr:<6} Ok, result as {head}"));
                for line in lines {
                    ctx.println(&line);
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// C's `MAX_UNITS_SIZE` (`db_access.h:36`), the width `dbChannel_get` copies a
/// record's EGU into. C `strncpy`s 8 bytes and then forces
/// `units[MAX_UNITS_SIZE-1] = '\0'` (`db_access.c:448-449`), so what a user
/// sees is capped at SEVEN characters — not at the 8 of the `%.8s` that
/// prints it.
const CA_MAX_UNITS_SIZE: usize = 8;

/// C's `MAX_ENUM_STRING_SIZE` (`db_access.h:34`) — `ca_dump_dbr`'s `%.26s`.
const CA_MAX_ENUM_STRING_SIZE: usize = 26;

/// C's `MAX_ELEMS` (`db_test.c:33`), the element ceiling `gft` clamps a
/// waveform to before it starts asking for DBR types.
const GFT_MAX_ELEMS: u32 = 10;

/// C `dbChannel_get`'s zero fill (`db_access.c:127-136`): when the database
/// returns fewer elements than were asked for, C pads the rest of the buffer
/// before the caller reads it, so the caller always sees exactly the count it
/// requested.
///
/// A reply shorter than one element per requested slot arrives here as a
/// scalar; C's buffer makes no such distinction, so it becomes the one-element
/// array plus the zeros.
fn ca_zero_fill(value: EpicsValue, no_elements: usize) -> EpicsValue {
    use EpicsValue as V;

    fn pad<T: Default>(mut v: Vec<T>, n: usize) -> Vec<T> {
        v.resize_with(n, T::default);
        v
    }

    if no_elements <= 1 || value.count() as usize >= no_elements {
        return value;
    }
    let n = no_elements;
    match value {
        V::ShortArray(v) => V::ShortArray(pad(v, n)),
        V::FloatArray(v) => V::FloatArray(pad(v, n)),
        V::EnumArray(v) => V::EnumArray(pad(v, n)),
        V::DoubleArray(v) => V::DoubleArray(pad(v, n)),
        V::LongArray(v) => V::LongArray(pad(v, n)),
        V::CharArray(v) => V::CharArray(pad(v, n)),
        V::StringArray(v) => V::StringArray(pad(v, n)),
        V::Short(v) => V::ShortArray(pad(vec![v], n)),
        V::Float(v) => V::FloatArray(pad(vec![v], n)),
        V::Enum(v) => V::EnumArray(pad(vec![v], n)),
        V::Double(v) => V::DoubleArray(pad(vec![v], n)),
        V::Long(v) => V::LongArray(pad(vec![v], n)),
        V::Char(v) => V::CharArray(pad(vec![v], n)),
        V::String(v) => V::StringArray(pad(vec![v], n)),
        // Not a family a CA request converts to, so there is no buffer of
        // this shape for C to have filled.
        other => other,
    }
}

/// C's element loop, shared by every `ca_dump_dbr` branch that has one:
///
/// ```c
/// for (i = 0; i < count; i++) {
///     if (count != 1 && (i % wrap == 0)) printf("\n");
///     printf(fmt " ", value);
/// }
/// ```
///
/// A newline BEFORE every `wrap`-th element and only for a non-scalar, plus
/// one trailing space after every element including the last.
fn ca_dump_elements(count: usize, wrap: usize, mut render: impl FnMut(usize) -> String) -> String {
    let mut out = String::new();
    for i in 0..count {
        if count != 1 && i % wrap == 0 {
            out.push('\n');
        }
        out.push_str(&render(i));
        out.push(' ');
    }
    out
}

/// The elements of a value already converted to one of C's integral DBR
/// families, sign preserved so each branch can apply its own C format.
fn ca_dump_ints(value: &EpicsValue) -> Vec<i64> {
    match value {
        EpicsValue::Short(v) => vec![*v as i64],
        EpicsValue::ShortArray(v) => v.iter().map(|e| *e as i64).collect(),
        EpicsValue::Enum(v) => vec![*v as i64],
        EpicsValue::EnumArray(v) => v.iter().map(|e| *e as i64).collect(),
        EpicsValue::Char(v) => vec![*v as i64],
        EpicsValue::CharArray(v) => v.iter().map(|e| *e as i64).collect(),
        EpicsValue::Long(v) => vec![*v as i64],
        EpicsValue::LongArray(v) => v.iter().map(|e| *e as i64).collect(),
        _ => Vec::new(),
    }
}

/// The elements of a value already converted to `DBR_FLOAT`/`DBR_DOUBLE`.
fn ca_dump_reals(value: &EpicsValue) -> Vec<f64> {
    match value {
        EpicsValue::Float(v) => vec![*v as f64],
        EpicsValue::FloatArray(v) => v.iter().map(|e| *e as f64).collect(),
        EpicsValue::Double(v) => vec![*v],
        EpicsValue::DoubleArray(v) => v.to_vec(),
        _ => Vec::new(),
    }
}

/// The elements of a value already converted to `DBR_STRING`.
fn ca_dump_texts(value: &EpicsValue) -> Vec<String> {
    match value {
        EpicsValue::String(v) => vec![v.as_str_lossy().into_owned()],
        EpicsValue::StringArray(v) => v.iter().map(|e| e.as_str_lossy().into_owned()).collect(),
        _ => Vec::new(),
    }
}

/// C `printf("%<width>.<decimals>f", x)` where `x` reaches the call as a
/// `float`. The `(float)` casts `ca_dump_dbr` applies to every double it
/// prints are load bearing — they round to single precision BEFORE the
/// decimals are taken — so the narrowing lives here rather than at each call
/// site.
fn ca_dump_f(x: f64, width: usize, decimals: usize) -> String {
    let narrowed = crate::types::c_cast::f64_to_f32(x) as f64;
    let text = crate::calc::engine::cvt::fmt_f(narrowed, decimals);
    format!("{text:>width$}")
}

/// One graphic/alarm/control limit of an integral `DBR_GR_*`/`DBR_CTRL_*`,
/// printed `"%8d"`.
///
/// `limit` has already been through
/// [`limits_as_integers`](crate::types::codec::limits_as_integers) — the
/// `epicsInt32` block `dbAccess.c` fills for the
/// `DBR_GR_LONG`/`DBR_CTRL_LONG`/`DBR_AL_LONG` options, alarm-limit NaN
/// guard included. What is left is C's second step: `db_access.c` assigns
/// that block into the reply struct's own integer field — `dbr_short_t` for
/// the SHORT family, `dbr_char_t` for CHAR, `dbr_long_t` for LONG
/// (`:450-455`) — an ordinary C integer conversion, i.e. modular truncation,
/// which is what `as` gives here. `family` is the plain DBR code of the
/// family, so a `HOPR` of 300 on a `DBR_GR_CHAR` really does read back as 44.
fn ca_dump_int_limit(limit: i32, family: u16) -> String {
    use crate::types::{DBR_CHAR, DBR_SHORT};

    let narrowed = match family {
        DBR_SHORT => limit as i16 as i64,
        DBR_CHAR => limit as u8 as i64,
        _ => limit as i64,
    };
    format!("{narrowed:8}")
}

/// The six graphic/alarm limits in the `upper_disp, lower_disp, upper_alarm,
/// upper_warning, lower_warning, lower_alarm` order every `GR_`/`CTRL_`
/// branch prints them in (`test_event.cpp:340-346`).
fn ca_dump_int_limits(limits: &[i32], family: u16) -> String {
    limits
        .iter()
        .map(|l| ca_dump_int_limit(*l, family))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The same six for a real family, `"%8.3f %8.3f %8.3f %8.3f %8.3f %8.3f"`
/// (`test_event.cpp:357-364`).
fn ca_dump_real_limits(limits: &[f64]) -> String {
    limits
        .iter()
        .map(|l| ca_dump_f(*l, 8, 3))
        .collect::<Vec<_>>()
        .join(" ")
}

/// C `epicsTimeToStrftime(buf, 50, "%Y/%m/%d %H:%M:%S.%06f", &stamp)`, the one
/// call `ca_dump_dbr` makes for every `DBR_TIME_*`.
///
/// Six digits rather than [`dbtgf_time_text`]'s nine, and six is where
/// `epicsTime.cpp:527-535` actually rounds: `frac = nsec + div[6]/2` with
/// `div[6] = 1000`, clamped below a whole second so the carry can never reach
/// `%S`.
fn ca_dump_time_stamp(ts: crate::types::WallTime) -> String {
    let epics_epoch_unix = crate::runtime::general_time::EPICS_EPOCH_UNIX_SECS;
    let nsec = ts.subsec_nanos() as u64;
    if ts.unix_secs() == epics_epoch_unix && nsec == 0 {
        return "<undefined>".to_string();
    }
    let micros = (nsec + 500).min(999_999_999) / 1_000;
    match chrono::DateTime::from_timestamp(ts.unix_secs() as i64, 0) {
        Some(dt) => format!(
            "{}.{micros:06}",
            dt.with_timezone(&chrono::Local).format("%Y/%m/%d %H:%M:%S")
        ),
        None => "<undefined>".to_string(),
    }
}

/// C `ca_dump_dbr` (`ca/src/client/test_event.cpp:51-587`) — the formatter
/// `gft` and `pft` print every DBR answer with.
///
/// `value` is the reply's elements ALREADY converted to `dbr`'s family and
/// `snap` is where its metadata comes from; C assembles the same two halves
/// in `dbChannel_get` (`db_access.c:143-806`) and hands one packed struct
/// here. Metadata a record type does not supply reads as zero, which is C's
/// answer too — `getOptions` memsets the payload of every slot it has to turn
/// off (`dbAccess.c:229-230`, `:376-393`).
///
/// The returned text carries C's embedded newlines but not its final one; the
/// caller's `println` is C's closing `printf("\n")` (`:586`).
///
/// How many elements to print is a property of the REPLY, not a second
/// argument: C has to pass a count because a C buffer has no length, and
/// `dbChannel_get` guarantees the buffer really holds that many by zero-filling
/// whatever the database did not return (`db_access.c:127-136`). The port's
/// reply carries its own length and [`CaChannel::get`] applies the same fill,
/// so an unprocessed `NELM=3` waveform arrives here as three zeros and prints
/// as three zeros. Taking a separate count and then shortening it to the
/// value's length — which is how this printed nothing at all — gives one
/// reply two lengths depending on who reads it.
///
/// Three C oddities are reproduced rather than corrected, because they are
/// what a user comparing the two IOCs sees: `DBR_STS_SHORT` prints a SIGNED
/// short through `%u` (`:161`) where `DBR_TIME_SHORT` prints the same value
/// through `%d` (`:262`); `DBR_CTRL_DOUBLE` prints its value with six decimals
/// (`:561`) where every other real family prints four; and `DBR_STRING` stops
/// at the first EMPTY element instead of at `count` (`:69`).
fn ca_dump_dbr(dbr: u16, snap: &crate::server::snapshot::Snapshot) -> String {
    use crate::types::{
        DBR_CHAR, DBR_CLASS_NAME, DBR_CTRL_CHAR, DBR_CTRL_DOUBLE, DBR_CTRL_ENUM, DBR_CTRL_FLOAT,
        DBR_CTRL_LONG, DBR_CTRL_SHORT, DBR_CTRL_STRING, DBR_DOUBLE, DBR_ENUM, DBR_FLOAT,
        DBR_GR_CHAR, DBR_GR_DOUBLE, DBR_GR_ENUM, DBR_GR_FLOAT, DBR_GR_LONG, DBR_GR_SHORT,
        DBR_GR_STRING, DBR_LONG, DBR_SHORT, DBR_STRING, DBR_STS_CHAR, DBR_STS_DOUBLE, DBR_STS_ENUM,
        DBR_STS_FLOAT, DBR_STS_LONG, DBR_STS_SHORT, DBR_STS_STRING, DBR_STSACK_STRING,
        DBR_TIME_CHAR, DBR_TIME_DOUBLE, DBR_TIME_ENUM, DBR_TIME_FLOAT, DBR_TIME_LONG,
        DBR_TIME_SHORT, DBR_TIME_STRING, dbr_type_to_text,
    };

    let value = &snap.value;
    let sts = format!("{:2} {:2}", snap.alarm.status, snap.alarm.severity);
    // `%.8s` over a buffer C already NUL-terminated at index 7.
    let units: String = snap
        .units()
        .map(|u| u.as_str_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .take(CA_MAX_UNITS_SIZE - 1)
        .collect();
    let precision = snap.precision().unwrap_or(0);
    // The reply's limit block, seeded exactly as `dbAccess.c` seeds it: zero
    // for the display and control pairs, which `get_graphics`/`get_control`
    // `memset` when the rset slot is NULL, and NaN for the alarm four, which
    // `get_alarm` leaves at its `struct dbr_alDouble ald` initialiser
    // (`:294`, `:318-323`). Reading an absent alarm slot as zero here made
    // `gft <bo> DBR_CTRL_DOUBLE` print four zeros where C prints four `nan`.
    // Shared with the CA wire encoder rather than re-seeded, because two
    // seeds for one C initialiser is how this diverged in the first place.
    let limits = crate::types::codec::get_limits(snap, 8);
    let int_limits = crate::types::codec::limits_as_integers(limits);
    let (lo_ctrl, hi_ctrl) = (limits[7], limits[6]);
    let ints = ca_dump_ints(value);
    let reals = ca_dump_reals(value);
    // C prefixes the element list with `\tValue: ` only for a scalar; a
    // vector's elements are introduced by the newline the element loop emits.
    let count = value.count() as usize;
    let head = if count == 1 { "\tValue: " } else { "" };
    let n_ints = ints.len();
    let n_reals = reals.len();

    let body = match dbr {
        DBR_STRING => {
            let texts = ca_dump_texts(value);
            let stop = texts
                .iter()
                .position(|s| s.is_empty())
                .unwrap_or(texts.len());
            ca_dump_elements(stop, 5, |i| texts[i].clone())
        }
        DBR_SHORT | DBR_ENUM | DBR_CHAR | DBR_LONG => {
            ca_dump_elements(n_ints, 10, |i| ints[i].to_string())
        }
        DBR_FLOAT | DBR_DOUBLE => ca_dump_elements(n_reals, 10, |i| ca_dump_f(reals[i], 6, 4)),
        // One C branch for all three (`test_event.cpp:129-139`): the status
        // pair and the FIRST string, whatever `count` is.
        DBR_STS_STRING | DBR_GR_STRING | DBR_CTRL_STRING => format!(
            "{sts}\tValue: {}",
            ca_dump_texts(value).first().cloned().unwrap_or_default()
        ),
        DBR_STS_ENUM | DBR_STS_CHAR | DBR_STS_LONG => format!(
            "{sts}{head}{}",
            ca_dump_elements(n_ints, 10, |i| ints[i].to_string())
        ),
        DBR_STS_SHORT => format!(
            "{sts}{head}{}",
            // `printf("%u", (short)v)`: the short promotes to int and is then
            // read as unsigned, so -1 prints as 4294967295.
            ca_dump_elements(n_ints, 10, |i| (ints[i] as i32 as u32).to_string())
        ),
        DBR_STS_FLOAT | DBR_STS_DOUBLE => format!(
            "{sts}{head}{}",
            ca_dump_elements(n_reals, 10, |i| ca_dump_f(reals[i], 6, 4))
        ),
        // The only branch whose `\tValue: ` is unconditional (`:229`).
        DBR_TIME_STRING => format!(
            "{sts}\tTimeStamp: {}\tValue: {}",
            ca_dump_time_stamp(snap.timestamp),
            ca_dump_texts(value).first().cloned().unwrap_or_default()
        ),
        DBR_TIME_ENUM | DBR_TIME_SHORT | DBR_TIME_CHAR | DBR_TIME_LONG => format!(
            "{sts}\tTimeStamp: {}{head}{}",
            ca_dump_time_stamp(snap.timestamp),
            ca_dump_elements(n_ints, 10, |i| ints[i].to_string())
        ),
        DBR_TIME_FLOAT | DBR_TIME_DOUBLE => format!(
            "{sts}\tTimeStamp: {}{head}{}",
            ca_dump_time_stamp(snap.timestamp),
            ca_dump_elements(n_reals, 10, |i| ca_dump_f(reals[i], 6, 4))
        ),
        // The two enum branches are the same code twice in C
        // (`test_event.cpp:373-400`): one value, then the label table, and no
        // limits in either.
        DBR_GR_ENUM | DBR_CTRL_ENUM => {
            let mut out = format!("{sts}\tValue: {}", ints.first().copied().unwrap_or(0));
            let strings = snap
                .enums
                .as_ref()
                .map(|e| e.strings.as_slice())
                .unwrap_or_default();
            if !strings.is_empty() {
                out.push_str(&format!("\n\t{:3}", strings.len()));
                for s in strings {
                    out.push_str(&format!(
                        "\n\t{}",
                        s.as_str_lossy()
                            .chars()
                            .take(CA_MAX_ENUM_STRING_SIZE)
                            .collect::<String>()
                    ));
                }
            }
            out
        }
        DBR_GR_SHORT | DBR_GR_CHAR | DBR_GR_LONG => format!(
            "{sts} {units}\n\t{}{head}{}",
            ca_dump_int_limits(&int_limits[..6], dbr - 21),
            ca_dump_elements(n_ints, 10, |i| ints[i].to_string())
        ),
        DBR_GR_FLOAT | DBR_GR_DOUBLE => format!(
            "{sts} {units} {precision:3}\n\t{}{head}{}",
            ca_dump_real_limits(&limits[..6]),
            ca_dump_elements(n_reals, 10, |i| ca_dump_f(reals[i], 6, 4))
        ),
        DBR_CTRL_SHORT | DBR_CTRL_CHAR | DBR_CTRL_LONG => {
            let family = dbr - 28;
            // `DBR_CTRL_CHAR` alone pads its elements to `%4d` (`:515`).
            let width = if dbr == DBR_CTRL_CHAR { 4 } else { 0 };
            format!(
                "{sts} {units}\n\t{} {} {}{head}{}",
                ca_dump_int_limits(&int_limits[..6], family),
                ca_dump_int_limit(int_limits[6], family),
                ca_dump_int_limit(int_limits[7], family),
                ca_dump_elements(n_ints, 10, |i| format!("{:width$}", ints[i]))
            )
        }
        DBR_CTRL_FLOAT | DBR_CTRL_DOUBLE => {
            // `DBR_CTRL_DOUBLE` alone prints its value with SIX decimals
            // (`:561`).
            let decimals = if dbr == DBR_CTRL_DOUBLE { 6 } else { 4 };
            format!(
                "{sts} {units} {precision:3}\n\t{} {} {}{head}{}",
                ca_dump_real_limits(&limits[..6]),
                ca_dump_f(hi_ctrl, 8, 3),
                ca_dump_f(lo_ctrl, 8, 3),
                ca_dump_elements(n_reals, 10, |i| ca_dump_f(reals[i], 6, decimals))
            )
        }
        DBR_STSACK_STRING => format!(
            "{sts} {:2} {:2} {}",
            snap.alarm.ackt.unwrap_or(0),
            snap.alarm.acks.unwrap_or(0),
            ca_dump_texts(value).first().cloned().unwrap_or_default()
        ),
        DBR_CLASS_NAME => ca_dump_texts(value).first().cloned().unwrap_or_default(),
        // C's `default` (`test_event.cpp:581-584`), unreachable from `gft` and
        // `pft` because `dbChannel_get` refuses `DBR_PUT_ACKT`/`DBR_PUT_ACKS`
        // first (`db_access.c:807-809`) and they print `Failed` instead.
        _ => "unsupported by ca_dbrDump()".to_string(),
    };
    format!("{}\t{body}", dbr_type_to_text(dbr))
}

/// The value family a `DBR_*` request converts the field into.
///
/// `None` for the two `DBR_PUT_*` codes `dbChannel_get` refuses outright
/// (`db_access.c:807-809`). `DBR_CLASS_NAME` is not a conversion of the field
/// at all — C answers it from `dbGetRecordTypeName` (`:784-806`) — so it is
/// the caller's job, not this table's.
fn ca_request_family(dbr: u16) -> Option<crate::types::DbFieldType> {
    use crate::types::DbFieldType as T;
    use crate::types::{DBR_PUT_ACKS, DBR_PUT_ACKT, DBR_STSACK_STRING, LAST_BUFFER_TYPE};

    if dbr == DBR_PUT_ACKT || dbr == DBR_PUT_ACKS || dbr > LAST_BUFFER_TYPE {
        return None;
    }
    if dbr == DBR_STSACK_STRING {
        return Some(T::String);
    }
    // The seven families repeat every seven codes, plain / STS / TIME / GR /
    // CTRL: STRING SHORT FLOAT ENUM CHAR LONG DOUBLE.
    Some(match dbr % 7 {
        0 => T::String,
        1 => T::Short,
        2 => T::Float,
        3 => T::Enum,
        4 => T::Char,
        5 => T::Long,
        _ => T::Double,
    })
}

/// What C's `dbChannel_create(pname)` hands `gft` and `pft`: the record, the
/// field it addressed, and everything the two commands read off it.
///
/// The metadata snapshot is taken once at create time and reused for every
/// request type, which is also what C does — `dbChannel_get` re-reads the
/// record per type, but under `dbScanLock`, so nothing between two types can
/// change it.
struct CaChannel {
    /// C `dbChannelRecord(chan)->name` — the RECORD's own name, so an alias
    /// prints the record it resolves to.
    record_name: String,
    /// C `dbGetRecordTypeName`, the answer `DBR_CLASS_NAME` carries.
    record_type: String,
    /// The channel's field, already defaulted to `VAL` and upper-cased —
    /// C's `chan->addr.pfldDes->name`. Only `pft` needs it, to name the put
    /// target and to re-read the field between two rungs of its ladder.
    field: String,
    /// C `dbChannelExportCAType(chan)`.
    export_type: u16,
    /// C `dbChannelElements(chan)`.
    elements: u32,
    /// Metadata plus the field's native value, the two halves every
    /// [`ca_dump_dbr`] branch reads.
    snap: crate::server::snapshot::Snapshot,
    /// C `dbChannelRecord(chan)`, printed as `Record Address`.
    address: usize,
    /// C `dbChannelFieldSize(chan)`, or `(none)` where the C number is a fact
    /// about a C struct this port does not have (see [`c_field_size`]).
    field_size: Option<u16>,
    /// The precision `getDoubleString`/`getFloatString` render a `DBR_STRING`
    /// row with — the record's own `get_precision`, or C's initialiser 6 when
    /// the record type has no such slot (`dbConvert.c:778-783`). This is NOT
    /// the `DBR_PRECISION` option [`ca_dump_dbr`] prints, which `getOptions`
    /// zeroes for a non-float field (`dbAccess.c:386-393`).
    string_precision: i16,
}

impl CaChannel {
    /// C `dbChannel_create` (`dbChannel.c:395-421`) as `gft`/`pft` use it.
    /// `None` is its NULL return, which both commands report as
    /// `Channel couldn't be created`.
    ///
    /// C reads `ACKT`/`ACKS` out of `dbCommon` on the same `DBRstatus` option
    /// every reply carries (`dbAccess.c:336-345`), so `DBR_STSACK_STRING` sees
    /// them. The port's `Snapshot` leaves them `None` on the GET path, so they
    /// are filled from the record here rather than defaulted to zero.
    fn create(ctx: &CommandContext, pname: &str) -> Option<Self> {
        let (base, field) = parse_pv_name(pname);
        let field = if field.is_empty() {
            "VAL".to_string()
        } else {
            field.to_ascii_uppercase()
        };
        let rec = ctx.db().get_record(base)?;
        let inst = rec.read();
        let fd = inst.field_desc(&field)?;
        let mut snap = inst.snapshot_for_field(&field)?;
        snap.alarm.ackt = Some(u16::from(inst.common.ackt));
        snap.alarm.acks = Some(inst.common.acks as u16);
        Some(Self {
            record_name: inst.name.clone(),
            record_type: inst.record.record_type().to_string(),
            field: field.clone(),
            // All three of C's remaining header numbers are properties of the
            // channel's FINAL field type, which `cvt_dbaddr` rewrites from
            // `FTVL` for an array field and leaves at the declaration
            // everywhere else. The value read back already carries that final
            // type, so taking all three off `snap.value` is one uniform rule
            // and needs no array special case: `dbChannelExportCAType` is
            // `dbDBRnewToDBRold[final type]`, `dbChannelFieldSize` is the
            // width of one element of it, and `dbChannelElements` is the
            // DBADDR capacity (`NELM`) rather than the current valid length
            // (`NORD`) that a GET would return.
            export_type: snap.value.db_field_type().ca_wire_type(),
            elements: inst
                .record
                .field_native_count(&field)
                .or_else(|| inst.record.get_field(&field).map(|v| v.count()))
                .unwrap_or(1),
            field_size: c_field_size(snap.value.db_field_type().dbf_code(), fd.size),
            string_precision: snap.precision().unwrap_or(6),
            address: Arc::as_ptr(&rec) as *const () as usize,
            snap,
        })
    }

    /// The six-line header `gft` and `pft` both open with (`db_test.c:58-63`,
    /// `:116-121`).
    ///
    /// C's `Field Address` is `dbChannelField(chan)`, the address of the
    /// member inside the record's C struct. This port reads a field through
    /// `Record::get_field`, so there is no such address and the line prints
    /// `(none)` — the same answer, for the same reason, that `dba` gives (see
    /// [`print_db_addr`]).
    fn header(&self) -> Vec<String> {
        vec![
            format!("   Record Name: {}", self.record_name),
            // C's format is `"Record Address: 0x%p\n"` (`db_test.c:59`) and
            // glibc's `%p` emits its own `0x`, so a C IOC really prints
            // `0x0x55f...` here. Not a typo to tidy: `dba` — whose C has no
            // literal prefix (`dbTest.c:800`) — prints ONE, and a user
            // diffing the two IOCs sees the doubled one only under `gft`
            // and `pft`.
            format!("Record Address: 0x{:#x}", self.address),
            format!("   Export Type: {}", self.export_type),
            " Field Address: (none)".to_string(),
            match self.field_size {
                Some(n) => format!("    Field Size: {n}"),
                None => "    Field Size: (none)".to_string(),
            },
            format!("   No Elements: {}", self.elements),
        ]
    }

    /// The reply `dbChannel_get(chan, dbr, pbuffer, no_elements, NULL)` would
    /// build: this channel's metadata plus the value converted into `dbr`'s
    /// family and padded with zeros out to `no_elements`
    /// (`db_access.c:121-138`). `None` is C's negative status, which both
    /// callers print as `Failed`.
    ///
    /// The pad is what makes an unprocessed waveform dump `NELM` zeros: the
    /// database returns `NORD` elements, C fills the rest of the buffer, and
    /// the caller prints the length it asked for. It belongs on this side of
    /// the call, as it does in C, so that every reader of a reply sees one
    /// length.
    fn get(&self, dbr: u16, no_elements: usize) -> Option<crate::server::snapshot::Snapshot> {
        use crate::types::{DBR_CLASS_NAME, DbFieldType};

        let mut reply = self.snap.clone();
        if dbr == DBR_CLASS_NAME {
            reply.value = EpicsValue::String(self.record_type.as_str().into());
            return Some(reply);
        }
        let family = ca_request_family(dbr)?;
        // A float or double field's `DBR_STRING` row is rendered by the
        // RECORD's precision, which a value-to-value conversion cannot see —
        // see [`dbr_string_row`], which `dbtgf` reaches through the same door.
        let precision_row = (family == DbFieldType::String)
            .then(|| dbr_string_row(&self.snap, self.string_precision))
            .flatten();
        let converted = match precision_row {
            Some(texts) => {
                EpicsValue::StringArray(texts.iter().map(|t| t.as_str().into()).collect())
            }
            None => self.snap.value.get_convert(family).ok()?,
        };
        reply.value = ca_zero_fill(converted, no_elements);
        Some(reply)
    }

    /// The channel addressed as a PV name, always `record.FIELD` — the form
    /// the put and re-read below hand back to the database. Built from the
    /// RECORD's own name, so an alias and its target share one channel.
    fn pv_name(&self) -> String {
        format!("{}.{}", self.record_name, self.field)
    }

    /// C `dbChannel_put(chan, ..., 1)` (`dbChannel.h:295`), which is `dbPut`:
    /// it writes the field and runs its `special`, and it does NOT process the
    /// record — that is `dbPutField`, and `pft` does not call it. `Err` is C's
    /// negative status, which `pft` prints as its `failed` fragment.
    fn put(&self, ctx: &CommandContext, value: EpicsValue) -> bool {
        ctx.block_on(async { ctx.db().put_pv(&self.pv_name(), value).await })
            .is_ok()
    }

    /// Re-read the field, so the next [`Self::get`] answers what C's next
    /// `dbChannel_get` would after the put in between. C holds a pointer and
    /// needs no such step; this port holds a snapshot, so the snapshot is the
    /// thing that has to be refreshed — through the same constructor, so
    /// there is one place that decides what a channel's reply is made of.
    fn refresh(&mut self, ctx: &CommandContext) {
        if let Some(fresh) = Self::create(ctx, &self.pv_name()) {
            self.snap = fresh.snap;
            self.string_precision = fresh.string_precision;
        }
    }
}

/// `gft "pv name"` — Get Field Test.
///
/// C `gft` (`db_test.c:35-83`): print the channel header, then read the field
/// once per `DBR_*` request type — all 39 of them — and dump each answer
/// through [`ca_dump_dbr`].
///
/// Unlike `dbgf`/`dbtgf` there is no `only works after iocInit` gate: C's
/// `dbChannel_create` never looks at `precord->lset`.
fn cmd_gft() -> CommandDef {
    CommandDef::new(
        "gft",
        vec![ArgDesc {
            // C `gftArg0` (`dbIocRegister.c:341`).
            name: "record name",
            arg_type: ArgType::Record,
        }],
        "gft record name - Get Field Test",
        |args: &[ArgValue], ctx: &CommandContext| {
            use crate::types::{
                DBR_CTRL_STRING, DBR_GR_STRING, DBR_STRING, DBR_STS_STRING, DBR_TIME_STRING,
                LAST_BUFFER_TYPE, dbr_type_to_text,
            };

            // C `db_test.c:44-47`, returning -1.
            let name = match args.first() {
                Some(ArgValue::String(s)) if !s.is_empty() => s,
                _ => {
                    ctx.println("Usage: gft \"pv_name\"");
                    return Ok(CommandOutcome::Failed);
                }
            };
            // C `db_test.c:48-52`, returning 1 — its own message, not the
            // `dbNameToAddr` one `dba`/`dbgf` print.
            let Some(chan) = CaChannel::create(ctx, name) else {
                ctx.println("Channel couldn't be created");
                return Ok(CommandOutcome::Failed);
            };
            for line in chan.header() {
                ctx.println(&line);
            }

            let count = chan.elements.min(GFT_MAX_ELEMS) as usize;
            for dbr in 0..=LAST_BUFFER_TYPE {
                // C `db_test.c:69-74`: a channel that exports as `DBR_STRING`
                // asks for the five STRING request types and nothing else.
                if chan.export_type == DBR_STRING
                    && !matches!(
                        dbr,
                        DBR_STRING
                            | DBR_STS_STRING
                            | DBR_TIME_STRING
                            | DBR_GR_STRING
                            | DBR_CTRL_STRING
                    )
                {
                    continue;
                }
                match chan.get(dbr, count) {
                    None => ctx.println(&format!("\t{} Failed", dbr_type_to_text(dbr))),
                    Some(reply) => ctx.println(&ca_dump_dbr(dbr, &reply)),
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// C `sscanf(s, "%hd", &v)` and `sscanf(s, "%ld", &v)` as `pft` uses them
/// (`db_test.c:129`, `:137`, `:163`, `:171`).
///
/// These are LENIENT where `epicsScanFloat` is strict: `sscanf` stops at the
/// first byte its conversion cannot use and reports a match anyway, so `12.7`
/// scans as the short 12 and only a string with no leading integer at all —
/// `abc` — leaves the rung out. That asymmetry is the whole reason `pft REC
/// 12.7` prints seven rows and `pft REC abc` prints one.
///
/// The engine is this workspace's C `sscanf`
/// ([`crate::calc::engine::scanf::sscanf`]), including the length-modifier
/// narrowing table, so `%hd` really wraps into a `short` here. Its one output
/// object is a `f64`, so a `%ld` result above 2^53 loses C's low bits before
/// the caller truncates it to `dbr_long_t`.
fn c_scan_int(s: &str, fmt: &[u8]) -> Option<i64> {
    match crate::calc::engine::scanf::sscanf(s.as_bytes(), fmt) {
        Ok(crate::calc::engine::value::StackValue::Double(v)) => Some(v as i64),
        _ => None,
    }
}

/// Print a C printf STREAM through the shell's line-at-a-time output.
///
/// `pft` is the one command in `db_test.c` whose output is not a sequence of
/// whole lines: a failed put prints `"\n\t failed "` with no terminator and
/// the dump that follows continues that same line. So the stream is built
/// whole and cut here, rather than each fragment guessing where a line ends.
///
/// The port differs by one newline in one case: when the stream does not end
/// in `'\n'` C leaves the cursor mid-line for the next shell prompt to land
/// on, and this terminates it — there is no unterminated output primitive
/// (see `CommandContext::println`).
fn print_c_stream(ctx: &CommandContext, stream: &str) {
    if stream.is_empty() {
        return;
    }
    for line in stream.strip_suffix('\n').unwrap_or(stream).split('\n') {
        ctx.println(line);
    }
}

/// `pft "pv name", "value"` — Put Field Test.
///
/// C `pft` (`db_test.c:85-186`): the same six-line channel header `gft`
/// prints, then a ladder of put-then-get pairs, each dumped through
/// [`ca_dump_dbr`] at count 1.
///
/// Three things about the ladder are not guessable from the DBR table:
///
/// * its order is STRING, SHORT, LONG, FLOAT, DOUBLE, CHAR, ENUM — `LONG`
///   before `FLOAT`, which the DBR numbering has the other way round;
/// * every rung after the first is guarded by whether the argument scans as
///   that C type, and the integer rungs use `sscanf` while the two float
///   rungs use `epicsScanFloat`, so `12.7` takes all seven rungs and `12.7`
///   with a trailing letter takes five (see [`c_scan_int`]);
/// * a channel exporting as `DBR_STRING` or `DBR_ENUM` stops after the first
///   rung and does NOT print C's closing blank line, which only the full
///   ladder reaches.
///
/// Every rung PUTS. `pft` is a destructive command: the field is left holding
/// whatever the last rung wrote, which for a numeric field is the argument
/// re-read as an enum index.
fn cmd_pft() -> CommandDef {
    CommandDef::new(
        "pft",
        vec![
            ArgDesc {
                // C `pftArg0`/`pftArg1` (`dbIocRegister.c:352-353`).
                name: "record name",
                arg_type: ArgType::Record,
            },
            ArgDesc {
                name: "value",
                arg_type: ArgType::String,
            },
        ],
        "pft record name value - Put Field Test",
        |args: &[ArgValue], ctx: &CommandContext| {
            use crate::types::c_parse::{NumericField, parse_base10_units_null};
            use crate::types::{
                DBR_CHAR, DBR_DOUBLE, DBR_ENUM, DBR_FLOAT, DBR_LONG, DBR_SHORT, DBR_STRING,
            };

            // C `db_test.c:100-103`, returning -1.
            let (Some(ArgValue::String(name)), Some(ArgValue::String(value))) =
                (args.first(), args.get(1))
            else {
                ctx.println("Usage: pft \"pv_name\", \"value\"");
                return Ok(CommandOutcome::Failed);
            };
            if name.is_empty() {
                ctx.println("Usage: pft \"pv_name\", \"value\"");
                return Ok(CommandOutcome::Failed);
            }
            // C `db_test.c:104-108`, returning 1.
            let Some(mut chan) = CaChannel::create(ctx, name) else {
                ctx.println("Channel couldn't be created");
                return Ok(CommandOutcome::Failed);
            };
            for line in chan.header() {
                ctx.println(&line);
            }

            // One rung: put, then read back and dump — and note C reads back
            // even when the put failed, which is why `pft REC abc` still
            // prints the value the field already held.
            let mut out = String::new();
            let rung = |out: &mut String,
                        chan: &mut CaChannel,
                        dbr: u16,
                        put: EpicsValue,
                        put_failed: &str,
                        get_failed: &str| {
                if !chan.put(ctx, put) {
                    out.push_str(put_failed);
                }
                chan.refresh(ctx);
                // C `pft` asks for one element on every rung
                // (`db_test.c:122-193`).
                match chan.get(dbr, 1) {
                    None => out.push_str(get_failed),
                    Some(reply) => {
                        out.push_str(&ca_dump_dbr(dbr, &reply));
                        out.push('\n');
                    }
                }
            };

            rung(
                &mut out,
                &mut chan,
                DBR_STRING,
                EpicsValue::String(value.as_str().into()),
                "\n\t failed ",
                // The one rung whose GET failure has no leading space and no
                // type name (`db_test.c:124`).
                "\n\tfailed",
            );

            // C `db_test.c:127`: `type <= DBF_STRING || type == DBF_ENUM`,
            // testing the CA export type against the dbStatic codes, which
            // agree on 0 and 3. No closing newline on this path.
            if chan.export_type == DBR_STRING || chan.export_type == DBR_ENUM {
                print_c_stream(ctx, &out);
                return Ok(CommandOutcome::Continue);
            }

            if let Some(v) = c_scan_int(value, b"%hd") {
                rung(
                    &mut out,
                    &mut chan,
                    DBR_SHORT,
                    EpicsValue::Short(v as i16),
                    "\n\t SHORT failed ",
                    "\n\t SHORT GET failed",
                );
            }
            if let Some(v) = c_scan_int(value, b"%ld") {
                // C puts `&longvalue` as a `dbr_long_t`, i.e. reads the low 32
                // bits of a 64-bit `long` on this ABI.
                rung(
                    &mut out,
                    &mut chan,
                    DBR_LONG,
                    EpicsValue::Long(v as i32),
                    "\n\t LONG failed ",
                    "\n\t LONG GET failed",
                );
            }
            // `epicsScanFloat` — STRICT, so a trailing tail refuses the rung
            // where the two `sscanf` rungs above accepted it.
            let scanned_float = match parse_base10_units_null(NumericField::Float, value) {
                Some(EpicsValue::Float(f)) => Some(f),
                _ => None,
            };
            if let Some(f) = scanned_float {
                rung(
                    &mut out,
                    &mut chan,
                    DBR_FLOAT,
                    EpicsValue::Float(f),
                    "\n\t FLOAT failed ",
                    "\n\t FLOAT GET failed",
                );
                // C `db_test.c:155`: `doublevalue = floatvalue`, so the DOUBLE
                // rung carries the float's rounding, not the argument's.
                rung(
                    &mut out,
                    &mut chan,
                    DBR_DOUBLE,
                    EpicsValue::Double(f as f64),
                    "\n\t DOUBLE failed ",
                    "\n\t DOUBLE GET failed",
                );
            }
            if let Some(v) = c_scan_int(value, b"%hd") {
                // `charvalue = (unsigned char) shortvalue`, and `dbr_char_t`
                // is `epicsUInt8` — the wire's DBR_CHAR is unsigned even
                // though the DBF_CHAR it shares a name with is not (see
                // `DbFieldType::ca_wire_type`).
                rung(
                    &mut out,
                    &mut chan,
                    DBR_CHAR,
                    EpicsValue::UChar(v as i16 as u8),
                    "\n\t CHAR failed ",
                    "\n\t CHAR GET failed",
                );
                // C passes `&shortvalue` for a `dbr_enum_t`, so a negative
                // argument arrives as its unsigned 16-bit reading.
                rung(
                    &mut out,
                    &mut chan,
                    DBR_ENUM,
                    EpicsValue::Enum(v as i16 as u16),
                    "\n\t ENUM failed ",
                    "\n\t ENUM GET failed",
                );
            }
            // C `db_test.c:183`.
            out.push('\n');
            print_c_stream(ctx, &out);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbtr "pv name"` — process the record, then print it at level 3.
///
/// C `dbtr` (`dbTest.c:464-498`).
fn cmd_dbtr() -> CommandDef {
    CommandDef::new(
        "dbtr",
        vec![ArgDesc {
            // C `dbtrArg0` (`dbIocRegister.c:294`).
            name: "record name",
            arg_type: ArgType::Record,
        }],
        "dbtr record name - Process a record then print its fields",
        |args: &[ArgValue], ctx: &CommandContext| {
            let name = match args.first() {
                Some(ArgValue::String(s)) if !s.is_empty() => s,
                _ => {
                    ctx.println("Usage: dbtr \"pv name\"");
                    return Ok(CommandOutcome::Failed);
                }
            };
            let (base, _) = parse_pv_name(name);
            let Some(rec) = ctx.db().get_record(base) else {
                print_pv_not_found(ctx, name);
                return Ok(CommandOutcome::Failed);
            };
            if !dbt_requires_ioc_init(ctx, "dbtr") {
                return Ok(CommandOutcome::Failed);
            }
            // C `dbTest.c:482-484` — an active record is left alone.
            if rec.read().is_processing() {
                ctx.println("record active");
                return Ok(CommandOutcome::Failed);
            }

            let processed = ctx.block_on(ctx.db().process_record(base));
            if let Err(e) = processed {
                // C `recGblRecordError(status, precord, "dbtr(dbProcess)")`
                // (`dbTest.c:491-492`) renders the numeric status through
                // `errSymLookup`; the port's process path reports a
                // `CaError` and has no status number to look up, so the
                // error's own text takes that slot.
                crate::server::recgbl::rec_gbl_record_error(
                    &e.to_string(),
                    base,
                    "dbtr(dbProcess)",
                );
            }
            dbpr_report(ctx, name, 3);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// The `dtype` word C's `lnkConst` gives a JSON value, decided the way
/// yajl decides which jlif callback to run: an integer token reaches
/// `lnkConst_integer` (`si64`/`ai64`), a real reaches `lnkConst_double`
/// (`sf64`/`af64`), a quoted token reaches `lnkConst_string`
/// (`sc40`/`ac40`) — `lnkConst.c:143-232`. Everything else is C's
/// `type_names[0]`, `"bug"`.
fn const_value_dtype(token: &str) -> &'static str {
    let t = token.trim();
    if t.starts_with('"') || t.starts_with('\'') {
        "string"
    } else if t.starts_with('-')
        || t.starts_with('+')
        || t.starts_with(|c: char| c.is_ascii_digit())
    {
        if t.contains(['.', 'e', 'E']) {
            "double"
        } else {
            "integer"
        }
    } else {
        "bug"
    }
}

/// One `{const: …}` value rendered as C's `lnkConst_report` renders it
/// (`lnkConst.c:286-347`), at `indent` spaces.
///
/// The scalar arms print `'const': <dtype> <value>`, an integer with
/// `%lld`, a double with `%g` and a string requoted; the array arms
/// print `'const': array of <n> <dtype>s` and, only from level 2, the
/// bracketed element list at `indent + 2`.
fn const_link_report_lines(value: &str, level: i32, indent: usize) -> Vec<String> {
    use crate::calc::engine::cvt::fmt_g;

    let pad = " ".repeat(indent);
    let render = |token: &str| -> String {
        let t = token.trim();
        match const_value_dtype(t) {
            // `%g`, not the verbatim token: C reports the parsed double.
            "double" => t
                .parse::<f64>()
                .map(|v| fmt_g(v, 6, false, false))
                .unwrap_or_else(|_| t.to_string()),
            // Requoted, because C prints `"%s"` around the decoded string.
            "string" => format!("\"{}\"", t.trim_matches(['"', '\''])),
            _ => t.to_string(),
        }
    };

    let v = value.trim();
    let Some(inner) = v.strip_prefix('[').and_then(|r| r.strip_suffix(']')) else {
        let dtype = const_value_dtype(v);
        if dtype == "bug" {
            // C's `default:` arm — it prints the numeric type tag it could
            // not name. The port has no such tag, so the token stands in.
            return vec![format!("{pad}'const': bug -- {v}")];
        }
        return vec![format!("{pad}'const': {dtype} {}", render(v))];
    };
    let elements: Vec<&str> = inner
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .collect();
    let dtype = elements.first().map_or("bug", |e| const_value_dtype(e));
    let plural = if elements.len() > 1 { "s" } else { "" };
    let head = format!("{pad}'const': array of {} {dtype}{plural}", elements.len());
    if level < 2 {
        return vec![head];
    }
    let body: Vec<String> = elements.iter().map(|e| render(e)).collect();
    vec![
        head,
        format!("{}[{}]", " ".repeat(indent + 2), body.join(", ")),
    ]
}

/// One JSON link field's report — C `dbJLinkReport` (`dbJLink.c:468-471`),
/// which calls the link type's own `report` **only when its jlif has
/// one** and otherwise prints nothing under the `Link field` header.
///
/// `const` is the one type this port reports. C also reports `calc`
/// (`lnkCalc_report`, `lnkCalc.c:412-460`) and pvxs reports `pva`
/// (`pva_report`, `pvxs/ioc/pvalink_jlif.cpp:228-283`), and both print LIVE link
/// state — `calc`'s current value, precision, units, alarm and per-input
/// arguments; `pva`'s open channel and its in-flight put. The port's
/// `ParsedLink::Calc` / `PvaJsonLink` are parse results with no handle on
/// the evaluated or opened link, so those two report as a jlif with no
/// `report` member does: header only. `ca` is a port-only link type with
/// no C jlif to match at all.
fn jlink_report_lines(raw: &str, level: i32, indent: usize) -> Vec<String> {
    let Ok(normalized) = crate::json5::relaxed_to_strict(raw.trim()) else {
        return Vec::new();
    };
    match crate::server::record::json_const_value(&normalized) {
        Some(value) => const_link_report_lines(value, level, indent),
        None => Vec::new(),
    }
}

/// `dbior [driver name] [interest level]` — Driver Report.
///
/// C `dbior` (`dbTest.c:709-771`), registered at `dbIocRegister.c:616`. Walks
/// the driver table and calls each driver's `report(level)`, then does the same
/// over every record type's device support.
///
/// This port has the driver half. Two differences, both structural rather than
/// elective:
///
/// * C's `No driver entry table is present for %s` (`:733-736`) cannot happen
///   here. It is what a `.dbd` `driver(drvXxx)` declaration prints when nothing
///   registered `drvXxx`'s entry table, and this port has no run-time `.dbd`
///   load to declare a name without one — see
///   [`crate::server::driver_support`].
/// * The DEVICE-support half (`:746-768`) is absent. C walks each record type's
///   `devList` and calls `pdset->report(level)` once per (record type, device
///   support name), because a `dset` is one static table shared by every record
///   of that DTYP. The port has no such object: device support is a
///   `DeviceSupportFactory` that mints a fresh instance PER RECORD
///   (`ioc_builder.rs:119`), so there is nothing to call `report` on once per
///   record type, and calling it per record would be a different report.
///
/// The `No database loaded` guard (`:715-718`) is likewise unreachable: a
/// `CommandContext` always carries a database.
fn cmd_dbior() -> CommandDef {
    CommandDef::new(
        "dbior",
        vec![
            ArgDesc {
                // C `dbiorArg0`/`Arg1` (`dbIocRegister.c:323-324`). Arg0 is a
                // plain `iocshArgString`, NOT a record name: it names a driver.
                name: "driver name",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "interest level",
                arg_type: ArgType::Int,
            },
        ],
        "dbior driver name, interest level - Driver Report.",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C folds a missing, empty or `*` name to "every driver"
            // (`dbTest.c:720-721`).
            let wanted = match &args[0] {
                ArgValue::String(s) if !s.is_empty() && s != "*" => Some(s.clone()),
                _ => None,
            };
            let level = match &args[1] {
                ArgValue::Int(n) => *n as i32,
                _ => 0,
            };

            for (name, drvet) in crate::server::driver_support::driver_supports() {
                if wanted.as_ref().is_some_and(|want| *want != name) {
                    continue;
                }
                match drvet.report(level) {
                    // C `pdrvet->report == NULL` (`dbTest.c:738-739`).
                    None => ctx.println(&format!("Driver: {name} No report available")),
                    Some(text) => {
                        ctx.println(&format!("Driver: {name}"));
                        // C's header is a separate `printf` from whatever
                        // `report()` writes, so a report that printed nothing
                        // leaves the header alone on its line.
                        if !text.is_empty() {
                            for line in text.trim_end_matches('\n').split('\n') {
                                ctx.println(line);
                            }
                        }
                    }
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbjlr [record name] [level]` — report every record's JSON links.
///
/// C `dbjlr` (`dbJLink.c:493-545`), registered at `dbIocRegister.c:598`.
/// An empty name and the literal `*` are both the all-records sentinel,
/// exactly as in `dbcar`; a named record that does not exist prints the
/// header and nothing else, because C's walk simply never matches it.
///
/// C lists a link field when `plink->type == JSON_LINK` and
/// `dbLinkIsDefined(plink)`. The first test is `dbParseLink`'s
/// (`dbStaticLib.c:2280`): brace-delimited link text IS a JSON link, with
/// no escape hatch — so the port asks the same question of the field's
/// verbatim text. The second drops `{}`, whose jlink is NULL; the port's
/// `record_link_fields` already drops it as `ParsedLink::None`.
fn cmd_dbjlr() -> CommandDef {
    CommandDef::new(
        "dbjlr",
        vec![
            ArgDesc {
                // C `dbjlrArg0`/`Arg1` (`dbIocRegister.c:159-160`).
                name: "record name",
                arg_type: ArgType::Record,
            },
            ArgDesc {
                name: "level",
                arg_type: ArgType::Int,
            },
        ],
        "dbjlr record name, level - Report JSON links",
        |args: &[ArgValue], ctx: &CommandContext| {
            let target = match &args[0] {
                ArgValue::String(s) if !s.is_empty() && s != "*" => Some(s.clone()),
                _ => None,
            };
            let level = match &args[1] {
                ArgValue::Int(n) => *n as i32,
                _ => 0,
            };
            match &target {
                Some(name) => ctx.println(&format!("JSON links in record '{name}'")),
                None => ctx.println("JSON links in all records"),
            }
            ctx.println("");

            for name in record_names_type_major(ctx) {
                if target.as_ref().is_some_and(|want| *want != name) {
                    continue;
                }
                let Some(rec) = ctx.db().get_record(&name) else {
                    continue;
                };
                let record_type = rec.read().record.record_type();
                ctx.println(&format!("  {record_type} record '{name}':"));
                for (field, raw, _) in ctx.db().record_link_fields(&name) {
                    let text = raw.trim();
                    if !(text.starts_with('{') && text.ends_with('}')) {
                        continue;
                    }
                    ctx.println(&format!("    Link field '{field}':"));
                    for line in jlink_report_lines(text, level, 6) {
                        ctx.println(&line);
                    }
                }
                if target.is_some() {
                    break;
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// One monitor's line in `dbel`'s per-subscription block
/// (`dbEvent.c:184-243`), which C emits only for `level > 0`.
///
/// Returns the line(s) as C writes them: normally one, but C's
/// `duplicate count` conversion carries its own `\n` INSIDE the format
/// string (`dbEvent.c:231`) and is still followed by the block's closing
/// `printf("\n")`, so that case is genuinely two lines — the second empty.
///
/// Two of C's tokens are omitted rather than invented, and the omission is
/// the whole reason this is not byte-identical at levels 2 and 4:
///
/// * `thread=%p` (`dbEvent.c:206-215`) is `pevent->ev_que->evUser->taskid`,
///   the OS id of the thread C's `event_task` runs the callbacks on. The
///   port has no such thread: [`EventReader`] is polled by whatever task
///   owns the channel, so there is no id to print.
/// * The whole `level > 3` block (`dbEvent.c:235-240`) prints the addresses
///   of `evSubscrip`, `event_que` and `event_user`. Those are C debugging
///   handles; the port's counterparts are `Arc` interiors whose addresses
///   say nothing a reader could act on.
///
/// [`EventReader`]: crate::server::event_queue::EventReader
fn dbel_subscription_lines(
    field: &str,
    mask: u16,
    level: i32,
    report: &crate::server::event_queue::QueReport,
) -> Vec<String> {
    use crate::server::recgbl::EventMask;

    // C `printf("%4.4s", pdbFldDes->name)` — minimum width 4, truncated
    // to 4, which is why a 3-letter field arrives with a leading space.
    let mut line = format!("{field:>4.4}");

    let select = EventMask::from_bits(mask);
    line.push_str(" { ");
    for (bit, name) in [
        (EventMask::VALUE, "VALUE "),
        (EventMask::LOG, "LOG "),
        (EventMask::ALARM, "ALARM "),
        (EventMask::PROPERTY, "PROPERTY "),
    ] {
        if select.contains(bit) {
            line.push_str(name);
        }
    }
    line.push('}');

    if report.npend != 0 {
        line.push_str(&format!(" undelivered={}", report.npend));
    }

    if level > 1 {
        // C `dbEvent.c:206-215`, minus the `thread=%p` token.
        if report.ring_space == 0 {
            line.push_str(", queue full");
        } else if report.ring_space == report.ring_size {
            line.push_str(", queue empty");
        } else {
            line.push_str(&format!(", unused entries={}", report.ring_space));
        }
    }

    let mut lines = vec![];
    if level > 2 {
        if report.nreplace != 0 {
            line.push_str(&format!(", discarded by replacement={}", report.nreplace));
        }
        if report.latest_only {
            line.push_str(", queueing disabled");
        }
        if report.n_duplicates != 0 {
            line.push_str(&format!(", duplicate count ={}", report.n_duplicates));
            lines.push(std::mem::take(&mut line));
        }
    }
    lines.push(line);
    lines
}

/// `dbel record name, level` — C `dbel` (`dbEvent.c:154-251`), registered at
/// `dbIocRegister.c:597`.
///
/// C reads `precord->mlis`, the record's list of `evSubscrip`s, which holds
/// EVERY event subscription on the record whatever created it — a CA or PVA
/// monitor, and equally a local `CP`/`CPP` input link, since `dbCa`/`dbLink`
/// register those through the same `db_add_event`. The port's monitor list
/// (`RecordInstance::subscribers`) holds only the first class: a local CP/CPP
/// link is registered instead in `PvDatabase::cp_links`
/// (`database/links.rs:2886-2909`), an index keyed by source record NAME that
/// carries neither the linked FIELD nor a select mask nor a queue, so those
/// links have nothing this report could print and are not counted here. On the
/// `R:CALC.INPA = "R:SRC CP"` database this command was measured against, C
/// reports one subscription on `R:SRC` and the port reports none.
///
/// C orders `mlis` by attach time (`ellAdd` appends); the port's map is keyed
/// by field, so the flattened list is sorted by `sid`, which is allocated
/// increasing and is therefore the same order.
fn cmd_dbel() -> CommandDef {
    CommandDef::new(
        "dbel",
        vec![
            ArgDesc {
                // C `dbelArg0`/`Arg1` (`dbIocRegister.c:171-172`).
                name: "record name",
                arg_type: ArgType::Record,
            },
            ArgDesc {
                name: "level",
                arg_type: ArgType::Int,
            },
        ],
        "dbel record name, level - Database event list.\n\
         Show information on dbEvent subscriptions.\n\
         Higher level shows more information (0 - 4)\n\
         Example: dbel aitest 2",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `if ( ! pname ) return DB_EVENT_OK;` (`dbEvent.c:161`) — a
            // NULL name is silent success, NOT a usage line. `ArgValue::Missing`
            // is exactly iocsh's NULL `sval`; an explicit `dbel ""` is a
            // non-NULL empty string and falls through to `dbNameToAddr`.
            let ArgValue::String(name) = &args[0] else {
                return Ok(CommandOutcome::Continue);
            };
            let level = match &args[1] {
                ArgValue::Int(n) => *n as i32,
                _ => 0,
            };

            let record = name.split('.').next().unwrap_or(name);
            let Some(rec) = ctx.db().get_record(record) else {
                // C `errMessage(status, " dbNameToAddr failed")`
                // (`dbEvent.c:164`), which `errlog.c:503-508` renders on the
                // errlog stream — stdout stays empty. Measured on `softIoc`
                // R7.0.10-146 as `Record Not Found filename="../db/dbEvent.c"
                // line number=164   dbNameToAddr failed`; the three spaces are
                // one from `line number=%d ` and two from `errMessage`'s
                // `" %s\n"` meeting the leading space C put in the literal.
                // The file and line are ours, for the reason
                // `access_commands.rs` gives at its own `errMessage` site.
                crate::runtime::log::errlog_printf(&format!(
                    "Record Not Found filename=\"{}\" line number={}   dbNameToAddr failed\n",
                    file!(),
                    line!()
                ));
                return Ok(CommandOutcome::Failed);
            };

            let inst = rec.read();
            let mut subs: Vec<(&String, &crate::server::pv::Subscriber)> = inst
                .subscribers
                .iter()
                .flat_map(|(field, bucket)| bucket.iter().map(move |sub| (field, sub)))
                .collect();
            subs.sort_by_key(|(_, sub)| sub.sid);

            if subs.is_empty() {
                // C `dbEvent.c:173` prints the name AS GIVEN, not the record
                // it resolved to.
                ctx.println(&format!(
                    "\"{name}\": No PV event subscriptions ( monitors )."
                ));
                return Ok(CommandOutcome::Continue);
            }

            ctx.println(&format!(
                "{} PV Event Subscriptions ( monitors ).",
                subs.len()
            ));
            if level > 0 {
                for (field, sub) in subs {
                    let report = sub.sink.report();
                    for line in dbel_subscription_lines(field, sub.mask, level, &report) {
                        ctx.println(&line);
                    }
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// The lines C's `dbtpn` callbacks print for one run, in order
/// (`dbNotify.c:509-574`) — kept out of the shell so the measured C session
/// can be asserted directly rather than through a spawned task's stdout.
///
/// C splits the work over three callbacks hung on one `processNotify`
/// (`putCallback`, `getCallback`, `doneCallback`); the port reaches the same
/// two states through the database's own put-notify entries, so the mapping
/// is by state, not by callback:
///
/// * `pvalue` present — C `putProcessRequest`: `putCallback` writes the text
///   as `DBR_STRING`, the record processes, `doneCallback` reports. That is
///   [`PvDatabase::put_record_field_from_ca`], whose completion fires when the
///   whole FLNK/OUT chain settles, which is C's `dbNotifyCompletion`.
/// * `pvalue` absent — C `processGetRequest`: process, then `getCallback`
///   reads the channel back as `DBR_STRING`. That is
///   [`PvDatabase::process_record_with_notify`] followed by the same
///   `DBR_STRING` rendering `dbtgf` uses.
///
/// C's failure arms print `ppn->status` as an integer from `notifyStatus`
/// (`dbNotify.h:47-53`: `notifyOK` 0, `notifyCanceled` 1, `notifyError` 2,
/// `notifyPutDisabled` 3). The port's put and process paths report a
/// `CaError`, not that enum, and only `notifyError` is reachable from what
/// they can fail with — a refused put or a failed read — so `2` is the only
/// number this ever prints. `notifyCanceled` has no port path at all: C
/// reaches it from `dbNotifyCancel` racing the callback, and nothing here
/// cancels a notify from outside.
///
/// [`PvDatabase::put_record_field_from_ca`]: crate::server::database::PvDatabase::put_record_field_from_ca
/// [`PvDatabase::process_record_with_notify`]: crate::server::database::PvDatabase::process_record_with_notify
async fn dbtpn_lines(
    db: &std::sync::Arc<crate::server::database::PvDatabase>,
    name: &str,
    value: Option<String>,
) -> Vec<String> {
    let (base, field) = parse_pv_name(name);
    let field = field.to_ascii_uppercase();
    let mut lines = Vec::new();

    let Some(text) = value else {
        // C `processGetRequest` (`dbNotify.c:573`): process first, read after.
        let processed = db.process_record_with_notify(base).await;
        let ok = match processed {
            Ok(completion) => match completion.into_handle() {
                Some(rx) => rx.await.is_ok(),
                None => true,
            },
            Err(_) => false,
        };
        if !ok {
            lines.push(format!("{base} dbtpnCallback processNotify.status 2"));
            return lines;
        }
        // C `getCallback` -> `dbChannelGet(ppn->chan, DBR_STRING, ...)`
        // (`dbNotify.c:550-551`), printed unquoted.
        let rendered = db.get_record(base).and_then(|rec| {
            let inst = rec.read();
            let snap = inst.snapshot_for_field(&field)?;
            let precision = snap.precision().unwrap_or(6);
            Some(dbr_string_text(&snap.value, precision))
        });
        match rendered {
            Some(text) => lines.push(format!("dbtpn:getCallback value {text}")),
            None => {
                lines.push("dbtpn:getCallback error".to_string());
                lines.push(format!("{base} dbtpnCallback processNotify.status 2"));
                return lines;
            }
        }
        lines.push(format!("dbtpnCallback: success record={base}"));
        return lines;
    };

    // C `putProcessRequest` (`dbNotify.c:607`).
    let put = db
        .put_record_field_from_ca(base, &field, EpicsValue::String(text.as_str().into()))
        .await;
    match put {
        Ok(completion) => {
            let settled = match completion.into_handle() {
                Some(rx) => rx.await.is_ok(),
                None => true,
            };
            if settled {
                lines.push(format!("dbtpnCallback: success record={base}"));
            } else {
                lines.push(format!("{base} dbtpnCallback processNotify.status 2"));
            }
        }
        Err(_) => lines.push(format!("{base} dbtpnCallback processNotify.status 2")),
    }
    lines
}

/// One value as C `dbChannelGet(.., DBR_STRING, ..)` renders it — the
/// unquoted form of what `dbtgf`'s `DBF_STRING` row prints.
fn dbr_string_text(value: &EpicsValue, precision: i16) -> String {
    use crate::calc::engine::cvt::cvt_double_to_string;

    match value {
        EpicsValue::Double(v) => cvt_double_to_string(*v, precision.max(0) as u16),
        EpicsValue::Float(v) => cvt_double_to_string(*v as f64, precision.max(0) as u16),
        EpicsValue::String(s) => s.to_string(),
        other => match other.get_convert(crate::types::DbFieldType::String) {
            Ok(EpicsValue::String(s)) => s.to_string(),
            _ => String::new(),
        },
    }
}

/// `dbtpn "name", "value"` — C `dbtpn` (`dbNotify.c:590-625`), registered at
/// `dbIocRegister.c:620`.
///
/// C hands the whole run to a `dbtpn` thread it creates at
/// `epicsThreadPriorityHigh` and returns 0 immediately, so the callback lines
/// arrive on the process's stdout after the shell has already printed its next
/// prompt — measured on `softIoc` R7.0.10-146. The port spawns the same work
/// on the IOC's executor for the same reason: a `dbtpn` onto a record with
/// async device support must not hold the shell for the whole motion. That is
/// also why the callback lines go to `println!` and not to `ctx.println` —
/// C's `printf` writes past iocsh's `>` redirection too, and the task outlives
/// the borrow of the shell's output cell.
fn cmd_dbtpn() -> CommandDef {
    CommandDef::new(
        "dbtpn",
        vec![
            ArgDesc {
                // C `dbtpnArg0`/`Arg1` (`dbIocRegister.c:361-362`).
                name: "record name",
                arg_type: ArgType::Record,
            },
            ArgDesc {
                name: "value",
                arg_type: ArgType::String,
            },
        ],
        "dbtpn record name, value - Database Test Process Notify\n\
         Without value, begin async. processing and get\n\
         With value, begin put, process, and get\n\
         Example: dbtpn aitest\n\
         Example: dbtpn aitest 5.0",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `if (!pname)` (`dbNotify.c:596-599`) — NULL only. An explicit
            // `dbtpn ""` is a non-NULL empty name and falls through to
            // `dbChannelCreate`, which refuses it.
            let ArgValue::String(name) = &args[0] else {
                ctx.println("Usage: dbtpn \"name\", \"value\"");
                return Ok(CommandOutcome::Failed);
            };
            // C `dbChannelCreate` (`dbNotify.c:600-604`) resolves record AND
            // field; either missing is one message.
            let (base, field) = parse_pv_name(name);
            let known = ctx.db().get_record(base).is_some_and(|rec| {
                let inst = rec.read();
                inst.snapshot_for_field(&field.to_ascii_uppercase())
                    .is_some()
            });
            if !known {
                ctx.println("dbtpn: No such channel");
                return Ok(CommandOutcome::Failed);
            }

            let db = ctx.db().clone();
            let name = name.clone();
            let value = match &args[1] {
                ArgValue::String(v) => Some(v.clone()),
                _ => None,
            };
            ctx.bridge().spawn(async move {
                for line in dbtpn_lines(&db, &name, value).await {
                    println!("{line}");
                }
            });
            Ok(CommandOutcome::Continue)
        },
    )
}

/// One `tpn` run as C's `doneCallback` prints it (`db_test.c:202-213`).
///
/// C always issues a `putProcessRequest` — `tpn` has no get-only arm,
/// which is the whole difference from `dbtpn` — and its `putCallback`
/// hands the argument down as `DBR_STRING` (`db_test.c:195-199`), so
/// this reuses the same string put the `dbtpn` arm takes.
async fn tpn_lines(
    db: &std::sync::Arc<crate::server::database::PvDatabase>,
    name: &str,
    value: String,
) -> Vec<String> {
    let (base, field) = parse_pv_name(name);
    let field = field.to_ascii_uppercase();
    let put = db
        .put_record_field_from_ca(base, &field, EpicsValue::String(value.as_str().into()))
        .await;
    let ok = match put {
        Ok(completion) => match completion.into_handle() {
            Some(rx) => rx.await.is_ok(),
            None => true,
        },
        Err(_) => false,
    };
    // C prints the RECORD name — `dbChannelRecord(ppn->chan)->name`
    // (`db_test.c:206`) — not the channel name the caller typed.
    if ok {
        vec![format!("tpnCallback '{base}': Success")]
    } else {
        // `notifyError` (2), the status the port's other notify path
        // reports a failed completion as.
        vec![format!("tpnCallback '{base}': Notify status 2")]
    }
}

/// `tpn "pv_name", "value"` — C `tpn` (`db_test.c:229-270`), registered
/// at `dbIocRegister.c:622`.
///
/// The narrower sibling of `dbtpn`: both arguments are required, the
/// request is always `putProcessRequest`, and the completion line is
/// the one `doneCallback` prints. Like `dbtpn` it returns as soon as
/// the work is handed off — C creates a `tpn` thread at
/// `epicsThreadPriorityHigh` (`db_test.c:267-268`) — so the callback
/// line arrives after the shell's next prompt and goes to `println!`
/// rather than `ctx.println`.
fn cmd_tpn() -> CommandDef {
    CommandDef::new(
        "tpn",
        vec![
            ArgDesc {
                // C `tpnArg0`/`Arg1` (`dbIocRegister.c:390-391`).
                name: "record name",
                arg_type: ArgType::Record,
            },
            ArgDesc {
                name: "value",
                arg_type: ArgType::String,
            },
        ],
        "tpn record name, value - Test Process Notify.\n\
         Example: tpn aitest 5.0",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `if (!pname || !pvalue)` (`db_test.c:235-238`) — NULL
            // only, so an explicit `tpn "" ""` is two non-NULL empty
            // strings and falls through to `dbChannel_create`.
            let (Some(ArgValue::String(name)), Some(ArgValue::String(value))) =
                (args.first(), args.get(1))
            else {
                ctx.println("Usage: tpn \"pv_name\", \"value\"");
                return Ok(CommandOutcome::Failed);
            };
            // C `dbChannel_create` (`db_test.c:239-243`) resolves record
            // AND field; either missing is the one message.
            let (base, field) = parse_pv_name(name);
            let known = ctx.db().get_record(base).is_some_and(|rec| {
                let inst = rec.read();
                inst.snapshot_for_field(&field.to_ascii_uppercase())
                    .is_some()
            });
            if !known {
                ctx.println("Channel couldn't be created");
                return Ok(CommandOutcome::Failed);
            }

            let db = ctx.db().clone();
            let name = name.clone();
            let value = value.clone();
            ctx.bridge().spawn(async move {
                for line in tpn_lines(&db, &name, value).await {
                    println!("{line}");
                }
            });
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Shared handler for the record-name glob search `dbglob` / `dbgrep`
/// (C `dbglob`, `dbTest.c:298-345`; `dbgrep` tail-calls it).
/// Mirrors epics-base PR #626 (rename `dbgrep` → `dbglob` with alias)
/// and PR #613 (add fields argument). The `fields` argument is
/// SPACE-separated (`splitFieldsList`) and each matching record is one
/// line built by `printFieldsList`. (`dbsr` is the *server report* — a
/// separate command — not this name search.)
fn dbglob_handler(args: &[ArgValue], ctx: &CommandContext) -> CommandResult {
    // C `dbTest.c:307-309`: a missing or empty pattern is a usage
    // error, not an implicit `*`, and a 1 return.
    let pattern = match args.first() {
        Some(ArgValue::String(s)) if !s.is_empty() => s.as_str(),
        _ => {
            ctx.println("Usage: dbglob \"pattern\" \"fields\"");
            return Ok(CommandOutcome::Failed);
        }
    };
    let fields: Vec<String> = match args.get(1) {
        Some(ArgValue::String(s)) => split_fields_list(s),
        _ => Vec::new(),
    };

    // C's `dbglob` names no `dbIsAlias` filter either (`dbTest.c:325-333`),
    // so the walk carries the alias nodes at their own load position rather
    // than in a lump at the end. The port then adds what C has no equivalent
    // of: `add_pv`-registered simple PVs (CA gateway shadows, IOC-stat
    // scratchpads), which a user globbing for every channel name would be
    // confused to miss. Field lookup via `get_record` follows
    // alias→canonical; for a simple PV the field-dump branch silently skips,
    // since it is not a record.
    let mut names: Vec<String> = db_nodes_type_major(ctx)
        .into_iter()
        .map(|node| node.name)
        .collect();
    let mut extra: Vec<String> = ctx.block_on(ctx.db().all_simple_pv_names());
    extra.sort();
    extra.dedup();
    extra.retain(|n| !names.contains(n));
    names.extend(extra);

    for name in &names {
        if !glob_match(pattern, name) {
            continue;
        }
        print_fields_list(ctx, name, &fields);
    }
    Ok(CommandOutcome::Continue)
}

/// `dbsr [interest level]` — Database Server Report.
///
/// C `dbIocRegister.c:137-140` registers `dbsr` as the *Database
/// Server Report* (`dbServerReport` — prints CA/PVA server status and
/// connected-client information). The Rust port previously aliased
/// `dbsr` to the record-name glob search, which is the wrong command
/// (`dbgrep`/`dbglob` is the name search — kept below).
///
/// `dbsr` knows nothing about channels or clients: it walks the layers
/// registered with [`crate::server::db_server`] and delegates to each
/// one's own report, which for the CA server is `casr` — exactly as
/// RSRV hands `casr` over in `rsrv_server` (`caservertask.c:1561-1569`).
/// The port used to print the database's record/alias/simple-PV
/// population instead, a figure no C `dbsr` has ever shown.
///
/// Measured against `softIoc` at `R7.0.10-146-g8f5015b66` with one
/// `camonitor` on two channels, `dbsr` / `dbsr 1` / `dbsr 2` print
/// `Server state: running`, `Server 'rsrv'`, then RSRV's own report
/// widening with the level. The port emits the first two lines
/// identically; the third is this workspace's `casr`, whose wording is
/// its own and is not this command's to choose.
fn cmd_dbsr() -> CommandDef {
    CommandDef::new(
        "dbsr",
        vec![ArgDesc {
            // C `dbsrArg0` (`dbIocRegister.c:135`).
            name: "interest level",
            arg_type: ArgType::Int,
        }],
        "dbsr [interest level] — Database Server Report",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `dbsrCallFunc` passes `args[0].ival` straight through
            // (`dbIocRegister.c:141`) into an `unsigned` parameter.
            let level = match args.first() {
                Some(ArgValue::Int(n)) => *n as u32,
                _ => 0,
            };
            crate::server::db_server::dbsr(level, &|line: &str| ctx.println(line));
            Ok(CommandOutcome::Continue)
        },
    )
}

fn cmd_dbglob() -> CommandDef {
    CommandDef::new(
        "dbglob",
        vec![
            ArgDesc {
                // C `dbglobArg0` (`dbIocRegister.c:237`).
                name: "pattern",
                arg_type: ArgType::Record,
            },
            ArgDesc {
                name: "fields",
                arg_type: ArgType::String,
            },
        ],
        "dbglob [pattern] [fields] — Search records by name pattern \
         (epics-base PR #626; `?` matches 0-or-1 chars)",
        dbglob_handler,
    )
}

fn cmd_dbgrep() -> CommandDef {
    CommandDef::new(
        "dbgrep",
        vec![
            ArgDesc {
                // C `dbgrep` shares `dbglobArgs` (`dbIocRegister.c:250`).
                name: "pattern",
                arg_type: ArgType::Record,
            },
            ArgDesc {
                name: "fields",
                arg_type: ArgType::String,
            },
        ],
        "dbgrep [pattern] [fields] — Search records by name pattern \
         (legacy spelling of dbglob, epics-base PR #626)",
        dbglob_handler,
    )
}

/// Has `scanInit` run? — the question C's three scan-report commands ask by
/// finding their list heads non-NULL.
///
/// Those heads are built during `iocInit`: `papPeriodic` by `initPeriodic`,
/// `pevent_list` by `scanAdd`'s `eventNameToHandle`, `pioscan_list` by
/// `scanIoInit` from device support. Before then C has nothing to walk, and
/// each command says so in its own shape — `scanppl` prints
/// `scanppl: dbScan subsystem not initialized` and returns -1
/// (`dbScan.c:388-392`), while `scanpel` (`:414-428`) and `scanpiol`
/// (`:434-455`) walk an empty list, print nothing and return 0.
///
/// The port's three read the SCAN-field index instead, which exists from the
/// moment the records load — so all three reported a scan subsystem that did
/// not exist yet, `scanpiol` even listing an `I/O Intr` record that `iocInit`
/// then rejects for having no `get_ioint_info`. One gate here, three C-shaped
/// answers at the call sites.
fn scan_lists_exist(ctx: &CommandContext) -> bool {
    ctx.db().ioc_is_running()
}

fn cmd_scanppl() -> CommandDef {
    CommandDef::new(
        "scanppl",
        vec![ArgDesc {
            name: "rate",
            arg_type: ArgType::Double,
        }],
        "scanppl [rate] — Print periodic scan lists, optionally just one rate",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `dbScan.c:388-392`, the first thing the body does — and the
            // status reaches the shell, because `scanpplCallFunc` is
            // `iocshSetError(scanppl(...))` (`dbIocRegister.c:460-462`).
            if !scan_lists_exist(ctx) {
                ctx.println("scanppl: dbScan subsystem not initialized");
                return Ok(CommandOutcome::Failed);
            }
            // C `scanppl` (`dbScan.c:399-401`): a positive rate selects
            // the one periodic list whose period is within 0.05 s; 0 or
            // no argument prints them all.
            let rate = match args.first() {
                Some(ArgValue::Double(r)) if *r > 0.0 => Some(*r),
                _ => None,
            };
            // The rates of the LOADED menuScan, not a fixed seven: C walks
            // `papPeriodic[0..nPeriodic]`, which is the site's own menu
            // (`dbScan.c:390`), in that array's own index order — slowest
            // rate first, because `papPeriodic[0]` is `SCAN_1ST_PERIODIC`.
            //
            // And it walks NOTHING else. The Event lists belong to `scanpel`
            // (`dbScan.c:411-428`), the I/O Intr lists to `scanpiol`
            // (`:430-452`), and a Passive record is in no scan list at all —
            // `scanAdd` returns before linking it (`:241-243`). Printing them
            // here made `scanppl` a report C has no counterpart for, so a
            // head-to-head against a C IOC could not be read.
            let scan_types = crate::server::scan::periodic_scans();

            for st in &scan_types {
                if let Some(rate) = rate {
                    let matches = st
                        .interval()
                        .is_some_and(|d| (rate - d.as_secs_f64()).abs() <= 0.05);
                    if !matches {
                        continue;
                    }
                }
                let names = ctx.block_on(ctx.db().records_for_scan(*st));
                // C `printList` (`dbScan.c:969-991`) returns before printing
                // anything at all when the list is empty — not even the
                // header.
                if names.is_empty() {
                    continue;
                }
                // C prints the list's cumulative over-run count in the
                // header — `Records with SCAN = '%s' (%lu over-runs):`
                // (`dbScan.c:404-406`). It is the observable half of the
                // over-run rule: without it a list that keeps missing its
                // deadline looks identical to one that never does. Every
                // entry here is a periodic list, which is what carries the
                // counter (C keeps it on `periodic_scan_list`).
                let overruns = st
                    .scan_list()
                    .map(|l| ctx.db().scan_overruns(l))
                    .unwrap_or(0);
                ctx.println(&format!(
                    "Records with SCAN = '{st}' ({overruns} over-runs):"
                ));
                for name in &names {
                    // C `printf("    %-28s\n", pse->precord->name)`
                    // (`dbScan.c:980`) — four spaces, then the name padded to
                    // 28 columns.
                    ctx.println(&format!("    {name:<28}"));
                }
            }

            Ok(CommandOutcome::Continue)
        },
    )
}

/// C `priorityName[]` (`dbScan.c:104-106`) — the three callback
/// priorities `scanpel` and `scanpiol` band their records into. Not
/// `menuPriority`, whose choices are `LOW`/`MEDIUM`/`HIGH`: these are
/// the report's own spelling and the two must not be conflated.
const SCAN_PRIORITY_NAMES: [&str; 3] = ["Low", "Medium", "High"];

/// C `printList` (`dbScan.c:969-992`): print nothing at all when the
/// list is empty, otherwise the caller's header and then one
/// left-justified 28-column record name per line.
fn print_scan_list(ctx: &CommandContext, header: &str, names: &[String]) {
    if names.is_empty() {
        return;
    }
    ctx.println(header);
    for name in names {
        ctx.println(&format!("    {name:<28}"));
    }
}

/// Split `names` into C's three per-priority lists by each record's
/// `PRIO` field — C keeps one `scan_list` per priority inside the event
/// (or I/O event) head and `scanAdd` files the record by `precord->prio`
/// (`dbScan.c:601-612`), so the grouping is a property of the record,
/// not of the report.
fn scan_lists_by_priority(ctx: &CommandContext, names: &[String]) -> [Vec<String>; 3] {
    let mut out: [Vec<String>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for name in names {
        let Some(rec) = ctx.db().get_record(name) else {
            continue;
        };
        let prio = rec.read().common.prio;
        let idx = (prio.clamp(0, 2)) as usize;
        out[idx].push(name.clone());
    }
    out
}

/// `scanpel ["event name"]` — C `scanpel` (`dbScan.c:411-428`), printing
/// the `SCAN = "Event"` records grouped by event name and then by
/// callback priority. The argument is an `epicsStrGlobMatch` PATTERN
/// over the event name, not a key, and an absent one prints every event.
///
/// C walks `pevent_list`, the chain `eventNameToHandle` builds, so it
/// can print an `Event "x"` header for a name that has been registered
/// but holds no records. The port has no such chain — an event exists
/// exactly as long as some record's `EVNT` names it — so an empty event
/// does not appear here. Everything an event actually scans does.
fn cmd_scanpel() -> CommandDef {
    CommandDef::new(
        "scanpel",
        vec![ArgDesc {
            // C `scanpelArg0` (`dbIocRegister.c:459`).
            name: "event name",
            arg_type: ArgType::String,
        }],
        "scanpel [\"event name\"] — Print info for records with SCAN = \"Event\".",
        |args: &[ArgValue], ctx: &CommandContext| {
            use crate::server::database::scan_index::normalize_event_name;
            use crate::server::record::ScanType;

            // C walks `pevent_list`, which `scanAdd` fills at `iocInit`, so
            // before then it prints nothing and returns 0 — no refusal line.
            if !scan_lists_exist(ctx) {
                return Ok(CommandOutcome::Continue);
            }

            let pattern = match args.first() {
                Some(ArgValue::String(p)) if !p.is_empty() => Some(p.clone()),
                _ => None,
            };

            // Group the Event-scanned records by the event their `EVNT`
            // names, through the same normalisation `post_event_named`
            // routes on, so `"5"`, `" 5 "` and `"5.0"` are one event
            // here exactly as they are one handle in C.
            let mut by_event: std::collections::BTreeMap<String, Vec<String>> =
                std::collections::BTreeMap::new();
            for name in ctx.block_on(ctx.db().records_for_scan(ScanType::Event)) {
                let Some(rec) = ctx.db().get_record(&name) else {
                    continue;
                };
                let evnt = normalize_event_name(&rec.read().common.evnt);
                if evnt.is_empty() {
                    continue;
                }
                by_event.entry(evnt).or_default().push(name);
            }

            for (event, names) in &by_event {
                if let Some(pattern) = &pattern
                    && !epics_strn_glob_match(event.as_bytes(), event.len(), pattern.as_bytes())
                {
                    continue;
                }
                ctx.println(&format!("Event \"{event}\""));
                for (prio, list) in scan_lists_by_priority(ctx, names).iter().enumerate() {
                    print_scan_list(
                        ctx,
                        &format!(" Priority {}", SCAN_PRIORITY_NAMES[prio]),
                        list,
                    );
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `scanpiol` — C `scanpiol` (`dbScan.c:430-452`), the `SCAN = "I/O
/// Intr"` report.
///
/// C keeps one `ioscan_head` per interrupt SOURCE — device support
/// calls `scanIoInit` to get one — and heads its blocks
/// `IO Event %p: Priority %s`, the pointer being the only name a source
/// has. The port has one I/O Intr list and no per-source head, so there
/// is one block and no address to print; the header keeps C's shape
/// minus the pointer rather than inventing an identity.
fn cmd_scanpiol() -> CommandDef {
    CommandDef::new(
        "scanpiol",
        vec![],
        "scanpiol — Print info for records with SCAN = \"I/O Intr\".",
        |_args: &[ArgValue], ctx: &CommandContext| {
            use crate::server::record::ScanType;
            // C walks `pioscan_list`, which `scanIoInit` fills at `iocInit`
            // from each record's device support, so before then it prints
            // nothing and returns 0 — no refusal line. A record whose SCAN
            // says `I/O Intr` is in the port's index from load, but it is not
            // in an I/O event list until `iocInit` has accepted its DSET.
            if !scan_lists_exist(ctx) {
                return Ok(CommandOutcome::Continue);
            }
            let names = ctx.block_on(ctx.db().records_for_scan(ScanType::IoIntr));
            for (prio, list) in scan_lists_by_priority(ctx, &names).iter().enumerate() {
                print_scan_list(
                    ctx,
                    &format!("IO Event: Priority {}", SCAN_PRIORITY_NAMES[prio]),
                    list,
                );
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `pushd [dir]` — push the current directory onto the stack and `cd`.
/// With no argument, swaps the current dir with the top of the stack.
///
/// Port-only, with [`popd`](cmd_popd) and [`dirs`](cmd_dirs), and listed in
/// `PORT_ONLY_COMMANDS`: C's `libComRegister.c` registers `cd` and `pwd` and
/// nothing else, and C's `cd` keeps no stack. The gap it fills is an `st.cmd`
/// that `iocshLoad`s a script which `cd`s: in C the caller can only get back
/// by knowing its own absolute path, because the callee's `cd` is global.
fn cmd_pushd() -> CommandDef {
    CommandDef::new(
        "pushd",
        vec![ArgDesc {
            // Port-only command; the argument is a directory.
            name: "dir",
            arg_type: ArgType::Path,
        }],
        "pushd [dir] — push current dir onto stack and cd to <dir>",
        |args: &[ArgValue], ctx: &CommandContext| {
            let cwd = match std::env::current_dir() {
                Ok(p) => p,
                Err(e) => {
                    return Err(format!("pushd: cannot read cwd: {e}"));
                }
            };
            match &args[0] {
                ArgValue::String(dir) => {
                    if let Err(e) = super::core_commands::set_working_dir(dir) {
                        return Err(format!("pushd: {dir}: {e}"));
                    }
                    dir_stack().lock().unwrap().push(cwd);
                }
                _ => {
                    // No arg: swap cwd with top of stack.
                    let mut stack = dir_stack().lock().unwrap();
                    let Some(top) = stack.pop() else {
                        return Err("pushd: directory stack empty".into());
                    };
                    if let Err(e) = super::core_commands::set_working_dir(&top) {
                        // Restore on failure.
                        stack.push(top);
                        return Err(format!("pushd: {e}"));
                    }
                    stack.push(cwd);
                }
            }
            print_stack(ctx);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `popd` — pop the top of the directory stack and `cd` to it. Port-only; see
/// [`cmd_pushd`] for why the stack exists and `PORT_ONLY_COMMANDS` for the
/// census that keeps this documented.
fn cmd_popd() -> CommandDef {
    CommandDef::new(
        "popd",
        vec![],
        "popd — pop top of directory stack and cd to it",
        |_args: &[ArgValue], ctx: &CommandContext| {
            let mut stack = dir_stack().lock().unwrap();
            let Some(top) = stack.pop() else {
                return Err("popd: directory stack empty".into());
            };
            if let Err(e) = super::core_commands::set_working_dir(&top) {
                // Restore the entry — failed cd must not lose stack state.
                stack.push(top);
                return Err(format!("popd: {e}"));
            }
            drop(stack);
            print_stack(ctx);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dirs` — list the directory stack (cwd + saved entries). Port-only; see
/// [`cmd_pushd`] and `PORT_ONLY_COMMANDS`.
fn cmd_dirs() -> CommandDef {
    CommandDef::new(
        "dirs",
        vec![],
        "dirs — list the iocsh directory stack",
        |_args: &[ArgValue], ctx: &CommandContext| {
            print_stack(ctx);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbStaticLib.h:255,256,269,270` status codes, as `errSymMsg` prints
/// them: `M_dbLib` is `512 << 16`, so the numbers below are what a C
/// console shows. Measured on `softIoc` R7.0.10-146 —
/// `dbCreateRecord(pdbbase,"nosuchtype","R:X")` answers `ERROR: 33554433
/// Record Type does not exist`.
const S_DB_LIB_RECORD_TYPE_NOT_FOUND: u32 = (512 << 16) | 1;
const S_DB_LIB_REC_EXISTS: u32 = (512 << 16) | 3;
/// C `S_dbLib_recNotFound` (`dbStaticLib.h:257`) — "Record Not Found".
const S_DB_LIB_REC_NOT_FOUND: u32 = (512 << 16) | 5;
const S_DB_LIB_POST_INIT_REC_REGISTER: u32 = (512 << 16) | 31;
const S_DB_LIB_RECORD_NAME_MISSING: u32 = (512 << 16) | 33;

/// `dbCreateRecord <type> <name>` — create a record BEFORE `iocInit`.
///
/// Mirrors epics-base PR #812. Validates the name with the same rules
/// as `parse_db` (PR #78), refuses duplicate names, and routes the
/// instantiation through the same factory registry as `dbLoadRecords`.
///
/// C gained the command in `f4ccf7bc8` (PR #812, 2026-03-10), which is in
/// no release tag: at the `R7.0.10` pin `dbStaticIocRegister.c` is 281
/// lines and holds none of this. Every line number below is therefore read
/// at `f4ccf7bc8`, where the file is identical to `origin/7.0`.
///
/// "At runtime" it is not: C's `dbCreateRecordCallFunc`
/// (`dbStaticIocRegister.c:288-291` at `f4ccf7bc8`) refuses it outright once
/// `getIocState() != iocVoid`, with `S_dbLib_postInitRecRegister` (R19-63) —
/// the same gate `dbReadCOM` puts on a `.db` read:
///
/// ```text
/// epics> iocInit
/// epics> dbCreateRecord(pdbbase,"ai","NEWREC")
/// ERROR: 33554463 IOC already initialized - No new records can be added
/// epics> dbl
/// CO
/// ```
fn cmd_db_create_record() -> CommandDef {
    CommandDef::new(
        "dbCreateRecord",
        vec![
            ArgDesc {
                name: "pdbbase",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "recordType",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "recordName",
                arg_type: ArgType::String,
            },
        ],
        "dbCreateRecord pdbbase <type> <name> — Create a new record of <type> (before iocInit)",
        |args: &[ArgValue], ctx: &CommandContext| {
            check_pdbbase(&args[0])?;
            // Creating a record IS entering the load phase — the record's links
            // are classified by `iocInit`, with the rest of the database. Once
            // `iocInit` has run there is no phase to enter and C refuses.
            if let Err(e) = ctx.db().begin_load() {
                return Err(format!("{S_DB_LIB_POST_INIT_REC_REGISTER} {e}"));
            }
            // C `dbStaticIocRegister.c:294-297` at `f4ccf7bc8` asks for the NAME
            // first and counts an empty one as missing
            // (`S_dbLib_recordNameMissing`), reaching
            // `S_dbLib_recordTypeNotFound` only once a name is in
            // hand. Asking about the type first made a bare
            // `dbCreateRecord pdbbase` complain about the argument the
            // operator was not being asked for.
            let name = match &args[2] {
                ArgValue::String(s) if !s.is_empty() => s.clone(),
                _ => {
                    return Err(format!(
                        "{S_DB_LIB_RECORD_NAME_MISSING} Record name is required"
                    ));
                }
            };
            let rec_type = match &args[1] {
                ArgValue::String(s) => s.clone(),
                _ => {
                    return Err(format!(
                        "{S_DB_LIB_RECORD_TYPE_NOT_FOUND} Record Type does not exist"
                    ));
                }
            };
            // Failures return Err so `on error` sees them — current C
            // base wraps exactly these in `iocshSetError`
            // (`dbStaticIocRegister.c:282-310` at `f4ccf7bc8`);
            // epics-base#498 / UI-105.
            if let Err(e) = db_loader::validate_record_name(&name, 0, 0) {
                return Err(format!("dbCreateRecord: {e}"));
            }
            if ctx.db().get_record(&name).is_some() {
                return Err(format!("{S_DB_LIB_REC_EXISTS} Record Already exists"));
            }
            // C reaches this through `dbFindRecordType`
            // (`dbStaticIocRegister.c:299` at `f4ccf7bc8`), whose miss is the same
            // status as a missing type argument.
            let record = db_loader::create_record(&rec_type).map_err(|_| {
                format!("{S_DB_LIB_RECORD_TYPE_NOT_FOUND} Record Type does not exist")
            })?;
            if let Err(e) = ctx.block_on(ctx.db().add_record(&name, record)) {
                return Err(format!("dbCreateRecord: {e}"));
            }
            // C says nothing on success: `dbCreateRecordCallFunc`
            // (`dbStaticIocRegister.c:282-310` at `f4ccf7bc8`) prints only on
            // a non-zero status. Measured on `softIoc` R7.0.10-146,
            // `dbCreateRecord pdbbase ai NEW:ONE` produced no output and the
            // following `dbl` listed the record.
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `postEvent [event]` — process records scanned on a software event.
///
/// L-2: C `dbIocRegister.c:631` registers this command as `postEvent`
/// (camelCase); the Rust port registered the documented name with an
/// underscore (`post_event`), so an `st.cmd` calling the real name hit
/// "unknown command".
///
/// `post_event` is NOT a second spelling C would accept. It is a C
/// *function* — `dbScan.c:547-551`, `void post_event(int event)`, kept for
/// backward compatibility and taking an event NUMBER — which
/// `dbIocRegister.c` never hands to `iocshRegister`. A shell that answers
/// it is a shell whose `help` lists a command no C IOC has, so the alias
/// this port carried after L-2 is gone: one handler, one name, the name C
/// registers.
fn cmd_post_event() -> CommandDef {
    CommandDef::new(
        "postEvent",
        vec![ArgDesc {
            name: "event name",
            arg_type: ArgType::String,
        }],
        "postEvent <event name> — Manually scan all records with EVNT == name.",
        post_event_handler,
    )
}

fn post_event_handler(args: &[ArgValue], ctx: &CommandContext) -> CommandResult {
    // C `postEventCallFunc` (`dbIocRegister.c:471-475`) is
    // `postEvent(eventNameToHandle(args[0].sval))` and nothing else: the
    // argument is a NAME, not an index, and the command prints nothing.
    // `eventNameToHandle` returns NULL for a missing or blank name
    // (`dbScan.c:480-482`) and `postEvent(NULL)` returns at once, so a
    // bare `postEvent` scans nothing.
    match args.first() {
        Some(ArgValue::String(name)) if !name.trim().is_empty() => {
            ctx.block_on(ctx.db().post_event_named(name));
        }
        _ => {}
    }
    Ok(CommandOutcome::Continue)
}

/// C `epicsStrnGlobMatch` (`epicsString.c:282-312`) — the matcher
/// `dbglob` and `epicsEnvShow` both call. `?` consumes exactly one
/// character: the trailing `while (*pattern == '*') pattern++` skips
/// only `*`, so a `?` left over once the string is exhausted fails the
/// match. `dbglob`'s help text (`dbIocRegister.c:243`) says "0 or
/// one characters", but the code is what runs, and this is a
/// transcription of the code.
pub(super) fn epics_strn_glob_match(s: &[u8], len: usize, pattern: &[u8]) -> bool {
    let len = len.min(s.len());
    let at = |p: usize| pattern.get(p).copied().unwrap_or(0);
    let mut mp: Option<usize> = None;
    let mut cp: usize = 0;
    let mut i: usize = 0;
    let mut p: usize = 0;

    while i < len && at(p) != b'*' {
        if at(p) != s[i] && at(p) != b'?' {
            return false;
        }
        p += 1;
        i += 1;
    }
    while i < len {
        if at(p) == b'*' {
            p += 1;
            if at(p) == 0 {
                return true;
            }
            mp = Some(p);
            cp = i + 1;
        } else if at(p) == s[i] || at(p) == b'?' {
            p += 1;
            i += 1;
        } else {
            // The first loop returns on any mismatch, so this branch
            // is only reachable after a `*` set `mp`.
            p = mp.unwrap_or(0);
            i = cp;
            cp += 1;
        }
    }
    while at(p) == b'*' {
        p += 1;
    }
    at(p) == 0
}

/// [`epics_strn_glob_match`] over whole strings — C `epicsStrGlobMatch`.
fn glob_match(pattern: &str, text: &str) -> bool {
    epics_strn_glob_match(text.as_bytes(), text.len(), pattern.as_bytes())
}

/// `iocStats` — record count, uptime, RSS, cores and scan-list totals.
///
/// Port-only, and the one of the five with no C operation behind it at all:
/// no `*IocRegister.c` in R7.0.10 registers a runtime-statistics verb under
/// any spelling. The `devIocStats` module publishes these same numbers, but as
/// records a client reads, never as a command an `st.cmd` can call. Listed in
/// `PORT_ONLY_COMMANDS`.
fn cmd_ioc_stats() -> CommandDef {
    CommandDef::new(
        "iocStats",
        vec![],
        "iocStats — Show IOC runtime statistics",
        |_args: &[ArgValue], ctx: &CommandContext| {
            // Record count
            let names = ctx.block_on(ctx.db().all_record_names());
            ctx.println(&format!("Records:    {}", names.len()));

            // Uptime
            static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            let start = START.get_or_init(std::time::Instant::now);
            let uptime = start.elapsed();
            let hours = uptime.as_secs() / 3600;
            let mins = (uptime.as_secs() % 3600) / 60;
            let secs = uptime.as_secs() % 60;
            ctx.println(&format!("Uptime:     {hours}h {mins}m {secs}s"));

            // Memory (RSS) — read from /proc on Linux, skip on other platforms
            #[cfg(target_os = "linux")]
            if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
                for line in status.lines() {
                    if let Some(val) = line.strip_prefix("VmRSS:") {
                        ctx.println(&format!("RSS:        {}", val.trim()));
                        break;
                    }
                }
            }

            // Thread count (approximate via tokio metrics if available)
            let threads = std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(1);
            ctx.println(&format!("CPU cores:  {threads}"));

            // Scan types summary
            use crate::server::record::ScanType;
            let scan_types = [
                ScanType::SEC01,
                ScanType::SEC02,
                ScanType::SEC05,
                ScanType::SEC1,
                ScanType::SEC2,
                ScanType::SEC5,
                ScanType::SEC10,
            ];
            let mut total_scanned = 0;
            for st in &scan_types {
                total_scanned += ctx.block_on(ctx.db().records_for_scan(*st)).len();
            }
            let io_intr = ctx
                .block_on(ctx.db().records_for_scan(ScanType::IoIntr))
                .len();
            ctx.println(&format!("Periodic:   {total_scanned} records"));
            ctx.println(&format!("I/O Intr:   {io_intr} records"));

            Ok(CommandOutcome::Continue)
        },
    )
}

fn cmd_db_load_records() -> CommandDef {
    CommandDef::new(
        "dbLoadRecords",
        vec![
            ArgDesc {
                // C `dbLoadRecordsArg0` (`dbIocRegister.c:55`).
                name: "file",
                arg_type: ArgType::Path,
            },
            ArgDesc {
                name: "macros",
                arg_type: ArgType::String,
            },
        ],
        "dbLoadRecords file [macros] - Load records from a .db/.template file",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `dbAccess.c:799-802` tests the file name only for NULL,
            // so an empty name still reaches the open and fails there.
            let path = match &args[0] {
                ArgValue::String(s) => s,
                _ => {
                    // C prints the usage line and returns -1 (`dbAccess.c:799-802`),
                    // which `dbLoadRecordsCallFunc` hands straight to
                    // `iocshSetError` (`dbIocRegister.c:71-74`) — so the line
                    // FAILED, and under `on error break` a boot stops here.
                    ctx.println("Usage: dbLoadRecords \"file\", \"subs\"");
                    return Ok(CommandOutcome::Failed);
                }
            };
            let macros_str = match &args[1] {
                ArgValue::String(s) => s.as_str(),
                _ => "",
            };

            // C passes a hard `0` for the search path (`dbAccess.c:803`), so
            // a `dbLoadRecords` list can only ever come from the environment.
            let mut faults = db_loader::DbFaults::default();
            match db_read_database(ctx, path, "", macros_str, &mut faults) {
                // C says NOTHING on the success path: `dbLoadRecords`
                // calls the (always-NULL in base) `dbLoadRecordsHook` and
                // returns 0 (`dbAccess.c:804-806`). A progress line here
                // is output no C IOC produces, on the stream a startup
                // script's own output shares.
                Ok(()) => Ok(CommandOutcome::Continue),
                // C `dbAccess.c:807-811`: the summary the command adds on
                // top of whatever the read already reported, on every
                // failure alike, and the second line only the load-phase
                // refusal gets.
                //
                // ```text
                // epics> iocInit
                // epics> dbLoadRecords("b.db")
                // ERROR: Failed to load 'b.db'
                //     Records cannot be loaded after iocInit!
                // ```
                //
                // Written HERE and answered with `Failed` rather than
                // returned as `Err(...)`: C's `dbLoadRecords` prints its own
                // summary and hands `iocshSetError` a bare status, so the
                // shell adds nothing. Returning text made the shell print a
                // second copy of a diagnostic the read had already written.
                Err(failure) => {
                    ctx.eprintln(&format!("{ERL_ERROR}: Failed to load '{path}'"));
                    if matches!(failure, DbReadFailure::AfterIocInit) {
                        ctx.eprintln("    Records cannot be loaded after iocInit!");
                    }
                    Ok(CommandOutcome::Failed)
                }
            }
        },
    )
}

/// C `dbLoadDatabase` (`dbAccess.c:786-793`) — the only iocsh route to a
/// `.dbd` file, and the reason `menuConvert` could not grow a user
/// breakpoint table before now (`cvt_bpt.rs`).
///
/// It is [`cmd_db_load_records`] with exactly two differences: the caller
/// may name a search path of its own, and nothing is printed about the
/// outcome. C reports the result only through `iocshSetError`
/// (`dbIocRegister.c:49-52`), so a load that worked is silent and one that
/// did not says no more than the read itself already said.
///
/// LIMIT, stated: a `.dbd` reaches the same grammar C uses (one `yyparse`
/// for both commands), so its `record(...)`, `alias(...)` and
/// `breaktable(...)` install exactly as a `.db`'s would. Its DEFINITIONS —
/// `recordtype`, `menu`, `device`, `driver`, `registrar`, `function`,
/// `variable` — are parsed into [`db_loader::ParsedDb::dbd`] and then
/// dropped, because this port's type system is generated at build time
/// (`record/dbd_generated.rs`, `tools/dbd-codegen`) rather than built by the
/// load. `dbLoadDatabase("softIoc.dbd")` therefore leaves a port IOC in the
/// same state C reaches, since those record types are already present; a
/// SITE `.dbd` declaring a new record type or device support does not.
fn cmd_db_load_database() -> CommandDef {
    CommandDef::new(
        "dbLoadDatabase",
        vec![
            ArgDesc {
                // C `dbLoadDatabaseArg0..2` (`dbIocRegister.c:34-36`).
                name: "file name",
                arg_type: ArgType::Path,
            },
            ArgDesc {
                name: "path",
                arg_type: ArgType::Path,
            },
            ArgDesc {
                name: "substitutions",
                arg_type: ArgType::String,
            },
        ],
        "dbLoadDatabase file [path] [substitutions] - Load a .dbd or .db file",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C tests only for NULL (`dbAccess.c:788`) and returns -1, which
            // `dbLoadDatabaseCallFunc` hands to `iocshSetError`, so the shell
            // records the line as failed. Every `Usage:` line in this file that
            // C reaches through an `iocshSetError` CallFunc answers the same
            // way; `astac` is the exception, because `astacCallFunc`
            // (`asIocRegister.c:126-129`) discards `astac`'s status.
            let file = match &args[0] {
                ArgValue::String(s) => s,
                _ => {
                    ctx.println("Usage: dbLoadDatabase \"file\", \"path\", \"subs\"");
                    return Ok(CommandOutcome::Failed);
                }
            };
            let path = match &args[1] {
                ArgValue::String(s) => s.as_str(),
                _ => "",
            };
            let substitutions = match &args[2] {
                ArgValue::String(s) => s.as_str(),
                _ => "",
            };

            let mut faults = db_loader::DbFaults::default();
            match db_read_database(ctx, file, path, substitutions, &mut faults) {
                Ok(_) => Ok(CommandOutcome::Continue),
                // C `dbLoadDatabase` returns `dbReadDatabase`'s status and
                // says nothing of its own (`dbAccess.c:786-793`), so every
                // failure has already been reported in full by the read.
                Err(_) => Ok(CommandOutcome::Failed),
            }
        },
    )
}

/// Why a [`db_read_database`] call failed, in the three shapes C's two
/// callers tell apart.
///
/// Every variant means the same thing about output: the read has already
/// said everything IT is going to say. What the callers still differ on is
/// the SUMMARY C adds afterwards — `dbLoadRecords` writes one
/// (`dbAccess.c:807-811`), `dbLoadDatabase` writes none (`:786-793`) — so
/// the summary is the caller's and the diagnostics are the read's, and
/// neither repeats the other. No variant carries the diagnostic text: it is
/// on the stream already, and a value a caller could read back is one
/// `eprintln!` away from a second copy of it.
enum DbReadFailure {
    /// C `dbReadCOM` status -2 (`dbLexRoutines.c:236-239`): the read is
    /// refused before the file is even opened because the IOC has left
    /// `iocVoid`. C prints nothing of its own here.
    AfterIocInit,
    /// C `dbOpenFile` returned NULL (`dbLexRoutines.c:281-290`).
    CannotOpen,
    /// The parser or the record install rejected the contents.
    Rejected,
}

/// C `dbReadDatabase`/`dbReadCOM` (`dbLexRoutines.c:223-310`) — the whole
/// of the load that `dbLoadDatabase` and `dbLoadRecords` share. C's two
/// entry points differ only in the search path they hand in (`path` at
/// `dbAccess.c:792` against a hard `0` at `:803`) and in what they report
/// afterwards, so both of those stay with the callers and everything
/// between them lives here exactly once.
///
/// Returns C's status and nothing else. It used to hand back a record
/// count, which no caller needed for anything but a progress line C does
/// not print.
fn db_read_database(
    ctx: &CommandContext,
    file: &str,
    path: &str,
    substitutions: &str,
    faults: &mut db_loader::DbFaults,
) -> Result<(), DbReadFailure> {
    // A load OPENS the load phase; it does not close it — the boundary a
    // record's links are classified against is `iocInit`, after EVERY load
    // in the `st.cmd`, so a forward reference to a record loaded by a later
    // file is still a local PV (R18-92). Idempotent across the several
    // loads one script issues.
    //
    // And once `iocInit` has run there is no load phase to open: C fails
    // the read with -2 before it opens the file, so nothing is created and
    // nothing is parsed (R19-63).
    if ctx.db().begin_load().is_err() {
        return Err(DbReadFailure::AfterIocInit);
    }

    let macros = parse_macro_string(substitutions);

    // C installs the search path inside the read (`:245-253`), before the
    // open, so a load that cannot find its file has still replaced the list
    // `dbDumpPath` reports.
    let (config, file_path) = resolve_db_file(file, path);
    let Some(file_path) = file_path else {
        ctx.eprintln(&format!("{ERL_ERROR}: Can't open file '{file}'"));
        return Err(DbReadFailure::CannotOpen);
    };

    // C macros are pure text substitution (dbLexRoutines.c → macLib): a
    // `DTYP=` macro reaches a record only where the file wrote
    // `field(DTYP,"$(DTYP)")`. It does NOT rewrite a record that spells its
    // DTYP literally. `parse_db_file_with_breaktables` already performs that
    // substitution, so there is nothing further to do for DTYP here.
    // Printed HERE rather than carried out as text: this is the one
    // rejection the loader itself never reported, and leaving it to a caller
    // is what made the caller a printer.
    // NOT printed here. The loader reports its own abort through
    // `DbFaults::abort` — C's `yyerror(str)` arm — with the file, the
    // line and the ` <N> | <source>` echo. This site used to print the
    // error VALUE instead, `parse error: DB parse error at line 3,
    // column 6: expected 'field', got 'qqq'`, and that positionless line
    // was the whole of what an operator with a syntax error in a real
    // `.db` got: no file, no line, no source.
    let parsed = db_loader::parse_db_opened_with_breaktables(&file_path, &macros, &config)
        .map_err(|_| DbReadFailure::Rejected)?;

    // Merge any `breaktable(...)` definitions into the database's shared
    // breakpoint-table registry (C `bptList`) and snapshot it for the
    // records this call loads. A record resolves a table loaded by an
    // earlier or the same read (C ordering) — and because a `.dbd` reaches
    // the same grammar, `dbLoadDatabase` is how a site's own table gets in.
    let breaktable_registry =
        ctx.block_on(async { ctx.db().add_breaktables(parsed.breaktables).await });

    // C's `dbReadCOM` fills `pdbbase->registrarList` from the same parse that
    // yields the records, so a `.dbd` read through this command is what makes
    // `dbDumpRegistrar` answer. The parser already produced the names; nothing
    // kept them.
    super::dbstatic_commands::add_loaded_registrars(&parsed.dbd.registrars);
    // Same for `link()`: `dbLinkType` fills `pdbbase->linkList` from the same
    // parse, so a site's own `link(<key>, <jlif>)` line reaches `dbDumpLink`.
    super::dbstatic_commands::add_loaded_link_types(&parsed.dbd.link_types);

    // One install path for `dbLoadDatabase`, `dbLoadRecords` and
    // `dbLoadTemplate`: each definition flows through the SAME per-record
    // routine, so a record declared in a `.dbd` is indistinguishable from
    // one loaded from a `.db`.
    faults.absorb(parsed.faults);
    ctx.block_on(install_record_defs(
        ctx,
        parsed.records,
        parsed.unresolved_aliases,
        &breaktable_registry,
        faults,
    ))?;

    Ok(())
}

/// Build the DB include config the way `dbReadCOM` does
/// (`dbLexRoutines.c:245-253`): a non-empty `explicit_path` wins outright,
/// otherwise `EPICS_DB_INCLUDE_PATH`, otherwise `"."`. Only
/// `dbLoadDatabase` can supply the first arm — C hands `dbLoadRecords` a
/// hard `0` — which is why `explicit_path` had no caller and the arm was
/// missing until the command existed.
///
/// Resolving the list is also what INSTALLS it: C `dbPath` runs inside
/// `dbReadCOM`, so the path a load resolved is the path `dbDumpPath`
/// reports afterwards. This is the only routine that resolves one, so it
/// is the only writer of [`db_loader::set_loaded_path`].
fn db_load_config(explicit_path: &str) -> db_loader::DbLoadConfig {
    let include_paths = if explicit_path.is_empty() {
        std::env::var("EPICS_DB_INCLUDE_PATH").map_or_else(
            |_| vec![std::path::PathBuf::from(".")],
            |val| db_loader::db_path(&val),
        )
    } else {
        db_loader::db_path(explicit_path)
    };
    db_loader::set_loaded_path(&include_paths);
    db_loader::DbLoadConfig {
        include_paths,
        max_include_depth: 32,
    }
}

/// Resolve a load's file name through C `dbOpenFile`: the path list is
/// searched FIRST and the process CWD is never consulted for a bare name.
/// `None` is C's NULL return, which `dbReadCOM` turns into `Can't open
/// file` without ever reaching the parser.
///
/// `include_path` is `dbLoadDatabase`'s second argument, empty for every
/// other caller — see [`db_load_config`].
fn resolve_db_file(
    path: &str,
    include_path: &str,
) -> (db_loader::DbLoadConfig, Option<db_loader::DbOpenedFile>) {
    let config = db_load_config(include_path);
    // The located form, not the bare `PathBuf`: every diagnostic the load
    // raises names the file as the operator wrote it and the path entry it
    // was found under, which is what C's `inputFile` keeps — see
    // [`db_loader::DbOpenedFile`].
    let opened = db_loader::db_open_file_located(path, &config.include_paths);
    (config, opened)
}

/// Resolve a `dbLoadTemplate` `.substitutions` file. C keeps the
/// opposite gate here from the one it uses for the templates the file
/// names: `dbLoadTemplate.y:362-370` tries a bare `fopen(sub_file)`
/// first and only falls back to the path list when that fails and the
/// name is not absolute. The templates themselves go through
/// `dbLoadRecords`, i.e. [`resolve_db_file`]'s rule.
///
/// `search_path` is the command's third argument, which C substitutes
/// for `EPICS_DB_INCLUDE_PATH` when it is non-empty (`:363-366`). It
/// reaches only this lookup: `dbLoadRecords` resets the path list from
/// the environment for every template, so the argument never affects
/// the templates the file names.
fn resolve_substitutions_file(
    path: &str,
    search_path: &str,
) -> (db_loader::DbLoadConfig, std::path::PathBuf) {
    let config = db_load_config("");
    let direct = std::path::PathBuf::from(path);
    if direct.exists() {
        return (config, direct);
    }
    if direct.is_absolute() {
        return (config, direct);
    }
    let search = if search_path.is_empty() {
        config.include_paths.clone()
    } else {
        db_loader::db_path(search_path)
    };
    let file_path = db_loader::db_open_file(path, &search).unwrap_or(direct);
    (config, file_path)
}

/// How C classifies a per-record `.db` failure. `dbRecordHead` and
/// `dbRecordField` (`dbLexRoutines.c:1123-1197`, `:1199-1380`) reach for
/// `yyerror(NULL)` when the record can be skipped and the rest of the file
/// still read, and for `yyerrorAbort` only when the parse cannot continue
/// — an unknown record type or a `dbCreateRecord` that failed for any
/// other reason. The port had one class, so the first skippable record
/// discarded every record after it.
enum RecordFault {
    /// C `yyerror(NULL)`: report, skip this record, keep loading.
    Recoverable(String),
    /// C `yyerrorAbort`: stop the load here.
    Fatal(String),
}

/// Install each parsed record definition into the database through the
/// one routine both `dbLoadRecords` and `dbLoadTemplate` share.
///
/// This is the extracted body of the old `dbLoadRecords` install loop:
/// duplicate-name merge, field application, the creation sink's
/// load-then-init ordering, alias registration, and the post-load link /
/// SIMM / watchdog init passes — all identical for both commands so a
/// template-loaded record is byte-for-byte the same as a directly loaded
/// one. On the first rejected record it prints the C-visible diagnostic
/// and propagates the error to the iocsh script chain (epics-base
/// 144f975), exactly as the inline loop did.
async fn install_record_defs(
    ctx: &CommandContext,
    defs: Vec<db_loader::DbRecordDef>,
    unresolved_aliases: Vec<(String, String)>,
    breaktable_registry: &crate::server::cvt_bpt::BreakTableRegistry,
    faults: &mut db_loader::DbFaults,
) -> Result<(), DbReadFailure> {
    // Non-fatal per-record failures (alias reject, merge-field put) join
    // whatever the parse itself recovered from, in the one owner that
    // decides what recoverable means: the load continues past them, but
    // the command must still end in Err so `on error` fires — C's
    // dbLoadRecords returns non-zero after its parser recovered and kept
    // going (epics-base#498 / UI-105).
    for mut def in defs {
        // C `dbRecordHead` (`dbLexRoutines.c:1136-1157`) reads two record
        // types as instructions rather than types: `*` modifies the record
        // already in the database and `#` deletes it. Both skip the body
        // when the name is unknown (`duplicate = TRUE`), and they report
        // it differently — `*` with `yyerror(NULL)`, `#` with a bare
        // WARNING that leaves the load's status clean. The port passed
        // both to the record factory, so every such block failed as an
        // unknown record type and took the load with it.
        let modify_block = def.record_type == "*";
        if modify_block {
            let existing = ctx
                .db()
                .get_record(&def.name)
                .map(|rec| rec.read().record.record_type().to_string());
            match existing {
                // Continue as the type already there: from here on this is
                // the same field merge a same-type reload performs, which
                // is what C's pdbentry does after `dbFindRecord`.
                Some(t) => def.record_type = t,
                None => {
                    faults.recoverable(format!("{ERL_ERROR}: Record '{}' not found", def.name));
                    continue;
                }
            }
        }
        if def.record_type == "#" {
            if !ctx.db().remove_record(&def.name).await {
                // C also names the file and line here; the port does not
                // carry either as far as the install loop.
                ctx.eprintln(&format!(
                    "{ERL_WARNING}: Record '{}' not found, can't delete",
                    def.name
                ));
            }
            continue;
        }

        // Resolve a `LINR` field naming a loaded breakpoint table to its
        // menuConvert index (shared with the IocBuilder load path).
        db_loader::resolve_linr_breaktable_names(
            &def.record_type,
            &mut def.fields,
            breaktable_registry,
        );

        // C creates the record and only THEN puts each field
        // (`dbCreateRecord` at `dbLexRoutines.c:1172`, `dbPutString` at
        // `:1405`), so a value `dbPutString` refuses costs that FIELD its
        // value and nothing else: the record stays and keeps the default,
        // its other fields load, the rest of the file is read, and the
        // load's status goes non-zero only at the end (`yyerror`, `:1415`).
        //
        // Screening here, once, before the record exists, is what makes that
        // uniform. The refusal used to be decided at whichever site happened
        // to apply the field — `add_loaded_record` for a dbCommon field,
        // `apply_fields` for a record-own one — which gave one rule two
        // spellings: `SCAN` reported C's wording and aborted the file while
        // `SELM` reported the port's own wording and continued, and both
        // discarded a record C keeps. After the screen neither apply site can
        // see a value its menu would refuse, so neither can decide anything.
        let (screen_type, screen_name) = (def.record_type.clone(), def.name.clone());
        def.fields.retain(|f| {
            let Some(refusal) = db_loader::menu_value_refusal(
                &screen_type,
                &screen_name,
                &f.name.to_uppercase(),
                &f.value.as_str_lossy(),
            ) else {
                return true;
            };
            // C refuses this value inside the parse, so `yyerror` names the
            // line from the lexer. Here the parse is over, so the element's
            // own recorded line takes the lexer's seat — see
            // [`db_loader::DbFieldDef`]. Without it every refusal below
            // would print the file with no line at all, which is what an
            // operator debugging a real `.db` is left with.
            faults.seek(f.line, ")");
            // C's three lines around a refusal go out in one call, because
            // their ORDER is C's — see [`db_loader::DbDiagnostic`].
            faults.report(db_loader::DbDiagnostic {
                notice: refusal.notice,
                message: refusal.line,
                suggestion: refusal.suggestion,
            });
            false
        });

        let added: Result<(), RecordFault> = async {
            // C-parity (dbLexRoutines.c:1170-1188): the SAME
            // record name re-loaded with the SAME record_type
            // merges fields into the existing instance (the
            // standard ADCore convention — simDetector.template
            // overrides ColorMode menu choices declared by its
            // included NDArrayBase.template). A different
            // record_type is skipped with `yyerror(NULL)`, and the
            // diagnostic names the type being LOADED first and the
            // type already there last — `recordType` is the new one
            // and `dbGetRecordTypeName(pdbentry)` the existing one
            // (`dbLexRoutines.c:1173-1180`).
            let existing = if let Some(rec) = ctx.db().get_record(&def.name) {
                let r = rec.read();
                let existing_type = r.record.record_type();
                if existing_type != def.record_type {
                    return Err(RecordFault::Recoverable(format!(
                        "{ERL_ERROR}: {} record '{}' already exists, can't load {} record",
                        def.record_type, def.name, existing_type
                    )));
                }
                drop(r);
                // `dbRecordsOnceOnly` (`dbLexRoutines.c:1181-1187`) refuses
                // the SECOND declaration once the type matched, and sets
                // `duplicate` so the body is dropped whole — fields, info
                // tags and aliases alike. Returning here is that drop.
                //
                // A `record("*", …)` block is exempt because C's `*` arm
                // returns from `dbRecordHead` before `dbCreateRecord`
                // (`:1136-1144`), so it never reaches the flag: modifying
                // an existing record is what `*` is FOR, and refusing it
                // would make the knob mean two different things.
                if db_records_once_only() && !modify_block {
                    return Err(RecordFault::Recoverable(format!(
                        "{ERL_ERROR}: Record '{}' already defined; dbRecordsOnceOnly is set,\n  \
                         so can't modify record.",
                        def.name
                    )));
                }
                Some(rec)
            } else {
                None
            };

            let mut common_fields = Vec::new();
            let is_merge = existing.is_some();
            let rec_arc = if let Some(rec_arc) = existing {
                // Merge: apply field overrides directly to the
                // existing record instance.
                {
                    let mut inst = rec_arc.write();
                    if let Err(e) =
                        db_loader::apply_fields(&mut inst.record, &def.fields, &mut common_fields)
                    {
                        // C `dbRecordField` ends every one of its failure
                        // arms in `yyerror(NULL)` — a bad field name or an
                        // unconvertible value skips that record, never the
                        // file (`dbLexRoutines.c:1206-1380`).
                        return Err(RecordFault::Recoverable(format!("{e}")));
                    }
                }
                rec_arc
            } else {
                // C `dbFindRecordType` failing is `yyerrorAbort`
                // (`dbLexRoutines.c:1159-1165`) — an unknown record type
                // stops the load on both sides.
                let mut record = db_loader::create_record(&def.record_type)
                    .map_err(|e| RecordFault::Fatal(format!("{e}")))?;
                // The breakpoint-table registry is installed by the
                // creation sink; apply_fields only needs the LINR
                // index, already resolved above.
                if let Err(e) =
                    db_loader::apply_fields(&mut record, &def.fields, &mut common_fields)
                {
                    return Err(RecordFault::Recoverable(format!("{e}")));
                }
                // Record + its whole loaded field set in one call: the
                // sink applies the common fields and info tags, THEN
                // runs C's `iocInit` passes, so the initial UDF
                // severity sees the `.db`'s final UDF (a
                // `field(VAL,…)` clears it — `dbStaticLib.c:2653`).
                let load = RecordLoad {
                    common_fields: std::mem::take(&mut common_fields),
                    info_tags: def.info_tags.clone(),
                };
                if let Err(e) = ctx.db().add_loaded_record(&def.name, record, load).await {
                    // C `dbCreateRecord` failing for anything other than
                    // `S_dbLib_recExists` is `yyerrorAbort`
                    // (`dbLexRoutines.c:1187-1192`).
                    return Err(RecordFault::Fatal(format!(
                        "dbLoadRecords: '{}' rejected: {e}",
                        def.name
                    )));
                }
                ctx.db().get_record(&def.name).ok_or_else(|| {
                    RecordFault::Fatal(format!(
                        "dbLoadRecords: '{}' vanished between add_record and get_record",
                        def.name
                    ))
                })?
            };

            // Register any aliases declared in the record body
            // (epics-base PR #336). Failures are reported but
            // don't abort the load — the record is already in.
            // For a merge, aliases declared in the new block
            // are also registered (C parser appends).
            for alias in &def.aliases {
                install_alias(ctx, alias, &def.name, faults).await;
            }

            // A MERGE re-applies the new block's fields to a record that
            // is already in the database and already initialised, so
            // the load-then-init ordering the creation sink guarantees
            // has to be re-created by hand here: fields first, passes
            // after. A fresh record took that ordering from the sink
            // and must not run the passes twice.
            if is_merge {
                {
                    let mut instance = rec_arc.write();
                    // info(key, value) directives — last write
                    // wins. Populated before common-field application
                    // so device support seeing `init_record` can
                    // observe info tags.
                    for (k, v) in &def.info_tags {
                        instance.set_info(k, v);
                    }
                }
                for (name, value) in common_fields {
                    use crate::server::record::CommonFieldPutResult;
                    // `.db` load: C's loader converter, whose menu
                    // bound differs from a runtime dbPut's
                    // (`dbStaticRun.c::dbPutStringNum`).
                    // The record data lock is scoped per field so it is
                    // down before `update_scan_index` re-enters the
                    // database — the same rule as `field_io`'s put path;
                    // the data lock is not the processing-exclusion
                    // mechanism, so the per-field release is a bounded,
                    // `.await`-free window.
                    let put = rec_arc.write().put_common_field_db_load(&name, value);
                    match put {
                        Ok(CommonFieldPutResult::ScanChanged {
                            old_scan,
                            new_scan,
                            phas,
                        }) => {
                            ctx.db()
                                .update_scan_index(&def.name, old_scan, new_scan, phas, phas);
                        }
                        Ok(CommonFieldPutResult::PhasChanged {
                            scan,
                            old_phas,
                            new_phas,
                        }) => {
                            ctx.db()
                                .update_scan_index(&def.name, scan, scan, old_phas, new_phas);
                        }
                        Ok(CommonFieldPutResult::NoChange) => {}
                        Err(e) => {
                            faults.recoverable(format!(
                                "put_common_field({name}) failed for {}: {e}",
                                def.name
                            ));
                        }
                    }
                }
                // TODO: refactor to global two-pass if inter-record init dependencies arise.
                // C `iocInit` calls init_record once per record AFTER
                // all dbLoadRecords blocks, so init_record always
                // sees the final merged field set. Rust shortcuts by
                // running init_record inline at dbLoadRecords; on a
                // merge we re-run it so the new field values
                // (LINR / ESLO / ZRST / ...) take effect in
                // convert routines and post-init derived state.
                // The cost: stateful records (compress accum,
                // first_output_done) get re-initialised. The
                // alternative (skip init on merge) silently
                // ignored field overrides that affect init —
                // worse for typical use.
                rec_arc.write().run_init_passes(&def.name);
            }
            {
                // Hand the record its resolved common link fields so
                // a link-classifying record (calcout INAV..INUV/OUTV)
                // runs its C `init_record` checkLinks step at load —
                // the common OUT link is applied by the sink, after
                // `set_async_context`. Defaulted no-op for records
                // that do not classify common links.
                let mut instance = rec_arc.write();
                let inst = &mut *instance;
                inst.record.init_links(&inst.common);
            }
            // C `recGblInitConstantLink(&prec->inp, …)` /
            // `dbLoadLinkArray` from every soft INPUT dev support's
            // `init_record` — the only site that loads a constant INP
            // into the record's value.
            ctx.db().rec_gbl_init_constant_links(&rec_arc);
            if is_merge {
                // A merge re-ran `run_init_passes` above against the new
                // field set, so the rest of pass 1 owes a re-run too: a
                // second block may have introduced SIML/SIOL or moved SDEL.
                // A FRESH record takes both from the creation sink
                // (`PvDatabase::add_loaded_record`) and must not repeat them
                // here — `arm_watchdog` would spawn a second task and
                // supersede the first for nothing.
                ctx.db().rec_gbl_init_simm(&rec_arc);
                ctx.db().arm_watchdog(&def.name);
            }
            Ok(())
        }
        .await;
        match added {
            Ok(()) => {}
            // The load's status still goes non-zero (epics-base 144f975:
            // the `iocshSetError` equivalent, so a startup script fails),
            // but the records already installed stay and the ones after
            // this record are still read.
            Err(RecordFault::Recoverable(msg)) => faults.recoverable(msg),
            Err(RecordFault::Fatal(msg)) => {
                ctx.println(&msg);
                return Err(DbReadFailure::Rejected);
            }
        }
    }
    // File-scope `alias("record","new")` whose target this file does not
    // declare. C `dbAlias` (`dbLexRoutines.c:1508`) resolves against
    // `savedPdbbase`, so a record installed by an earlier
    // `dbLoadRecords` is a legal target; running after the loop above
    // means this file's own records are equally visible. A target
    // nothing owns gets C's diagnostic and fails the call's status
    // through the fault owner, leaving every record that parsed installed.
    for (target, alias) in unresolved_aliases {
        if ctx.db().get_record(&target).is_none() {
            faults.recoverable(db_loader::unknown_alias_message(&alias, &target));
            continue;
        }
        install_alias(ctx, &alias, &target, faults).await;
    }

    if faults.is_empty() {
        Ok(())
    } else {
        Err(DbReadFailure::Rejected)
    }
}

/// `dbLoadTemplate(subFile [, globalMacros])` — load records described by
/// a `.substitutions` file. Mirrors EPICS base `dbLoadTemplate`, which
/// drives `dbLoadRecords` once per substitution row: `subFile` names the
/// `.substitutions` file; the optional `globalMacros` ("A=1,B=2") applies
/// to every row, with each row's own substitutions taking precedence over
/// a global of the same name.
///
/// Precedence is grounded in the reused loader: `emit_load`
/// (db_loader/substitution.rs:423) builds each row's macro set as
/// `globals` first then the row appended, and `load_substitution_file`
/// (:449) inserts caller macros then per-row (globals + row) into a
/// last-definition-wins map, so a row macro overrides the global — the C
/// `dbLoadTemplate` order.
fn cmd_db_load_template() -> CommandDef {
    CommandDef::new(
        "dbLoadTemplate",
        vec![
            ArgDesc {
                // C `dbLoadTemplateArg0` (`dbtoolsIocRegister.c:16`).
                name: "subFile",
                arg_type: ArgType::Path,
            },
            ArgDesc {
                name: "var1=value1,var2=value2",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "path1:path2:...",
                arg_type: ArgType::String,
            },
        ],
        "dbLoadTemplate subFile [globalMacros] [path] - Load records from a .substitutions file",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `dbLoadTemplate.y:344-347` diagnoses a missing or empty
            // name itself, on stderr, and returns -1 —
            // `dbLoadTemplateCallFunc` passes that status to `iocshSetError`
            // (`dbtoolsIocRegister.c:33-36`), so the line FAILED.
            let path = match &args[0] {
                ArgValue::String(s) if !s.is_empty() => s,
                _ => {
                    ctx.eprintln("must specify variable substitution file");
                    return Ok(CommandOutcome::Failed);
                }
            };
            let macros_str = match &args[1] {
                ArgValue::String(s) => s.as_str(),
                _ => "",
            };
            let search_path = match &args[2] {
                ArgValue::String(s) => s.as_str(),
                _ => "",
            };

            // Same load-phase gate as `dbLoadRecords`: opens the load phase
            // (idempotent across the several loads a script issues) and
            // refuses with C's diagnostic once `iocInit` has run — C's
            // `dbLoadTemplate` delegates to `dbReadDatabase`/`dbLoadRecords`,
            // which fail identically after init (dbAccess.c:808-812).
            if ctx.db().begin_load().is_err() {
                ctx.eprintln(&format!("{ERL_ERROR}: Failed to load '{path}'"));
                ctx.eprintln("    Records cannot be loaded after iocInit!");
                return Ok(CommandOutcome::Failed);
            }

            let macros = parse_macro_string(macros_str);

            let (config, file_path) = resolve_substitutions_file(path, search_path);

            // C `dbLoadTemplate` issues one `dbLoadRecords` per row from
            // the `pattern_definition` action (`dbLoadTemplate.y:186`), so
            // each row's records are committed before the next row is even
            // read and a failing row costs only the rows after it. Resolve,
            // parse and install one row at a time for the same reason: the
            // port used to concatenate every row's records and install the
            // batch, which threw away the rows that had already succeeded.
            let rows = db_loader::substitution_rows(&file_path, &macros)
                .map_err(|e| format!("parse error: {e}"))?;

            for (file, merged) in rows {
                let template = db_loader::resolve_template(&file, &config.include_paths)
                    .map_err(|e| format!("parse error: {e}"))?;
                // Reported by the loader, like `dbLoadRecords`'s own read
                // above; the row's summary is all that is owed here, and
                // C names the row's `.db` in it rather than the
                // `.substitutions` (`dbLoadTemplate.y:186` ->
                // `dbAccess.c:808`).
                let Ok(parsed) =
                    db_loader::parse_db_opened_with_breaktables(&template, &merged, &config)
                else {
                    ctx.eprintln(&format!("{ERL_ERROR}: Failed to load '{file}'"));
                    return Ok(CommandOutcome::Failed);
                };
                // Each row IS a `dbLoadRecords`, so its `breaktable(...)`
                // definitions join the database registry exactly as that
                // command's do, and a later row's `LINR` name resolves
                // against them.
                let breaktable_registry =
                    ctx.block_on(async { ctx.db().add_breaktables(parsed.breaktables).await });
                // Identical install path to `dbLoadRecords`: same
                // duplicate-name merge, field application, load-then-init
                // ordering and post-load passes, so a template-loaded record
                // is indistinguishable from a directly loaded one.
                let mut faults = parsed.faults;
                if ctx
                    .block_on(install_record_defs(
                        ctx,
                        parsed.records,
                        parsed.unresolved_aliases,
                        &breaktable_registry,
                        &mut faults,
                    ))
                    .is_err()
                {
                    // Each row IS a `dbLoadRecords`, so C's summary names the
                    // row's own file (`dbLoadTemplate.y:186` ->
                    // `dbAccess.c:808`), not the `.substitutions`.
                    ctx.eprintln(&format!("{ERL_ERROR}: Failed to load '{file}'"));
                    return Ok(CommandOutcome::Failed);
                }
            }

            // Silent on success like the `dbLoadRecords` each row is:
            // C's `dbLoadTemplate` returns `yyparse`'s status and its only
            // progress prints are `#ifdef ERROR_STUFF`
            // (`dbLoadTemplate.y:92-94`, not defined in a normal build).
            Ok(CommandOutcome::Continue)
        },
    )
}

fn cmd_epics_env_set() -> CommandDef {
    CommandDef::new(
        "epicsEnvSet",
        vec![
            ArgDesc {
                name: "name",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "value",
                arg_type: ArgType::String,
            },
        ],
        "epicsEnvSet name value - Set an environment variable",
        |args: &[ArgValue], _ctx: &CommandContext| {
            let name = match &args[0] {
                ArgValue::String(s) => s,
                _ => return Err("invalid argument".to_string()),
            };
            let value = match &args[1] {
                ArgValue::String(s) => s,
                _ => return Err("invalid argument".to_string()),
            };

            // C `epicsEnvSet` (`osdEnv.c:47-52`) clears the shell macro
            // of the same name FIRST, so a caller-installed
            // `iocshLoad("inner.cmd","PORT=OLD")` macro stops shadowing
            // the variable the loaded script is setting.
            super::iocsh_env_clear(name);
            // SAFETY: We're single-threaded in the REPL, and this matches C EPICS behavior
            unsafe { std::env::set_var(name, value) };
            Ok(CommandOutcome::Continue)
        },
    )
}

fn cmd_ioc_init() -> CommandDef {
    CommandDef::new(
        "iocInit",
        vec![],
        "iocInit - Initialize the IOC",
        |_args: &[ArgValue], ctx: &CommandContext| {
            // C `iocInit` is `iocBuild() || iocRun()` (`iocInit.c:111-113`),
            // and so is this: the same two halves the `iocBuild` and `iocRun`
            // commands drive, in the same order, over the same owner. THE
            // build, not a stand-in for it — the next line of the `st.cmd`, a
            // `dbpf`, a `dbl`, an `asSetFilename`, runs against an IOC with
            // device support, scan threads and PINI behind it. See
            // [`crate::server::ioc_app::IocLifecycle`] for the invariant.
            match crate::server::ioc_app::build_from_shell(ctx.bridge()) {
                crate::server::ioc_app::ShellTransition::Done => {
                    let _ = crate::server::ioc_app::run_from_shell(ctx.bridge());
                    return Ok(CommandOutcome::Continue);
                }
                crate::server::ioc_app::ShellTransition::Failed => {
                    return Ok(CommandOutcome::Failed);
                }
                // C `iocBuild_1` (`iocInit.c:116-121`) refuses from any state
                // but `iocVoid`, and `iocInit` is that refusal's caller.
                crate::server::ioc_app::ShellTransition::Refused => {
                    crate::runtime::log::errlog_printf(&format!(
                        "iocBuild: {} IOC can only be initialized from \
                         uninitialized or stopped state\n",
                        if crate::runtime::log::errlog_console_paints() {
                            crate::runtime::log::ERL_ERROR
                        } else {
                            "ERROR"
                        }
                    ));
                    return Ok(CommandOutcome::Failed);
                }
                crate::server::ioc_app::ShellTransition::NotOurs => {}
            }
            // No `IocApplication` build to drive: every `CaServerBuilder`
            // binary and every bare `PvDatabase` shell reaches here, and for
            // them `iocInit` has only ever meant closing the record-load
            // phase. A link that forward-references a record loaded by a LATER
            // `dbLoadRecords` in the same `st.cmd` must still classify as a
            // local PV (R18-92), which is what that close runs.
            ctx.block_on(async { ctx.db().ioc_init().await });
            ctx.println("iocInit: record initialization complete (scan/device init follows)");
            Ok(CommandOutcome::Continue)
        },
    )
}

fn cmd_exit() -> CommandDef {
    CommandDef::new(
        "exit",
        vec![],
        "exit - Exit the IOC shell",
        |_args: &[ArgValue], _ctx: &CommandContext| Ok(CommandOutcome::Exit),
    )
}

/// C-style escape a CHAR-array buffer for `dbgf` output (epics-base
/// dc70dfd6). Printable ASCII passes through; well-known C escapes
/// (`\n`, `\t`, `\r`, `\\`, `\"`, `\a`, `\b`, `\f`, `\v`) get their
/// short form; everything else is rendered as `\xNN`. Mirrors
/// `epicsStrnEscapedFromRaw` exactly enough for the dbgf use case —
/// the surrounding quote pair is added by the caller.
fn escape_char_array_for_dbgf(buf: &[u8]) -> String {
    let mut out = String::with_capacity(buf.len());
    for &b in buf {
        match b {
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            b'\r' => out.push_str("\\r"),
            b'\\' => out.push_str("\\\\"),
            b'"' => out.push_str("\\\""),
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            0x0b => out.push_str("\\v"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out
}

/// Split an IOC macro definition string into `(name, value)` pairs the
/// way libCom `macParseDefns` does (`macUtil.c:74-196`): commas separate
/// pairs and `=` separates a name from its value, but a separator inside
/// single/double quotes or escaped with a backslash is a literal, and
/// unquoted whitespace around names and values is trimmed. A name with
/// no `=` (e.g. `,FOO,`) is a deletion and yields `None`.
///
/// Quotes and escapes are stripped from both the name and the value. C
/// strips them from names in `macParseDefns` and from values later in
/// `macExpandString`; this port substitutes the value directly with no
/// second `macExpandString` pass, so both are stripped here to reach the
/// same observable substitution.
///
/// Exposed (re-exported as `iocsh::macro_defn_pairs`) so that other macLib
/// consumers — e.g. QSRV's `dbLoadGroup` macro parser — split definition
/// strings through this one owner of the `macParseDefns` grammar instead
/// of a second raw `split(',')` that would tear a quoted value on an
/// embedded comma. Callers that defer `$(...)` expansion to their own
/// `macExpandString` equivalent use these raw split pairs directly and do
/// NOT run `parse_macro_string`, which additionally substitutes the
/// environment eagerly.
pub fn macro_defn_pairs(s: &str) -> Vec<(String, Option<String>)> {
    #[derive(PartialEq, Clone, Copy)]
    enum St {
        PreName,
        InName,
        PreValue,
        InValue,
    }
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut state = St::PreName;
    let mut name = String::new();
    let mut value = String::new();
    // Unquoted whitespace seen mid-token: buffered so trailing whitespace
    // before a delimiter is dropped while interior whitespace is kept.
    let mut pending_ws = String::new();
    let mut quote: Option<char> = None;

    // Enter the token from a "pre" state if needed and report whether it
    // is a VALUE: C removes quotes and escapes from names in place
    // "(unlike values, they will not be re-parsed)" (`macUtil.c:198-200`),
    // so a value keeps them for the expander's `discard` to strip.
    macro_rules! enter_token {
        () => {{
            match state {
                St::PreName => state = St::InName,
                St::PreValue => state = St::InValue,
                _ => {}
            }
            matches!(state, St::InValue)
        }};
    }

    // Append a literal char to the token for the current state, entering
    // the token from a "pre" state if needed and flushing buffered ws.
    macro_rules! push_lit {
        ($c:expr) => {{
            match state {
                St::PreName => state = St::InName,
                St::PreValue => state = St::InValue,
                _ => {}
            }
            let target = if matches!(state, St::InName) {
                &mut name
            } else {
                &mut value
            };
            target.push_str(&pending_ws);
            pending_ws.clear();
            target.push($c);
        }};
    }

    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];

        // Escape: `\X` makes `X` a literal (and not a delimiter).
        // Quotes do not suppress escapes.
        if c == '\\' && i + 1 < chars.len() {
            if enter_token!() {
                push_lit!('\\');
            }
            push_lit!(chars[i + 1]);
            i += 2;
            continue;
        }

        // Inside a quote: every char is literal until the matching quote.
        if let Some(q) = quote {
            if c == q {
                quote = None;
                if enter_token!() {
                    push_lit!(c);
                }
            } else {
                push_lit!(c);
            }
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            // An opening quote also begins the token (e.g. `=""`).
            if enter_token!() {
                push_lit!(c);
            }
            i += 1;
            continue;
        }

        match state {
            St::PreName => {
                if c == '=' {
                    state = St::PreValue;
                } else if !(crate::runtime::stdlib::c_isspace(c) || c == ',') {
                    state = St::InName;
                    name.push(c);
                }
                // leading whitespace and bare commas: skip
            }
            St::InName => {
                if c == '=' {
                    pending_ws.clear();
                    state = St::PreValue;
                } else if c == ',' {
                    // name with no '=' → deletion
                    pending_ws.clear();
                    out.push((std::mem::take(&mut name), None));
                    state = St::PreName;
                } else if crate::runtime::stdlib::c_isspace(c) {
                    pending_ws.push(c);
                } else {
                    name.push_str(&pending_ws);
                    pending_ws.clear();
                    name.push(c);
                }
            }
            St::PreValue => {
                if c == ',' {
                    out.push((std::mem::take(&mut name), Some(String::new())));
                    state = St::PreName;
                } else if !crate::runtime::stdlib::c_isspace(c) {
                    state = St::InValue;
                    value.push(c);
                }
                // leading value whitespace: skip
            }
            St::InValue => {
                if c == ',' {
                    pending_ws.clear();
                    out.push((std::mem::take(&mut name), Some(std::mem::take(&mut value))));
                    state = St::PreName;
                } else if crate::runtime::stdlib::c_isspace(c) {
                    pending_ws.push(c);
                } else {
                    value.push_str(&pending_ws);
                    pending_ws.clear();
                    value.push(c);
                }
            }
        }
        i += 1;
    }

    // Flush the token open at end of string.
    match state {
        St::PreName => {}
        St::InName => out.push((std::mem::take(&mut name), None)),
        St::PreValue => out.push((std::mem::take(&mut name), Some(String::new()))),
        St::InValue => out.push((std::mem::take(&mut name), Some(std::mem::take(&mut value)))),
    }
    out
}

/// Parse a macro string like "P=IOC:,R=TEMP" into a HashMap.
///
/// C `macParseDefns` (`macUtil.c`) receives text `macDefExpand` has
/// already expanded and never consults the environment itself, so the
/// values land here verbatim — expanding them again would resolve a
/// `$(X)` the caller deliberately quoted through.
///
/// Splitting honors quotes/escapes and trims whitespace via
/// [`macro_defn_pairs`] so `DESC="a,b",P=IOC:` keeps `a,b` as one value
/// instead of tearing it on the embedded comma (libCom `macParseDefns`
/// parity). Deletion entries (a name with no `=`) have nothing to remove
/// from a fresh map and are skipped.
pub(super) fn parse_macro_string(s: &str) -> HashMap<String, String> {
    let mut macros = HashMap::new();
    for (k, v) in macro_defn_pairs(s) {
        if let Some(v) = v {
            if k.is_empty() {
                continue;
            }
            macros.insert(k, v);
        }
    }
    macros
}

// ---------------------------------------------------------------------------
// The `db*` report and state commands C registers from
// `dbIocRegister.c:587-645` and `dbStaticIocRegister.c:265-280` — the spans
// at `R7.0.10`, where the latter file is 281 lines. Only the ones the
// port has a data structure to answer from are here; the rest are enumerated,
// with the missing structure named, in `register_builtins` below.
// ---------------------------------------------------------------------------

/// C's leading `pdbbase` argument (`iocshArgPdbbase`, declared as `argPdbbase`
/// in `dbStaticIocRegister.c:21`): `cvtArg` (`iocsh.cpp:872-884`) accepts it
/// missing, starting with `0`, or spelled `pdbbase`, and refuses anything else.
/// Every `dbStaticIocRegister.c` command that carries the argument gets the
/// same treatment, so the check is shared rather than re-inlined per command.
pub(super) fn check_pdbbase(arg: &ArgValue) -> Result<(), String> {
    if let ArgValue::String(pdbbase) = arg
        && !(pdbbase.is_empty() || pdbbase.starts_with('0') || pdbbase == "pdbbase")
    {
        return Err(format!("Expecting 'pdbbase' got '{pdbbase}'."));
    }
    Ok(())
}

/// C `printf("%e", v)`: six fraction digits and a signed exponent of at least
/// two digits. Rust's `{:.6e}` writes the digits but spells the exponent `e2`
/// where C writes `e+02`, so `dbDumpBreaktable` (`dbStaticLib.c:3547-3548`)
/// needs the fixup to emit C's bytes.
fn c_exponential(v: f64) -> String {
    if !v.is_finite() {
        return format!("{v}");
    }
    let formatted = format!("{v:.6e}");
    let (mantissa, exponent) = formatted
        .split_once('e')
        .expect("Rust LowerExp always emits an `e`");
    let exponent: i32 = exponent.parse().expect("Rust LowerExp emits a decimal");
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exponent.abs())
}

/// The `DBF_INLINK` / `DBF_OUTLINK` / `DBF_FWDLINK` column `dblsr` prints for
/// one link field (`dbLock.c:891-897`).
///
/// C reads `pdbFldDes->field_type`, so the word is a property of the
/// DECLARATION and cannot be recovered from the field's name: `LNK1` is
/// `DBF_FWDLINK` on a `fanout` and `DBF_OUTLINK` on a `seq`, and `OUTA` is
/// `DBF_OUTLINK` on both `dfanout` and `aSub`. A name list answered `INLINK`
/// for all four.
///
/// `None` for a field no `.dbd` declares as a link, which is also the only
/// way C omits the row: no `dbFldDes`, no entry in `papFldDes`, nothing to
/// print.
fn link_field_kind(record_type: &str, field: &str) -> Option<&'static str> {
    use crate::types::DbfLinkClass;
    Some(match crate::types::dbf_link_class(record_type, field)? {
        DbfLinkClass::InLink => "\t INLINK",
        DbfLinkClass::OutLink => "\tOUTLINK",
        DbfLinkClass::FwdLink => "\tFWDLINK",
    })
}

/// One member record's `level >= 2` block: every DB link it holds
/// (`dbLock.c:886-900`).
///
/// Only a link that resolved LOCALLY appears, because C reads
/// `plink->type != DB_LINK` and skips everything else — a `ca://` target is a
/// `CA_LINK` and prints nothing here even when the name is local.
fn dblsr_link_lines(db: &crate::server::database::PvDatabase, record: &str) -> Vec<String> {
    use crate::server::record::ParsedLink;
    let Some(record_type) = db
        .get_record(record)
        .map(|rec| rec.read().record.record_type())
    else {
        return Vec::new();
    };
    db.record_link_fields(record)
        .into_iter()
        .filter_map(|(field, _, parsed)| match parsed {
            ParsedLink::Db(link) => {
                // The addressed record, not the raw half: `resolve_alias`
                // and `get_record_no_resolve` both miss on `src.[2]`, and the
                // `?` below then dropped every filtered link from the report.
                let addressed = link.target().record;
                let target = db
                    .resolve_alias(&addressed)
                    .unwrap_or_else(|| addressed.clone());
                db.get_record_no_resolve(&target)?;
                let pp = if link.policy == crate::server::record::LinkProcessPolicy::ProcessPassive
                {
                    " PP"
                } else {
                    "NPP"
                };
                Some(format!(
                    "\t{field}{} {pp} {} {target}",
                    link_field_kind(record_type, &field)?,
                    crate::server::record::record_instance::monitor_switch_word(
                        link.monitor_switch
                    )
                ))
            }
            _ => None,
        })
        .collect()
}

/// One lock set's block, header first — `dblsr`'s loop body
/// (`dbLock.c:875-901`).
fn dblsr_set_lines(
    db: &crate::server::database::PvDatabase,
    set: &crate::server::database::LockSetInfo,
    level: i64,
) -> Vec<String> {
    let mut lines = vec![format!(
        "Lock Set {} {} members {} refs epicsMutexId {}",
        set.id,
        set.members.len(),
        set.refs,
        // C prints the `epicsMutexId` with `%p`; this is the same identity the
        // process mutex list carries for that mutex, so `dbLockShowLocked` and
        // `epicsMutexShowAll` name the set with the same number.
        set.mutex
            .as_ref()
            .map_or_else(|| "(nil)".to_string(), |m| format!("{:#x}", m.addr()))
    )];
    if level == 0 {
        return lines;
    }
    for member in &set.members {
        lines.push(member.clone());
        if level <= 1 {
            continue;
        }
        lines.extend(dblsr_link_lines(db, member));
    }
    lines
}

/// `dblsr [record name] [interest level]` — Database Lockset report.
///
/// C `dblsr` (`dbLock.c:871-909`), registered at `dbIocRegister.c:624`.
///
/// A record name selects that record's set alone; `*`, `""` and a missing
/// argument select every active set, which is C's own normalisation
/// (`dbLock.c:875-876`). A name no record answers to prints `Record not
/// found`, and a record that has no lock set yet — the database is loaded but
/// `iocInit` has not run — prints nothing at all (`:900-901`).
fn cmd_dblsr() -> CommandDef {
    CommandDef::new(
        "dblsr",
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
        "dblsr [record name] [interest level] — Database Lockset report.\n\
         Generate a report showing the lock set to which each record belongs.\n\
         interest level 0 - Show lock set information only.\n\
         \x20              1 - Show each record in the lock set.\n\
         \x20              2 - Show each record and all database links in the lock set.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let name = match &args[0] {
                ArgValue::String(s) if s != "*" && !s.is_empty() => Some(s.clone()),
                _ => None,
            };
            let level = match args[1] {
                ArgValue::Int(v) => v,
                _ => 0,
            };
            let db = ctx.db();

            let sets = match name {
                Some(name) => {
                    if db.get_record(&name).is_none() {
                        ctx.println("Record not found");
                        return Ok(CommandOutcome::Continue);
                    }
                    // Before `iocInit` the record exists but has no set, and C
                    // returns without printing a header.
                    match db.lock_set_of(&name) {
                        Some(set) => vec![set],
                        None => return Ok(CommandOutcome::Continue),
                    }
                }
                None => db.lock_set_report().active,
            };

            for set in &sets {
                for line in dblsr_set_lines(db, set, level) {
                    ctx.println(&line);
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbLockShowLocked [interest level]` — show lock sets that are locked.
///
/// C `dbLockShowLocked` (`dbLock.c:911-941`), registered at
/// `dbIocRegister.c:625`.
///
/// Two passes over the active list, exactly as C makes them: the first prints
/// `listTypeScanLock` and then only the sets whose mutex cannot be taken, the
/// second prints `listTypeRecordLock` and then every set. Both headers appear
/// whenever the list is non-empty, even when no row follows — which is what an
/// idle IOC prints, and why the first pass usually shows a bare header.
///
/// The rows are `epicsMutexShow`'s, so the `interest level` is that function's
/// and adds the OSD line above 0.
/// `dbNotifyDump` — C `dbNotifyDump` (`dbNotify.c:627-689`) behind
/// `dbNotifyDumpCallFunc` (`dbIocRegister.c:376`).
///
/// One block per record a `processNotify` owns right now; an IOC with no put
/// in flight prints nothing at all. Measured on `softIoc` R7.0.10-146 with a
/// `calcout` holding `ODLY 10`, one `dbtpn` outstanding and then two:
///
/// ```text
/// epics> dbNotifyDump
/// N:S state 4 ppn 0x57e6f3f757e0
///   waitList
///     N:S pact 1
/// N:S restartList
///     N:S
/// ```
///
/// Three things here are ours rather than C's, and each is a datum the port
/// does not keep:
///
/// * **`state`** is C's seven-value `notifyState` enum printed raw. The
///   port's notify slot is occupied over exactly C's states 3-6 and it never
///   enters 3: C's `processNotifyCommon` attaches a notify to a record that
///   is already PACT (`dbNotify.c:225-231`,
///   `notifyRestartInProgress`), where the port queues it on the record's
///   restart list instead, so the two states left are
///   `notifyProcessInProgress` (4) while the chain runs and
///   `notifyUserCallbackRequested` (5) once the wait-set has settled and the
///   client callback is pending. States 1 and 2 are unreachable in a block
///   head in C too — a notify in `notifyWaitForRestart` is not
///   `precord->ppn`, and `notifyRestartCallbackRequested` is the instant
///   between `restartCheck` and the callback.
/// * **`ppn`** is C's `processNotify` address; here it is the wait-set's,
///   which is the same identity — two blocks printing one address are one
///   notify.
/// * **the completion accounting with no `processNotify` behind it**. A
///   downstream link put arms a wait-set of its own
///   (`PvDatabase::new_put_notify`) that C reaches through `dbNotifyAdd` on
///   the initiator's existing `ppn` instead. Such a set has no
///   `dbChannelRecord(ppn->chan)`, so — like C — it heads no block, and the
///   records that joined it appear here only if they also hold an entry
///   notify of their own.
///
/// The `waitList` members are exact — a member is a record whose notify slot
/// is the same `Arc` — but they come out in the walk order below rather than
/// in C's join order. The `restartList` is exact including its count: every
/// entry C prints resolves to `dbChannelRecord(ppnRestart->chan)`, which is
/// the record whose queue it sits in, so the block is that record's own name
/// repeated.
///
/// C brackets the walk in `epicsMutexTryLock(pnotifyGlobal->lock)` retried
/// 100 times at 0.05 s (`:634-639`) and dumps unlocked if it never gets it.
/// There is no global notify lock here — a notify slot is read under its own
/// record's lock — so there is nothing to try for and no unlocked-anyway arm.
/// C `dbcar` (`dbCaTest.c:54-162`), registered at `dbIocRegister.c:143-156`.
///
/// Three deviations, all forced by where the port keeps the state C keeps on
/// one `caLink`:
///
/// * **`nNoWrite` comes from the database, not from the lset.** C stages the
///   pending out-value on the `caLink` and counts the overwrite there
///   (`dbCa.c:548`, `:582`); this port stages it on
///   [`PvDatabase::external_link_puts_coalesced_for`]'s queue, keyed by
///   `(scheme, PV name)`. Two record fields naming one PV therefore share the
///   count where C gives each its own.
/// * **The `[IN IS ON OS]` columns.** `ON`/`OS` are dead at the pin — both
///   assignments sit inside `/* Disabled by ANJ ... */` (`dbCa.c:539-542`,
///   `:555-558`) — so C prints them blank too. `IS` needs C's second,
///   `DBR_STRING` monitor, which this port does not keep: it renders strings
///   from the one native monitor, so the column stays blank where C would
///   show it for an enum link read as a string.
/// * **`level > 2` prints no `ca_context_status`.** That report belongs to
///   libca's client context; `epics-base-rs` holds the link registry but not
///   the CA client, which lives in `epics-ca-rs` behind the `LinkSet`
///   boundary.
///
/// [`PvDatabase::external_link_puts_coalesced_for`]: crate::server::database::PvDatabase::external_link_puts_coalesced_for
fn cmd_dbcar() -> CommandDef {
    CommandDef::new(
        "dbcar",
        vec![
            ArgDesc {
                name: "record name",
                arg_type: ArgType::Record,
            },
            ArgDesc {
                name: "level",
                arg_type: ArgType::Int,
            },
        ],
        "dbcar [record name] [level] — Database Channel Access Report.\n\
         Shows status of Channel Access links (CA_LINK).\n\
         level 0 - Shows statistics for all links.\n      \
         \x201 - Shows info. of only disconnected links.\n      \
         \x202 - Shows info. for all links.",
        |args: &[ArgValue], ctx: &CommandContext| {
            use crate::server::record::ParsedLink;

            // C: `!precordname || precordname[0] == '\0' || !strcmp(..., "*")`
            // all mean "every record" (`dbCaTest.c:72-77`).
            let only = match &args[0] {
                ArgValue::String(s) if !s.is_empty() && s != "*" => Some(s.clone()),
                _ => None,
            };
            let level = match args[1] {
                ArgValue::Int(v) => v,
                _ => 0,
            };
            match &only {
                None => ctx.println("CA links in all records\n"),
                Some(name) => ctx.println(&format!("CA links in record named '{name}'\n")),
            }

            // C's walk is `dbFirstRecordType`/`dbFirstRecord` with aliases
            // skipped, which is `record_names_type_major`; naming one record
            // instead short-circuits after the first match (`dbCaTest.c:138`)
            // and DOES match an alias, which `get_record` resolves.
            let records = match &only {
                Some(name) => vec![name.clone()],
                None => record_names_type_major(ctx),
            };
            let ca_lset = ctx.block_on(ctx.db().link_set("ca"));

            let mut ncalinks = 0i64;
            let mut nconnected = 0i64;
            let mut no_read_access = 0i64;
            let mut no_write_access = 0i64;
            let mut total_disconnect = 0u64;
            let mut total_no_write = 0u64;

            for record in records {
                // C prints `precord->name`, the record an alias resolves to,
                // not the name the walk matched.
                let Some(rec) = ctx.db().get_record(&record) else {
                    continue;
                };
                let display = rec.read().name.clone();
                let mut fields = ctx.db().record_link_fields(&record);
                // C walks `pdbRecordType->link_ind[]`, i.e. papFldDes order,
                // which puts every `dbCommon` link field ahead of the record
                // type's own. `record_link_fields` groups by where the port
                // stores them instead, so restore the dbCommon-first half
                // here; the order WITHIN the record-specific group stays the
                // enumerator's, because `INP`/`OUT` carry `DBF_*LINK`
                // descriptors and appear in no generated field table to sort
                // against.
                fields.sort_by_key(|(field, _, _)| {
                    usize::from(!matches!(field.as_str(), "TSEL" | "SDIS" | "FLNK"))
                });
                for (field, _raw, parsed) in fields {
                    let ParsedLink::Ca(ca) = parsed else { continue };
                    ncalinks += 1;
                    let diagnostics = ca_lset
                        .as_ref()
                        .and_then(|lset| ctx.block_on(lset.link_diagnostics(&ca.pv)));
                    let no_write = ctx.db().external_link_puts_coalesced_for(&ca.pv);
                    match diagnostics {
                        Some(d) if d.connected => {
                            nconnected += 1;
                            total_disconnect += d.n_disconnect;
                            total_no_write += no_write;
                            if !d.read_access {
                                no_read_access += 1;
                            }
                            if !d.write_access {
                                no_write_access += 1;
                            }
                            if level > 1 {
                                ctx.println(&format!(
                                    "{display:>28}.{field:<4} ==> {:<28}  ({}, {no_write})",
                                    ca.pv, d.n_disconnect
                                ));
                                const RIGHTS: [&str; 4] =
                                    ["No Access", "Read Only", "Write Only", "Read/Write"];
                                let rights =
                                    usize::from(d.read_access) | usize::from(d.write_access) << 1;
                                ctx.println(&format!(
                                    "{:>21} [{}{}{}{}] host {}, {}",
                                    "",
                                    if d.input_native { "IN" } else { "  " },
                                    if d.input_string { "IS" } else { "  " },
                                    if d.output_native { "ON" } else { "  " },
                                    if d.output_string { "OS" } else { "  " },
                                    d.host,
                                    RIGHTS[rights],
                                ));
                            }
                        }
                        // C's `pca ? pca->nDisconnect : 0` (`dbCaTest.c:131`):
                        // a link the facility never opened reports zeros, one
                        // it opened and lost reports its real counters.
                        other => {
                            if level > 0 {
                                let (disconnects, no_write) = match other {
                                    Some(d) => (d.n_disconnect, no_write),
                                    None => (0, 0),
                                };
                                ctx.println(&format!(
                                    "{display:>28}.{field:<4} --> {:<28}  ({disconnects}, {no_write})",
                                    ca.pv
                                ));
                            }
                        }
                    }
                }
            }

            if (level > 1 && nconnected > 0) || (level > 0 && ncalinks != nconnected) {
                ctx.println("");
            }
            ctx.println(&format!(
                "Total {ncalinks} CA link{}; {nconnected} connected, {} not connected.",
                if ncalinks != 1 { "s" } else { "" },
                ncalinks - nconnected,
            ));
            ctx.println(&format!(
                "    {no_read_access} can't read, {no_write_access} can't write.  \
                 ({total_disconnect} disconnects, {total_no_write} writes prohibited)\n"
            ));
            Ok(CommandOutcome::Continue)
        },
    )
}

fn cmd_db_notify_dump() -> CommandDef {
    CommandDef::new(
        "dbNotifyDump",
        vec![],
        "dbNotifyDump — Report status of any active async processing with completion notification.",
        |_args: &[ArgValue], ctx: &CommandContext| {
            use std::sync::Arc;

            // One read guard per record, taken and dropped here, so the
            // render below never nests a second guard on a record it is
            // already holding — the waitList of one block names records
            // whose own blocks are still to come.
            struct Row {
                name: String,
                notify: Option<Arc<crate::server::record::NotifyWaitSet>>,
                pact: bool,
                queued: usize,
            }
            let rows: Vec<Row> = record_names_type_major(ctx)
                .into_iter()
                .filter_map(|name| {
                    let rec = ctx.db().get_record(&name)?;
                    let inst = rec.read();
                    Some(Row {
                        notify: inst.notify.clone(),
                        pact: inst.is_processing(),
                        queued: inst.notify_restart_len(),
                        name,
                    })
                })
                .collect();

            // C `dbNotify.c:659-660`: a record heads a block only when it is
            // the notify's own `dbChannelRecord(ppn->chan)`, so the chain
            // members a `dbNotifyAdd` put the same `ppn` on are skipped and
            // appear only inside the entry's `waitList`.
            for row in rows
                .iter()
                .filter(|r| r.notify.as_ref().and_then(|w| w.entry_record()) == Some(&r.name[..]))
            {
                let ws = row.notify.as_ref().expect("filtered");
                // C's `notifyProcessInProgress` / `notifyUserCallbackRequested`
                // — the two of its seven states the port's slot distinguishes.
                let state = if ws.completed() { 5 } else { 4 };
                ctx.println(&format!(
                    "{} state {} ppn {:p}\n  waitList",
                    row.name,
                    state,
                    Arc::as_ptr(ws)
                ));
                for member in rows
                    .iter()
                    .filter(|m| m.notify.as_ref().is_some_and(|w| Arc::ptr_eq(w, ws)))
                {
                    ctx.println(&format!(
                        "    {} pact {}",
                        member.name,
                        u8::from(member.pact)
                    ));
                }
                if row.queued > 0 {
                    ctx.println(&format!("{} restartList", row.name));
                    for _ in 0..row.queued {
                        ctx.println(&format!("    {}", row.name));
                    }
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbPutAttribute <record type> <attribute name> <value>` — C
/// `dbPutAttribute` (`dbAccess.c:436-460`) behind
/// `dbPutAttrCallFunc` (`dbIocRegister.c:386-387`).
///
/// The command prints nothing on success and nothing on the shell on
/// failure: `iocshSetError` only arms the script-abort status, and the whole
/// visible failure is the `errMessage(status, "dbPutAttribute failure")`
/// line C writes to the errlog stream (`:458`). Measured on `softIoc`
/// R7.0.10-146:
///
/// ```text
/// epics> dbPutAttribute nosuchtype VERS x
/// Record Type does not exist filename="../db/dbAccess.c" line number=459  dbPutAttribute failure
/// epics> dbPutAttribute ai
/// Illegal field value filename="../db/dbAccess.c" line number=459  dbPutAttribute failure
/// ```
///
/// The two spaces before `dbPutAttribute failure` are one from `errPrintf`'s
/// `"line number=%d "` and one from `errMessage`'s own format
/// (`errlog.c:503-504`); the file and line are ours, for the reason
/// `access_commands.rs` gives at its own `errMessage` site.
///
/// What the write is FOR is [`crate::server::database::PvDatabase::get_pv`]: C resolves `rec.VERS`
/// through `dbGetAttributePart` once the declared field list has missed
/// (`dbChannel.c:326-327`), so `dbgf A:ONE.VERS` answers `"none specified"`
/// on a fresh IOC and the value set here afterwards.
fn cmd_db_put_attribute() -> CommandDef {
    CommandDef::new(
        "dbPutAttribute",
        vec![
            ArgDesc {
                // C `dbPutAttrArg0` (`dbIocRegister.c:379`).
                name: "record type",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "attribute name",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "value",
                arg_type: ArgType::String,
            },
        ],
        "dbPutAttribute <record type> <attribute name> <value> — Set/Create record attribute.",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C's `const char *` arguments: an omitted iocsh argument is NULL
            // and an argument given as `""` is a zero-length string, and
            // `dbPutAttribute` treats the two differently.
            let text = |arg: &ArgValue| match arg {
                ArgValue::String(s) => Some(s.clone()),
                _ => None,
            };
            let record_type = text(&args[0]).unwrap_or_default();
            let name = text(&args[1]);
            let value = text(&args[2]);

            match ctx.db().put_record_type_attribute(
                &record_type,
                name.as_deref(),
                value.as_deref(),
            ) {
                Ok(()) => Ok(CommandOutcome::Continue),
                Err(e) => {
                    crate::runtime::log::errlog_printf(&format!(
                        "{} filename=\"{}\" line number={}  dbPutAttribute failure\n",
                        e.message(),
                        file!(),
                        line!()
                    ));
                    Ok(CommandOutcome::Failed)
                }
            }
        },
    )
}

fn cmd_db_lock_show_locked() -> CommandDef {
    CommandDef::new(
        "dbLockShowLocked",
        vec![ArgDesc {
            name: "interest level",
            arg_type: ArgType::Int,
        }],
        "dbLockShowLocked [interest level] — Show Locksets which are currently locked.\n\
         interest level argument is passed to epicsMutexShow to adjust reported\n\
         information.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let level = match args[0] {
                ArgValue::Int(v) => v.max(0) as u32,
                _ => 0,
            };
            let report = ctx.db().lock_set_report();

            ctx.println(&format!("Active lockSets: {}", report.active.len()));
            ctx.println(&format!("Free lockSets: {}", report.free));

            for (pass, header) in ["listTypeScanLock", "listTypeRecordLock"]
                .into_iter()
                .enumerate()
            {
                if report.active.is_empty() {
                    continue;
                }
                ctx.println(header);
                for set in &report.active {
                    if pass == 0 && !set.locked {
                        continue;
                    }
                    let Some(info) = &set.mutex else { continue };
                    for line in info.show_lines(level) {
                        ctx.println(&line);
                    }
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbnr [verbose]` — record and alias counts by record type.
/// C `dbnr` (`dbTest.c:202-239`), registered at `dbIocRegister.c:605`.
fn cmd_dbnr() -> CommandDef {
    CommandDef::new(
        "dbnr",
        vec![ArgDesc {
            name: "verbose",
            arg_type: ArgType::Int,
        }],
        "dbnr [verbose] — List number of records and aliases by type.\n\
         If verbose, list all record types regardless of being instanced",
        |args: &[ArgValue], ctx: &CommandContext| {
            use crate::server::record::dbd_generated::RECORD_TYPES;

            let verbose = matches!(args[0], ArgValue::Int(v) if v != 0);

            // C counts through one list per record type that holds records AND
            // alias nodes, so it reports `dbGetNRecords - dbGetNAliases`
            // (`dbTest.c:226-228`). The port's list is the same list, so the
            // two columns are a partition of it — the record an alias names
            // gives the alias its type, exactly as C's node placement does.
            let mut per_type: HashMap<String, (i64, i64)> = HashMap::new();
            for node in db_nodes_type_major(ctx) {
                let Some(rec) = ctx.db().get_record(&node.name) else {
                    continue;
                };
                let record_type = rec.read().record.record_type().to_string();
                let counts = per_type.entry(record_type).or_default();
                if node.alias_of.is_some() {
                    counts.1 += 1;
                } else {
                    counts.0 += 1;
                }
            }

            // C walks `recordTypeList`, the order the loaded `.dbd` declared the
            // types in; the port's sequence is `RECORD_TYPES` (name order) with
            // any runtime-registered type after it, which is the same rule
            // `record_names_type_major` states and follows.
            let mut runtime_types: Vec<String> = per_type
                .keys()
                .filter(|t| !RECORD_TYPES.contains(&t.as_str()))
                .cloned()
                .collect();
            runtime_types.sort();

            ctx.println("Records  Aliases  Record Type");
            let (mut total_records, mut total_aliases) = (0i64, 0i64);
            for record_type in RECORD_TYPES
                .iter()
                .map(|t| (*t).to_string())
                .chain(runtime_types)
            {
                let (nrecords, naliases) = per_type.get(&record_type).copied().unwrap_or((0, 0));
                total_aliases += naliases;
                total_records += nrecords;
                if verbose || nrecords != 0 {
                    ctx.println(&format!(" {nrecords:5}    {naliases:5}    {record_type}"));
                }
            }
            ctx.println(&format!(
                "Total {total_records} records, {total_aliases} aliases"
            ));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbla [pattern]` — list record aliases whose ALIAS name matches the glob.
/// C `dbla` (`dbTest.c:241-273`), registered at `dbIocRegister.c:606`.
fn cmd_dbla() -> CommandDef {
    CommandDef::new(
        "dbla",
        vec![ArgDesc {
            name: "pattern",
            // C hints this one as a record name (`dbIocRegister.c:229`)
            // even though it matches alias names.
            arg_type: ArgType::Record,
        }],
        "dbla [pattern] — List record alias()s by alias name pattern.",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C skips the glob when the pattern is NULL or empty
            // (`dbTest.c:263`), listing every alias.
            let pattern = match &args[0] {
                ArgValue::String(s) if !s.is_empty() => Some(s.clone()),
                _ => None,
            };
            // C walks the record types and each type's record list, printing an
            // alias node where it sits in that list — the same walk `dbl` runs,
            // kept to the alias half by C's own `if (!dbIsAlias) continue`
            // (`dbTest.c:259-260`).
            for node in db_nodes_type_major(ctx) {
                let Some(target) = node.alias_of else {
                    continue;
                };
                if let Some(pattern) = &pattern
                    && !glob_match(pattern, &node.name)
                {
                    continue;
                }
                // C prints the target's NAME field (`dbTest.c:265-266`),
                // which is the record's own name.
                ctx.println(&format!("{} -> {target}", node.name));
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbli [pattern]` — list `info()` tags whose TAG NAME matches the glob.
/// C `dbli` (`dbTest.c:275-296`), registered at `dbIocRegister.c:607`.
fn cmd_dbli() -> CommandDef {
    CommandDef::new(
        "dbli",
        vec![ArgDesc {
            name: "pattern",
            arg_type: ArgType::String,
        }],
        "dbli [pattern] — List info() tags with names matching pattern.",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C globs the INFO NAME, not the record name: `dbNextMatchingInfo`
            // (`dbStaticLib.c:2936`) tests `dbGetInfoName`, and a NULL or empty
            // pattern matches everything (`:2935`).
            let pattern = match &args[0] {
                ArgValue::String(s) if !s.is_empty() => Some(s.clone()),
                _ => None,
            };
            for name in record_names_type_major(ctx) {
                let Some(rec) = ctx.db().get_record(&name) else {
                    continue;
                };
                // C's `infoList` is an ELLLIST in declaration order
                // (`dbStaticLib.c:2948-2955`); the port's `info` is a HashMap,
                // which has no order to report, so the tags of one record come
                // out sorted by tag name.
                let mut tags: Vec<(String, String)> = rec
                    .read()
                    .info
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                tags.sort();
                for (key, value) in tags {
                    if let Some(pattern) = &pattern
                        && !glob_match(pattern, &key)
                    {
                        continue;
                    }
                    // C appends `, %p` when the tag carries an info POINTER
                    // (`dbTest.c:290-291`). `dbPutInfoPointer` has no port
                    // analogue — the port's tags are strings only — so that
                    // arm is unreachable here, not omitted.
                    ctx.println(&format!("{name} info({key}, \"{value}\")"));
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbCreateAlias pdbbase <record> <alias>` — add a new record alias.
/// C `dbCreateAliasCallFunc` (`dbStaticIocRegister.c:241-261`) over
/// `dbCreateAlias` (`dbStaticLib.c:1663-1710`), registered at
/// `dbStaticIocRegister.c:280`.
fn cmd_db_create_alias() -> CommandDef {
    CommandDef::new(
        "dbCreateAlias",
        vec![
            ArgDesc {
                name: "pdbbase",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "record",
                arg_type: ArgType::Record,
            },
            ArgDesc {
                name: "alias",
                arg_type: ArgType::Record,
            },
        ],
        "dbCreateAlias pdbbase <record> <alias> — Add a new record alias.",
        |args: &[ArgValue], ctx: &CommandContext| {
            check_pdbbase(&args[0])?;
            // C treats either argument missing as `S_dbLib_recNotFound`
            // (`dbStaticIocRegister.c:247-248`) and reports every failure as
            // `ERROR: <status> <errSymMsg>` (`:257-260`). C does NOT gate this
            // on iocState — unlike `dbCreateRecord`, an alias may be added
            // after `iocInit`.
            let (ArgValue::String(record), ArgValue::String(alias)) = (&args[1], &args[2]) else {
                return Err(format!("{S_DB_LIB_REC_NOT_FOUND} Record Not Found"));
            };
            // C: "alias of alias still references actual record"
            // (`dbStaticLib.c:1675-1677`).
            let target = ctx
                .db()
                .resolve_alias(record)
                .unwrap_or_else(|| record.clone());
            match ctx.block_on(ctx.db().add_alias(alias, &target)) {
                Ok(()) => Ok(CommandOutcome::Continue),
                // `add_alias` rejects a missing target with `ChannelNotFound`
                // and a name already taken with `DbParseError`, which are
                // C's `S_dbLib_recNotFound` (`dbStaticLib.c:1679-1680`) and
                // `S_dbLib_recExists` (`:1685-1686`).
                Err(CaError::ChannelNotFound(_)) => {
                    Err(format!("{S_DB_LIB_REC_NOT_FOUND} Record Not Found"))
                }
                Err(_) => Err(format!("{S_DB_LIB_REC_EXISTS} Record Already exists")),
            }
        },
    )
}

/// Print one named state the way C `dbStateShow` does
/// (`dbState.c:99-104`): the `id <ptr> '<name>' : ` prefix only at level 1 or
/// above, then `TRUE`/`FALSE`. The pointer C prints is the `dbState` node's
/// address; the port prints the address of the shared `DbState` the same
/// name resolves to, which is the same identity for the same purpose.
fn print_db_state(ctx: &CommandContext, name: &str, state: &std::sync::Arc<DbState>, level: i64) {
    let value = if state.get() { "TRUE" } else { "FALSE" };
    if level >= 1 {
        ctx.println(&format!(
            "id {:p} '{name}' : {value}",
            std::sync::Arc::as_ptr(state)
        ));
    } else {
        ctx.println(value);
    }
}

/// The name argument shared by the five `dbState*` commands
/// (C `dbStateArgName`, `dbIocRegister.c:520`).
fn db_state_name_arg() -> ArgDesc {
    ArgDesc {
        name: "name",
        arg_type: ArgType::String,
    }
}

/// `dbStateCreate <name>` — C `dbIocRegister.c:521-528`.
fn cmd_db_state_create() -> CommandDef {
    CommandDef::new(
        "dbStateCreate",
        vec![db_state_name_arg()],
        "dbStateCreate <name> — Allocate new state name for \"state\" filter.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let ArgValue::String(name) = &args[0] else {
                // C `dbStateCreate(NULL)` returns NULL (`dbState.c:55-56`)
                // and the callFunc turns that into a bare
                // `iocshSetError(-1)` (`dbIocRegister.c:526-527`) — the
                // line fails and nothing is printed.
                return Ok(CommandOutcome::Failed);
            };
            crate::server::database::filters::sync::db_state_registry().get_or_create(name);
            let _ = ctx;
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbStateSet <name>` — C `dbIocRegister.c:531-542`.
fn cmd_db_state_set() -> CommandDef {
    CommandDef::new(
        "dbStateSet",
        vec![db_state_name_arg()],
        "dbStateSet <name> — Change state to set for \"state\" filter.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let Some(state) = find_db_state(&args[0]) else {
                return Ok(CommandOutcome::Failed);
            };
            state.set(true);
            let _ = ctx;
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbStateClear <name>` — C `dbIocRegister.c:545-556`.
fn cmd_db_state_clear() -> CommandDef {
    CommandDef::new(
        "dbStateClear",
        vec![db_state_name_arg()],
        "dbStateClear <name> — Change state to clear for \"state\" filter.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let Some(state) = find_db_state(&args[0]) else {
                return Ok(CommandOutcome::Failed);
            };
            state.set(false);
            let _ = ctx;
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbStateShow <name> [level]` — C `dbIocRegister.c:559-571`.
fn cmd_db_state_show() -> CommandDef {
    CommandDef::new(
        "dbStateShow",
        vec![
            db_state_name_arg(),
            ArgDesc {
                name: "level",
                arg_type: ArgType::Int,
            },
        ],
        "dbStateShow <name> [level] — Show set/clear status of named state.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let Some(state) = find_db_state(&args[0]) else {
                return Ok(CommandOutcome::Failed);
            };
            let name = match &args[0] {
                ArgValue::String(s) => s.as_str(),
                _ => "",
            };
            let level = match args[1] {
                ArgValue::Int(v) => v,
                _ => 0,
            };
            print_db_state(ctx, name, &state, level);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbStateShowAll [level]` — C `dbIocRegister.c:574-581`.
fn cmd_db_state_show_all() -> CommandDef {
    CommandDef::new(
        "dbStateShowAll",
        vec![ArgDesc {
            name: "level",
            arg_type: ArgType::Int,
        }],
        "dbStateShowAll [level] — Show set/clear status of all named states.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let level = match args[0] {
                ArgValue::Int(v) => v,
                _ => 0,
            };
            // C passes `level+1` (`dbState.c:113`), so the `id <ptr> '<name>'`
            // prefix always prints from here even at level 0.
            for (name, state) in
                crate::server::database::filters::sync::db_state_registry().entries()
            {
                print_db_state(ctx, &name, &state, level + 1);
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// The `dbStateFind` half the three name-taking `dbState*` commands share
/// (`dbIocRegister.c:536 :550 :565`). `None` is C's miss, which the callers
/// answer with a bare [`CommandOutcome::Failed`]: measured on `softIoc`
/// R7.0.10-146, `dbStateSet NOPE` prints nothing at all and only sets the
/// line's status.
fn find_db_state(arg: &ArgValue) -> Option<std::sync::Arc<DbState>> {
    let ArgValue::String(name) = arg else {
        return None;
    };
    crate::server::database::filters::sync::db_state_registry().find(name)
}

/// `dbDumpBreaktable pdbbase [tableName]` — C `dbDumpBreaktable`
/// (`dbStaticLib.c:3533-3555`), registered at `dbStaticIocRegister.c:276`.
/// `dbDumpPath pdbbase` — print the `.db`/`.dbd` search path.
/// C `dbDumpPath` (`dbStaticLib.c:3262-3283`), registered at
/// `dbStaticIocRegister.c:265`.
///
/// C reports `pdbbase->pathPvt`, which `dbReadCOM` installs from this same
/// `EPICS_DB_INCLUDE_PATH` (`dbLexRoutines.c:244-253`) on every load, so the
/// port reports the list its own last load installed — see
/// [`db_loader::loaded_path`]. Before any load C's list is empty and prints
/// `no path defined`; so does this one.
fn cmd_db_dump_path() -> CommandDef {
    CommandDef::new(
        "dbDumpPath",
        vec![ArgDesc {
            name: "pdbbase",
            arg_type: ArgType::String,
        }],
        "dbDumpPath pdbbase — Dump .db/.dbd file search path.",
        |args: &[ArgValue], ctx: &CommandContext| {
            check_pdbbase(&args[0])?;
            // C takes the empty-list branch on both a NULL `pathPvt` and a
            // list whose first node is absent (`dbStaticLib.c:3272-3275`),
            // so a load that installed a blank list prints the same line as
            // no load at all.
            let paths = db_loader::loaded_path().unwrap_or_default();
            if paths.is_empty() {
                ctx.println("no path defined");
                return Ok(CommandOutcome::Continue);
            }
            let joined: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
            ctx.println(&joined.join(&db_loader::PATH_LIST_SEPARATOR.to_string()));
            Ok(CommandOutcome::Continue)
        },
    )
}

fn cmd_db_dump_breaktable() -> CommandDef {
    CommandDef::new(
        "dbDumpBreaktable",
        vec![
            ArgDesc {
                name: "pdbbase",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "tableName",
                arg_type: ArgType::String,
            },
        ],
        "dbDumpBreaktable pdbbase [tableName] — Dump the given break table.\n\
         If the last argument is missing, dump all breakpoint tables.",
        |args: &[ArgValue], ctx: &CommandContext| {
            check_pdbbase(&args[0])?;
            // C compares the name with `strcmp` and skips non-matches
            // (`dbStaticLib.c:3545`); a missing argument is NULL, which
            // matches every table. There is no glob here.
            let wanted = match &args[1] {
                ArgValue::String(s) => Some(s.clone()),
                _ => None,
            };
            let registry = ctx.db().breaktable_registry();
            for table in registry.tables() {
                if let Some(wanted) = &wanted
                    && *wanted != table.name
                {
                    continue;
                }
                ctx.println(&format!("breaktable({}) {{", table.name));
                for point in &table.points {
                    ctx.println(&format!(
                        "\traw={:.6} slope={} eng={:.6}",
                        point.raw,
                        c_exponential(point.slope),
                        point.eng
                    ));
                }
                ctx.println("}");
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

#[cfg(test)]
mod dbgf_escape_tests {
    use super::escape_char_array_for_dbgf;

    /// epics-base dc70dfd6 — printable ASCII passes through unchanged.
    #[test]
    fn printable_ascii_passes_through() {
        assert_eq!(escape_char_array_for_dbgf(b"hello"), "hello");
        assert_eq!(escape_char_array_for_dbgf(b"a b c"), "a b c");
    }

    /// Common C escapes get their short form.
    #[test]
    fn common_c_escapes() {
        assert_eq!(escape_char_array_for_dbgf(b"a\nb"), "a\\nb");
        assert_eq!(escape_char_array_for_dbgf(b"a\tb"), "a\\tb");
        assert_eq!(escape_char_array_for_dbgf(b"a\rb"), "a\\rb");
        assert_eq!(escape_char_array_for_dbgf(b"a\\b"), "a\\\\b");
        assert_eq!(escape_char_array_for_dbgf(b"a\"b"), "a\\\"b");
        assert_eq!(escape_char_array_for_dbgf(b"a\x07b"), "a\\ab");
        assert_eq!(escape_char_array_for_dbgf(b"a\x08b"), "a\\bb");
        assert_eq!(escape_char_array_for_dbgf(b"a\x0cb"), "a\\fb");
        assert_eq!(escape_char_array_for_dbgf(b"a\x0bb"), "a\\vb");
    }

    /// Other non-printable bytes (and any high-bit byte) become `\xNN`.
    #[test]
    fn other_bytes_use_hex_escape() {
        assert_eq!(escape_char_array_for_dbgf(&[0x01, 0xff]), "\\x01\\xff");
        // DEL (0x7f) is non-printable per the C convention.
        assert_eq!(escape_char_array_for_dbgf(&[0x7f]), "\\x7f");
        // Empty buffer stays empty.
        assert_eq!(escape_char_array_for_dbgf(&[]), "");
    }
}

/// Map common field names to their DBF types.
fn common_field_dbf_type(field: &str) -> Option<crate::types::DbFieldType> {
    use crate::types::DbFieldType;
    match field {
        "SCAN" => Some(DbFieldType::String),
        "DTYP" => Some(DbFieldType::String),
        "INP" | "OUT" | "FLNK" | "ASG" => Some(DbFieldType::String),
        // The dbCommon `DBF_MENU` fields carry a menu index (served as
        // `DBR_ENUM`); `UDF`/`TPRO` are the genuine `DBF_UCHAR` flags.
        "SEVR" | "STAT" | "PINI" => Some(DbFieldType::Short),
        "UDF" | "TPRO" => Some(DbFieldType::Char),
        "HIHI" | "HIGH" | "LOW" | "LOLO" => Some(DbFieldType::Double),
        "HHSV" | "HSV" | "LSV" | "LLSV" => Some(DbFieldType::Short),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `dblsr`'s type word is `pdbFldDes->field_type`, so the same field NAME
    /// takes a different word on a different record type. Measured against
    /// softIoc R7.0.10 on one database holding all seven shapes: 13 link rows,
    /// byte-identical.
    ///
    /// Seven of those thirteen are here; the six the old name list happened to
    /// get right (`INP`, `OUT`, `FLNK`, `TSEL`, `SDIS`, and anything else
    /// falling through to its `INLINK` default) are the reason it looked
    /// correct.
    /// `dbgf` on a field that resolves but cannot be read, byte for byte.
    ///
    /// softIoc R7.0.10 prints `DBF_CHAR:           failed.   ` — thirty
    /// columns, two ten-wide tab stops for the header and one for the word.
    /// The port prints the same shape with the type C meant: `dbr[]` holds
    /// `"NOACCESS"` at index 12 while `DBR_NOACCESS` is 17, so C's own label
    /// lookup misses it and lands five past the end of the table.
    #[test]
    fn a_field_that_resolves_but_cannot_be_read_prints_the_type_and_failed() {
        let lines = printbuffer_lines(DbfCode::NoAccess.name(), 1, None);
        assert_eq!(lines, vec!["DBF_NOACCESS:       failed.   ".to_string()]);
        // C's line for the same read, whose only difference is the word the
        // out-of-bounds lookup produced.
        assert_eq!(
            printbuffer_lines("CHAR", 1, None),
            vec!["DBF_CHAR:           failed.   ".to_string()]
        );
    }

    /// The `recGblDbaddrError` line `dbgf` puts on stderr, byte for byte.
    ///
    /// Measured on softIoc R7.0.10, `dbgf T:AI.TIME` on an `ai`:
    ///
    /// ```text
    /// recGblDbaddrError: dbGet: dbrType = 17, field_type = DBF_NOACCESS (17). Illegal Database Request Type PV: T:AI.TIME
    /// ```
    ///
    /// Pinned because this line is the half `printBuffer` does NOT print, so a
    /// wrong rendering here is invisible to the stdout A/B that covers the
    /// `DBF_…: failed.` row.
    #[test]
    fn the_failed_read_reports_c_s_own_dbget_message() {
        assert_eq!(
            dbget_bad_dbrtype_message(DbfCode::NoAccess, DbfCode::NoAccess),
            "dbGet: dbrType = 17, field_type = DBF_NOACCESS (17)."
        );
        // The two codes are distinct inputs, so a renderer that printed one
        // twice would pass the case above and fail this one.
        assert_eq!(
            dbget_bad_dbrtype_message(DbfCode::String, DbfCode::Device),
            "dbGet: dbrType = 0, field_type = DBF_DEVICE (13)."
        );
    }

    /// `dba`'s `Field Size` for a `DBF_NOACCESS` field is `pdbFldDes->size`,
    /// which for `dbCommon.TIME` is `sizeof(epicsTimeStamp)`. Measured: C
    /// prints `    Field Size: 8`. A link keeps `(none)` — that number is
    /// `sizeof(DBLINK)`, a C ABI fact this port does not have.
    #[test]
    fn a_no_access_field_reports_the_width_its_declaration_carries() {
        assert_eq!(c_field_size(DbfCode::NoAccess, 8), Some(8));
        assert_eq!(c_field_size(DbfCode::NoAccess, 1), Some(1));
        assert_eq!(c_field_size(DbfCode::NoAccess, 0), None);
        assert_eq!(c_field_size(DbfCode::Inlink, 0), None);
    }

    #[test]
    fn the_link_word_comes_from_the_declaration_not_the_field_name() {
        for (record_type, field, word) in [
            // The name that means two things: FWDLINK on a fanout...
            ("fanout", "LNK1", "\tFWDLINK"),
            ("fanout", "LNK2", "\tFWDLINK"),
            // ...and OUTLINK on a seq, whose LNK1 drives DOL1's value out.
            ("seq", "LNK1", "\tOUTLINK"),
            ("seq", "DOL1", "\t INLINK"),
            // OUTA is an output on both types that declare it.
            ("dfanout", "OUTA", "\tOUTLINK"),
            ("dfanout", "OUTB", "\tOUTLINK"),
            ("aSub", "OUTA", "\tOUTLINK"),
            ("aSub", "SUBL", "\t INLINK"),
            // SIOL follows its record's direction, like SIML does not:
            // `aoRecord.dbd.pod:551` makes it DBF_OUTLINK, `ai` DBF_INLINK.
            ("ao", "SIOL", "\tOUTLINK"),
            ("ao", "SIML", "\t INLINK"),
            ("ai", "SIOL", "\t INLINK"),
            // The dbCommon fields the name list did get right.
            ("calc", "FLNK", "\tFWDLINK"),
            ("calc", "INPA", "\t INLINK"),
            ("ao", "OUT", "\tOUTLINK"),
        ] {
            assert_eq!(
                link_field_kind(record_type, field),
                Some(word),
                "{record_type}.{field}"
            );
        }
    }

    /// A field no `.dbd` declares as a link has no `dbFldDes` for C to read,
    /// and C prints no row for it. `None` is what makes the caller skip it.
    #[test]
    fn a_field_that_is_not_a_link_has_no_word() {
        assert_eq!(link_field_kind("calc", "VAL"), None);
        assert_eq!(link_field_kind("calc", "NOSUCH"), None);
        assert_eq!(link_field_kind("nosuchrecord", "INP"), None);
    }

    /// No two of the built-in families may claim one name. C's registry
    /// replaces silently and so does ours, so a collision between two
    /// family files would otherwise reach a merged tree with one
    /// registration gone and every test still green.
    #[test]
    fn the_builtin_table_registers_no_name_twice() {
        let mut reg = CommandRegistry::new();
        register_builtins(&mut reg);
        assert!(
            reg.displaced().is_empty(),
            "two built-in families claim the same iocsh name(s): {:?}",
            reg.displaced()
        );
    }
    use crate::server::database::PvDatabase;
    use crate::server::records::ai::AiRecord;
    use crate::types::EpicsValue;
    use std::sync::Arc;

    fn make_ctx() -> (Arc<PvDatabase>, CommandContext) {
        // RTEMS-EXEC-MODEL-ALLOW(1): the shared iocsh test context hand-builds a runtime and leaks it so the CommandContext outlives it; runs and passes in the exec-backend suite.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(PvDatabase::new());
        let bridge = {
            let _guard = rt.enter();
            crate::runtime::task::BlockingBridge::capture()
        };
        let ctx = CommandContext::new(db.clone(), bridge);
        // Leak the runtime so it stays alive for the test
        std::mem::forget(rt);
        (db, ctx)
    }

    /// A context whose database holds the oracle's three records and has been
    /// through `iocInit`, so the lock sets exist.
    fn lock_set_ctx() -> (Arc<PvDatabase>, CommandContext, tempfile::TempDir) {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t9.db");
        std::fs::write(
            &path,
            "record(calc, \"R:A\") { field(INPA, \"R:B\") field(CALC, \"A+1\") field(FLNK, \"R:B\") }\n\
             record(calc, \"R:B\") { field(CALC, \"1\") }\n\
             record(ai,   \"R:C\") { }\n",
        )
        .unwrap();
        run_cmd(&ctx, "dbLoadRecords", &[path.to_str().unwrap()]);
        ctx.block_on(db.ioc_init());
        (db, ctx, dir)
    }

    /// Two `calc` records, initialised, for the notify-dump renderer. The
    /// notify slots are driven directly rather than through a real async
    /// cycle so each block is a fixed string; the live shape is in the C
    /// capture on [`cmd_db_notify_dump`].
    fn notify_ctx() -> (Arc<PvDatabase>, CommandContext, tempfile::TempDir) {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify.db");
        std::fs::write(
            &path,
            "record(calc, \"N:A\") { field(CALC, \"1\") }\n\
             record(calc, \"N:B\") { field(CALC, \"2\") }\n",
        )
        .unwrap();
        run_cmd(&ctx, "dbLoadRecords", &[path.to_str().unwrap()]);
        ctx.block_on(db.ioc_init());
        (db, ctx, dir)
    }

    /// Install a put-notify on `record` and hand back the wait-set, dropping
    /// the completion receiver — nothing here waits on it.
    fn install_notify(db: &PvDatabase, record: &str) -> Arc<crate::server::record::NotifyWaitSet> {
        let (tx, _rx) = crate::runtime::sync::oneshot::channel();
        db.get_record(record)
            .unwrap()
            .write()
            .install_or_queue_notify(tx)
            .expect("the slot was free")
    }

    /// C prints nothing at all when no `processNotify` is outstanding —
    /// measured on `softIoc` R7.0.10-146, where the first `dbNotifyDump`
    /// after `iocInit` emits no bytes.
    #[test]
    fn db_notify_dump_prints_nothing_on_an_idle_ioc() {
        let (_db, ctx, _dir) = notify_ctx();
        assert_eq!(run_cmd(&ctx, "dbNotifyDump", &[]), "");
    }

    /// The block C prints for one outstanding notify:
    ///
    /// ```text
    /// N:S state 4 ppn 0x57e6f3f757e0
    ///   waitList
    ///     N:S pact 1
    /// ```
    ///
    /// `pact` is 0 here because the fixture installs the slot without
    /// driving a cycle; the live capture has 1.
    #[test]
    fn db_notify_dump_prints_the_header_wait_list_and_one_member() {
        let (db, ctx, _dir) = notify_ctx();
        let _ws = install_notify(&db, "N:A");
        let out = run_cmd(&ctx, "dbNotifyDump", &[]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert!(lines[0].starts_with("N:A state 4 ppn 0x"), "{}", lines[0]);
        assert_eq!(lines[1], "  waitList");
        assert_eq!(lines[2], "    N:A pact 0");
    }

    /// A settled wait-set is C's `notifyUserCallbackRequested`, the second of
    /// the two states the port's slot distinguishes — the client callback is
    /// pending while the record still carries the notify.
    #[test]
    fn a_settled_wait_set_reports_state_five() {
        let (db, ctx, _dir) = notify_ctx();
        let ws = install_notify(&db, "N:A");
        ws.leave();
        assert!(ws.completed());
        assert!(
            run_cmd(&ctx, "dbNotifyDump", &[])
                .lines()
                .next()
                .unwrap()
                .starts_with("N:A state 5 ppn 0x")
        );
    }

    /// One chain, ONE block: C skips every record whose `precord->ppn` is a
    /// notify some other record entered (`dbNotify.c:659-660`), so a joined
    /// member shows up only inside the entry's `waitList`.
    ///
    /// Measured against `softIoc` R7.0.10-146 with a `fanout` driving two
    /// `ODLY` `calcout`s: C printed NOTHING there, because the fanout had
    /// already completed and only its targets were still running.
    #[test]
    fn only_the_entry_record_of_a_chain_heads_a_block() {
        let (db, ctx, _dir) = notify_ctx();
        let ws = install_notify(&db, "N:A");
        db.get_record("N:B")
            .unwrap()
            .write()
            .join_put_notify(Some(&ws));

        let out = run_cmd(&ctx, "dbNotifyDump", &[]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "{out}");
        assert!(lines[0].starts_with("N:A state 4 ppn 0x"), "{}", lines[0]);
        assert_eq!(lines[1], "  waitList");
        assert_eq!(lines[2], "    N:A pact 0");
        assert_eq!(lines[3], "    N:B pact 0");
    }

    /// The member that joined a chain heads no block of its own even when the
    /// entry record has left — C's filter is `ppn->chan`, not liveness, and a
    /// record whose `ppn` was entered by someone else never matches it.
    #[test]
    fn a_joined_member_alone_prints_nothing() {
        let (db, ctx, _dir) = notify_ctx();
        let ws = install_notify(&db, "N:A");
        db.get_record("N:B")
            .unwrap()
            .write()
            .join_put_notify(Some(&ws));
        db.get_record("N:A").unwrap().write().notify = None;

        assert_eq!(run_cmd(&ctx, "dbNotifyDump", &[]), "");
    }

    /// A wait-set armed for a downstream link put (`new_put_notify`) is not a
    /// `processNotify`: C reaches the same completion through `dbNotifyAdd` on
    /// the initiator's own `ppn`, so there is no `chan` and no block.
    #[test]
    fn a_chain_internal_wait_set_heads_no_block() {
        let (db, ctx, _dir) = notify_ctx();
        let (ws, _rx) = PvDatabase::new_put_notify();
        db.get_record("N:A")
            .unwrap()
            .write()
            .join_put_notify(Some(&ws));

        assert_eq!(run_cmd(&ctx, "dbNotifyDump", &[]), "");
    }

    /// C prints `%s restartList` once and then one line per queued entry,
    /// each `dbChannelRecord(ppnRestart->chan)->name` — which is the record
    /// whose queue it sits in, so the name repeats:
    ///
    /// ```text
    /// N:S restartList
    ///     N:S
    /// ```
    #[test]
    fn db_notify_dump_prints_one_restart_line_per_queued_put() {
        let (db, ctx, _dir) = notify_ctx();
        let _ws = install_notify(&db, "N:A");
        {
            let rec = db.get_record("N:A").unwrap();
            let mut inst = rec.write();
            for _ in 0..2 {
                let (tx, _rx) = crate::runtime::sync::oneshot::channel();
                inst.queue_notify_put(crate::server::record::DeferredNotify::Process {
                    completion: tx,
                });
            }
        }
        let out = run_cmd(&ctx, "dbNotifyDump", &[]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 6, "{out}");
        assert_eq!(lines[3], "N:A restartList");
        assert_eq!(lines[4], "    N:A");
        assert_eq!(lines[5], "    N:A");
    }

    /// An lset whose `link_diagnostics` answers from a table the test writes,
    /// so every branch of `dbcar`'s render is reachable without a CA server.
    /// `None` for an unlisted name is the lset contract's "never opened",
    /// C's `pca == NULL`.
    #[derive(Default)]
    struct StubCaLset {
        links: std::sync::Mutex<HashMap<String, crate::server::database::LinkDiagnostics>>,
        /// Set by the coalescing test: `put_value` reports that it was
        /// entered and then never returns, which pins the link IN FLIGHT so
        /// the puts staged behind it have a deterministic fate.
        entered: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
    }

    impl StubCaLset {
        fn set(&self, name: &str, d: crate::server::database::LinkDiagnostics) {
            self.links.lock().unwrap().insert(name.to_string(), d);
        }

        fn hold_puts(&self) -> std::sync::mpsc::Receiver<()> {
            let (tx, rx) = std::sync::mpsc::channel();
            *self.entered.lock().unwrap() = Some(tx);
            rx
        }
    }

    #[async_trait::async_trait]
    impl crate::server::database::LinkSet for StubCaLset {
        fn is_connected(&self, name: &str) -> bool {
            self.links
                .lock()
                .unwrap()
                .get(name)
                .is_some_and(|d| d.connected)
        }
        async fn get_value(&self, _name: &str) -> Option<EpicsValue> {
            None
        }
        fn put_admission(&self, name: &str) -> crate::server::database::PutAdmission {
            if self.is_connected(name) {
                crate::server::database::PutAdmission::Connected
            } else {
                crate::server::database::PutAdmission::Refused
            }
        }
        async fn put_value(
            &self,
            _name: &str,
            _value: EpicsValue,
            _op: crate::server::database::LinkPutOp,
        ) -> Result<(), String> {
            let held = self.entered.lock().unwrap().clone();
            match held {
                Some(tx) => {
                    let _ = tx.send(());
                    std::future::pending::<()>().await;
                    unreachable!("held forever")
                }
                None => Ok(()),
            }
        }
        async fn link_diagnostics(
            &self,
            name: &str,
        ) -> Option<crate::server::database::LinkDiagnostics> {
            self.links.lock().unwrap().get(name).cloned()
        }
        fn link_names(&self) -> Vec<String> {
            self.links.lock().unwrap().keys().cloned().collect()
        }
    }

    fn diag(
        connected: bool,
        read: bool,
        write: bool,
        nd: u64,
    ) -> crate::server::database::LinkDiagnostics {
        crate::server::database::LinkDiagnostics {
            connected,
            host: "ioc1.lab:5064".to_string(),
            read_access: read,
            write_access: write,
            n_disconnect: nd,
            input_native: true,
            ..Default::default()
        }
    }

    /// Two `ai` records whose INP links are CA (their targets are not in this
    /// IOC, which is C `dbInitLink`'s locality fallthrough), plus a link-free
    /// record so the walk has something to skip.
    fn dbcar_ctx() -> (
        Arc<PvDatabase>,
        CommandContext,
        Arc<StubCaLset>,
        tempfile::TempDir,
    ) {
        // The database is built INSIDE the runtime, unlike `make_ctx`: the
        // link-put queue captures its reactor at construction and refuses
        // every write without one, so a coalesce count is unreachable from a
        // database made outside.
        // RTEMS-EXEC-MODEL-ALLOW(1): dbcar's context builds the database INSIDE the runtime because the link-put queue captures its reactor at construction; runs and passes in the exec-backend suite.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (db, bridge) = {
            let _guard = rt.enter();
            (
                Arc::new(PvDatabase::new()),
                crate::runtime::task::BlockingBridge::capture(),
            )
        };
        let ctx = CommandContext::new(db.clone(), bridge);
        std::mem::forget(rt);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dbcar.db");
        std::fs::write(
            &path,
            "record(ai, \"L:UP\")   { field(INP, \"UP:PV CA\") }\n\
             record(ai, \"L:DOWN\") { field(INP, \"DOWN:PV CA\") }\n\
             record(ai, \"L:NONE\") { }\n",
        )
        .unwrap();
        run_cmd(&ctx, "dbLoadRecords", &[path.to_str().unwrap()]);
        let lset = Arc::new(StubCaLset::default());
        ctx.block_on(db.register_link_set("ca", lset.clone()));
        ctx.block_on(db.ioc_init());
        (db, ctx, lset, dir)
    }

    /// C `dbCaTest.c:145-154` with nothing to report: header, blank, then the
    /// two summary lines and the trailing blank the second `printf`'s `\n\n`
    /// leaves.
    #[test]
    fn dbcar_on_a_link_free_ioc_prints_only_the_summary() {
        let (_db, ctx) = make_ctx();
        assert_eq!(
            run_cmd(&ctx, "dbcar", &[]),
            "CA links in all records\n\
             \n\
             Total 0 CA links; 0 connected, 0 not connected.\n    \
             0 can't read, 0 can't write.  (0 disconnects, 0 writes prohibited)\n\
             \n"
        );
    }

    /// `(ncalinks != 1)` picks the plural (`dbCaTest.c:147-148`), so one link
    /// is "1 CA link", not "1 CA links".
    #[test]
    fn one_link_is_singular() {
        let (_db, ctx, lset, _dir) = dbcar_ctx();
        lset.set("UP:PV", diag(true, true, true, 0));
        let out = run_cmd(&ctx, "dbcar", &["L:UP"]);
        assert!(
            out.contains("Total 1 CA link; 1 connected, 0 not connected."),
            "{out}"
        );
    }

    /// Level 0 is the summary alone — neither branch prints (`dbCaTest.c:103`,
    /// `:126`).
    #[test]
    fn level_zero_prints_no_link_lines() {
        let (_db, ctx, lset, _dir) = dbcar_ctx();
        lset.set("UP:PV", diag(true, true, true, 2));
        lset.set("DOWN:PV", diag(false, true, true, 5));
        let out = run_cmd(&ctx, "dbcar", &[]);
        assert!(!out.contains("==>"), "{out}");
        assert!(!out.contains("-->"), "{out}");
        assert!(
            out.contains("Total 2 CA links; 1 connected, 1 not connected."),
            "{out}"
        );
        // Only the CONNECTED branch accumulates (`dbCaTest.c:98-102`), so the
        // disconnected link's 5 never reaches the total.
        assert!(
            out.contains("(2 disconnects, 0 writes prohibited)"),
            "{out}"
        );
    }

    /// Level 1 is the disconnected links only, with C's `-->` arrow.
    #[test]
    fn level_one_prints_only_the_disconnected_links() {
        let (_db, ctx, lset, _dir) = dbcar_ctx();
        lset.set("UP:PV", diag(true, true, true, 0));
        lset.set("DOWN:PV", diag(false, true, true, 5));
        let out = run_cmd(&ctx, "dbcar", &["", "1"]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[2],
            "                      L:DOWN.INP  --> DOWN:PV                       (5, 0)"
        );
        assert!(!out.contains("==>"), "{out}");
    }

    /// Level 2 adds the connected links and their second line
    /// (`dbCaTest.c:111-123`).
    #[test]
    fn level_two_prints_the_host_and_rights_line() {
        let (_db, ctx, lset, _dir) = dbcar_ctx();
        lset.set("UP:PV", diag(true, true, true, 3));
        let out = run_cmd(&ctx, "dbcar", &["L:UP", "2"]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[2],
            "                        L:UP.INP  ==> UP:PV                         (3, 0)"
        );
        assert_eq!(
            lines[3],
            "                      [IN      ] host ioc1.lab:5064, Read/Write"
        );
    }

    /// `rw = ca_read_access | ca_write_access << 1` indexes
    /// `{"No Access","Read Only","Write Only","Read/Write"}`
    /// (`dbCaTest.c:104-109`) — one case per index, and the two `can't`
    /// counters that move with them (`:101-102`).
    #[test]
    fn every_access_rights_combination_has_its_word() {
        for (read, write, word, no_read, no_write) in [
            (false, false, "No Access", 1, 1),
            (true, false, "Read Only", 0, 1),
            (false, true, "Write Only", 1, 0),
            (true, true, "Read/Write", 0, 0),
        ] {
            let (_db, ctx, lset, _dir) = dbcar_ctx();
            lset.set("UP:PV", diag(true, read, write, 0));
            let out = run_cmd(&ctx, "dbcar", &["L:UP", "2"]);
            assert!(
                out.contains(&format!("host ioc1.lab:5064, {word}")),
                "{out}"
            );
            assert!(
                out.contains(&format!(
                    "    {no_read} can't read, {no_write} can't write."
                )),
                "{word}: {out}"
            );
        }
    }

    /// C prints `pca ? pca->nDisconnect : 0` (`dbCaTest.c:131-132`): a link
    /// the CA facility never opened reports zeros rather than the counters of
    /// whatever else shares its name.
    #[test]
    fn a_never_opened_link_reports_zero_counters() {
        let (_db, ctx, _lset, _dir) = dbcar_ctx();
        let out = run_cmd(&ctx, "dbcar", &["L:UP", "1"]);
        assert!(
            out.contains("--> UP:PV                         (0, 0)"),
            "{out}"
        );
    }

    /// The blank line before the summary is gated by
    /// `(level > 1 && nconnected > 0) || (level > 0 && ncalinks != nconnected)`
    /// (`dbCaTest.c:145-146`) — four boundary cases, one per operand state.
    #[test]
    fn the_blank_line_before_the_summary_follows_cs_gate() {
        // Two blank lines are unconditional — the header's `\n\n` and the
        // trailing `\n\n` of the last summary `printf`. The gate is the
        // third.
        let blanks = |out: &str| out.lines().filter(|l| l.is_empty()).count();
        let run = |level: &str, down_connected: bool| {
            let (_db, ctx, lset, _dir) = dbcar_ctx();
            lset.set("UP:PV", diag(true, true, true, 0));
            lset.set("DOWN:PV", diag(down_connected, true, true, 0));
            blanks(&run_cmd(&ctx, "dbcar", &["", level]))
        };
        // level 0: neither operand can fire, whatever the links are doing.
        assert_eq!(run("0", true), 2);
        assert_eq!(run("0", false), 2);
        // level 1 with everything connected: `ncalinks != nconnected` false.
        assert_eq!(run("1", true), 2);
        // level 1 with one down: second operand true.
        assert_eq!(run("1", false), 3);
        // level 2 with everything connected: first operand true.
        assert_eq!(run("2", true), 3);
    }

    /// `*` and the empty string are C's "all records" spellings
    /// (`dbCaTest.c:72`), and a name that matches nothing reports no links at
    /// all rather than falling back to the whole database.
    #[test]
    fn the_record_argument_selects_c_s_three_ways() {
        let (_db, ctx, lset, _dir) = dbcar_ctx();
        lset.set("UP:PV", diag(true, true, true, 0));
        lset.set("DOWN:PV", diag(true, true, true, 0));
        for all in ["", "*"] {
            let out = run_cmd(&ctx, "dbcar", &[all]);
            assert!(out.starts_with("CA links in all records\n\n"), "{out}");
            assert!(out.contains("Total 2 CA links;"), "{out}");
        }
        let named = run_cmd(&ctx, "dbcar", &["L:UP"]);
        assert!(
            named.starts_with("CA links in record named 'L:UP'\n\n"),
            "{named}"
        );
        assert!(named.contains("Total 1 CA link;"), "{named}");
        let missing = run_cmd(&ctx, "dbcar", &["L:NOSUCH"]);
        assert!(missing.contains("Total 0 CA links;"), "{missing}");
    }

    /// C prints `precord->name`, so a walk that matched an alias still names
    /// the record the alias resolves to (`dbCaTest.c:112`).
    #[test]
    fn an_alias_reports_under_the_records_own_name() {
        let (db, ctx, lset, _dir) = dbcar_ctx();
        lset.set("UP:PV", diag(true, true, true, 0));
        ctx.block_on(db.add_alias("L:ALIAS", "L:UP")).unwrap();
        let out = run_cmd(&ctx, "dbcar", &["L:ALIAS", "2"]);
        assert!(out.contains("CA links in record named 'L:ALIAS'"), "{out}");
        assert!(out.contains("L:UP.INP  ==>"), "{out}");
    }

    /// C's walk is `link_ind[]`, i.e. papFldDes order, which puts every
    /// `dbCommon` link field ahead of the record type's own — so a record
    /// with both a CA `FLNK` and a CA `INP` prints FLNK first.
    #[test]
    fn dbcommon_link_fields_print_before_the_record_types_own() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("order.db");
        std::fs::write(
            &path,
            "record(ai, \"O:A\") { field(INP, \"IN:PV CA\") field(FLNK, \"FWD:PV CA\") }\n",
        )
        .unwrap();
        run_cmd(&ctx, "dbLoadRecords", &[path.to_str().unwrap()]);
        let lset = Arc::new(StubCaLset::default());
        ctx.block_on(db.register_link_set("ca", lset.clone()));
        ctx.block_on(db.ioc_init());
        lset.set("IN:PV", diag(true, true, true, 0));
        lset.set("FWD:PV", diag(true, true, true, 0));
        let out = run_cmd(&ctx, "dbcar", &["O:A", "2"]);
        let arrows: Vec<&str> = out
            .lines()
            .filter(|l| l.contains("==>"))
            .map(|l| l.split_whitespace().next().unwrap())
            .collect();
        assert_eq!(arrows, ["O:A.FLNK", "O:A.INP"], "{out}");
    }

    /// The `nNoWrite` column end to end: C counts a staged out-value a later
    /// put replaced before it reached the wire (`dbCa.c:582`), which in this
    /// port is `LinkPutQueue::supersede`. Two puts with no owner draining
    /// between them coalesce into one, so the link reports 1 and the summary
    /// carries it into `writes prohibited`.
    #[test]
    fn a_superseded_out_link_write_is_one_write_prohibited() {
        let (db, ctx, lset, _dir) = dbcar_ctx();
        lset.set("UP:PV", diag(true, true, true, 0));
        let entered = lset.hold_puts();
        db.write_external_pv("UP:PV", EpicsValue::Double(1.0), None)
            .unwrap();
        entered.recv().expect("the first write reached the lset");
        // Staged behind an in-flight write: restaged, nothing displaced.
        db.write_external_pv("UP:PV", EpicsValue::Double(2.0), None)
            .unwrap();
        assert_eq!(db.external_link_puts_coalesced_for("UP:PV"), 0);
        // Now one IS displaced.
        db.write_external_pv("UP:PV", EpicsValue::Double(3.0), None)
            .unwrap();
        assert_eq!(db.external_link_puts_coalesced_for("UP:PV"), 1);
        let out = run_cmd(&ctx, "dbcar", &["L:UP", "2"]);
        assert!(
            out.contains("==> UP:PV                         (0, 1)"),
            "{out}"
        );
        assert!(
            out.contains("(0 disconnects, 1 writes prohibited)"),
            "{out}"
        );
    }

    /// A context holding an `ai` and a `calc` — a second record type, to
    /// prove the attribute map is per-type.
    fn attr_ctx() -> (Arc<PvDatabase>, CommandContext, tempfile::TempDir) {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attr.db");
        std::fs::write(
            &path,
            "record(ai,   \"A:ONE\") { field(DESC, \"one\") }\n\
             record(calc, \"A:TWO\") { field(CALC, \"1\") }\n",
        )
        .unwrap();
        run_cmd(&ctx, "dbLoadRecords", &[path.to_str().unwrap()]);
        ctx.block_on(db.ioc_init());
        (db, ctx, dir)
    }

    fn attr_of(db: &PvDatabase, pv: &str) -> Option<String> {
        match db.get_pv(pv) {
            Ok(crate::types::EpicsValue::String(s)) => Some(s.to_string()),
            _ => None,
        }
    }

    /// The seed C's `.dbd` read leaves behind (`dbLexRoutines.c:311-331`),
    /// against the C capture:
    ///
    /// ```text
    /// epics> dbgf A:ONE.RTYP
    /// DBF_STRING:         "ai"
    /// epics> dbgf A:ONE.VERS
    /// DBF_STRING:         "none specified"
    /// ```
    ///
    /// `RTYP` already answered here before this map existed, as a virtual
    /// field; `VERS` did not answer at all.
    #[test]
    fn every_record_type_starts_with_rtyp_and_vers() {
        let (db, ctx, _dir) = attr_ctx();
        assert_eq!(attr_of(&db, "A:ONE.RTYP").as_deref(), Some("ai"));
        assert_eq!(
            attr_of(&db, "A:ONE.VERS").as_deref(),
            Some("none specified")
        );
        assert_eq!(attr_of(&db, "A:TWO.RTYP").as_deref(), Some("calc"));
        assert!(
            run_cmd(&ctx, "dbgf", &["A:ONE.VERS"]).contains("\"none specified\""),
            "the shell path resolves it too"
        );
    }

    /// `dbPutAttribute` writes one record type and leaves the others, against
    /// the C capture:
    ///
    /// ```text
    /// epics> dbPutAttribute ai VERS "1.2.3"
    /// epics> dbgf A:ONE.VERS
    /// DBF_STRING:         "1.2.3"
    /// epics> dbgf A:TWO.VERS
    /// DBF_STRING:         "none specified"
    /// ```
    ///
    /// The command itself prints nothing, on either arm.
    #[test]
    fn db_put_attribute_rewrites_one_record_type_only() {
        let (db, ctx, _dir) = attr_ctx();
        let (out, failed) = run_cmd_outcome(&ctx, "dbPutAttribute", &["ai", "VERS", "1.2.3"]);
        assert_eq!(out, "", "C prints nothing on success");
        assert!(!failed);
        assert_eq!(attr_of(&db, "A:ONE.VERS").as_deref(), Some("1.2.3"));
        assert_eq!(
            attr_of(&db, "A:TWO.VERS").as_deref(),
            Some("none specified")
        );
    }

    /// A name no record type has is created, not refused — C's `createNew`
    /// arm (`dbStaticLib.c:1251-1273`):
    ///
    /// ```text
    /// epics> dbPutAttribute ai FOO bar
    /// epics> dbgf A:ONE.FOO
    /// DBF_STRING:         "bar"
    /// ```
    #[test]
    fn db_put_attribute_creates_a_name_no_record_type_declares() {
        let (db, ctx, _dir) = attr_ctx();
        run_cmd(&ctx, "dbPutAttribute", &["ai", "FOO", "bar"]);
        assert_eq!(attr_of(&db, "A:ONE.FOO").as_deref(), Some("bar"));
        assert!(
            db.get_pv("A:TWO.FOO").is_err(),
            "a `calc` gained nothing from an `ai` attribute"
        );
    }

    /// C keeps `MAX_STRING_SIZE - 1` characters, measured against a 43-byte
    /// value on `softIoc` R7.0.10-146:
    ///
    /// ```text
    /// epics> dbPutAttribute ai LONG "0123456789012345678901234567890123456789ZZZ"
    /// epics> dbgf A:ONE.LONG
    /// DBF_STRING:         "012345678901234567890123456789012345678"
    /// ```
    #[test]
    fn db_put_attribute_keeps_thirty_nine_characters() {
        let (db, ctx, _dir) = attr_ctx();
        run_cmd(
            &ctx,
            "dbPutAttribute",
            &["ai", "LONG", "0123456789012345678901234567890123456789ZZZ"],
        );
        assert_eq!(
            attr_of(&db, "A:ONE.LONG").as_deref(),
            Some("012345678901234567890123456789012345678")
        );
    }

    /// An omitted value is `""` and an omitted NAME is `S_db_badField`, but a
    /// name GIVEN as `""` is neither — C's test is `!name`, so the empty
    /// string reaches `dbPutRecordAttribute` and creates an attribute. All
    /// three arms measured on `softIoc` R7.0.10-146; the failing one prints
    /// nothing on the shell, only `errMessage` on the errlog stream.
    #[test]
    fn db_put_attribute_separates_a_missing_argument_from_an_empty_one() {
        let (db, ctx, _dir) = attr_ctx();

        let (out, failed) = run_cmd_outcome(&ctx, "dbPutAttribute", &["ai", "EMPTY"]);
        assert_eq!(out, "");
        assert!(!failed, "a missing VALUE is `\"\"`, not an error");
        assert_eq!(attr_of(&db, "A:ONE.EMPTY").as_deref(), Some(""));

        let (out, failed) = run_cmd_outcome(&ctx, "dbPutAttribute", &["ai"]);
        assert_eq!(out, "", "the whole failure is on the errlog stream");
        assert!(failed, "a missing NAME is S_db_badField");

        let (_, failed) = run_cmd_outcome(&ctx, "dbPutAttribute", &["ai", "", "empty-name"]);
        assert!(!failed, "an empty name is a name C accepts");
    }

    /// An unknown record type is `S_dbLib_recordTypeNotFound`, silent on the
    /// shell.
    #[test]
    fn db_put_attribute_refuses_a_record_type_the_dbd_does_not_declare() {
        let (db, ctx, _dir) = attr_ctx();
        let (out, failed) = run_cmd_outcome(&ctx, "dbPutAttribute", &["nosuchtype", "VERS", "x"]);
        assert_eq!(out, "");
        assert!(failed);
        assert_eq!(
            db.record_type_attributes("nosuchtype"),
            Vec::<(String, String)>::new()
        );
    }

    /// The search side asks the same question: C's `dbChannelTest` runs the
    /// same `pvNameLookup` as `dbChannelCreate` (`dbChannel.c:331-343`), so
    /// an attribute name answers a SEARCH and a client can create a channel
    /// on it. Without this the attribute would be readable from iocsh and
    /// invisible to `caget`.
    #[test]
    fn an_attribute_name_answers_the_search_side_too() {
        let (db, ctx, _dir) = attr_ctx();
        assert!(ctx.block_on(db.has_name("A:ONE.VERS")));
        assert!(!ctx.block_on(db.has_name("A:ONE.NOSUCHATTR")));
        run_cmd(&ctx, "dbPutAttribute", &["ai", "NOSUCHATTR", "now-it-is"]);
        assert!(ctx.block_on(db.has_name("A:ONE.NOSUCHATTR")));
        assert!(
            !ctx.block_on(db.has_name("A:TWO.NOSUCHATTR")),
            "the attribute belongs to `ai`, not to every type"
        );
    }

    /// C reaches `dbGetAttributePart` only after the record type's DECLARED
    /// field list has missed (`dbChannel.c:326-327`), so an attribute that
    /// collides with a field never answers, against the C capture:
    ///
    /// ```text
    /// epics> dbgf A:ONE.DESC
    /// DBF_STRING:         "one"
    /// epics> dbPutAttribute ai DESC shadowed
    /// epics> dbgf A:ONE.DESC
    /// DBF_STRING:         "one"
    /// ```
    ///
    /// The write is accepted — C stores it in `attributeList` either way —
    /// and simply unreachable through a record.
    #[test]
    fn a_declared_field_shadows_the_attribute_of_the_same_name() {
        let (db, ctx, _dir) = attr_ctx();
        run_cmd(&ctx, "dbPutAttribute", &["ai", "DESC", "shadowed"]);
        assert_eq!(
            attr_of(&db, "A:ONE.DESC").as_deref(),
            Some("one"),
            "the DESC FIELD still answers"
        );
        assert_eq!(
            db.record_type_attribute("ai", "DESC").as_deref(),
            Some("shadowed"),
            "and the attribute was still written"
        );
    }

    /// The whole list comes back in C's `strcmp` order, which is the order
    /// `dbPutRecordAttribute` inserts in and the order C prints.
    #[test]
    fn record_type_attributes_come_back_sorted_by_name() {
        let (db, ctx, _dir) = attr_ctx();
        run_cmd(&ctx, "dbPutAttribute", &["ai", "BBB", "b"]);
        run_cmd(&ctx, "dbPutAttribute", &["ai", "AAA", "a"]);
        let names: Vec<String> = db
            .record_type_attributes("ai")
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(names, ["AAA", "BBB", "RTYP", "VERS"]);
    }

    /// `dblsr`'s header line, against the C capture:
    ///
    /// ```text
    /// epics> dblsr("*",0)
    /// Lock Set 2 1 members 1 refs epicsMutexId 0x60c74b8a5ad0
    /// Lock Set 3 2 members 2 refs epicsMutexId 0x60c74b8a5c10
    /// ```
    ///
    /// Two sets for three records, member counts 2 and 1, and `refs` equal to
    /// the member count because nothing holds a set. The ids and therefore the
    /// listing order differ from C's — C numbers sets in record-type order and
    /// this port in name order — which is why the assertion is on the shape
    /// and the counts rather than on C's exact digits.
    #[test]
    fn dblsr_reports_one_set_per_link_component() {
        let (_db, ctx, _dir) = lock_set_ctx();
        let out = run_cmd(&ctx, "dblsr", &["*", "0"]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "three records, two lock sets:\n{out}");
        for (line, members) in lines.iter().zip([2, 1]) {
            let head = line
                .split(" epicsMutexId ")
                .next()
                .expect("split always yields one");
            let id: u64 = head.split_whitespace().nth(2).unwrap().parse().unwrap();
            assert_eq!(
                head,
                format!("Lock Set {id} {members} members {members} refs"),
                "in {line}"
            );
            assert!(
                line.contains(" epicsMutexId 0x"),
                "C prints the set's epicsMutexId with %p: {line}"
            );
        }
    }

    /// Level 1 adds the member names, level 2 the DB links of each member —
    /// `dbLock.c:880-900`.
    ///
    /// The links come out in C's order, which is the record type's field
    /// order: `dbCommon`'s link fields first (so `FLNK` precedes `INPA`),
    /// because `link_ind` indexes the field table and `dbCommon` heads it. A
    /// bare link carries `NPP NMS` — C reads `pvlMask`, which no modifier
    /// leaves zero — and a `FWDLINK` can carry nothing else, since
    /// `DBF_FWDLINK` masks every process and maximize-severity modifier off.
    #[test]
    fn dblsr_levels_add_members_then_their_links() {
        let (_db, ctx, _dir) = lock_set_ctx();

        let level1 = run_cmd(&ctx, "dblsr", &["*", "1"]);
        assert!(level1.contains("\nR:A\nR:B\n"), "{level1}");
        assert!(!level1.contains("INPA"), "level 1 stops at the names");

        let level2 = run_cmd(&ctx, "dblsr", &["*", "2"]);
        let block = level2.split_once("\nR:A\n").expect("R:A heads its set").1;
        assert!(
            block.starts_with("\tFLNK\tFWDLINK NPP NMS R:B\n\tINPA\t INLINK NPP NMS R:B\nR:B\n"),
            "{level2}"
        );
    }

    /// The other arm of the `%s` C picks with `pvlMask & pvlOptPP` — a
    /// modifier'd link prints `" PP"`, whose leading space is C's and lands as
    /// a double space after the field-type column, and `MS` in the
    /// `msstring[4]` slot the mode selects.
    #[test]
    fn dblsr_prints_the_pp_and_ms_modifiers_a_link_carries() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pp.db");
        std::fs::write(
            &path,
            "record(calc, \"P:A\") { field(INPA, \"P:B PP MS\") field(CALC, \"A\") }\n\
             record(calc, \"P:B\") { field(CALC, \"1\") }\n",
        )
        .unwrap();
        run_cmd(&ctx, "dbLoadRecords", &[path.to_str().unwrap()]);
        ctx.block_on(db.ioc_init());

        let out = run_cmd(&ctx, "dblsr", &["*", "2"]);
        assert!(out.contains("\tINPA\t INLINK  PP MS P:B\n"), "{out}");
    }

    /// One record selects its own set and nothing else, and an unknown name is
    /// C's `Record not found` (`dbLock.c:882-887`).
    #[test]
    fn dblsr_selects_one_record_s_set_and_reports_a_miss() {
        let (_db, ctx, _dir) = lock_set_ctx();
        let one = run_cmd(&ctx, "dblsr", &["R:C", "1"]);
        assert_eq!(one.lines().count(), 2, "one header, one member:\n{one}");
        assert!(one.ends_with("\nR:C\n"), "{one}");

        assert_eq!(
            run_cmd(&ctx, "dblsr", &["R:NONE", "0"]),
            "Record not found\n"
        );
    }

    /// `dbLockShowLocked` on an idle IOC, against the C capture:
    ///
    /// ```text
    /// epics> dbLockShowLocked(0)
    /// Active lockSets: 2
    /// Free lockSets: 1
    /// listTypeScanLock
    /// listTypeRecordLock
    /// epicsMutexId 0x60c74b8a5ad0 source ../db/dbLock.c line 86
    /// epicsMutexId 0x60c74b8a5c10 source ../db/dbLock.c line 86
    /// ```
    ///
    /// The first pass prints a bare header because no set is held, the second
    /// prints one `epicsMutexShow` row per set, and `Free lockSets: 1` is the
    /// set the merge emptied. C's `source` is `makeSet`'s line in `dbLock.c`;
    /// here it is this port's equivalent, `record_lock.rs`.
    #[test]
    fn db_lock_show_locked_reports_the_counts_both_headers_and_a_row_per_set() {
        let (_db, ctx, _dir) = lock_set_ctx();
        let out = run_cmd(&ctx, "dbLockShowLocked", &["0"]);
        let lines: Vec<&str> = out.lines().collect();

        assert_eq!(lines[0], "Active lockSets: 2");
        assert_eq!(lines[1], "Free lockSets: 1");
        assert_eq!(lines[2], "listTypeScanLock");
        assert_eq!(
            lines[3], "listTypeRecordLock",
            "no set is held, so the first pass is a bare header:\n{out}"
        );
        assert_eq!(lines.len(), 6, "one row per active set:\n{out}");
        for line in &lines[4..] {
            assert!(
                line.starts_with("epicsMutexId 0x") && line.contains("record_lock.rs line "),
                "{line}"
            );
        }
    }

    /// Both counts are 0 while the records are loaded but `iocInit` has not
    /// run, because C builds the sets from the link graph at init and not at
    /// load — the C capture's first `dbLockShowLocked(0)`:
    ///
    /// ```text
    /// epics> dbLoadRecords("t9.db")
    /// epics> dbLockShowLocked(0)
    /// Active lockSets: 0
    /// Free lockSets: 0
    /// ```
    ///
    /// This arm is unreachable through `softioc-rs`, which runs `iocInit`
    /// before it hands over the shell, so it is only provable here.
    ///
    /// Neither pass header appears either: C guards both on `if(plockSet)`
    /// after `ellFirst(&lockSetsActive)` (`dbLock.c:953-958`), so an empty
    /// active list prints the two counts and nothing else — which is a
    /// different emptiness from the loaded-and-initialised IOC above, where
    /// `listTypeScanLock` heads an empty pass because sets exist and none is
    /// held.
    #[test]
    fn db_lock_show_locked_counts_nothing_before_ioc_init() {
        let (_db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t9.db");
        std::fs::write(
            &path,
            "record(calc, \"R:A\") { field(INPA, \"R:B\") field(CALC, \"A+1\") field(FLNK, \"R:B\") }\n\
             record(calc, \"R:B\") { field(CALC, \"1\") }\n\
             record(ai,   \"R:C\") { }\n",
        )
        .unwrap();
        run_cmd(&ctx, "dbLoadRecords", &[path.to_str().unwrap()]);

        let out = run_cmd(&ctx, "dbLockShowLocked", &["0"]);
        assert_eq!(
            out, "Active lockSets: 0\nFree lockSets: 0\n",
            "the sets are built at iocInit, not at dbLoadRecords"
        );
        assert_eq!(run_cmd(&ctx, "dblsr", &["*", "0"]), "");
    }

    /// A held set appears in the FIRST pass too — C's `epicsMutexTryLock`
    /// filter (`dbLock.c:963-965`) is the only thing separating the two
    /// passes.
    #[test]
    fn a_held_set_shows_in_the_scan_lock_pass() {
        let (db, ctx, _dir) = lock_set_ctx();
        let holder = db.clone();
        // A real thread: the gate blocks whoever takes it, and this thread has
        // to keep running to print the report.
        let (tx, rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let held = std::thread::spawn(move || {
            let _g = holder.lock_record("R:A");
            tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        rx.recv().unwrap();

        let out = run_cmd(&ctx, "dbLockShowLocked", &["0"]);
        let scan_pass: Vec<&str> = out
            .lines()
            .skip_while(|l| *l != "listTypeScanLock")
            .skip(1)
            .take_while(|l| *l != "listTypeRecordLock")
            .collect();
        assert_eq!(scan_pass.len(), 1, "exactly the held set:\n{out}");

        release_tx.send(()).unwrap();
        held.join().unwrap();
    }

    /// Level 1 is `epicsMutexShow`'s, not `dblsr`'s: it adds the OSD line
    /// (`os/posix/osdMutex.c:188-195`).
    #[test]
    fn db_lock_show_locked_level_1_adds_the_osd_line() {
        let (_db, ctx, _dir) = lock_set_ctx();
        let out = run_cmd(&ctx, "dbLockShowLocked", &["1"]);
        assert_eq!(
            out.matches(" uaddr=0x").count(),
            2,
            "one OSD line per active set:\n{out}"
        );
    }

    /// `dbior`'s four branches, one case per branch of `dbTest.c:723-744`:
    /// a driver with a report, a driver whose `report` slot is NULL, the
    /// name filter, and the interest level reaching `report()`.
    ///
    /// C's fifth branch — `No driver entry table is present for %s` — is
    /// unrepresentable here and has no case; see `cmd_dbior`.
    #[test]
    fn dbior_reports_every_driver_or_only_the_named_one() {
        use crate::server::driver_support::{DriverSupport, register_driver_support};

        struct Chatty;
        impl DriverSupport for Chatty {
            fn report(&self, level: i32) -> Option<String> {
                Some(format!("  chatty state, level {level}"))
            }
        }
        struct NoReport;
        impl DriverSupport for NoReport {
            fn report(&self, _level: i32) -> Option<String> {
                None
            }
        }

        let (_db, ctx) = make_ctx();
        register_driver_support("drvDbiorChatty", Arc::new(Chatty));
        register_driver_support("drvDbiorSilent", Arc::new(NoReport));

        let all = run_cmd(&ctx, "dbior", &[]);
        assert!(
            all.contains("Driver: drvDbiorChatty\n  chatty state, level 0\n"),
            "a driver with a report prints its header then the report: {all}"
        );
        assert!(
            all.contains("Driver: drvDbiorSilent No report available\n"),
            "a NULL report slot is one line and no header: {all}"
        );

        // C `strcmp(pdrvName, pname) != 0 -> continue` (`dbTest.c:729-731`).
        let one = run_cmd(&ctx, "dbior", &["drvDbiorChatty", "3"]);
        assert!(
            one.contains("Driver: drvDbiorChatty\n  chatty state, level 3\n"),
            "the interest level reaches report(): {one}"
        );
        assert!(
            !one.contains("drvDbiorSilent"),
            "a named driver excludes every other: {one}"
        );

        // C folds `*` to "all" (`dbTest.c:720-721`).
        assert!(run_cmd(&ctx, "dbior", &["*"]).contains("drvDbiorSilent"));
        // A name nothing registered prints nothing, as C's filtered walk does.
        assert_eq!(run_cmd(&ctx, "dbior", &["drvDbiorNoSuch"]), "");
    }

    /// `scanppl` prints the periodic lists and ONLY the periodic lists,
    /// in C's format.
    ///
    /// One case per boundary of `dbScan.c:379-408` + `printList`
    /// (`:969-991`), not one per story: the header carries the over-run
    /// count; a record on a non-periodic list (Event here, and Passive by
    /// construction for `IDLE` before it moves) appears nowhere, because C
    /// prints those from `scanpel`/`scanpiol`; a periodic rate with no
    /// records prints no header at all; the record lines are `    %-28s`;
    /// and the lists come out slowest-first, `papPeriodic`'s own index
    /// order.
    #[test]
    fn scanppl_prints_the_periodic_lists_and_only_those() {
        use crate::server::record::ScanType;
        use std::io::Write;
        use std::sync::Mutex;

        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("TICKER", Box::new(AiRecord::new(1.0)))
                .await
                .unwrap();
            db.add_record("IDLE", Box::new(AiRecord::new(1.0)))
                .await
                .unwrap();
        });
        db.get_record("TICKER").unwrap().write().common.scan = ScanType::SEC1;
        db.update_scan_index("TICKER", ScanType::Passive, ScanType::SEC1, 0, 0);
        db.get_record("IDLE").unwrap().write().common.scan = ScanType::Event;
        db.update_scan_index("IDLE", ScanType::Passive, ScanType::Event, 0, 0);
        for _ in 0..3 {
            db.record_scan_overrun(ScanType::SEC1);
        }
        // C's `papPeriodic` is built by `initPeriodic` at `iocInit`; before
        // that `scanppl` refuses. See `the_scan_reports_wait_for_scan_init`.
        ctx.block_on(db.ioc_init());

        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("scanppl").unwrap();
        let args = parse_args(&[], &cmd.args).unwrap();
        ctx.with_output(Sink(buf.clone()), || {
            cmd.handler.call(&args, &ctx).unwrap();
        });
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();

        assert!(
            out.contains("Records with SCAN = '1 second' (3 over-runs):\n"),
            "periodic list header carries its over-run count — got:\n{out}"
        );
        assert!(
            out.contains(&format!("    {:<28}\n", "TICKER")),
            "record lines are C's `    %-28s` — got:\n{out:?}"
        );
        // The Event list is `scanpel`'s to print (`dbScan.c:411-428`), never
        // `scanppl`'s.
        assert!(
            !out.contains("IDLE"),
            "a record on the Event list must not appear in scanppl — got:\n{out}"
        );
        assert!(
            !out.contains("Event"),
            "scanppl prints no Event list — got:\n{out}"
        );
        assert!(
            !out.contains("Passive"),
            "a Passive record is in no scan list at all — got:\n{out}"
        );
        assert!(
            !out.contains("I/O Intr"),
            "the I/O Intr lists are scanpiol's — got:\n{out}"
        );
        // C `printList` returns before printing anything when the list is
        // empty, so no other rate's header appears.
        assert_eq!(
            out.matches("Records with SCAN").count(),
            1,
            "only the one non-empty periodic list prints a header — got:\n{out}"
        );

        // Slowest-first: `papPeriodic[0]` is `SCAN_1ST_PERIODIC`. Put a
        // record on a second rate and check the order rather than asserting
        // on one list.
        db.get_record("IDLE").unwrap().write().common.scan = ScanType::SEC10;
        db.update_scan_index("IDLE", ScanType::Event, ScanType::SEC10, 0, 0);
        let buf2 = Arc::new(Mutex::new(Vec::new()));
        ctx.with_output(Sink(buf2.clone()), || {
            cmd.handler.call(&args, &ctx).unwrap();
        });
        let out2 = String::from_utf8(buf2.lock().unwrap().clone()).unwrap();
        let at10 = out2
            .find("SCAN = '10 second'")
            .unwrap_or_else(|| panic!("no 10 second list — got:\n{out2}"));
        let at1 = out2
            .find("SCAN = '1 second'")
            .unwrap_or_else(|| panic!("no 1 second list — got:\n{out2}"));
        assert!(
            at10 < at1,
            "papPeriodic order is slowest-first — got:\n{out2}"
        );
    }
    /// The three `dbScan` report commands read lists `scanInit` builds during
    /// `iocInit`, so before it C has nothing to walk — and says so in three
    /// different shapes.
    ///
    /// MEASURED on the built C `softIoc` (7.0.10.1-DEV) with one
    /// `SCAN="1 second"` calc, one `SCAN="Event"` calc and (second script) one
    /// `SCAN="I/O Intr"` ai:
    ///
    /// ```text
    /// epics> scanppl
    /// scanppl: dbScan subsystem not initialized
    /// epics> scanpel
    /// epics> scanpiol
    /// epics> iocBuild
    /// epics> scanppl
    /// Records with SCAN = '1 second' (0 over-runs):
    ///     P1
    /// ```
    ///
    /// The port printed all three lists before `iocInit`, because its reports
    /// read the SCAN-field index, which exists from load.
    #[test]
    fn the_scan_reports_wait_for_scan_init() {
        use crate::server::record::ScanType;

        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            for name in ["P1", "E1", "IO1"] {
                db.add_record(name, Box::new(AiRecord::new(1.0)))
                    .await
                    .unwrap();
            }
        });
        for (name, scan, evnt) in [
            ("P1", ScanType::SEC1, ""),
            ("E1", ScanType::Event, "alarm"),
            ("IO1", ScanType::IoIntr, ""),
        ] {
            {
                let rec = db.get_record(name).unwrap();
                let mut w = rec.write();
                w.common.scan = scan;
                w.common.evnt = evnt.to_string();
            }
            db.update_scan_index(name, ScanType::Passive, scan, 0, 0);
        }

        // C `dbScan.c:388-392`, and `scanpplCallFunc` is
        // `iocshSetError(scanppl(...))`, so the line failed too.
        assert_eq!(
            run_cmd_outcome(&ctx, "scanppl", &[]),
            (
                "scanppl: dbScan subsystem not initialized\n".to_string(),
                true
            )
        );
        // C `:414-428` / `:434-455` walk an empty list: no output, no refusal,
        // and the line did NOT fail.
        assert_eq!(
            run_cmd_outcome(&ctx, "scanpel", &[]),
            (String::new(), false)
        );
        assert_eq!(
            run_cmd_outcome(&ctx, "scanpiol", &[]),
            (String::new(), false)
        );

        ctx.block_on(db.ioc_init());

        let (out, failed) = run_cmd_outcome(&ctx, "scanppl", &[]);
        assert!(
            !failed,
            "an initialized scan subsystem does not fail the line"
        );
        assert!(
            out.contains("Records with SCAN = '1 second' (0 over-runs):"),
            "the gate must open at iocInit — got:\n{out}"
        );
        assert!(
            run_cmd(&ctx, "scanpel", &[]).contains("Event \"alarm\""),
            "so must scanpel's"
        );
    }

    /// C `scanpel` (`dbScan.c:411-428`) heads each event `Event "NAME"`,
    /// bands its records by `PRIO` into `priorityName[]` sub-lists, and
    /// prints each name through `printList` — which emits NOTHING for an
    /// empty band, so a missing priority line is the report, not a gap.
    /// The argument is an `epicsStrGlobMatch` pattern over the event name.
    #[test]
    fn scanpel_groups_by_event_then_priority_like_c() {
        use crate::server::record::ScanType;

        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            for name in ["E_LOW", "E_HIGH", "E_ALARM", "IO_MED"] {
                db.add_record(name, Box::new(AiRecord::new(1.0)))
                    .await
                    .unwrap();
            }
        });
        // Two records on event "5" at different priorities, spelled the
        // two ways `eventNameToHandle` folds together; one on "alarm".
        for (name, scan, evnt, prio) in [
            ("E_LOW", ScanType::Event, "5", 0i16),
            ("E_HIGH", ScanType::Event, " 5 ", 2),
            ("E_ALARM", ScanType::Event, "alarm", 0),
            ("IO_MED", ScanType::IoIntr, "", 1),
        ] {
            {
                let rec = db.get_record(name).unwrap();
                let mut w = rec.write();
                w.common.scan = scan;
                w.common.evnt = evnt.to_string();
                w.common.prio = prio;
            }
            db.update_scan_index(name, ScanType::Passive, scan, 0, 0);
        }
        // C's `pevent_list` is built by `scanAdd` at `iocInit`; before that
        // `scanpel` walks nothing. See `the_scan_reports_wait_for_scan_init`.
        ctx.block_on(db.ioc_init());

        let out = run_cmd(&ctx, "scanpel", &[]);
        assert!(out.contains("Event \"5\"\n"), "event header — got:\n{out}");
        assert!(
            out.contains("Event \"alarm\"\n"),
            "a non-numeric event name is an event too — got:\n{out}"
        );
        // C pads each name to 28 columns behind four spaces.
        assert!(
            out.contains(&format!(" Priority Low\n    {:<28}\n", "E_LOW")),
            "PRIO 0 lands in C's Low band, 28-column padded — got:\n{out}"
        );
        assert!(
            out.contains(&format!(" Priority High\n    {:<28}\n", "E_HIGH")),
            "PRIO 2 lands in C's High band — got:\n{out}"
        );
        assert!(
            !out.contains("Priority Medium"),
            "printList prints nothing for an empty band — got:\n{out}"
        );
        assert!(
            !out.contains("IO_MED"),
            "an I/O Intr record is not on any event list — got:\n{out}"
        );

        // The argument is a glob PATTERN over the event name, not a key.
        let globbed = run_cmd(&ctx, "scanpel", &["al*"]);
        assert!(globbed.contains("Event \"alarm\""), "got:\n{globbed}");
        assert!(!globbed.contains("Event \"5\""), "got:\n{globbed}");
        // A pattern that matches nothing prints nothing — C has no
        // "no such event" line.
        assert_eq!(run_cmd(&ctx, "scanpel", &["nosuch"]), "");

        // `scanpiol` reports the I/O Intr list, banded the same way.
        let io = run_cmd(&ctx, "scanpiol", &[]);
        assert!(
            io.contains(&format!(
                "IO Event: Priority Medium\n    {:<28}\n",
                "IO_MED"
            )),
            "PRIO 1 lands in C's Medium band — got:\n{io}"
        );
        assert!(
            !io.contains("Priority Low") && !io.contains("Priority High"),
            "empty bands print nothing — got:\n{io}"
        );
    }

    /// C `gft` (`db_test.c:35-83`) on an `ai`.
    ///
    /// Every expected line here was MEASURED against
    /// `bin/linux-x86_64/softIoc` (R7.0.10-146-g8f5015b663d764ad75df) on the
    /// same database, not derived from the C source. One column differs and is
    /// not gft's: C's record starts at `STAT=UDF, SEVR=NO_ALARM` (`STAT`'s dbd
    /// `initial("UDF")` with no `SEVR` initial, dbCommon.dbd.pod:296-306) and
    /// this port's starts at severity INVALID, so the status pair reads
    /// `17  3` here against C's `17  0` — identically through `dbgf`, `dbtgf`
    /// and CA.
    /// `dbAccess.c` seeds the reply's three limit groups differently, and a
    /// record type with no `get_alarm_double` is where that shows: the
    /// display and control pairs are `memset` to zero, the alarm four keep
    /// `struct dbr_alDouble ald = {epicsNAN, …}` (`:294`, `:318-323`). The
    /// integral options are the same four through `finite(x) ? (epicsInt32) x
    /// : 0` (`:305-312`), so they read zero.
    ///
    /// Measured on `bin/linux-x86_64/softIoc` (EPICS 7.0.10),
    /// `gft B:ONE` on `record(bo, "B:ONE")`, the six limits being
    /// `upper_disp, lower_disp, upper_alarm, upper_warning, lower_warning,
    /// lower_alarm` followed by the control pair.
    #[test]
    fn a_record_type_with_no_alarm_slot_dumps_four_nans_on_the_real_families() {
        let (db, ctx) = make_ctx();
        load_records(&ctx, r#"record(bo, "B:ONE") { }"#).expect("bo must load");
        ctx.block_on(db.ioc_init());

        let out = run_cmd(&ctx, "gft", &["B:ONE"]);

        for line in [
            // NaN survives into the reply for every real family…
            "\nDBR_GR_DOUBLE\t17  3    0\n\t   0.000    0.000      nan      nan      nan      nan",
            "\nDBR_GR_FLOAT\t17  3    0\n\t   0.000    0.000      nan      nan      nan      nan",
            "\nDBR_CTRL_DOUBLE\t17  3    0\n\t   0.000    0.000      nan      nan      nan      nan 65535.000    0.000",
            // …and is the ONE group C guards on the way to an integer, so
            // these read zero rather than whatever a NaN cast produces. The
            // control pair is not guarded: 65535 truncates modularly into
            // `dbr_short_t` and `dbr_char_t`.
            "\nDBR_GR_SHORT\t17  3 \n\t       0        0        0        0        0        0",
            "\nDBR_CTRL_SHORT\t17  3 \n\t       0        0        0        0        0        0       -1        0",
            "\nDBR_CTRL_CHAR\t17  3 \n\t       0        0        0        0        0        0      255        0",
        ] {
            assert!(out.contains(line), "missing {line:?} in:\n{out}");
        }
    }

    /// C `gft` prints the element count it ASKED the channel for, and
    /// `dbChannel_get` guarantees the buffer holds that many by zero-filling
    /// what the database did not return (`db_access.c:127-136`). So an
    /// unprocessed waveform — `NORD` 0 against `NELM` 3 — dumps three zeros
    /// per numeric family, not an empty list.
    ///
    /// Measured on `bin/linux-x86_64/softIoc` (EPICS 7.0.10),
    /// `gft V:ONE` on `record(waveform, "V:ONE") { FTVL=DOUBLE NELM=3 }`.
    #[test]
    fn an_unprocessed_waveform_dumps_nelm_zeros_not_an_empty_list() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            r#"record(waveform, "V:ONE") { field(FTVL, "DOUBLE") field(NELM, "3") }"#,
        )
        .expect("waveform must load");
        ctx.block_on(db.ioc_init());

        let out = run_cmd(&ctx, "gft", &["V:ONE"]);

        assert!(out.contains("   No Elements: 3\n"), "got:\n{out}");
        for line in [
            "\nDBR_SHORT\t\n0 0 0 \n",
            "\nDBR_FLOAT\t\n0.0000 0.0000 0.0000 \n",
            "\nDBR_ENUM\t\n0 0 0 \n",
            "\nDBR_CHAR\t\n0 0 0 \n",
            "\nDBR_LONG\t\n0 0 0 \n",
            "\nDBR_DOUBLE\t\n0.0000 0.0000 0.0000 \n",
            // The status and time families carry the same three elements
            // behind their own header.
            "\nDBR_STS_LONG\t17  3\n0 0 0 \n",
            "\nDBR_TIME_DOUBLE\t17  3\tTimeStamp: <undefined>\n0.0000 0.0000 0.0000 \n",
            // `DBR_CTRL_CHAR` alone pads each element to `%4d`.
            "\n   0    0    0 \n",
            // Six decimals for `DBR_CTRL_DOUBLE`, four everywhere else.
            "\n0.000000 0.000000 0.000000 \n",
        ] {
            assert!(out.contains(line), "missing {line:?} in:\n{out}");
        }
        // C's `DBR_STRING` loop stops at the first empty element, and every
        // element of a zero-filled string buffer is empty.
        assert!(out.contains("\nDBR_STRING\t\nDBR_SHORT"), "got:\n{out}");
        // `\tValue: ` is the SCALAR prefix; a three-element reply has none.
        assert!(
            !out.contains("\nDBR_STS_LONG\t17  3\tValue:"),
            "got:\n{out}"
        );
    }

    #[test]
    fn gft_dumps_every_dbr_request_type_the_way_c_does() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("R:AI", Box::new(AiRecord::new(25.5)))
                .await
                .unwrap();
            for (field, value) in [
                ("EGU", EpicsValue::String("millimetres".into())),
                ("PREC", EpicsValue::Short(3)),
                ("HOPR", EpicsValue::Double(100.0)),
                ("LOPR", EpicsValue::Double(-100.0)),
            ] {
                db.put_pv(&format!("R:AI.{field}"), value).await.unwrap();
            }
            db.ioc_init().await;
        });

        let out = run_cmd(&ctx, "gft", &["R:AI"]);

        // The header. C's format is `0x%p`, so the `0x` really is doubled;
        // `Field Address` is the one C column this port cannot have.
        assert!(
            out.starts_with("   Record Name: R:AI\nRecord Address: 0x0x"),
            "got:\n{out}"
        );
        assert!(
            out.contains(
                "   Export Type: 6\n Field Address: (none)\n    Field Size: 8\n   No Elements: 1\n"
            ),
            "got:\n{out}"
        );

        // The `DBR_STRING` row is the RECORD's precision, not the value's —
        // C prints `25.500` for PREC=3 where `dbgf` prints `25.5`.
        assert!(out.contains("\nDBR_STRING\t25.500 \n"), "got:\n{out}");
        // `%6.4f` on the reals, plain `%d`/`%u` on the integers.
        assert!(out.contains("\nDBR_FLOAT\t25.5000 \n"), "got:\n{out}");
        assert!(out.contains("\nDBR_DOUBLE\t25.5000 \n"), "got:\n{out}");
        assert!(out.contains("\nDBR_CHAR\t25 \n"), "got:\n{out}");
        // `\tValue: ` appears only because this is a scalar.
        assert!(
            out.contains("\nDBR_STS_DOUBLE\t17  3\tValue: 25.5000 \n"),
            "got:\n{out}"
        );

        // The graphic block: `%.8s` over a buffer C already cut at seven
        // bytes, `%3d` precision, then the six limits. `LOPR=-100` reaching a
        // `dbr_char_t` wraps to 156, and the four alarm limits are NaN because
        // `aiRecord::get_alarm_double` answers NaN for an unset severity —
        // which the LONG families then read as C's `finite(x) ? (int)x : 0`.
        assert!(
            out.contains("\nDBR_GR_SHORT\t17  3 millime\n\t     100     -100        0        0        0        0\tValue: 25 \n"),
            "got:\n{out}"
        );
        assert!(
            out.contains("\nDBR_GR_CHAR\t17  3 millime\n\t     100      156 "),
            "the char family narrows -100 to 156 — got:\n{out}"
        );
        assert!(
            out.contains("\nDBR_GR_DOUBLE\t17  3 millime   3\n\t 100.000 -100.000      nan      nan      nan      nan\tValue: 25.5000 \n"),
            "got:\n{out}"
        );
        // CTRL adds the two control limits after the six, and `DBR_CTRL_CHAR`
        // is the one family that pads its elements to `%4d`.
        assert!(
            out.contains("        0        0      100      156\tValue:   25 \n"),
            "got:\n{out}"
        );
        // `DBR_CTRL_DOUBLE` is the one family that prints SIX decimals.
        assert!(out.contains("\tValue: 25.500000 \n"), "got:\n{out}");

        // The three tail types: both `DBR_PUT_*` are C's `default: status=-1`,
        // `DBR_STSACK_STRING` carries ACKT/ACKS, and `DBR_CLASS_NAME` is the
        // record TYPE.
        assert!(
            out.contains("\n\tDBR_PUT_ACKT Failed\n\tDBR_PUT_ACKS Failed\n"),
            "got:\n{out}"
        );
        assert!(
            out.contains("\nDBR_STSACK_STRING\t17  3  1  0 25.500\n"),
            "got:\n{out}"
        );
        assert!(out.ends_with("\nDBR_CLASS_NAME\tai\n"), "got:\n{out}");
    }

    /// A channel that exports as `DBR_STRING` asks for the five STRING request
    /// types and nothing else (`db_test.c:69-74`), and an enum-valued field
    /// renders its CHOICE rather than its index. Both MEASURED on the same
    /// `softIoc`.
    #[test]
    fn gft_skips_the_numeric_types_on_a_string_channel_and_labels_an_enum() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("R:AI", Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
            db.ioc_init().await;
        });

        let name = run_cmd(&ctx, "gft", &["R:AI.NAME"]);
        assert!(name.contains("   Export Type: 0\n"), "got:\n{name}");
        assert!(name.contains("    Field Size: 61\n"), "got:\n{name}");
        assert_eq!(
            name.lines().filter(|l| l.starts_with("DBR_")).count(),
            5,
            "only the five STRING request types — got:\n{name}"
        );
        assert!(name.contains("\nDBR_STRING\tR:AI \n"), "got:\n{name}");
        assert!(
            !name.contains("DBR_SHORT") && !name.contains("DBR_DOUBLE"),
            "got:\n{name}"
        );

        // A `DBF_MENU` field exports as `DBR_ENUM`, so every type is asked
        // for, and C's `getMenuString` row prints the choice.
        let scan = run_cmd(&ctx, "gft", &["R:AI.SCAN"]);
        assert!(scan.contains("   Export Type: 3\n"), "got:\n{scan}");
        assert!(scan.contains("\nDBR_STRING\tPassive \n"), "got:\n{scan}");
        assert!(scan.contains("\tValue: Passive\n"), "got:\n{scan}");
        // `DBR_GR_ENUM` carries the menu's ten choices, `%3d` count first.
        assert!(
            scan.contains("\tValue: 0\n\t 10\n\tPassive\n\tEvent\n\tI/O Intr\n"),
            "got:\n{scan}"
        );
    }

    /// C `gft` with no argument prints its usage and fails the line
    /// (`db_test.c:44-47`), and an unknown PV gets `dbChannel_create`'s own
    /// message, not the `dbNameToAddr` one `dba`/`dbgf` print.
    #[test]
    fn gft_reports_a_missing_argument_and_an_unknown_pv_the_way_c_does() {
        let (_db, ctx) = make_ctx();

        let (out, failed) = run_cmd_outcome(&ctx, "gft", &[]);
        assert_eq!(out, "Usage: gft \"pv_name\"\n");
        assert!(failed, "C returns -1");

        let (out, failed) = run_cmd_outcome(&ctx, "gft", &["NO:SUCH"]);
        assert_eq!(out, "Channel couldn't be created\n");
        assert!(failed, "C returns 1");
    }

    /// A waveform takes the element loop's other half: no `\tValue: ` prefix,
    /// a newline before every tenth element (fifth for strings), and C's
    /// `MAX_ELEMS` clamp of ten (`db_test.c:65-66`) applied to the request but
    /// NOT to the `No Elements` header.
    ///
    /// MEASURED on the same `softIoc` against a `waveform` with `FTVL=CHAR`,
    /// `NELM=32`, after `dbpf("R:WFC", "ABCDEFGHIJKLMNO")`.
    #[test]
    fn gft_clamps_a_waveform_to_ten_elements_and_wraps_them() {
        use crate::server::records::waveform::WaveformRecord;

        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record(
                "R:WFC",
                Box::new(WaveformRecord::new(32, crate::types::DbFieldType::Char)),
            )
            .await
            .unwrap();
            db.ioc_init().await;
            // C's `dbpf` is `dbPutField`, which PROCESSES the waveform and so
            // clears `UDF`; `put_pv` is bare `dbPut` and would leave the
            // record undefined, which is a different record state, not a
            // different `gft`. Go through the same entry `cmd_dbpf` uses.
            //
            // C stores the 15 characters straight from the string token. This
            // port's `EpicsValue::parse(Char, "ABC...")` refuses a non-numeric
            // string, so the bytes are handed over already split; that put-path
            // gap belongs to `types/c_parse.rs`, not to `db_test.c`.
            db.put_record_field_from_ca_no_notify(
                "R:WFC",
                "VAL",
                EpicsValue::CharArray(b"ABCDEFGHIJKLMNO".to_vec()),
            )
            .await
            .unwrap();
        });

        let out = run_cmd(&ctx, "gft", &["R:WFC"]);
        assert!(
            out.contains(
                "   Export Type: 4\n Field Address: (none)\n    Field Size: 1\n   No Elements: 32\n"
            ),
            "the header carries NELM, unclamped — got:\n{out}"
        );
        // Strings wrap at five; a `DBF_CHAR` waveform read as `DBR_STRING` is
        // one 40-byte slot per BYTE holding that byte's decimal text.
        assert!(
            out.contains("\nDBR_STRING\t\n65 66 67 68 69 \n70 71 72 73 74 \n"),
            "got:\n{out}"
        );
        // Everything else wraps at ten, and stops at ten.
        assert!(
            out.contains("\nDBR_SHORT\t\n65 66 67 68 69 70 71 72 73 74 \n"),
            "got:\n{out}"
        );
        assert!(
            out.contains("\nDBR_DOUBLE\t\n65.0000 66.0000 67.0000 68.0000 69.0000 70.0000 71.0000 72.0000 73.0000 74.0000 \n"),
            "got:\n{out}"
        );
        // A vector never gets the scalar `\tValue: ` prefix — except
        // `DBR_STS_STRING`, whose C branch prints one string unconditionally.
        assert!(
            out.contains("\nDBR_STS_SHORT\t 0  0\n65 66 "),
            "got:\n{out}"
        );
        assert!(
            out.contains("\nDBR_STS_STRING\t 0  0\tValue: 65\n"),
            "got:\n{out}"
        );
    }

    /// C `pft` (`db_test.c:85-186`) on a numeric field.
    ///
    /// MEASURED against `bin/linux-x86_64/softIoc`
    /// (R7.0.10-146-g8f5015b663d764ad75df) on the same database. Every rung
    /// PUTS, so the seven rows are seven different values of the field, and
    /// the ladder ends leaving the enum rung's write behind.
    #[test]
    fn pft_walks_the_whole_ladder_on_a_numeric_field() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("R:AI", Box::new(AiRecord::new(25.5)))
                .await
                .unwrap();
            db.put_pv("R:AI.PREC", EpicsValue::Short(3)).await.unwrap();
            db.ioc_init().await;
        });

        let out = run_cmd(&ctx, "pft", &["R:AI", "12.7"]);
        assert!(
            out.contains(
                "   Export Type: 6\n Field Address: (none)\n    Field Size: 8\n   No Elements: 1\n"
            ),
            "the header is `gft`'s — got:\n{out}"
        );
        // C's order: LONG comes before FLOAT, which the DBR numbering has the
        // other way round. `%hd`/`%ld` take the integer prefix of "12.7", so
        // the two integer rungs write 12 and the field reads back 12.
        assert!(
            out.contains(concat!(
                "DBR_STRING\t12.700 \n",
                "DBR_SHORT\t12 \n",
                "DBR_LONG\t12 \n",
                "DBR_FLOAT\t12.7000 \n",
                "DBR_DOUBLE\t12.7000 \n",
                "DBR_CHAR\t12 \n",
                "DBR_ENUM\t12 \n",
            )),
            "got:\n{out}"
        );
        // C's closing `printf("\n")`, which only the full ladder reaches.
        assert!(out.ends_with("DBR_ENUM\t12 \n\n"), "got:\n{out}");
    }

    /// A put the database refuses prints C's un-terminated `"\n\t failed "`
    /// fragment and the GET that follows continues that same line
    /// (`db_test.c:122-125`).
    ///
    /// MEASURED on the same `softIoc`: both halves of this test are C output,
    /// including the missing type name on the string rung.
    #[test]
    fn pft_reports_a_refused_put_and_still_reads_the_field_back() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("R:AI", Box::new(AiRecord::new(12.0)))
                .await
                .unwrap();
            db.put_pv("R:AI.PREC", EpicsValue::Short(3)).await.unwrap();
            db.ioc_init().await;
        });

        // "abc" converts to no numeric type, so the string put fails and
        // every guarded rung is skipped — one dump, of the untouched field.
        let out = run_cmd(&ctx, "pft", &["R:AI", "abc"]);
        assert!(
            out.ends_with("   No Elements: 1\n\n\t failed DBR_STRING\t12.000 \n\n"),
            "got:\n{out}"
        );

        // `NAME` is `special(SPC_NOMOD)`, so the put is refused where the
        // conversion would have worked — and the channel exports as
        // `DBR_STRING`, which returns before C's closing newline.
        let name = run_cmd(&ctx, "pft", &["R:AI.NAME", "x"]);
        assert!(
            name.ends_with(
                "    Field Size: 61\n   No Elements: 1\n\n\t failed DBR_STRING\tR:AI \n"
            ),
            "got:\n{name}"
        );
    }

    /// C `db_test.c:127`: a channel exporting as `DBR_STRING` or `DBR_ENUM`
    /// stops after the string rung — the six typed rungs below it would put
    /// through a conversion the field does not have.
    ///
    /// MEASURED on the same `softIoc`.
    #[test]
    fn pft_stops_after_the_string_rung_on_a_string_or_enum_channel() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("R:AI", Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
            db.ioc_init().await;
        });

        // A `DBF_MENU` field: the string put resolves the choice by index and
        // the read-back is the label, not the number.
        let scan = run_cmd(&ctx, "pft", &["R:AI.SCAN", "1"]);
        assert!(
            scan.contains("   Export Type: 3\n Field Address: (none)\n    Field Size: 2\n"),
            "got:\n{scan}"
        );
        assert!(
            scan.ends_with("   No Elements: 1\nDBR_STRING\tEvent \n"),
            "got:\n{scan}"
        );
        assert!(
            !scan.contains("DBR_SHORT"),
            "the ladder stops — got:\n{scan}"
        );

        // A `DBF_STRING` field, C's other early return.
        let egu = run_cmd(&ctx, "pft", &["R:AI.EGU", "cm"]);
        assert!(egu.contains("   Export Type: 0\n"), "got:\n{egu}");
        assert!(egu.ends_with("\nDBR_STRING\tcm \n"), "got:\n{egu}");
        assert!(!egu.contains("DBR_DOUBLE"), "got:\n{egu}");
    }

    /// The ladder on an ARRAY channel: `pft` always puts and gets ONE
    /// element, so a waveform takes the same seven rungs a scalar does, and
    /// only the header shows it is an array.
    ///
    /// MEASURED on the same `softIoc` against `FTVL=CHAR`, `NELM=32`.
    #[test]
    fn pft_puts_one_element_of_a_waveform() {
        use crate::server::records::waveform::WaveformRecord;

        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record(
                "R:WFC",
                Box::new(WaveformRecord::new(32, crate::types::DbFieldType::Char)),
            )
            .await
            .unwrap();
            db.ioc_init().await;
        });

        let out = run_cmd(&ctx, "pft", &["R:WFC", "77"]);
        assert!(
            out.contains(
                "   Export Type: 4\n Field Address: (none)\n    Field Size: 1\n   No Elements: 32\n"
            ),
            "got:\n{out}"
        );
        assert!(
            out.ends_with(concat!(
                "DBR_STRING\t77 \n",
                "DBR_SHORT\t77 \n",
                "DBR_LONG\t77 \n",
                "DBR_FLOAT\t77.0000 \n",
                "DBR_DOUBLE\t77.0000 \n",
                "DBR_CHAR\t77 \n",
                "DBR_ENUM\t77 \n\n",
            )),
            "got:\n{out}"
        );
    }

    /// C's two refusals (`db_test.c:100-107`), byte for byte.
    #[test]
    fn pft_reports_a_missing_argument_and_an_unknown_pv_the_way_c_does() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("R:AI", Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
            db.ioc_init().await;
        });

        assert_eq!(
            run_cmd(&ctx, "pft", &[]),
            "Usage: pft \"pv_name\", \"value\"\n"
        );
        assert_eq!(
            run_cmd(&ctx, "pft", &["R:AI"]),
            "Usage: pft \"pv_name\", \"value\"\n"
        );
        assert_eq!(
            run_cmd(&ctx, "pft", &["R:NOSUCH", "1"]),
            "Channel couldn't be created\n"
        );
    }

    /// Run `handler` with the context's stdout sink pointed at a temp
    /// file and return what it wrote.
    fn capture(ctx: &CommandContext, f: impl FnOnce()) -> String {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        ctx.with_output(std::fs::File::create(&path).unwrap(), f);
        std::fs::read_to_string(&path).unwrap()
    }

    fn run_cmd(ctx: &CommandContext, name: &str, tokens: &[&str]) -> String {
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get(name).unwrap();
        let tokens: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
        let args = parse_args(&tokens, &cmd.args).unwrap();
        capture(ctx, || {
            cmd.handler.call(&args, ctx).unwrap();
        })
    }

    /// Run one command and report whether it FAILED the line, alongside
    /// what it printed — `run_cmd` throws the outcome away, and the
    /// `iocshSetError` parity below is entirely about the outcome.
    fn run_cmd_outcome(ctx: &CommandContext, name: &str, tokens: &[&str]) -> (String, bool) {
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get(name).unwrap();
        let tokens: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
        let args = parse_args(&tokens, &cmd.args).unwrap();
        let mut failed = false;
        let printed = capture(ctx, || {
            failed = matches!(cmd.handler.call(&args, ctx), Ok(CommandOutcome::Failed));
        });
        (printed, failed)
    }

    /// C `printDbAddr` (`dbTest.c:795-818`) on the declaration shapes the
    /// port's two type fields disagree about.
    ///
    /// Every expected string here was MEASURED against `softIoc`
    /// linux-x86_64 on the same two records, not derived from the C source:
    /// C prints the identical seven lines for `R:AI`, `R:AI.VAL`,
    /// `R:AI.INP`, `R:AI.SCAN` and `R:AI.NAME`, differing only in the three
    /// pointers and in `Field Size: 80` for the link, which is
    /// `sizeof(DBLINK)`.
    ///
    /// The point of the `ai.INP` case is that `dba` must print the DECLARED
    /// token: `FieldDesc::dbf_type` says `String` for a link because that is
    /// what the port SERVES, and printing `0 = DBF_STRING` there would be a
    /// different C fact (`dbDumpField`'s) under `dba`'s heading.
    #[test]
    fn dba_prints_the_declared_type_not_the_served_one() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            "record(ai, \"R:AI\") { field(DTYP, \"Soft Channel\") field(INP, \"R:LO\") }\n\
             record(longout, \"R:LO\") { field(DTYP, \"Soft Channel\") }",
        )
        .unwrap();
        ctx.block_on(db.ioc_init());

        let lines = |pv: &str| -> Vec<String> {
            run_cmd(&ctx, "dba", &[pv])
                .lines()
                .map(str::to_string)
                .collect()
        };

        // `ai.VAL` — a plain DBF_DOUBLE, every column a number.
        let val = lines("R:AI");
        assert_eq!(val[1], "   No Elements: 1", "{val:?}");
        assert_eq!(val[2], "   Record Type: ai", "{val:?}");
        assert_eq!(val[3], "    Field Type: 10 = DBF_DOUBLE", "{val:?}");
        assert_eq!(val[4], "    Field Size: 8", "{val:?}");
        assert_eq!(val[5], "       Special: 0", "{val:?}");
        assert_eq!(val[6], "DBR Field Type: 10 = DBR_DOUBLE", "{val:?}");

        // `ai.INP` — DBF_INLINK (14), which `mapDBFToDBR` folds onto
        // DBR_STRING (0), and whose C size is `sizeof(DBLINK)`.
        let inp = lines("R:AI.INP");
        assert_eq!(inp[3], "    Field Type: 14 = DBF_INLINK", "{inp:?}");
        assert_eq!(inp[4], "    Field Size: (none)", "{inp:?}");
        assert_eq!(inp[5], "       Special: 0", "{inp:?}");
        assert_eq!(inp[6], "DBR Field Type: 0 = DBR_STRING", "{inp:?}");

        // `dbCommon.SCAN` — DBF_MENU (12), folded onto DBR_ENUM (11), and
        // `epicsEnum16`-wide. Served as `Enum`, so this is the case where
        // the two numberings agree on the DBR line and disagree on the DBF
        // one.
        let scan = lines("R:AI.SCAN");
        assert_eq!(scan[3], "    Field Type: 12 = DBF_MENU", "{scan:?}");
        assert_eq!(scan[4], "    Field Size: 2", "{scan:?}");
        // `special(SPC_SCAN)` (`dbCommon.dbd.pod:165`) reaching the shell as
        // C's number 3 — the SPC_ numbering, end to end.
        assert_eq!(scan[5], "       Special: 3", "{scan:?}");
        assert_eq!(scan[6], "DBR Field Type: 11 = DBR_ENUM", "{scan:?}");

        // `dbCommon.NAME` — DBF_STRING, whose width is the `.dbd` `size(N)`
        // and not a property of the type.
        let name = lines("R:AI.NAME");
        assert_eq!(name[3], "    Field Type: 0 = DBF_STRING", "{name:?}");
        assert_eq!(name[4], "    Field Size: 61", "{name:?}");
        // `special(SPC_NOMOD)` (`dbCommon.dbd.pod:31`).
        assert_eq!(name[5], "       Special: 1", "{name:?}");
        assert_eq!(name[6], "DBR Field Type: 0 = DBR_STRING", "{name:?}");
    }

    /// The two pointers the port DOES have carry C's meaning: two names for
    /// one record share a record address, two records of one type share a
    /// field description, and the field address is the column with no
    /// counterpart.
    #[test]
    fn dbas_pointers_answer_identity_the_way_cs_do() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            "record(ai, \"R:A\") { field(DTYP, \"Soft Channel\") }\n\
             record(ai, \"R:B\") { field(DTYP, \"Soft Channel\") }",
        )
        .unwrap();
        ctx.block_on(db.ioc_init());

        let head = |pv: &str| {
            run_cmd(&ctx, "dba", &[pv])
                .lines()
                .next()
                .unwrap()
                .to_string()
        };
        let a_val = head("R:A");
        let a_again = head("R:A.VAL");
        let b_val = head("R:B");

        // An omitted field is VAL (`dbIocRegister.c:190`), so these are one
        // address pair.
        assert_eq!(a_val, a_again);
        // Different records, different record address...
        assert_ne!(a_val, b_val, "two records shared a record address");
        // ...but one `&'static FieldDesc`, because the declaration is the
        // record TYPE's.
        let desc = |l: &str| l.split("Field Description: ").nth(1).unwrap().to_string();
        assert_eq!(desc(&a_val), desc(&b_val));
        assert!(a_val.contains("Field Address: (none)"), "{a_val:?}");
    }

    /// Every `dbTest.c` command is registered through `iocshSetError`
    /// (`dbIocRegister.c:209`, `:247`, `:262`, `:273`, `:291`), so a non-zero
    /// return fails the line — which under `on error break` / `on error halt`
    /// stops the startup script. The usage and unresolved-name arms all
    /// return non-zero: `dbgf` 1/-1 (`dbTest.c:359-364`), `dbpf` 1/-1
    /// (`:401-406`), `dbpr` 1/-1 (`:445-450`), `dbglob` 1 (`:308-309`).
    ///
    /// The printed text is unchanged and is re-asserted here so a later edit
    /// cannot trade one for the other.
    #[test]
    fn the_db_test_commands_fail_the_line_where_c_returns_non_zero() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            "record(longout, \"R:LO\") { field(DTYP, \"Soft Channel\") }",
        )
        .unwrap();
        ctx.block_on(db.ioc_init());

        for (cmd, tokens, expected) in [
            ("dbgf", &[][..], "Usage: dbgf \"pv name\"\n"),
            ("dbgf", &["R:NOPE"][..], "PV 'R:NOPE' not found\n"),
            ("dbpf", &[][..], "Usage: dbpf \"pv name\", \"value\"\n"),
            ("dbpf", &["R:NOPE", "1"][..], "PV 'R:NOPE' not found\n"),
            ("dbpr", &[][..], "Usage: dbpr \"pv name\", level\n"),
            ("dbpr", &["R:NOPE"][..], "PV 'R:NOPE' not found\n"),
            ("dbglob", &[][..], "Usage: dbglob \"pattern\" \"fields\"\n"),
            ("dbgrep", &[][..], "Usage: dbglob \"pattern\" \"fields\"\n"),
        ] {
            let (printed, failed) = run_cmd_outcome(&ctx, cmd, tokens);
            assert_eq!(printed, expected, "{cmd} {tokens:?}");
            assert!(failed, "{cmd} {tokens:?} must fail the line");
        }

        // The success arms still succeed.
        let (printed, failed) = run_cmd_outcome(&ctx, "dbgf", &["R:LO"]);
        assert!(!failed, "a resolved dbgf must not fail the line");
        assert!(printed.starts_with("DBF_LONG:"), "got {printed:?}");
        let (_, failed) = run_cmd_outcome(&ctx, "dbglob", &["R:*"]);
        assert!(!failed, "a matching dbglob must not fail the line");
    }

    /// A refused `dbpf` says nothing of its own.
    ///
    /// `dbpfCallFunc` is `iocshSetError(dbpf(...))` (`dbIocRegister.c:272-273`)
    /// and `iocshSetError` only sets `scope.errored` (`iocsh.cpp:1004-1018`),
    /// so C's shell prints not one word for a non-zero `dbpf`. The three
    /// refusals below were measured on `softIoc` R7.0.10-146 with stderr
    /// captured separately: a bad number, a read-only field and a bad link
    /// each leave stderr EMPTY, and the only output is the read-back
    /// `dbgf` line `dbpf` ends with whatever `dbPutField` returned
    /// (`dbTest.c:433`) — showing the value the record kept.
    #[test]
    fn a_refused_dbpf_prints_only_its_read_back_and_fails_the_line() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            "record(ai, \"P:A\") { field(DTYP, \"Soft Channel\") field(INP, \"P:B\") }\n\
             record(ai, \"P:B\") { field(VAL, \"3\") }\n",
        )
        .unwrap();
        ctx.block_on(db.ioc_init());

        for (pv, value, read_back) in [
            ("P:A.PREC", "abc", "DBF_SHORT:          0 = 0x0   \n"),
            ("P:A.RTYP", "x", "DBF_STRING:         \"ai\"      \n"),
            (
                "P:A.INP",
                "@bogus port",
                "DBF_STRING:         \"P:B NPP NMS\"       \n",
            ),
        ] {
            let (printed, failed) = run_cmd_outcome(&ctx, "dbpf", &[pv, value]);
            assert_eq!(printed, read_back, "dbpf {pv} {value:?}");
            assert!(failed, "dbpf {pv} {value:?} must fail the line");
        }
    }

    /// The one refusal that DOES speak is the converter's, not `dbpf`'s.
    ///
    /// C `dbPutConvertJSON` errlogs `dbConvertJSON: %s`
    /// (`dbConvertJSON.c:170-176`) and returns `S_db_badField`, which
    /// `dbTest.c:425-426` hands back before `dbPutField` and before the
    /// closing `dbgf` — so the operator sees an unframed errlog block, stdout
    /// stays empty, and the array keeps its old contents. Measured on
    /// `softIoc`: `dbpf("P:W.VAL","[1,2,zz]")` prints nothing on stdout and
    /// three `dbConvertJSON:` lines on stderr. The port's converter reports
    /// through `serde_json`, so only the prefix is asserted here; the text
    /// after it belongs to `dbConvertJSON.c`'s port.
    #[test]
    fn a_refused_json_array_answers_on_the_errlog_and_leaves_the_field() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            "record(waveform, \"P:W\") { field(FTVL, \"LONG\") field(NELM, \"4\") }\n",
        )
        .unwrap();
        ctx.block_on(db.ioc_init());

        let good = run_cmd(&ctx, "dbpf", &["P:W.VAL", "[1,2]"]);
        assert_eq!(good, "DBF_LONG[2]:        1 = 0x1   2 = 0x2   \n");

        let buf = CaptureBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_max_level(tracing::Level::INFO)
            .with_target(false)
            .without_time()
            .finish();
        let (printed, failed) = {
            let _guard = tracing::subscriber::set_default(subscriber);
            run_cmd_outcome(&ctx, "dbpf", &["P:W.VAL", "[1,2,zz]"])
        };
        assert_eq!(
            printed, "",
            "C prints the refusal on the errlog, not stdout"
        );
        assert!(failed, "a refused JSON literal fails the line");
        let logged = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
        assert!(
            logged.contains("dbConvertJSON: "),
            "the converter's own line must survive, got: {logged:?}"
        );

        // The refused literal never reached `dbPutField`.
        assert_eq!(
            run_cmd(&ctx, "dbgf", &["P:W.VAL"]),
            "DBF_LONG[2]:        1 = 0x1   2 = 0x2   \n"
        );
    }

    /// A `tracing` writer that keeps every formatted event, so a test
    /// can read back what the `errlog` sink received.
    #[derive(Clone, Default)]
    struct CaptureBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureBuf {
        type Writer = CaptureBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// `astac` on a record the database does not hold answers through
    /// `errMessage(status, "dbNameToAddr error")` (`asDbLib.c:249-252`),
    /// which `errlog.h:86-87` + `errlog.c:503-508` render as
    /// `<errSym> filename="<f>" line number=<n>  <msg>` on the errlog
    /// stream — measured on `softIoc` R7.0.10-146 as `Record Not Found
    /// filename="../as/asDbLib.c" line number=251  dbNameToAddr error`.
    /// Stdout therefore stays empty, and the file and line are this
    /// crate's, because C's would name a file this binary lacks.
    #[test]
    fn astac_on_an_unknown_record_answers_on_errlog_in_c_s_shape() {
        let (_db, ctx) = make_ctx();
        let buf = CaptureBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_max_level(tracing::Level::INFO)
            .with_target(false)
            .without_time()
            .finish();
        let printed = {
            let _guard = tracing::subscriber::set_default(subscriber);
            run_cmd(&ctx, "astac", &["NOSUCH:REC", "user", "host"])
        };
        assert_eq!(printed, "", "C writes this to the errlog, not stdout");

        let logged = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
        assert!(
            logged.contains("Record Not Found filename=\"")
                && logged.contains("access_commands.rs\" line number=")
                && logged.contains("  dbNameToAddr error"),
            "errlog line must carry C's shape with our own location, got: {logged:?}"
        );
    }

    /// C `dbl` (`dbTest.c:164-180`, registered with TWO args at
    /// `dbIocRegister.c:198`): `*` and `""` are the all-types sentinel,
    /// the field list is SPACE separated and printed inline by
    /// `printFieldsList`, and an unknown type prints `No record type`.
    #[test]
    fn dbl_sentinel_fields_and_unknown_type_match_c() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("AI_REC", Box::new(AiRecord::new(1.0)))
                .await
                .unwrap();
            db.add_record(
                "BO_REC",
                Box::new(crate::server::records::bo::BoRecord::new(0)),
            )
            .await
            .unwrap();
        });

        // `*` is the sentinel, not a record type to compare against.
        let starred = run_cmd(&ctx, "dbl", &["*"]);
        assert_eq!(
            starred,
            "AI_REC
BO_REC
",
            "got {starred:?}"
        );
        // So is the empty string.
        assert_eq!(
            run_cmd(&ctx, "dbl", &[""]),
            "AI_REC
BO_REC
"
        );

        // Second argument: space separated, one line per record.
        let fields = run_cmd(&ctx, "dbl", &["ai", "VAL recordType"]);
        assert_eq!(fields, "AI_REC, \"1\", \"ai\"\n", "got {fields:?}");
        // A comma-separated list is ONE field name in C, and no record
        // has it, so each record contributes a bare separator.
        let commas = run_cmd(&ctx, "dbl", &["ai", "VAL,recordType"]);
        assert_eq!(commas, "AI_REC, \n", "got {commas:?}");

        assert_eq!(run_cmd(&ctx, "dbl", &["nosuchtype"]), "No record type\n");
    }

    /// C `dbglob` (`dbTest.c:298-345`): space-separated field list,
    /// one `printFieldsList` line per match, no trailing total, and a
    /// usage line when the pattern is missing or empty.
    #[test]
    fn dbglob_line_shape_matches_c() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("SIM:T1", Box::new(AiRecord::new(1.0)))
                .await
                .unwrap();
            db.add_record("SIM:T2", Box::new(AiRecord::new(2.0)))
                .await
                .unwrap();
            db.add_record("OTHER", Box::new(AiRecord::new(3.0)))
                .await
                .unwrap();
        });

        let plain = run_cmd(&ctx, "dbglob", &["SIM:*"]);
        assert_eq!(plain, "SIM:T1\nSIM:T2\n", "got {plain:?}");

        let with_fields = run_cmd(&ctx, "dbglob", &["SIM:*", "VAL recordType"]);
        assert_eq!(
            with_fields, "SIM:T1, \"1\", \"ai\"\nSIM:T2, \"2\", \"ai\"\n",
            "got {with_fields:?}"
        );

        assert_eq!(
            run_cmd(&ctx, "dbglob", &[]),
            "Usage: dbglob \"pattern\" \"fields\"\n"
        );
        assert_eq!(
            run_cmd(&ctx, "dbgrep", &[""]),
            "Usage: dbglob \"pattern\" \"fields\"\n"
        );
    }

    /// C `printBuffer` (`dbTest.c:985-1151`) through `dbpr_msgOut`'s
    /// 10-column tab buffer: `DBF_<T>:` padded to the next tab stop,
    /// integers with a hex companion, strings quoted and escaped, and
    /// doubles at `%.12g`.
    ///
    /// The DBR code is the CALLER's, never the value's — that is the whole
    /// point of [`native_readback_lines`], so it is supplied here rather than
    /// inferred, and the type decision itself is
    /// [`dbgf_reads_the_field_declaration_not_the_stored_variant`].
    #[test]
    fn dbgf_line_shape_matches_c() {
        assert_eq!(
            native_readback_lines(DbfCode::Long, &EpicsValue::Long(42)),
            vec![format!("DBF_LONG:{}42 = 0x2a ", " ".repeat(11))]
        );
        assert_eq!(
            native_readback_lines(DbfCode::Long, &EpicsValue::Long(-1)),
            // The value crosses the 30-column stop, so the fill runs to
            // 40 exactly as C's dbpr_insert_msg does.
            vec![format!(
                "DBF_LONG:{}-1 = 0xffffffff{}",
                " ".repeat(11),
                " ".repeat(5)
            )]
        );
        assert_eq!(
            native_readback_lines(DbfCode::Double, &EpicsValue::Double(25.0)),
            vec![format!("DBF_DOUBLE:{}25{}", " ".repeat(9), " ".repeat(8))]
        );
        // %.12g, not Rust's shortest round-trip.
        assert_eq!(
            native_readback_lines(DbfCode::Double, &EpicsValue::Double(1.0 / 3.0))[0].trim_end(),
            "DBF_DOUBLE:         0.333333333333"
        );
        assert_eq!(
            native_readback_lines(DbfCode::String, &EpicsValue::String("hi\"there".into()))[0]
                .trim_end(),
            "DBF_STRING:         \"hi\\\"there\""
        );
        // C `cvtInt64ToHexString` (`cvtFast.c:483-507`) is signed, unlike the
        // `%x` the 8/16/32-bit arms use on the same negative value above.
        assert_eq!(
            native_readback_lines(DbfCode::Int64, &EpicsValue::Int64(-5))[0].trim_end(),
            "DBF_INT64:          -5 = -0x5"
        );
        assert_eq!(
            native_readback_lines(DbfCode::Int64, &EpicsValue::Int64(i64::MIN))[0].trim_end(),
            "DBF_INT64:          -9223372036854775808 = -0x8000000000000000"
        );
    }

    /// The type a readback labels and renders is the field DECLARATION's, not
    /// the variant the record happens to store.
    ///
    /// Measured against `softIoc` @`R7.0.10` on this exact database. Every arm
    /// answered the stored variant before [`field_addr_types`] owned the
    /// decision: `SELM` and `SCAN` printed their menu INDEX, `LINR` printed
    /// `DBF_SHORT: 1`, and `G:B1` printed `1` rather than the undefined `ONAM`.
    #[test]
    fn dbgf_reads_the_field_declaration_not_the_stored_variant() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decl.db");
        std::fs::write(
            &path,
            "record(sel, \"G:S1\") { field(SELM, \"Low Signal\") }\n\
             record(ai,  \"G:A1\") { field(SCAN, \"1 second\") field(LINR, \"SLOPE\") }\n\
             record(bo,  \"G:B1\") { field(VAL, \"1\") }\n\
             record(bo,  \"G:B2\") { field(ZNAM, \"Off\") field(ONAM, \"On\") field(VAL, \"1\") }\n",
        )
        .unwrap();
        run_cmd(&ctx, "dbLoadRecords", &[path.to_str().unwrap()]);
        ctx.block_on(db.ioc_init());

        for (pv, expected) in [
            ("G:S1.SELM", "DBF_STRING:         \"Low Signal\""),
            ("G:A1.SCAN", "DBF_STRING:         \"1 second\""),
            ("G:A1.LINR", "DBF_STRING:         \"SLOPE\""),
            ("G:A1.PRIO", "DBF_STRING:         \"LOW\""),
            // `DTYP` is C's only `DBF_DEVICE` field; `ai` declares device
            // support, so its menu answers.
            ("G:A1.DTYP", "DBF_STRING:         \"Soft Channel\""),
            // An undefined `ONAM` is the EMPTY choice, not the index.
            ("G:B1", "DBF_STRING:         \"\""),
            ("G:B2", "DBF_STRING:         \"On\""),
        ] {
            assert_eq!(
                run_cmd(&ctx, "dbgf", &[pv]).trim_end(),
                expected,
                "dbgf {pv}"
            );
        }

        // `dbpr` renders through `dbGetString`, which answers the menu CHOICE
        // for `DBF_MENU`/`DBF_DEVICE` and the NUMBER for `DBF_ENUM` — so the
        // same `bo` whose `dbgf` is `""` shows `VAL : 1` here.
        let report = run_cmd(&ctx, "dbpr", &["G:A1", "1"]);
        for cell in ["LINR: SLOPE", "SCAN: 1 second", "DTYP: Soft Channel"] {
            assert!(
                report.contains(cell),
                "dbpr G:A1 missing {cell:?}: {report}"
            );
        }
        assert!(
            run_cmd(&ctx, "dbpr", &["G:B1", "1"]).contains("VAL : 1"),
            "dbpr renders a DBF_ENUM VAL as its number"
        );
        assert!(
            run_cmd(&ctx, "dbpr", &["G:S1", "1"]).contains("SELM: Low Signal"),
            "dbpr renders a DBF_MENU field as its choice"
        );
    }

    /// C's `getMaxRangeValues` (`recGbl.c:372-419`) switches on the DECLARED
    /// DBF, and `DBF_MENU`/`DBF_DEVICE` have no case there — so a menu field
    /// carries no range at all.
    ///
    /// The boundary this pins is that menu-ness is NOT "the field carries its
    /// own inline choice list". `LINR` does and was already right; `SCAN` and
    /// `DTYP` take their choices from the scan table and the device registry,
    /// and both reported `65535 0` here (with `-1` where `DBR_CTRL_SHORT`
    /// truncated it) until the discriminator became `desc.declared_dbf`.
    /// `PHAS` and `UDF` are the other side: real integer DBFs that must keep
    /// their ranges. Every row measured on `softIoc` @`R7.0.10`.
    #[test]
    fn a_menu_field_has_no_range_whether_or_not_it_carries_its_own_menu() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("range.db");
        std::fs::write(
            &path,
            "record(ai, \"G:A1\") { field(SCAN, \"1 second\") field(LINR, \"SLOPE\") }\n",
        )
        .unwrap();
        run_cmd(&ctx, "dbLoadRecords", &[path.to_str().unwrap()]);
        ctx.block_on(db.ioc_init());

        // The `DBR_GR_LONG` payload row: upper, lower, then the four alarm
        // limits `recGblGetAlarmDouble` leaves at 0 for an integer request.
        let gr_long = |pv: &str| {
            let out = run_cmd(&ctx, "gft", &[pv]);
            let mut lines = out.lines().skip_while(|l| !l.starts_with("DBR_GR_LONG"));
            lines
                .next()
                .unwrap_or_else(|| panic!("no DBR_GR_LONG in gft {pv}:\n{out}"));
            let row = lines
                .next()
                .unwrap_or_else(|| panic!("DBR_GR_LONG has no payload in gft {pv}:\n{out}"));
            let nums: Vec<&str> = row.split_whitespace().collect();
            (nums[0].to_string(), nums[1].to_string())
        };

        for pv in ["G:A1.SCAN", "G:A1.DTYP", "G:A1.LINR"] {
            assert_eq!(
                gr_long(pv),
                ("0".to_string(), "0".to_string()),
                "{pv} is a DBF_MENU/DBF_DEVICE field: C's switch has no case",
            );
        }
        // recGbl.c:384-387 and :380-383 — the codes that DO have a case.
        assert_eq!(
            gr_long("G:A1.PHAS"),
            ("32767".to_string(), "-32768".to_string())
        );
        assert_eq!(gr_long("G:A1.UDF"), ("255".to_string(), "0".to_string()));
    }

    /// C `dbpf` on an ARRAY field (`dbTest.c:413-429`): the put runs in the
    /// field's own `dbr_field_type`, a `DBR_CHAR`/`DBR_UCHAR` buffer takes the
    /// raw text plus its NUL, and a refused JSON literal returns AHEAD of the
    /// closing `dbgf` so nothing is printed and nothing is stored.
    ///
    /// Every row measured on `softIoc` @`R7.0.10` against this database. The
    /// port used to send `DBR_STRING` for all of them, so `dbpf B:WL "[1,2,3]"`
    /// left `DBF_LONG[0]: (empty)`, `dbpf B:WC 65` stored the single byte `A`,
    /// and `dbpf B:WS hey` was accepted where C refuses.
    #[test]
    fn dbpf_puts_an_array_field_in_its_own_dbr_type() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arr.db");
        std::fs::write(
            &path,
            "record(waveform, \"B:WL\") { field(FTVL, \"LONG\")   field(NELM, \"4\") }\n\
             record(waveform, \"B:WC\") { field(FTVL, \"CHAR\")   field(NELM, \"8\") }\n\
             record(waveform, \"B:WD\") { field(FTVL, \"DOUBLE\") field(NELM, \"3\") }\n\
             record(waveform, \"B:WS\") { field(FTVL, \"STRING\") field(NELM, \"3\") }\n\
             record(ai,       \"B:A1\") { }\n",
        )
        .unwrap();
        run_cmd(&ctx, "dbLoadRecords", &[path.to_str().unwrap()]);
        ctx.block_on(db.ioc_init());

        for (pv, put, expected) in [
            // A bare scalar into an array field is still one element, and it
            // is labelled with the array's type, not `DBF_STRING`.
            ("B:WL", "7", "DBF_LONG:           7 = 0x7"),
            (
                "B:WL",
                "[1,2,3]",
                "DBF_LONG[3]:        1 = 0x1   2 = 0x2   3 = 0x3",
            ),
            // `NELM` is 4: the fifth element is dropped without a word.
            (
                "B:WL",
                "[1,2,3,4,5]",
                "DBF_LONG[4]:        1 = 0x1   2 = 0x2   3 = 0x3   4 = 0x4",
            ),
            ("B:WL", "", "DBF_LONG[0]: (empty)"),
            ("B:WL", "[1.9,2.1]", "DBF_LONG[2]:        1 = 0x1   2 = 0x2"),
            ("B:WD", "[1.5,2.5]", "DBF_DOUBLE[2]:      1.5       2.5"),
            ("B:WD", "3", "DBF_DOUBLE:         3"),
            // `no_elements == 1`, so this one never enters the array branch.
            ("B:A1", "1.25", "DBF_DOUBLE:         1.25"),
        ] {
            assert_eq!(
                run_cmd(&ctx, "dbpf", &[pv, put]).trim_end(),
                expected,
                "dbpf {pv} {put:?}"
            );
        }

        // `dbTest.c:425-426`: the conversion failure returns before
        // `dbPutField` AND before `dbgf`, so stdout stays empty and the LONG
        // waveform keeps the two elements the previous row left in it.
        for bad in ["hey", "null", "true", "{}", "[[1]]", "1 2 3", "[\"a\"]"] {
            assert_eq!(
                run_cmd_outcome(&ctx, "dbpf", &["B:WL", bad]).0,
                "",
                "dbpf B:WL {bad:?} must print nothing"
            );
        }
        assert_eq!(
            run_cmd(&ctx, "dbgf", &["B:WL"]).trim_end(),
            "DBF_LONG[2]:        1 = 0x1   2 = 0x2",
            "a refused dbpf leaves the field alone"
        );
        // `n = strlen(pvalue) + 1` for a CHAR buffer (`dbTest.c:415-416`) —
        // the characters plus the NUL, which is why the count is 3 and not 2.
        for (put, expected) in [
            ("65", "DBF_CHAR[3]:        \"65\""),
            ("ABC", "DBF_CHAR[4]:        \"ABC\""),
            // An empty text is still one byte, so this is `printBuffer`'s
            // SCALAR row rather than a quoted empty string.
            ("", "DBF_CHAR:           0 = 0x0"),
        ] {
            assert_eq!(
                run_cmd(&ctx, "dbpf", &["B:WC", put]).trim_end(),
                expected,
                "dbpf B:WC {put:?}"
            );
        }

        // A `DBF_STRING` array is the one that takes text — but only as JSON.
        assert_eq!(run_cmd_outcome(&ctx, "dbpf", &["B:WS", "hey"]).0, "");
        assert_eq!(
            run_cmd(&ctx, "dbpf", &["B:WS", "[\"a\",\"bb\"]"]).trim_end(),
            "DBF_STRING[2]:      \"a\"       \"bb\""
        );
    }

    /// C `printBuffer`'s two `DBR_CHAR` arms (`dbTest.c:1013-1039`), which
    /// both key on `no_elements` and neither on how the bytes are stored.
    ///
    /// The scalar row reads the byte through an `epicsInt8 *`, so 0xc8 is -56
    /// and not 200, and masks the hex back to one byte. The array row is a
    /// single quoted string cut at `epicsStrnLen(pbuffer, no_elements)`, so a
    /// buffer whose FIRST byte is NUL leaves `len == 0` and C's
    /// `while (len > 0)` prints nothing under the header.
    ///
    /// The `"ab"` and `0 = 0x0` rows are measured (`dbpf` on a `CHAR`
    /// waveform); the rest are transcribed from the two arms.
    #[test]
    fn printbuffer_char_keys_on_no_elements_and_stops_at_the_nul() {
        let render = |bytes: Vec<u8>| {
            native_readback_lines(DbfCode::Char, &EpicsValue::CharArray(bytes))[0]
                .trim_end()
                .to_string()
        };
        // `dbpf C:W5 ab` stores 'a','b',0 and C reads back `DBF_CHAR[3]`.
        assert_eq!(render(vec![b'a', b'b', 0]), "DBF_CHAR[3]:        \"ab\"");
        // No NUL at all: the whole buffer is the text.
        assert_eq!(render(vec![b'a', b'b']), "DBF_CHAR[2]:        \"ab\"");
        // Trailing bytes past the NUL are not text either.
        assert_eq!(
            render(vec![b'a', 0, b'z', b'z']),
            "DBF_CHAR[4]:        \"a\""
        );
        // `len == 0`: header only, and NOT a quoted empty string.
        assert_eq!(render(vec![0, 0, 0]), "DBF_CHAR[3]:");
        // `no_elements == 1` is the numeric row whatever the variant.
        assert_eq!(render(vec![0]), "DBF_CHAR:           0 = 0x0");
        assert_eq!(render(vec![b'A']), "DBF_CHAR:           65 = 0x41 = \'A\'");
        assert_eq!(render(vec![b' ']), "DBF_CHAR:           32 = 0x20 = \' \'");
        // `epicsInt8`, so the high half of the byte range is negative.
        assert_eq!(render(vec![0xc8]), "DBF_CHAR:           -56 = 0xc8");
        assert_eq!(
            native_readback_lines(DbfCode::Char, &EpicsValue::Char(0xc8))[0].trim_end(),
            "DBF_CHAR:           -56 = 0xc8"
        );
    }

    /// C `realToString` (`dbStaticLib.c:233-320`), the `dbpr` float renderer.
    ///
    /// Each row was read back off `softIoc` @`R7.0.10` through `dbpr` on an
    /// `ao`'s `DBF_DOUBLE` fields. `%g` and Rust's `Display` both disagree with
    /// C on the exponential rows, and `Display` alone on `NaN`.
    #[test]
    fn real_to_string_matches_dbgetstring() {
        for (value, expected) in [
            (0.0, "0"),
            (1.5, "1.5"),
            (0.1, "0.1"),
            // Within one `delta` of an `epicsInt32`, so C prints the integer
            // and never reaches the exponential arm its magnitude suggests.
            (1e7, "10000000"),
            (1e6, "1000000"),
            (123456789.0, "123456789"),
            (-1e7, "-10000000"),
            (1e5, "100000"),
            // `logval < -2` is the cut, and it is `(int)log10` — so `1e-3`
            // goes exponential while the barely larger `0.001234`, whose
            // `log10` truncates to -2, stays fixed.
            (1e-3, "1.0e-03"),
            (0.001234, "0.001234"),
            (0.01, "0.01"),
            (1e-6, "1.0e-06"),
            (1e-7, "1.0e-07"),
            (1e-15, "1.0e-15"),
            (1e-20, "1.0e-20"),
            (1e15, "1.0e+15"),
            (1e20, "1.0e+20"),
            (6.02e23, "6.02e+23"),
            (0.000999999, "9.99999e-04"),
            (1.2345678901234567e19, "1.23456789012346e+19"),
            (f64::MAX, "1.79769313486232e+308"),
            (0.5, "0.5"),
            (0.6, "0.6"),
            (2.5, "2.5"),
            (7.5, "7.5"),
            (1.05, "1.05"),
            (2.675, "2.675"),
            (1.0 / 3.0, "0.33333333333333"),
            (999999.9, "999999.9"),
            (100000.5, "100000.5"),
            // The trim doubles as the rounding, and it carries.
            (99999.99999999999, "100000"),
            (9.999999999, "9.999999999"),
            (3.14159265358979, "3.14159265358979"),
            (123456.789, "123456.789"),
            (-1.5, "-1.5"),
            (-0.1, "-0.1"),
            (f64::NAN, "nan"),
        ] {
            assert_eq!(real_to_string(value, true), expected, "{value}");
        }
        // `isdouble = 0` is the `DBF_FLOAT` arm: six digits, not fourteen.
        assert_eq!(real_to_string(1.0 / 3.0, false), "0.333333");
    }

    /// A missing or empty name is C's usage line (`dbTest.c:358-361`).
    #[test]
    fn dbgf_missing_name_prints_usage() {
        let (_db, ctx) = make_ctx();
        assert_eq!(run_cmd(&ctx, "dbgf", &[]), "Usage: dbgf \"pv name\"\n");
    }

    /// `epicsStrnGlobMatch` (`epicsString.c:282-312`) consumes exactly
    /// one character per `?`. `dbglob`'s help text claims "0 or one",
    /// but the trailing skip loop only skips `*`, so a leftover `?`
    /// fails the match.
    ///
    /// The help text is wrong about the code, and the code is what a
    /// user observes. Measured 2026-08-26 against a live C IOC
    /// (`bin/linux-x86_64/softIoc`, banner `R7.0.10-146-g8f5015b663`)
    /// holding `record(ai,"REC")`, `record(ai,"RECX")` and
    /// `record(ai,"RECXY")`:
    ///
    /// ```text
    /// epics> dbglob("REC?")      epics> dbglob("REC*")
    /// RECX                       REC
    ///                            RECX
    ///                            RECXY
    /// ```
    ///
    /// `REC` is absent from the `REC?` listing, so a `?` matching zero
    /// characters would diverge from a running C IOC. The `REC`/`RECX`/
    /// `RECXY` cases below reproduce that transcript through the same
    /// `glob_match` call `dbglob_handler` filters record names with.
    #[test]
    fn glob_match_question_mark_consumes_exactly_one() {
        assert!(glob_match("ab?", "abc"));
        assert!(!glob_match("ab?", "ab"));
        assert!(!glob_match("ab?", "abcd"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*d", "abcd"));
        assert!(!glob_match("a*d", "abce"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));

        // The live-IOC transcript above, name by name.
        assert!(!glob_match("REC?", "REC"));
        assert!(glob_match("REC?", "RECX"));
        assert!(!glob_match("REC?", "RECXY"));
        assert!(glob_match("REC*", "REC"));
        assert!(glob_match("REC*", "RECX"));
        assert!(glob_match("REC*", "RECXY"));
    }

    /// Every command the mechanical registration sweep found declaring
    /// fewer arguments than its C `iocshFuncDef`. A narrower
    /// declaration silently drops the trailing token, and for
    /// `dbCreateRecord` it shifts every argument by one.
    #[test]
    fn registrations_declare_the_c_argument_count() {
        let mut reg = CommandRegistry::new();
        register_builtins(&mut reg);
        for (name, nargs) in [
            ("dbLoadTemplate", 3), // dbtoolsIocRegister.c:19-24
            ("dbCreateRecord", 3), // dbStaticIocRegister.c:267-274 @f4ccf7bc8
            ("scanppl", 1),        // dbIocRegister.c scanpplArg0
            ("dbl", 2),            // dbIocRegister.c:198
        ] {
            assert_eq!(
                reg.get(name).unwrap().args.len(),
                nargs,
                "{name} must declare C's argument count"
            );
        }
    }

    /// `cvtArg` for `iocshArgPdbbase` (`iocsh.cpp:872-884`) accepts the
    /// argument missing, starting with `0`, or spelled `pdbbase`, and
    /// refuses anything else — which is what stops a C script's
    /// `dbCreateRecord("pdbbase","ai","X")` from being read as
    /// type `pdbbase`, name `ai`.
    #[test]
    fn db_create_record_takes_the_pdbbase_argument() {
        let (db, ctx) = make_ctx();
        let mut reg = CommandRegistry::new();
        register_builtins(&mut reg);
        let cmd = reg.get("dbCreateRecord").unwrap();

        let tokens = ["pdbbase", "ai", "SIM:NEW"].map(str::to_string);
        let args = parse_args(&tokens, &cmd.args).unwrap();
        cmd.handler
            .call(&args, &ctx)
            .expect("must create the record");
        assert!(db.get_record("SIM:NEW").is_some());

        let bad = ["ai", "SIM:OTHER", "x"].map(str::to_string);
        let args = parse_args(&bad, &cmd.args).unwrap();
        let Err(msg) = cmd.handler.call(&args, &ctx) else {
            panic!("a non-pdbbase first argument must be refused");
        };
        assert_eq!(msg, "Expecting 'pdbbase' got 'ai'.");
    }

    /// C `dbCreateRecordCallFunc` (`dbStaticIocRegister.c:294-297` at
    /// `f4ccf7bc8`) asks for the record NAME before the record type, and an
    /// empty name is `S_dbLib_recordNameMissing` just like an absent one.
    /// Asking about the type first made a bare `dbCreateRecord
    /// pdbbase` name the wrong argument.
    #[test]
    fn db_create_record_asks_for_the_name_before_the_type() {
        let (db, ctx) = make_ctx();
        let mut reg = CommandRegistry::new();
        register_builtins(&mut reg);
        let cmd = reg.get("dbCreateRecord").unwrap();
        let call = |tokens: &[&str]| {
            let tokens: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
            let args = parse_args(&tokens, &cmd.args).expect("C accepts a missing argument");
            match cmd.handler.call(&args, &ctx) {
                Err(msg) => msg,
                Ok(_) => panic!("{tokens:?} must fail"),
            }
        };
        // `errSymMsg(S_dbLib_recordNameMissing)` behind its status, as
        // `dbStaticIocRegister.c:307` at `f4ccf7bc8` prints it.
        assert_eq!(call(&["pdbbase"]), "33554465 Record name is required");
        assert_eq!(call(&["pdbbase", "ai"]), "33554465 Record name is required");
        assert_eq!(
            call(&["pdbbase", "ai", ""]),
            "33554465 Record name is required"
        );
        assert_eq!(call(&[]), "33554465 Record name is required");
        assert!(
            db.get_record("").is_none(),
            "no record may be created for an empty name"
        );
    }

    /// `dbLoadTemplate`'s third argument replaces
    /// `EPICS_DB_INCLUDE_PATH` when looking for the `.substitutions`
    /// file itself (`dbLoadTemplate.y:362-368`).
    #[test]
    #[serial_test::serial(epics_env)]
    fn db_load_template_third_argument_finds_the_subs_file() {
        let (db, ctx) = make_ctx();
        let subs_dir = tempfile::tempdir().unwrap();
        let tpl_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            tpl_dir.path().join("t.template"),
            "record(ai,\"SIM:FROM_ARG\") { }\n",
        )
        .unwrap();
        std::fs::write(
            subs_dir.path().join("t.substitutions"),
            "file \"t.template\" { { } }\n",
        )
        .unwrap();

        // The templates still come from EPICS_DB_INCLUDE_PATH: C's
        // dbLoadRecords resets the path list from the environment.
        let _guard = DbIncludePath::set(tpl_dir.path());
        let mut reg = CommandRegistry::new();
        register_builtins(&mut reg);
        let cmd = reg.get("dbLoadTemplate").unwrap();
        let tokens = [
            "t.substitutions".to_string(),
            String::new(),
            subs_dir.path().display().to_string(),
        ];
        let args = parse_args(&tokens, &cmd.args).unwrap();
        cmd.handler
            .call(&args, &ctx)
            .expect("the third argument must locate the .substitutions file");
        assert!(db.get_record("SIM:FROM_ARG").is_some());
    }

    #[test]
    fn test_dbl() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("REC_A", Box::new(AiRecord::new(1.0)))
                .await
                .unwrap();
            db.add_record("REC_B", Box::new(AiRecord::new(2.0)))
                .await
                .unwrap();
        });

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbl").unwrap();
        let args = parse_args(&[], &cmd.args).unwrap();
        let result = cmd.handler.call(&args, &ctx);
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    #[test]
    fn test_dbgf() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("TEMP", Box::new(AiRecord::new(25.0)))
                .await
                .unwrap();
            // C `dbgf` refuses before `iocInit` (`dbTest.c:366-368`).
            db.ioc_init().await;
        });

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbgf").unwrap();
        let tokens = vec!["TEMP".to_string()];
        let args = parse_args(&tokens, &cmd.args).unwrap();
        let result = cmd.handler.call(&args, &ctx);
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    /// C `dbl` walks `dbFirstRecordType` -> `dbFirstRecord` ...
    /// `dbNextRecordType` (`dbTest.c:174-193`), so its output is
    /// grouped by record type and, inside a group, in the order the
    /// records were loaded. One global sort over every name gave
    /// neither: it interleaved the types and re-ordered each type's
    /// records alphabetically.
    #[test]
    fn dbl_lists_records_type_major_in_load_order() {
        use crate::server::records::bo::BoRecord;

        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("Z:A1", Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
            db.add_record("A:B1", Box::new(BoRecord::new(0)))
                .await
                .unwrap();
            db.add_record("M:A2", Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
        });

        // `ai` precedes `bo`, and inside `ai` the load order Z:A1,
        // M:A2 survives — the alphabetical order would be the reverse.
        assert_eq!(run_cmd(&ctx, "dbl", &[]), "Z:A1\nM:A2\nA:B1\n");
        assert_eq!(run_cmd(&ctx, "dbl", &["ai"]), "Z:A1\nM:A2\n");
    }

    /// C `nameToAddr` (`dbTest.c:787-795`) prints `PV '<name>' not
    /// found` on STDOUT for `dbgf`, `dbpf` and `dbpr` alike; the
    /// command then returns -1 and prints nothing else. Reporting it
    /// through the shell's error channel instead put the port's text
    /// on stderr, so a script that captured stdout saw nothing at all.
    #[test]
    fn an_unknown_pv_is_reported_on_stdout_by_every_db_command() {
        let (_db, ctx) = make_ctx();
        assert_eq!(
            run_cmd(&ctx, "dbgf", &["NONEXISTENT"]),
            "PV 'NONEXISTENT' not found\n"
        );
        assert_eq!(
            run_cmd(&ctx, "dbpf", &["NONEXISTENT", "1"]),
            "PV 'NONEXISTENT' not found\n"
        );
        assert_eq!(
            run_cmd(&ctx, "dbpr", &["NONEXISTENT"]),
            "PV 'NONEXISTENT' not found\n"
        );
    }

    /// C `dbpf` ends with `dbgf(pname)` (`dbTest.c:433`), so the
    /// read-back is byte-identical to what `dbgf` prints — including
    /// the tab-buffer padding and the `= 0x%x` suffix that the
    /// hand-rolled `"{type}: {val}"` read-back never had.
    #[test]
    fn dbpf_reads_back_through_the_dbgf_printer() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("TEMP", Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
            // C `dbpf` refuses before `iocInit` (`dbTest.c:408-410`).
            db.ioc_init().await;
        });

        let put = run_cmd(&ctx, "dbpf", &["TEMP", "42"]);
        assert_eq!(put, run_cmd(&ctx, "dbgf", &["TEMP"]));
        assert_eq!(put.trim_end(), "DBF_DOUBLE:         42");

        // The read-back of an integer-class field carries C's hex
        // suffix, which is the divergence the second renderer had.
        let prec = run_cmd(&ctx, "dbpf", &["TEMP.PREC", "3"]);
        assert_eq!(prec, run_cmd(&ctx, "dbgf", &["TEMP.PREC"]));
        assert_eq!(prec.trim_end(), "DBF_SHORT:          3 = 0x3");

        // C runs `dbgf` after a failed `dbPutField` too, so a refused
        // put still reports the value the record kept — and the failure
        // itself is silent, because `iocshSetError` prints nothing
        // (`iocsh.cpp:1004-1018`).
        let (refused, failed) = run_cmd_outcome(&ctx, "dbpf", &["TEMP.SEVR", "0"]);
        assert!(failed, "a refused put fails the line");
        assert_eq!(refused, run_cmd(&ctx, "dbgf", &["TEMP.SEVR"]));
    }

    /// Every port command whose C original diagnoses a missing string
    /// argument itself: C's iocsh hands the body a NULL and the body
    /// prints one line and returns nonzero, so declaring the argument
    /// required made the port answer with the registry's "missing
    /// required argument" on stderr and never run the body at all.
    /// The C texts are `dbTest.c:308`, `:359`, `:401`, `:445`,
    /// `dbAccess.c:801`, `dbLoadTemplate.y:345` (stderr) and
    /// `asDbLib.c:244`.
    #[test]
    fn a_missing_argument_reaches_c_s_own_diagnostic() {
        let (_db, ctx) = make_ctx();
        assert_eq!(
            run_cmd(&ctx, "dbpr", &[]),
            "Usage: dbpr \"pv name\", level\n"
        );
        assert_eq!(
            run_cmd(&ctx, "dbpr", &[""]),
            "Usage: dbpr \"pv name\", level\n"
        );
        assert_eq!(
            run_cmd(&ctx, "dbLoadRecords", &[]),
            "Usage: dbLoadRecords \"file\", \"subs\"\n"
        );
        // ...and the line FAILED: `dbLoadRecordsCallFunc` hands
        // `dbLoadRecords`'s -1 to `iocshSetError` (`dbIocRegister.c:71-74`),
        // so `on error break` stops the boot here.
        assert!(matches!(
            run_capturing(&ctx, "dbLoadRecords", &[]).2,
            Ok(CommandOutcome::Failed)
        ));
        assert_eq!(
            run_cmd(&ctx, "astac", &[]),
            "Usage: astac \"record name\", \"user\", \"host\"\n"
        );
        assert_eq!(
            run_cmd(&ctx, "astac", &["REC", "user"]),
            "Usage: astac \"record name\", \"user\", \"host\"\n"
        );
        // C prints this one on stderr, so stdout stays empty and the
        // shell must not have refused the call before the body ran.
        assert_eq!(run_cmd(&ctx, "dbLoadTemplate", &[]), "");
        assert_eq!(run_cmd(&ctx, "dbLoadTemplate", &[""]), "");
        for args in [&[][..], &[""][..]] {
            let (_out, err, result) = run_capturing(&ctx, "dbLoadTemplate", args);
            assert_eq!(err, "must specify variable substitution file\n");
            assert!(
                matches!(result, Ok(CommandOutcome::Failed)),
                "dbLoadTemplate.y:344-347 returns -1 and                  dbtoolsIocRegister.c:33-36 sets it as the shell error"
            );
        }
        // Already C-shaped; pinned here so the family stays closed.
        assert_eq!(
            run_cmd(&ctx, "dbglob", &[]),
            "Usage: dbglob \"pattern\" \"fields\"\n"
        );
        assert_eq!(run_cmd(&ctx, "dbgf", &[]), "Usage: dbgf \"pv name\"\n");
    }

    /// C `dbTest.c:400-403`: `dbpf` with no arguments is a usage line
    /// on stdout, not an argument-parse failure.
    #[test]
    fn dbpf_missing_arguments_print_usage() {
        let (_db, ctx) = make_ctx();
        assert_eq!(
            run_cmd(&ctx, "dbpf", &[]),
            "Usage: dbpf \"pv name\", \"value\"\n"
        );
        assert_eq!(
            run_cmd(&ctx, "dbpf", &["TEMP"]),
            "Usage: dbpf \"pv name\", \"value\"\n"
        );
    }

    /// End-to-end through the real `dbLoadRecords` command: a `DTYP=` macro is
    /// pure text substitution (C `dbLexRoutines.c` runs macros through macLib
    /// during lexing), so it reaches only the record that wrote
    /// `field(DTYP,"$(DTYP)")` and must leave a literal DTYP alone.
    ///
    /// The db below is the shape of the vendored `scaler-rs/db/scaler.db`: two
    /// `bo` helper records with a literal `Soft Channel` DTYP plus the counting
    /// record referencing `$(DTYP)` (an `ai` here, since the built-in loader has no
    /// `scaler` type). The old force-override rewrote all three,
    /// leaving the soft helpers bound to a hardware DTYP.
    #[test]
    fn db_load_records_dtyp_macro_leaves_literal_dtyp_alone() {
        use std::io::Write;

        let (db, ctx) = make_ctx();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scaler_shaped.db");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
record(bo, "$(P)_calcEnable") {{
    field(DTYP, "Soft Channel")
    field(ZNAM, "ENABLE")
}}
record(ai, "$(P)") {{
    field(DTYP, "$(DTYP)")
}}
record(bo, "$(P)_calc_ctrl") {{
    field(DTYP, "Soft Channel")
    field(ONAM, "Cts/sec")
}}
"#
        )
        .unwrap();
        drop(f);

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbLoadRecords").unwrap();
        let tokens = vec![
            path.to_str().unwrap().to_string(),
            "P=SCALER1,DTYP=Scaler-rs".to_string(),
        ];
        let args = parse_args(&tokens, &cmd.args).unwrap();
        cmd.handler.call(&args, &ctx).unwrap();

        let dtyp_of = |name: &str| -> String {
            ctx.block_on(async {
                let rec = db.get_record(name).expect("record loaded");
                let inst = rec.read();
                inst.common.dtyp.clone()
            })
        };

        // Literal DTYP on the soft helpers: untouched. Force-override used to
        // corrupt both into "Scaler-rs".
        assert_eq!(dtyp_of("SCALER1_calcEnable"), "Soft Channel");
        assert_eq!(dtyp_of("SCALER1_calc_ctrl"), "Soft Channel");
        // The `$(DTYP)` reference: substituted.
        assert_eq!(dtyp_of("SCALER1"), "Scaler-rs");
    }

    #[test]
    fn test_dbpf_and_readback() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("TEMP", Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
            // C `dbpf` refuses before `iocInit` (`dbTest.c:408-410`).
            db.ioc_init().await;
        });

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);

        // Put a value
        let cmd = registry.get("dbpf").unwrap();
        let tokens = vec!["TEMP".to_string(), "42.0".to_string()];
        let args = parse_args(&tokens, &cmd.args).unwrap();
        let result = cmd.handler.call(&args, &ctx);
        assert!(matches!(result, Ok(CommandOutcome::Continue)));

        // Read back
        let val = db.get_pv("TEMP").unwrap();
        match val {
            EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
            other => panic!("expected Double(42.0), got {:?}", other),
        }
    }

    /// Regression: `dbpf <rec>.DTYP <device-support-name>` must succeed for a
    /// device-support name valid for the record type. DTYP is `DBF_DEVICE`,
    /// served as `DBR_ENUM`, but its choices are the record type's live device
    /// menu, NOT the static common-menu table `EpicsValue::parse(Enum,_)`
    /// used to consult. `"Async Soft Channel"` is a declared
    /// `ai` device support but is absent from that static table, so before the
    /// fix `cmd_dbpf` parsed it as an Enum and the handler returned
    /// `Err("invalid enum or menu string: ...")` — which, when the `dbpf` sat in
    /// an st.cmd, made `iocInit` fail before the CA server bound. The fix routes
    /// the value to the put path, which validates it against the record's live
    /// device menu (`device_choices()` = declared + runtime-contributed + the
    /// current DTYP).
    #[test]
    fn test_dbpf_dtyp_accepts_device_support_name() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("DEV", Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
            // C `dbpf` refuses before `iocInit` (`dbTest.c:408-410`).
            db.ioc_init().await;
        });

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);

        let cmd = registry.get("dbpf").unwrap();
        let tokens = vec!["DEV.DTYP".to_string(), "Async Soft Channel".to_string()];
        let args = parse_args(&tokens, &cmd.args).unwrap();
        let result = cmd.handler.call(&args, &ctx);
        if let Err(e) = &result {
            panic!("dbpf DEV.DTYP 'Async Soft Channel' should succeed, got Err({e})");
        }
        assert!(matches!(result, Ok(CommandOutcome::Continue)));

        // The device-support NAME landed on the record's DTYP.
        let dtyp = ctx.block_on(async {
            let rec = db.get_record("DEV").expect("record present");
            let inst = rec.read();
            inst.common.dtyp.clone()
        });
        assert_eq!(dtyp, "Async Soft Channel");
    }

    /// 03 L-7 — a record-specific menu label put through `dbpf` resolves
    /// against THAT field's menu, not a cross-menu global table.
    ///
    /// `sel.SELM`'s menu is `selSELM`, whose "Specified" is index 0.
    /// `menuFanout`'s "Specified" is index 1. `dbpf` used to pre-parse the
    /// token with the field-blind `EpicsValue::parse`, whose one global table
    /// carried menuFanout's 1, so this put stored the wrong choice — a `sel`
    /// record left selecting by the wrong rule with no error anywhere.
    #[test]
    fn dbpf_menu_label_resolves_against_the_fields_own_menu() {
        use crate::server::records::sel::SelRecord;

        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("SL", Box::new(SelRecord::default()))
                .await
                .unwrap();
            db.ioc_init().await;
        });

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbpf").unwrap();

        let put = |label: &str| {
            let tokens = vec!["SL.SELM".to_string(), label.to_string()];
            let args = parse_args(&tokens, &cmd.args).unwrap();
            cmd.handler.call(&args, &ctx)
        };

        assert!(matches!(put("Specified"), Ok(CommandOutcome::Continue)));
        let selm = ctx.block_on(async {
            let rec = db.get_record("SL").expect("record present");
            let inst = rec.read();
            inst.record.get_field("SELM")
        });
        assert_eq!(
            selm,
            Some(EpicsValue::Enum(0)),
            "selSELM's \"Specified\" is 0; menuFanout's 1 is the cross-menu guess"
        );

        // A later choice proves the whole menu, not just index 0.
        assert!(matches!(put("High Signal"), Ok(CommandOutcome::Continue)));
        let selm = ctx.block_on(async {
            let rec = db.get_record("SL").expect("record present");
            let inst = rec.read();
            inst.record.get_field("SELM")
        });
        assert_eq!(selm, Some(EpicsValue::Enum(1)));
    }

    /// Regression for the put/read DTYP asymmetry: a device-support name that is
    /// NOT in the record type's static `device()` menu but IS registered at
    /// runtime (`register_device_menu`, the asyn / scaler-rs path) must be
    /// put-able via `dbpf`, because the read path already advertises it. Before
    /// the fix the CA-put validation (`coerce_put_value`) and the DTYP write-back
    /// (`put_common_field`) both consulted the static-only device menu, so a
    /// contributed name either failed `S_db_noRSET` (types with no static menu)
    /// or resolved to `NoChange` (leaving DTYP unset) — exactly what blocked
    /// scaler974-ioc's `dbpf scaler1.DTYP "Asyn Scaler"`.
    #[test]
    fn test_dbpf_dtyp_accepts_contributed_device_support_name() {
        use crate::server::record::register_device_menu;
        // A record-type name + DTYP name no other test touches (the registry is
        // process-global and append-only).
        register_device_menu("ai", &["Dbpf Contributed Probe"]);

        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("CONTRIB", Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
            // C `dbpf` refuses before `iocInit` (`dbTest.c:408-410`).
            db.ioc_init().await;
        });

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);

        let cmd = registry.get("dbpf").unwrap();
        let tokens = vec![
            "CONTRIB.DTYP".to_string(),
            "Dbpf Contributed Probe".to_string(),
        ];
        let args = parse_args(&tokens, &cmd.args).unwrap();
        let result = cmd.handler.call(&args, &ctx);
        if let Err(e) = &result {
            panic!("dbpf CONTRIB.DTYP 'Dbpf Contributed Probe' should succeed, got Err({e})");
        }
        assert!(matches!(result, Ok(CommandOutcome::Continue)));

        // The contributed NAME is what landed on DTYP — not NoChange, not a
        // stale value.
        let dtyp = ctx.block_on(async {
            let rec = db.get_record("CONTRIB").expect("record present");
            let inst = rec.read();
            inst.common.dtyp.clone()
        });
        assert_eq!(dtyp, "Dbpf Contributed Probe");
    }

    #[test]
    fn test_dbpr_levels() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("TEMP", Box::new(AiRecord::new(25.0)))
                .await
                .unwrap();
        });

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);

        for level in [0, 1, 2] {
            let cmd = registry.get("dbpr").unwrap();
            let tokens = vec!["TEMP".to_string(), level.to_string()];
            let args = parse_args(&tokens, &cmd.args).unwrap();
            let result = cmd.handler.call(&args, &ctx);
            assert!(matches!(result, Ok(CommandOutcome::Continue)));
        }
    }

    /// The field names in a `dbpr` block. Every message is padded to
    /// the next 20-column stop (`dbTest.c:1345-1355`), so a name cell
    /// always begins at a multiple of 20 and holds `NAME: ` in its
    /// first six bytes.
    fn dbpr_field_names(printed: &str) -> Vec<String> {
        printed
            .lines()
            .flat_map(|line| line.as_bytes().chunks(20))
            .filter_map(|cell| {
                let cell = String::from_utf8_lossy(cell);
                let (name, _) = cell.split_once(':')?;
                let name = name.trim_end();
                (name.len() <= 4
                    && !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()))
                .then(|| name.to_string())
            })
            .collect()
    }

    /// Which fields `dbpr` prints is `interest <= level`, over the
    /// record type's whole table, in field-name order — C
    /// `dbpr_report` (`dbTest.c:1179-1182`) walking `sortFldInd`
    /// (`dbLexRoutines.c:781-798`). Asserted per interest boundary,
    /// not per narrative level.
    #[test]
    fn dbpr_prints_every_field_whose_interest_the_level_covers() {
        use crate::server::record::dbd_generated;

        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("TEMP", Box::new(AiRecord::new(25.0)))
                .await
                .unwrap();
        });

        let mut declared: Vec<(&str, u8)> = dbd_generated::DB_COMMON_FIELDS
            .iter()
            .chain(dbd_generated::record_fields("ai").unwrap())
            .map(|d| (d.name, d.interest))
            .collect();
        declared.sort();

        for level in 0..=4u8 {
            let printed = run_cmd(&ctx, "dbpr", &["TEMP", &level.to_string()]);
            let seen = dbpr_field_names(&printed);
            let seen: Vec<&str> = seen.iter().map(String::as_str).collect();
            for (name, interest) in &declared {
                // A field the port declares but cannot resolve a value
                // for prints nothing; the boundary being pinned is that
                // no field ABOVE the level is ever printed, and that a
                // field at or below it is not filtered out.
                if *interest > level {
                    assert!(
                        !seen.contains(name),
                        "level {level} printed {name} (interest {interest})"
                    );
                }
            }
            // The four dbCommon fields every level must reach.
            for name in ["NAME", "DESC", "ASG", "STAT"] {
                assert!(
                    seen.contains(&name),
                    "level {level} lost {name}: {printed:?}"
                );
            }
            if level >= 1 {
                assert!(seen.contains(&"SCAN"), "level {level}: {printed:?}");
            }
        }

        // `SCAN` is DBF_MENU, so C prints the choice string, not the
        // index (`dbStaticLib.c:2131-2147`).
        assert!(
            run_cmd(&ctx, "dbpr", &["TEMP", "1"]).contains("SCAN: Passive"),
            "{:?}",
            run_cmd(&ctx, "dbpr", &["TEMP", "1"])
        );
    }

    /// One `dbpr` cell's payload: everything after `NAME: ` up to the tab
    /// padding that starts the next cell.
    fn dbpr_field(printed: &str, field: &str) -> String {
        let key = format!("{field:<4}: ");
        let idx = printed
            .find(&key)
            .unwrap_or_else(|| panic!("{field} absent from {printed:?}"));
        let rest = &printed[idx + key.len()..];
        let end = rest
            .find("  ")
            .unwrap_or(rest.len())
            .min(rest.find('\n').unwrap_or(rest.len()));
        rest[..end].to_string()
    }

    /// `dbpr`'s `DBF_NOACCESS` rows, against `softIoc` R7.0.10 on the same
    /// record: `BKPT: 00` and `TIME: <undefined>`.
    ///
    /// Both come from the declaration, not from a converted value — C's switch
    /// takes the `DBF_NOACCESS` arm before any arm that calls `dbGetString`
    /// (`dbTest.c:1225`) — so a walk that asked for a value first would print
    /// nothing at all here, which is what the port did while the descriptors
    /// were dropped.
    #[test]
    fn dbpr_renders_the_no_access_rows_from_the_declaration() {
        let (db, ctx) = make_ctx();
        load_records(&ctx, r#"record(ai, "N:AI") { field(VAL, "7.0") }"#)
            .expect("ai .db must load");
        ctx.block_on(db.ioc_init());

        let printed = capture(&ctx, || {
            dbpr_report(&ctx, "N:AI", 4);
        });
        // `size` bytes as `%02x ` (`dbTest.c:1249-1262`); BKPT is one byte.
        assert_eq!(dbpr_field(&printed, "BKPT"), "00", "dbpr N:AI 4 BKPT");
        // C's named case, `epicsTimeToStrftime` on an unset stamp
        // (`dbTest.c:1228-1231`).
        assert_eq!(
            dbpr_field(&printed, "TIME"),
            "<undefined>",
            "dbpr N:AI 4 TIME"
        );
    }

    /// `base(HEX)` reaches `dbpr` and nothing else.
    ///
    /// The whole `base(HEX)` population in EPICS base is `mbbi`/`mbbo`'s
    /// sixteen `*VL` fields; the rest of the record — `MASK`, `NOBT`, `SHFT`,
    /// `RVAL` — stays decimal. Every expectation below is
    /// `bin/linux-x86_64/softIoc` (R7.0.10-146) answering `dbpr H:I 2` and
    /// `dbgf` on the same record, which is also what pins the split: `dbgf`
    /// goes through `dbConvert` and prints `DBF_ULONG:          10 = 0xa`
    /// whatever the base says.
    #[test]
    fn a_base_hex_field_renders_hex_in_dbpr_only() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            r#"record(mbbi, "H:I") { field(ZRVL, "10") field(ONVL, "255") field(NOBT, "4") field(SHFT, "2") field(MASK, "15") }"#,
        )
        .expect("mbbi .db must load");
        ctx.block_on(db.ioc_init());

        let printed = capture(&ctx, || {
            dbpr_report(&ctx, "H:I", 2);
        });
        for (field, expected) in [
            ("ZRVL", "0xa"),
            ("ONVL", "0xff"),
            ("TWVL", "0x0"),
            ("FFVL", "0x0"),
            // Not `base(HEX)`: same record, same `dbpr`, decimal.
            ("MASK", "15"),
            ("NOBT", "4"),
            ("SHFT", "2"),
            ("RVAL", "0"),
        ] {
            assert_eq!(dbpr_field(&printed, field), expected, "dbpr H:I 2 {field}");
        }

        // `dbgf` is `dbConvert`, which has no base — C prints the decimal
        // value with its own `= 0x` suffix here whatever `dbpr` showed.
        assert_eq!(
            run_cmd(&ctx, "dbgf", &["H:I.ZRVL"]).trim_end(),
            "DBF_ULONG:          10 = 0xa"
        );
    }

    /// C's hex converters at their boundaries, not at a narrative.
    ///
    /// The 32-bit arms go through `ulongToHexString(epicsUInt32, ...)`
    /// (`dbStaticLib.c:208-231`), which takes an UNSIGNED argument, so a
    /// negative `DBF_LONG` arrives sign-extended and prints with no minus at
    /// all; the 64-bit arms are `cvtInt64ToHexString` (`cvtFast.c:483-507`),
    /// which writes the minus BEFORE the `0x` and special-cases `INT64_MIN`.
    #[test]
    fn the_hex_renderer_matches_c_s_two_converters() {
        for (value, expected) in [
            (EpicsValue::ULong(0), "0x0"),
            (EpicsValue::ULong(10), "0xa"),
            (EpicsValue::ULong(u32::MAX), "0xffffffff"),
            (EpicsValue::Long(-1), "0xffffffff"),
            (EpicsValue::Short(-1), "0xffffffff"),
            (EpicsValue::Char(0xff), "0xffffffff"),
            (EpicsValue::UChar(255), "0xff"),
            (EpicsValue::UShort(65535), "0xffff"),
            (EpicsValue::Int64(0), "0x0"),
            (EpicsValue::Int64(-42), "-0x2a"),
            (EpicsValue::Int64(i64::MIN), "-0x8000000000000000"),
            (EpicsValue::UInt64(u64::MAX), "0xffffffffffffffff"),
        ] {
            assert_eq!(
                super::hex_string(&value).as_deref(),
                Some(expected),
                "{value:?}"
            );
        }
        // Nothing C's switch has a hex arm for.
        assert_eq!(super::hex_string(&EpicsValue::Double(1.5)), None);
        assert_eq!(super::hex_string(&EpicsValue::String("x".into())), None);
    }

    const DBPR_LINK_DB: &str = r#"
record(ai, "L:B")       { field(VAL, "1") }
record(ai, "P:CON")     { field(INP, "5") }
record(ai, "P:ARR")     { field(INP, "[1,2,3]") }
record(ai, "P:LOCAL")   { field(INP, "L:B.VAL") }
record(ai, "P:LOCALCP") { field(INP, "L:B.VAL CPP MS") }
record(ai, "P:EXT")     { field(INP, "OTHER:PV") }
record(ai, "P:CA")      { field(INP, "OTHER:PV CA") }
record(ai, "P:JSON")    { field(INP, {"const":[1,2,3]}) }
record(ai, "P:UNSET")   { }
record(ai, "P:FL")      { field(FLNK, "L:B") }
"#;

    /// A link field prints C's `plink->type` in front of its text
    /// (`dbTest.c:1205-1224`) — one case per branch of `dbInitLink`
    /// (`dbLink.c:92-130`), not per narrative.
    ///
    /// Every expectation below is `bin/linux-x86_64/softIoc` (EPICS 7.0.10)
    /// answering `dbpr <rec> 1` on this same `.db` after `iocInit`.
    #[test]
    fn a_link_field_prints_the_type_dbinitlink_resolved_it_to() {
        let (db, ctx) = make_ctx();
        load_records(&ctx, DBPR_LINK_DB).expect("link .db must load");
        ctx.block_on(db.ioc_init());

        for (record, field, expected) in [
            // No text at all: `dbInitRecordLinks` leaves the devsup type,
            // which for every soft support is CONSTANT.
            ("P:UNSET", "INP", "CONSTANT"),
            // `epicsParseDouble` succeeds — CONSTANT, text kept.
            ("P:CON", "INP", "CONSTANT 5"),
            // The bracketed array constant, C's second constant test.
            ("P:ARR", "INP", "CONSTANT [1,2,3]"),
            // PV_LINK, target local, no CA/CP/CPP — `dbDbInitLink`.
            ("P:LOCAL", "INP", "DB_LINK L:B.VAL NPP NMS"),
            // PV_LINK, target LOCAL but CPP set: C skips `dbDbInitLink`
            // entirely (`dbAccess.c:1104`), so the local target still
            // becomes a CA link. This port keeps such a link on the DB
            // link set on purpose; only the printed identity follows C.
            ("P:LOCALCP", "INP", "CA_LINK L:B.VAL CPP MS"),
            // PV_LINK, target not in this database — the locality arm.
            ("P:EXT", "INP", "CA_LINK OTHER:PV NPP NMS"),
            // PV_LINK with an explicit CA modifier.
            ("P:CA", "INP", "CA_LINK OTHER:PV CA NMS"),
            // Braces make it JSON_LINK in C whatever the JSON evaluates
            // to; this port resolves `{const:…}` to a constant link, so
            // the variant alone would have answered CONSTANT here.
            ("P:JSON", "INP", r#"JSON_LINK {"const":[1,2,3]}"#),
            // DBF_FWDLINK carries no process class and no MS switch.
            ("P:FL", "FLNK", "DB_LINK L:B"),
        ] {
            let printed = run_cmd(&ctx, "dbpr", &[record, "1"]);
            assert_eq!(
                dbpr_field(&printed, field),
                expected,
                "{record}.{field} in {printed:?}"
            );
        }
    }

    /// Before `iocInit` the link still HAS its text, so C prints the literal
    /// word `LINK` and `dbGetString` returns that text verbatim — no
    /// modifiers filled in (`dbStaticLib.c:1914-1915`, `:2214-2231`).
    /// `dbpr` is the only reader that can reach a record in this state.
    #[test]
    fn before_ioc_init_a_link_prints_the_word_link_and_its_stored_text() {
        let (_db, ctx) = make_ctx();
        load_records(&ctx, DBPR_LINK_DB).expect("link .db must load");

        for (record, field, expected) in [
            // Text present — verbatim, NOT `L:B.VAL NPP NMS`.
            ("P:LOCAL", "INP", "LINK L:B.VAL"),
            ("P:CON", "INP", "LINK 5"),
            ("P:JSON", "INP", r#"LINK {"const":[1,2,3]}"#),
            // No text: `!plink->text` holds from the start, so this record
            // reads the same on both sides of `iocInit`.
            ("P:UNSET", "INP", "CONSTANT"),
        ] {
            let printed = run_cmd(&ctx, "dbpr", &[record, "1"]);
            assert_eq!(
                dbpr_field(&printed, field),
                expected,
                "{record}.{field} in {printed:?}"
            );
        }
    }

    #[test]
    fn test_dbl_filter_by_type() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("AI_REC", Box::new(AiRecord::new(1.0)))
                .await
                .unwrap();
            db.add_record(
                "BO_REC",
                Box::new(crate::server::records::bo::BoRecord::new(0)),
            )
            .await
            .unwrap();
        });

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbl").unwrap();
        let tokens = vec!["ai".to_string()];
        let args = parse_args(&tokens, &cmd.args).unwrap();
        let result = cmd.handler.call(&args, &ctx);
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    #[test]
    fn test_exit() {
        let (_db, ctx) = make_ctx();
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("exit").unwrap();
        let args = parse_args(&[], &cmd.args).unwrap();
        let result = cmd.handler.call(&args, &ctx);
        assert!(matches!(result, Ok(CommandOutcome::Exit)));
    }

    /// C `dbLexRoutines.c:1170-1188` parity: dbLoadRecords MUST allow
    /// the same record name to be re-loaded with the SAME record type
    /// and merge fields into the existing instance. ADCore convention
    /// (simDetector.template overriding ColorMode menu from the
    /// included NDArrayBase.template) depends on this.
    #[test]
    fn test_db_load_records_same_type_duplicate_merges_fields() {
        use std::io::Write;
        let (db, ctx) = make_ctx();

        // Write a tiny .db with the duplicate-record pattern: an mbbo
        // declared twice, with the second block overriding ZRST.
        let tmp = tempfile::Builder::new()
            .suffix(".db")
            .tempfile()
            .expect("tempfile");
        writeln!(
            tmp.as_file(),
            r#"
record(mbbo, "DUP:CM") {{
    field(DESC, "first")
    field(ZRST, "Mono")
    field(ONST, "Bayer")
}}

record(mbbo, "DUP:CM") {{
    field(DESC, "second")
    field(ZRST, "Mono-Override")
}}
"#
        )
        .expect("write tempfile");

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbLoadRecords").unwrap();
        let args = parse_args(&[tmp.path().to_string_lossy().to_string()], &cmd.args).unwrap();
        let result = cmd.handler.call(&args, &ctx);
        assert!(
            matches!(result, Ok(CommandOutcome::Continue)),
            "merge-duplicate must succeed; got Err? {}",
            result.is_err()
        );

        ctx.block_on(async {
            let rec = db
                .get_record("DUP:CM")
                .expect("DUP:CM must be registered exactly once");
            let inst = rec.read();
            // Last-write-wins: DESC + ZRST should reflect the SECOND
            // record block. ONST stays from the FIRST block since
            // the second didn't override it.
            assert_eq!(inst.common.desc, "second", "second block's DESC must win");
            assert_eq!(
                inst.record.get_field("ZRST"),
                Some(crate::types::EpicsValue::String("Mono-Override".into())),
                "second block's ZRST must override the first"
            );
            assert_eq!(
                inst.record.get_field("ONST"),
                Some(crate::types::EpicsValue::String("Bayer".into())),
                "ONST from first block survives (no override)"
            );
        });
    }

    /// Q2: a merge-reload that loads a new breakpoint table AND repoints an
    /// existing record's LINR to it. The merge branch updates the existing
    /// instance in place (it never goes back through `add_record`'s install),
    /// so the registry must reach it via `add_breaktables`' re-install. Proves
    /// the repointed record linearises through the table loaded in the same
    /// reload.
    #[test]
    fn test_db_load_records_merge_repoints_linr_to_new_breaktable() {
        use std::io::Write;
        let (db, ctx) = make_ctx();

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbLoadRecords").unwrap();

        // First load: an `ao` with no breakpoint table.
        let tmp1 = tempfile::Builder::new()
            .suffix(".db")
            .tempfile()
            .expect("tempfile");
        writeln!(
            tmp1.as_file(),
            r#"record(ao, "BPT:RBK") {{ field(DESC, "first") }}"#
        )
        .expect("write tempfile 1");
        let args = parse_args(&[tmp1.path().to_string_lossy().to_string()], &cmd.args).unwrap();
        assert!(matches!(
            cmd.handler.call(&args, &ctx),
            Ok(CommandOutcome::Continue)
        ));

        // Second load: define the table AND repoint the existing record's LINR.
        let tmp2 = tempfile::Builder::new()
            .suffix(".db")
            .tempfile()
            .expect("tempfile");
        writeln!(
            tmp2.as_file(),
            r#"
breaktable(ramp) {{ 0 0  100 10  300 30 }}
record(ao, "BPT:RBK") {{ field(LINR, "ramp") }}
"#
        )
        .expect("write tempfile 2");
        let args = parse_args(&[tmp2.path().to_string_lossy().to_string()], &cmd.args).unwrap();
        assert!(matches!(
            cmd.handler.call(&args, &ctx),
            Ok(CommandOutcome::Continue)
        ));

        ctx.block_on(async {
            let rec = db.get_record("BPT:RBK").expect("BPT:RBK exists");
            let mut inst = rec.write();
            // The merge resolved "ramp" (non-standard) to the first user-table
            // index (15); standard menuConvert names reserve 3..=14.
            assert_eq!(
                inst.record.get_field("LINR"),
                Some(crate::types::EpicsValue::Short(15))
            );
            // eng 5.0 -> raw 50 through the re-installed registry.
            inst.record
                .put_field("VAL", crate::types::EpicsValue::Double(5.0))
                .unwrap();
            inst.record.process().unwrap();
            assert_eq!(
                inst.record.get_field("RVAL"),
                Some(crate::types::EpicsValue::Long(50)),
                "merge-repointed LINR must linearise through the new table"
            );
        });
    }

    /// Regression: a record resolved to a breakpoint table in load #1 must
    /// keep converting through THAT table after load #2 adds an
    /// alphabetically-earlier table. With the old name-sorted index, loading
    /// "alpha" shifted "zebra" from index 15 to 16 while the record's frozen
    /// LINR stayed 15, so re-install silently re-pointed it to "alpha".
    /// Load-order user-table indices keep "zebra" at 15.
    #[test]
    fn test_db_load_records_later_table_does_not_repoint_resolved_record() {
        use std::io::Write;
        let (db, ctx) = make_ctx();
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbLoadRecords").unwrap();

        // Load #1: table "zebra" (slope 0.1) + record A referencing it.
        let tmp1 = tempfile::Builder::new()
            .suffix(".db")
            .tempfile()
            .expect("tempfile");
        writeln!(
            tmp1.as_file(),
            r#"
breaktable(zebra) {{ 0 0  100 10 }}
record(ai, "A:BPT") {{ field(LINR, "zebra") }}
"#
        )
        .expect("write tempfile 1");
        let args = parse_args(&[tmp1.path().to_string_lossy().to_string()], &cmd.args).unwrap();
        assert!(matches!(
            cmd.handler.call(&args, &ctx),
            Ok(CommandOutcome::Continue)
        ));

        // Load #2: an alphabetically-earlier table "alpha" (slope 1.0).
        let tmp2 = tempfile::Builder::new()
            .suffix(".db")
            .tempfile()
            .expect("tempfile");
        writeln!(tmp2.as_file(), r#"breaktable(alpha) {{ 0 0  100 100 }}"#)
            .expect("write tempfile 2");
        let args = parse_args(&[tmp2.path().to_string_lossy().to_string()], &cmd.args).unwrap();
        assert!(matches!(
            cmd.handler.call(&args, &ctx),
            Ok(CommandOutcome::Continue)
        ));

        ctx.block_on(async {
            let rec = db.get_record("A:BPT").expect("A:BPT exists");
            let mut inst = rec.write();
            inst.record
                .put_field("RVAL", crate::types::EpicsValue::Long(50))
                .unwrap();
            inst.record.process().unwrap();
            // zebra: 50 * 0.1 = 5.0. alpha (the wrong table) would give 50.0.
            assert_eq!(
                inst.record.get_field("VAL"),
                Some(crate::types::EpicsValue::Double(5.0)),
                "resolved record must still convert through zebra, not the later alpha"
            );
        });
    }

    /// R5-6: C registers `postEvent` with one `iocshArgString` "event
    /// name" and its handler is `postEvent(eventNameToHandle(sval))` —
    /// nothing is printed. The port declared the argument as an Int, so
    /// `postEvent reset` was refused before the handler ran and every
    /// named `EVNT` was unreachable from iocsh.
    #[test]
    fn post_event_takes_the_event_name_and_prints_nothing() {
        let (db, ctx) = make_ctx();
        let mut reg = CommandRegistry::new();
        register_builtins(&mut reg);
        let cmd = reg.get("postEvent").unwrap();
        assert_eq!(cmd.args.len(), 1);
        assert!(matches!(cmd.args[0].arg_type, ArgType::String));
        // C registers this ONE name. `post_event` is `dbScan.c:547`'s
        // int-taking compatibility function, never an iocsh command, so a
        // shell that answers it advertises a command no C IOC has.
        assert!(reg.get("post_event").is_none());

        load_records(
            &ctx,
            r#"record(ai, "X:E") { field(SCAN, "Event") field(EVNT, "reset") }"#,
        )
        .expect("load");
        ctx.block_on(db.ioc_init());

        let out = run_cmd(&ctx, "postEvent", &["reset"]);
        assert_eq!(out, "", "C's postEventCallFunc prints nothing");
    }

    /// R5-2: `record("*", …)` modifies the record already in the database
    /// and `record("#", …)` deletes it (`dbLexRoutines.c:1136-1157`).
    /// Neither is a record type, so the port's factory lookup failed and
    /// took the whole load with it.
    #[test]
    fn record_star_modifies_in_place_and_hash_deletes() {
        let (db, ctx) = make_ctx();
        load_records(&ctx, r#"record(ai, "X:T") { field(VAL, "5") }"#).expect("first load");

        load_records(&ctx, r#"record("*", "X:T") { field(VAL, "90") }"#)
            .expect("a '*' block must load");
        ctx.block_on(async {
            let rec = db.get_record("X:T").expect("X:T");
            let inst = rec.read();
            assert_eq!(
                inst.record.get_field("VAL"),
                Some(crate::types::EpicsValue::Double(90.0))
            );
        });

        load_records(&ctx, r##"record("#", "X:T") { }"##).expect("a '#' block must load");
        assert!(!exists(&db, &ctx, "X:T"), "'#' deletes the record");
    }

    /// The two not-found halves are reported differently: C `yyerror`s the
    /// `*` miss and only warns on the `#` miss, so the second leaves the
    /// load's status clean. Either way the block's body is skipped and the
    /// records after it still load.
    #[test]
    fn record_star_and_hash_report_a_missing_name_differently() {
        let (db, ctx) = make_ctx();
        let err = load_records(
            &ctx,
            r#"
record("*", "NO:SUCH") { field(VAL, "1") }
record(ai, "AFTER:STAR") { }
"#,
        )
        .expect_err("a '*' miss must fail the call's status");
        assert_eq!(err, format!("{ERL_ERROR}: Record 'NO:SUCH' not found"));
        assert!(exists(&db, &ctx, "AFTER:STAR"));

        load_records(
            &ctx,
            r##"
record("#", "NO:SUCH") { }
record(ai, "AFTER:HASH") { }
"##,
        )
        .expect("a '#' miss is only a warning");
        assert!(exists(&db, &ctx, "AFTER:HASH"));
    }

    /// R5-4: the same name at a different record type is `yyerror(NULL)`
    /// (`dbLexRoutines.c:1173-1180`), so the record is skipped and the
    /// rest of the file still loads. The message names the type being
    /// LOADED first and the type already in the database last —
    /// `recordType` then `dbGetRecordTypeName(pdbentry)` — which the port
    /// had the other way round, without C's `ERROR: ` prefix.
    #[test]
    fn test_db_load_records_different_type_duplicate_is_skipped() {
        let (db, ctx) = make_ctx();
        // Pre-register DUP:CM as an `ai`.
        ctx.block_on(async {
            db.add_record("DUP:CM", Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
        });
        let err = load_records(
            &ctx,
            r#"
record(mbbo, "DUP:CM") { field(ZRST, "Mono") }
record(ai, "SIM:C") { field(VAL, "1") }
"#,
        )
        .expect_err("the load's status must still be non-zero");
        assert_eq!(
            err,
            format!("{ERL_ERROR}: mbbo record 'DUP:CM' already exists, can't load ai record")
        );
        assert!(
            exists(&db, &ctx, "SIM:C"),
            "the record after the duplicate must still load"
        );
        ctx.block_on(async {
            let rec = db.get_record("DUP:CM").expect("DUP:CM");
            let inst = rec.read();
            assert_eq!(
                inst.record.record_type(),
                "ai",
                "the existing record keeps its own type"
            );
        });
    }

    // R19-63 — the two record-creating iocsh commands, on either side of the
    // `iocInit` boundary. C gates both on `getIocState() != iocVoid`
    // (`dbLexRoutines.c:236` for every `.db` read, and
    // `dbStaticIocRegister.c:288-291` at `f4ccf7bc8` for `dbCreateRecord`, a
    // command no release tag carries) and creates NOTHING once the IOC is
    // running.
    //
    // softIoc 7.0.10.1-DEV — `a.db` holds `CO`, `b.db` holds two records:
    //
    //     epics> dbLoadRecords("a.db")
    //     epics> iocInit
    //     epics> dbLoadRecords("b.db")
    //     ERROR: Failed to load 'b.db'
    //         Records cannot be loaded after iocInit!
    //     epics> dbCreateRecord(pdbbase,"ai","NEWREC")
    //     ERROR: 33554463 IOC already initialized - No new records can be added
    //     epics> dbl
    //     CO
    /// `dbtgf` / `dbtpf` / `dbtr` against `bin/linux-x86_64/softIoc`
    /// (EPICS R7.0.10-146-g8f5015b663d764ad75df) on the same record.
    ///
    /// `r.db`:
    ///
    /// ```text
    /// record(ai, "R:SRC") {
    ///   field(DTYP, "Soft Channel")
    ///   field(VAL,  "3.5")
    ///   field(EGU,  "mm")
    ///   field(PREC, "3")
    /// }
    /// ```
    ///
    /// ```text
    /// epics> dbtgf "R:SRC"
    /// ...
    /// DBF_DOUBLE[0]: (empty)
    /// DBF_STRING:         "3.500"
    /// DBF_CHAR:           3 = 0x3
    /// DBF_UCHAR:          3 = 0x3
    /// DBF_SHORT:          3 = 0x3
    /// DBF_USHORT:         3 = 0x3
    /// DBF_LONG:           3 = 0x3
    /// DBF_ULONG:          3 = 0x3
    /// DBF_INT64:          3 = 0x3
    /// DBF_UINT64:         3 = 0x3
    /// DBF_FLOAT:          3.5
    /// DBF_DOUBLE:         3.5
    /// DBF_ENUM:           3
    /// ```
    ///
    /// The option block above those lines is C's shape with the values
    /// C intends rather than the ones it prints — see
    /// [`dbtgf_option_lines`] for the two upstream defects that make
    /// `softIoc`'s own numbers unusable as a target.
    #[test]
    fn dbtgf_prints_the_value_in_every_dbr_request_type() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            "record(ai, \"R:SRC\") { field(DTYP, \"Soft Channel\") \
             field(VAL, \"3.5\") field(EGU, \"mm\") field(PREC, \"3\") }",
        )
        .unwrap();
        ctx.block_on(db.ioc_init());

        let out = run_cmd(&ctx, "dbtgf", &["R:SRC"]);
        let value_lines: Vec<&str> = out
            .lines()
            .skip_while(|l| !l.starts_with("DBF_"))
            .map(|l| l.trim_end())
            .collect();
        assert_eq!(
            value_lines,
            vec![
                "DBF_DOUBLE[0]: (empty)",
                "DBF_STRING:         \"3.500\"",
                "DBF_CHAR:           3 = 0x3",
                "DBF_UCHAR:          3 = 0x3",
                "DBF_SHORT:          3 = 0x3",
                "DBF_USHORT:         3 = 0x3",
                "DBF_LONG:           3 = 0x3",
                "DBF_ULONG:          3 = 0x3",
                "DBF_INT64:          3 = 0x3",
                "DBF_UINT64:         3 = 0x3",
                "DBF_FLOAT:          3.5",
                "DBF_DOUBLE:         3.5",
                "DBF_ENUM:           3",
            ],
            "full output:\n{out}"
        );

        // The option block: shapes are C's, and which lines appear is
        // C's answer too — an `ai` supplies units, precision and all
        // three limit pairs.
        let opts: Vec<&str> = out.lines().take_while(|l| !l.starts_with("DBF_")).collect();
        // C's own first line, byte for byte: the STATUS block is the one
        // `printBuffer` still reads from the right offset.
        assert_eq!(opts[0], "status = 17, severity = 0");
        assert_eq!(opts[1], "units = \"mm\"");
        assert_eq!(opts[2], "precision = 3");
        assert_eq!(opts[4], "enum strings not returned");
        assert!(opts[5].starts_with("grLong: "), "{opts:?}");
        assert!(opts[7].starts_with("ctrlLong: "), "{opts:?}");
        assert!(opts[9].starts_with("alLong: "), "{opts:?}");
    }

    /// `dbtgf`'s enum-strings line is decided by the ADDRESS's DBF class, not
    /// by whether the record owns a choice table.
    ///
    /// C's `get_enum_strs` (`dbAccess.c:160-180`) is reached only for
    /// `DBF_ENUM`, `DBF_MENU` and `DBF_DEVICE`; everything else clears the
    /// option bit. Both arms measured on `softIoc` @`R7.0.10`: the `mbbo` with
    /// no state string has been demoted to `DBF_USHORT` by its own
    /// `cvt_dbaddr`, so it reports the strings as not returned even though the
    /// record still has sixteen slots to render.
    #[test]
    fn dbtgf_enum_strings_follow_the_address_class() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            "record(bo,   \"E:B\") { field(ZNAM, \"Off\") field(ONAM, \"On\") }\n\
             record(mbbo, \"E:M\") { }\n",
        )
        .unwrap();
        ctx.block_on(db.ioc_init());

        let strs = |pv: &str| -> String {
            run_cmd(&ctx, "dbtgf", &[pv])
                .lines()
                .find(|l| l.starts_with("no_strs") || l.starts_with("enum strings"))
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(strs("E:B"), "no_strs = 2:");
        assert_eq!(strs("E:M"), "enum strings not returned");
    }

    /// C prints `<cmd> only works after iocInit` and fails the line
    /// when the record's `lset` is still NULL — `dbgf`
    /// (`dbTest.c:366-368`), `dbpf` (`:408-410`), `dbtr` (`:476-478`),
    /// `dbtgf` (`:520-522`), `dbtpf` (`:621-623`). The message is the
    /// command's own, so the line fails with nothing more printed.
    ///
    /// Measured on `softIoc` R7.0.10-146 in one session, before `iocInit`:
    /// `dbgf "R:SRC"` and `dbpf "R:SRC", "1.5"` each print their refusal,
    /// and `dbpr "R:SRC"` on the same name prints the record's fields —
    /// C gates exactly these five and not `dbpr`.
    #[test]
    fn the_dbt_commands_refuse_before_ioc_init() {
        for (cmd, tokens) in [
            ("dbgf", &["R:SRC"][..]),
            ("dbpf", &["R:SRC", "1.5"][..]),
            ("dbtgf", &["R:SRC"][..]),
            ("dbtpf", &["R:SRC", "1"][..]),
            ("dbtr", &["R:SRC"][..]),
        ] {
            let (_db, ctx) = make_ctx();
            load_records(&ctx, "record(ai, \"R:SRC\") { field(VAL, \"3.5\") }").unwrap();
            let (out, failed) = run_cmd_outcome(&ctx, cmd, tokens);
            assert_eq!(out, format!("{cmd} only works after iocInit\n"));
            assert!(failed, "{cmd} must fail the line");
        }

        // C does NOT gate `dbpr`, and the port must not either.
        let (_db, ctx) = make_ctx();
        load_records(&ctx, "record(ai, \"R:SRC\") { field(VAL, \"3.5\") }").unwrap();
        let (out, failed) = run_cmd_outcome(&ctx, "dbpr", &["R:SRC"]);
        assert!(!failed, "dbpr is ungated before iocInit");
        assert!(out.contains("VAL : 3.5"), "full output:\n{out}");
    }

    /// C `dbtpf` puts the text as each `DBR_*` type and prints the
    /// record's native read-back after every put its `epicsParse*`
    /// accepted. Measured on the same `softIoc` with `R:SRC` an `ai`:
    ///
    /// ```text
    /// epics> dbtpf "R:SRC", "9.25"
    /// Put as DBR_STRING Ok, result as DBF_DOUBLE:         9.25
    /// Cvt to DBR_CHAR failed.
    /// Cvt to DBR_UCHAR failed.
    /// Cvt to DBR_SHORT failed.
    /// Cvt to DBR_USHORT failed.
    /// Cvt to DBR_LONG failed.
    /// Cvt to DBR_ULONG failed.
    /// Cvt to DBR_INT64 failed.
    /// Cvt to DBR_UINT64 failed.
    /// Put as DBR_FLOAT  Ok, result as DBF_DOUBLE:         9.25
    /// Put as DBR_DOUBLE Ok, result as DBF_DOUBLE:         9.25
    /// Cvt to DBR_ENUM failed.
    /// ```
    #[test]
    fn dbtpf_puts_the_text_as_every_dbr_request_type() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            "record(ai, \"R:SRC\") { field(DTYP, \"Soft Channel\") field(VAL, \"3.5\") }",
        )
        .unwrap();
        ctx.block_on(db.ioc_init());

        let out = run_cmd(&ctx, "dbtpf", &["R:SRC", "9.25"]);
        let lines: Vec<&str> = out.lines().map(|l| l.trim_end()).collect();
        assert_eq!(
            lines,
            vec![
                "Put as DBR_STRING Ok, result as DBF_DOUBLE:         9.25",
                "Cvt to DBR_CHAR failed.",
                "Cvt to DBR_UCHAR failed.",
                "Cvt to DBR_SHORT failed.",
                "Cvt to DBR_USHORT failed.",
                "Cvt to DBR_LONG failed.",
                "Cvt to DBR_ULONG failed.",
                "Cvt to DBR_INT64 failed.",
                "Cvt to DBR_UINT64 failed.",
                "Put as DBR_FLOAT  Ok, result as DBF_DOUBLE:         9.25",
                "Put as DBR_DOUBLE Ok, result as DBF_DOUBLE:         9.25",
                "Cvt to DBR_ENUM failed.",
            ],
            "full output:\n{out}"
        );
    }

    /// `dbtr` is `dbProcess` followed by `dbpr` at level 3
    /// (`dbTest.c:487-495`), so its field set is `dbpr <rec>, 3`'s and
    /// the process actually happened. Measured on `softIoc`, `dbtr`
    /// alone leaves `R:SRC` with `UDF: 0` where a bare `dbpr` before it
    /// reports `UDF: 1`.
    #[test]
    fn dbtr_processes_then_prints_at_level_three() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            "record(ai, \"R:SRC\") { field(DTYP, \"Soft Channel\") field(VAL, \"3.5\") }",
        )
        .unwrap();
        ctx.block_on(db.ioc_init());

        let printed = run_cmd(&ctx, "dbtr", &["R:SRC"]);
        let at_level_3 = run_cmd(&ctx, "dbpr", &["R:SRC", "3"]);
        assert_eq!(
            dbpr_field_names(&printed),
            dbpr_field_names(&at_level_3),
            "dbtr prints dbpr's level-3 field set\n{printed}"
        );
        assert!(
            printed.contains("UDF : 0"),
            "the record was processed:\n{printed}"
        );
    }

    /// C `dbtr` on an unknown PV prints `nameToAddr`'s line and nothing
    /// else, and on a missing name prints the usage line — both fail
    /// the shell line, because `dbtrCallFunc` routes the non-zero
    /// return through `iocshSetError` (`dbIocRegister.c:298`).
    #[test]
    fn dbtr_reports_a_missing_name_and_an_unknown_pv() {
        let (_db, ctx) = make_ctx();
        assert_eq!(run_cmd(&ctx, "dbtr", &[]), "Usage: dbtr \"pv name\"\n");
        assert_eq!(run_cmd(&ctx, "dbtr", &["NOPE"]), "PV 'NOPE' not found\n");
        assert_eq!(run_cmd(&ctx, "dbtgf", &[]), "Usage: dbtgf \"pv name\"\n");
        assert_eq!(
            run_cmd(&ctx, "dbtpf", &["R:SRC"]),
            "Usage: dbtpf \"pv name\", \"value\"\n"
        );
    }

    /// C builds the zero-element header and `(empty)` as one message
    /// (`strcat(pmsg, "(empty)")`, `dbTest.c:1144`), so the word starts
    /// right after the header rather than at the next tab stop.
    /// Measured on `softIoc` with `W:EMPTY` a `waveform(DOUBLE, NELM=5)`:
    ///
    /// ```text
    /// epics> dbgf "W:EMPTY"
    /// DBF_DOUBLE[0]: (empty)
    /// ```
    #[test]
    fn an_empty_array_keeps_empty_on_the_header_s_tab_stop() {
        assert_eq!(
            printbuffer_lines("DOUBLE", 0, Some(&[])),
            vec!["DBF_DOUBLE[0]: (empty)        "]
        );
    }

    /// C `dbjlr` on the reference softIoc (R7.0.10-146), measured with
    /// the same five records this test loads:
    ///
    /// ```text
    /// epics> dbjlr
    /// JSON links in all records
    ///
    ///   ai record 'R:SRC':
    ///   ai record 'R:CALINK':
    ///   ai record 'R:JLINK':
    ///     Link field 'INP':
    ///       'const': double 4.25
    ///   calc record 'R:CALC':
    ///   longout record 'R:LO':
    /// epics> dbjlr "R:NOPE"
    /// JSON links in record 'R:NOPE'
    ///
    /// ```
    #[test]
    fn dbjlr_reports_every_records_json_links() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            "record(ai, \"R:SRC\") { field(DTYP, \"Soft Channel\") field(VAL, \"3.5\") } \
             record(ai, \"R:JLINK\") { field(DTYP, \"Soft Channel\") field(INP, {\"const\": 4.25}) } \
             record(calc, \"R:CALC\") { field(INPA, \"R:SRC CP\") field(CALC, \"A*2\") } \
             record(longout, \"R:LO\") { field(DTYP, \"Soft Channel\") }",
        )
        .unwrap();
        ctx.block_on(db.ioc_init());

        let out = run_cmd(&ctx, "dbjlr", &[]);
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            vec![
                "JSON links in all records",
                "",
                "  ai record 'R:SRC':",
                "  ai record 'R:JLINK':",
                "    Link field 'INP':",
                "      'const': double 4.25",
                "  calc record 'R:CALC':",
                "  longout record 'R:LO':",
            ],
            "full output:\n{out}"
        );

        // A named record prints its own header and stops there; a name
        // the walk never matches prints the header alone.
        assert_eq!(
            run_cmd(&ctx, "dbjlr", &["R:JLINK"])
                .lines()
                .collect::<Vec<_>>(),
            vec![
                "JSON links in record 'R:JLINK'",
                "",
                "  ai record 'R:JLINK':",
                "    Link field 'INP':",
                "      'const': double 4.25",
            ]
        );
        assert_eq!(
            run_cmd(&ctx, "dbjlr", &["R:NOPE"]),
            "JSON links in record 'R:NOPE'\n\n"
        );
    }

    /// C `lnkConst_report`'s type word comes from which yajl callback
    /// the token reached, and its array arm prints the element list only
    /// from level 2 (`lnkConst.c:286-347`).
    #[test]
    fn const_link_report_names_the_json_type_it_parsed() {
        assert_eq!(
            const_link_report_lines("4.25", 0, 6),
            vec!["      'const': double 4.25"]
        );
        assert_eq!(
            const_link_report_lines("4", 0, 6),
            vec!["      'const': integer 4"]
        );
        assert_eq!(
            const_link_report_lines("\"hi\"", 0, 6),
            vec!["      'const': string \"hi\""]
        );
        assert_eq!(
            const_link_report_lines("[1, 2, 3]", 0, 6),
            vec!["      'const': array of 3 integers"]
        );
        assert_eq!(
            const_link_report_lines("[1, 2, 3]", 2, 6),
            vec!["      'const': array of 3 integers", "        [1, 2, 3]"]
        );
        assert_eq!(
            const_link_report_lines("[1.5]", 2, 6),
            vec!["      'const': array of 1 double", "        [1.5]"]
        );
    }

    /// C `dbel` (`dbEvent.c:154-251`), registered at `dbIocRegister.c:597`.
    ///
    /// Measured on `softIoc` R7.0.10-146 against a database whose
    /// `R:CALC.INPA` is `"R:SRC CP"` — the CP link is what puts the single
    /// `VALUE|ALARM` subscription on `R:SRC.VAL`:
    ///
    /// ```text
    /// epics> dbel "R:SRC"
    /// 1 PV Event Subscriptions ( monitors ).
    /// epics> dbel "R:SRC", 1
    /// 1 PV Event Subscriptions ( monitors ).
    ///  VAL { VALUE ALARM }
    /// epics> dbel "R:SRC", 2
    /// 1 PV Event Subscriptions ( monitors ).
    ///  VAL { VALUE ALARM }, thread=0x7c83dc023690, queue empty
    /// epics> dbel "R:LO"
    /// "R:LO": No PV event subscriptions ( monitors ).
    /// epics> dbel "R:NOPE"
    /// epics> dbel
    /// ```
    ///
    /// The port reaches that state by subscribing directly, because a CP
    /// link lands in `cp_links` here and not in the record's monitor list —
    /// the divergence `cmd_dbel`'s own comment records. `thread=%p` is the
    /// one token dropped from the level-2 line.
    #[test]
    fn dbel_lists_a_records_event_subscriptions() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            "record(ai, \"R:SRC\") { field(DTYP, \"Soft Channel\") field(VAL, \"3.5\") } \
             record(longout, \"R:LO\") { field(DTYP, \"Soft Channel\") }",
        )
        .unwrap();
        ctx.block_on(db.ioc_init());

        let reader = db
            .get_record("R:SRC")
            .unwrap()
            .write()
            .add_subscriber(
                "VAL",
                1,
                crate::types::DbFieldType::Double,
                (crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::ALARM)
                    .bits(),
            )
            .unwrap();

        assert_eq!(
            run_cmd(&ctx, "dbel", &["R:SRC"]),
            "1 PV Event Subscriptions ( monitors ).\n"
        );
        assert_eq!(
            run_cmd(&ctx, "dbel", &["R:SRC", "1"]),
            "1 PV Event Subscriptions ( monitors ).\n VAL { VALUE ALARM }\n"
        );
        assert_eq!(
            run_cmd(&ctx, "dbel", &["R:SRC", "2"]),
            "1 PV Event Subscriptions ( monitors ).\n VAL { VALUE ALARM }, queue empty\n",
            "C's level-2 line carries a `thread=` token the port has no thread for"
        );
        assert_eq!(
            run_cmd(&ctx, "dbel", &["R:LO"]),
            "\"R:LO\": No PV event subscriptions ( monitors ).\n"
        );
        // C prints the name as given, not the record it resolved to.
        assert_eq!(
            run_cmd(&ctx, "dbel", &["R:LO.VAL"]),
            "\"R:LO.VAL\": No PV event subscriptions ( monitors ).\n"
        );
        // `dbNameToAddr` failure answers on the errlog; stdout stays empty.
        assert_eq!(run_cmd(&ctx, "dbel", &["R:NOPE"]), "");
        // C `if ( ! pname ) return DB_EVENT_OK;` — no name is silent success.
        assert_eq!(run_cmd(&ctx, "dbel", &[]), "");

        drop(reader);
    }

    /// The queue counters C's level-3 block prints, and the extra newline
    /// C's `duplicate count` conversion carries inside its own format string
    /// (`dbEvent.c:231`).
    #[test]
    fn dbel_level_3_reports_the_queue_counters() {
        use crate::server::event_queue::QueReport;
        use crate::server::recgbl::EventMask;

        let mask = (EventMask::VALUE | EventMask::LOG).bits();
        let idle = QueReport {
            npend: 0,
            ring_space: 144,
            ring_size: 144,
            nreplace: 0,
            latest_only: false,
            n_duplicates: 0,
        };
        assert_eq!(
            dbel_subscription_lines("VAL", mask, 3, &idle),
            vec![" VAL { VALUE LOG }, queue empty"]
        );

        let busy = QueReport {
            npend: 2,
            ring_space: 140,
            ring_size: 144,
            nreplace: 7,
            latest_only: true,
            n_duplicates: 3,
        };
        assert_eq!(
            dbel_subscription_lines("SEVR", mask, 3, &busy),
            vec![
                "SEVR { VALUE LOG } undelivered=2, unused entries=140, \
                 discarded by replacement=7, queueing disabled, duplicate count =3",
                "",
            ]
        );

        // `%4.4s` truncates as well as pads.
        assert_eq!(
            dbel_subscription_lines("PROC", EventMask::PROPERTY.bits(), 0, &idle),
            vec!["PROC { PROPERTY }"]
        );
        let full = QueReport {
            ring_space: 0,
            ..idle
        };
        assert_eq!(
            dbel_subscription_lines("A", EventMask::NONE.bits(), 2, &full),
            vec!["   A { }, queue full"]
        );
    }

    /// C `dbtpn` (`dbNotify.c:590-625`), registered at
    /// `dbIocRegister.c:620`. Measured on `softIoc` R7.0.10-146 against the
    /// same `r.db` the rest of this family was measured on (`R:SRC` is an
    /// `ai` with `VAL 3.5` and `PREC 3`):
    ///
    /// ```text
    /// epics> dbtpn "R:LO", "7"
    /// epics> dbtpnCallback: success record=R:LO
    /// epics> dbtpn "R:SRC"
    /// epics> dbtpn:getCallback value 3.500
    /// dbtpnCallback: success record=R:SRC
    /// epics> dbtpn "R:NOPE", "1"
    /// dbtpn: No such channel
    /// epics> dbtpn
    /// Usage: dbtpn "name", "value"
    /// ```
    ///
    /// The callback lines land after the next prompt because C runs them on
    /// its own `dbtpn` thread; the port spawns for the same reason, so the
    /// lines are asserted through `dbtpn_lines` — the value the spawned task
    /// prints — rather than through the shell's captured output.
    #[test]
    fn dbtpn_puts_processes_and_reads_back_as_dbr_string() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            "record(ai, \"R:SRC\") { field(DTYP, \"Soft Channel\") field(VAL, \"3.5\") \
             field(EGU, \"mm\") field(PREC, \"3\") } \
             record(longout, \"R:LO\") { field(DTYP, \"Soft Channel\") }",
        )
        .unwrap();
        ctx.block_on(db.ioc_init());

        assert_eq!(
            ctx.block_on(dbtpn_lines(&db, "R:LO", Some("7".to_string()))),
            vec!["dbtpnCallback: success record=R:LO"]
        );
        assert_eq!(db.get_pv("R:LO").unwrap(), EpicsValue::Long(7));

        assert_eq!(
            ctx.block_on(dbtpn_lines(&db, "R:SRC", None)),
            vec![
                "dbtpn:getCallback value 3.500",
                "dbtpnCallback: success record=R:SRC",
            ],
            "C reads the channel back as DBR_STRING, which honours PREC"
        );

        // The two arms C answers on the shell thread.
        assert_eq!(
            run_cmd(&ctx, "dbtpn", &["R:NOPE", "1"]),
            "dbtpn: No such channel\n"
        );
        assert_eq!(
            run_cmd(&ctx, "dbtpn", &["R:SRC.NOSUCH", "1"]),
            "dbtpn: No such channel\n",
            "C `dbChannelCreate` resolves the FIELD too"
        );
        assert_eq!(
            run_cmd(&ctx, "dbtpn", &[]),
            "Usage: dbtpn \"name\", \"value\"\n"
        );
    }

    /// `tpn` is the put-only sibling: both arguments are required, the
    /// completion line is `doneCallback`'s (`db_test.c:206-211`) and it
    /// names the RECORD, and a channel that will not resolve is
    /// `dbChannel_create` failing, which is a different sentence from
    /// `dbtpn`'s.
    ///
    /// ```text
    /// epics> tpn "R:LO", "7"
    /// epics> tpnCallback 'R:LO': Success
    /// epics> tpn "R:NOPE", "1"
    /// Channel couldn't be created
    /// epics> tpn "R:LO"
    /// Usage: tpn "pv_name", "value"
    /// ```
    #[test]
    fn tpn_puts_and_processes_then_names_the_record_in_its_callback() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            "record(longout, \"R:LO\") { field(DTYP, \"Soft Channel\") }",
        )
        .unwrap();
        ctx.block_on(db.ioc_init());

        assert_eq!(
            ctx.block_on(tpn_lines(&db, "R:LO", "7".to_string())),
            vec!["tpnCallback 'R:LO': Success"]
        );
        assert_eq!(db.get_pv("R:LO").unwrap(), EpicsValue::Long(7));

        // The callback names the record, not the channel the caller typed.
        assert_eq!(
            ctx.block_on(tpn_lines(&db, "R:LO.VAL", "9".to_string())),
            vec!["tpnCallback 'R:LO': Success"]
        );

        // The two arms C answers on the shell thread.
        assert_eq!(
            run_cmd(&ctx, "tpn", &["R:NOPE", "1"]),
            "Channel couldn't be created\n"
        );
        assert_eq!(
            run_cmd(&ctx, "tpn", &["R:LO.NOSUCH", "1"]),
            "Channel couldn't be created\n",
            "C `dbChannel_create` resolves the FIELD too"
        );
        assert_eq!(
            run_cmd(&ctx, "tpn", &["R:LO"]),
            "Usage: tpn \"pv_name\", \"value\"\n",
            "C requires BOTH arguments; dbtpn does not"
        );
        assert_eq!(
            run_cmd(&ctx, "tpn", &[]),
            "Usage: tpn \"pv_name\", \"value\"\n"
        );
    }

    /// Load a `.db` and report the diagnostic the load produced.
    ///
    /// Through [`db_read_database`] rather than the `dbLoadRecords` command,
    /// because the command deliberately keeps none of this text: it writes
    /// C's `Failed to load` summary and answers `CommandOutcome::Failed`,
    /// and the wording under test belongs to the read. Asserting it here is
    /// asserting it where it is produced.
    fn load_records(ctx: &CommandContext, body: &str) -> Result<(), String> {
        use std::io::Write;
        let tmp = tempfile::Builder::new().suffix(".db").tempfile().unwrap();
        write!(tmp.as_file(), "{body}").unwrap();
        let mut faults = db_loader::DbFaults::default();
        match db_read_database(ctx, &tmp.path().to_string_lossy(), "", "", &mut faults) {
            Ok(_) => Ok(()),
            Err(DbReadFailure::Rejected) => Err(faults
                .first_diagnostic()
                .expect("a rejected read reported at least one diagnostic")),
            Err(DbReadFailure::CannotOpen) => Err(format!(
                "{ERL_ERROR}: Can't open file '{}'",
                tmp.path().display()
            )),
            Err(DbReadFailure::AfterIocInit) => Err(format!(
                "{ERL_ERROR}: Failed to load '{}'\n    Records cannot be loaded after iocInit!",
                tmp.path().display()
            )),
        }
    }

    fn create_record(ctx: &CommandContext, name: &str) -> Result<(), String> {
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbCreateRecord").unwrap();
        let args = parse_args(
            &["pdbbase".to_string(), "ai".to_string(), name.to_string()],
            &cmd.args,
        )
        .unwrap();
        cmd.handler.call(&args, ctx).map(|_| ())
    }

    fn exists(db: &PvDatabase, ctx: &CommandContext, name: &str) -> bool {
        ctx.block_on(async { db.get_record(name).is_some() })
    }

    /// I-R3-3: `dbLoadRecords` resolves its file through C `dbOpenFile`
    /// (`dbLexRoutines.c:174-175`), so a name that already carries a
    /// separator goes straight to a bare open and the path list is not
    /// consulted — even when the list would have resolved it. The port
    /// searched the list for any relative name that did not exist in the
    /// process CWD.
    #[test]
    #[serial_test::serial(epics_env)]
    fn db_load_records_does_not_search_the_path_list_for_a_name_with_a_separator() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let _path = DbIncludePath::set(dir.path());
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        write_file(
            &dir.path().join("sub"),
            "sep.db",
            r#"record(ai, "SEP") { }"#,
        );

        let (_out, err, result) = run_capturing(&ctx, "dbLoadRecords", &["sub/sep.db"]);
        assert!(
            matches!(result, Ok(CommandOutcome::Failed)),
            "a name with a separator must not be searched on the path list"
        );
        assert!(err.contains("sub/sep.db"), "got: {err}");
        assert!(!exists(&db, &ctx, "SEP"));

        // Control: the same file under a bare name IS found on the list.
        write_file(dir.path(), "bare.db", r#"record(ai, "BARE") { }"#);
        let (_out, _err, result) = run_capturing(&ctx, "dbLoadRecords", &["bare.db"]);
        assert!(
            matches!(result, Ok(CommandOutcome::Continue)),
            "bare name on the list"
        );
        assert!(exists(&db, &ctx, "BARE"));
    }

    /// I-R3-4(a): a file-scope alias naming a record an EARLIER
    /// `dbLoadRecords` installed resolves, because C `dbAlias`
    /// (`dbLexRoutines.c:1508`) looks the target up in `savedPdbbase`,
    /// not in the file being parsed.
    #[test]
    #[serial_test::serial(epics_env)]
    fn db_load_records_alias_resolves_against_an_earlier_load() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let _path = DbIncludePath::set(dir.path());
        write_file(
            dir.path(),
            "a.db",
            "record(ai, \"BASE:TEMP\") { field(DESC, \"t\") }\n",
        );
        write_file(
            dir.path(),
            "b.db",
            "alias(\"BASE:TEMP\", \"OLD:TEMP\")\nrecord(ai, \"B:X\") { }\n",
        );

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbLoadRecords").unwrap();
        for name in ["a.db", "b.db"] {
            let args = parse_args(&[name.to_string()], &cmd.args).unwrap();
            cmd.handler.call(&args, &ctx).unwrap_or_else(|e| {
                panic!("{name}: {e}");
            });
        }

        assert!(exists(&db, &ctx, "BASE:TEMP"));
        assert!(exists(&db, &ctx, "B:X"), "b.db's own record must load");
        assert_eq!(db.resolve_alias("OLD:TEMP").as_deref(), Some("BASE:TEMP"));
    }

    /// I-R3-4(b): a target NO load owns is a diagnostic that fails the
    /// call's status, and the records that did parse stay installed —
    /// C's `yyerror(NULL)` sets `yyFailed` and returns 0, so the parse
    /// runs to the end and `dbReadCOM` still reports the failure.
    #[test]
    #[serial_test::serial(epics_env)]
    fn db_load_records_unknown_alias_target_fails_status_but_keeps_records() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let _path = DbIncludePath::set(dir.path());
        write_file(
            dir.path(),
            "c.db",
            "alias(\"NOPE\", \"BAD\")\nrecord(ai, \"C:Y\") { }\n",
        );

        // At the read, which owns the wording: the command keeps none of
        // it — it writes C's summary and answers `Failed`.
        let mut faults = db_loader::DbFaults::default();
        let Err(DbReadFailure::Rejected) = db_read_database(&ctx, "c.db", "", "", &mut faults)
        else {
            panic!("an unknown alias target must fail the call's status");
        };
        let err = faults.first_diagnostic().unwrap();
        assert!(err.contains("names an unknown record 'NOPE'"), "got: {err}");
        assert!(exists(&db, &ctx, "C:Y"), "the record that parsed must stay");
        assert!(db.resolve_alias("BAD").is_none());
    }

    /// R5-3: an `include` C cannot open is `yyerror(NULL)`
    /// (`dbLexRoutines.c:450-456`), not `yyerrorAbort` — the records on
    /// either side of it still load and only the call's status goes
    /// non-zero. The port propagated the failure out of the include
    /// expansion, so `dbl` listed nothing.
    #[test]
    #[serial_test::serial(epics_env)]
    fn an_unopenable_include_keeps_the_records_around_it() {
        let (db, ctx) = make_ctx();
        let err = load_records(
            &ctx,
            r#"
record(ai, "SIM:A") { field(VAL, "1") }
include "missing.db"
record(ai, "SIM:B") { field(VAL, "2") }
"#,
        )
        .expect_err("the load's status must still be non-zero");
        assert_eq!(
            err,
            format!("{ERL_ERROR}: Can't open include file 'missing.db'")
        );
        assert!(exists(&db, &ctx, "SIM:A"));
        assert!(exists(&db, &ctx, "SIM:B"));
    }

    /// Boundary: LOAD phase (pre-`iocInit`). Both creators work — the control
    /// case, so the gate below is not vacuous.
    #[test]
    fn record_creation_before_ioc_init_is_allowed() {
        let (db, ctx) = make_ctx();

        load_records(&ctx, r#"record(ai, "PRE") { field(VAL, "1") }"#).expect("pre-init load");
        create_record(&ctx, "PRE2").expect("pre-init dbCreateRecord");

        assert!(exists(&db, &ctx, "PRE"));
        assert!(exists(&db, &ctx, "PRE2"));
    }

    /// Boundary: RUNNING phase. `dbLoadRecords` is refused with C's diagnostic
    /// and loads NO record from the file — not even the first one. The port used
    /// to load them all and print "Loaded 2 record(s)"; because that also
    /// re-opened the load phase, every later link classification was stranded
    /// (R19-62 — this command is its enabling condition).
    #[test]
    fn db_load_records_after_ioc_init_is_refused_and_creates_nothing() {
        let (db, ctx) = make_ctx();
        load_records(&ctx, r#"record(ai, "CO") { field(VAL, "1") }"#).expect("pre-init load");
        ctx.block_on(db.ioc_init());

        let err = load_records(
            &ctx,
            r#"
record(ai, "LATER") { field(VAL, "1") }
record(ai, "LATER2") { field(VAL, "2") }
"#,
        )
        .expect_err("C refuses a .db read once the IOC is running");

        assert!(
            err.contains("Records cannot be loaded after iocInit!"),
            "expected C's dbLoadRecords diagnostic; got {err}"
        );
        assert!(!exists(&db, &ctx, "LATER"), "no record may be created");
        assert!(!exists(&db, &ctx, "LATER2"));
        assert!(exists(&db, &ctx, "CO"), "the loaded database is untouched");
    }

    /// The same boundary for the other creator (C `dbCreateRecordCallFunc`).
    #[test]
    fn db_create_record_after_ioc_init_is_refused_and_creates_nothing() {
        let (db, ctx) = make_ctx();
        load_records(&ctx, r#"record(ai, "CO") { field(VAL, "1") }"#).expect("pre-init load");
        ctx.block_on(db.ioc_init());

        let err = create_record(&ctx, "NEWREC").expect_err("C refuses dbCreateRecord once running");

        assert_eq!(
            err,
            "33554463 IOC already initialized - No new records can be added"
        );
        assert!(!exists(&db, &ctx, "NEWREC"));
    }

    #[test]
    fn test_help_registered() {
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let names = registry.list();
        assert!(names.contains(&"help"));
        assert!(names.contains(&"dbl"));
        assert!(names.contains(&"dbgf"));
        assert!(names.contains(&"dbpf"));
        assert!(names.contains(&"dbpr"));
        assert!(names.contains(&"dbLoadRecords"));
        assert!(names.contains(&"epicsEnvSet"));
        assert!(names.contains(&"exit"));
    }

    #[test]
    fn test_parse_macro_string() {
        let macros = parse_macro_string("P=IOC:,R=TEMP");
        assert_eq!(macros.get("P").unwrap(), "IOC:");
        assert_eq!(macros.get("R").unwrap(), "TEMP");

        let empty = parse_macro_string("");
        assert!(empty.is_empty());
    }

    /// C `macParseDefns` removes quotes and escapes from NAMES only —
    /// its own comment says values are left alone because, unlike names,
    /// "they will not be re-parsed" is false for them (`macUtil.c:198-200`):
    /// a value IS re-parsed, by `trans`, whose `discard` does the single
    /// removal. Both halves are asserted, because stripping here as well
    /// removes everything twice.
    #[test]
    fn parse_macro_string_keeps_values_raw_for_the_expander() {
        use crate::server::db_loader::{MacroExpandOptions, expand_macros};
        fn expand(m: &HashMap<String, String>, name: &str) -> String {
            expand_macros(&format!("$({name})"), m, MacroExpandOptions::default()).text
        }

        // Quoted comma stays inside the value (macParseDefns parity):
        // raw split would tear this into `DESC="a` + a stray `b"`.
        let m = parse_macro_string(r#"DESC="a,b",P=IOC:"#);
        assert_eq!(m.get("DESC").unwrap(), r#""a,b""#);
        assert_eq!(expand(&m, "DESC"), "a,b");
        assert_eq!(m.get("P").unwrap(), "IOC:");

        // Escaped comma is a literal; the backslash survives the parse
        // and the expansion drops it.
        let m = parse_macro_string(r#"DESC=a\,b,P=IOC:"#);
        assert_eq!(m.get("DESC").unwrap(), r#"a\,b"#);
        assert_eq!(expand(&m, "DESC"), "a,b");

        // Two backslashes: one removal, not two — C expands this to
        // `a\b`.
        let m = parse_macro_string(r#"B=a\\b"#);
        assert_eq!(m.get("B").unwrap(), r#"a\\b"#);
        assert_eq!(expand(&m, "B"), r#"a\b"#);

        // Whitespace around names and values is trimmed; a quoted NAME
        // is stripped in place, because a name is not re-parsed.
        let m = parse_macro_string(r#" P = IOC: , "R" = TEMP "#);
        assert_eq!(m.get("P").unwrap(), "IOC:");
        assert_eq!(m.get("R").unwrap(), "TEMP");

        // Quoted whitespace inside a value is preserved.
        let m = parse_macro_string(r#"MSG="a b c""#);
        assert_eq!(m.get("MSG").unwrap(), r#""a b c""#);
        assert_eq!(expand(&m, "MSG"), "a b c");

        // A name with no '=' is a deletion: nothing to remove from a
        // fresh map, and the surrounding assignments still parse.
        let m = parse_macro_string("A=1,DROP,B=2");
        assert_eq!(m.get("A").unwrap(), "1");
        assert_eq!(m.get("B").unwrap(), "2");
        assert!(!m.contains_key("DROP"));
    }

    /// R17-66: `dbLoadRecords` is a record-creation path, so the records it
    /// creates must come out of C's `iocInit` init passes in the same state as
    /// every other path — including the UDF tail of pass 1
    /// (`post_init_finalize_undef`), which this path used to skip.
    ///
    /// softIoc (EPICS 7.0.10, linux-x86_64), `dbLoadRecords` + `iocInit` then
    /// `dbgf X.UDF`:
    ///
    /// ```text
    /// record(histogram,"HG"){field(NELM,"8") field(SVL,"0")}  UDF 0
    /// record(mbboDirect,"MBD"){field(B0,"1") field(B2,"1")}   UDF 0, VAL 5
    /// record(mbboDirect,"MBD0"){}                             UDF 1
    /// ```
    ///
    /// (`clear_histogram` at histogramRecord.c:361 and the B0..B1F fold at
    /// mbboDirectRecord.c:142-158; a bare mbboDirect has neither, so it stays
    /// undefined.)
    #[test]
    fn r17_66_db_load_records_runs_the_post_init_udf_tail() {
        use std::io::Write;

        let (db, ctx) = make_ctx();

        let tmp = tempfile::Builder::new()
            .suffix(".db")
            .tempfile()
            .expect("tempfile");
        write!(
            tmp.as_file(),
            r#"
record(histogram, "HG")   {{ field(NELM, "8") field(SVL, "0") }}
record(mbboDirect, "MBD") {{ field(B0, "1") field(B2, "1") }}
record(mbboDirect, "MBD0") {{ }}
"#
        )
        .expect("write tempfile");

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbLoadRecords").unwrap();
        let args = parse_args(&[tmp.path().to_string_lossy().to_string()], &cmd.args).unwrap();
        assert!(matches!(
            cmd.handler.call(&args, &ctx),
            Ok(CommandOutcome::Continue)
        ));

        ctx.block_on(async {
            let udf = |name: &'static str| {
                let db = db.clone();
                async move { db.get_record(name).unwrap().read().common.udf != 0 }
            };
            assert!(
                !udf("HG").await,
                "histogram: `clear_histogram` clears UDF at init (softIoc: UDF 0)"
            );
            assert!(
                !udf("MBD").await,
                "mbboDirect: the B0..B1F fold clears UDF at init (softIoc: UDF 0)"
            );
            assert_eq!(
                db.get_record("MBD").unwrap().read().record.get_field("VAL"),
                Some(crate::types::EpicsValue::Long(5)),
                "B0|B2 folds into VAL=5"
            );
            assert!(
                udf("MBD0").await,
                "a bare mbboDirect has no DOL and no bits: C leaves it UDF=1"
            );
        });
    }

    // ---- dbLoadTemplate ----

    /// Write `body` to `dir/name` and return the path.
    fn write_file(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::io::Write;
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    /// Run one command and report stdout, stderr and the outcome. C's
    /// `dbLoadDatabase` says NOTHING when the load worked, so what is not
    /// printed is half the parity, and its diagnostics go to stderr where
    /// `capture` alone cannot see them.
    fn run_capturing(
        ctx: &CommandContext,
        name: &str,
        tokens: &[&str],
    ) -> (String, String, CommandResult) {
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get(name).unwrap();
        let tokens: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
        let args = parse_args(&tokens, &cmd.args).unwrap();
        let err_file = tempfile::NamedTempFile::new().unwrap();
        let err_path = err_file.path().to_path_buf();
        let mut result = Ok(CommandOutcome::Continue);
        let out = capture(ctx, || {
            ctx.with_error(std::fs::File::create(&err_path).unwrap(), || {
                result = cmd.handler.call(&args, ctx);
            });
        });
        (out, std::fs::read_to_string(&err_path).unwrap(), result)
    }

    /// The `bptTypeKdegC.dbd` this crate ships, the file C names in
    /// `dbLoadDatabase("$(EPICS_BASE)/dbd/bptTypeKdegC.dbd")`.
    const SHIPPED_BPT_TYPE_KDEGC: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/dbd/bptTypeKdegC.dbd");

    /// The census is MEASURED, not asserted: a name is present when
    /// `register_builtins` puts it in the registry the shell serves, whichever
    /// module registered it.
    #[test]
    fn the_database_command_census_matches_the_registry() {
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let present: std::collections::HashSet<&str> = registry.list().into_iter().collect();
        let measured: Vec<&str> = C_DATABASE_COMMANDS
            .iter()
            .copied()
            .filter(|name| !present.contains(name))
            .collect();
        assert_eq!(measured, ABSENT_DATABASE_COMMANDS);
    }

    /// The knob is a process global, so a case that sets it takes this
    /// lock for its whole body and puts it back — the same discipline
    /// `access_commands::as_state_test_guard` keeps for `as_state`.
    fn once_only_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `var dbRecordsOnceOnly 1` must reach the loader, not just echo: the
    /// name has to be in the variable table AND the value has to be what
    /// the install loop reads.
    #[test]
    fn the_once_only_var_is_registered_and_reaches_the_loader() {
        let _g = once_only_guard();
        let (_db, ctx) = make_ctx();
        // The table `var` lists and completes from is filled by
        // `register_builtins`, which `run_cmd` runs; read the knob back
        // through the command rather than trusting the registration call.
        assert_eq!(
            run_cmd(&ctx, "var", &["dbRecordsOnceOnly"]).trim(),
            "int dbRecordsOnceOnly = 0"
        );
        assert!(super::super::vars::variable_names().contains(&"dbRecordsOnceOnly"));
        run_cmd(&ctx, "var", &["dbRecordsOnceOnly", "1"]);
        assert!(db_records_once_only());
        // C tests it with a bare `if`, so a negative is set too.
        run_cmd(&ctx, "var", &["dbRecordsOnceOnly", "-2"]);
        assert!(db_records_once_only());
        run_cmd(&ctx, "var", &["dbRecordsOnceOnly", "0"]);
        assert!(!db_records_once_only());
    }

    /// Set, C refuses the SECOND block and drops its body whole; the
    /// record keeps the first block's fields and the load's status goes
    /// non-zero. Measured on `softIoc` R7.0.10-146 with this database:
    /// `D:ONE.DESC` stays `first`, `D:ONE.EGU` stays empty, and stderr
    /// carries the two-line diagnostic asserted here.
    #[test]
    fn once_only_refuses_a_second_declaration_and_drops_its_body() {
        let _g = once_only_guard();
        let (db, ctx) = make_ctx();
        run_cmd(&ctx, "var", &["dbRecordsOnceOnly", "1"]);
        let err = load_records(
            &ctx,
            r#"record(ai, "D:ONE") { field(DESC, "first") }
               record(ai, "D:ONE") { field(DESC, "second") field(EGU, "V") }"#,
        )
        .expect_err("the load reports the duplicate");
        run_cmd(&ctx, "var", &["dbRecordsOnceOnly", "0"]);
        assert_eq!(
            err,
            format!(
                "{ERL_ERROR}: Record 'D:ONE' already defined; dbRecordsOnceOnly is set,\n  \
                 so can't modify record."
            )
        );
        assert_eq!(db.get_pv("D:ONE.DESC").unwrap().to_string(), "first");
        assert_eq!(db.get_pv("D:ONE.EGU").unwrap().to_string(), "");
    }

    /// Clear — C's default — the same file merges, which is the whole
    /// point of the knob having two settings.
    #[test]
    fn the_default_merges_the_second_declaration() {
        let _g = once_only_guard();
        let (db, ctx) = make_ctx();
        run_cmd(&ctx, "var", &["dbRecordsOnceOnly", "0"]);
        load_records(
            &ctx,
            r#"record(ai, "D:ONE") { field(DESC, "first") }
               record(ai, "D:ONE") { field(DESC, "second") field(EGU, "V") }"#,
        )
        .expect("the default merges");
        assert_eq!(db.get_pv("D:ONE.DESC").unwrap().to_string(), "second");
        assert_eq!(db.get_pv("D:ONE.EGU").unwrap().to_string(), "V");
    }

    /// `record("*", …)` returns from C's `dbRecordHead` before
    /// `dbCreateRecord` (`dbLexRoutines.c:1136-1144`), so the flag never
    /// sees it — a modify block still modifies with the knob set.
    #[test]
    fn once_only_does_not_reach_a_star_modify_block() {
        let _g = once_only_guard();
        let (db, ctx) = make_ctx();
        run_cmd(&ctx, "var", &["dbRecordsOnceOnly", "0"]);
        load_records(&ctx, r#"record(ai, "D:ONE") { field(DESC, "first") }"#).expect("load");
        run_cmd(&ctx, "var", &["dbRecordsOnceOnly", "1"]);
        let outcome = load_records(&ctx, r#"record("*", "D:ONE") { field(DESC, "edited") }"#);
        run_cmd(&ctx, "var", &["dbRecordsOnceOnly", "0"]);
        outcome.expect("a modify block is exempt");
        assert_eq!(db.get_pv("D:ONE.DESC").unwrap().to_string(), "edited");
    }

    /// C `createAlias` (`dbLexRoutines.c:1459-1476`): an alias that already
    /// names this same record creates nothing and says nothing — only
    /// `dbRecordsOnceOnly` makes it an error. The port used to reject the
    /// repeat at C's own default, because every alias went to `add_alias`
    /// and its name-free check saw the first one.
    #[test]
    fn a_repeat_alias_is_silent_by_default_and_an_error_once_only() {
        let _g = once_only_guard();
        let (db, ctx) = make_ctx();
        run_cmd(&ctx, "var", &["dbRecordsOnceOnly", "0"]);
        load_records(
            &ctx,
            r#"record(ai, "D:TWO") { field(DESC, "t") }
               alias("D:TWO", "D:ALIAS")
               alias("D:TWO", "D:ALIAS")"#,
        )
        .expect("the default accepts the repeat");
        assert_eq!(db.get_pv("D:ALIAS.DESC").unwrap().to_string(), "t");

        run_cmd(&ctx, "var", &["dbRecordsOnceOnly", "1"]);
        let err = load_records(&ctx, r#"alias("D:TWO", "D:ALIAS")"#).expect_err("once-only");
        run_cmd(&ctx, "var", &["dbRecordsOnceOnly", "0"]);
        assert_eq!(
            err,
            format!("{ERL_ERROR}: Alias 'D:ALIAS' already defined; dbRecordsOnceOnly is set.")
        );
        // The alias the FIRST declaration made is untouched either way.
        assert_eq!(db.get_pv("D:ALIAS.DESC").unwrap().to_string(), "t");
    }

    /// An alias name already taken by something else is C's OTHER arm and
    /// stays an error at both settings (`dbLexRoutines.c:1461-1471`).
    #[test]
    fn an_alias_naming_a_different_record_is_still_rejected() {
        let _g = once_only_guard();
        let (_db, ctx) = make_ctx();
        run_cmd(&ctx, "var", &["dbRecordsOnceOnly", "0"]);
        load_records(
            &ctx,
            r#"record(ai, "D:ONE") { }
               record(ai, "D:TWO") { }
               alias("D:ONE", "D:ALIAS")
               alias("D:TWO", "D:ALIAS")"#,
        )
        .expect_err("one name cannot alias two records");
    }

    /// The CA server reads a field with `PvDatabase::get_pv`
    /// (`ca_server.rs:918`), so a `caget` of a link field must see C's
    /// rendering without the shell being involved at all — that is what makes
    /// this a property of the read funnel and not of `dbgf`.
    ///
    /// Measured on `softIoc` R7.0.10-146 over CA, `EPICS_CA_SERVER_PORT=5271`:
    /// `A:BARE.INP` → `L:B.VAL NPP NMS`, `A:TWO.FLNK` → `L:B` for a field
    /// written `L:B PP MS`, `A:CAO.OUT` → `OTHER:PV CA NMS`.
    #[test]
    fn a_ca_read_of_a_link_field_gets_cs_rendering() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            r#"record(calc, "L:B") { field(CALC, "1") }
               record(ai, "A:BARE") { field(INP, "L:B.VAL") }
               record(ao, "A:TWO") { field(OUT, "L:B.VAL PP MSS") field(FLNK, "L:B PP MS") }
               record(ao, "A:CAO") { field(OUT, "OTHER:PV CA") }"#,
        )
        .expect("load");
        ctx.block_on(db.ioc_init());
        for (pv, want) in [
            ("A:BARE.INP", "L:B.VAL NPP NMS"),
            ("A:TWO.OUT", "L:B.VAL PP MSS"),
            ("A:TWO.FLNK", "L:B"),
            ("A:CAO.OUT", "OTHER:PV CA NMS"),
            ("L:B.FLNK", ""),
        ] {
            assert_eq!(db.get_pv(pv).unwrap().to_string(), want, "{pv}");
        }
    }

    /// The read-back is the point: C's `dbpf` ends with `dbgf`, and `dbgf` is
    /// `dbGet` on a link field, which is `dbGetString`. Measured on `softIoc`
    /// R7.0.10-146 with the same two-record database — `dbpf C:ONE.INPA "L:B"`
    /// answers `L:B NPP NMS`, not the `L:B` it was handed.
    #[test]
    fn dbpf_on_a_link_field_reads_back_the_parsed_link() {
        let (db, ctx) = make_ctx();
        load_records(
            &ctx,
            r#"record(ai, "L:A") { field(VAL, "3.5") }
               record(ai, "L:B") { }
               record(calc, "C:ONE") { field(INPA, "L:A NPP NMS") field(CALC, "A+1") }"#,
        )
        .expect("load");
        ctx.block_on(db.ioc_init());

        let out = run_cmd(&ctx, "dbpf", &["C:ONE.INPA", "L:B"]);
        assert_eq!(out, "DBF_STRING:         \"L:B NPP NMS\"       \n");
        let out = run_cmd(&ctx, "dbgf", &["C:ONE.INPA"]);
        assert_eq!(out, "DBF_STRING:         \"L:B NPP NMS\"       \n");
        // A forward link is the other side of the mask and must stay bare.
        let out = run_cmd(&ctx, "dbpf", &["C:ONE.FLNK", "L:A"]);
        assert_eq!(out, "DBF_STRING:         \"L:A\"     \n");
    }

    /// Each name in [`PORT_ONLY_COMMANDS`] must actually be registered — a
    /// stale entry documents a command that no longer exists — and must not be
    /// a `dbIocRegister.c` name, which would mean it was mis-classified and the
    /// port is answering C's command under a private explanation.
    #[test]
    fn every_port_only_command_is_registered_and_is_not_a_c_name() {
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let present: std::collections::HashSet<&str> = registry.list().into_iter().collect();
        for (name, _) in PORT_ONLY_COMMANDS {
            assert!(
                present.contains(name),
                "{name} is documented port-only but not registered"
            );
            assert!(
                !C_DATABASE_COMMANDS.contains(name),
                "{name} IS a dbIocRegister.c name, so it is not port-only"
            );
        }
    }

    /// C `dbIocRegister.c:600`.
    #[test]
    fn db_load_database_is_registered() {
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        assert!(registry.list().contains(&"dbLoadDatabase"));
    }

    /// C `dbAccess.c:788-791` prints the usage line and returns -1, which
    /// `dbLoadDatabaseCallFunc` hands to `iocshSetError` — so unlike every
    /// other usage line in this file the shell records the line as failed.
    #[test]
    fn db_load_database_without_a_file_prints_usage_and_fails() {
        let (_db, ctx) = make_ctx();
        let (out, err, result) = run_capturing(&ctx, "dbLoadDatabase", &[]);
        assert_eq!(out, "Usage: dbLoadDatabase \"file\", \"path\", \"subs\"\n");
        assert_eq!(err, "");
        assert!(matches!(result, Ok(CommandOutcome::Failed)));
    }

    /// Measured on C `softIoc` R7.0.10-146 with the `.dbd` below reachable
    /// only through the command's second argument:
    ///
    /// ```text
    /// epics> dbLoadDatabase("tcurve.dbd", "sub")
    /// epics> dbLoadRecords("t8.db")
    /// epics> iocInit()
    /// epics> dbgf("R:FROMDBD.DESC")
    /// DBF_STRING:         "declared in a .dbd"
    /// epics> dbpf("R:LIN.RVAL", "150")
    /// epics> dbtr("R:LIN")
    /// ... LINR: typeTdegC ... RVAL: 150 ... STAT: NO_ALARM ... VAL : 20
    /// epics> dbpf("R:NOLIN.RVAL", "150")
    /// epics> dbtr("R:NOLIN")
    /// ... LINR: typeSdegC ... RVAL: 150 ... STAT: SOFT ... VAL : 150
    /// ```
    ///
    /// Three C facts in one case: the second argument IS the search path,
    /// a `record(...)` inside a `.dbd` installs like any other, and a
    /// `breaktable(...)` it declares is what a later `LINR` resolves
    /// against — the arm `cvt_bpt.rs` documented as unreachable while the
    /// command was missing. A load that worked says nothing at all.
    #[test]
    fn db_load_database_installs_records_and_breaktables_from_a_dbd() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "tcurve.dbd",
            "breaktable(typeTdegC) {\n    0.0   0.0\n    100.0 10.0\n    200.0 30.0\n}\n\
             record(ai, \"R:FROMDBD\") {\n    field(DESC, \"declared in a .dbd\")\n}\n",
        );

        let (out, err, result) = run_capturing(
            &ctx,
            "dbLoadDatabase",
            &["tcurve.dbd", &dir.path().display().to_string()],
        );
        assert_eq!(out, "", "C prints nothing when the load worked");
        assert_eq!(err, "");
        assert!(matches!(result, Ok(CommandOutcome::Continue)));

        assert!(
            exists(&db, &ctx, "R:FROMDBD"),
            "a .dbd record(...) installs"
        );
        let desc = ctx.block_on(async {
            let rec = db.get_record("R:FROMDBD").unwrap();
            let r = rec.read();
            r.common.desc.to_string()
        });
        assert_eq!(desc, "declared in a .dbd");

        load_records(
            &ctx,
            r#"
record(ai, "R:LIN") {
    field(DTYP, "Raw Soft Channel")
    field(LINR, "typeTdegC")
    field(PREC, "3")
    field(INP,  "0")
}
record(ai, "R:NOLIN") {
    field(DTYP, "Raw Soft Channel")
    field(LINR, "typeSdegC")
    field(PREC, "3")
    field(INP,  "0")
}
"#,
        )
        .expect("the .db must load");
        ctx.block_on(async { db.ioc_init().await });

        run_cmd(&ctx, "dbpf", &["R:LIN.RVAL", "150"]);
        assert_eq!(ai_val(&db, &ctx, "R:LIN"), 20.0, "typeTdegC converts 150");

        run_cmd(&ctx, "dbpf", &["R:NOLIN.RVAL", "150"]);
        assert_eq!(
            ai_val(&db, &ctx, "R:NOLIN"),
            150.0,
            "typeSdegC names no loaded table, so the raw value stands"
        );
    }

    /// C `dbReadCOM` (`dbLexRoutines.c:281-290`) prints this itself, so both
    /// commands say it and neither says it twice; only `dbLoadRecords` adds
    /// a summary line of its own (`dbAccess.c:807-808`).
    ///
    /// ```text
    /// epics> dbLoadDatabase("nosuch.dbd")
    /// ERROR: Can't open file 'nosuch.dbd'
    /// ```
    #[test]
    fn a_file_that_cannot_be_opened_reports_through_the_shared_read() {
        let (_db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().display().to_string();

        let (out, err, result) = run_capturing(&ctx, "dbLoadDatabase", &["nosuch.dbd", &empty]);
        assert_eq!(out, "");
        assert_eq!(err, format!("{ERL_ERROR}: Can't open file 'nosuch.dbd'\n"));
        assert!(matches!(result, Ok(CommandOutcome::Failed)));

        // Both of C's lines, in C's order and on C's stream: the read's
        // own diagnostic, then the summary `dbLoadRecords` adds
        // (`dbAccess.c:808`, measured — C writes it on this arm too).
        let missing = dir.path().join("nosuch.db").display().to_string();
        let (out, err, result) = run_capturing(&ctx, "dbLoadRecords", &[&missing]);
        assert_eq!(out, "");
        assert_eq!(
            err,
            format!(
                "{ERL_ERROR}: Can't open file '{missing}'\n\
                 {ERL_ERROR}: Failed to load '{missing}'\n"
            )
        );
        assert!(matches!(result, Ok(CommandOutcome::Failed)));
    }

    /// C words a refused menu field the same way wherever the field lives.
    ///
    /// Through the LOAD, not through `menu_value_refusal` directly: the
    /// helper's wording was already right and asserted
    /// (`db_loader::tests::the_refusal_is_byte_exact_against_the_reference_ioc`),
    /// but only the dbCommon half reached it. A record-OWN menu field
    /// (`sel.SELM`, `ai.LINR`) was refused by `apply_fields` instead and
    /// reported the port's own `illegal menu choice: …`. Asserting on the
    /// path the operator actually uses is what tells the two apart.
    #[test]
    fn a_refused_menu_field_is_worded_like_c_wherever_the_field_lives() {
        let (db, ctx) = make_ctx();

        let err = load_records(&ctx, r#"record(sel, "S1") { field(SELM, "Bogus") }"#)
            .expect_err("C refuses selSELM (measured on softIoc @R7.0.10)");
        assert_eq!(
            err,
            format!(
                "{ERL_ERROR}: Can't set 'S1.SELM' to 'Bogus' using menu selSELM : Illegal choice"
            )
        );
        // C `dbCreateRecord` ran BEFORE `dbPutString`, so the record exists
        // with SELM at its default — `dbl` lists S1.
        assert!(exists(&db, &ctx, "S1"), "C keeps the record");

        let err = load_records(&ctx, r#"record(ai, "L1") { field(LINR, "NoSuchTable") }"#)
            .expect_err("C refuses menuConvert");
        assert_eq!(
            err,
            format!(
                "{ERL_ERROR}: Can't set 'L1.LINR' to 'NoSuchTable' using menu menuConvert : Illegal choice"
            )
        );
        assert!(exists(&db, &ctx, "L1"));
    }

    /// One refused field costs that field its value and nothing else.
    ///
    /// C reports EVERY bad field and reads the whole file (`yyerror` at
    /// `dbLexRoutines.c:1415`, not `yyerrorAbort`), so `dbl` on this database
    /// lists R1 through R4 — measured. The port classified the refusal as
    /// fatal, so it stopped at R2, never reported R3, and listed R1 alone.
    #[test]
    fn one_refused_field_does_not_discard_the_records_after_it() {
        let (db, ctx) = make_ctx();
        let err = load_records(
            &ctx,
            r#"
record(ai, "R1") { field(SCAN, "Passive") }
record(ai, "R2") { field(SCAN, "Passiv") }
record(ai, "R3") { field(PINI, "YESS") }
record(ai, "R4") { field(SCAN, "1 second") }
"#,
        )
        .expect_err("the load's status goes non-zero at the END");
        assert!(err.contains("R2.SCAN"), "first refusal reported; got {err}");
        assert!(
            err.contains("(+1 more)"),
            "R3's was reported too; got {err}"
        );
        for name in ["R1", "R2", "R3", "R4"] {
            assert!(exists(&db, &ctx, name), "C's dbl lists {name}");
        }
    }

    /// A load that succeeds says NOTHING, on either stream.
    ///
    /// C `dbLoadRecords` runs `dbLoadRecordsHook` — NULL throughout base —
    /// and returns 0 (`dbAccess.c:804-806`); C `dbLoadTemplate` returns
    /// `yyparse`'s status, its progress prints being `#ifdef ERROR_STUFF`
    /// (`dbLoadTemplate.y:92-94`). The port printed `Loaded N record(s)
    /// from <file>` from both, which is a line no C IOC emits and which
    /// lands on the same stdout a startup script's own output uses.
    #[test]
    fn a_successful_load_prints_nothing() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let _path = DbIncludePath::set(dir.path());
        write_file(dir.path(), "quiet.db", "record(ai, \"Q1\") { }\n");

        let (out, err, result) = run_capturing(&ctx, "dbLoadRecords", &["quiet.db"]);
        assert_eq!(out, "", "C prints nothing on the success path");
        assert_eq!(err, "");
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
        assert!(exists(&db, &ctx, "Q1"), "the record still loaded");
    }

    /// The `.substitutions` twin of [`a_successful_load_prints_nothing`]:
    /// each row IS a `dbLoadRecords`, so silence must be the same.
    #[test]
    fn a_successful_template_load_prints_nothing() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let _path = DbIncludePath::set(dir.path());
        write_file(dir.path(), "row.db", "record(ai, \"$(N)\") { }\n");
        write_file(
            dir.path(),
            "quiet.substitutions",
            "file row.db { pattern { N }\n{ \"T1\" }\n{ \"T2\" }\n}\n",
        );

        let (out, err, result) = run_capturing(&ctx, "dbLoadTemplate", &["quiet.substitutions"]);
        assert_eq!(out, "");
        assert_eq!(err, "");
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
        assert!(exists(&db, &ctx, "T1") && exists(&db, &ctx, "T2"));
    }

    /// A rejected load is reported ONCE.
    ///
    /// The read writes its own diagnostic (C `dbLexRoutines` writes those
    /// with a bare `fprintf(stderr, ...)`, so they are not on the shell's
    /// redirectable diagnostic stream) and the command adds C's summary and
    /// nothing else. The captured stream is therefore exactly the summary:
    /// if the command ever hands the diagnostic back as an `Err(String)`
    /// again, a second copy of it lands here and this assertion fails.
    #[test]
    fn a_rejected_load_is_reported_once() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let _path = DbIncludePath::set(dir.path());
        write_file(
            dir.path(),
            "once.db",
            "alias(\"NOPE\", \"BAD\")\nrecord(ai, \"KEPT\") { }\n",
        );

        let (out, err, result) = run_capturing(&ctx, "dbLoadRecords", &["once.db"]);
        assert_eq!(out, "");
        assert_eq!(err, format!("{ERL_ERROR}: Failed to load 'once.db'\n"));
        assert!(matches!(result, Ok(CommandOutcome::Failed)));
        assert!(exists(&db, &ctx, "KEPT"), "the record that parsed stays");
    }

    /// C `dbReadCOM` refuses with -2 once the IOC has left `iocVoid`
    /// (`dbLexRoutines.c:236-239`) and says nothing about it, so the whole
    /// difference is what each command adds: `dbLoadDatabase` nothing,
    /// `dbLoadRecords` its two lines (`dbAccess.c:807-810`).
    #[test]
    fn a_load_after_ioc_init_is_silent_for_db_load_database_only() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let dbd = write_file(dir.path(), "late.dbd", "record(ai, \"R:LATE\") { }\n")
            .display()
            .to_string();
        let dbf = write_file(dir.path(), "late.db", "record(ai, \"R:LATE2\") { }\n")
            .display()
            .to_string();
        ctx.block_on(async { db.ioc_init().await });

        let (out, err, result) = run_capturing(&ctx, "dbLoadDatabase", &[&dbd]);
        assert_eq!(out, "");
        assert_eq!(err, "");
        assert!(matches!(result, Ok(CommandOutcome::Failed)));
        assert!(!exists(&db, &ctx, "R:LATE"), "nothing is created");

        let (out, err, result) = run_capturing(&ctx, "dbLoadRecords", &[&dbf]);
        assert_eq!(out, "");
        assert_eq!(
            err,
            format!(
                "{ERL_ERROR}: Failed to load '{dbf}'\n    \
                 Records cannot be loaded after iocInit!\n"
            )
        );
        assert!(matches!(result, Ok(CommandOutcome::Failed)));
        assert!(!exists(&db, &ctx, "R:LATE2"));
    }

    /// Put `dir` on `EPICS_DB_INCLUDE_PATH` for the life of the guard.
    /// C `dbLoadRecords` resolves a `.db` (and every template a
    /// `.substitutions` names) through `dbOpenFile`, which searches
    /// only that list.
    struct DbIncludePath(Option<std::ffi::OsString>);

    impl DbIncludePath {
        fn set(dir: &std::path::Path) -> Self {
            let prev = std::env::var_os("EPICS_DB_INCLUDE_PATH");
            // SAFETY: serial(epics_env) serialises env-mutating tests.
            unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir) };
            Self(prev)
        }
    }

    impl Drop for DbIncludePath {
        fn drop(&mut self) {
            // SAFETY: as above.
            unsafe {
                match self.0.take() {
                    Some(v) => std::env::set_var("EPICS_DB_INCLUDE_PATH", v),
                    None => std::env::remove_var("EPICS_DB_INCLUDE_PATH"),
                }
            }
        }
    }

    /// Drive the real `dbLoadTemplate` command over `sub_file`, optionally
    /// with a `globalMacros` argument.
    fn load_template(
        ctx: &CommandContext,
        sub_file: &std::path::Path,
        global_macros: Option<&str>,
    ) -> Result<(), String> {
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbLoadTemplate").unwrap();
        let mut tokens = vec![sub_file.to_string_lossy().to_string()];
        if let Some(g) = global_macros {
            tokens.push(g.to_string());
        }
        let args = parse_args(&tokens, &cmd.args).unwrap();
        // `dbLoadTemplate` writes C's summary itself and answers `Failed`,
        // so the diagnostic is on the stream, not in the return value.
        let err_file = tempfile::NamedTempFile::new().unwrap();
        let err_path = err_file.path().to_path_buf();
        let mut result = Ok(CommandOutcome::Continue);
        ctx.with_error(std::fs::File::create(&err_path).unwrap(), || {
            result = cmd.handler.call(&args, ctx);
        });
        match result {
            Ok(CommandOutcome::Failed) => Err(std::fs::read_to_string(&err_path).unwrap()),
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Read a record's `VAL` as f64 (ai stores it as `Double`).
    fn ai_val(db: &PvDatabase, _ctx: &CommandContext, name: &str) -> f64 {
        let rec = db
            .get_record(name)
            .unwrap_or_else(|| panic!("record '{name}' does not exist"));
        let r = rec.read();
        match r.record.get_field("VAL") {
            Some(EpicsValue::Double(d)) => d,
            other => panic!("unexpected VAL for '{name}': {other:?}"),
        }
    }

    /// A `.substitutions` fixture with per-row macros expands to one record
    /// per row, each with its own macros substituted into name and fields.
    #[test]
    #[serial_test::serial(epics_env)]
    fn db_load_template_expands_rows_with_per_row_macros() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let _path = DbIncludePath::set(dir.path());
        write_file(
            dir.path(),
            "a.db",
            r#"record(ai, "$(P)AI$(N)") { field(VAL, "$(V)") }"#,
        );
        let subs = write_file(
            dir.path(),
            "a.substitutions",
            r#"
file "a.db" {
    pattern { P, N, V }
        { "IOC:", "1", "1.5" }
        { "IOC:", "2", "2.5" }
}
"#,
        );

        load_template(&ctx, &subs, None).expect("template load");

        assert!(exists(&db, &ctx, "IOC:AI1"), "row 1 expands to IOC:AI1");
        assert!(exists(&db, &ctx, "IOC:AI2"), "row 2 expands to IOC:AI2");
        assert!(!exists(&db, &ctx, "IOC:AI3"), "only two rows");
        assert_eq!(ai_val(&db, &ctx, "IOC:AI1"), 1.5, "row 1 V substituted");
        assert_eq!(ai_val(&db, &ctx, "IOC:AI2"), 2.5, "row 2 V substituted");
    }

    /// R4-7: C `dbLoadTemplate` calls `dbLoadRecords` from the
    /// `pattern_definition` action (`dbLoadTemplate.y:186`), so row 1's
    /// records are in `pdbbase` before row 2 is read and only the rows
    /// after the failure are lost. The port concatenated every row's
    /// records and installed the batch, so one unloadable row threw away
    /// the rows that had already succeeded.
    #[test]
    #[serial_test::serial(epics_env)]
    fn db_load_template_keeps_the_rows_that_already_loaded() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let _path = DbIncludePath::set(dir.path());
        write_file(dir.path(), "ok.db", r#"record(ai, "ROW:$(N)") { }"#);
        let subs = write_file(
            dir.path(),
            "partial.substitutions",
            r#"
file "ok.db" {
    { N=1 }
}
file "gone.db" {
    { N=2 }
}
file "ok.db" {
    { N=3 }
}
"#,
        );

        load_template(&ctx, &subs, None).expect_err("the missing template must fail the call");

        assert!(
            exists(&db, &ctx, "ROW:1"),
            "the row before the failure stays"
        );
        assert!(
            !exists(&db, &ctx, "ROW:3"),
            "the rows after it are not loaded"
        );
    }

    /// `globalMacros` applies to every row; a row-level macro of the same
    /// name overrides the global. Grounded in the reused loader:
    /// `substitution_rows` (substitution.rs) inserts the caller macros
    /// (the command's `globalMacros`) first, then each row's macros into a
    /// last-definition-wins map, so the row wins — the C `dbLoadTemplate`
    /// precedence (see the loader-level regression
    /// `substitution_rows_caller_macros_overridden_by_row`).
    #[test]
    #[serial_test::serial(epics_env)]
    fn db_load_template_global_macros_apply_and_row_overrides() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let _path = DbIncludePath::set(dir.path());
        write_file(
            dir.path(),
            "b.db",
            r#"record(ai, "$(P)AI$(N)") { field(VAL, "$(V)") }"#,
        );
        // Row 1 overrides the global V; row 2 omits V and inherits the global.
        let subs = write_file(
            dir.path(),
            "b.substitutions",
            r#"
file "b.db" {
    { N=1, V=1 }
    { N=2 }
}
"#,
        );

        load_template(&ctx, &subs, Some("V=9,P=IOC:")).expect("template load");

        // P applied to both rows (global reaches every row).
        assert!(exists(&db, &ctx, "IOC:AI1"), "global P reaches row 1");
        assert!(exists(&db, &ctx, "IOC:AI2"), "global P reaches row 2");
        assert_eq!(
            ai_val(&db, &ctx, "IOC:AI1"),
            1.0,
            "row-level V=1 overrides the global V=9"
        );
        assert_eq!(
            ai_val(&db, &ctx, "IOC:AI2"),
            9.0,
            "row 2 has no V, so the global V=9 applies"
        );
    }

    /// Parity: records loaded by `dbLoadTemplate` are identical to the ones
    /// produced by the equivalent hand-written `dbLoadRecords` calls — both
    /// commands install through `install_record_defs`, and the substitutions
    /// loader parses each template through the same `parse_db_file` as
    /// `dbLoadRecords`.
    #[test]
    #[serial_test::serial(epics_env)]
    fn db_load_template_parity_with_hand_written_db_load_records() {
        let dir = tempfile::tempdir().unwrap();
        let _path = DbIncludePath::set(dir.path());
        write_file(
            dir.path(),
            "p.db",
            r#"record(ai, "$(P)AI$(N)") { field(VAL, "$(V)") field(EGU, "$(EGU)") }"#,
        );
        let subs = write_file(
            dir.path(),
            "p.substitutions",
            r#"
file "p.db" {
    pattern { P, N, V, EGU }
        { "IOC:", "1", "1.5", "volts" }
        { "IOC:", "2", "2.5", "amps" }
}
"#,
        );

        // Template path.
        let (db_t, ctx_t) = make_ctx();
        load_template(&ctx_t, &subs, None).expect("template load");

        // Hand-expanded dbLoadRecords path — the exact rows the template yields.
        let (db_r, ctx_r) = make_ctx();
        load_records(
            &ctx_r,
            r#"
record(ai, "IOC:AI1") { field(VAL, "1.5") field(EGU, "volts") }
record(ai, "IOC:AI2") { field(VAL, "2.5") field(EGU, "amps") }
"#,
        )
        .expect("hand-written load");

        // Every field-visible property must match record-for-record.
        for name in ["IOC:AI1", "IOC:AI2"] {
            {
                let rt = db_t.get_record(name).expect("template record");
                let rr = db_r.get_record(name).expect("hand record");
                let rt = rt.read();
                let rr = rr.read();
                assert_eq!(
                    rt.record.record_type(),
                    rr.record.record_type(),
                    "{name}: record type parity"
                );
                assert_eq!(
                    rt.record.get_field("VAL"),
                    rr.record.get_field("VAL"),
                    "{name}: VAL parity"
                );
                assert_eq!(
                    rt.record.get_field("EGU"),
                    rr.record.get_field("EGU"),
                    "{name}: EGU parity"
                );
                assert_eq!(rt.common.udf, rr.common.udf, "{name}: UDF parity");
            }
        }
    }

    /// A missing `.substitutions` file returns an error, not a panic.
    #[test]
    fn db_load_template_file_not_found_errors() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.substitutions");

        let err = load_template(&ctx, &missing, None)
            .expect_err("a nonexistent substitutions file must error");
        assert!(
            err.contains("parse error"),
            "expected a parse error, got: {err}"
        );
        assert!(!exists(&db, &ctx, "IOC:AI1"), "nothing is created on error");
    }

    /// A malformed `.substitutions` file returns an error, not a panic.
    #[test]
    fn db_load_template_malformed_substitutions_errors() {
        let (_db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        // `{ A=1 }` with no enclosing `file "..." { }` — the parser rejects a
        // row block that is missing the `file` keyword.
        let subs = write_file(dir.path(), "bad.substitutions", "{ A=1 }\n");

        let err = load_template(&ctx, &subs, None)
            .expect_err("a malformed substitutions file must error");
        assert!(
            err.contains("parse error"),
            "expected a parse error, got: {err}"
        );
    }

    /// The command is registered under its EPICS-base name.
    #[test]
    fn db_load_template_is_registered() {
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        assert!(registry.list().contains(&"dbLoadTemplate"));
    }

    /// Boundary: `dbLoadTemplate` is refused after `iocInit` with C's
    /// diagnostic and creates nothing — the same load-phase gate as
    /// `dbLoadRecords`.
    #[test]
    fn db_load_template_after_ioc_init_is_refused_and_creates_nothing() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "t.db",
            r#"record(ai, "$(N)") { field(VAL, "1") }"#,
        );
        let subs = write_file(
            dir.path(),
            "t.substitutions",
            r#"file "t.db" { { N=LATER } }"#,
        );

        ctx.block_on(db.ioc_init());

        let err =
            load_template(&ctx, &subs, None).expect_err("C refuses a load once the IOC is running");
        assert!(
            err.contains("Records cannot be loaded after iocInit!"),
            "expected C's diagnostic; got {err}"
        );
        assert!(!exists(&db, &ctx, "LATER"), "no record may be created");
    }

    /// Two records of different types, one alias each, and the columns C
    /// prints them in. Measured on `softIoc` R7.0.10-146 with the same
    /// database: `dbnr` emits `Records  Aliases  Record Type`, then
    /// ` %5d    %5d    %s` per type that HAS records, then
    /// `Total %d records, %d aliases` (`dbTest.c:224-237`). The two aliases
    /// count in the total even though C reaches them by subtraction and the
    /// port by a second count.
    #[test]
    fn dbnr_prints_c_s_columns_and_counts_aliases_in_the_total() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("A:one", Box::new(AiRecord::new(1.0)))
                .await
                .unwrap();
            db.add_record(
                "B:one",
                Box::new(crate::server::records::bi::BiRecord::new(0)),
            )
            .await
            .unwrap();
            db.add_alias("A:alias1", "A:one").await.unwrap();
            db.add_alias("B:aliasX", "B:one").await.unwrap();
        });

        assert_eq!(
            run_cmd(&ctx, "dbnr", &[]),
            "Records  Aliases  Record Type
     1        1    ai
     1        1    bi
Total 2 records, 2 aliases
"
        );

        // `verbose` keeps the types with no records; C's list is the loaded
        // `.dbd`'s, the port's is `RECORD_TYPES`, so only the presence of a
        // zero row is asserted here, not the whole table.
        let verbose = run_cmd(&ctx, "dbnr", &["1"]);
        assert!(
            verbose.contains("     0        0    calc\n"),
            "verbose must list an uninstanced type — got:\n{verbose}"
        );
        assert!(
            verbose.ends_with("Total 2 records, 2 aliases\n"),
            "the totals do not change with verbose — got:\n{verbose}"
        );
    }

    /// C `dbla` prints `<alias> -> <target NAME>` and globs the ALIAS name,
    /// not the record's (`dbTest.c:262-266`). Measured on `softIoc`
    /// R7.0.10-146: `dbla "A:*"` returned only `A:alias1 -> A:one`.
    #[test]
    fn dbla_prints_alias_to_target_and_globs_the_alias_name() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("A:one", Box::new(AiRecord::new(1.0)))
                .await
                .unwrap();
            db.add_record(
                "B:one",
                Box::new(crate::server::records::bi::BiRecord::new(0)),
            )
            .await
            .unwrap();
            db.add_alias("A:alias1", "A:one").await.unwrap();
            db.add_alias("B:aliasX", "B:one").await.unwrap();
        });

        assert_eq!(
            run_cmd(&ctx, "dbla", &[]),
            "A:alias1 -> A:one\nB:aliasX -> B:one\n"
        );
        assert_eq!(run_cmd(&ctx, "dbla", &["A:*"]), "A:alias1 -> A:one\n");
        // The record names both start with the same letters as the aliases
        // they carry, so a pattern that matches only a RECORD name must
        // print nothing.
        assert_eq!(run_cmd(&ctx, "dbla", &["*:one"]), "");
    }

    /// The alias half of the node walk, against C's own bytes.
    ///
    /// Both spellings of an alias are here — a record-body `alias("...")` on
    /// `A:ONE` and a top-level `alias("A:THREE","A:THREE:ALT")` — because the
    /// two reach the database by different routes and only the list they land
    /// in makes them the same thing.
    ///
    /// MEASURED on `softIoc` 7.0.10.1-DEV
    /// (`/home/stevek/work/epics-base/bin/linux-x86_64/softIoc`) over this
    /// exact `.db`:
    ///
    /// ```text
    /// dbl            -> A:ONE  A:ONE:ALT  A:THREE  A:THREE:ALT  B:TWO
    /// dbl "ai"       -> A:ONE  A:ONE:ALT  A:THREE  A:THREE:ALT
    /// dbla           -> A:ONE:ALT -> A:ONE     A:THREE:ALT -> A:THREE
    /// dbgrep "*ALT"  -> A:ONE:ALT  A:THREE:ALT
    /// dbnr           -> ai 2 records 2 aliases; bo 1 record 0 aliases
    /// ```
    ///
    /// The alias sits where it was DECLARED, not after the records: C numbers
    /// the alias node from the same counter as the records
    /// (`dbStaticLib.c:1704`), so `A:ONE:ALT` precedes `A:THREE`.
    #[test]
    fn dbl_lists_alias_nodes_where_they_were_declared() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alias.db");
        std::fs::write(
            &path,
            "record(ai, \"A:ONE\") { field(VAL, \"1\") alias(\"A:ONE:ALT\") }\n\
             record(bo, \"B:TWO\") { }\n\
             record(ai, \"A:THREE\") { field(VAL, \"3\") }\n\
             alias(\"A:THREE\", \"A:THREE:ALT\")\n",
        )
        .unwrap();
        run_cmd(&ctx, "dbLoadRecords", &[path.to_str().unwrap()]);
        ctx.block_on(db.ioc_init());

        assert_eq!(
            run_cmd(&ctx, "dbl", &[]),
            "A:ONE\nA:ONE:ALT\nA:THREE\nA:THREE:ALT\nB:TWO\n",
            "C lists the alias nodes; a site building its PV inventory with \
             `dbl > pvlist` needs the names its clients use"
        );
        assert_eq!(
            run_cmd(&ctx, "dbl", &["ai"]),
            "A:ONE\nA:ONE:ALT\nA:THREE\nA:THREE:ALT\n",
            "an alias belongs to the type list of the record it names"
        );
        assert_eq!(
            run_cmd(&ctx, "dbla", &[]),
            "A:ONE:ALT -> A:ONE\nA:THREE:ALT -> A:THREE\n"
        );
        assert_eq!(
            run_cmd(&ctx, "dbgrep", &["*ALT"]),
            "A:ONE:ALT\nA:THREE:ALT\n"
        );
        assert_eq!(
            run_cmd(&ctx, "dbnr", &[]),
            "Records  Aliases  Record Type
     2        2    ai
     1        0    bo
Total 3 records, 2 aliases
"
        );
    }

    /// C `dbli` globs the INFO TAG NAME (`dbStaticLib.c:2936` tests
    /// `dbGetInfoName`), and an empty pattern lists every tag (`:2935`).
    /// Measured on `softIoc` R7.0.10-146: `dbli "auto*"` returned only the
    /// `autosaveFields` line.
    #[test]
    fn dbli_globs_the_info_tag_name_not_the_record_name() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("A:one", Box::new(AiRecord::new(1.0)))
                .await
                .unwrap();
        });
        {
            let rec = db.get_record("A:one").unwrap();
            let mut inst = rec.write();
            inst.set_info("autosaveFields", "VAL");
            inst.set_info("zzz", "second");
        }

        assert_eq!(
            run_cmd(&ctx, "dbli", &[]),
            "A:one info(autosaveFields, \"VAL\")\nA:one info(zzz, \"second\")\n"
        );
        assert_eq!(
            run_cmd(&ctx, "dbli", &["auto*"]),
            "A:one info(autosaveFields, \"VAL\")\n"
        );
        // The record name matches this glob; the tag names do not.
        assert_eq!(run_cmd(&ctx, "dbli", &["A:*"]), "");
    }

    /// C reports every `dbCreateAlias` failure as `ERROR: <status>
    /// <errSymMsg>` (`dbStaticIocRegister.c:257-260`). Measured on `softIoc`
    /// R7.0.10-146: a duplicate alias gives `33554435 Record Already exists`,
    /// and both an unknown target and a missing argument give
    /// `33554437 Record Not Found`.
    #[test]
    fn db_create_alias_carries_c_s_status_numbers() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("A:one", Box::new(AiRecord::new(1.0)))
                .await
                .unwrap();
        });
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbCreateAlias").unwrap();
        let call = |tokens: &[&str]| {
            let tokens: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
            let args = parse_args(&tokens, &cmd.args).expect("C accepts a missing argument");
            cmd.handler.call(&args, &ctx).map(|_| ())
        };

        assert!(call(&["pdbbase", "A:one", "A:alias2"]).is_ok());
        assert_eq!(db.resolve_alias("A:alias2").as_deref(), Some("A:one"));
        // C does not gate this on iocState, so an alias of an alias resolves
        // to the real record (`dbStaticLib.c:1675-1677`).
        assert!(call(&["pdbbase", "A:alias2", "A:alias3"]).is_ok());
        assert_eq!(db.resolve_alias("A:alias3").as_deref(), Some("A:one"));

        assert_eq!(
            call(&["pdbbase", "A:one", "A:alias2"]).unwrap_err(),
            "33554435 Record Already exists"
        );
        assert_eq!(
            call(&["pdbbase", "NOSUCH", "A:alias9"]).unwrap_err(),
            "33554437 Record Not Found"
        );
        assert_eq!(call(&["pdbbase"]).unwrap_err(), "33554437 Record Not Found");
        assert_eq!(
            call(&["ai", "A:one", "A:alias9"]).unwrap_err(),
            "Expecting 'pdbbase' got 'ai'."
        );
    }

    /// C `dbStateShow` prints the `id <ptr> '<name>' : ` prefix only from
    /// level 1 (`dbState.c:101-102`), while `dbStateShowAll` passes
    /// `level+1` (`:113`) so the prefix always shows. Measured on `softIoc`
    /// R7.0.10-146: `dbStateShow S1` printed `TRUE`, `dbStateShow S1 1`
    /// printed `id 0x574de7ce38f0 'S1' : TRUE`, and `dbStateShowAll` with no
    /// argument printed the prefixed form for both states in creation order.
    #[test]
    fn db_state_commands_follow_c_s_level_and_creation_order() {
        let (_db, ctx) = make_ctx();
        // The state registry is process-wide, as C's `states` ELLLIST is, so
        // these names must not collide with another test's.
        let first = "IOCSH_TEST_STATE_1";
        let second = "IOCSH_TEST_STATE_2";

        assert_eq!(run_cmd(&ctx, "dbStateCreate", &[first]), "");
        assert_eq!(run_cmd(&ctx, "dbStateCreate", &[second]), "");
        assert_eq!(run_cmd(&ctx, "dbStateShow", &[first]), "FALSE\n");
        assert_eq!(run_cmd(&ctx, "dbStateSet", &[first]), "");
        assert_eq!(run_cmd(&ctx, "dbStateShow", &[first]), "TRUE\n");
        assert_eq!(run_cmd(&ctx, "dbStateClear", &[first]), "");
        assert_eq!(run_cmd(&ctx, "dbStateShow", &[first]), "FALSE\n");

        run_cmd(&ctx, "dbStateSet", &[second]);
        let level_one = run_cmd(&ctx, "dbStateShow", &[second, "1"]);
        assert!(
            level_one.starts_with("id 0x") && level_one.ends_with(&format!("'{second}' : TRUE\n")),
            "level 1 must carry C's id prefix — got:\n{level_one}"
        );

        // `dbStateShowAll` prefixes at every level, and reports creation
        // order. Other tests share the registry, so only the two names
        // created here are asserted, and only relative to each other.
        for argv in [vec![], vec!["1"]] {
            let all = run_cmd(&ctx, "dbStateShowAll", &argv);
            let at_first = all
                .find(&format!("'{first}' : FALSE"))
                .unwrap_or_else(|| panic!("{first} missing from:\n{all}"));
            let at_second = all
                .find(&format!("'{second}' : TRUE"))
                .unwrap_or_else(|| panic!("{second} missing from:\n{all}"));
            assert!(at_first < at_second, "creation order lost:\n{all}");
            assert!(all.starts_with("id 0x"), "prefix missing:\n{all}");
        }

        // C's miss arm is a bare `iocshSetError(-1)`: the line fails and
        // nothing is printed. Measured on `softIoc` R7.0.10-146,
        // `dbStateSet NOPE` produced no output whatsoever.
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        for name in ["dbStateSet", "dbStateClear", "dbStateShow", "dbStateCreate"] {
            let cmd = registry.get(name).unwrap();
            let tokens = if name == "dbStateCreate" {
                Vec::new()
            } else {
                vec!["IOCSH_TEST_NO_SUCH_STATE".to_string()]
            };
            let args = parse_args(&tokens, &cmd.args).unwrap();
            let printed = capture(&ctx, || {
                assert!(
                    matches!(cmd.handler.call(&args, &ctx), Ok(CommandOutcome::Failed)),
                    "{name} must fail the line"
                );
            });
            assert_eq!(printed, "", "{name} must print nothing on a miss");
        }
    }

    /// The exact bytes C prints for the vendored `typeKdegC` curve, measured
    /// on `softIoc` R7.0.10-146 after
    /// `dbLoadDatabase("$(EPICS_BASE)/dbd/bptTypeKdegC.dbd")`:
    /// `breaktable(%s) {`, then `\traw=%f slope=%e eng=%f` per point, then
    /// `}` (`dbStaticLib.c:3546-3552`). `%e` is what forces `c_exponential`
    /// — Rust's `{:.6e}` would write `e-1` where C writes `e-01`.
    #[test]
    fn db_dump_breaktable_prints_c_s_bytes_for_type_kdegc() {
        let (_db, ctx) = make_ctx();
        // The curve is not present until that line runs — C's `bptList`
        // starts empty and so does `BreakTableRegistry` (`cvt_bpt.rs`).
        assert_eq!(
            run_cmd(&ctx, "dbDumpBreaktable", &["pdbbase", "typeKdegC"]),
            ""
        );
        let (_, err, result) = run_capturing(&ctx, "dbLoadDatabase", &[SHIPPED_BPT_TYPE_KDEGC]);
        assert_eq!(err, "");
        assert!(matches!(result, Ok(CommandOutcome::Continue)));

        let expected = "breaktable(typeKdegC) {
\traw=0.000000 slope=2.472694e-01 eng=0.000000
\traw=299.268700 slope=2.462073e-01 eng=74.000000
\traw=660.752744 slope=2.499770e-01 eng=163.000000
\traw=1104.793671 slope=2.409860e-01 eng=274.000000
\traw=1702.338802 slope=2.374113e-01 eng=418.000000
\traw=2902.787322 slope=2.438970e-01 eng=703.000000
\traw=3427.599045 slope=2.516896e-01 eng=831.000000
\traw=3912.323051 slope=2.573081e-01 eng=953.000000
\traw=4098.869854 slope=2.573081e-01 eng=1001.000000
}
";
        assert_eq!(
            run_cmd(&ctx, "dbDumpBreaktable", &["pdbbase", "typeKdegC"]),
            expected
        );
        // C compares the name with `strcmp` (`dbStaticLib.c:3545`) — no glob,
        // and an unknown name prints nothing at all.
        assert_eq!(
            run_cmd(&ctx, "dbDumpBreaktable", &["pdbbase", "typeK*"]),
            ""
        );
        assert_eq!(
            run_cmd(&ctx, "dbDumpBreaktable", &["pdbbase", "nosuch"]),
            ""
        );
        // A missing table name is C's NULL, which matches every table.
        assert!(
            run_cmd(&ctx, "dbDumpBreaktable", &["pdbbase"]).contains(expected),
            "a bare `dbDumpBreaktable pdbbase` must dump every table"
        );
    }

    /// `c_exponential` is the `%e` half of `dbDumpBreaktable`'s format: six
    /// fraction digits and a signed exponent of at least two digits.
    #[test]
    fn c_exponential_writes_c_s_two_digit_signed_exponent() {
        assert_eq!(c_exponential(0.2472694), "2.472694e-01");
        assert_eq!(c_exponential(0.0), "0.000000e+00");
        assert_eq!(c_exponential(-1.5), "-1.500000e+00");
        assert_eq!(c_exponential(1.0e100), "1.000000e+100");
        assert_eq!(c_exponential(1.0e-7), "1.000000e-07");
    }

    /// `dbDumpPath` reports the path a load INSTALLED, not the one the next
    /// load would resolve. C reaches the `no path defined` line from two
    /// states — a NULL `pathPvt` and an empty one (`dbStaticLib.c:3272-3275`)
    /// — so both are one branch here too, and only a load can leave the
    /// other.
    #[test]
    #[serial_test::serial(epics_env)]
    fn db_dump_path_reports_the_path_the_last_load_installed() {
        let (_db, ctx) = make_ctx();

        // Boundary 1: nothing installed. C's `!ppathList` arm, which a
        // blank `EPICS_DB_INCLUDE_PATH` also reaches through `db_path`.
        db_loader::set_loaded_path(&[]);
        assert_eq!(
            run_cmd(&ctx, "dbDumpPath", &["pdbbase"]).trim_end(),
            "no path defined",
            "an empty path list is C's `no path defined`"
        );

        // Boundary 2: a load installed one. The list is what the load
        // resolved, joined by OSI_PATH_LIST_SEPARATOR.
        let dir = tempfile::tempdir().unwrap();
        let _path = DbIncludePath::set(dir.path());
        write_file(dir.path(), "one.db", r#"record(ai, "PATHONE") { }"#);
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbLoadRecords").unwrap();
        let args = parse_args(&["one.db".to_string()], &cmd.args).unwrap();
        capture(&ctx, || {
            cmd.handler.call(&args, &ctx).unwrap();
        });

        assert_eq!(
            run_cmd(&ctx, "dbDumpPath", &["pdbbase"]).trim_end(),
            dir.path().display().to_string(),
            "after a load the report is the list that load installed"
        );

        // The pdbbase argument is checked as it is everywhere else.
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbDumpPath").unwrap();
        let args = parse_args(&["notpdbbase".to_string()], &cmd.args).unwrap();
        assert!(
            cmd.handler.call(&args, &ctx).is_err(),
            "dbDumpPath must reject a first argument that is not pdbbase"
        );
    }
}
