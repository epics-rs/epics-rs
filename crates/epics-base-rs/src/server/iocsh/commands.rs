use std::collections::HashMap;

use super::registry::*;
use crate::error::CaResult;
use crate::server::database::parse_pv_name;
use crate::server::db_loader;
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
    registry.register(cmd_ioc_stats());
    registry.register(cmd_db_load_records());
    registry.register(cmd_db_create_record());
    registry.register(cmd_db_delete_record());
    registry.register(cmd_epics_env_set());
    registry.register(cmd_pushd());
    registry.register(cmd_popd());
    registry.register(cmd_dirs());
    registry.register(cmd_ioc_init());
    registry.register(cmd_after_ioc_running());
    registry.register(cmd_exit());
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
                ctx.println(&format!(
                    "dbDeleteRecord: no record named '{name}'"
                ));
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
                    let rec = ctx.block_on(ctx.db().get_record(name));
                    if let Some(rec) = rec {
                        let inst = ctx.block_on(rec.read());
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

            match ctx.block_on(ctx.db().get_pv(name)) {
                Ok(val) => {
                    let type_name = dbf_type_name(&val);
                    ctx.println(&format!("{type_name}: {val}"));
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
                if let Some(rec) = ctx.db().get_record(base).await {
                    let inst = rec.read().await;
                    // Check record-specific fields
                    if let Some(desc) = inst.record.field_list().iter().find(|f| f.name == field) {
                        return Some(desc.dbf_type);
                    }
                    // Common field types
                    return common_field_dbf_type(&field);
                }
                None
            });

            let value = if let Some(dbf) = dbf_type {
                EpicsValue::parse(dbf, value_str)
                    .map_err(|e| format!("cannot parse '{value_str}' as {dbf:?}: {e}"))?
            } else {
                // No type info available, try as string
                EpicsValue::String(value_str.clone())
            };

            // Use put_record_field_from_ca for records (triggers process like CA put).
            // Fall back to put_pv for simple PVs.
            let put_result: CaResult<()> = ctx.block_on(async {
                let db = ctx.db();
                if db.get_record(base).await.is_some() {
                    db.put_record_field_from_ca(base, &field, value)
                        .await
                        .map(|_| ())
                } else {
                    db.put_pv(name, value).await
                }
            });
            put_result.map_err(|e| {
                // Round-23: epics-base PR #689 — when the field
                // doesn't exist, suggest near-by names so a typo
                // ("DSEC" instead of "DESC") is caught quickly.
                let msg = format!("{e}");
                if msg.contains("FieldNotFound") || msg.contains(&format!("'{field}'")) {
                    if let Some(suggestion) = ctx.block_on(suggest_field_name(ctx.db(), base, &field)) {
                        return format!("{msg}; did you mean '{suggestion}'?");
                    }
                }
                msg
            })?;

            // Read back to confirm
            match ctx.block_on(ctx.db().get_pv(name)) {
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
                .block_on(ctx.db().get_record(name))
                .ok_or_else(|| format!("record '{}' not found", name))?;

            // Collect field values inside lock, format outside
            let fields: Vec<(String, String)> = ctx.block_on(async {
                let inst = rec.read().await;
                let mut fields = Vec::new();

                // Level 0: NAME, RTYP, VAL (+ alias names if any —
                // base's dbpr surfaces aliases here so admins know
                // every spelling that resolves to this record).
                fields.push(("NAME".to_string(), inst.name.clone()));
                let aliases = ctx.db().aliases_for_record(&inst.name).await;
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
                    fields.push(("PINI".to_string(), format!("{}", inst.common.pini)));
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
                    // Round-20: surface info(...) tags so admins can
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

/// Shared handler used by `dbsr`, `dbglob`, and `dbgrep`. Mirrors
/// epics-base PR #626 (rename `dbgrep` → `dbglob` with alias) and
/// PR #613 (add fields argument). The `fields` argument is comma-
/// separated; when present each matching record additionally dumps
/// the listed field values.
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

    // Walk both record names and aliases — base's dbFirstRecord
    // iteration includes alias-form entries, so dbsr / dbgrep /
    // dbglob must too. Field lookup via `get_record` already follows
    // the alias to the canonical record (round 7).
    let mut names = ctx.block_on(ctx.db().all_record_names());
    names.extend(ctx.block_on(ctx.db().all_alias_names()));
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
        if let Some(rec_arc) = ctx.block_on(ctx.db().get_record(name)) {
            let inst = ctx.block_on(rec_arc.read());
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

fn cmd_dbsr() -> CommandDef {
    CommandDef::new(
        "dbsr",
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
        "dbsr [pattern] [fields] — Search records by name pattern; \
         optional comma-separated fields list dumps each value",
        dbsr_handler,
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
        "dbglob [pattern] [fields] — Alias of dbsr (epics-base PR #626)",
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
        "dbgrep [pattern] [fields] — Legacy alias retained per epics-base PR #626",
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

/// `dbCreateRecord <type> <name>` — create a new record at runtime.
///
/// Mirrors epics-base PR #812. Validates the name with the same rules
/// as `parse_db` (PR #78), refuses duplicate names, and routes the
/// instantiation through the same factory registry as `dbLoadRecords`.
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
        "dbCreateRecord <type> <name> — Create a new record of <type> at runtime",
        |args: &[ArgValue], ctx: &CommandContext| {
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
            if ctx.block_on(ctx.db().get_record(&name)).is_some() {
                ctx.println(&format!(
                    "dbCreateRecord: record '{name}' already exists"
                ));
                return Ok(CommandOutcome::Continue);
            }
            let record = match db_loader::create_record(&rec_type) {
                Ok(r) => r,
                Err(e) => {
                    ctx.println(&format!("dbCreateRecord: {e}"));
                    return Ok(CommandOutcome::Continue);
                }
            };
            ctx.block_on(ctx.db().add_record(&name, record));
            ctx.println(&format!(
                "dbCreateRecord: created '{name}' ({rec_type})"
            ));
            Ok(CommandOutcome::Continue)
        },
    )
}

fn cmd_post_event() -> CommandDef {
    CommandDef::new(
        "post_event",
        vec![],
        "post_event — Process all records with SCAN=Event",
        |_args: &[ArgValue], ctx: &CommandContext| {
            ctx.block_on(ctx.db().post_event());
            ctx.println("Event scan processed");
            Ok(CommandOutcome::Continue)
        },
    )
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
                    pat.next();
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

            let macros = parse_macro_string(macros_str);

            // Build include config from EPICS_DB_INCLUDE_PATH
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

            // Resolve the template file path: check if it exists directly,
            // otherwise search EPICS_DB_INCLUDE_PATH (matching C dbLoadRecords behavior)
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
            let mut defs = db_loader::parse_db_file(&file_path, &macros, &config)
                .map_err(|e| format!("parse error: {e}"))?;

            // DTYP override: if macros contain DTYP, override existing DTYP fields
            if let Some(dtyp) = macros.get("DTYP") {
                db_loader::override_dtyp(&mut defs, dtyp);
            }

            let count = defs.len();

            for def in defs {
                let mut record =
                    db_loader::create_record(&def.record_type).map_err(|e| format!("{e}"))?;
                let mut common_fields = Vec::new();
                db_loader::apply_fields(&mut record, &def.fields, &mut common_fields)
                    .map_err(|e| format!("{e}"))?;

                ctx.block_on(async {
                    ctx.db().add_record(&def.name, record).await;

                    // Register any aliases declared in the record body
                    // (epics-base PR #336). Failures are reported but
                    // don't abort the load — the record is already in.
                    for alias in &def.aliases {
                        if let Err(e) = ctx.db().add_alias(alias, &def.name).await {
                            eprintln!(
                                "dbLoadRecords: alias '{alias}' for '{}' rejected: {e}",
                                def.name
                            );
                        }
                    }

                    if let Some(rec_arc) = ctx.db().get_record(&def.name).await {
                        let mut instance = rec_arc.write().await;
                        // info(key, value) directives — last write
                        // wins. Populated before common-field application
                        // so device support seeing `init_record` can
                        // observe info tags.
                        for (k, v) in &def.info_tags {
                            instance.set_info(k, v);
                        }
                        for (name, value) in common_fields {
                            use crate::server::record::CommonFieldPutResult;
                            match instance.put_common_field(&name, value) {
                                Ok(CommonFieldPutResult::ScanChanged {
                                    old_scan,
                                    new_scan,
                                    phas,
                                }) => {
                                    drop(instance);
                                    ctx.db()
                                        .update_scan_index(
                                            &def.name, old_scan, new_scan, phas, phas,
                                        )
                                        .await;
                                    instance = rec_arc.write().await;
                                }
                                Ok(CommonFieldPutResult::PhasChanged {
                                    scan,
                                    old_phas,
                                    new_phas,
                                }) => {
                                    drop(instance);
                                    ctx.db()
                                        .update_scan_index(
                                            &def.name, scan, scan, old_phas, new_phas,
                                        )
                                        .await;
                                    instance = rec_arc.write().await;
                                }
                                Ok(CommonFieldPutResult::NoChange) => {}
                                Err(e) => {
                                    eprintln!(
                                        "put_common_field({name}) failed for {}: {e}",
                                        def.name
                                    );
                                }
                            }
                        }
                        // TODO: refactor to global two-pass if inter-record init dependencies arise
                        if let Err(e) = instance.record.init_record(0) {
                            eprintln!("init_record(0) failed for {}: {e}", def.name);
                        }
                        if let Err(e) = instance.record.init_record(1) {
                            eprintln!("init_record(1) failed for {}: {e}", def.name);
                        }
                    }
                });
            }

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
        "iocInit - Initialize the IOC (handled automatically by IocApplication)",
        |_args: &[ArgValue], ctx: &CommandContext| {
            ctx.println("iocInit: skipped (handled automatically after script execution)");
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

/// Parse a macro string like "P=IOC:,R=TEMP" into a HashMap.
/// Macro values may reference environment variables via `$(ENVVAR)`.
fn parse_macro_string(s: &str) -> HashMap<String, String> {
    let mut macros = HashMap::new();
    if s.is_empty() {
        return macros;
    }
    for pair in s.split(',') {
        if let Some((k, v)) = pair.split_once('=') {
            macros.insert(
                k.trim().to_string(),
                super::registry::substitute_env_vars(v.trim()),
            );
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
    let rec = db.get_record(record_name).await?;
    let inst = rec.read().await;
    let mut candidates: Vec<&str> = inst.record.field_list().iter().map(|d| d.name).collect();
    // Common dbCommon fields are also valid PUT targets.
    candidates.extend([
        "VAL", "DESC", "EGU", "SCAN", "PINI", "DTYP", "INP", "OUT", "FLNK", "NAME", "RTYP",
        "PHAS", "PRIO", "DISA", "DISV", "DISS", "DISP", "PROC", "ASG", "TPRO", "TSE", "TSEL",
        "UDF", "SEVR", "STAT", "AMSG",
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
            curr[j + 1] = (curr[j] + 1)
                .min(prev[j + 1] + 1)
                .min(prev[j] + cost);
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
        EpicsValue::Enum(_) => "DBF_ENUM",
        EpicsValue::Char(_) => "DBF_CHAR",
        EpicsValue::Long(_) => "DBF_LONG",
        EpicsValue::Double(_) => "DBF_DOUBLE",
        EpicsValue::Int64(_) | EpicsValue::Int64Array(_) => "DBF_INT64",
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
        "SEVR" | "STAT" => Some(DbFieldType::Short),
        "UDF" | "PINI" | "TPRO" => Some(DbFieldType::Char),
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
            db.add_record("REC_A", Box::new(AiRecord::new(1.0))).await;
            db.add_record("REC_B", Box::new(AiRecord::new(2.0))).await;
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
            db.add_record("TEMP", Box::new(AiRecord::new(25.0))).await;
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

    #[test]
    fn test_dbpf_and_readback() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("TEMP", Box::new(AiRecord::new(0.0))).await;
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
        let val = ctx.block_on(db.get_pv("TEMP")).unwrap();
        match val {
            EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
            other => panic!("expected Double(42.0), got {:?}", other),
        }
    }

    #[test]
    fn test_dbpr_levels() {
        let (db, ctx) = make_ctx();
        ctx.block_on(async {
            db.add_record("TEMP", Box::new(AiRecord::new(25.0))).await;
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
            db.add_record("AI_REC", Box::new(AiRecord::new(1.0))).await;
            db.add_record(
                "BO_REC",
                Box::new(crate::server::records::bo::BoRecord::new(0)),
            )
            .await;
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
}
