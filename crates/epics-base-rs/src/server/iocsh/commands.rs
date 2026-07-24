// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the feature-ON suite.

use std::collections::HashMap;

use super::registry::*;
use crate::error::CaResult;
use crate::server::database::{RecordLoad, parse_pv_name};
use crate::server::db_loader;
use crate::server::record::FieldDeclaration;
use crate::types::EpicsValue;

/// Register all built-in iocsh commands.
pub(crate) fn register_builtins(registry: &mut CommandRegistry) {
    registry.register(cmd_help());
    registry.register(cmd_dbl());
    registry.register(cmd_dbgf());
    registry.register(cmd_dbpf());
    registry.register(cmd_dbpr());
    registry.register(cmd_dbsr());
    registry.register(cmd_dbglob());
    registry.register(cmd_dbgrep());
    registry.register(cmd_scanppl());
    registry.register(cmd_post_event());
    registry.register(cmd_post_event_alias());
    registry.register(cmd_ioc_stats());
    registry.register(cmd_db_load_records());
    registry.register(cmd_db_load_template());
    registry.register(cmd_db_create_record());
    registry.register(cmd_db_delete_record());
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
    super::access_commands::register(registry);
}

/// `afterIocRunning <command>` — queue an iocsh command line to run
/// after iocInit completes. Mirrors epics-base PR #558.
fn cmd_after_ioc_running() -> CommandDef {
    CommandDef::new(
        "afterIocRunning",
        vec![ArgDesc {
            name: "command",
            arg_type: ArgType::String,
            optional: false,
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
/// Mirrors epics-base PR #505 (record deletion at DB creation).
fn cmd_db_delete_record() -> CommandDef {
    CommandDef::new(
        "dbDeleteRecord",
        vec![ArgDesc {
            name: "recordName",
            arg_type: ArgType::String,
            optional: false,
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
                ctx.println(&format!("dbDeleteRecord: no record named '{name}'"));
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
            optional: true,
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

fn cmd_dbl() -> CommandDef {
    CommandDef::new(
        "dbl",
        vec![ArgDesc {
            name: "recordType",
            arg_type: ArgType::String,
            optional: true,
        }],
        "dbl [recordType] - List record names, optionally filtered by type",
        |args: &[ArgValue], ctx: &CommandContext| {
            let type_filter = match &args[0] {
                ArgValue::String(s) => Some(s.as_str()),
                _ => None,
            };

            let names = ctx.block_on(ctx.db().all_record_names());
            let mut names = names;
            names.sort();

            for name in &names {
                if let Some(filter) = type_filter {
                    let rec = ctx.db().get_record(name);
                    if let Some(rec) = rec {
                        let inst = rec.read();
                        if inst.record.record_type() != filter {
                            continue;
                        }
                    }
                }
                ctx.println(name);
            }

            Ok(CommandOutcome::Continue)
        },
    )
}

fn cmd_dbgf() -> CommandDef {
    CommandDef::new(
        "dbgf",
        vec![ArgDesc {
            name: "pvname",
            arg_type: ArgType::String,
            optional: false,
        }],
        "dbgf pvname - Get field value",
        |args: &[ArgValue], ctx: &CommandContext| {
            let name = match &args[0] {
                ArgValue::String(s) => s,
                _ => return Err("invalid argument".to_string()),
            };

            match ctx.db().get_pv(name) {
                Ok(val) => {
                    let type_name = dbf_type_name(&val);
                    // epics-base dc70dfd6: dbgf must C-style-escape
                    // non-printable bytes in CHAR-array output and
                    // wrap in double-quotes so a CHAR array carrying
                    // control bytes does not corrupt the operator's
                    // terminal. Other types fall through to the
                    // standard Display formatter.
                    let formatted = match &val {
                        EpicsValue::CharArray(arr) => {
                            format!("\"{}\"", escape_char_array_for_dbgf(arr))
                        }
                        _ => format!("{val}"),
                    };
                    ctx.println(&format!("{type_name}: {formatted}"));
                    Ok(CommandOutcome::Continue)
                }
                Err(e) => Err(format!("{e}")),
            }
        },
    )
}

fn cmd_dbpf() -> CommandDef {
    CommandDef::new(
        "dbpf",
        vec![
            ArgDesc {
                name: "pvname",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "value",
                arg_type: ArgType::String,
                optional: false,
            },
        ],
        "dbpf pvname value - Put field value",
        |args: &[ArgValue], ctx: &CommandContext| {
            let name = match &args[0] {
                ArgValue::String(s) => s,
                _ => return Err("invalid argument".to_string()),
            };
            let value_str = match &args[1] {
                ArgValue::String(s) => s,
                _ => return Err("invalid argument".to_string()),
            };

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

            let value = if field == "DTYP" {
                // DTYP is DBF_DEVICE: its choices are the record type's live
                // device menu (dynamic, per record type — device support names
                // registered at runtime), NOT the field-blind static table that
                // `EpicsValue::parse(Enum, _)` consults via `resolve_menu_string`.
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
                EpicsValue::parse(dbf, value_str)
                    .map_err(|e| format!("cannot parse '{value_str}' as {dbf:?}: {e}"))?
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
            put_result.map_err(|e| {
                // epics-base PR #689 — when the field
                // doesn't exist, suggest near-by names so a typo
                // ("DSEC" instead of "DESC") is caught quickly.
                let msg = format!("{e}");
                if msg.contains("FieldNotFound") || msg.contains(&format!("'{field}'")) {
                    if let Some(suggestion) =
                        ctx.block_on(suggest_field_name(ctx.db(), base, &field))
                    {
                        return format!("{msg}; did you mean '{suggestion}'?");
                    }
                }
                msg
            })?;

            // Read back to confirm
            match ctx.db().get_pv(name) {
                Ok(val) => {
                    let type_name = dbf_type_name(&val);
                    ctx.println(&format!("{type_name}: {val}"));
                }
                Err(_) => {}
            }

            Ok(CommandOutcome::Continue)
        },
    )
}

fn cmd_dbpr() -> CommandDef {
    CommandDef::new(
        "dbpr",
        vec![
            ArgDesc {
                name: "record",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "level",
                arg_type: ArgType::Int,
                optional: true,
            },
        ],
        "dbpr record [level] - Print record fields (level 0-2)",
        |args: &[ArgValue], ctx: &CommandContext| {
            let name = match &args[0] {
                ArgValue::String(s) => s,
                _ => return Err("invalid argument".to_string()),
            };
            let level = match &args[1] {
                ArgValue::Int(n) => *n as i32,
                ArgValue::Missing => 0,
                _ => 0,
            };

            let rec = ctx
                .db()
                .get_record(name)
                .ok_or_else(|| format!("record '{}' not found", name))?;

            // Collect field values inside lock, format outside
            let fields: Vec<(String, String)> = ctx.block_on(async {
                // Record name for the alias query, read under a short-lived
                // guard that is dropped (block close) before the field reads
                // below take their own `rec.read()`.
                let rec_name = { rec.read().name.clone() };
                let aliases = ctx.db().aliases_for_record(&rec_name);

                let inst = rec.read();
                let mut fields = Vec::new();

                // Level 0: NAME, RTYP, VAL (+ alias names if any —
                // base's dbpr surfaces aliases here so admins know
                // every spelling that resolves to this record).
                fields.push(("NAME".to_string(), inst.name.clone()));
                if !aliases.is_empty() {
                    fields.push(("ALIASES".to_string(), aliases.join(", ")));
                }
                fields.push(("RTYP".to_string(), inst.record.record_type().to_string()));
                if let Some(val) = inst.record.val() {
                    fields.push(("VAL".to_string(), format!("{val}")));
                }
                if inst.common.sevr != crate::server::record::AlarmSeverity::NoAlarm {
                    fields.push(("SEVR".to_string(), format!("{:?}", inst.common.sevr)));
                    fields.push(("STAT".to_string(), format!("{}", inst.common.stat)));
                }

                if level >= 1 {
                    fields.push(("SCAN".to_string(), format!("{}", inst.common.scan)));
                    fields.push(("DTYP".to_string(), inst.common.dtyp.clone()));
                    if !inst.common.inp.is_empty() {
                        fields.push(("INP".to_string(), inst.common.inp.clone()));
                    }
                    if !inst.common.out.is_empty() {
                        fields.push(("OUT".to_string(), inst.common.out.clone()));
                    }
                    if !inst.common.flnk.is_empty() {
                        fields.push(("FLNK".to_string(), inst.common.flnk.clone()));
                    }
                    fields.push((
                        "PINI".to_string(),
                        format!(
                            "{}",
                            crate::server::record::PiniMode::from_u16(inst.common.pini as u16)
                        ),
                    ));
                    fields.push(("UDF".to_string(), format!("{}", inst.common.udf)));
                }

                if level >= 2 {
                    // All record-specific fields
                    for desc in inst.record.field_list() {
                        let fname = desc.name.to_string();
                        if fields.iter().any(|(n, _)| n == &fname) {
                            continue;
                        }
                        if let Some(val) = inst.record.get_field(desc.name) {
                            fields.push((fname, format!("{val}")));
                        }
                    }
                    // Alarm fields
                    if let Some(ref alarm) = inst.common.analog_alarm {
                        fields.push(("HIHI".to_string(), format!("{}", alarm.hihi)));
                        fields.push(("HIGH".to_string(), format!("{}", alarm.high)));
                        fields.push(("LOW".to_string(), format!("{}", alarm.low)));
                        fields.push(("LOLO".to_string(), format!("{}", alarm.lolo)));
                        fields.push(("HHSV".to_string(), format!("{:?}", alarm.hhsv)));
                        fields.push(("HSV".to_string(), format!("{:?}", alarm.hsv)));
                        fields.push(("LSV".to_string(), format!("{:?}", alarm.lsv)));
                        fields.push(("LLSV".to_string(), format!("{:?}", alarm.llsv)));
                    }
                    fields.push(("ASG".to_string(), inst.common.asg.clone()));
                    // Surface info(...) tags so admins can
                    // verify driver hints (asyn:READBACK, Q:group, …)
                    // landed on the record. Sorted for stable output.
                    let mut info_keys: Vec<&String> = inst.info.keys().collect();
                    info_keys.sort();
                    for key in info_keys {
                        let val = inst.info.get(key).cloned().unwrap_or_default();
                        fields.push((format!("info({key})"), val));
                    }
                }

                fields
            });

            // Format outside lock
            for (name, value) in &fields {
                ctx.println(&format!("{name:>8}: {value}"));
            }

            Ok(CommandOutcome::Continue)
        },
    )
}

/// Shared handler for the record-name glob search `dbglob` / `dbgrep`.
/// Mirrors epics-base PR #626 (rename `dbgrep` → `dbglob` with alias)
/// and PR #613 (add fields argument). The `fields` argument is comma-
/// separated; when present each matching record additionally dumps
/// the listed field values. (`dbsr` is the *server report* — a
/// separate command — not this name search.)
fn dbsr_handler(args: &[ArgValue], ctx: &CommandContext) -> CommandResult {
    let pattern = args
        .first()
        .and_then(|a| {
            if let ArgValue::String(s) = a {
                Some(s.as_str())
            } else {
                None
            }
        })
        .unwrap_or("*");
    let fields: Vec<String> = args
        .get(1)
        .and_then(|a| {
            if let ArgValue::String(s) = a {
                Some(s.split(',').map(|f| f.trim().to_string()).collect())
            } else {
                None
            }
        })
        .unwrap_or_default();

    // Walk record names + aliases + simple PVs. Base's
    // `dbFirstRecord` iteration only sees records, but our
    // PvDatabase also serves `add_pv`-registered simple PVs (CA
    // gateway shadows, IOC-stat scratchpads). A user globbing for
    // every channel name would be confused if simple PVs were
    // hidden. Field lookup via `get_record` follows alias→canonical;
    // for simple PVs the field-dump branch silently
    // skips since they're not records.
    let mut names = ctx.block_on(ctx.db().all_record_names());
    names.extend(ctx.db().all_alias_names());
    names.extend(ctx.block_on(ctx.db().all_simple_pv_names()));
    names.sort();
    names.dedup();

    let mut count = 0;
    for name in &names {
        if !glob_match(pattern, name) {
            continue;
        }
        ctx.println(name);
        count += 1;
        if fields.is_empty() {
            continue;
        }
        // Dump each requested field for this record.
        if let Some(rec_arc) = ctx.db().get_record(name) {
            let inst = rec_arc.read();
            for fname in &fields {
                let value = inst
                    .record
                    .get_field(fname)
                    .map(|v| format!("{v:?}"))
                    .unwrap_or_else(|| "<no field>".to_string());
                ctx.println(&format!("  {fname:>8}: {value}"));
            }
        }
    }
    ctx.println(&format!("Total: {count} records"));
    Ok(CommandOutcome::Continue)
}

/// `dbsr [interest level]` — Database Server Report.
///
/// C `dbIocRegister.c:142-144` registers `dbsr` as the *Database
/// Server Report* (`dbServerReport` — prints CA/PVA server status and
/// connected-client information). The Rust port previously aliased
/// `dbsr` to the record-name glob search, which is the wrong command
/// (`dbgrep`/`dbglob` is the name search — kept below).
///
/// This crate has no live CA-server client registry reachable from the
/// iocsh `CommandContext` (only `PvDatabase`), so the connected-client
/// detail a C `dbsr` prints is unavailable here. The report covers what
/// the database server *can* expose: the channel population it serves.
fn cmd_dbsr() -> CommandDef {
    CommandDef::new(
        "dbsr",
        vec![ArgDesc {
            name: "interest level",
            arg_type: ArgType::Int,
            optional: true,
        }],
        "dbsr [interest level] — Database Server Report (served-channel statistics)",
        |_args: &[ArgValue], ctx: &CommandContext| {
            let records = ctx.block_on(ctx.db().all_record_names());
            let aliases = ctx.db().all_alias_names();
            let simple = ctx.block_on(ctx.db().all_simple_pv_names());
            ctx.println("Database Server Report");
            ctx.println(&format!("  Records served:     {}", records.len()));
            ctx.println(&format!("  Record aliases:     {}", aliases.len()));
            ctx.println(&format!("  Simple PVs served:  {}", simple.len()));
            ctx.println(&format!(
                "  Total channels:     {}",
                records.len() + aliases.len() + simple.len()
            ));
            Ok(CommandOutcome::Continue)
        },
    )
}

fn cmd_dbglob() -> CommandDef {
    CommandDef::new(
        "dbglob",
        vec![
            ArgDesc {
                name: "pattern",
                arg_type: ArgType::String,
                optional: true,
            },
            ArgDesc {
                name: "fields",
                arg_type: ArgType::String,
                optional: true,
            },
        ],
        "dbglob [pattern] [fields] — Search records by name pattern \
         (epics-base PR #626; `?` matches 0-or-1 chars)",
        dbsr_handler,
    )
}

fn cmd_dbgrep() -> CommandDef {
    CommandDef::new(
        "dbgrep",
        vec![
            ArgDesc {
                name: "pattern",
                arg_type: ArgType::String,
                optional: true,
            },
            ArgDesc {
                name: "fields",
                arg_type: ArgType::String,
                optional: true,
            },
        ],
        "dbgrep [pattern] [fields] — Search records by name pattern \
         (legacy spelling of dbglob, epics-base PR #626)",
        dbsr_handler,
    )
}

fn cmd_scanppl() -> CommandDef {
    CommandDef::new(
        "scanppl",
        vec![],
        "scanppl — Print scan phase lists",
        |_args: &[ArgValue], ctx: &CommandContext| {
            use crate::server::record::ScanType;
            let scan_types = [
                ScanType::Sec01,
                ScanType::Sec02,
                ScanType::Sec05,
                ScanType::Sec1,
                ScanType::Sec2,
                ScanType::Sec5,
                ScanType::Sec10,
                ScanType::Event,
                ScanType::Passive,
            ];

            for st in &scan_types {
                let names = ctx.block_on(ctx.db().records_for_scan(*st));
                if !names.is_empty() {
                    ctx.println(&format!("{st}: {} records", names.len()));
                    for name in &names {
                        ctx.println(&format!("  {name}"));
                    }
                }
            }

            let io_count = ctx
                .block_on(ctx.db().records_for_scan(ScanType::IoIntr))
                .len();
            if io_count > 0 {
                ctx.println(&format!("I/O Intr: {io_count} records"));
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `pushd [dir]` — push the current directory onto the stack and `cd`.
/// With no argument, swaps the current dir with the top of the stack.
fn cmd_pushd() -> CommandDef {
    CommandDef::new(
        "pushd",
        vec![ArgDesc {
            name: "dir",
            arg_type: ArgType::String,
            optional: true,
        }],
        "pushd [dir] — push current dir onto stack and cd to <dir>",
        |args: &[ArgValue], ctx: &CommandContext| {
            let cwd = match std::env::current_dir() {
                Ok(p) => p,
                Err(e) => {
                    ctx.println(&format!("pushd: cannot read cwd: {e}"));
                    return Ok(CommandOutcome::Continue);
                }
            };
            match &args[0] {
                ArgValue::String(dir) => {
                    if let Err(e) = std::env::set_current_dir(dir) {
                        ctx.println(&format!("pushd: {dir}: {e}"));
                        return Ok(CommandOutcome::Continue);
                    }
                    dir_stack().lock().unwrap().push(cwd);
                }
                _ => {
                    // No arg: swap cwd with top of stack.
                    let mut stack = dir_stack().lock().unwrap();
                    let Some(top) = stack.pop() else {
                        ctx.println("pushd: directory stack empty");
                        return Ok(CommandOutcome::Continue);
                    };
                    if let Err(e) = std::env::set_current_dir(&top) {
                        // Restore on failure.
                        stack.push(top);
                        ctx.println(&format!("pushd: {e}"));
                        return Ok(CommandOutcome::Continue);
                    }
                    stack.push(cwd);
                }
            }
            print_stack(ctx);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `popd` — pop the top of the directory stack and `cd` to it.
fn cmd_popd() -> CommandDef {
    CommandDef::new(
        "popd",
        vec![],
        "popd — pop top of directory stack and cd to it",
        |_args: &[ArgValue], ctx: &CommandContext| {
            let mut stack = dir_stack().lock().unwrap();
            let Some(top) = stack.pop() else {
                ctx.println("popd: directory stack empty");
                return Ok(CommandOutcome::Continue);
            };
            if let Err(e) = std::env::set_current_dir(&top) {
                // Restore the entry — failed cd must not lose stack state.
                stack.push(top);
                ctx.println(&format!("popd: {e}"));
                return Ok(CommandOutcome::Continue);
            }
            drop(stack);
            print_stack(ctx);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dirs` — list the directory stack (cwd + saved entries).
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

/// `dbCreateRecord <type> <name>` — create a record BEFORE `iocInit`.
///
/// Mirrors epics-base PR #812. Validates the name with the same rules
/// as `parse_db` (PR #78), refuses duplicate names, and routes the
/// instantiation through the same factory registry as `dbLoadRecords`.
///
/// "At runtime" it is not: C's `dbCreateRecordCallFunc`
/// (`dbStaticIocRegister.c:288`) refuses the command outright once
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
                name: "recordType",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "recordName",
                arg_type: ArgType::String,
                optional: false,
            },
        ],
        "dbCreateRecord <type> <name> — Create a new record of <type> (before iocInit)",
        |args: &[ArgValue], ctx: &CommandContext| {
            // Creating a record IS entering the load phase — the record's links
            // are classified by `iocInit`, with the rest of the database. Once
            // `iocInit` has run there is no phase to enter and C refuses.
            if let Err(e) = ctx.db().begin_load() {
                return Err(e.to_string());
            }
            let rec_type = match &args[0] {
                ArgValue::String(s) => s.clone(),
                _ => {
                    ctx.println("dbCreateRecord: missing recordType");
                    return Ok(CommandOutcome::Continue);
                }
            };
            let name = match &args[1] {
                ArgValue::String(s) => s.clone(),
                _ => {
                    ctx.println("dbCreateRecord: missing recordName");
                    return Ok(CommandOutcome::Continue);
                }
            };
            if let Err(e) = db_loader::validate_record_name(&name, 0, 0) {
                ctx.println(&format!("dbCreateRecord: {e}"));
                return Ok(CommandOutcome::Continue);
            }
            if ctx.db().get_record(&name).is_some() {
                ctx.println(&format!("dbCreateRecord: record '{name}' already exists"));
                return Ok(CommandOutcome::Continue);
            }
            let record = match db_loader::create_record(&rec_type) {
                Ok(r) => r,
                Err(e) => {
                    ctx.println(&format!("dbCreateRecord: {e}"));
                    return Ok(CommandOutcome::Continue);
                }
            };
            if let Err(e) = ctx.block_on(ctx.db().add_record(&name, record)) {
                ctx.println(&format!("dbCreateRecord: {e}"));
                return Ok(CommandOutcome::Continue);
            }
            ctx.println(&format!("dbCreateRecord: created '{name}' ({rec_type})"));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `postEvent [event]` — process records scanned on a software event.
///
/// L-2: C `dbIocRegister.c` registers this command as `postEvent`
/// (camelCase); the Rust port previously registered the documented
/// name with an underscore (`post_event`), so an `st.cmd` calling the
/// real name hit "unknown command". Both spellings are registered now
/// — `postEvent` is the canonical C name, `post_event` is kept as a
/// back-compat alias for any existing Rust-side scripts.
fn cmd_post_event() -> CommandDef {
    CommandDef::new(
        "postEvent",
        vec![ArgDesc {
            name: "event",
            arg_type: ArgType::Int,
            optional: true,
        }],
        "postEvent [event] — Process records with SCAN=Event",
        post_event_handler,
    )
}

fn cmd_post_event_alias() -> CommandDef {
    CommandDef::new(
        "post_event",
        vec![ArgDesc {
            name: "event",
            arg_type: ArgType::Int,
            optional: true,
        }],
        "post_event [event] — Back-compat alias of postEvent",
        post_event_handler,
    )
}

fn post_event_handler(args: &[ArgValue], ctx: &CommandContext) -> CommandResult {
    // C `dbIocRegister.c` `postEvent <event>` routes through
    // `post_event(int)` -> `postEvent(pevent_list[event])`, posting
    // ONLY the records whose `EVNT` matches that event. Route to
    // `post_event_named` when an event argument is given; with no
    // argument fall back to the (non-C) "process every Event record".
    match args.first() {
        Some(ArgValue::Int(event)) => {
            ctx.block_on(ctx.db().post_event_named(&event.to_string()));
            ctx.println(&format!("Posted event {event}"));
        }
        Some(ArgValue::String(name)) if !name.is_empty() => {
            ctx.block_on(ctx.db().post_event_named(name));
            ctx.println(&format!("Posted event {name}"));
        }
        _ => {
            ctx.block_on(ctx.db().post_event());
            ctx.println("Event scan processed (all SCAN=Event records)");
        }
    }
    Ok(CommandOutcome::Continue)
}

/// Simple glob matching (* and ? wildcards).
fn glob_match(pattern: &str, text: &str) -> bool {
    let mut pi = pattern.chars().peekable();
    let mut ti = text.chars().peekable();

    fn do_match(
        pat: &mut std::iter::Peekable<std::str::Chars>,
        txt: &mut std::iter::Peekable<std::str::Chars>,
    ) -> bool {
        while let Some(&pc) = pat.peek() {
            match pc {
                '*' => {
                    pat.next();
                    if pat.peek().is_none() {
                        return true; // trailing * matches everything
                    }
                    // Try matching rest from every position
                    loop {
                        let mut pat_clone = pat.clone();
                        let mut txt_clone = txt.clone();
                        if do_match(&mut pat_clone, &mut txt_clone) {
                            return true;
                        }
                        if txt.next().is_none() {
                            return false;
                        }
                    }
                }
                '?' => {
                    // L-1: C `dbglob` documents `?` as matching "0 or
                    // one characters" (dbIocRegister.c:246-248), not
                    // exactly one. Try both the skip-zero and
                    // consume-one branches.
                    pat.next();
                    {
                        let mut pat_zero = pat.clone();
                        let mut txt_zero = txt.clone();
                        if do_match(&mut pat_zero, &mut txt_zero) {
                            return true;
                        }
                    }
                    if txt.next().is_none() {
                        return false;
                    }
                }
                c => {
                    pat.next();
                    match txt.next() {
                        Some(tc) if tc == c => {}
                        _ => return false,
                    }
                }
            }
        }
        txt.peek().is_none()
    }

    do_match(&mut pi, &mut ti)
}

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
                ScanType::Sec01,
                ScanType::Sec02,
                ScanType::Sec05,
                ScanType::Sec1,
                ScanType::Sec2,
                ScanType::Sec5,
                ScanType::Sec10,
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
                name: "file",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "macros",
                arg_type: ArgType::String,
                optional: true,
            },
        ],
        "dbLoadRecords file [macros] - Load records from a .db/.template file",
        |args: &[ArgValue], ctx: &CommandContext| {
            let path = match &args[0] {
                ArgValue::String(s) => s,
                _ => return Err("invalid argument".to_string()),
            };
            let macros_str = match &args[1] {
                ArgValue::String(s) => s.as_str(),
                _ => "",
            };

            // `dbLoadRecords` OPENS the load phase; it does not close it — the
            // boundary a record's links are classified against is `iocInit`,
            // after EVERY `dbLoadRecords` in the `st.cmd`, so a forward
            // reference to a record loaded by a later file is still a local PV
            // (R18-92). Idempotent across the several loads one script issues.
            //
            // And once `iocInit` has run there is no load phase to open: C's
            // `dbReadCOM` (dbLexRoutines.c:236) fails the read with -2 before it
            // even opens the file, and `dbLoadRecords` (dbAccess.c:808-812)
            // prints exactly this (R19-63). So this is asked BEFORE the file is
            // read, and a refusal creates nothing.
            //
            // ```text
            // epics> iocInit
            // epics> dbLoadRecords("b.db")
            // ERROR: Failed to load 'b.db'
            //     Records cannot be loaded after iocInit!
            // ```
            if ctx.db().begin_load().is_err() {
                return Err(format!(
                    "Failed to load '{path}'\n    Records cannot be loaded after iocInit!"
                ));
            }

            let macros = parse_macro_string(macros_str);

            // Build include config from EPICS_DB_INCLUDE_PATH and resolve the
            // file against it (matching C dbLoadRecords behavior). Shared with
            // `dbLoadTemplate`, which resolves its `.substitutions` file the
            // same way.
            let (config, file_path) = resolve_db_include_path(path);
            // C `dbLoadRecords` macros are pure text substitution (dbLexRoutines.c
            // → macLib): a `DTYP=` macro reaches a record only where the file wrote
            // `field(DTYP,"$(DTYP)")`. It does NOT rewrite a record that spells its
            // DTYP literally. `parse_db_file_with_breaktables` already performs that
            // substitution, so there is nothing further to do for DTYP here.
            let (defs, breaktables) =
                db_loader::parse_db_file_with_breaktables(&file_path, &macros, &config)
                    .map_err(|e| format!("parse error: {e}"))?;

            // Merge any `breaktable(...)` definitions into the database's shared
            // breakpoint-table registry (C `bptList`) and snapshot it for the
            // records loaded by this command. A record resolves a table loaded
            // by an earlier or the same `dbLoadRecords` (C ordering).
            let breaktable_registry =
                ctx.block_on(async { ctx.db().add_breaktables(breaktables).await });

            let count = defs.len();

            // One install path for both `dbLoadRecords` and `dbLoadTemplate`:
            // each expanded record flows through the SAME per-record routine,
            // so template-loaded records are indistinguishable from directly
            // loaded ones.
            ctx.block_on(install_record_defs(ctx, defs, &breaktable_registry))?;

            ctx.println(&format!("Loaded {count} record(s) from {path}"));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Build the DB include config from `EPICS_DB_INCLUDE_PATH` and resolve
/// `path` against it: an existing path is used directly; otherwise a
/// relative name is searched across the include paths (matching C
/// `dbLoadRecords` search). Shared by `dbLoadRecords` and
/// `dbLoadTemplate`.
fn resolve_db_include_path(path: &str) -> (db_loader::DbLoadConfig, std::path::PathBuf) {
    let include_paths: Vec<std::path::PathBuf> =
        if let Ok(val) = std::env::var("EPICS_DB_INCLUDE_PATH") {
            split_db_paths(&val)
        } else {
            Vec::new()
        };
    let config = db_loader::DbLoadConfig {
        include_paths,
        max_include_depth: 32,
    };
    let file_path = {
        let p = std::path::Path::new(path);
        if p.exists() {
            p.to_path_buf()
        } else if !p.is_absolute() {
            // Search include paths for relative filenames
            let mut resolved = None;
            for dir in &config.include_paths {
                let candidate = dir.join(p);
                if candidate.exists() {
                    resolved = Some(candidate);
                    break;
                }
            }
            resolved.unwrap_or_else(|| p.to_path_buf())
        } else {
            p.to_path_buf()
        }
    };
    (config, file_path)
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
    breaktable_registry: &crate::server::cvt_bpt::BreakTableRegistry,
) -> Result<(), String> {
    for mut def in defs {
        // Resolve a `LINR` field naming a loaded breakpoint table to its
        // menuConvert index (shared with the IocBuilder load path).
        db_loader::resolve_linr_breaktable_names(
            &def.record_type,
            &mut def.fields,
            breaktable_registry,
        );
        let added: Result<(), String> = async {
            // C-parity (dbLexRoutines.c:1170-1188): the SAME
            // record name re-loaded with the SAME record_type
            // merges fields into the existing instance (the
            // standard ADCore convention — simDetector.template
            // overrides ColorMode menu choices declared by its
            // included NDArrayBase.template). A different
            // record_type is fatal. `dbRecordsOnceOnly` global
            // is not yet wired; tighten here if/when needed.
            let existing = if let Some(rec) = ctx.db().get_record(&def.name) {
                let r = rec.read();
                let existing_type = r.record.record_type();
                if existing_type != def.record_type {
                    return Err(format!(
                        "dbLoadRecords: {} record '{}' already exists, can't load {} record",
                        existing_type, def.name, def.record_type
                    ));
                }
                drop(r);
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
                        return Err(format!("{e}"));
                    }
                }
                rec_arc
            } else {
                let mut record =
                    db_loader::create_record(&def.record_type).map_err(|e| format!("{e}"))?;
                // The breakpoint-table registry is installed by the
                // creation sink; apply_fields only needs the LINR
                // index, already resolved above.
                if let Err(e) =
                    db_loader::apply_fields(&mut record, &def.fields, &mut common_fields)
                {
                    return Err(format!("{e}"));
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
                    return Err(format!("dbLoadRecords: '{}' rejected: {e}", def.name));
                }
                ctx.db().get_record(&def.name).ok_or_else(|| {
                    format!(
                        "dbLoadRecords: '{}' vanished between add_record and get_record",
                        def.name
                    )
                })?
            };

            // Register any aliases declared in the record body
            // (epics-base PR #336). Failures are reported but
            // don't abort the load — the record is already in.
            // For a merge, aliases declared in the new block
            // are also registered (C parser appends).
            for alias in &def.aliases {
                if let Err(e) = ctx.db().add_alias(alias, &def.name).await {
                    eprintln!(
                        "dbLoadRecords: alias '{alias}' for '{}' rejected: {e}",
                        def.name
                    );
                }
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
                            eprintln!("put_common_field({name}) failed for {}: {e}", def.name);
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
            // C `recGblInitSimm` + `recGblInitConstantLink(&siol, …,
            // &sval)`, run from every SIML-bearing `init_record`
            // (pass 1) — the only site that loads a constant
            // SIML/SIOL into SIMM/SVAL.
            ctx.db().rec_gbl_init_simm(&rec_arc);
            // C `wdogInit(prec)` from `init_record` pass 1
            // (histogramRecord.c:168) — arms the SDEL monitor
            // watchdog; a re-arm supersedes the previous one, which is
            // what the merge re-init above needs.
            ctx.db().arm_watchdog(&def.name);
            Ok(())
        }
        .await;
        if let Err(e) = added {
            // epics-base 144f975: propagate the failure to the
            // iocsh script chain (equivalent of `iocshSetError`)
            // so a startup script returns non-zero on a rejected
            // record load. The printed message stays for
            // operator-visible diagnostics; the `Err` return
            // lets `execute_script` mark its `last_err`.
            ctx.println(&e);
            return Err(e);
        }
    }
    Ok(())
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
                name: "subFile",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "globalMacros",
                arg_type: ArgType::String,
                optional: true,
            },
        ],
        "dbLoadTemplate subFile [globalMacros] - Load records from a .substitutions file",
        |args: &[ArgValue], ctx: &CommandContext| {
            let path = match &args[0] {
                ArgValue::String(s) => s,
                _ => return Err("invalid argument".to_string()),
            };
            let macros_str = match &args[1] {
                ArgValue::String(s) => s.as_str(),
                _ => "",
            };

            // Same load-phase gate as `dbLoadRecords`: opens the load phase
            // (idempotent across the several loads a script issues) and
            // refuses with C's diagnostic once `iocInit` has run — C's
            // `dbLoadTemplate` delegates to `dbReadDatabase`/`dbLoadRecords`,
            // which fail identically after init (dbAccess.c:808-812).
            if ctx.db().begin_load().is_err() {
                return Err(format!(
                    "Failed to load '{path}'\n    Records cannot be loaded after iocInit!"
                ));
            }

            let macros = parse_macro_string(macros_str);

            // Resolve the `.substitutions` file the same way `dbLoadRecords`
            // resolves a `.db` file (existing path, else EPICS_DB_INCLUDE_PATH).
            let (config, file_path) = resolve_db_include_path(path);

            // `load_substitution_file` parses the `.substitutions` file, then
            // for every template load it describes calls `parse_db_file` with
            // the merged macro set (globals + row, row winning), concatenating
            // the records — the same parser `dbLoadRecords` uses. No second
            // substitutions parser.
            let defs = db_loader::load_substitution_file(&file_path, &macros, &config)
                .map_err(|e| format!("parse error: {e}"))?;

            // `load_substitution_file` does not surface `breaktable(...)`
            // definitions; snapshot the database's current registry (no
            // mutation for an empty push) so a template whose `.db` uses a
            // `LINR` table name loaded by an earlier `dbLoadRecords` still
            // resolves it, matching that command's install path.
            let breaktable_registry =
                ctx.block_on(async { ctx.db().add_breaktables(vec![]).await });

            let count = defs.len();

            // Identical install path to `dbLoadRecords`: same duplicate-name
            // merge, field application, load-then-init ordering and post-load
            // passes, so a template-loaded record is indistinguishable from a
            // directly loaded one.
            ctx.block_on(install_record_defs(ctx, defs, &breaktable_registry))?;

            ctx.println(&format!("Loaded {count} record(s) from {path}"));
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
                optional: false,
            },
            ArgDesc {
                name: "value",
                arg_type: ArgType::String,
                optional: false,
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
        "iocInit - Initialize the IOC (record init here; the rest is run by IocApplication)",
        |_args: &[ArgValue], ctx: &CommandContext| {
            // The record-initialisation half of C's `iocInit` runs HERE, not at
            // each `dbLoadRecords`: a link that forward-references a record
            // loaded by a LATER `dbLoadRecords` in the same `st.cmd` must still
            // classify as a local PV (R18-92). `PvDatabase::ioc_init` closes the
            // load phase and runs every classification the loads queued; it is
            // idempotent, so the `IocApplication`'s own call after the script is
            // a no-op when the script spells `iocInit` out. Device support,
            // scanning and PINI remain the application's, hence the note.
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

        // Escape: `\X` makes `X` a literal (and not a delimiter); the
        // backslash itself is dropped. Quotes do not suppress escapes.
        if c == '\\' && i + 1 < chars.len() {
            push_lit!(chars[i + 1]);
            i += 2;
            continue;
        }

        // Inside a quote: every char is literal until the matching quote.
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                push_lit!(c);
            }
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            // An opening quote also begins the token (e.g. `=""`).
            match state {
                St::PreName => state = St::InName,
                St::PreValue => state = St::InValue,
                _ => {}
            }
            i += 1;
            continue;
        }

        match state {
            St::PreName => {
                if c == '=' {
                    state = St::PreValue;
                } else if !(c.is_ascii_whitespace() || c == ',') {
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
                } else if c.is_ascii_whitespace() {
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
                } else if !c.is_ascii_whitespace() {
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
                } else if c.is_ascii_whitespace() {
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
/// Macro values may reference environment variables via `$(ENVVAR)`.
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
            macros.insert(k, super::registry::substitute_env_vars(&v));
        }
    }
    macros
}

#[cfg(test)]
mod field_suggestion_tests {
    use super::edit_distance_short;

    #[test]
    fn edit_distance_recognises_simple_typos() {
        // Substitution within budget.
        assert!(edit_distance_short("DSEC", "DESC") <= 2);
        assert!(edit_distance_short("EGUU", "EGU") <= 2);
        // Deletion.
        assert!(edit_distance_short("DESCR", "DESC") <= 2);
        // Long-distance — must exceed 2 so suggester rejects.
        assert!(edit_distance_short("HELLO", "DESC") > 2);
    }

    #[test]
    fn edit_distance_handles_empty_inputs() {
        assert_eq!(edit_distance_short("", ""), 0);
        assert_eq!(edit_distance_short("ABC", ""), 3);
        assert_eq!(edit_distance_short("", "XYZ"), 3);
    }
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

/// Suggest a field name close to `typo` that actually exists on
/// the record `record_name`. Returns `None` when no candidate is
/// within edit-distance ≤ 2 (Damerau-Levenshtein subset). Mirrors
/// epics-base PR #689 — "did you mean" hint on field-not-found
/// errors. Uppercase comparison so `desc` ≈ `DESC` matches.
async fn suggest_field_name(
    db: &std::sync::Arc<crate::server::database::PvDatabase>,
    record_name: &str,
    typo: &str,
) -> Option<String> {
    let typo_uc = typo.to_ascii_uppercase();
    let rec = db.get_record(record_name)?;
    let inst = rec.read();
    let mut candidates: Vec<&str> = inst.record.field_list().iter().map(|d| d.name).collect();
    // Common dbCommon fields are also valid PUT targets.
    candidates.extend([
        "VAL", "DESC", "EGU", "SCAN", "PINI", "DTYP", "INP", "OUT", "FLNK", "NAME", "RTYP", "PHAS",
        "PRIO", "DISA", "DISV", "DISS", "DISP", "PROC", "ASG", "TPRO", "TSE", "TSEL", "UDF",
        "SEVR", "STAT", "AMSG",
    ]);
    let mut best: Option<(usize, &str)> = None;
    for cand in &candidates {
        let dist = edit_distance_short(&typo_uc, cand);
        if dist > 2 {
            continue;
        }
        match best {
            None => best = Some((dist, cand)),
            Some((d, _)) if dist < d => best = Some((dist, cand)),
            _ => {}
        }
    }
    best.map(|(_, name)| name.to_string())
}

/// Bounded Damerau-Levenshtein for short ASCII strings used by
/// `suggest_field_name`. Returns the edit distance; cap at
/// `a.len() + b.len()` so the loop never blows up.
fn edit_distance_short(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }
    let a: Vec<u8> = a.bytes().collect();
    let b: Vec<u8> = b.bytes().collect();
    if a.is_empty() || b.is_empty() {
        return a.len().max(b.len());
    }
    let mut prev = (0..=b.len()).collect::<Vec<usize>>();
    let mut curr = vec![0; b.len() + 1];
    for (i, ai) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, bj) in b.iter().enumerate() {
            let cost = if ai == bj { 0 } else { 1 };
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
            // Damerau transposition.
            if i > 0 && j > 0 && a[i] == b[j - 1] && a[i - 1] == b[j] {
                // prev2 not tracked; skip the pure-Damerau optimization
                // and rely on the Levenshtein floor — we only care that
                // small typos like "DSEC"↔"DESC" are within ≤2.
            }
            let _ = cost;
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// Get a display name for the DBF type of a value.
fn dbf_type_name(val: &EpicsValue) -> &'static str {
    match val {
        EpicsValue::String(_) => "DBF_STRING",
        EpicsValue::Short(_) => "DBF_SHORT",
        EpicsValue::Float(_) => "DBF_FLOAT",
        EpicsValue::Enum(_) | EpicsValue::EnumWithChoices { .. } => "DBF_ENUM",
        EpicsValue::Char(_) => "DBF_CHAR",
        EpicsValue::Long(_) => "DBF_LONG",
        EpicsValue::Double(_) => "DBF_DOUBLE",
        EpicsValue::Int64(_) | EpicsValue::Int64Array(_) => "DBF_INT64",
        EpicsValue::UInt64(_) | EpicsValue::UInt64Array(_) => "DBF_UINT64",
        EpicsValue::UShort(_) | EpicsValue::UShortArray(_) => "DBF_USHORT",
        EpicsValue::ULong(_) | EpicsValue::ULongArray(_) => "DBF_ULONG",
        EpicsValue::UChar(_) | EpicsValue::UCharArray(_) => "DBF_UCHAR",
        EpicsValue::ShortArray(_) => "DBF_SHORT",
        EpicsValue::FloatArray(_) => "DBF_FLOAT",
        EpicsValue::EnumArray(_) => "DBF_ENUM",
        EpicsValue::DoubleArray(_) => "DBF_DOUBLE",
        EpicsValue::LongArray(_) => "DBF_LONG",
        EpicsValue::CharArray(_) => "DBF_CHAR",
        EpicsValue::StringArray(_) => "DBF_STRING",
    }
}

/// Split `EPICS_DB_INCLUDE_PATH` into individual paths.
///
/// Supports both `;`-separated (Windows convention) and `:`-separated (Unix convention)
/// path lists. When splitting on `:`, a single ASCII letter followed by `:` is treated
/// as a Windows drive letter and is NOT used as a split point.
fn split_db_paths(val: &str) -> Vec<std::path::PathBuf> {
    if val.contains(';') {
        return val
            .split(';')
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .collect();
    }
    let mut paths = Vec::new();
    let mut current = String::new();
    for ch in val.chars() {
        if ch == ':' {
            let is_drive = current.len() == 1
                && current
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_alphabetic())
                    .unwrap_or(false);
            if is_drive {
                current.push(':');
            } else {
                if !current.is_empty() {
                    paths.push(std::path::PathBuf::from(&current));
                    current.clear();
                }
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        paths.push(std::path::PathBuf::from(current));
    }
    paths
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
    use crate::server::database::PvDatabase;
    use crate::server::records::ai::AiRecord;
    use crate::types::EpicsValue;
    use std::sync::Arc;

    fn make_ctx() -> (Arc<PvDatabase>, CommandContext) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(PvDatabase::new());
        let handle = rt.handle().clone();
        let ctx = CommandContext::new(db.clone(), handle);
        // Leak the runtime so it stays alive for the test
        std::mem::forget(rt);
        (db, ctx)
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
        });

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbgf").unwrap();
        let tokens = vec!["TEMP".to_string()];
        let args = parse_args(&tokens, &cmd.args).unwrap();
        let result = cmd.handler.call(&args, &ctx);
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    #[test]
    fn test_dbgf_not_found() {
        let (_db, ctx) = make_ctx();

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbgf").unwrap();
        let tokens = vec!["NONEXISTENT".to_string()];
        let args = parse_args(&tokens, &cmd.args).unwrap();
        let result = cmd.handler.call(&args, &ctx);
        assert!(result.is_err());
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
    /// consults via `resolve_menu_string`. `"Async Soft Channel"` is a declared
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

    /// Different record type for the same name is fatal — mirrors C
    /// `dbLexRoutines.c:1173-1180` "record '%s' already exists, can't
    /// load %s record".
    #[test]
    fn test_db_load_records_different_type_duplicate_rejected() {
        use std::io::Write;
        let (db, ctx) = make_ctx();
        // Pre-register DUP:CM as an `ai`.
        ctx.block_on(async {
            db.add_record("DUP:CM", Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
        });
        let tmp = tempfile::Builder::new()
            .suffix(".db")
            .tempfile()
            .expect("tempfile");
        writeln!(
            tmp.as_file(),
            r#"
record(mbbo, "DUP:CM") {{
    field(ZRST, "Mono")
}}
"#
        )
        .expect("write tempfile");
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbLoadRecords").unwrap();
        let args = parse_args(&[tmp.path().to_string_lossy().to_string()], &cmd.args).unwrap();
        let result = cmd.handler.call(&args, &ctx);
        match result {
            Err(e) => assert!(
                e.contains("already exists, can't load mbbo"),
                "expected type-mismatch error; got {e}"
            ),
            Ok(_) => panic!("different-type duplicate must error, but call succeeded"),
        }
    }

    // R19-63 — the two record-creating iocsh commands, on either side of the
    // `iocInit` boundary. C gates both on `getIocState() != iocVoid`
    // (`dbLexRoutines.c:236` for every `.db` read, `dbStaticIocRegister.c:288`
    // for `dbCreateRecord`) and creates NOTHING once the IOC is running.
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
    fn load_records(ctx: &CommandContext, body: &str) -> Result<(), String> {
        use std::io::Write;
        let tmp = tempfile::Builder::new().suffix(".db").tempfile().unwrap();
        write!(tmp.as_file(), "{body}").unwrap();
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbLoadRecords").unwrap();
        let args = parse_args(&[tmp.path().to_string_lossy().to_string()], &cmd.args).unwrap();
        cmd.handler.call(&args, ctx).map(|_| ())
    }

    fn create_record(ctx: &CommandContext, name: &str) -> Result<(), String> {
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbCreateRecord").unwrap();
        let args = parse_args(&["ai".to_string(), name.to_string()], &cmd.args).unwrap();
        cmd.handler.call(&args, ctx).map(|_| ())
    }

    fn exists(db: &PvDatabase, ctx: &CommandContext, name: &str) -> bool {
        ctx.block_on(async { db.get_record(name).is_some() })
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

        assert_eq!(err, "IOC already initialized - No new records can be added");
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
    fn test_split_db_paths_unix() {
        let paths = split_db_paths("/opt/epics/db:/home/user/db");
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], std::path::PathBuf::from("/opt/epics/db"));
        assert_eq!(paths[1], std::path::PathBuf::from("/home/user/db"));
    }

    #[test]
    fn test_split_db_paths_windows_semicolon() {
        let paths = split_db_paths(r"C:\epics\db;D:\user\db");
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], std::path::PathBuf::from(r"C:\epics\db"));
        assert_eq!(paths[1], std::path::PathBuf::from(r"D:\user\db"));
    }

    #[test]
    fn test_split_db_paths_windows_colon_separator() {
        // st.cmd uses ':' separator even on Windows — must not split inside drive letter
        let paths = split_db_paths(r"C:\epics\db:D:\user\db");
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], std::path::PathBuf::from(r"C:\epics\db"));
        assert_eq!(paths[1], std::path::PathBuf::from(r"D:\user\db"));
    }

    #[test]
    fn test_split_db_paths_single() {
        let paths = split_db_paths("/opt/epics/db");
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], std::path::PathBuf::from("/opt/epics/db"));
    }

    #[test]
    fn test_split_db_paths_empty() {
        let paths = split_db_paths("");
        assert!(paths.is_empty());
    }

    #[test]
    fn test_parse_macro_string() {
        let macros = parse_macro_string("P=IOC:,R=TEMP");
        assert_eq!(macros.get("P").unwrap(), "IOC:");
        assert_eq!(macros.get("R").unwrap(), "TEMP");

        let empty = parse_macro_string("");
        assert!(empty.is_empty());
    }

    #[test]
    fn parse_macro_string_honors_quotes_escapes_whitespace() {
        // Quoted comma stays inside the value (macParseDefns parity):
        // raw split would tear this into `DESC="a` + a stray `b"`.
        let m = parse_macro_string(r#"DESC="a,b",P=IOC:"#);
        assert_eq!(m.get("DESC").unwrap(), "a,b");
        assert_eq!(m.get("P").unwrap(), "IOC:");

        // Escaped comma is a literal; the backslash is dropped.
        let m = parse_macro_string(r#"DESC=a\,b,P=IOC:"#);
        assert_eq!(m.get("DESC").unwrap(), "a,b");
        assert_eq!(m.get("P").unwrap(), "IOC:");

        // Whitespace around names and values is trimmed; quoted names
        // and quoted/escaped names round-trip.
        let m = parse_macro_string(r#" P = IOC: , "R" = TEMP "#);
        assert_eq!(m.get("P").unwrap(), "IOC:");
        assert_eq!(m.get("R").unwrap(), "TEMP");

        // Quoted whitespace inside a value is preserved.
        let m = parse_macro_string(r#"MSG="a b c""#);
        assert_eq!(m.get("MSG").unwrap(), "a b c");

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
        cmd.handler.call(&args, ctx).map(|_| ())
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
    fn db_load_template_expands_rows_with_per_row_macros() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
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

    /// `globalMacros` applies to every row; a row-level macro of the same
    /// name overrides the global. Grounded in the reused loader:
    /// `load_substitution_file` (substitution.rs:449) inserts the caller
    /// macros (the command's `globalMacros`) first, then each row's macros
    /// into a last-definition-wins map, so the row wins — the C
    /// `dbLoadTemplate` precedence (see the loader-level regression
    /// `load_substitution_file_caller_macros_overridden_by_row`).
    #[test]
    fn db_load_template_global_macros_apply_and_row_overrides() {
        let (db, ctx) = make_ctx();
        let dir = tempfile::tempdir().unwrap();
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
    fn db_load_template_parity_with_hand_written_db_load_records() {
        let dir = tempfile::tempdir().unwrap();
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
}
