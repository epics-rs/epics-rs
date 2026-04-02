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
    registry.register(cmd_db_load_records());
    registry.register(cmd_epics_env_set());
    registry.register(cmd_ioc_init());
    registry.register(cmd_exit());
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
            put_result.map_err(|e| format!("{e}"))?;

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

                // Level 0: NAME, RTYP, VAL
                fields.push(("NAME".to_string(), inst.name.clone()));
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
                    std::env::split_paths(&val).collect()
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

                    if let Some(rec_arc) = ctx.db().get_record(&def.name).await {
                        let mut instance = rec_arc.write().await;
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
        EpicsValue::ShortArray(_) => "DBF_SHORT",
        EpicsValue::FloatArray(_) => "DBF_FLOAT",
        EpicsValue::EnumArray(_) => "DBF_ENUM",
        EpicsValue::DoubleArray(_) => "DBF_DOUBLE",
        EpicsValue::LongArray(_) => "DBF_LONG",
        EpicsValue::CharArray(_) => "DBF_CHAR",
    }
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
    fn test_parse_macro_string() {
        let macros = parse_macro_string("P=IOC:,R=TEMP");
        assert_eq!(macros.get("P").unwrap(), "IOC:");
        assert_eq!(macros.get("R").unwrap(), "TEMP");

        let empty = parse_macro_string("");
        assert!(empty.is_empty());
    }
}
