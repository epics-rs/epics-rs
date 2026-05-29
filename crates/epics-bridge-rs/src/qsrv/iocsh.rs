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
/// (groupsourcehooks.cpp:99). The `macros` argument is the pvxs/iocsh
/// `name=value,...` form, and the JSON text is run through the full
/// EPICS Base `macLib` engine (`macCore.c` `trans`/`refer`) before the
/// group parser sees it — `$(NAME)` / `${NAME}` references, defaults
/// (`$(NAME=default)`), nested references (`${BAR=${FOO}}`), scoped
/// definitions (`${BAR,BAR=$(FOO)}`), and environment fallback.
///
/// pvxs only creates the `MAC_HANDLE` when the macros argument is
/// non-empty (`groupconfigprocessor.cpp` guards on `macros[0]!='\0'`,
/// `groupsourcehooks.cpp:153`); with an empty macros argument the JSON
/// lines are used verbatim and no macro/env expansion happens at all.
/// We mirror that: an empty `macros` string skips expansion entirely.
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

            let raw = match std::fs::read_to_string(&filename) {
                Ok(s) => s,
                Err(e) => return Err(format!("dbLoadGroup '{filename}': {e}")),
            };
            // pvxs only builds a MAC_HANDLE (and so only expands macros)
            // when the macros argument is non-empty; an empty argument
            // means the JSON is used verbatim.
            let expanded = if macros_str.is_empty() {
                raw
            } else {
                expand_macros(&raw, &parse_macros(&macros_str))
            };
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

/// Expand `$(...)` / `${...}` macro references in `s`, mirroring the C
/// EPICS Base `macLib` engine (`modules/libcom/src/macLib/macCore.c`
/// `trans` / `refer` / `lookup`) that pvxs drives through `macDefExpand`
/// (`groupconfigprocessor.cpp:88-101`). Implemented behaviors:
///
///   - `$(NAME)` and `${NAME}` are both references; `\<char>` blocks
///     detection and copies both bytes verbatim (`trans:740-749`).
///   - macros are NOT expanded inside single quotes (`trans:722-733`).
///   - a reference name is itself macro-expanded before lookup, so
///     `$($(WHICH))` resolves the inner reference first.
///   - the name terminates at `=`, `,`, or the closing bracket
///     (`macEnd = "=,)"`): `$(NAME=default)` supplies a default and
///     `$(NAME,key=val)` introduces scoped macros visible only inside
///     that reference's expansion.
///   - lookup order is scoped frames (innermost first), then the
///     supplied `macros`, then the process environment (pvxs creates
///     the handle with the `{"","environ"}` pair —
///     `groupsourcehooks.cpp:154`, `lookup`+`FLAG_USE_ENVIRONMENT`).
///   - a resolved (macro or env) value is re-scanned for further
///     references (chained expansion); a self-referential macro emits
///     its value once without recursing (`refentry->visited`).
///   - an undefined name with no default emits the `macLib` placeholder
///     `$(name,undefined)` (`refer:errval`), which the JSON parser then
///     rejects rather than silently producing wrong output.
fn expand_macros(s: &str, macros: &std::collections::HashMap<String, String>) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    mac_trans(&chars, macros, &mut Vec::new(), &mut Vec::new(), &mut out);
    out
}

/// Translate `chars` into `out`, expanding macro references.
///
/// `scopes` is the stack of scoped-macro frames pushed by enclosing
/// `$(name,key=val)` references; lookup walks it innermost-first, then
/// `macros`, then the environment. `visiting` is the stack of macro
/// names currently being expanded — it guards a self-referential macro
/// (`A=$(A)`) against infinite recursion, mirroring C `macCore.c`'s
/// per-entry `visited` flag.
fn mac_trans(
    chars: &[char],
    macros: &std::collections::HashMap<String, String>,
    scopes: &mut Vec<std::collections::HashMap<String, String>>,
    visiting: &mut Vec<String>,
    out: &mut String,
) {
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];

        // Track single/double quote state (C `trans` `quote` var).
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
        }

        // `\<char>`: emit both verbatim, skip macro detection.
        if c == '\\' && i + 1 < chars.len() {
            out.push('\\');
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }

        // Macro reference: `$` followed by `(` or `{`, NOT inside single
        // quotes (C `macRef && quote != '\''`).
        let mac_ref =
            c == '$' && i + 1 < chars.len() && (chars[i + 1] == '(' || chars[i + 1] == '{');
        if mac_ref && quote != Some('\'') {
            if let Some(next) = mac_refer(chars, i, macros, scopes, visiting, out) {
                i = next;
                continue;
            }
        }

        out.push(c);
        i += 1;
    }
}

