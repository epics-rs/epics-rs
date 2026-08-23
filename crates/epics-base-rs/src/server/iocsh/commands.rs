// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that hand-builds its own tokio runtime; runs and passes in the feature-ON suite.
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
    // Last: C registers `var` from `iocshRegisterVariable`, so it must
    // come after everything that contributes to the variable table.
    super::vars::register(registry);
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
/// (`dbTest.c:181-191`, `:322-341`): `dbFirstRecordType` selects a
/// record type and `dbFirstRecord`/`dbNextRecord` then walk that
/// type's own list — the order its records were loaded — before the
/// next type is touched. Sorting every name once interleaves the
/// types, which C never does, and throws away the load order
/// [`PvDatabase::all_record_names`](crate::server::database::PvDatabase::all_record_names) preserves.
///
/// The type sequence is `dbd_generated::RECORD_TYPES`, which the
/// generator emits in name order; C's is `recordTypeList`, the order
/// the loaded `.dbd` declared them in. The port has no per-database
/// declaration order to read, so the grouping is C's and the sequence
/// of the groups is the table's. A type registered at runtime is not
/// in the table and follows those that are, by name.
fn record_names_type_major(ctx: &CommandContext) -> Vec<String> {
    use crate::server::record::dbd_generated::RECORD_TYPES;

    let mut names = ctx.block_on(ctx.db().all_record_names());
    let rank = |name: &String| {
        let record_type = ctx
            .db()
            .get_record(name)
            .map(|rec| rec.read().record.record_type().to_string())
            .unwrap_or_default();
        match RECORD_TYPES.iter().position(|t| *t == record_type) {
            Some(i) => (0usize, i, String::new()),
            None => (1usize, 0, record_type),
        }
    };
    // Stable: records of one type keep the load order they arrived in.
    names.sort_by_cached_key(rank);
    names
}

fn cmd_dbl() -> CommandDef {
    CommandDef::new(
        "dbl",
        vec![
            ArgDesc {
                name: "record type",
                arg_type: ArgType::String,
                optional: true,
            },
            ArgDesc {
                name: "fields",
                arg_type: ArgType::String,
                optional: true,
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

            let names = record_names_type_major(ctx);

            // C walks the record types and reports an unknown one
            // before listing anything (`dbTest.c:172-180`).
            if let Some(filter) = type_filter {
                let known = names.iter().any(|name| {
                    ctx.db()
                        .get_record(name)
                        .is_some_and(|rec| rec.read().record.record_type() == filter)
                });
                if !known {
                    ctx.println("No record type");
                    return Ok(CommandOutcome::Continue);
                }
            }

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
                print_fields_list(ctx, name, &fields);
            }

            Ok(CommandOutcome::Continue)
        },
    )
}

/// C `dbpr_msgOut`'s `TAB_BUFFER` (`dbTest.c:1281-1370`) at
/// `tab_size` 10 — the layout `dbgf` puts every message through.
/// Each inserted message is padded out to the next 10-column stop, and
/// the buffer is flushed as one line when the next message would carry
/// it past `MAXLINE`. Reproducing the buffer rather than formatting a
/// line directly is what keeps a multi-element array wrapping where C
/// wraps it.
struct TabBuffer {
    out: String,
    next_tab: usize,
    lines: Vec<String>,
}

impl TabBuffer {
    const MAXLINE: usize = 80;
    const TAB: usize = 10;

