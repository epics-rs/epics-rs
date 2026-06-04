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

            match apply_group_file(&provider, &filename, &macros_str) {
                Ok(total) => {
                    ctx.println(&format!(
                        "dbLoadGroup: loaded '{filename}' ({total} groups total)"
                    ));
                    Ok(CommandOutcome::Continue)
                }
                Err(e) => Err(e),
            }
        },
    )
}

/// Apply one `dbLoadGroup(filename, macros)` to `provider`: read the file,
/// run it through macLib **only when `macros` is non-empty** (matching
/// pvxs's `macros[0]!='\0'` guard, groupsourcehooks.cpp:153), and merge the
/// parsed groups into the provider under the `(filename, macros)` source
/// identity so a later `dbLoadGroup("-file", macros)` can remove them.
/// Returns the running group count.
///
/// This is the single load path shared by the standalone
/// [`db_load_group_command`] and the QSRV protocol runner, which calls it
/// for every entry drained from the base `dbLoadGroup` startup queue
/// (pvxs `GroupConfigProcessor::loadConfigFiles`,
/// groupsourcehooks.cpp:200-207).
pub(crate) fn apply_group_file(
    provider: &BridgeProvider,
    filename: &str,
    macros: &str,
) -> Result<usize, String> {
    let raw =
        std::fs::read_to_string(filename).map_err(|e| format!("dbLoadGroup '{filename}': {e}"))?;
    let expanded = if macros.is_empty() {
        raw
    } else {
        // pvxs expands each group-config line independently through
        // `macDefExpand` and, when expansion fails (an undefined or recursive
        // macro makes `macExpandString` return a negative length, so
        // `macDefExpand` returns `NULL`), logs an error and skips that line —
        // the failed line never reaches the JSON buffer
        // (groupconfigprocessor.cpp:88-105). Expanding line-by-line, rather
        // than the whole file at once, is what stops an undefined reference
        // inside a quoted string (`"+channel": "$(MISSING)"`) or a group name
        // from surviving as a literal `$(MISSING,undefined)` placeholder.
        let parsed = parse_macros(macros);
        let mut buffer = String::with_capacity(raw.len());
        for (idx, line) in raw.lines().enumerate() {
            match expand_macros(line, &parsed) {
                Some(exp) => {
                    buffer.push_str(&exp);
                    buffer.push('\n');
                }
                None => {
                    tracing::error!(
                        file = %filename,
                        line = idx + 1,
                        "dbLoadGroup: macro expansion failed (undefined or \
                         recursive macro); skipping line"
                    );
                }
            }
        }
        buffer
    };
    provider
        .load_group_file_tracked(filename, macros, &expanded)
        .map_err(|e| format!("dbLoadGroup '{filename}' failed: {e}"))?;
    Ok(provider.group_count())
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
///     detection. At level 0 (the user's source string) both bytes are
///     copied verbatim; at a discard level (a substituted macro value) the
///     escape backslash is dropped and only the escaped byte is kept
///     (`trans:740-743`, `discard = level > 0`).
///   - quote delimiters are kept at level 0 (the user's quotes) but removed
///     from substituted macro values/names/defaults/scopes
///     (`trans:716-726`).
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
///     references (chained expansion); a self-referential macro is a
///     recursive reference and fails the expansion (`refentry->visited`).
///   - an undefined name with no default, or a recursive reference, is an
///     expansion *error* (`refer` sets `entry->error`, errval
///     `,undefined)`/`,recursive)`). `macExpandString` then returns a
///     negative length and `macDefExpand` returns `NULL`
///     (macCore.c:210,220,895-896,881-882), so this function returns `None`
///     instead of a string. pvxs's group loader skips the whole line on
///     `NULL`, so a placeholder can never register as a literal channel or
///     group name (groupconfigprocessor.cpp:91-103).
///
/// Returns `None` exactly when C `macDefExpand()` would return `NULL` for the
/// same input + macro set; `Some(expanded)` otherwise.
fn expand_macros(s: &str, macros: &std::collections::HashMap<String, String>) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    // `error` mirrors C `entry.error`: any undefined or recursive reference
    // anywhere in the (possibly nested) expansion sets it. The reference name
    // and an actually-used default value are translated through this same flag
    // (C uses the outer `entry`, macCore.c:798,890-893), so their errors
    // propagate; scoped-definition names/values are NOT (separate `subs`,
    // macCore.c:821-826), handled inside `mac_parse_scoped`.
    let mut error = false;
    // The user's source string is translated at level 0 (`discard = false`):
    // its own quote delimiters and escape backslashes are preserved
    // (`macExpandString` → `trans(handle, &entry, 0, ...)`, macCore.c:216).
    // Substituted macro values/names/defaults/scopes are translated at
    // level+1 inside `mac_refer`, where `discard = true` strips them
    // (matching C `expand`/`refer`, macCore.c:667-673,798,892).
    mac_trans(
        &chars,
        macros,
        &mut Vec::new(),
        &mut Vec::new(),
        false,
        &mut error,
        &mut out,
    );
    // macExpandString returns the destination text even on error, but
    // macDefExpand discards it and returns NULL when the length is negative
    // (macCore.c:220). Mirror that: a failed expansion yields no string.
    if error { None } else { Some(out) }
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
    discard: bool,
    error: &mut bool,
    out: &mut String,
) {
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];

        // Track single/double quote state (C `trans` `quote` var). At a
        // discard level (`level > 0` — i.e. these are NOT the user's quotes
        // but a substituted macro value/name/default/scope) the quote
        // DELIMITERS are dropped: C `continue`s past the opening and the
        // closing quote without copying them (macCore.c:716-726). The
        // characters between the quotes are still emitted.
        if let Some(q) = quote {
            if c == q {
                quote = None;
                if discard {
                    i += 1;
                    continue;
                }
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
            if discard {
                i += 1;
                continue;
            }
        }

        // `\<char>`: copy the escaped character; the backslash itself is
        // kept only at level 0 (the user's escape) and dropped at a discard
        // level (C `if (v < valend && !discard) *v++ = '\\'`,
        // macCore.c:740-743). Either way the macro detector does not see the
        // escaped byte.
        if c == '\\' && i + 1 < chars.len() {
            if !discard {
                out.push('\\');
            }
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }

        // Macro reference: `$` followed by `(` or `{`, NOT inside single
        // quotes (C `macRef && quote != '\''`).
        let mac_ref =
            c == '$' && i + 1 < chars.len() && (chars[i + 1] == '(' || chars[i + 1] == '{');
        if mac_ref && quote != Some('\'') {
            if let Some(next) = mac_refer(chars, i, macros, scopes, visiting, error, out) {
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
    error: &mut bool,
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

    // the name itself may contain macro references — expand it. The name
    // is a substituted (level+1) value, so its quotes/escapes are discarded
    // (C `trans(handle, entry, level+1, macEnd, ...)`, macCore.c:798). C
    // translates the name through the *outer* `entry`, so an undefined or
    // recursive reference inside the name fails the whole expansion — pass the
    // shared `error`.
    let mut name = String::new();
    mac_trans(name_chars, macros, scopes, visiting, true, error, &mut name);

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
                // Recursive reference: C `refer` finds `refentry->visited`
                // already set, sets `entry->error = TRUE`, and writes the
                // `$(name,recursive)` placeholder (macCore.c:880-882,903-912).
                // `macExpandString` then reports a negative length so
                // `macDefExpand` returns `NULL`. Flag the error; the
                // placeholder text is discarded by `expand_macros` on error,
                // exactly as `macDefExpand` discards the destination buffer.
                *error = true;
                out.push_str("$(");
                out.push_str(&name);
                out.push_str(",recursive)");
            } else {
                visiting.push(name.clone());
                let val_chars: Vec<char> = val.chars().collect();
                // A resolved macro value is a substituted (level+1) value:
                // its quote delimiters and escape backslashes are stripped
                // (C `trans(handle, entry, level+1, "", &rv, ...)` /
                // pre-`expand` at level 1, macCore.c:667-673,875). Errors in
                // the value (e.g. a chained reference to an undefined macro)
                // propagate through the shared `entry` (macCore.c:870,875).
                mac_trans(&val_chars, macros, scopes, visiting, true, error, out);
                visiting.pop();
            }
        }
        None => match default {
            Some(def_chars) => {
                // The default value is also substituted at level+1, so its
                // quotes/escapes are discarded by the translation itself —
                // C `trans(handle, entry, level+1, macEnd+1, &defval, ...)`
                // (macCore.c:892). It is translated through the outer `entry`,
                // so a default that itself resolves to an undefined/recursive
                // reference still fails the expansion — pass the shared
                // `error`. A present, resolvable default is NOT an error.
                mac_trans(def_chars, macros, scopes, visiting, true, error, out);
            }
            None => {
                // Undefined reference with no default: C `refer` sets
                // `entry->error = TRUE` and writes the `$(name,undefined)`
                // placeholder (macCore.c:895-896,903-912), so
                // `macExpandString` returns a negative length and
                // `macDefExpand` returns `NULL`. Flag the error; the
                // placeholder is discarded by `expand_macros` on error.
                *error = true;
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
    // C translates scoped-definition names and values through a SEPARATE
    // `MAC_ENTRY subs` whose `error` flag is initialized fresh and never
    // merged back into the enclosing `entry` (macCore.c:821-826,841,849). So
    // an undefined or recursive reference inside a scoped definition does NOT
    // fail the surrounding expansion. Route these translations through a
    // throwaway sink rather than the caller's `error` to match.
    let mut scoped_err = false;
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
        // Scoped macro names/values are substituted at level+1, so their
        // quotes/escapes are discarded (C `trans(handle, &subs, level+1,
        // ...)`, macCore.c:841,849).
        mac_trans(
            name_part,
            macros,
            scopes,
            visiting,
            true,
            &mut scoped_err,
            &mut sname,
        );
        k += name_part.len();
        if let Some('=') = tail.first() {
            let valseg = &tail[1..];
            let (val_part, _) = match mac_top_level_comma(valseg) {
                Some(t) => (&valseg[..t], &valseg[t..]),
                None => (valseg, &valseg[valseg.len()..]),
            };
            let mut sval = String::new();
            mac_trans(
                val_part,
                macros,
                scopes,
                visiting,
                true,
                &mut scoped_err,
                &mut sval,
            );
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

/// `pvxsl [<detail>]` — list the single-record source PV names served
/// by QSRV. Mirrors pvxs `pvxsl` (`singlesourcehooks.cpp:33-65`,
/// registered at `:162-166`): `detail=0` prints just the PV names, one
/// per line; a non-zero `detail` prints a source header followed by an
/// indented record list. The Rust single-record source is the
/// `BridgeProvider` database (records + aliases), excluding group PVs
/// (those are listed by `pvxgl`).
pub fn pvxsl_command(provider: Arc<BridgeProvider>) -> CommandDef {
    CommandDef::new(
        "pvxsl",
        vec![ArgDesc {
            name: "detail",
            arg_type: ArgType::Int,
            optional: true,
        }],
        "pvxsl [<detail>]",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let detail = match args.first() {
                Some(ArgValue::Int(n)) => *n,
                _ => 0,
            };
            let db = provider.database().clone();
            let mut names = ctx.block_on(async {
                let mut n = db.all_record_names().await;
                n.extend(db.all_alias_names().await);
                n
            });
            names.sort();
            names.dedup();
            // pvxs prints the SOURCE header only when the source has at
            // least one name (`list.names && !list.names->empty()`).
            if detail != 0 && !names.is_empty() {
                ctx.println("------------------");
                ctx.println("SOURCE: qsrv single records");
                ctx.println("------------------");
                ctx.println("RECORDS: ");
            }
            for n in names {
                if detail != 0 {
                    ctx.println(&format!("  {n}"));
                } else {
                    ctx.println(&n);
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `pvxgl [<level>] [<pattern>]` — list QSRV group PV names, optionally
/// filtered by a glob `pattern` and, when `level > 0`, with group
/// detail. Mirrors pvxs `pvxsgl` (`groupsourcehooks.cpp:50-83`,
/// registered at `:233-240`) and `Group::show` (`group.cpp`): an empty
/// pattern matches every group; `level > 0` prints the atomic flag and
/// member count; `level > 1` prints one line per member.
pub fn pvxgl_command(provider: Arc<BridgeProvider>) -> CommandDef {
    CommandDef::new(
        "pvxgl",
        vec![
            ArgDesc {
                name: "level",
                arg_type: ArgType::Int,
                optional: true,
            },
            ArgDesc {
                name: "pattern",
                arg_type: ArgType::String,
                optional: true,
            },
        ],
        "pvxgl [<level>] [<pattern>]",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let level = match args.first() {
                Some(ArgValue::Int(n)) => *n,
                _ => 0,
            };
            let pattern = match args.get(1) {
                Some(ArgValue::String(s)) => s.as_str(),
                _ => "",
            };
            let groups = provider.groups();
            let mut names: Vec<&String> = groups.keys().collect();
            names.sort();
            for name in names {
                // empty pattern matches everything (pvxs `!pattern[0]`).
                if !pattern.is_empty() && !glob_match(name, pattern) {
                    continue;
                }
                ctx.println(name);
                if level > 0 {
                    let def = &groups[name];
                    // pvxs `Group::show`: atomic flag + member count.
                    ctx.println(&format!(
                        "  Atomic Get/Put:{} Atomic Members:{}",
                        if def.atomic { "yes" } else { "no" },
                        def.members.len()
                    ));
                    if level > 1 {
                        for m in &def.members {
                            // "  grp.fld <mapping> id=foo chan=pv chan ..."
                            let id = m
                                .struct_id
                                .as_ref()
                                .map(|s| format!(" id={s}"))
                                .unwrap_or_default();
                            let chan = if m.channel.is_empty() {
                                String::new()
                            } else {
                                format!(" chan={}", m.channel)
                            };
                            let trig =
                                if matches!(m.triggers, super::group_config::TriggerDef::None) {
                                    ""
                                } else {
                                    " has triggers"
                                };
                            ctx.println(&format!(
                                "  {}\t<{:?}>{id}{chan}{trig}",
                                m.field_name, m.mapping
                            ));
                        }
                    }
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Port of EPICS Base `epicsStrGlobMatch` (`misc/epicsString.c`):
/// `*` matches any run of characters, `?` matches exactly one, all
/// other characters are literal. Used by `pvxgl` to filter group names,
/// matching pvxs `epicsStrGlobMatch(groupName, pattern)`.
fn glob_match(s: &str, pattern: &str) -> bool {
    let text: Vec<char> = s.chars().collect();
    let pat: Vec<char> = pattern.chars().collect();
    let mut i = 0usize; // index into text
    let mut p = 0usize; // index into pattern
    let mut mp: Option<usize> = None; // pattern resume pos after a '*'
    let mut cp = 0usize; // text resume pos for that '*'

    while i < text.len() && p < pat.len() && pat[p] != '*' {
        if pat[p] != text[i] && pat[p] != '?' {
            return false;
        }
        p += 1;
        i += 1;
    }
    while i < text.len() {
        if p < pat.len() && pat[p] == '*' {
            p += 1;
            if p >= pat.len() {
                return true;
            }
            mp = Some(p);
            cp = i + 1;
        } else if p < pat.len() && (pat[p] == text[i] || pat[p] == '?') {
            p += 1;
            i += 1;
        } else if let Some(m) = mp {
            p = m;
            i = cp;
            cp += 1;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p >= pat.len()
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
/// `processGroups`, `qsrvStats`, the pvxs-compatible `pvxsl` / `pvxgl`
/// list diagnostics, and `resetGroups`) bound to `provider`. Drop the
/// returned vector into
/// [`epics_base_rs::server::ioc_app::IocRunConfig::shell_commands`].
pub fn register_qsrv_commands(provider: Arc<BridgeProvider>) -> Vec<CommandDef> {
    vec![
        db_load_group_command(provider.clone()),
        process_groups_command(provider.clone()),
        qsrv_stats_command(provider.clone()),
        pvxsl_command(provider.clone()),
        pvxgl_command(provider.clone()),
        reset_groups_command(provider),
    ]
}

/// Build the QSRV **runtime** (interactive-shell) command set bound to
/// `provider`: `processGroups`, `qsrvStats`, `pvxsl`, `pvxgl`,
/// `resetGroups`. Deliberately excludes `dbLoadGroup` — that command is
/// registered as a base startup command (it queues group files for the
/// runner before iocInit, pvxs only permits it before `iocInit`,
/// groupsourcehooks.cpp:99-123). The QSRV protocol runner registers this
/// set into the post-iocInit interactive shell, bound to the *same*
/// `BridgeProvider` that the served `QsrvPvStore` wraps — so post-init
/// `processGroups` / `pvxgl` act on the served groups, not a throwaway
/// provider.
pub fn register_qsrv_runtime_commands(provider: Arc<BridgeProvider>) -> Vec<CommandDef> {
    vec![
        process_groups_command(provider.clone()),
        qsrv_stats_command(provider.clone()),
        pvxsl_command(provider.clone()),
        pvxgl_command(provider.clone()),
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
    fn register_qsrv_commands_includes_pvxs_list_commands() {
        let db = Arc::new(PvDatabase::new());
        let provider = Arc::new(BridgeProvider::new(db));
        let cmds = register_qsrv_commands(provider);
        assert_eq!(cmds.len(), 6);
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"dbLoadGroup"));
        assert!(names.contains(&"processGroups"));
        assert!(names.contains(&"qsrvStats"));
        // pvxs-compatible list diagnostics.
        assert!(names.contains(&"pvxsl"));
        assert!(names.contains(&"pvxgl"));
        assert!(names.contains(&"resetGroups"));
    }

    #[test]
    fn glob_match_star_question_literal() {
        // EPICS `epicsStrGlobMatch` semantics.
        assert!(glob_match("TEST:grp", "TEST:*"));
        assert!(glob_match("TEST:grp", "*grp"));
        assert!(glob_match("TEST:grp", "TEST:gr?"));
        assert!(glob_match("TEST:grp", "*"));
        assert!(glob_match("abc", "a*c"));
        assert!(!glob_match("TEST:grp", "OTHER:*"));
        assert!(!glob_match("TEST:grp", "TEST:gr"));
        assert!(!glob_match("abc", "a?"));
        assert!(glob_match("", "*"));
    }

    #[test]
    fn macro_substitution_replaces_tokens() {
        let mut m = std::collections::HashMap::new();
        m.insert("PVNAME".to_string(), "TEST:val".to_string());
        m.insert("UNIT".to_string(), "deg".to_string());
        // both `${...}` and `$(...)` forms resolve (macLib `macEnd`).
        let s = expand_macros(r#"{"+id": "${PVNAME}_$(UNIT)", "+atomic": false}"#, &m).unwrap();
        assert_eq!(s, r#"{"+id": "TEST:val_deg", "+atomic": false}"#);
    }

    /// Regression R0604-BRQSRV-MACLIB-ERROR-STATE-1.
    ///
    /// An undefined macro with no default is an expansion *error*: C `refer`
    /// sets `entry->error`, so `macExpandString` returns a negative length and
    /// `macDefExpand` returns `NULL` (macCore.c:895-896,210,220). pvxs skips
    /// the whole line on `NULL` (groupconfigprocessor.cpp:99-103); it never
    /// feeds the `$(name,undefined)` placeholder to the JSON parser. So
    /// `expand_macros` must return `None`, not a placeholder string.
    #[test]
    fn macro_undefined_signals_error() {
        let m = std::collections::HashMap::new();
        assert_eq!(expand_macros("${R0604_MACRO_NOT_SET_ANYWHERE}", &m), None);
        // Undefined inside a quoted `+channel` string (the pvxs line shape):
        // still an error, so the line is dropped rather than registering a
        // `$(...,undefined)` channel.
        assert_eq!(
            expand_macros(
                r#"  "value": { "+channel": "$(R0604_MISSING_CHANNEL)", "+type": "plain" },"#,
                &m
            ),
            None
        );
        // Undefined inside a group-name key is likewise an error.
        assert_eq!(
            expand_macros(r#"  "$(R0604_MISSING_GROUP):grp": {"#, &m),
            None
        );
    }

    #[test]
    fn macro_default_value() {
        // `$(NAME=default)` / `${NAME=default}` supply a fallback used
        // when the macro is otherwise undefined (macLib default arm).
        let m = std::collections::HashMap::new();
        // A present, resolvable default is NOT an error (C uses the default
        // arm and leaves `entry->error` clear, macCore.c:890-893).
        assert_eq!(expand_macros("${P=DEFAULT}", &m).unwrap(), "DEFAULT");
        let mut m2 = std::collections::HashMap::new();
        m2.insert("P".to_string(), "SET".to_string());
        assert_eq!(expand_macros("$(P=DEFAULT)", &m2).unwrap(), "SET");
    }

    #[test]
    fn macro_nested_reference() {
        // `${BAR=${FOO}}`: the default is itself macro-expanded
        // (macLib `macDefExpandTest.c:193-218`).
        let mut m = std::collections::HashMap::new();
        m.insert("FOO".to_string(), "fromfoo".to_string());
        assert_eq!(expand_macros("${BAR=${FOO}}", &m).unwrap(), "fromfoo");
    }

    #[test]
    fn macro_scoped_definition() {
        // `${BAR,BAR=$(FOO)}`: a scoped definition assigns BAR for the
        // duration of this reference's expansion.
        let mut m = std::collections::HashMap::new();
        m.insert("FOO".to_string(), "scopedval".to_string());
        assert_eq!(expand_macros("${BAR,BAR=$(FOO)}", &m).unwrap(), "scopedval");
    }

    #[test]
    fn macro_chained_reexpansion() {
        // a resolved value is re-scanned for further references.
        let mut m = std::collections::HashMap::new();
        m.insert("A".to_string(), "$(B)".to_string());
        m.insert("B".to_string(), "deep".to_string());
        assert_eq!(expand_macros("$(A)", &m).unwrap(), "deep");
    }

    /// Regression R0604-BRQSRV-MACLIB-DISCARD-LEVEL-1.
    ///
    /// C `macCore.c` expands a substituted macro value at `level > 0`, where
    /// `discard` removes the value's quote delimiters and escape backslashes
    /// (macCore.c:716-726,740-743). A site macro `P="IOC:"` therefore expands
    /// to `IOC:`, not `"IOC:"`. The Rust expander used to copy quotes/escapes
    /// from every level, corrupting JSON or PV names substituted from quoted
    /// macro values.
    #[test]
    fn macro_value_quotes_and_escapes_discarded() {
        // Macro value with surrounding quotes → quotes removed on expansion.
        let mut m = std::collections::HashMap::new();
        m.insert("P".to_string(), "\"IOC:\"".to_string());
        assert_eq!(expand_macros("$(P)grp", &m).unwrap(), "IOC:grp");

        // Macro value with an escape → backslash dropped, escaped byte kept.
        let mut m = std::collections::HashMap::new();
        m.insert("E".to_string(), "a\\,b".to_string());
        assert_eq!(expand_macros("$(E)", &m).unwrap(), "a,b");

        // Default value quotes are likewise discarded (substituted level+1),
        // including interior quotes a one-pair strip would have left behind.
        let m = std::collections::HashMap::new();
        assert_eq!(expand_macros(r#"$(X="a"b)"#, &m).unwrap(), "ab");

        // Scoped value quotes are discarded too.
        let m = std::collections::HashMap::new();
        assert_eq!(expand_macros(r#"$(BAR,BAR="v")"#, &m).unwrap(), "v");
    }

    /// Regression R0604-BRQSRV-MACLIB-DISCARD-LEVEL-1 (level-0 preservation).
    ///
    /// The user's own source string is translated at level 0 (`discard =
    /// false`), so its quote delimiters and escape backslashes survive — only
    /// substituted macro values are normalized (macExpandString runs
    /// `trans(..., 0, ...)`, macCore.c:216).
    #[test]
    fn macro_user_quotes_preserved_at_level_zero() {
        let mut m = std::collections::HashMap::new();
        m.insert("V".to_string(), "x".to_string());
        // The literal JSON quotes around the value are the user's quotes and
        // must remain; only `$(V)` is substituted.
        assert_eq!(
            expand_macros(r#"{"k": "$(V)"}"#, &m).unwrap(),
            r#"{"k": "x"}"#
        );
    }

    /// Regression R0604-BRQSRV-MACLIB-ERROR-STATE-1.
    ///
    /// `A=$(A)` must not recurse forever (macLib `visited`), and the recursion
    /// is an *error*: C `refer` sets `entry->error` on the recursive reference,
    /// so `macExpandString` returns a negative length and `macDefExpand`
    /// returns `NULL` (macCore.c:880-882,210,220). pvxs then drops the line, so
    /// the recursive macro never reaches the JSON parser as literal text —
    /// `expand_macros` must return `None`, not the resolved value.
    #[test]
    fn macro_self_reference_signals_error() {
        let mut m = std::collections::HashMap::new();
        m.insert("A".to_string(), "$(A)".to_string());
        assert_eq!(expand_macros("$(A)", &m), None);
    }

    /// Regression R0604-BRQSRV-MACLIB-ERROR-STATE-1.
    ///
    /// An undefined/recursive reference inside a *scoped definition* uses C's
    /// separate `MAC_ENTRY subs`, whose error flag is discarded
    /// (macCore.c:821-826,841,849). It must NOT fail the enclosing expansion,
    /// so a reference whose own name resolves still succeeds even when a
    /// scoped-definition value it never uses is undefined.
    #[test]
    fn macro_scoped_definition_error_does_not_fail_outer() {
        let mut m = std::collections::HashMap::new();
        m.insert("P".to_string(), "val".to_string());
        // P resolves; the scoped `K=$(UNDEF)` value is undefined but its error
        // is confined to the scoped `subs` entry, so the whole expansion is
        // still successful.
        assert_eq!(expand_macros("$(P,K=$(UNDEF))", &m).unwrap(), "val");
    }

    /// Regression R0604-BRQSRV-MACLIB-ERROR-STATE-1.
    ///
    /// pvxs expands each group-config line through `macDefExpand` and skips a
    /// line whose expansion returns `NULL` (groupconfigprocessor.cpp:88-105).
    /// An undefined macro inside a quoted `+channel` value must therefore never
    /// register as a literal `$(MISSING,undefined)` channel: the line is
    /// dropped, leaving no such group.
    #[test]
    fn group_file_undefined_macro_line_dropped() {
        let db = Arc::new(PvDatabase::new());
        let provider = Arc::new(BridgeProvider::new(db));
        // Single self-contained line whose only macro is undefined. With the
        // line dropped the buffer is empty, so parsing fails and — crucially —
        // no group named after the placeholder is ever created.
        let json = r#"{ "G:grp": { "value": { "+channel": "$(R0604_MISSING_CHANNEL)", "+type": "plain" } } }"#;
        let path = std::env::temp_dir().join("qsrv_iocsh_undef_macro_line.json");
        std::fs::write(&path, json).unwrap();
        // Non-empty macros enables expansion (pvxs `macros[0] != '\0'` gate).
        let res = apply_group_file(&provider, path.to_str().unwrap(), "DUMMY=1");
        let _ = std::fs::remove_file(&path);
        assert!(
            res.is_err(),
            "an all-dropped file must not parse into a group, got {res:?}"
        );
        assert_eq!(provider.group_count(), 0);
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
