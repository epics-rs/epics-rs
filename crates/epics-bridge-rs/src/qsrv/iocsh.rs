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

// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the exec-backend
// suite.

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
/// `groupsourcehooks.cpp:154`); with an empty macros argument the JSON
/// lines are used verbatim and no macro/env expansion happens at all.
/// We mirror that: an empty `macros` string skips expansion entirely.
pub fn db_load_group_command(provider: Arc<BridgeProvider>) -> CommandDef {
    CommandDef::new(
        "dbLoadGroup",
        vec![
            ArgDesc {
                name: "filename",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "macros",
                arg_type: ArgType::String,
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
/// pvxs's `macros[0]!='\0'` guard, groupsourcehooks.cpp:154), and merge the
/// parsed groups into the provider under the `(filename, macros)` source
/// identity so a later `dbLoadGroup("-file", macros)` can remove them.
/// Returns the running group count.
///
/// This is the single load path shared by the standalone
/// [`db_load_group_command`] and the QSRV protocol runner, which calls it
/// for every entry drained from the base `dbLoadGroup` startup queue
/// (pvxs `GroupConfigProcessor::loadConfigFiles`,
/// groupsourcehooks.cpp:201).
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
        // `macDefExpand` and, when expansion fails (any of macLib's three
        // error arms — an undefined name, a recursive reference, or one
        // never closed — makes `macExpandString` return a negative length,
        // so `macDefExpand` returns `NULL`), logs an error and skips that
        // line — the failed line never reaches the JSON buffer
        // (groupconfigprocessor.cpp:88-106). Expanding line-by-line, rather
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
                    // The cause is already on the operator's console and is
                    // not this line's to restate: the expansion runs with
                    // `suppress_warnings` off, so macLib itself wrote
                    // `macLib: macro … is undefined` / `… is recursive` /
                    // `macLib: unterminated macro reference in string …`
                    // for this very line, naming the macro, before it
                    // returned. A cause list here is a second copy of that
                    // knowledge with no way to stay in step with it — it
                    // already said "undefined or recursive macro" after the
                    // unterminated arm began reaching this branch. What only
                    // this layer knows is WHICH line was dropped.
                    tracing::error!(
                        file = %filename,
                        line = idx + 1,
                        "dbLoadGroup: macro expansion failed; skipping line"
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

/// Parse a `name=value,name=value` macro-definition string into a map,
/// the way libCom `macParseDefns` does (`macUtil.c:74-196`): commas
/// separate pairs and `=` separates name from value, but a comma or `=`
/// inside single/double quotes or backslash-escaped is a literal, and
/// unquoted whitespace around names/values is trimmed. So `DESC="a,b"`
/// keeps `a,b` as one value instead of truncating it at the embedded
/// comma, and `DESC=a\,b` is equally literal.
///
/// Splitting is delegated to the canonical quote-aware splitter
/// [`epics_base_rs::server::iocsh::macro_defn_pairs`] — the single owner
/// of the `macParseDefns` grammar — rather than a second raw `split(',')`.
/// A name with no `=` is a macLib deletion entry; against the fresh map
/// built here it has nothing to remove, so it is skipped (matching
/// `macInstallMacros`).
///
/// Env-var fallback is intentionally NOT applied here. pvxs seeds the
/// `environ` scope into the `MAC_HANDLE` and resolves `$(...)` references
/// at `macExpandString` time with supplied macros taking precedence over
/// the environment (`groupsourcehooks.cpp:154-170`). QSRV mirrors that in
/// [`expand_macros`], which is the sole expansion owner; resolving the
/// environment eagerly at parse time (as the base `parse_macro_string`
/// does) would let env values shadow chained supplied-macro references.
fn parse_macros(s: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for (k, v) in epics_base_rs::server::iocsh::macro_defn_pairs(s) {
        if let Some(v) = v {
            if k.is_empty() {
                continue;
            }
            out.insert(k, v);
        }
    }
    out
}

/// Expand `$(...)` / `${...}` macro references in `s`, mirroring the C
/// EPICS Base `macLib` engine (`modules/libcom/src/macLib/macCore.c`
/// `trans` / `refer` / `lookup`) that pvxs drives through `macDefExpand`
/// (`groupconfigprocessor.cpp:88-106`). Implemented behaviors:
///
///   - `$(NAME)` and `${NAME}` are both references; `\<char>` blocks
///     detection. At level 0 (the user's source string) both bytes are
///     copied verbatim; at a discard level (a substituted macro value) the
///     escape backslash is dropped and only the escaped byte is kept
///     (`trans:741-744`, `discard = level > 0`).
///   - quote delimiters are kept at level 0 (the user's quotes) but removed
///     from substituted macro values/names/defaults/scopes
///     (`trans:717-726`).
///   - macros are NOT expanded inside single quotes (`trans:734-737`).
///   - a reference name is itself macro-expanded before lookup, so
///     `$($(WHICH))` resolves the inner reference first.
///   - the name terminates at `=`, `,`, or the closing bracket
///     (`macEnd = "=,)"`): `$(NAME=default)` supplies a default and
///     `$(NAME,key=val)` introduces scoped macros visible only inside
///     that reference's expansion.
///   - lookup order is scoped frames (innermost first), then the
///     supplied `macros`, then the process environment (pvxs creates
///     the handle with the `{"","environ"}` pair —
///     `groupsourcehooks.cpp:156-158`, `lookup`+`FLAG_USE_ENVIRONMENT`).
///   - a resolved (macro or env) value is re-scanned for further
///     references (chained expansion); a self-referential macro is a
///     recursive reference and fails the expansion (`refentry->visited`).
///   - three arms set `entry->error`: an undefined name with no default
///     and a recursive reference, which leave the errval placeholders
///     `,undefined)` / `,recursive)`, and a reference whose closing
///     delimiter never matched its opener, which writes no placeholder at
///     all and copies itself and the whole rest of the string through
///     verbatim (`macCore.c:862-875`). `macExpandString` then returns a
///     negative length and `macDefExpand` returns `NULL`
///     (macCore.c:210,220,895-896,881-882), so this function returns `None`
///     instead of a string. pvxs's group loader skips the whole line on
///     `NULL`, so a placeholder can never register as a literal channel or
///     group name (groupconfigprocessor.cpp:91-103).
///   - a fault inside a scoped DEFINITION is the exception that sets no
///     error: C translates the `,k=v` list through a separate `MAC_ENTRY
///     subs` whose flag is never merged back (`macCore.c:820-826`), so
///     `$(P,K=$(UNDEF))` with `P` defined expands to `P`'s value and
///     succeeds.
///
/// Returns `None` exactly when C `macDefExpand()` would return `NULL` for the
/// same input + macro set; `Some(expanded)` otherwise.
fn expand_macros(s: &str, macros: &std::collections::HashMap<String, String>) -> Option<String> {
    // The engine is `epics-base-rs`'s, not a copy of it. This used to be a
    // private fork — `mac_trans`/`mac_refer`/`mac_parse_scoped` plus two
    // byte-identical `top_level_*` helpers — and the fork is what let C
    // `refer`'s unterminated arm (`macCore.c:862-875`) stay open here after it
    // was closed in base: a `$(` with no `)` returned `None`, the caller
    // emitted a bare `$`, the shared `error` was never set, and pvxs's
    // skip-the-line path never fired. One owner, one arm.
    //
    // The three options are pvxs's, not base's `.db` defaults:
    //
    //   * `env_fallback` — pvxs builds the handle with the `{"", "environ"}`
    //     pair, so an unset name falls through to the process environment
    //     (`groupsourcehooks.cpp:155-158`, C `lookup` + `FLAG_USE_ENVIRONMENT`);
    //   * `dollar_escape` off — `$$` is not macLib syntax, it is an autosave
    //     `.req` convenience;
    //   * `suppress_warnings` off — `macDefExpand` never calls
    //     `macSuppressWarning` (`macEnv.c:27-79`) and pvxs does not either, so
    //     C writes the `macLib:` notice for every bad reference in a group
    //     file.
    let expanded = epics_base_rs::server::db_loader::expand_macros(
        s,
        macros,
        epics_base_rs::server::db_loader::MacroExpandOptions {
            env_fallback: true,
            dollar_escape: false,
            suppress_warnings: false,
        },
    );
    // C `macExpandString` returns the destination text even on error, but
    // `macDefExpand` discards it and returns NULL when the length is negative
    // (macCore.c:216-224, macEnv.c:58-61). `errored()` is that negative
    // length: it covers the undefined, recursive AND unterminated arms alike,
    // which is precisely what the fork could not do.
    (!expanded.errored()).then_some(expanded.text)
}

/// `processGroups` — finalize group config after `dbLoadGroup` calls
/// and (typically) `iocInit`. Validates trigger references and creates
/// the groups whose every `+channel` resolves, reporting the rest as
/// `"<group>: Error Group not created: <why>"`. Mirrors pvxs
/// `processGroups` (groupsourcehooks.cpp:192) → `createGroups`
/// (groupconfigprocessor.cpp:429-444).
pub fn process_groups_command(provider: Arc<BridgeProvider>) -> CommandDef {
    CommandDef::new(
        "processGroups",
        vec![],
        "processGroups",
        move |_args: &[ArgValue], ctx: &CommandContext| {
            let provider = provider.clone();
            let n = ctx.block_on(async move { provider.process_groups().await });
            ctx.println(&format!("processGroups: created {n} group(s)"));
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
                n.extend(db.all_alias_names());
                n
            });
            names.sort();
            names.dedup();
            for line in pvxsl_lines(&names, detail) {
                ctx.println(&line);
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Build the `pvxsl` output lines for a sorted, de-duplicated record-name
/// list, mirroring pvxs `pvxsl` (`singlesourcehooks.cpp:33-65`). Split out
/// of [`pvxsl_command`] so the exact pvxs-compatible text is unit-testable
/// without capturing stdout.
///
/// pvxs prints the `SOURCE`/`RECORDS:` header only when the source has at
/// least one name (`list.names && !list.names->empty()`), and the header
/// line is `SOURCE: <record>@<ioid><dynamic>` (`:52`). The QSRV single
/// source is registered as `qsrvSingle` at IOID 0 (`:159`) and its
/// `onList()` reports a static record set (`List::dynamic` defaults false,
/// `source.h:277`; `SingleSource::onList` returns `allRecords` unchanged,
/// `singlesource.h:32`), so the ` [dynamic]` suffix is never appended.
fn pvxsl_lines(names: &[String], detail: i64) -> Vec<String> {
    let mut out = Vec::new();
    if detail != 0 && !names.is_empty() {
        out.push("------------------".to_string());
        out.push("SOURCE: qsrvSingle@0".to_string());
        out.push("------------------".to_string());
        out.push("RECORDS: ".to_string());
    }
    for n in names {
        if detail != 0 {
            out.push(format!("  {n}"));
        } else {
            out.push(n.clone());
        }
    }
    out
}

/// `pvxgl [<level>] [<pattern>]` — list QSRV group PV names, optionally
/// filtered by a glob `pattern` and, when `level > 0`, with group
/// detail. Mirrors pvxs `pvxsgl` (`groupsourcehooks.cpp:57-83`,
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
            },
            ArgDesc {
                name: "pattern",
                arg_type: ArgType::String,
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
            for line in pvxgl_lines(&groups, level, pattern) {
                ctx.println(&line);
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Build the `pvxgl` output lines for a group-definition map, mirroring
/// pvxs `pvxsgl` (`groupsourcehooks.cpp:57-83`) plus `Group::show`
/// (`group.cpp:61-94`). Split out of [`pvxgl_command`] so the exact
/// pvxs-compatible text is unit-testable without capturing stdout.
///
/// Groups are listed in sorted name order; an empty `pattern` matches
/// every group (pvxs `!pattern[0]`). `level > 0` prints the atomic flag
/// and member count; `level > 1` prints one line per member as
/// `  <fieldName>\t<mappingName><id><chan><has triggers>` — the mapping
/// token comes from [`super::pvif::FieldMapping::pvxs_name`] (lowercase, matching
/// `MappingInfo::name`, `typeutils.cpp:65`), not Rust `Debug`. The
/// ` has triggers` suffix and the `level > 2` per-trigger-target lines are
/// both derived from the one resolved trigger set
/// ([`super::group_config::GroupPvDef::resolved_trigger_targets`]) so they can never disagree,
/// exactly as pvxs derives both from `field.triggers`.
fn pvxgl_lines(
    groups: &std::collections::HashMap<String, super::group_config::GroupPvDef>,
    level: i64,
    pattern: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut names: Vec<&String> = groups.keys().collect();
    names.sort();
    for name in names {
        if !pattern.is_empty() && !glob_match(name, pattern) {
            continue;
        }
        out.push(name.clone());
        if level > 0 {
            let def = &groups[name];
            out.push(format!(
                "  Atomic Get/Put:{} Atomic Members:{}",
                if def.atomic { "yes" } else { "no" },
                def.members.len()
            ));
            if level > 1 {
                for (idx, m) in def.members.iter().enumerate() {
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
                    let targets = def.resolved_trigger_targets(idx);
                    let trig = if targets.is_empty() {
                        ""
                    } else {
                        " has triggers"
                    };
                    out.push(format!(
                        "  {}\t<{}>{id}{chan}{trig}",
                        m.field_name,
                        m.mapping.pvxs_name()
                    ));
                    if level > 2 {
                        for t in &targets {
                            out.push(format!("    {t}"));
                        }
                    }
                }
            }
        }
    }
    out
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

/// QSRV2-gated runtime-command installer. pvxs registers the QSRV command
/// surface — `pvxsl` (`single_enable()`, singlesourcehooks.cpp:162-166),
/// `pvxgl` / `dbLoadGroup` (`group_enable()`, groupsourcehooks.cpp:233-245)
/// — only inside `if(enableQ)` in `pvxsBaseRegistrar()`
/// (iochooks.cpp:492-496), where `enableQ` is the one `enable2()` decision
/// that also gates `addSource()`. When QSRV2 is disabled (`PVXS_QSRV_ENABLE=NO`
/// or `EPICS_IOC_IGNORE_SERVERS=qsrv2`), none of those commands exist.
///
/// This is the single owner of that gate for the runtime (post-iocInit)
/// command set: when `enabled` is false it returns an empty set, so a
/// disabled IOC exposes no `processGroups` / `qsrvStats` / `pvxsl` / `pvxgl`
/// / `resetGroups` control surface (`pvxsl` reads the database directly and
/// `resetGroups` mutates the group registry — both must be absent when the
/// QSRV2 sources are not served). It is bound to the same `qsrv2_on` decision
/// that gates serving and group loading, so the whole QSRV surface follows
/// one decision rather than each call site re-deciding.
pub fn register_qsrv_runtime_commands_if_enabled(
    enabled: bool,
    provider: Arc<BridgeProvider>,
) -> Vec<CommandDef> {
    if enabled {
        register_qsrv_runtime_commands(provider)
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use epics_base_rs::server::database::PvDatabase;

    #[tokio::test]
    async fn db_load_group_then_process_succeeds() {
        let db = Arc::new(PvDatabase::new());
        // `processGroups` creates only groups whose every `+channel`
        // resolves (pvxs `createGroups`), so the backing record must exist.
        db.add_record(
            "TEST:val",
            Box::new(epics_base_rs::server::records::ai::AiRecord::new(1.0)),
        )
        .await
        .unwrap();
        let provider = Arc::new(BridgeProvider::new(db));
        let json = r#"{
            "TEST:grp": {
                "+id": "epics:nt/NTScalar:1.0",
                "+atomic": true,
                "value": { "+channel": "TEST:val.VAL", "+type": "plain" }
            }
        }"#;
        let tmpdir = tempfile::tempdir().expect("fixture root");
        let path = tmpdir.path().join("qsrv_iocsh_test.json");
        std::fs::write(&path, json).unwrap();

        provider.load_group_file(path.to_str().unwrap()).unwrap();
        assert_eq!(provider.group_count(), 1);

        let n = provider.process_groups().await;
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

    /// pvxs gates `single_enable()` / `group_enable()` command registration
    /// behind `enable2()` (iochooks.cpp:492-496). A QSRV2-enabled IOC
    /// installs the full runtime set; a disabled IOC
    /// (`PVXS_QSRV_ENABLE=NO` / `EPICS_IOC_IGNORE_SERVERS=qsrv2`) installs
    /// none of it — no `processGroups`/`qsrvStats`/`pvxsl`/`pvxgl`/
    /// `resetGroups` control surface.
    #[test]
    fn qsrv_runtime_commands_gated_on_qsrv2_enable() {
        let db = Arc::new(PvDatabase::new());
        let provider = Arc::new(BridgeProvider::new(db));

        let enabled = register_qsrv_runtime_commands_if_enabled(true, provider.clone());
        assert_eq!(enabled.len(), 5);
        let names: Vec<&str> = enabled.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"processGroups"));
        assert!(names.contains(&"qsrvStats"));
        assert!(names.contains(&"pvxsl"));
        assert!(names.contains(&"pvxgl"));
        assert!(names.contains(&"resetGroups"));
        // `dbLoadGroup` is a base startup command, never in the runtime set.
        assert!(!names.contains(&"dbLoadGroup"));

        let disabled = register_qsrv_runtime_commands_if_enabled(false, provider);
        assert!(disabled.is_empty());
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

    /// Borrow a `Vec<String>` as `Vec<&str>` for exact-output assertions
    /// against string-literal arrays.
    fn as_strs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    fn groups_from_json(
        json: &str,
    ) -> std::collections::HashMap<String, super::super::group_config::GroupPvDef> {
        super::super::group_config::parse_group_config(json)
            .unwrap()
            .into_iter()
            .map(|d| (d.name.clone(), d))
            .collect()
    }

    /// `pvxsl 1` prints the pvxs source identity `SOURCE: qsrvSingle@0`
    /// (singlesourcehooks.cpp:52 with the `qsrvSingle`/IOID-0 registration
    /// at :159 and a static, non-dynamic `onList()`), inside the
    /// `------`/`RECORDS:` frame — not the Rust-only
    /// `SOURCE: qsrv single records`. `pvxsl 0` prints bare names. The
    /// header is suppressed when the source serves no names
    /// (`list.names && !list.names->empty()`).
    #[test]
    fn pvxsl_lines_match_pvxs_source_identity() {
        let names = vec!["REC:a".to_string(), "REC:b".to_string()];
        assert_eq!(as_strs(&pvxsl_lines(&names, 0)), ["REC:a", "REC:b"]);
        assert_eq!(
            as_strs(&pvxsl_lines(&names, 1)),
            [
                "------------------",
                "SOURCE: qsrvSingle@0",
                "------------------",
                "RECORDS: ",
                "  REC:a",
                "  REC:b",
            ]
        );
        // No names -> no SOURCE header at all.
        assert!(pvxsl_lines(&[], 1).is_empty());
    }

    /// `pvxgl 2` prints each member mapping through the lowercase
    /// `MappingInfo::name()` spelling (`<scalar>`/`<plain>`/`<meta>`/
    /// `<const>`/`<structure>`), mirroring pvxs `Group::show`
    /// (group.cpp:73-80; typeutils.cpp:65) — never the capitalized Rust
    /// `Debug` variant names. Channel-less `const`/`structure` members
    /// carry no `chan=` and (being `TriggerDef::None`) no ` has triggers`.
    #[test]
    fn pvxgl_lines_mapping_names_lowercase_like_pvxs() {
        let json = r#"{
            "TEST:grp": {
                "+id": "epics:nt/NTTable:1.0",
                "+atomic": true,
                "s": { "+channel": "REC:s.VAL", "+type": "scalar" },
                "p": { "+channel": "REC:p.VAL", "+type": "plain" },
                "m": { "+channel": "REC:m.VAL", "+type": "meta" },
                "k": { "+const": 7, "+type": "const" },
                "n": { "+type": "structure" }
            }
        }"#;
        let groups = groups_from_json(json);
        // Members are emitted in canonical (put_order, field_name) order;
        // none carry +putorder, so the order is alphabetical by field name.
        assert_eq!(
            as_strs(&pvxgl_lines(&groups, 2, "")),
            [
                "TEST:grp",
                "  Atomic Get/Put:yes Atomic Members:5",
                "  k\t<const>",
                "  m\t<meta> chan=REC:m.VAL has triggers",
                "  n\t<structure>",
                "  p\t<plain> chan=REC:p.VAL has triggers",
                "  s\t<scalar> chan=REC:s.VAL has triggers",
            ]
        );
    }

    /// `pvxgl 3` prints, under each member, one line per resolved
    /// `+trigger` target field name, mirroring pvxs `Group::show`
    /// level>2 (group.cpp:81-92). Targets are sorted to match pvxs's
    /// `std::set<std::string>` ordering. A member with an explicit
    /// `+trigger` in a group that has triggers keeps its targets; a
    /// sibling without one is demoted to silence (`resolve_self_trigger_
    /// default`) and shows neither ` has triggers` nor target lines.
    #[test]
    fn pvxgl_lines_level3_prints_trigger_targets() {
        let json = r#"{
            "TRIG:grp": {
                "+atomic": true,
                "a": { "+channel": "REC:a.VAL", "+type": "scalar", "+trigger": "a,b" },
                "b": { "+channel": "REC:b.VAL", "+type": "scalar" }
            }
        }"#;
        let groups = groups_from_json(json);
        assert_eq!(
            as_strs(&pvxgl_lines(&groups, 3, "")),
            [
                "TRIG:grp",
                "  Atomic Get/Put:yes Atomic Members:2",
                "  a\t<scalar> chan=REC:a.VAL has triggers",
                "    a",
                "    b",
                "  b\t<scalar> chan=REC:b.VAL",
            ]
        );
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
        // arm and leaves `entry->error` clear, macCore.c:890-894).
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

    /// C `macCore.c` expands a substituted macro value at `level > 0`, where
    /// `discard` removes the value's quote delimiters and escape backslashes
    /// (macCore.c:717-726,741-744). A site macro `P="IOC:"` therefore expands
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

    /// Level-0 preservation.
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

    /// `A=$(A)` must not recurse forever (macLib `visited`), and the recursion
    /// is an *error*: C `refer` sets `entry->error` on the recursive reference,
    /// so `macExpandString` returns a negative length and `macDefExpand`
    /// returns `NULL` (macCore.c:881-882,210,220). pvxs then drops the line, so
    /// the recursive macro never reaches the JSON parser as literal text —
    /// `expand_macros` must return `None`, not the resolved value.
    #[test]
    fn macro_self_reference_signals_error() {
        let mut m = std::collections::HashMap::new();
        m.insert("A".to_string(), "$(A)".to_string());
        assert_eq!(expand_macros("$(A)", &m), None);
    }

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

    /// pvxs expands each group-config line through `macDefExpand` and skips a
    /// line whose expansion returns `NULL` (groupconfigprocessor.cpp:88-106).
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
        let tmpdir = tempfile::tempdir().expect("fixture root");
        let path = tmpdir.path().join("qsrv_iocsh_undef_macro_line.json");
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

    /// C `refer`'s unterminated arm (`macCore.c:862-875`), which this file
    /// carried open for as long as it had its own copy of the expander.
    ///
    /// A `$(`/`${` whose closing delimiter never arrives is not a reference:
    /// C copies it and the whole rest of the string through verbatim, sets
    /// `entry->error`, and writes `macLib: unterminated macro reference in
    /// string …`. `macExpandString` then returns a negative length, so
    /// `macDefExpand` returns `NULL` and pvxs skips the line
    /// (groupconfigprocessor.cpp:91-103). The fork returned `Some` with the
    /// `$` copied through and the shared `error` untouched, so the line
    /// survived into the JSON buffer carrying a literal `$(`.
    ///
    /// All four shapes, measured against `softIoc R7.0.10`: a plain `$(`, the
    /// `${` opener, a MISMATCHED `$(A}`, and two openers with no closer. The
    /// third and fourth matter because the tail after the opener is text — the
    /// `$(P)` in them is consumed as part of the unterminated reference's name
    /// and is never expanded, so a fork that rescanned the tail resolved a
    /// macro C leaves alone.
    #[test]
    fn an_unterminated_reference_fails_the_expansion() {
        let m = std::collections::HashMap::from([("P".to_string(), "IOC:".to_string())]);
        for s in [
            r#"  "$(P:grp": {"#,
            r#"  "${P:grp": {"#,
            r#"  "x$(A} $(P) y""#,
            r#"  "$(A$(B z""#,
        ] {
            assert_eq!(expand_macros(s, &m), None, "expanding {s:?}");
        }
        // The guard: a reference that IS closed still resolves, and the tail
        // after it is still scanned.
        assert_eq!(
            expand_macros(r#"  "$(P)grp": {"#, &m).as_deref(),
            Some(r#"  "IOC:grp": {"#)
        );
    }

    /// End to end: a group whose NAME carries an unterminated `$(` must never
    /// reach the provider.
    ///
    /// This is the hazard the line-by-line expansion exists to prevent, spelled
    /// out in [`apply_group_file`]'s own comment — pvxs drops the line rather
    /// than let a half-expanded name register as a real group. With the fork,
    /// `  "$(P:grp": {` expanded to itself with no error reported, the JSON
    /// stayed well-formed, and a group literally named `$(P:grp` was created.
    #[test]
    fn a_group_name_with_an_unterminated_reference_is_not_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("unterm-groups.json");
        std::fs::write(
            &path,
            "{\n  \"$(P:grp\": {\n    \"+id\": \"epics:nt/NTGroup:1.0\"\n  }\n}\n",
        )
        .expect("write the group file");

        let db = std::sync::Arc::new(epics_base_rs::server::database::PvDatabase::new());
        let provider = BridgeProvider::new(db);
        let loaded = apply_group_file(&provider, path.to_str().expect("utf-8 path"), "P=IOC:");

        assert!(
            loaded.is_err(),
            "the skipped line leaves malformed JSON, so the load must fail; got {loaded:?}"
        );
        let names: Vec<String> = provider.groups().into_keys().collect();
        assert!(
            !names.iter().any(|n| n.contains("$(")),
            "no group may be created from a half-expanded name: {names:?}"
        );
    }

    /// libCom `macParseDefns` (macUtil.c:74-196): a comma inside quotes or
    /// backslash-escaped is a literal, not a pair separator. A raw
    /// `split(',')` truncated `DESC="a,b"` to `DESC="a` and dropped `b"`;
    /// the quote-aware splitter keeps `a,b` as one value. The quotes and
    /// escapes themselves stay in the parsed value — macParseDefns removes
    /// them from names only (macUtil.c:198-200) — and the expander's
    /// `discard` level takes them off when the macro is substituted, so
    /// both halves are asserted here.
    #[test]
    fn parse_macros_keeps_quoted_or_escaped_comma_in_one_value() {
        let m = parse_macros(r#"DESC="a,b",P=IOC:"#);
        assert_eq!(m.get("DESC"), Some(&r#""a,b""#.to_string()));
        assert_eq!(expand_macros("$(DESC)", &m).as_deref(), Some("a,b"));
        assert_eq!(m.get("P"), Some(&"IOC:".to_string()));
        assert_eq!(m.len(), 2);

        let m = parse_macros(r#"DESC=a\,b,P=IOC:"#);
        assert_eq!(m.get("DESC"), Some(&r#"a\,b"#.to_string()));
        assert_eq!(expand_macros("$(DESC)", &m).as_deref(), Some("a,b"));
        assert_eq!(m.get("P"), Some(&"IOC:".to_string()));
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