    fn new() -> Self {
        Self {
            out: String::new(),
            next_tab: Self::TAB,
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
                self.next_tab += Self::TAB;
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
        self.next_tab = Self::TAB;
    }

    fn finish(mut self) -> Vec<String> {
        self.flush();
        self.lines
    }
}

/// The DBR type name and per-element renderings C `printBuffer`
/// (`dbTest.c:986-1150`) emits for a value. `dbgf` re-reads a DBR_ENUM
/// field as DBR_STRING first (`dbTest.c:371-380`), so an enum reports
/// itself as `DBF_STRING` and carries its choice text.
fn dbgf_dbr_render(val: &EpicsValue) -> (&'static str, Vec<String>) {
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

    match val {
        EpicsValue::String(v) => ("STRING", vec![quoted(v.as_bytes())]),
        EpicsValue::StringArray(v) => (
            "STRING",
            v.iter().map(|s| quoted(s.as_bytes())).collect::<Vec<_>>(),
        ),
        // DBR_ENUM read back as DBR_STRING.
        EpicsValue::Enum(v) => ("STRING", vec![quoted(v.to_string().as_bytes())]),
        EpicsValue::EnumWithChoices { index, choices } => (
            "STRING",
            vec![quoted(
                &choices
                    .get(*index as usize)
                    .map(|c| c.as_bytes().to_vec())
                    .unwrap_or_else(|| index.to_string().into_bytes()),
            )],
        ),
        EpicsValue::EnumArray(v) => (
            "STRING",
            v.iter()
                .map(|e| quoted(e.to_string().as_bytes()))
                .collect::<Vec<_>>(),
        ),
        // `%d = 0x%x`, plus the printable character when scalar
        // (`dbTest.c:1015-1023`).
        EpicsValue::Char(v) => (
            "CHAR",
            vec![{
                let val = *v as i32;
                if (0x20..0x7f).contains(&(*v as u8 as i32)) {
                    format!("{val} = 0x{:x} = '{}'", *v as u8, *v as u8 as char)
                } else {
                    format!("{val} = 0x{:x}", *v as u8)
                }
            }],
        ),
        // A CHAR array is one escaped, quoted string (`:1024-1041`).
        EpicsValue::CharArray(v) => (
            "CHAR",
            vec![format!("\"{}\"", escape_char_array_for_dbgf(v))],
        ),
        EpicsValue::UChar(v) => ("UCHAR", vec![format!("{v} = 0x{v:x}")]),
        EpicsValue::UCharArray(v) => (
            "UCHAR",
            v.iter().map(|e| format!("{e} = 0x{e:x}")).collect(),
        ),
        EpicsValue::Short(v) => ("SHORT", vec![i16_hex(*v)]),
        EpicsValue::ShortArray(v) => ("SHORT", v.iter().map(|e| i16_hex(*e)).collect()),
        EpicsValue::UShort(v) => ("USHORT", vec![format!("{v} = 0x{v:x}")]),
        EpicsValue::UShortArray(v) => (
            "USHORT",
            v.iter().map(|e| format!("{e} = 0x{e:x}")).collect(),
        ),
        EpicsValue::Long(v) => ("LONG", vec![i32_hex(*v)]),
        EpicsValue::LongArray(v) => ("LONG", v.iter().map(|e| i32_hex(*e)).collect()),
        EpicsValue::ULong(v) => ("ULONG", vec![format!("{v} = 0x{v:x}")]),
        EpicsValue::ULongArray(v) => (
            "ULONG",
            v.iter().map(|e| format!("{e} = 0x{e:x}")).collect(),
        ),
        EpicsValue::Int64(v) => ("INT64", vec![format!("{v} = 0x{:x}", *v as u64)]),
        EpicsValue::Int64Array(v) => (
            "INT64",
            v.iter()
                .map(|e| format!("{e} = 0x{:x}", *e as u64))
                .collect(),
        ),
        EpicsValue::UInt64(v) => ("UINT64", vec![format!("{v} = 0x{v:x}")]),
        EpicsValue::UInt64Array(v) => (
            "UINT64",
            v.iter().map(|e| format!("{e} = 0x{e:x}")).collect(),
        ),
        EpicsValue::Float(v) => ("FLOAT", vec![fmt_g(*v as f64, 6, false, false)]),
        EpicsValue::FloatArray(v) => (
            "FLOAT",
            v.iter()
                .map(|e| fmt_g(*e as f64, 6, false, false))
                .collect(),
        ),
        EpicsValue::Double(v) => ("DOUBLE", vec![fmt_g(*v, 12, false, false)]),
        EpicsValue::DoubleArray(v) => (
            "DOUBLE",
            v.iter().map(|e| fmt_g(*e, 12, false, false)).collect(),
        ),
    }
}

/// The lines C `dbgf` prints for a value: the `DBF_<T>:` header
/// (`dbTest.c:986-992`) followed by the element renderings, all laid
/// out through the tab buffer.
fn dbgf_lines(val: &EpicsValue) -> Vec<String> {
    let (dbr, values) = dbgf_dbr_render(val);
    let mut buf = TabBuffer::new();
    // A CHAR array is a single string element to C's printBuffer, but
    // its `no_elements` is still the byte count.
    let count = match val {
        EpicsValue::CharArray(v) => v.len(),
        _ => values.len(),
    };
    if count == 1 {
        buf.insert(&format!("DBF_{dbr}: "));
    } else {
        buf.insert(&format!("DBF_{dbr}[{count}]: "));
        if count == 0 {
            buf.insert("(empty)");
        }
    }
    for v in &values {
        buf.insert(v);
    }
    buf.finish()
}

/// C `nameToAddr` (`dbTest.c:787-795`) — the one place every
/// `dbTest.c` command reports a name it cannot resolve. C prints this
/// line on stdout and the caller then returns -1 without printing
/// anything else, so the port must not route it through the shell's
/// error channel, which writes to stderr and prefixes `Error:`.
fn print_pv_not_found(ctx: &CommandContext, pname: &str) {
    ctx.println(&format!("PV '{pname}' not found"));
}

fn cmd_dbgf() -> CommandDef {
    CommandDef::new(
        "dbgf",
        vec![ArgDesc {
            name: "record name",
            arg_type: ArgType::String,
            optional: true,
        }],
        "dbgf record name - Get field value",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `dbTest.c:358-361`.
            let name = match args.first() {
                Some(ArgValue::String(s)) if !s.is_empty() => s,
                _ => {
                    ctx.println("Usage: dbgf \"pv name\"");
                    return Ok(CommandOutcome::Continue);
                }
            };

            match ctx.db().get_pv(name) {
                Ok(val) => {
                    for line in dbgf_lines(&val) {
                        ctx.println(&line);
                    }
                }
                Err(_) => print_pv_not_found(ctx, name),
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
                name: "pvname",
                arg_type: ArgType::String,
                optional: true,
            },
            ArgDesc {
                name: "value",
                arg_type: ArgType::String,
                optional: true,
            },
        ],
        "dbpf pvname value - Put field value",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `dbTest.c:400-403`: a missing or empty name, or a
            // missing value, is a usage line on stdout.
            let (name, value_str) = match (&args[0], &args[1]) {
                (ArgValue::String(n), ArgValue::String(v)) if !n.is_empty() => (n, v),
                _ => {
                    ctx.println("Usage: dbpf \"pv name\", \"value\"");
                    return Ok(CommandOutcome::Continue);
                }
            };

            // C resolves the name before it puts (`dbTest.c:405-406`),
            // so an unknown PV never reaches `dbPutField`.
            if ctx.db().get_pv(name).is_err() {
                print_pv_not_found(ctx, name);
                return Ok(CommandOutcome::Continue);
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
            let put_err = put_result.err().map(|e| {
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
            });

            // C `dbpf` ends with `dbgf(pname)` (`dbTest.c:433`) whatever
            // `dbPutField` returned, so the read-back is that one
            // printer rather than a second rendering, and a rejected
            // put still shows the value the record kept.
            if let Ok(val) = ctx.db().get_pv(name) {
                for line in dbgf_lines(&val) {
                    ctx.println(&line);
                }
            }

            match put_err {
                Some(msg) => Err(msg),
                None => Ok(CommandOutcome::Continue),
            }
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
                optional: true,
            },
            ArgDesc {
                name: "level",
                arg_type: ArgType::Int,
                optional: true,
            },
        ],
        "dbpr record [level] - Print record fields (level 0-2)",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `dbTest.c:445-448`: a missing or empty name is a usage
            // line on stdout.
            let name = match &args[0] {
                ArgValue::String(s) if !s.is_empty() => s,
                _ => {
                    ctx.println("Usage: dbpr \"pv name\", level");
                    return Ok(CommandOutcome::Continue);
                }
            };
            let level = match &args[1] {
                ArgValue::Int(n) => *n as i32,
                ArgValue::Missing => 0,
                _ => 0,
            };