/// Expand one macro reference starting at `chars[start]` (`$`). Returns
/// the index just past the closing bracket, or `None` if the reference
/// is unterminated (caller then copies `$` raw).
fn mac_refer(
    chars: &[char],
    start: usize,
    macros: &std::collections::HashMap<String, String>,
    scopes: &mut Vec<std::collections::HashMap<String, String>>,
    visiting: &mut Vec<String>,
    out: &mut String,
) -> Option<usize> {
    let close = if chars[start + 1] == '(' { ')' } else { '}' };
    // Find the matching close bracket, honoring nested `$(`/`${`.
    let body_start = start + 2;
    let mut depth = 1usize;
    let mut j = body_start;
    while j < chars.len() && depth > 0 {
        if j + 1 < chars.len() && chars[j] == '$' && (chars[j + 1] == '(' || chars[j + 1] == '{') {
            depth += 1;
            j += 2;
            continue;
        }
        if depth == 1 && chars[j] == close || depth > 1 && (chars[j] == ')' || chars[j] == '}') {
            depth -= 1;
            if depth == 0 {
                break;
            }
        }
        j += 1;
    }
    if depth != 0 {
        return None; // unterminated — caller emits '$' literally
    }
    let body = &chars[body_start..j];
    let after = j + 1;

    // Split the body at the first top-level `=` or `,` (the C `macEnd`
    // terminator set). Nested `$(...)` brackets are skipped so a `=`/`,`
    // inside an inner reference does not terminate.
    let (name_chars, rest) = match mac_top_level_terminator(body) {
        Some(k) => (&body[..k], &body[k..]),
        None => (body, &body[body.len()..]),
    };

    // the name itself may contain macro references — expand it.
    let mut name = String::new();
    mac_trans(name_chars, macros, scopes, visiting, &mut name);

    // Default value (`=...`) and scoped definitions (`,k=v`).
    let mut default: Option<&[char]> = None;
    let mut scoped: Vec<(String, String)> = Vec::new();
    if let Some(first) = rest.first() {
        if *first == '=' {
            let dflt = &rest[1..];
            match mac_top_level_comma(dflt) {
                Some(k) => {
                    default = Some(&dflt[..k]);
                    mac_parse_scoped(&dflt[k..], macros, scopes, visiting, &mut scoped);
                }
                None => default = Some(dflt),
            }
        } else if *first == ',' {
            mac_parse_scoped(rest, macros, scopes, visiting, &mut scoped);
        }
    }

    // Push the scoped frame (visible only inside this expansion).
    let mut frame: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (k, v) in scoped {
        frame.insert(k, v);
    }
    scopes.push(frame);

    // Look up: innermost scope first, then base macros, then the
    // environment (pvxs's `{"","environ"}` handle / FLAG_USE_ENVIRONMENT).
    let resolved = scopes
        .iter()
        .rev()
        .find_map(|s| s.get(&name).cloned())
        .or_else(|| macros.get(&name).cloned())
        .or_else(|| {
            if name.is_empty() {
                None
            } else {
                std::env::var(&name).ok()
            }
        });

    match resolved {
        Some(val) => {
            if visiting.contains(&name) {
                // Recursive reference (C `refentry->visited`): emit the
                // resolved value once WITHOUT re-expansion to break the
                // cycle, rather than recursing forever.
                out.push_str(&val);
            } else {
                visiting.push(name.clone());
                let val_chars: Vec<char> = val.chars().collect();
                mac_trans(&val_chars, macros, scopes, visiting, out);
                visiting.pop();
            }
        }
        None => match default {
            Some(def_chars) => {
                // Strip a single layer of surrounding quotes from the
                // default (`$(NAME="value")` → value).
                let def = mac_strip_outer_quotes(def_chars);
                mac_trans(def, macros, scopes, visiting, out);
            }
            None => {
                // Undefined macro placeholder (C `refer` `errval`).
                out.push_str("$(");
                out.push_str(&name);
                out.push_str(",undefined)");
            }
        },
    }

    scopes.pop();
    Some(after)
}

/// Parse a `,key=val,key2=val2,...` scoped-definition tail. A bare
/// `,key` with no `=` defines nothing (C silently skips it).
fn mac_parse_scoped(
    rest: &[char],
    macros: &std::collections::HashMap<String, String>,
    scopes: &mut Vec<std::collections::HashMap<String, String>>,
    visiting: &mut Vec<String>,
    out: &mut Vec<(String, String)>,
) {
    let mut k = 0;
    while k < rest.len() {
        if rest[k] != ',' {
            break;
        }
        k += 1; // step over ','
        let seg = &rest[k..];
        let (name_part, tail) = match mac_top_level_terminator(seg) {
            Some(t) => (&seg[..t], &seg[t..]),
            None => (seg, &seg[seg.len()..]),
        };
        let mut sname = String::new();
        mac_trans(name_part, macros, scopes, visiting, &mut sname);
        k += name_part.len();
        if let Some('=') = tail.first() {
            let valseg = &tail[1..];
            let (val_part, _) = match mac_top_level_comma(valseg) {
                Some(t) => (&valseg[..t], &valseg[t..]),
                None => (valseg, &valseg[valseg.len()..]),
            };
            let mut sval = String::new();
            mac_trans(val_part, macros, scopes, visiting, &mut sval);
            out.push((sname, sval));
            k += 1 + val_part.len();
        }
        // else: bare `,name` — no value, defines nothing.
    }
}

