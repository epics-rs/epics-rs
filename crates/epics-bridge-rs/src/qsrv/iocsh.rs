//! iocsh commands for QSRV — `dbLoadGroup`, `processGroups`, `qsrvStats`.
//!
//! Mirrors pvxs `ioc/groupsourcehooks.cpp` (`dbLoadGroup`,
//! `processGroups`) and `ioc/singlesourcehooks.cpp` (`qStats`). Each
//! function in this module produces a [`CommandDef`] bound to a
//! shared [`BridgeProvider`]; register the resulting `Vec<CommandDef>`
//! into the [`epics_base_rs::server::ioc_app::IocRunConfig::shell_commands`]
//! list at startup so the shell line `dbLoadGroup grp.json` does the
//! right thing.
//!
//! Typical wiring:
//!
//! ```ignore
//! use std::sync::Arc;
//! use epics_bridge_rs::qsrv::{BridgeProvider, iocsh};
//!
//! let provider = Arc::new(BridgeProvider::new(db.clone()));
//! let mut cfg = IocRunConfig::default();
//! cfg.shell_commands.extend(iocsh::register_qsrv_commands(provider.clone()));
//! ```
//!
//! `dbLoadGroup` and `processGroups` should be invoked from `st.cmd`
//! in the same order they appear in pvxs IOCs:
//!
//! ```text
//! dbLoadRecords("foo.db", "")
//! dbLoadGroup("foo-groups.json", "")
//! iocInit
//! processGroups
//! ```

use std::sync::Arc;

use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

use super::provider::BridgeProvider;

/// `dbLoadGroup <jsonFilename> [<macros>]` — load a JSON group config
/// file into the [`BridgeProvider`]. Mirrors pvxs `dbLoadGroup`
/// (groupsourcehooks.cpp:99). The `macros` argument supports the
/// pvxs/iocsh `name=value,...` form — `${name}` tokens in the JSON
/// expand to the supplied value, and any unbound `${X}` falls back
/// to `std::env::var("X")` so site configs can pull in shell vars.
pub fn db_load_group_command(provider: Arc<BridgeProvider>) -> CommandDef {
    CommandDef::new(
        "dbLoadGroup",
        vec![
            ArgDesc {
                name: "filename",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "macros",
                arg_type: ArgType::String,
                optional: true,
            },
        ],
        "dbLoadGroup <jsonFilename> [<macros>]",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let filename = match args.first() {
                Some(ArgValue::String(s)) => s.clone(),
                _ => return Err("dbLoadGroup: missing filename".into()),
            };
            let macros_str = match args.get(1) {
                Some(ArgValue::String(s)) => s.clone(),
                _ => String::new(),
            };

            // pvxs leading-`-` removal syntax
            // (groupsourcehooks.cpp:133-183): `-*` clears all file-loaded
            // groups; `-file.json` removes the groups a previous
            // `dbLoadGroup(file.json, macros)` of the matching identity
            // placed. Removal is identity-based (raw filename + raw
            // macros), so it never touches the filesystem.
            if let Some(rest) = filename.strip_prefix('-') {
                if rest == "*" {
                    let n = provider.clear_group_files();
                    ctx.println(&format!(
                        "dbLoadGroup: cleared all file groups ({n} removed)"
                    ));
                } else {
                    let n = provider.remove_group_file(rest, &macros_str);
                    ctx.println(&format!("dbLoadGroup: removed '{rest}' ({n} group(s))"));
                }
                return Ok(CommandOutcome::Continue);
            }

            let macros = parse_macros(&macros_str);
            let raw = match std::fs::read_to_string(&filename) {
                Ok(s) => s,
                Err(e) => return Err(format!("dbLoadGroup '{filename}': {e}")),
            };
            let expanded = expand_macros(&raw, &macros);
            // Track under the raw (filename, macros) identity so a later
            // `dbLoadGroup("-filename", macros)` can remove these groups.
            match provider.load_group_file_tracked(&filename, &macros_str, &expanded) {
                Ok(()) => {
                    ctx.println(&format!(
                        "dbLoadGroup: loaded '{filename}' ({} groups total)",
                        provider.group_count()
                    ));
                    Ok(CommandOutcome::Continue)
                }
                Err(e) => Err(format!("dbLoadGroup '{filename}' failed: {e}")),
            }
        },
    )
}