            let rec = match ctx.db().get_record(name) {
                Some(rec) => rec,
                None => {
                    print_pv_not_found(ctx, name);
                    return Ok(CommandOutcome::Continue);
                }
            };

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

/// Shared handler for the record-name glob search `dbglob` / `dbgrep`
/// (C `dbglob`, `dbTest.c:298-345`; `dbgrep` tail-calls it).
/// Mirrors epics-base PR #626 (rename `dbgrep` → `dbglob` with alias)
/// and PR #613 (add fields argument). The `fields` argument is
/// SPACE-separated (`splitFieldsList`) and each matching record is one
/// line built by `printFieldsList`. (`dbsr` is the *server report* — a
/// separate command — not this name search.)
fn dbglob_handler(args: &[ArgValue], ctx: &CommandContext) -> CommandResult {
    // C `dbTest.c:306-309`: a missing or empty pattern is a usage
    // error, not an implicit `*`.
    let pattern = match args.first() {
        Some(ArgValue::String(s)) if !s.is_empty() => s.as_str(),
        _ => {
            ctx.println("Usage: dbglob \"pattern\" \"fields\"");
            return Ok(CommandOutcome::Continue);
        }
    };
    let fields: Vec<String> = match args.get(1) {
        Some(ArgValue::String(s)) => split_fields_list(s),
        _ => Vec::new(),
    };

    // Walk record names + aliases + simple PVs. Base's
    // `dbFirstRecord` iteration only sees records, but our
    // PvDatabase also serves `add_pv`-registered simple PVs (CA
    // gateway shadows, IOC-stat scratchpads). A user globbing for
    // every channel name would be confused if simple PVs were
    // hidden. Field lookup via `get_record` follows alias→canonical;
    // for simple PVs the field-dump branch silently
    // skips since they're not records.
    let mut names = record_names_type_major(ctx);
    let mut extra: Vec<String> = ctx.db().all_alias_names();
    extra.extend(ctx.block_on(ctx.db().all_simple_pv_names()));
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
        dbglob_handler,
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
        dbglob_handler,
    )
}