/// Index of the first top-level `=` or `,` in `body`, skipping any
/// nested `$(...)` / `${...}` reference.
fn mac_top_level_terminator(body: &[char]) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = 0;
    while i < body.len() {
        let c = body[i];
        if c == '$' && i + 1 < body.len() && (body[i + 1] == '(' || body[i + 1] == '{') {
            depth += 1;
            i += 2;
            continue;
        }
        if (c == ')' || c == '}') && depth > 0 {
            depth -= 1;
        } else if depth == 0 && (c == '=' || c == ',') {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Index of the first top-level `,` in `body` (splits a default value
/// from trailing scoped definitions).
fn mac_top_level_comma(body: &[char]) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = 0;
    while i < body.len() {
        let c = body[i];
        if c == '$' && i + 1 < body.len() && (body[i + 1] == '(' || body[i + 1] == '{') {
            depth += 1;
            i += 2;
            continue;
        }
        if (c == ')' || c == '}') && depth > 0 {
            depth -= 1;
        } else if depth == 0 && c == ',' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Strip one layer of matching surrounding double quotes from a slice.
fn mac_strip_outer_quotes(s: &[char]) -> &[char] {
    if s.len() >= 2 && s[0] == '"' && s[s.len() - 1] == '"' {
        &s[1..s.len() - 1]
    } else {
        s
    }
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
        // both `${...}` and `$(...)` forms resolve (macLib `macEnd`).
        let s = expand_macros(r#"{"+id": "${PVNAME}_$(UNIT)", "+atomic": false}"#, &m);
        assert_eq!(s, r#"{"+id": "TEST:val_deg", "+atomic": false}"#);
    }

    #[test]
    fn macro_unbound_emits_maclib_placeholder() {
        // macLib (`macCore.c` `refer`) replaces an undefined macro with
        // the `$(name,undefined)` marker so the JSON parser then errors,
        // rather than leaving the raw `${name}` token.
        let m = std::collections::HashMap::new();
        let s = expand_macros("${THIS_MACRO_IS_NOT_SET_ANYWHERE}", &m);
        assert_eq!(s, "$(THIS_MACRO_IS_NOT_SET_ANYWHERE,undefined)");
    }

    #[test]
    fn macro_default_value() {
        // `$(NAME=default)` / `${NAME=default}` supply a fallback used
        // when the macro is otherwise undefined (macLib default arm).
        let m = std::collections::HashMap::new();
        assert_eq!(expand_macros("${P=DEFAULT}", &m), "DEFAULT");
        let mut m2 = std::collections::HashMap::new();
        m2.insert("P".to_string(), "SET".to_string());
        assert_eq!(expand_macros("$(P=DEFAULT)", &m2), "SET");
    }

    #[test]
    fn macro_nested_reference() {
        // `${BAR=${FOO}}`: the default is itself macro-expanded
        // (macLib `macDefExpandTest.c:193-218`).
        let mut m = std::collections::HashMap::new();
        m.insert("FOO".to_string(), "fromfoo".to_string());
        assert_eq!(expand_macros("${BAR=${FOO}}", &m), "fromfoo");
    }

    #[test]
    fn macro_scoped_definition() {
        // `${BAR,BAR=$(FOO)}`: a scoped definition assigns BAR for the
        // duration of this reference's expansion.
        let mut m = std::collections::HashMap::new();
        m.insert("FOO".to_string(), "scopedval".to_string());
        assert_eq!(expand_macros("${BAR,BAR=$(FOO)}", &m), "scopedval");
    }

    #[test]
    fn macro_chained_reexpansion() {
        // a resolved value is re-scanned for further references.
        let mut m = std::collections::HashMap::new();
        m.insert("A".to_string(), "$(B)".to_string());
        m.insert("B".to_string(), "deep".to_string());
        assert_eq!(expand_macros("$(A)", &m), "deep");
    }

    #[test]
    fn macro_self_reference_terminates() {
        // `A=$(A)` must not recurse forever (macLib `visited`).
        let mut m = std::collections::HashMap::new();
        m.insert("A".to_string(), "$(A)".to_string());
        // emits the resolved value once; the inner self-ref is emitted
        // raw (not re-expanded) so it cannot loop.
        assert_eq!(expand_macros("$(A)", &m), "$(A)");
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