/// Parse a `name=value,name=value` string into a map. Whitespace
/// around tokens is stripped. Empty entries are skipped.
fn parse_macros(s: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for tok in s.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        if let Some((k, v)) = tok.split_once('=') {
            out.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    out
}

/// Expand `${NAME}` tokens in `s` against `macros`, falling back to
/// `std::env::var("NAME")`. Unbound names are left literal so the
/// downstream JSON parser surfaces them as parse errors.
fn expand_macros(s: &str, macros: &std::collections::HashMap<String, String>) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'{'
            && let Some(end) = s[i + 2..].find('}')
        {
            let name = &s[i + 2..i + 2 + end];
            if let Some(v) = macros.get(name) {
                out.push_str(v);
            } else if let Ok(v) = std::env::var(name) {
                out.push_str(&v);
            } else {
                // Leave the token literal so the JSON parser
                // errors on the unbound macro instead of silently
                // producing wrong output.
                out.push_str(&s[i..i + 3 + end]);
            }
            i += 3 + end;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// `processGroups` — finalize group config after `dbLoadGroup` calls
/// and (typically) `iocInit`. Validates trigger references and
/// reports counts. Mirrors pvxs `processGroups`
/// (groupsourcehooks.cpp:192).
pub fn process_groups_command(provider: Arc<BridgeProvider>) -> CommandDef {
    CommandDef::new(
        "processGroups",
        vec![],
        "processGroups",
        move |_args: &[ArgValue], ctx: &CommandContext| {
            let n = provider.process_groups();
            ctx.println(&format!("processGroups: finalized {n} group(s)"));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `qsrvStats [<recordOrGroupName>]` — print summary diagnostics for
/// QSRV-bridged channels. With no argument, lists all groups + the
/// total record count. With a name, prints the group's member roster
/// (or "single record" for a non-group channel name). Mirrors pvxs
/// `qStats` (singlesourcehooks.cpp:88) at the summary level.
pub fn qsrv_stats_command(provider: Arc<BridgeProvider>) -> CommandDef {
    CommandDef::new(
        "qsrvStats",
        vec![ArgDesc {
            name: "name",
            arg_type: ArgType::String,
            optional: true,
        }],
        "qsrvStats [<recordOrGroupName>]",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let groups = provider.groups();
            match args.first() {
                Some(ArgValue::String(name)) if !name.is_empty() => {
                    if let Some(def) = groups.get(name) {
                        ctx.println(&format!(
                            "Group '{}' (atomic={}, struct_id={:?}): {} member(s)",
                            def.name,
                            def.atomic,
                            def.struct_id,
                            def.members.len()
                        ));
                        for m in &def.members {
                            ctx.println(&format!(
                                "  {} <- {} (mapping={:?}, put_order={}, triggers={:?})",
                                m.field_name,
                                m.channel,
                                m.mapping,
                                m.put_order
                                    .map(|p| p.to_string())
                                    .unwrap_or_else(|| "<none>".into()),
                                m.triggers
                            ));
                        }
                    } else {
                        ctx.println(&format!(
                            "qsrvStats: '{name}' is not a registered group; treating as single record."
                        ));
                    }
                }
                _ => {
                    let stats = provider.op_stats();
                    ctx.println(&format!(
                        "qsrvStats: {} group(s), {} channels created (cumulative), {} get / {} put / {} subscribe",
                        groups.len(),
                        stats.channels_created,
                        stats.gets,
                        stats.puts,
                        stats.subscribes,
                    ));
                    let mut names: Vec<&String> = groups.keys().collect();
                    names.sort();
                    for n in names {
                        let def = &groups[n];
                        ctx.println(&format!(
                            "  {n}  ({} member{}, atomic={})",
                            def.members.len(),
                            if def.members.len() == 1 { "" } else { "s" },
                            def.atomic
                        ));
                    }
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `resetGroups` — clear the group-PV registry. Mirrors pvxs
/// `resetGroups` (groupsourcehooks.cpp:222). Used between IOC reload
/// cycles in tests.
pub fn reset_groups_command(provider: Arc<BridgeProvider>) -> CommandDef {
    CommandDef::new(
        "resetGroups",
        vec![],
        "resetGroups",
        move |_args: &[ArgValue], ctx: &CommandContext| {
            let n = provider.reset_groups();
            ctx.println(&format!("resetGroups: dropped {n} group(s)"));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Convenience: build the full QSRV iocsh command set (`dbLoadGroup`,
/// `processGroups`, `qsrvStats`, `resetGroups`) bound to `provider`.
/// Drop the returned vector into
/// [`epics_base_rs::server::ioc_app::IocRunConfig::shell_commands`].
pub fn register_qsrv_commands(provider: Arc<BridgeProvider>) -> Vec<CommandDef> {
    vec![
        db_load_group_command(provider.clone()),
        process_groups_command(provider.clone()),
        qsrv_stats_command(provider.clone()),
        reset_groups_command(provider),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use epics_base_rs::server::database::PvDatabase;

    #[tokio::test]
    async fn db_load_group_then_process_succeeds() {
        let db = Arc::new(PvDatabase::new());
        let provider = Arc::new(BridgeProvider::new(db));
        let json = r#"{
            "TEST:grp": {
                "+id": "epics:nt/NTScalar:1.0",
                "+atomic": true,
                "value": { "+channel": "TEST:val.VAL", "+type": "plain" }
            }
        }"#;
        let path = std::env::temp_dir().join("qsrv_iocsh_test.json");
        std::fs::write(&path, json).unwrap();

        provider.load_group_file(path.to_str().unwrap()).unwrap();
        assert_eq!(provider.group_count(), 1);

        let n = provider.process_groups();
        assert_eq!(n, 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn register_qsrv_commands_returns_four() {
        let db = Arc::new(PvDatabase::new());
        let provider = Arc::new(BridgeProvider::new(db));
        let cmds = register_qsrv_commands(provider);
        assert_eq!(cmds.len(), 4);
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"dbLoadGroup"));
        assert!(names.contains(&"processGroups"));
        assert!(names.contains(&"qsrvStats"));
        assert!(names.contains(&"resetGroups"));
    }

    #[test]
    fn macro_substitution_replaces_tokens() {
        let mut m = std::collections::HashMap::new();
        m.insert("PVNAME".to_string(), "TEST:val".to_string());
        m.insert("UNIT".to_string(), "deg".to_string());
        let s = expand_macros(r#"{"+id": "${PVNAME}_${UNIT}", "+atomic": false}"#, &m);
        assert_eq!(s, r#"{"+id": "TEST:val_deg", "+atomic": false}"#);
    }

    #[test]
    fn macro_unbound_left_literal() {
        let m = std::collections::HashMap::new();
        let s = expand_macros("${MISSING}", &m);
        assert_eq!(s, "${MISSING}");
    }

    #[test]
    fn parse_macros_strips_whitespace() {
        let m = parse_macros(" name = TEST:val , unit = deg ,, ");
        assert_eq!(m.get("name"), Some(&"TEST:val".to_string()));
        assert_eq!(m.get("unit"), Some(&"deg".to_string()));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn reset_groups_clears_registry() {
        let db = Arc::new(PvDatabase::new());
        let provider = Arc::new(BridgeProvider::new(db));
        provider
            .load_group_config(
                r#"{ "G:a": { "+atomic": false, "v": { "+channel": "X.VAL", "+type": "plain" } } }"#,
            )
            .unwrap();
        assert_eq!(provider.group_count(), 1);
        let dropped = provider.reset_groups();
        assert_eq!(dropped, 1);
        assert_eq!(provider.group_count(), 0);
    }
}