fn cmd_scanppl() -> CommandDef {
    CommandDef::new(
        "scanppl",
        vec![ArgDesc {
            name: "rate",
            arg_type: ArgType::Double,
            optional: true,
        }],
        "scanppl [rate] — Print periodic scan lists, optionally just one rate",
        |args: &[ArgValue], ctx: &CommandContext| {
            use crate::server::record::ScanType;
            // C `scanppl` (`dbScan.c:394-405`): a positive rate selects
            // the one periodic list whose period is within 0.05 s; 0 or
            // no argument prints them all.
            let rate = match args.first() {
                Some(ArgValue::Double(r)) if *r > 0.0 => Some(*r),
                _ => None,
            };
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
                if let Some(rate) = rate {
                    let matches = st
                        .interval()
                        .is_some_and(|d| (rate - d.as_secs_f64()).abs() <= 0.05);
                    if !matches {
                        continue;
                    }
                }
                let names = ctx.block_on(ctx.db().records_for_scan(*st));
                if !names.is_empty() {
                    // C prints the list's cumulative over-run count in the
                    // header — `Records with SCAN = '%s' (%lu over-runs):`
                    // (`dbScan.c:408-409`). It is the observable half of the
                    // over-run rule: without it a list that keeps missing its
                    // deadline looks identical to one that never does. Only
                    // periodic rates have the counter; C keeps it on
                    // `periodic_scan_list`, and the event and passive lists
                    // are not one.
                    let overruns = crate::server::scan::PERIODIC_SCANS
                        .contains(st)
                        .then(|| st.scan_list().map(|l| ctx.db().scan_overruns(l)))
                        .flatten();
                    match overruns {
                        Some(n) => {
                            ctx.println(&format!("{st}: {} records ({n} over-runs)", names.len()))
                        }
                        None => ctx.println(&format!("{st}: {} records", names.len())),
                    }
                    for name in &names {
                        ctx.println(&format!("  {name}"));
                    }
                }
            }

            let io_count = ctx
                .block_on(ctx.db().records_for_scan(ScanType::IoIntr))
                .len();
            if rate.is_none() && io_count > 0 {
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

/// `popd` — pop the top of the directory stack and `cd` to it.
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
                name: "pdbbase",
                arg_type: ArgType::String,
                optional: true,
            },
            ArgDesc {
                name: "recordType",
                arg_type: ArgType::String,
                optional: true,
            },
            ArgDesc {
                name: "recordName",
                arg_type: ArgType::String,
                optional: true,
            },
        ],
        "dbCreateRecord pdbbase <type> <name> — Create a new record of <type> (before iocInit)",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `dbStaticIocRegister.c:36-42` declares the leading
            // `iocshArgPdbbase`, and `cvtArg` (`iocsh.cpp:878-890`)
            // accepts it missing, starting with `0`, or spelled
            // `pdbbase`, refusing anything else.
            if let ArgValue::String(pdbbase) = &args[0]
                && !(pdbbase.is_empty() || pdbbase.starts_with('0') || pdbbase == "pdbbase")
            {
                return Err(format!("Expecting 'pdbbase' got '{pdbbase}'."));
            }
            // Creating a record IS entering the load phase — the record's links
            // are classified by `iocInit`, with the rest of the database. Once
            // `iocInit` has run there is no phase to enter and C refuses.
            if let Err(e) = ctx.db().begin_load() {
                return Err(e.to_string());
            }
            // C `dbStaticIocRegister.c:292-296` asks for the NAME
            // first and counts an empty one as missing
            // (`S_dbLib_recordNameMissing`), reaching
            // `S_dbLib_recordTypeNotFound` only once a name is in
            // hand. Asking about the type first made a bare
            // `dbCreateRecord pdbbase` complain about the argument the
            // operator was not being asked for.
            let name = match &args[2] {
                ArgValue::String(s) if !s.is_empty() => s.clone(),
                _ => return Err("Record name is required".to_string()),
            };
            let rec_type = match &args[1] {
                ArgValue::String(s) => s.clone(),
                _ => return Err("Record Type does not exist".to_string()),
            };
            // Failures return Err so `on error` sees them — current C
            // base wraps exactly these in `iocshSetError`
            // (dbStaticIocRegister.c:282-310); epics-base#498 / UI-105.
            if let Err(e) = db_loader::validate_record_name(&name, 0, 0) {
                return Err(format!("dbCreateRecord: {e}"));
            }
            if ctx.db().get_record(&name).is_some() {
                return Err(format!("dbCreateRecord: record '{name}' already exists"));
            }
            let record =
                db_loader::create_record(&rec_type).map_err(|e| format!("dbCreateRecord: {e}"))?;
            if let Err(e) = ctx.block_on(ctx.db().add_record(&name, record)) {
                return Err(format!("dbCreateRecord: {e}"));
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
            name: "event name",
            arg_type: ArgType::String,
            optional: true,
        }],
        "postEvent <event name> — Manually scan all records with EVNT == name.",
        post_event_handler,
    )
}

fn cmd_post_event_alias() -> CommandDef {
    CommandDef::new(
        "post_event",
        vec![ArgDesc {
            name: "event name",
            arg_type: ArgType::String,
            optional: true,
        }],
        "post_event <event name> — Back-compat alias of postEvent",
        post_event_handler,
    )
}

fn post_event_handler(args: &[ArgValue], ctx: &CommandContext) -> CommandResult {
    // C `postEventCallFunc` (`dbIocRegister.c:472-480`) is
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
/// match. `dbglob`'s help text (`dbIocRegister.c:246-248`) says "0 or
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
                optional: true,
            },
            ArgDesc {
                name: "macros",
                arg_type: ArgType::String,
                optional: true,
            },
        ],
        "dbLoadRecords file [macros] - Load records from a .db/.template file",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `dbAccess.c:800-803` tests the file name only for NULL,
            // so an empty name still reaches the open and fails there.
            let path = match &args[0] {
                ArgValue::String(s) => s,
                _ => {
                    ctx.println("Usage: dbLoadRecords \"file\", \"subs\"");
                    return Ok(CommandOutcome::Continue);
                }
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

            let (config, file_path) = resolve_db_file(path);
            // C `dbLoadRecords` macros are pure text substitution (dbLexRoutines.c
            // → macLib): a `DTYP=` macro reaches a record only where the file wrote
            // `field(DTYP,"$(DTYP)")`. It does NOT rewrite a record that spells its
            // DTYP literally. `parse_db_file_with_breaktables` already performs that
            // substitution, so there is nothing further to do for DTYP here.
            let parsed = db_loader::parse_db_file_with_breaktables(&file_path, &macros, &config)
                .map_err(|e| format!("parse error: {e}"))?;

            // Merge any `breaktable(...)` definitions into the database's shared
            // breakpoint-table registry (C `bptList`) and snapshot it for the
            // records loaded by this command. A record resolves a table loaded
            // by an earlier or the same `dbLoadRecords` (C ordering).
            let breaktable_registry =
                ctx.block_on(async { ctx.db().add_breaktables(parsed.breaktables).await });

            let count = parsed.records.len();

            // One install path for both `dbLoadRecords` and `dbLoadTemplate`:
            // each expanded record flows through the SAME per-record routine,
            // so template-loaded records are indistinguishable from directly
            // loaded ones.
            ctx.block_on(install_record_defs(
                ctx,
                parsed.records,
                parsed.unresolved_aliases,
                &breaktable_registry,
                parsed.faults,
            ))?;

            ctx.println(&format!("Loaded {count} record(s) from {path}"));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Build the DB include config from `EPICS_DB_INCLUDE_PATH`, the only
/// source `dbReadCOM` (`dbLexRoutines.c:244-253`) has for a
/// `dbLoadRecords` call. C installs the variable through `dbPath` and
/// falls back to `"."` when it is unset, so both spellings of "no
/// list" reach the same one-entry search path.
fn db_load_config() -> db_loader::DbLoadConfig {
    db_loader::DbLoadConfig {
        include_paths: std::env::var("EPICS_DB_INCLUDE_PATH").map_or_else(
            |_| vec![std::path::PathBuf::from(".")],
            |val| db_loader::db_path(&val),
        ),
        max_include_depth: 32,
    }
}

/// Resolve a `dbLoadRecords` file name through C `dbOpenFile`: the path
/// list is searched FIRST and the process CWD is never consulted for a
/// bare name. An unresolved name is handed back unchanged so the open
/// that follows produces the "cannot read" diagnostic C prints.
fn resolve_db_file(path: &str) -> (db_loader::DbLoadConfig, std::path::PathBuf) {
    let config = db_load_config();
    let file_path = db_loader::db_open_file(path, &config.include_paths)
        .unwrap_or_else(|| std::path::PathBuf::from(path));
    (config, file_path)
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
    let config = db_load_config();
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
    mut faults: db_loader::DbFaults,
) -> Result<(), String> {
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
        if def.record_type == "*" {
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
                    faults.recoverable(format!("ERROR: Record '{}' not found", def.name));
                    continue;
                }
            }
        }
        if def.record_type == "#" {
            if !ctx.db().remove_record(&def.name).await {
                // C also names the file and line here; the port does not
                // carry either as far as the install loop.
                eprintln!("WARNING: Record '{}' not found, can't delete", def.name);
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
            // (`dbLexRoutines.c:1173-1180`). `dbRecordsOnceOnly`
            // global is not yet wired; tighten here if/when needed.
            let existing = if let Some(rec) = ctx.db().get_record(&def.name) {
                let r = rec.read();
                let existing_type = r.record.record_type();
                if existing_type != def.record_type {
                    return Err(RecordFault::Recoverable(format!(
                        "ERROR: {} record '{}' already exists, can't load {} record",
                        def.record_type, def.name, existing_type
                    )));
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
                if let Err(e) = ctx.db().add_alias(alias, &def.name).await {
                    faults.recoverable(format!(
                        "dbLoadRecords: alias '{alias}' for '{}' rejected: {e}",
                        def.name
                    ));
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
                return Err(msg);
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
        let msg = if ctx.db().get_record(&target).is_none() {
            db_loader::unknown_alias_message(&alias, &target)
        } else if let Err(e) = ctx.db().add_alias(&alias, &target).await {
            format!("dbLoadRecords: alias '{alias}' for '{target}' rejected: {e}")
        } else {
            continue;
        };
        faults.recoverable(msg);
    }

    faults.status()
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
                optional: true,
            },
            ArgDesc {
                name: "var1=value1,var2=value2",
                arg_type: ArgType::String,
                optional: true,
            },
            ArgDesc {
                name: "path1:path2:...",
                arg_type: ArgType::String,
                optional: true,
            },
        ],
        "dbLoadTemplate subFile [globalMacros] [path] - Load records from a .substitutions file",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `dbLoadTemplate.y:344-347` diagnoses a missing or empty
            // name itself, on stderr, and returns -1.
            let path = match &args[0] {
                ArgValue::String(s) if !s.is_empty() => s,
                _ => {
                    eprintln!("must specify variable substitution file");
                    return Ok(CommandOutcome::Continue);
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
                return Err(format!(
                    "Failed to load '{path}'\n    Records cannot be loaded after iocInit!"
                ));
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

            let mut count = 0usize;
            for (file, merged) in rows {
                let template = db_loader::resolve_template(&file, &config.include_paths)
                    .map_err(|e| format!("parse error: {e}"))?;
                let parsed = db_loader::parse_db_file_with_breaktables(&template, &merged, &config)
                    .map_err(|e| format!("parse error: {e}"))?;
                // Each row IS a `dbLoadRecords`, so its `breaktable(...)`
                // definitions join the database registry exactly as that
                // command's do, and a later row's `LINR` name resolves
                // against them.
                let breaktable_registry =
                    ctx.block_on(async { ctx.db().add_breaktables(parsed.breaktables).await });
                count += parsed.records.len();
                // Identical install path to `dbLoadRecords`: same
                // duplicate-name merge, field application, load-then-init
                // ordering and post-load passes, so a template-loaded record
                // is indistinguishable from a directly loaded one.
                ctx.block_on(install_record_defs(
                    ctx,
                    parsed.records,
                    parsed.unresolved_aliases,
                    &breaktable_registry,
                    parsed.faults,
                ))?;
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
        let bridge = {
            let _guard = rt.enter();
            crate::runtime::task::BlockingBridge::capture()
        };
        let ctx = CommandContext::new(db.clone(), bridge);
        // Leak the runtime so it stays alive for the test
        std::mem::forget(rt);
        (db, ctx)
    }

    /// `scanppl` prints each periodic list's cumulative over-run count —
    /// C `Records with SCAN = '%s' (%lu over-runs):` (`dbScan.c:408-409`).
    /// It is the observable half of the over-run rule: without it a list
    /// that keeps missing its deadline reads exactly like one that never
    /// does. The event and passive lists have no such counter in C.
    #[test]
    fn scanppl_prints_the_over_run_count_for_periodic_lists() {
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
        db.get_record("TICKER").unwrap().write().common.scan = ScanType::Sec1;
        db.update_scan_index("TICKER", ScanType::Passive, ScanType::Sec1, 0, 0);
        db.get_record("IDLE").unwrap().write().common.scan = ScanType::Event;
        db.update_scan_index("IDLE", ScanType::Passive, ScanType::Event, 0, 0);
        for _ in 0..3 {
            db.record_scan_overrun(ScanType::Sec1);
        }

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
            out.contains("1 second: 1 records (3 over-runs)"),
            "periodic list carries its over-run count — got:\n{out}"
        );
        assert!(
            out.contains("Event: 1 records\n"),
            "a non-periodic list has no over-run counter in C — got:\n{out}"
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

    /// C `dbl` (`dbTest.c:164-180`, registered with TWO args at
    /// `dbIocRegister.c:203`): `*` and `""` are the all-types sentinel,
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

    /// C `printBuffer` (`dbTest.c:986-1150`) through `dbpr_msgOut`'s
    /// 10-column tab buffer: `DBF_<T>:` padded to the next tab stop,
    /// integers with a hex companion, strings quoted and escaped,
    /// doubles at `%.12g`, and a DBR_ENUM field re-read as DBR_STRING.
    #[test]
    fn dbgf_line_shape_matches_c() {
        assert_eq!(
            dbgf_lines(&EpicsValue::Long(42)),
            vec![format!("DBF_LONG:{}42 = 0x2a ", " ".repeat(11))]
        );
        assert_eq!(
            dbgf_lines(&EpicsValue::Long(-1)),
            // The value crosses the 30-column stop, so the fill runs to
            // 40 exactly as C's dbpr_insert_msg does.
            vec![format!(
                "DBF_LONG:{}-1 = 0xffffffff{}",
                " ".repeat(11),
                " ".repeat(5)
            )]
        );
        assert_eq!(
            dbgf_lines(&EpicsValue::Double(25.0)),
            vec![format!("DBF_DOUBLE:{}25{}", " ".repeat(9), " ".repeat(8))]
        );
        // %.12g, not Rust's shortest round-trip.
        assert_eq!(
            dbgf_lines(&EpicsValue::Double(1.0 / 3.0))[0].trim_end(),
            "DBF_DOUBLE:         0.333333333333"
        );
        assert_eq!(
            dbgf_lines(&EpicsValue::String("hi\"there".into()))[0].trim_end(),
            "DBF_STRING:         \"hi\\\"there\""
        );
        // DBR_ENUM is fetched as DBR_STRING, so it reports DBF_STRING
        // and carries the choice text.
        assert_eq!(
            dbgf_lines(&EpicsValue::EnumWithChoices {
                index: 1,
                choices: vec!["OFF".into(), "ON".into()],
            })[0]
                .trim_end(),
            "DBF_STRING:         \"ON\""
        );
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
            ("dbCreateRecord", 3), // dbStaticIocRegister.c:36-42
            ("scanppl", 1),        // dbIocRegister.c scanpplArg0
            ("dbl", 2),            // dbIocRegister.c:203
        ] {
            assert_eq!(
                reg.get(name).unwrap().args.len(),
                nargs,
                "{name} must declare C's argument count"
            );
        }
    }

    /// `cvtArg` for `iocshArgPdbbase` (`iocsh.cpp:878-890`) accepts the
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

    /// C `dbCreateRecordCallFunc` (`dbStaticIocRegister.c:292-296`)
    /// asks for the record NAME before the record type, and an empty
    /// name is `S_dbLib_recordNameMissing` just like an absent one.
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
        assert_eq!(call(&["pdbbase"]), "Record name is required");
        assert_eq!(call(&["pdbbase", "ai"]), "Record name is required");
        assert_eq!(call(&["pdbbase", "ai", ""]), "Record name is required");
        assert_eq!(call(&[]), "Record name is required");
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
    /// `dbNextRecordType` (`dbTest.c:181-191`), so its output is
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
        // put still reports the value the record kept.
        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbpf").unwrap();
        let tokens = vec!["TEMP.SEVR".to_string(), "0".to_string()];
        let args = parse_args(&tokens, &cmd.args).unwrap();
        let refused = capture(&ctx, || {
            assert!(cmd.handler.call(&args, &ctx).is_err());
        });
        assert_eq!(refused, run_cmd(&ctx, "dbgf", &["TEMP.SEVR"]));
    }

    /// Every port command whose C original diagnoses a missing string
    /// argument itself: C's iocsh hands the body a NULL and the body
    /// prints one line and returns nonzero, so declaring the argument
    /// required made the port answer with the registry's "missing
    /// required argument" on stderr and never run the body at all.
    /// The C texts are `dbTest.c:308`, `:359`, `:401`, `:447`,
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
        for name in ["postEvent", "post_event"] {
            let cmd = reg.get(name).unwrap();
            assert_eq!(cmd.args.len(), 1);
            assert!(matches!(cmd.args[0].arg_type, ArgType::String));
        }

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
        assert_eq!(err, "ERROR: Record 'NO:SUCH' not found");
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
            "ERROR: mbbo record 'DUP:CM' already exists, can't load ai record"
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

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbLoadRecords").unwrap();
        let args = parse_args(&["sub/sep.db".to_string()], &cmd.args).unwrap();
        let Err(err) = cmd.handler.call(&args, &ctx) else {
            panic!("a name with a separator must not be searched on the path list");
        };
        assert!(err.contains("sub/sep.db"), "got: {err}");
        assert!(!exists(&db, &ctx, "SEP"));

        // Control: the same file under a bare name IS found on the list.
        write_file(dir.path(), "bare.db", r#"record(ai, "BARE") { }"#);
        let args = parse_args(&["bare.db".to_string()], &cmd.args).unwrap();
        cmd.handler
            .call(&args, &ctx)
            .expect("bare name on the list");
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

        let mut registry = CommandRegistry::new();
        register_builtins(&mut registry);
        let cmd = registry.get("dbLoadRecords").unwrap();
        let args = parse_args(&["c.db".to_string()], &cmd.args).unwrap();
        let Err(err) = cmd.handler.call(&args, &ctx) else {
            panic!("an unknown alias target must fail the call's status");
        };
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
        assert_eq!(err, "ERROR: Can't open include file 'missing.db'");
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
}
