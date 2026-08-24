use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::error::{CaError, CaResult};
use crate::server::record::{Record, check_link_assignment};
use crate::types::{EpicsValue, PvString};

mod include;
mod substitution;
#[cfg(test)]
pub(crate) use include::parse_include_directive;
pub use include::{
    DbLoadConfig, db_add_path, db_open_file, db_path, expand_includes, parse_db_file,
    parse_db_file_with_breaktables,
};
pub(crate) use substitution::resolve_template;
pub use substitution::{TemplateLoad, parse_substitutions, substitution_rows};

/// Factory function that creates a record instance.
pub type RecordFactory = Box<dyn Fn() -> Box<dyn Record> + Send + Sync>;

/// Global registry of external record type factories.
/// External crates (e.g., asyn-rs) can register their own record types
/// to override built-in stubs.
static RECORD_FACTORY_REGISTRY: OnceLock<Mutex<HashMap<String, RecordFactory>>> = OnceLock::new();

fn get_registry() -> &'static Mutex<HashMap<String, RecordFactory>> {
    RECORD_FACTORY_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register an external record type factory.
/// This allows external crates to override built-in record stubs.
/// The factory is checked FIRST in `create_record()`, so it takes priority.
pub fn register_record_type(name: &str, factory: RecordFactory) {
    let mut reg = get_registry()
        .lock()
        .expect("record factory registry mutex poisoned");
    reg.insert(name.to_string(), factory);
}

/// A record definition parsed from a .db file.
#[derive(Debug, Clone)]
pub struct DbRecordDef {
    pub record_type: String,
    pub name: String,
    /// The record's fields, in file order. A `.db` value is a BYTE STRING, not
    /// text: C's escape translation writes one byte per `\xHH`
    /// (`epicsString.c:106`), and a DBF_STRING's budget is 40 BYTES. Modelling
    /// the value as a Rust `String` made `\xff` two bytes (R19-68).
    pub fields: Vec<(String, PvString)>,
    /// Aliases declared inside the record body (`alias("name")`).
    /// Mirrors epics-base PR #336 — alias names are validated with
    /// the same rules as record names.
    pub aliases: Vec<String>,
    /// `info(tag, value)` pairs declared inside the record body.
    /// Mirrors `info` directives in EPICS db files; common tags include
    /// `asyn:READBACK` (asyn upstream PRs #60 / #208), `Q:form`, etc.
    pub info_tags: Vec<(String, String)>,
}

/// Validate a record (or alias) name against epics-base PR #78 rules.
///
/// Returns `Err(CaError::DbParseError)` for the hard-error cases (empty
/// name, embedded space/tab/quote/dot/dollar). Logs `tracing::warn!`
/// for the soft-warning cases (leading `-`/`+`/`[`/`{`, embedded
/// non-printable characters); the parse continues so legacy databases
/// still load.
///
/// The check runs **after** macro substitution, mirroring base where
/// `dbRecordHead` is invoked from the lexer with the substituted name.
pub(crate) fn validate_record_name(name: &str, line: usize, col: usize) -> CaResult<()> {
    if name.is_empty() {
        return Err(CaError::DbParseError {
            line,
            column: col,
            message: "record/alias name can't be empty".into(),
        });
    }
    for (i, c) in name.chars().enumerate() {
        if i == 0 && matches!(c, '-' | '+' | '[' | '{') {
            tracing::warn!(name, "record/alias name should not begin with '{}'", c);
        }
        // Non-printable ASCII (< space) is a warning, not an error —
        // matches base PR #78's `errlogPrintf("Warning: ...")` branch.
        if (c as u32) < 0x20 {
            tracing::warn!(
                name,
                "record/alias name should not contain non-printable 0x{:02X}",
                c as u32
            );
            continue;
        }
        if matches!(c, ' ' | '\t' | '"' | '\'' | '.' | '$') {
            return Err(CaError::DbParseError {
                line,
                column: col,
                message: format!("bad character '{c}' in record/alias name \"{name}\""),
            });
        }
    }
    Ok(())
}

/// The single owner of C's `yyerror` / `yyerrorAbort` distinction.
///
/// C's `.db` reader has two error routines and uses both inside the same
/// file: `yyerror(NULL)` (`dbLexRoutines.c:452`, `:1175`) records the
/// failure, leaves everything already parsed in `pdbbase`, and lets
/// `pvt_yy_parse` return -1 once the whole text has been read
/// (`dbYacc.y:370-397`); `yyerrorAbort` (`:1133`, `:1159-1165`) stops the
/// parse outright.
///
/// Invariant: a per-item failure records a diagnostic and a non-zero final
/// status but MUST NOT discard items already parsed; only an abort-class
/// failure — an `Err` return out of the parse — stops it. Every
/// recoverable site reports through [`DbFaults::recoverable`], which is
/// the one place that decides what a recoverable failure does, so no
/// caller has to re-derive the classification.
#[derive(Debug, Default)]
pub struct DbFaults {
    messages: Vec<String>,
}

impl DbFaults {
    /// C `yyerror(NULL)`: print the diagnostic, remember that the load
    /// failed, and let the caller carry on with the next item.
    pub fn recoverable(&mut self, message: String) {
        eprintln!("{message}");
        self.messages.push(message);
    }

    /// Take over the faults a nested parse layer collected — an
    /// `include`, a `.substitutions` row — so the outermost load still
    /// reports them exactly once.
    pub fn absorb(&mut self, other: DbFaults) {
        self.messages.extend(other.messages);
    }

    /// Whether anything recoverable was reported.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// C `pvt_yy_parse` returning -1 with `yyFailed` set: the load's
    /// status is non-zero when anything was reported. The first
    /// diagnostic is the one the caller surfaces.
    pub fn status(&self) -> Result<(), String> {
        match self.messages.split_first() {
            None => Ok(()),
            Some((first, [])) => Err(first.clone()),
            Some((first, rest)) => Err(format!("{first} (+{} more)", rest.len())),
        }
    }
}

/// Everything one `.db` text yields.
pub struct ParsedDb {
    pub records: Vec<DbRecordDef>,
    /// `breaktable(...)` definitions found in the text (C `dbBreakBody`,
    /// `dbLexRoutines.c`). The IOC builder feeds these into the
    /// breakpoint-table registry so `ai`/`ao` records with `LINR >= 3`
    /// can resolve their linearisation table.
    pub breaktables: Vec<crate::server::cvt_bpt::BrkTable>,
    /// File-scope `alias("record","new")` directives whose target is not
    /// declared in this text. C `dbAlias` (`dbLexRoutines.c:1508`) looks
    /// the target up in `savedPdbbase` — the whole accumulated database
    /// — so only a caller that owns that database can resolve them, and
    /// the parser must not decide their fate on its own.
    pub unresolved_aliases: Vec<(String, String)>,
    /// Recoverable failures this text produced (C `yyerror(NULL)`). The
    /// records above are everything that parsed in spite of them; the
    /// caller turns a non-empty set into the load's non-zero status.
    pub faults: DbFaults,
}

/// C `dbAlias`'s diagnostic for a target no database knows
/// (`dbLexRoutines.c:1509-1513`). C follows it with `yyerror(NULL)`,
/// which sets `yyFailed` and returns 0: the load's status goes
/// non-zero, the parse continues, and everything already read stays.
pub fn unknown_alias_message(alias: &str, target: &str) -> String {
    format!("ERROR: Alias '{alias}' names an unknown record '{target}'")
}

/// Parse an EPICS .db file with macro substitution.
///
/// The records-only entry point: it has no database to resolve a
/// file-scope alias against, so an unmatched one is reported to stderr
/// and dropped. C keeps the records it already read too — callers that
/// own a database ([`crate::server::ioc_builder`], `dbLoadRecords`)
/// take [`ParsedDb::unresolved_aliases`] instead and resolve them.
pub fn parse_db(input: &str, macros: &HashMap<String, String>) -> CaResult<Vec<DbRecordDef>> {
    let parsed = parse_db_with_breaktables(input, macros)?;
    for (target, alias) in &parsed.unresolved_aliases {
        eprintln!("{}", unknown_alias_message(alias, target));
    }
    Ok(parsed.records)
}

/// Like [`parse_db`] but returns the whole [`ParsedDb`].
pub fn parse_db_with_breaktables(
    input: &str,
    macros: &HashMap<String, String>,
) -> CaResult<ParsedDb> {
    // Per LINE, not per file: C's loader expands each `fgets` line
    // separately (`dbLexRoutines.c:375-391`), so quote state cannot leak
    // across lines — see `substitute_macros_per_line`.
    let expanded = substitute_macros_per_line(input, macros);
    let mut records = Vec::new();
    let mut breaktables: Vec<crate::server::cvt_bpt::BrkTable> = Vec::new();
    // Standalone `alias("record","newname")` directives (dbYacc.y:275).
    // Resolved against the record list after the full file is parsed
    // so the alias target may appear before or after the directive.
    let mut global_aliases: Vec<(String, String)> = Vec::new();
    // `dbYacc.y` declares no `error` productions, so a genuine syntax
    // error ends `yyparse` and the whole text is lost in C too: every
    // `?` below is abort-class by C's own choice. Only the semantic
    // actions C guards with `yyerror(NULL)` are recoverable, and those
    // live in the layers that own a database — see [`DbFaults`].
    let faults = DbFaults::default();
    let chars: Vec<char> = expanded.chars().collect();
    let mut pos = 0;
    let mut line = 1;
    let mut col = 1;

    while pos < chars.len() {
        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        if pos >= chars.len() {
            break;
        }

        // Top-level keyword. C dbStatic (`dbYacc.y:48-62`) accepts
        // `record`/`grecord` plus the directives below at file scope.
        let word = read_word(&chars, &mut pos, &mut col);
        if word.is_empty() {
            pos += 1;
            col += 1;
            continue;
        }

        // `path "dir"` / `addpath "dir"` — search-path directives
        // (dbYacc.y:71-81). Include resolution is handled by the
        // file-expansion layer (`expand_includes`); by the time raw
        // text reaches `parse_db` the path is already fixed, so these
        // are accepted and skipped rather than erroring out.
        if word == "path" || word == "addpath" {
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            let _dir = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
            continue;
        }

        // `include "file"` (dbYacc.y:65-69). Includes are normally
        // inlined by `expand_includes` before `parse_db` runs; a bare
        // `include` reaching here means the caller parsed un-expanded
        // text. Accept the directive so the grammar matches C, but the
        // file is NOT loaded at this layer — that is the expansion
        // layer's job.
        if word == "include" {
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            let _file = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
            continue;
        }

        // Standalone 2-arg `alias("record","newname")` (dbYacc.y:275).
        // Distinct from the in-record-body `alias("name")` form. The
        // new name is attached to the named record's alias list once
        // all records are parsed (the target may appear later in the
        // file).
        if word == "alias" {
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            expect_char(&chars, &mut pos, &mut col, '(', line)?;
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            let target = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            expect_char(&chars, &mut pos, &mut col, ',', line)?;
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            let alias_name = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
            validate_record_name(&alias_name, line, col)?;
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            expect_char(&chars, &mut pos, &mut col, ')', line)?;
            global_aliases.push((target, alias_name));
            continue;
        }

        // `breaktable(name) { raw eng  raw eng  ... }` (dbYacc.y / C
        // `dbBreakBody`, dbLexRoutines.c:1003-1080). The body is a flat list
        // of numbers in `(raw, eng)` pairs, whitespace- or comma-separated;
        // `BrkTable::build` computes the interval slopes and validates the
        // table (>= 2 points, monotonic, no zero slope).
        if word == "breaktable" {
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            expect_char(&chars, &mut pos, &mut col, '(', line)?;
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            let bt_name = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            expect_char(&chars, &mut pos, &mut col, ')', line)?;
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            expect_char(&chars, &mut pos, &mut col, '{', line)?;

            // Read numeric tokens until the closing brace. Numbers are
            // separated by whitespace and/or commas (C `tokenVALUE` lexing).
            let is_number_char =
                |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | ':' | '.');
            let mut nums: Vec<f64> = Vec::new();
            loop {
                skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                if pos >= chars.len() {
                    return Err(CaError::DbParseError {
                        line,
                        column: col,
                        message: "unexpected end of file in breaktable body".into(),
                    });
                }
                if chars[pos] == '}' {
                    pos += 1;
                    col += 1;
                    break;
                }
                if chars[pos] == ',' {
                    pos += 1;
                    col += 1;
                    continue;
                }
                let start = col;
                let mut tok = String::new();
                while pos < chars.len() && is_number_char(chars[pos]) {
                    tok.push(chars[pos]);
                    pos += 1;
                    col += 1;
                }
                if tok.is_empty() {
                    return Err(CaError::DbParseError {
                        line,
                        column: col,
                        message: format!(
                            "breaktable {bt_name}: expected a number, got '{}'",
                            chars[pos]
                        ),
                    });
                }
                let num: f64 = tok.parse().map_err(|_| CaError::DbParseError {
                    line,
                    column: start,
                    message: format!("breaktable {bt_name}: non-numeric value '{tok}'"),
                })?;
                nums.push(num);
            }

            // C `dbBreakBody`: an odd count is a missing raw/eng partner.
            if nums.len() % 2 != 0 {
                return Err(CaError::DbParseError {
                    line,
                    column: col,
                    message: format!("breaktable {bt_name}: Raw value missing"),
                });
            }
            let pairs: Vec<(f64, f64)> = nums.chunks_exact(2).map(|c| (c[0], c[1])).collect();
            let table = crate::server::cvt_bpt::BrkTable::build(bt_name, &pairs).map_err(|e| {
                CaError::DbParseError {
                    line,
                    column: col,
                    message: e,
                }
            })?;
            breaktables.push(table);
            continue;
        }

        if word != "record" && word != "grecord" {
            return Err(CaError::DbParseError {
                line,
                column: col,
                message: format!("expected 'record', got '{word}'"),
            });
        }

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        expect_char(&chars, &mut pos, &mut col, '(', line)?;

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        let rec_type = read_token_string(&chars, &mut pos, &mut line, &mut col)?;

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        expect_char(&chars, &mut pos, &mut col, ',', line)?;

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        let name = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
        validate_record_name(&name, line, col)?;

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        expect_char(&chars, &mut pos, &mut col, ')', line)?;

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        let mut fields = Vec::new();
        let mut aliases: Vec<String> = Vec::new();
        let mut info_tags: Vec<(String, String)> = Vec::new();
        // C `dbYacc.y:237-241`: `record_body: /* empty */` is the FIRST
        // alternative, ahead of `'{' '}'` and `'{' record_field_list '}'`,
        // and `tokenRECORD` and `tokenGRECORD` share it — so
        // `record(ai,"TEMP")` with no braces is an all-defaults record,
        // not a syntax error. `record`/`grecord` reach this through the
        // same path above, so both forms are covered here.
        if pos < chars.len() && chars[pos] == '{' {
            pos += 1;
            col += 1;
            loop {
                skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                if pos >= chars.len() {
                    return Err(CaError::DbParseError {
                        line,
                        column: col,
                        message: "unexpected end of file in record body".into(),
                    });
                }
                if chars[pos] == '}' {
                    pos += 1;
                    col += 1;
                    break;
                }

                let kw = read_word(&chars, &mut pos, &mut col);
                if kw != "field" && kw != "info" && kw != "alias" {
                    return Err(CaError::DbParseError {
                        line,
                        column: col,
                        message: format!("expected 'field', got '{kw}'"),
                    });
                }

                if kw == "alias" {
                    // alias("name") — capture and validate per PR #336.
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    expect_char(&chars, &mut pos, &mut col, '(', line)?;
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    let alias_name = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
                    validate_record_name(&alias_name, line, col)?;
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    expect_char(&chars, &mut pos, &mut col, ')', line)?;
                    aliases.push(alias_name);
                    continue;
                }
                if kw == "info" {
                    // info(tag, value) — capture for downstream consumers
                    // (asyn:READBACK, Q:form, etc.). PR #60 / #208 needs
                    // the asyn:READBACK tag in particular.
                    //
                    // Both `tag` and `value` accept quoted *or* unquoted
                    // tokens. Base's dbStaticLib parser tolerates either
                    // form and ad-core templates rely on the unquoted
                    // shape (`info(asyn:READBACK, "1")`).
                    //
                    // C `info(tokenSTRING, json_value)` (dbYacc.y:262-267): the tag
                    // is a `tokenSTRING` — the same raw, untranslated token a record
                    // NAME is — and only the value is a `jsonSTRING` that
                    // `dbRecordInfo` runs `dbTranslateEscape` over
                    // (dbLexRoutines.c:1435-1440). Hence the two different readers.
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    expect_char(&chars, &mut pos, &mut col, '(', line)?;
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    let tag = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    expect_char(&chars, &mut pos, &mut col, ',', line)?;
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    let value = read_field_value(&chars, &mut pos, &mut line, &mut col)?;
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    expect_char(&chars, &mut pos, &mut col, ')', line)?;
                    info_tags.push((tag, value.as_str_lossy().into_owned()));
                    continue;
                }

                skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                expect_char(&chars, &mut pos, &mut col, '(', line)?;

                skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                let field_name = read_token_string(&chars, &mut pos, &mut line, &mut col)?;

                skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                expect_char(&chars, &mut pos, &mut col, ',', line)?;

                skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                let field_value = read_field_value(&chars, &mut pos, &mut line, &mut col)?;

                skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                expect_char(&chars, &mut pos, &mut col, ')', line)?;

                fields.push((field_name, field_value));
            }
        }

        records.push(DbRecordDef {
            record_type: rec_type,
            name,
            fields,
            aliases,
            info_tags,
        });
    }

    // Attach standalone `alias("record","newname")` directives to the
    // matching record (C `dbAlias`). A target this text does not
    // declare is NOT an error here: C resolves it against
    // `savedPdbbase`, so an earlier `dbLoadRecords` may own it. Hand it
    // to the caller, which is the only party that can see the database.
    let mut unresolved_aliases = Vec::new();
    for (target, alias_name) in global_aliases {
        match records.iter_mut().find(|r| r.name == target) {
            Some(rec) => rec.aliases.push(alias_name),
            None => unresolved_aliases.push((target, alias_name)),
        }
    }

    Ok(ParsedDb {
        records,
        breaktables,
        unresolved_aliases,
        faults,
    })
}

/// Resolution options for the macLib expansion engine ([`expand_macros`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct MacroExpandOptions {
    /// Fall back to the process environment when a name is unset,
    /// matching C `macCreateHandle(&h, environ)`. The `.db` parser
    /// leaves this off (its substitutions come only from the
    /// `dbLoadRecords` / `.substitutions` macro map); `dbLoadGroup`
    /// and autosave turn it on.
    pub env_fallback: bool,
    /// Treat `$$` as a literal `$`. An autosave `.req` convenience, NOT
    /// a macLib behavior — C macLib leaves `$$` verbatim (`$` is only
    /// special before `(`/`{`), so the `.db` parser leaves it off.
    pub dollar_escape: bool,
}

/// Outcome of [`expand_macros`]: the expanded text plus the names of
/// every macro referenced with neither a definition (nor an env value,
/// when `env_fallback`) nor a default. The text still carries the C
/// `$(name,undefined)` placeholder for each; callers that must hard-fail
/// on an undefined macro (autosave) inspect `undefined` instead.
#[derive(Clone, Debug, Default)]
pub struct MacroExpansion {
    pub text: String,
    pub undefined: Vec<String>,
}

/// Engine state threaded through [`trans`] / [`refer`] / [`parse_scoped`]:
/// the base macro map, the resolution options, and the running list of
/// undefined-macro names.
struct ExpandCtx<'a> {
    macros: &'a HashMap<String, String>,
    opts: MacroExpandOptions,
    undefined: Vec<String>,
}

/// Expand `$(...)` / `${...}` macro references, mirroring the C `macLib`
/// engine (`modules/libcom/src/macLib/macCore.c` `trans` / `refer`).
/// This is the single macLib implementation for the crate; the `.db`
/// parser, `dbLoadGroup`, and autosave all route through it (with
/// per-caller [`MacroExpandOptions`]) rather than re-implementing it.
/// Implemented behaviors:
///
///   - `\<char>` blocks macro detection; both bytes reach the output in
///     the caller's own string, and the backslash is dropped from
///     anything that arrived through a macro (`trans:700-703,738-745`;
///     `macLib.plt:52`).
///   - macros are NOT expanded inside single quotes (`trans:713-723`);
///     the quote characters themselves survive only at level 0.
///   - a reference name is itself macro-expanded before lookup
///     (`refer` runs `trans` on the name — `$($(WHICH))`).
///   - the name terminates at `=`, `,` or the closing bracket
///     (`macEnd = "=,)"`); `,name=val` introduces scoped macro
///     definitions visible only inside that reference's expansion.
///   - a resolved macro value is re-scanned for further `$(...)`
///     (chained expansion); a self-/mutually-referential macro stops
///     at the `visiting` guard (C per-entry `visited`).
///   - an undefined macro with no default emits the placeholder
///     `$(name,undefined)` (`refer:errval = ",undefined)"`) and is
///     recorded in [`MacroExpansion::undefined`].
///   - with [`MacroExpandOptions::env_fallback`], an otherwise-unset
///     name resolves from the process environment before the default
///     (C `macCreateHandle(&h, environ)`).
pub fn expand_macros(
    input: &str,
    macros: &HashMap<String, String>,
    opts: MacroExpandOptions,
) -> MacroExpansion {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut ctx = ExpandCtx {
        macros,
        opts,
        undefined: Vec::new(),
    };
    trans(
        &chars,
        0,
        &mut ctx,
        &mut Vec::new(),
        &mut Vec::new(),
        &mut out,
    );
    MacroExpansion {
        text: out,
        undefined: ctx.undefined,
    }
}

/// Expand `$(...)` / `${...}` macro references with the default
/// macLib options (no env fallback, no `$$` escape, undefined →
/// placeholder). Thin wrapper over [`expand_macros`]; `pub` so
/// `dbLoadGroup` and other consumers reuse the one engine instead of
/// re-implementing macLib.
pub fn substitute_macros(input: &str, macros: &HashMap<String, String>) -> String {
    expand_macros(input, macros, MacroExpandOptions::default()).text
}

/// [`substitute_macros`], one line at a time — how C's file readers feed
/// macLib.
///
/// `dbLoadRecords` hands `macExpandString` a single `fgets` line
/// (`dbLexRoutines.c:375-391`), and `asInitFile` does the same
/// (`asLibRoutines.c:202-219`), so the expander's quote tracking (`trans`'s
/// `quote`, C `macCore.c`) resets at every newline. Expanding a whole file
/// in one [`substitute_macros`] call let one line's quote state leak into
/// the next: an apostrophe in a `#` comment opened single-quote suppression
/// and silently disabled every `$(...)` on the lines after it, until the
/// parser failed on an unexpanded record name. Within a line the quote
/// rules are unchanged — `'$(X)'` still suppresses.
pub fn substitute_macros_per_line(input: &str, macros: &HashMap<String, String>) -> String {
    input
        .split_inclusive('\n')
        .map(|line| substitute_macros(line, macros))
        .collect()
}

/// Translate `chars` into `out`, expanding macro references.
///
/// `scopes` is the stack of scoped-macro frames pushed by enclosing
/// `$(name,key=val)` references; lookup walks it innermost-first then
/// falls back to `ctx.macros` (and, when enabled, the environment).
/// `visiting` is the stack of macro names currently being expanded — it
/// guards against a self-referential macro (`A=$(A)`) recursing forever,
/// mirroring C `macCore.c`'s per-entry `visited` flag.
fn trans(
    chars: &[char],
    level: usize,
    ctx: &mut ExpandCtx,
    scopes: &mut Vec<HashMap<String, String>>,
    visiting: &mut Vec<String>,
    out: &mut String,
) {
    // C `macCore.c:700-703`: "discard quotes and escapes if level is > 0
    // (i.e. if these aren't the user's quotes and escapes)". Level 0 is
    // the string the caller handed `macExpandString`; every recursion out
    // of `refer` — a macro's value, a reference name, a default, a scoped
    // definition — runs at `level + 1`, so a quote or a backslash that
    // arrived through a macro is syntax and never reaches the output.
    let discard = level > 0;
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];

        // Track single/double quote state (C `trans` `quote` var).
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

        // `$$` → literal `$` (opt-in; autosave `.req` convenience).
        if ctx.opts.dollar_escape && c == '$' && i + 1 < chars.len() && chars[i + 1] == '$' {
            out.push('$');
            i += 2;
            continue;
        }

        // `\<char>`: skip macro detection; the backslash itself is
        // emitted only at level 0 (C `if (v < valend && !discard)`).
        if c == '\\' && i + 1 < chars.len() {
            if !discard {
                out.push('\\');
            }
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }

        // Macro reference: `$` followed by `(` or `{`, and NOT inside
        // single quotes (C `macRef && quote != '\''`).
        let mac_ref =
            c == '$' && i + 1 < chars.len() && (chars[i + 1] == '(' || chars[i + 1] == '{');
        if mac_ref && quote != Some('\'') {
            if let Some(next) = refer(chars, i, level, ctx, scopes, visiting, out) {
                i = next;
                continue;
            }
        }

        out.push(c);
        i += 1;
    }
}

/// Expand one macro reference starting at `chars[start]` (`$`). On
/// success returns the index just past the closing bracket; returns
/// `None` if the reference is unterminated (caller copies `$` raw).
fn refer(
    chars: &[char],
    start: usize,
    level: usize,
    ctx: &mut ExpandCtx,
    scopes: &mut Vec<HashMap<String, String>>,
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

    // Split the body at the first top-level `=` or `,` (the C
    // `macEnd` terminator set). Nested `$(...)` brackets are skipped
    // so a `=`/`,` inside an inner reference does not terminate.
    let split = top_level_terminator(body);
    let (name_chars, rest) = match split {
        Some(k) => (&body[..k], &body[k..]),
        None => (body, &body[body.len()..]),
    };

    // the name itself may contain macro references — expand it.
    let mut name = String::new();
    trans(name_chars, level + 1, ctx, scopes, visiting, &mut name);

    // Default value (`=...`) and scoped definitions (`,k=v`).
    let mut default: Option<&[char]> = None;
    let mut scoped: Vec<(String, String)> = Vec::new();
    if let Some(first) = rest.first() {
        if *first == '=' {
            // Default runs until the first top-level `,` or end.
            let dflt = &rest[1..];
            let dsplit = top_level_comma(dflt);
            match dsplit {
                Some(k) => {
                    default = Some(&dflt[..k]);
                    parse_scoped(&dflt[k..], level, ctx, scopes, visiting, &mut scoped);
                }
                None => default = Some(dflt),
            }
        } else if *first == ',' {
            parse_scoped(rest, level, ctx, scopes, visiting, &mut scoped);
        }
    }

    // Push the scoped frame (visible only inside this expansion).
    let mut frame: HashMap<String, String> = HashMap::new();
    for (k, v) in scoped {
        frame.insert(k, v);
    }
    scopes.push(frame);

    // Look up: innermost scope first, then base macros, then (when
    // enabled) the process environment — C `macCreateHandle(&h, environ)`
    // installs env entries as macros, so env resolution sits at the same
    // level as a defined macro, before any default.
    let resolved = scopes
        .iter()
        .rev()
        .find_map(|s| s.get(&name).cloned())
        .or_else(|| ctx.macros.get(&name).cloned())
        .or_else(|| {
            if ctx.opts.env_fallback {
                crate::runtime::env::get(&name)
            } else {
                None
            }
        });

    match resolved {
        Some(val) => {
            if visiting.contains(&name) {
                // Recursive reference (C `refentry->visited`): emit
                // the resolved value verbatim WITHOUT re-expansion to
                // break the cycle, rather than recursing forever.
                out.push_str(&val);
            } else {
                // re-scan the resolved value for further refs.
                visiting.push(name.clone());
                let val_chars: Vec<char> = val.chars().collect();
                trans(&val_chars, level + 1, ctx, scopes, visiting, out);
                visiting.pop();
            }
        }
        None => match default {
            Some(def_chars) => {
                // C `refer` translates the default at `level + 1`
                // (`macCore.c:909`), so every quote in it is discarded —
                // not just a surrounding pair.
                trans(def_chars, level + 1, ctx, scopes, visiting, out);
            }
            None => {
                // L-4: undefined macro placeholder. C emits
                // `$(name,undefined)` when warnings are enabled. Record
                // the name so a hard-fail caller (autosave) can surface
                // it instead of accepting the placeholder.
                ctx.undefined.push(name.clone());
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
fn parse_scoped(
    rest: &[char],
    level: usize,
    ctx: &mut ExpandCtx,
    scopes: &mut Vec<HashMap<String, String>>,
    visiting: &mut Vec<String>,
    out: &mut Vec<(String, String)>,
) {
    let mut k = 0;
    while k < rest.len() {
        if rest[k] != ',' {
            break;
        }
        k += 1; // step over ','
        // Scoped name: up to next top-level `=` or `,`.
        let seg = &rest[k..];
        let term = top_level_terminator(seg);
        let (name_part, tail) = match term {
            Some(t) => (&seg[..t], &seg[t..]),
            None => (seg, &seg[seg.len()..]),
        };
        let mut sname = String::new();
        trans(name_part, level + 1, ctx, scopes, visiting, &mut sname);
        k += name_part.len();
        if let Some('=') = tail.first() {
            let valseg = &tail[1..];
            let vterm = top_level_comma(valseg);
            let (val_part, _) = match vterm {
                Some(t) => (&valseg[..t], &valseg[t..]),
                None => (valseg, &valseg[valseg.len()..]),
            };
            let mut sval = String::new();
            trans(val_part, level + 1, ctx, scopes, visiting, &mut sval);
            out.push((sname, sval));
            k += 1 + val_part.len();
        }
        // else: bare `,name` — no value, defines nothing.
    }
}

/// Index of the first top-level `=` or `,` in `body`, skipping any
/// `$(...)` / `${...}` nested reference.
fn top_level_terminator(body: &[char]) -> Option<usize> {
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

/// Index of the first top-level `,` in `body` (used to split a
/// default value from trailing scoped definitions).
fn top_level_comma(body: &[char]) -> Option<usize> {
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

fn skip_whitespace_and_comments(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
) {
    while *pos < chars.len() {
        match chars[*pos] {
            ' ' | '\t' | '\r' => {
                *pos += 1;
                *col += 1;
            }
            '\n' => {
                *pos += 1;
                *line += 1;
                *col = 1;
            }
            '#' => {
                // Line comment
                while *pos < chars.len() && chars[*pos] != '\n' {
                    *pos += 1;
                }
            }
            _ => break,
        }
    }
}

fn read_word(chars: &[char], pos: &mut usize, col: &mut usize) -> String {
    let mut word = String::new();
    while *pos < chars.len() && (chars[*pos].is_ascii_alphanumeric() || chars[*pos] == '_') {
        word.push(chars[*pos]);
        *pos += 1;
        *col += 1;
    }
    word
}

fn read_quoted_string(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
) -> CaResult<String> {
    if *pos >= chars.len() || chars[*pos] != '"' {
        return Err(CaError::DbParseError {
            line: *line,
            column: *col,
            message: "expected '\"'".into(),
        });
    }
    *pos += 1;
    *col += 1;

    // C dbStatic lexer parity (dbLex.l:88-92): a quoted `tokenSTRING` matches
    // `{doublequote}({dqschar}|{escape})*{doublequote}` where `escape =
    // {backslash}.`. The lexer keeps the **raw bytes** of the string body —
    // `dbmfStrdup(yytext+1)` then NUL-terminates one byte early, so only the
    // surrounding quotes are stripped; escape sequences are NOT translated.
    //
    // This rule reads NAMES ONLY (record/alias/breaktable names, `path` and
    // `include` arguments, the `info` tag) — every `tokenSTRING` in the C
    // grammar. Field and info VALUES are a different token from a different
    // start condition (`jsonSTRING`, dbLex.l:111-114) and DO get translated;
    // they go through [`read_json_string`]. Do not merge the two: unescaping
    // here would corrupt every record name (R18-91).
    //
    // The escape sequence still consumes its following char for delimiter
    // purposes — `\"` does not terminate the string — but both bytes are
    // emitted verbatim.
    let mut s = String::new();
    while *pos < chars.len() && chars[*pos] != '"' {
        if chars[*pos] == '\\' && *pos + 1 < chars.len() && chars[*pos + 1] != '\n' {
            // Emit both the backslash and the escaped char raw.
            //
            // The `!= '\n'` guard is required: C `{escape}` is
            // `{backslash}.` and flex `.` never matches a newline
            // (`{dqschar}` is `[^"\n\\]`), so a backslash immediately
            // before a newline is NOT an escape. Skipping this branch
            // lets the newline fall through to the `Newline in string`
            // error below (dbLex.l:131-133).
            s.push('\\');
            s.push(chars[*pos + 1]);
            *pos += 2;
            *col += 2;
        } else if chars[*pos] == '\n' {
            // dbLex.l:131-133: a newline inside an unterminated quoted
            // string is a hard parse error (`yyerrorAbort("Newline in
            // string, closing quote missing")`), not a literal char.
            return Err(CaError::DbParseError {
                line: *line,
                column: *col,
                message: "Newline in string, closing quote missing".into(),
            });
        } else {
            s.push(chars[*pos]);
            *pos += 1;
            *col += 1;
        }
    }

    if *pos >= chars.len() {
        return Err(CaError::DbParseError {
            line: *line,
            column: *col,
            message: "unterminated string".into(),
        });
    }
    *pos += 1; // skip closing "
    *col += 1;
    Ok(s)
}

/// A quoted field/info VALUE — C's `jsonSTRING` (dbLex.l:111-114, matched in
/// the `JSON` start condition the `field(`/`info(` rules switch into,
/// dbYacc.y:256-267), followed by the translation `dbLexRoutines.c:1398-1403`
/// and `:1435-1440` run on it.
///
/// C keeps the quotes on a `jsonSTRING` — they are precisely the marker that
/// the translation is still owed — and `dbGetFieldValue`/`dbRecordInfo` then
/// strip them and call `dbTranslateEscape(value, value)` before `dbPutString`.
/// So `field(DESC,"hex\x41end")` stores `hexAend`, and a link field's
/// `"@drvUser(\"chan1\")"` reaches device support with real quotes in it, not
/// backslashes. This is the half [`read_quoted_string`] (names, raw) must NOT
/// do, and the whole of R18-91.
///
/// The accepted grammar is C's (dbLex.l:23-33), which is STRICTER than a raw
/// read: a literal control character and the escapes `\1`..`\9` are syntax
/// errors, `\x` demands exactly two hex digits and `\u` exactly four. Single
/// quotes delimit a value string as well as double (`jsonsqstr`), and the other
/// quote character is then an ordinary character. C accepts `\uHHHH` in the
/// lexer but implements no `\u` in the translation, so it comes out as the
/// literal text `uHHHH` — reproduced, because a `.db` in the field may rely on
/// what C actually stores.
fn read_json_string(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
) -> CaResult<PvString> {
    let quote = match chars.get(*pos) {
        Some(&c @ ('"' | '\'')) => c,
        _ => {
            return Err(CaError::DbParseError {
                line: *line,
                column: *col,
                message: "expected a quoted value".into(),
            });
        }
    };
    *pos += 1;
    *col += 1;

    let err = |line: usize, col: usize, message: &str| CaError::DbParseError {
        line,
        column: col,
        message: message.into(),
    };

    // The escaped source text, quotes stripped — C's `yytext`, minus the two
    // quote characters `dbGetFieldValue` steps over.
    let mut escaped = String::new();
    loop {
        let Some(&c) = chars.get(*pos) else {
            return Err(err(*line, *col, "unterminated string"));
        };
        if c == quote {
            *pos += 1;
            *col += 1;
            break;
        }
        if c == '\n' {
            // dbLex.l:131-133 — the JSON string rule cannot match a newline
            // (`normalchar` excludes every control character), and the
            // catch-all reports the missing quote.
            return Err(err(*line, *col, "Newline in string, closing quote missing"));
        }
        if c == '\\' {
            let Some(&esc) = chars.get(*pos + 1) else {
                return Err(err(*line, *col, "unterminated string"));
            };
            // `escapedchar` is `{backslash}[^ux1-9]`; `x` and `u` introduce the
            // fixed-width `latinchar`/`unicodechar` forms, and `\1`..`\9` are
            // not escapes at all (C has no octal here).
            let hex = |n: usize| {
                chars
                    .get(*pos + 2..*pos + 2 + n)
                    .is_some_and(|ds| ds.iter().all(|d| d.is_ascii_hexdigit()))
            };
            let width = match esc {
                'x' if hex(2) => 4,
                'u' if hex(4) => 6,
                'x' | 'u' | '1'..='9' => {
                    return Err(err(
                        *line,
                        *col,
                        "invalid escape sequence (\\x needs 2 hex digits, \
                         \\u needs 4, and \\1..\\9 are not escapes)",
                    ));
                }
                _ => 2,
            };
            // `\<newline>` is a legal escape here (the class `[^ux1-9]` matches
            // a newline where the name rule's `.` does not), so the line
            // counter has to follow it.
            let consumed = &chars[*pos..*pos + width];
            escaped.extend(consumed);
            *pos += width;
            if consumed.contains(&'\n') {
                *line += 1;
                *col = 0;
            } else {
                *col += width;
            }
            continue;
        }
        if (c as u32) < 0x20 {
            return Err(err(
                *line,
                *col,
                "a control character must be escaped inside a quoted value",
            ));
        }
        escaped.push(c);
        *pos += 1;
        *col += 1;
    }

    Ok(PvString::from_bytes(
        crate::runtime::epics_string::raw_from_escaped(&escaped),
    ))
}

/// A brace-delimited JSON field value, returned verbatim (braces included).
/// Braces inside a JSON string do not nest the value, and a `\` escapes the
/// next character inside a string.
fn read_json_value(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
) -> CaResult<String> {
    let start_line = *line;
    let close = if chars.get(*pos) == Some(&'[') {
        ']'
    } else {
        '}'
    };
    let mut s = String::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    while *pos < chars.len() {
        let c = chars[*pos];
        s.push(c);
        *pos += 1;
        if c == '\n' {
            *line += 1;
            *col = 0;
        } else {
            *col += 1;
        }

        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(s);
                }
            }
            _ => {}
        }
    }

    Err(CaError::DbParseError {
        line: start_line,
        column: *col,
        message: format!("unterminated JSON value (missing '{close}')"),
    })
}

/// A C `tokenSTRING` — the ONE reader for every grammar position `dbYacc.y`
/// spells that way (record type, record name, field name, info tag, alias
/// names, breaktable name, path/include).
///
/// `dbLex.l:88-97` gives `tokenSTRING` two lexical forms and the grammar never
/// distinguishes them:
///
/// * a bareword — `[a-zA-Z0-9_\-+:.\[\]<>;]+` (`dbLex.l:21`);
/// * a double-quoted string — taken RAW, the lexer only strips the quotes; no
///   escape translation happens in the INITIAL start condition (that is the
///   `JSON` condition's job, and it applies to VALUES only — see
///   [`read_field_value`]).
///
/// So `record("ai", "QT1") { field("VAL", "5") }` and `record(ai, QT2)` are the
/// same grammar to C, and softIoc loads both. The port used to read the type
/// and the field name with [`read_word`] (bareword only, and a narrower charset
/// at that) and the record/alias names with [`read_quoted_string`] (quoted
/// only) — so each position accepted one of C's two forms and refused the
/// other, turning a legal `.db` into a hard startup failure. Reading them all
/// through here is what makes the two forms indistinguishable to the caller,
/// as they are to C.
fn read_token_string(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
) -> CaResult<String> {
    if matches!(chars.get(*pos), Some('"')) {
        return read_quoted_string(chars, pos, line, col);
    }

    let mut s = String::new();
    while let Some(&c) = chars.get(*pos) {
        if c.is_ascii_alphanumeric()
            || matches!(c, '_' | '-' | '+' | ':' | '.' | '[' | ']' | '<' | '>' | ';')
        {
            s.push(c);
            *pos += 1;
            *col += 1;
        } else {
            break;
        }
    }
    if s.is_empty() {
        let got = chars.get(*pos).map_or("EOF".to_string(), |c| c.to_string());
        return Err(CaError::DbParseError {
            line: *line,
            column: *col,
            message: format!("expected a name (bareword or quoted string), got '{got}'"),
        });
    }
    Ok(s)
}

/// A field or info VALUE — C's `json_value` (dbYacc.y:256-267): the parser
/// switches into the `JSON` start condition for it, so a quoted value is a
/// `jsonSTRING` and its escapes are translated ([`read_json_string`]). The
/// unquoted forms (`jsonBARE`, `jsonNUMBER`) carry no escapes and are taken
/// verbatim, as C does.
fn read_field_value(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
) -> CaResult<PvString> {
    if matches!(chars.get(*pos), Some('"' | '\'')) {
        return read_json_string(chars, pos, line, col);
    }

    // JSON value: C's dbLex tokenizes a brace- or bracket-delimited field
    // value as JSON (`field(DOL, {const:"hello"})`, `field(INP, {pva:"PV"})`,
    // `field(INP, [1, 2, 3])`) and hands the raw text to `dbParseLink`, which
    // calls `dbJLinkParse` for an object (dbStaticLib.c:2280-2285) and takes
    // a bracketed value as an array CONSTANT (`:2352-2356`). `json_array` is
    // a `json_value` alternative just like `json_object` (dbYacc.y:328-337,
    // `:356-363`), so both open the same way; the port had no `[` branch, so
    // `[1, 2, 3]` fell through to the bareword reader, stopped at the first
    // comma and failed the whole file. Keep the text verbatim — the link
    // parser is the one that interprets it.
    if matches!(chars.get(*pos), Some('{' | '[')) {
        // The brace text is JSON source — ASCII/UTF-8 by construction, and the
        // escapes inside it belong to yajl, not to the `.db` lexer (R19-67).
        return read_json_value(chars, pos, line, col).map(PvString::from);
    }

    // Unquoted value: a C `bareword` (dbLex.l:21) —
    // `[a-zA-Z0-9_\-+:.\[\]<>;]+`. Leading/trailing whitespace is
    // skipped; an embedded space or any character outside the
    // bareword set is a lexer error in C (the text would tokenize as
    // two tokens). L-5: the Rust parser previously accepted arbitrary
    // bytes up to the next `,`/`)`, which is strictly more permissive
    // than C.
    let is_bareword = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '-' | '+' | ':' | '.' | '[' | ']' | '<' | '>' | ';')
    };

    let mut s = String::new();
    while *pos < chars.len() && is_bareword(chars[*pos]) {
        s.push(chars[*pos]);
        *pos += 1;
        *col += 1;
    }
    // Skip trailing whitespace before the delimiter.
    while *pos < chars.len() && matches!(chars[*pos], ' ' | '\t' | '\r' | '\n') {
        if chars[*pos] == '\n' {
            *line += 1;
            *col = 0;
        }
        *pos += 1;
        *col += 1;
    }
    // The only thing allowed after an unquoted value is the field
    // delimiter (`,` or `)`); anything else means the value contained
    // an illegal bareword character.
    if *pos < chars.len() && chars[*pos] != ')' && chars[*pos] != ',' {
        return Err(CaError::DbParseError {
            line: *line,
            column: *col,
            message: format!(
                "illegal character '{}' in unquoted value (expected a quoted string or bareword)",
                chars[*pos]
            ),
        });
    }
    Ok(PvString::from(s))
}

fn expect_char(
    chars: &[char],
    pos: &mut usize,
    col: &mut usize,
    expected: char,
    line: usize,
) -> CaResult<()> {
    if *pos >= chars.len() || chars[*pos] != expected {
        let got = if *pos < chars.len() {
            chars[*pos].to_string()
        } else {
            "EOF".to_string()
        };
        return Err(CaError::DbParseError {
            line,
            column: *col,
            message: format!("expected '{expected}', got '{got}'"),
        });
    }
    *pos += 1;
    *col += 1;
    Ok(())
}

/// C `dbStaticLib`'s record PROTOTYPE — the `initial("…")` directive.
///
/// `dbReadDatabase` builds one zeroed prototype per record type and writes each
/// field's `.dbd` `initial(...)` into it (`dbLexRoutines.c:1150-1163`,
/// `dbPutStringNum`); `dbCreateRecord` then copies that prototype, and the
/// `.db`'s own `field(...)` lines overwrite it. So a `.dbd` initial is the
/// record's default, and a `record(calc,"X") {}` with no CALC line still has
/// `CALC="0"` — not the empty string.
///
/// The port had no such step: every record hand-wrote a Rust `Default`, and
/// nothing tied it to the `.dbd`. calc, calcout and swait all forgot
/// `initial("0")` on their CALC/OCAL (`calcRecord.dbd:26`,
/// `calcoutRecord.dbd:62,382`, `swaitRecord.dbd:424`), so a default record
/// carried the EMPTY expression — which base's `postfix()` refuses
/// (`CALC_ERR_NULL_ARG`) and every evaluation then fails, putting a plain
/// `record(calc,"X") {}` into CALC/INVALID on its first process where C sits at
/// NO_ALARM.
///
/// This is the single owner of that step, so the `.dbd` is the only place a
/// default can be stated. A record's `Default` supplies the zeroed struct; the
/// `.dbd` supplies the initial value. The store is [`Record::put_field_internal`]
/// — dbStatic writes the prototype directly, with no `special()` and no
/// `SPC_NOMOD` gate, so a read-only field's initial lands too (its record's
/// `init_record` overwrites it afterwards, exactly as in C).
///
/// A field the record does not own carries its default in `CommonFields`
/// instead; there is no record storage here to write it into, and
/// `tests/dbd_initial_parity.rs` is what holds those to the `.dbd`.
fn apply_dbd_initials(record: &mut Box<dyn Record>) -> CaResult<()> {
    // The `.dbd` is the source, so the initials come from the GENERATED table —
    // `crate::server::record::dbd_generated`, which `tools/dbd-codegen` emits
    // from the vendored `.dbd` files. Asking the `.dbd` directly is what makes
    // the default impossible to get wrong by hand.
    //
    // This is BASE's generated table, so it seeds base's record types only. The
    // downstream record types (`motor`, `table`, `scaler`, `epid`, `throttle`,
    // `timestamp`) now carry a generated table too — in their own crate, which
    // base cannot name — and their `.dbd` `initial()` values are therefore
    // parsed but never applied: those records keep the defaults their `Default`
    // impl sets. Seeding them is not a table lookup but a semantic change (C
    // applies the `.dbd` initial BEFORE `init_record`, and each record's
    // `init_record` then overwrites some of them — `motor`'s VERS is stamped
    // 7.4 over an `initial("1")`), so it is left to a change that can validate
    // each record against its C `init_record` rather than folded in here.
    let Some(fields) = crate::server::record::dbd_generated::record_fields(record.record_type())
    else {
        return Ok(()); // a record type base's vendored `.dbd` set does not declare
    };
    let initials: Vec<(&'static str, &'static str)> = fields
        .iter()
        .filter(|f| record.implements_field(f.name))
        .filter_map(|f| f.initial.map(|v| (f.name, v)))
        .collect();

    for (name, initial) in initials {
        let spec = fields.iter().find(|f| f.name == name);
        // The `.db` load path's coercion, reused verbatim: a `menu()` field's
        // initial is a LABEL (`initial("Passive")`), everything else parses to
        // the field's stored type — which is what the record already holds, the
        // `.dbd` type being the fallback for a field it cannot yet produce.
        let dbf_type = match record
            .get_field(name)
            .map(|v| v.db_field_type())
            .or_else(|| spec.map(|f| f.dbf_type))
        {
            Some(t) => t,
            None => continue,
        };
        let parsed = if let Some(choices) = spec
            .and_then(|f| f.menu)
            .or_else(|| record.menu_field_choices(name))
            .or_else(|| crate::server::record::shared_menu_choices(name))
        {
            crate::server::record::resolve_menu_field_string_db_load(
                name, choices, dbf_type, initial,
            )?
        } else {
            EpicsValue::parse_bytes(dbf_type, initial.as_bytes()).map_err(|e| {
                CaError::InvalidValue(format!(
                    "{}.{name}: cannot parse .dbd initial(\"{initial}\") as {dbf_type:?}: {e}",
                    record.record_type()
                ))
            })?
        };
        // A field whose zeroed Rust default already IS the `.dbd` initial needs
        // no store — and must not get one: a `SPC_NOMOD` field the record serves
        // but has no store arm for (`sseq.IX1`, C's read-only step index, whose
        // initial is the 0 it already holds) would fail the write. The write is
        // therefore driven by the DIFFERENCE, which is the thing this step exists
        // to remove.
        if record.get_field(name).as_ref() == Some(&parsed) {
            continue;
        }
        record.put_field_internal(name, parsed)?;
    }
    Ok(())
}

/// Create a record from a type name.
/// Checks the external factory registry first, then falls back to built-in types.
///
/// The record comes back holding its `.dbd` initial values — see
/// `apply_dbd_initials`. Every load path (`IocBuilder`, `dbLoadRecords`,
/// `dbCreateRecord`) goes through here, so no record can reach the database
/// disagreeing with its `.dbd` about a default.
pub fn create_record(record_type: &str) -> CaResult<Box<dyn Record>> {
    let mut record = create_record_raw(record_type)?;
    apply_dbd_initials(&mut record)?;
    Ok(record)
}

fn create_record_raw(record_type: &str) -> CaResult<Box<dyn Record>> {
    // Check external registry first (allows overrides from e.g. asyn-rs)
    if let Ok(reg) = get_registry().lock() {
        if let Some(factory) = reg.get(record_type) {
            return Ok(factory());
        }
    }

    use crate::server::records::*;

    match record_type {
        "ai" => Ok(Box::new(ai::AiRecord::default())),
        "ao" => Ok(Box::new(ao::AoRecord::default())),
        "bi" => Ok(Box::new(bi::BiRecord::default())),
        "bo" => Ok(Box::new(bo::BoRecord::default())),
        "busy" => Ok(Box::new(busy::BusyRecord::default())),
        "stringin" => Ok(Box::new(stringin::StringinRecord::default())),
        "asyn" => Ok(Box::new(asyn_record::AsynRecord::default())),
        "stringout" => Ok(Box::new(stringout::StringoutRecord::default())),
        "longin" => Ok(Box::new(longin::LonginRecord::default())),
        "longout" => Ok(Box::new(longout::LongoutRecord::default())),
        "int64in" => Ok(Box::new(int64in::Int64inRecord::default())),
        "int64out" => Ok(Box::new(int64out::Int64outRecord::default())),
        "lsi" => Ok(Box::new(lsi::LsiRecord::default())),
        "lso" => Ok(Box::new(lso::LsoRecord::default())),
        "mbbi" => Ok(Box::new(mbbi::MbbiRecord::default())),
        "mbbo" => Ok(Box::new(mbbo::MbboRecord::default())),
        "mbbiDirect" => Ok(Box::new(mbbi_direct::MbbiDirectRecord::default())),
        "mbboDirect" => Ok(Box::new(mbbo_direct::MbboDirectRecord::default())),
        "event" => Ok(Box::new(event::EventRecord::default())),
        "printf" => Ok(Box::new(printf::PrintfRecord::default())),
        "swait" => Ok(Box::new(swait::SwaitRecord::default())),
        "waveform" => Ok(Box::new(waveform::WaveformRecord::with_kind(
            waveform::ArrayKind::Waveform,
        ))),
        "aai" => Ok(Box::new(waveform::WaveformRecord::with_kind(
            waveform::ArrayKind::Aai,
        ))),
        "aao" => Ok(Box::new(waveform::WaveformRecord::with_kind(
            waveform::ArrayKind::Aao,
        ))),
        "subArray" => Ok(Box::new(waveform::WaveformRecord::with_kind(
            waveform::ArrayKind::SubArray,
        ))),
        "calc" => Ok(Box::new(calc::CalcRecord::default())),
        "fanout" => Ok(Box::new(fanout::FanoutRecord::default())),
        "seq" => Ok(Box::new(seq::SeqRecord::default())),
        "sseq" => Ok(Box::new(sseq::SseqRecord::default())),
        "scalcout" => Ok(Box::new(scalcout::ScalcoutRecord::default())),
        "acalcout" => Ok(Box::new(acalcout::AcalcoutRecord::default())),
        "transform" => Ok(Box::new(transform::TransformRecord::default())),
        "calcout" => Ok(Box::new(calcout::CalcoutRecord::default())),
        "dfanout" => Ok(Box::new(dfanout::DfanoutRecord::default())),
        "compress" => Ok(Box::new(compress::CompressRecord::default())),
        "histogram" => Ok(Box::new(histogram::HistogramRecord::default())),
        "sel" => Ok(Box::new(sel::SelRecord::default())),
        "sub" => Ok(Box::new(sub_record::SubRecord::default())),
        "aSub" => Ok(Box::new(asub_record::ASubRecord::default())),
        "permissive" => Ok(Box::new(permissive::PermissiveRecord::default())),
        "state" => Ok(Box::new(state::StateRecord::default())),
        _ => Err(CaError::DbParseError {
            line: 0,
            column: 0,
            message: format!("unknown record type: '{record_type}'"),
        }),
    }
}

/// Create a record, checking extra factories first, then built-in types.
/// Preferred over `create_record()` — avoids the global registry.
pub fn create_record_with_factories(
    record_type: &str,
    extra_factories: &std::collections::HashMap<String, super::RecordFactory>,
) -> CaResult<Box<dyn Record>> {
    if let Some(factory) = extra_factories.get(record_type) {
        let mut record = factory();
        apply_dbd_initials(&mut record)?;
        return Ok(record);
    }
    create_record(record_type)
}

/// Apply fields from a DbRecordDef to a record.
/// Returns the record along with any common field values.
/// Rewrite a `LINR` field that names a loaded breakpoint table to the numeric
/// `menuConvert` index that selects it. C extends the `menuConvert` menu with
/// one name-sorted choice per loaded table, so the first table is index 3
/// ([`crate::server::cvt_bpt::BreakTableRegistry`]). Only `ai`/`ao` carry a
/// `LINR` field; fixed labels (`NO_CONVERSION`/`SLOPE`/`LINEAR`) and numeric
/// values match no table name and are left for [`apply_fields`]'s menu
/// resolution. A no-op when the registry is empty. Shared by the IocBuilder
/// and `dbLoadRecords` load paths so both resolve table names identically.
pub fn resolve_linr_breaktable_names(
    record_type: &str,
    fields: &mut [(String, PvString)],
    registry: &crate::server::cvt_bpt::BreakTableRegistry,
) {
    if registry.is_empty() || !matches!(record_type, "ai" | "ao") {
        return;
    }
    for (fname, fvalue) in fields.iter_mut() {
        if fname.eq_ignore_ascii_case("LINR") {
            // A breakpoint-table NAME is an identifier — ASCII.
            if let Some(idx) = registry.linr_index_of(&fvalue.as_str_lossy()) {
                *fvalue = PvString::from(idx.to_string());
            }
        }
    }
}

/// The link fields whose value must reach [`RecordCommon`] — and therefore
/// device-support init via `DeviceSupportContext` — for every record type,
/// even one that stores the field itself.
///
/// [`RecordCommon`]: crate::server::record::CommonFields
fn is_common_link_field(upper_name: &str) -> bool {
    matches!(upper_name, "INP" | "OUT")
}

/// C's db-load link-type gate (`dbStaticLib.c:2222`): every link field the
/// `.db` explicitly assigns must carry a type the record's bound device support
/// takes. The rule itself lives in [`check_link_assignment`] — this is only the
/// half that says WHICH links a `.db` load offers to it.
///
/// A field the `.db` never mentions is NOT checked; C skips it too
/// (`if (!plink->text) continue;`, `dbStaticLib.c:2213`), which is why this
/// walks the `.db`'s own field list rather than the record's declaration.
///
/// DEVIATION from C (Tier 2): C prints the error, leaves the link uninitialized
/// and runs the IOC anyway, so the record sits there with a dead link that will
/// never read or write anything. The port refuses the load. The dead link is
/// precisely the illegal state, and a `.db` C has already declared invalid is
/// not worth constructing it for.
fn check_link_types(record_type: &str, fields: &[(String, PvString)]) -> CaResult<()> {
    let dtyp = fields
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("DTYP"))
        .map(|(_, v)| v.as_str_lossy().trim().to_string());

    for (name, value) in fields {
        check_link_assignment(
            record_type,
            dtyp.as_deref(),
            &name.to_uppercase(),
            &value.as_str_lossy(),
        )?;
    }
    Ok(())
}

pub fn apply_fields(
    record: &mut Box<dyn Record>,
    fields: &[(String, PvString)],
    common_fields: &mut Vec<(String, EpicsValue)>,
) -> CaResult<()> {
    check_link_types(record.record_type(), fields)?;

    for (name, value) in fields {
        let upper_name = name.to_uppercase();
        // Menu labels, numbers and link text are ASCII by construction; only a
        // DBF_STRING can carry a byte outside it, and that one goes to
        // `EpicsValue::parse_bytes` below, which keeps the bytes.
        let value_str = &value.as_str_lossy();

        // Two separate questions, and they must stay separate: *who owns* this
        // field (the record, or the framework's dbCommon fallback), and *what
        // type* the `.db` string parses as. `field_list()` is the spec and now
        // carries every field the `.dbd` declares — including INP/OUT, which
        // most record types leave to the framework — so ownership is asked of
        // `implements_field()`, not of table membership.
        let owned = record.implements_field(&upper_name);
        // The DECLARATION, from the same owner every other consumer asks: the
        // `.dbd`-generated table first, the record's hand table only where the
        // `.dbd` does not reach. Reading it off `field_list()` alone left an
        // `aSub`'s `field(FTA,"LONG")` with no `menu()` to resolve against.
        let spec = crate::server::record::record_instance::field_desc_of(
            record.as_ref(),
            upper_name.as_str(),
        );

        if owned {
            // C refuses `field(VAL, ...)` on a long-string field at load
            // time: lso/lsi/printf VAL (and lsi OVAL) is `DBF_NOACCESS` +
            // `SPC_DBADDR`, and `dbPutString` reports "Can't set array
            // field before iocInit()" (epics-base#548 keeps it that way).
            // The port refuses identically; asking `long_string_fields`
            // names that constraint instead of the accidental Char-parse
            // failure the value text would otherwise produce.
            if record
                .long_string_fields()
                .iter()
                .any(|f| f.eq_ignore_ascii_case(&upper_name))
            {
                return Err(CaError::InvalidValue(format!(
                    "field {upper_name}: can't set array field before iocInit() \
                     (a long-string field is not settable from a .db file, as in C)"
                )));
            }
            // Parse to the type the record STORES, exactly as
            // `put_field_internal_default` coerces to it: `put_field`'s arms
            // match on what is stored, and a `menu()` field is declared
            // `DBF_MENU` but stored as a `Short` choice index. The `.dbd` type
            // is the fallback for a field the record cannot yet produce a value
            // for.
            let dbf_type = record
                .get_field(&upper_name)
                .map(|v| v.db_field_type())
                .or_else(|| spec.map(|f| f.dbf_type))
                .ok_or_else(|| {
                    CaError::InvalidValue(format!("field {upper_name}: no type to parse it as"))
                })?;
            // A `DBF_MENU` field's `.db` value resolves against THAT field's
            // own menu (label-first, then a numeric index) — C dbStaticLib
            // `dbPutStringNum`. Using the field's choices, not a cross-menu
            // global table, is what keeps a menu-specific label from being
            // dropped or mis-mapped (e.g. `field(SELM,"Specified")`).
            let parsed = if let Some(choices) = spec
                .and_then(|f| f.menu)
                .or_else(|| record.menu_field_choices(&upper_name))
                .or_else(|| crate::server::record::shared_menu_choices(&upper_name))
            {
                crate::server::record::resolve_menu_field_string_db_load(
                    &upper_name,
                    choices,
                    dbf_type,
                    value_str,
                )?
            } else {
                // BYTES, not text: a DBF_STRING is a byte string with a 40-byte
                // budget, and `field(VAL,"h\xffz")` is 3 bytes in C (R19-68).
                EpicsValue::parse_bytes(dbf_type, value.as_bytes()).map_err(|e| {
                    CaError::InvalidValue(format!(
                        "field {upper_name} (type {dbf_type:?}): cannot parse '{value_str}': {e}"
                    ))
                })?
            };
            record.put_field(&upper_name, parsed)?;
            // A record type may declare INP/OUT in its own `field_list`,
            // mirroring its C `.dbd` (`scalerRecord`, `motorRecord`,
            // `acalcout`, …). The value is then stored by the record — but in
            // C the link is *also* what device support reads at init:
            // `devXxx.c::init_record` dereferences `prec->out` / `prec->inp`
            // directly (e.g. `devScalerAsyn.c::scaler_init_record` parses OUT
            // to pick its board), and a record owning the field changes
            // nothing about that. Routing the value to the record ALONE left
            // `RecordCommon.inp`/`.out` — the single source of truth
            // `DeviceSupportContext` is built from — empty, so a dynamic
            // device-support factory saw `ctx.out == ""` and could not
            // disambiguate. Mirror link fields into the common fields as well
            // so the link text reaches device-support init for every record
            // type. This is text only: `RecordInstance::put_common_field`
            // arms the framework's own link dispatch (`parsed_inp` /
            // `parsed_out`) only for a record that does NOT declare the field,
            // so a record driving its own link is not driven twice.
            if is_common_link_field(&upper_name) {
                common_fields.push((upper_name.clone(), EpicsValue::String(value.clone())));
            }
        } else {
            // Store as common field for RecordInstance to handle. The BYTES: a
            // common DBF_STRING (DESC, EGU, …) has the same 40-byte budget.
            common_fields.push((upper_name.clone(), EpicsValue::String(value.clone())));
        }

        // C `dbPutString` (`dbStaticLib.c:2653-2660`): writing VAL through the
        // STATIC library also writes UDF = 0 —
        //
        // ```c
        // if (!status && strcmp(pflddes->name, "VAL") == 0) {
        //     ...
        //     if (!dbFindField(&dbentry, "UDF")) dbPutString(&dbentry, "0");
        // }
        // ```
        //
        // — so `field(VAL,"1")` in a `.db` DEFINES the record at load, on every
        // record type, whatever its `process()` later does with UDF. The load
        // owner is the only place this can live: it is the one gate both
        // `dbLoadRecords` paths (IocBuilder and iocsh) cross. Pushed in file
        // order, so an explicit `field(UDF,...)` after `field(VAL,...)` still
        // wins, exactly as it does in C.
        if upper_name == "VAL" {
            common_fields.push(("UDF".to_string(), EpicsValue::Char(0)));
        }
    }
    Ok(())
}

/// Render a fixture path so it survives being used as a MACRO or
/// ENVIRONMENT value.
///
/// `trans` discards escapes in a substituted value (C
/// `macCore.c:700-703`), so every `\` in a Windows path is eaten the
/// moment the value is re-scanned and `$(TOP)/x.db` resolves against a
/// separator-less string. Win32 accepts `/` as a separator, which is
/// why EPICS build systems put `TOP` in forward-slash form there; a
/// test that feeds `tempdir()` into a macro has to do the same.
#[cfg(test)]
pub(crate) fn macro_safe_path(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_db() {
        let input = r#"
    record(ai, "TEMP") {
    field(DESC, "Temperature")
    field(SCAN, "1 second")
    field(HOPR, "100")
    field(LOPR, "0")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record_type, "ai");
        assert_eq!(records[0].name, "TEMP");
        assert_eq!(records[0].fields.len(), 4);
        assert_eq!(records[0].fields[0], ("DESC".into(), "Temperature".into()));
    }

    #[test]
    fn test_macro_substitution() {
        let input = r#"
    record(ai, "$(P)TEMP") {
    field(DESC, "$(D=Default Desc)")
    }
    "#;
        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "IOC:".to_string());

        let records = parse_db(input, &macros).unwrap();
        assert_eq!(records[0].name, "IOC:TEMP");
        assert_eq!(records[0].fields[0].1, "Default Desc");
    }

    #[test]
    fn test_multiple_records() {
        let input = r#"
    record(ai, "TEMP1") {
    field(VAL, "25.0")
    }
    record(bo, "SWITCH") {
    field(VAL, "1")
    field(ZNAM, "Off")
    field(ONAM, "On")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].record_type, "ai");
        assert_eq!(records[1].record_type, "bo");
    }

    #[test]
    fn test_comments() {
        let input = r#"
    # This is a comment
    record(ai, "TEMP") {
    # Another comment
    field(VAL, "25.0")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn test_unknown_record_type() {
        let result = create_record("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_quoted_string_escape() {
        // R18-91: a quoted field VALUE is a `jsonSTRING`, and
        // `dbGetFieldValue` runs `dbTranslateEscape` over it — so
        // `"hello \"world\""` stores the 13 chars `hello "world"`, with real
        // quotes. (This test previously pinned the defect, asserting the raw
        // 15-char form.) The `\"` still does not terminate the string.
        //
        // softIoc: field(VAL,"a \"b\" c") -> dbgf prints "a \"b\" c", i.e. it
        // re-escapes on print (dbTest.c:1007), so the stored bytes are 9.
        let input = r#"
    record(stringin, "TEST") {
    field(VAL, "hello \"world\"")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records[0].fields[0].1, r#"hello "world""#);
    }

    #[test]
    fn test_quoted_value_translates_escapes() {
        // R18-91: `\n`, `\t`, `\\` and `\xHH` are all TRANSLATED in a field
        // value — softIoc `field(DESC,"hex\x41end")` -> `dbgf E2.DESC` =
        // "hexAend". (This test previously pinned the defect under the name
        // `test_quoted_string_keeps_escapes_raw`.)
        let input = r#"
    record(stringin, "TEST") {
    field(DESC, "line1\nline2")
    field(OUT, "a\\b\tc")
    field(SIOL, "hex\x41end")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records[0].fields[0].1, "line1\nline2");
        assert_eq!(records[0].fields[1].1, "a\\b\tc");
        assert_eq!(records[0].fields[2].1, "hexAend");
    }

    /// The other half of the split: a record/alias NAME is a `tokenSTRING`
    /// (dbLex.l:88-92) and keeps its escape bytes RAW. Unescaping the shared
    /// helper would have broken every name.
    #[test]
    fn test_record_name_keeps_escapes_raw() {
        let input = r#"
    record(stringin, "T\tEST") {
    field(VAL, "x")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records[0].name, r"T\tEST");
    }

    /// An `info` VALUE is translated (`dbRecordInfo`, dbLexRoutines.c:1435-1440);
    /// the info TAG is a `tokenSTRING` and is not.
    #[test]
    fn test_info_value_translates_escapes() {
        let input = r#"
    record(stringin, "TEST") {
    info("Q:group", "tab\there")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records[0].info_tags[0].0, "Q:group");
        assert_eq!(records[0].info_tags[0].1, "tab\there");
    }

    /// A single-quoted value is a `jsonsqstr` (dbLex.l:31-33) and is translated
    /// the same way — softIoc: `field(VAL,'sq:\tx')` stores `sq:<TAB>x`.
    #[test]
    fn test_single_quoted_value_translates_escapes() {
        let input = "record(stringin, \"TEST\") {\n field(VAL, 'sq:\\tx')\n}\n";
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records[0].fields[0].1, "sq:\tx");
    }

    /// C's value grammar is STRICTER than a raw read: `\1`..`\9` is not an
    /// escape (`escapedchar` = `{backslash}[^ux1-9]`, dbLex.l:25), so a `.db`
    /// carrying an octal escape is a syntax error — softIoc rejects
    /// `field(VAL,"octal\101x")` with `ERROR: syntax error`, and the port must
    /// not silently start up with different data.
    #[test]
    fn test_octal_escape_in_value_is_rejected() {
        let input = r#"
    record(stringin, "TEST") {
    field(VAL, "octal\101x")
    }
    "#;
        assert!(matches!(
            parse_db(input, &HashMap::new()),
            Err(CaError::DbParseError { .. })
        ));
    }

    /// A link field rides the same path: the escaped quotes reach device
    /// support as real quotes, not backslashes (R18-91's headline impact).
    #[test]
    fn test_link_field_value_is_translated() {
        let input = r#"
    record(stringin, "TEST") {
    field(INP, "@drvUser(\"chan1\")")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records[0].fields[0].1, r#"@drvUser("chan1")"#);
    }

    #[test]
    fn test_quoted_string_newline_aborts() {
        // a literal newline inside a quoted string (missing
        // closing quote) is a hard parse error in C (dbLex.l:131-133).
        let input = "record(stringin, \"TEST\") {\n    field(DESC, \"line1\nline2\")\n}\n";
        let res = parse_db(input, &HashMap::new());
        assert!(
            matches!(res, Err(CaError::DbParseError { ref message, .. })
                if message.contains("Newline in string")),
            "expected newline-in-string abort, got {res:?}"
        );
    }

    #[test]
    fn test_quoted_string_backslash_before_newline_aborts() {
        // The NAME rule (dbLex.l:88-92): `{escape}` is `{backslash}.` and flex
        // `.` never matches a newline (`{dqschar}` is `[^"\n\\]`), so a
        // backslash immediately before a newline is NOT an escape — the string
        // stays unterminated and aborts (dbLex.l:131-133).
        //
        // softIoc, `record(stringin,"BN2\<newline>X")`:
        //   ERROR: Invalid character '"' / Invalid character '\' / syntax error
        let input = "record(stringin, \"TE\\\nST\") {\n    field(DESC, \"x\")\n}\n";
        let res = parse_db(input, &HashMap::new());
        assert!(
            matches!(res, Err(CaError::DbParseError { ref message, .. })
                if message.contains("Newline in string")),
            "expected newline-in-string abort, got {res:?}"
        );
    }

    /// The VALUE rule is the other start condition and disagrees on exactly
    /// this character: `escapedchar` is `{backslash}[^ux1-9]` (dbLex.l:25), a
    /// character CLASS, which does match a newline. So a backslash before a
    /// newline is a valid escape in a value, and the translation's default arm
    /// yields the newline itself.
    ///
    /// softIoc, `field(DESC, "line1\<newline>line2")` → `dbgf BN.DESC` prints
    /// `"line1\nline2"` — i.e. a real newline, re-escaped for display.
    #[test]
    fn test_value_backslash_before_newline_is_a_newline() {
        let input = "record(stringin, \"TEST\") {\n    field(DESC, \"line1\\\nline2\")\n}\n";
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records[0].fields[0].1, "line1\nline2");
    }

    #[test]
    fn test_macro_with_quoted_default_in_string() {
        // C EPICS macLib treats quotes inside $(...) as literal characters.
        // e.g. $(XPOS="") means "default to empty-string pair".
        let input = r#"
    record(longout, "$(P)$(R)PositionXLink") {
    field(DOL, "$(XPOS="") CP MS")
    }
    "#;
        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "SIM1:".to_string());
        macros.insert("R".to_string(), "Over1:1:".to_string());
        macros.insert("XPOS".to_string(), "SIM1:ROI1:MinX_RBV".to_string());
        let records = parse_db(input, &macros).unwrap();
        assert_eq!(records[0].fields[0].1, "SIM1:ROI1:MinX_RBV CP MS");
    }

    #[test]
    fn test_macro_with_quoted_default_unset() {
        // When XPOS is not set, $(XPOS="") should expand to "" (literal quotes)
        let input = r#"
    record(longout, "TEST:Link") {
    field(DOL, "$(XPOS="") CP MS")
    }
    "#;
        let macros = HashMap::new();
        let records = parse_db(input, &macros).unwrap();
        // With undefined macro and default="", the field gets the raw default
        assert!(records[0].fields[0].1.as_str_lossy().contains("CP MS"));
    }

    #[test]
    fn test_recursive_macro_default() {
        // $(TS_PORT=$(PORT)_TS) with PORT=ATTR1 → ATTR1_TS
        let input = r#"
    record(stringin, "TEST") {
    field(VAL, "$(TS_PORT=$(PORT)_TS)")
    }
    "#;
        let mut macros = HashMap::new();
        macros.insert("PORT".to_string(), "ATTR1".to_string());
        let records = parse_db(input, &macros).unwrap();
        assert_eq!(records[0].fields[0].1, "ATTR1_TS");
    }

    #[test]
    fn test_substitute_directive_in_expand() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        // Create a simple child template
        let child = dir.path().join("child.db");
        let mut f = std::fs::File::create(&child).unwrap();
        writeln!(f, r#"record(ai, "$(P)$(R)Val") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "$(ADDR)")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        // Create parent with substitute + include
        let parent = dir.path().join("parent.db");
        let mut f = std::fs::File::create(&parent).unwrap();
        writeln!(f, r#"substitute "R=A:,ADDR=0""#).unwrap();
        writeln!(f, r#"include "child.db""#).unwrap();
        writeln!(f, r#"substitute "R=B:,ADDR=1""#).unwrap();
        writeln!(f, r#"include "child.db""#).unwrap();

        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "IOC:".to_string());
        // C `dbOpenFile` searches the path list, never the including
        // file's directory, so the tempdir has to be ON the list.
        let config = DbLoadConfig {
            include_paths: vec![dir.path().to_path_buf()],
            max_include_depth: 10,
        };
        let records = parse_db_file(&parent, &macros, &config).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "IOC:A:Val");
        assert_eq!(records[0].fields[0].1, "0");
        assert_eq!(records[1].name, "IOC:B:Val");
        assert_eq!(records[1].fields[0].1, "1");
    }

    #[test]
    fn test_empty_string_numeric_parse() {
        // C EPICS treats empty VAL as 0 for numeric record types
        let input = r#"
    record(longin, "TEST:Int") {
    field(VAL, "")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        // Should parse without error — empty string → 0
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn test_calcout_process() {
        use crate::server::record::Record;
        use crate::server::records::calcout::CalcoutRecord;

        let mut rec = CalcoutRecord::default();
        rec.put_field("CALC", EpicsValue::String("A+B".into()))
            .unwrap();
        // C compiles CALC in special(SPC_CALC), never in the field write —
        // dbPut always runs both, so drive the same pair here.
        rec.special("CALC", true).unwrap();
        rec.put_field("A", EpicsValue::Double(3.0)).unwrap();
        rec.put_field("B", EpicsValue::Double(4.0)).unwrap();
        rec.process().unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 7.0).abs() < 1e-10),
            other => panic!("expected Double(7.0), got {:?}", other),
        }
    }

    #[test]
    fn test_calcout_oopt() {
        use crate::server::record::Record;
        use crate::server::records::calcout::CalcoutRecord;

        let mut rec = CalcoutRecord::default();
        rec.put_field("CALC", EpicsValue::String("A".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("OOPT", EpicsValue::Short(1)).unwrap(); // On Change
        rec.put_field("A", EpicsValue::Double(5.0)).unwrap();

        // First process — value changes from 0 to 5
        rec.process().unwrap();
        assert!((rec.oval - 5.0).abs() < 1e-10);

        // Second process — same value, OVAL should not update (but val still computes)
        rec.process().unwrap();
        // OVAL is still 5.0 since val didn't change
    }

    #[test]
    fn test_calcout_dopt() {
        use crate::server::record::Record;
        use crate::server::records::calcout::CalcoutRecord;

        let mut rec = CalcoutRecord::default();
        rec.put_field("CALC", EpicsValue::String("A+B".into()))
            .unwrap();
        // C compiles CALC in special(SPC_CALC), never in the field write —
        // dbPut always runs both, so drive the same pair here.
        rec.special("CALC", true).unwrap();
        rec.put_field("OCAL", EpicsValue::String("A*B".into()))
            .unwrap();
        rec.special("OCAL", true).unwrap();
        rec.put_field("DOPT", EpicsValue::Short(1)).unwrap(); // Use OCAL
        rec.put_field("A", EpicsValue::Double(3.0)).unwrap();
        rec.put_field("B", EpicsValue::Double(4.0)).unwrap();
        rec.process().unwrap();

        // VAL = A+B = 7, OVAL = A*B = 12
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 7.0).abs() < 1e-10),
            other => panic!("expected Double(7.0), got {:?}", other),
        }
        match rec.get_field("OVAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 12.0).abs() < 1e-10),
            other => panic!("expected Double(12.0), got {:?}", other),
        }
    }

    #[test]
    fn test_dfanout_basic() {
        use crate::server::record::Record;
        use crate::server::records::dfanout::DfanoutRecord;

        let mut rec = DfanoutRecord::default();
        rec.put_field("VAL", EpicsValue::Double(42.0)).unwrap();
        assert_eq!(rec.record_type(), "dfanout");
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 42.0).abs() < 1e-10),
            other => panic!("expected Double(42.0), got {:?}", other),
        }
    }

    #[test]
    fn test_dfanout_output_links() {
        use crate::server::record::Record;
        use crate::server::records::dfanout::DfanoutRecord;

        let mut rec = DfanoutRecord::default();
        rec.put_field("OUTA", EpicsValue::String("REC_A".into()))
            .unwrap();
        rec.put_field("OUTB", EpicsValue::String("REC_B".into()))
            .unwrap();
        let links = rec.output_links();
        assert_eq!(links.len(), 2);
    }

    #[test]
    fn test_compress_circular_buffer() {
        use crate::server::record::Record;
        use crate::server::records::compress::CompressRecord;

        let mut rec = CompressRecord::new(5, 4); // nsam=5, alg=Circular Buffer
        for i in 0..7 {
            rec.push_value(i as f64);
        }
        // C `get_array_info` linearises FIFO oldest→newest. After
        // 7 pushes to nsam=5: nuse saturates at 5, the last 5 values
        // are [2, 3, 4, 5, 6].
        match rec.get_field("VAL") {
            Some(EpicsValue::DoubleArray(arr)) => {
                assert_eq!(arr, vec![2.0, 3.0, 4.0, 5.0, 6.0]);
            }
            other => panic!("expected DoubleArray, got {:?}", other),
        }
    }

    #[test]
    fn test_compress_n_to_1_mean() {
        use crate::server::record::Record;
        use crate::server::records::compress::CompressRecord;

        let mut rec = CompressRecord::new(10, 2); // alg=Mean
        rec.put_field("N", EpicsValue::Long(3)).unwrap();
        rec.push_value(3.0);
        rec.push_value(6.0);
        rec.push_value(9.0); // mean = 6.0
        match rec.get_field("VAL") {
            Some(EpicsValue::DoubleArray(arr)) => {
                assert!((arr[0] - 6.0).abs() < 1e-10);
            }
            other => panic!("expected DoubleArray, got {:?}", other),
        }
    }

    #[test]
    fn test_histogram_bucket_count() {
        use crate::server::records::histogram::HistogramRecord;

        let mut rec = HistogramRecord::new(10, 0.0, 10.0);
        rec.add_sample(2.5); // bucket 2
        rec.add_sample(2.7); // bucket 2
        // C `histogramRecord.c:340-345` selects the bucket with a
        // closed upper edge (`temp <= i*wdth`): a value exactly on a
        // boundary lands in the LOWER bucket. sgnl=7.0, wdth=1.0 ->
        // i=7 is the first `7.0 <= i*1.0`, dest = i-1 = bucket 6.
        rec.add_sample(7.0); // boundary value -> bucket 6 (C parity)
        assert_eq!(rec.val[2], 2);
        assert_eq!(rec.val[6], 1);
    }

    #[test]
    fn test_histogram_out_of_range() {
        use crate::server::records::histogram::HistogramRecord;

        let mut rec = HistogramRecord::new(10, 0.0, 10.0);
        rec.add_sample(-1.0); // below range
        rec.add_sample(10.0); // at upper limit (excluded)
        rec.add_sample(15.0); // above range
        let total: u32 = rec.val.iter().sum();
        assert_eq!(total, 0);
    }

    #[test]
    fn test_sel_specified() {
        use crate::server::record::Record;
        use crate::server::records::sel::SelRecord;

        let mut rec = SelRecord::default();
        rec.put_field("SELM", EpicsValue::Short(0)).unwrap(); // Specified
        rec.put_field("SELN", EpicsValue::Short(2)).unwrap(); // Select C
        rec.put_field("C", EpicsValue::Double(99.0)).unwrap();
        rec.process().unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 99.0).abs() < 1e-10),
            other => panic!("expected Double(99.0), got {:?}", other),
        }
    }

    #[test]
    fn test_sel_high_low_median() {
        use crate::server::record::Record;
        use crate::server::records::sel::SelRecord;

        let mut rec = SelRecord::default();
        rec.put_field("A", EpicsValue::Double(10.0)).unwrap();
        rec.put_field("B", EpicsValue::Double(30.0)).unwrap();
        rec.put_field("C", EpicsValue::Double(20.0)).unwrap();

        // High
        rec.put_field("SELM", EpicsValue::Short(1)).unwrap();
        rec.process().unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 30.0).abs() < 1e-10),
            other => panic!("expected Double(30.0), got {:?}", other),
        }

        // Low
        rec.put_field("SELM", EpicsValue::Short(2)).unwrap();
        rec.process().unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 10.0).abs() < 1e-10), // min of finite values (A=10,B=30,C=20)
            other => panic!("expected near 0.0, got {:?}", other),
        }
    }

    #[test]
    fn db_load_menu_labels_resolve_against_field_menu() {
        // A DBF_MENU field's `.db` value resolves against THAT field's own
        // menu (C dbStaticLib dbPutStringNum: label-first, then a numeric
        // index), not a cross-menu global table. Before the fix, the loader
        // routed every menu label through one global table that mis-mapped
        // menu-specific labels and dropped labels it did not carry.
        use crate::server::record::Record;

        let apply = |rt: &str, field: &str, value: &str| -> CaResult<Box<dyn Record>> {
            let mut rec = create_record(rt).unwrap();
            let mut common = Vec::new();
            apply_fields(&mut rec, &[(field.to_string(), value.into())], &mut common)?;
            Ok(rec)
        };

        // sel.SELM (record-specific menu selSELM): "Specified" is index 0.
        // The old global table returned 1 (from menuFanout's "Specified").
        let rec = apply("sel", "SELM", "Specified").unwrap();
        assert_eq!(rec.get_field("SELM"), Some(EpicsValue::Enum(0)));

        // A later choice proves the whole menu, not just index 0.
        let rec = apply("sel", "SELM", "High Signal").unwrap();
        assert_eq!(rec.get_field("SELM"), Some(EpicsValue::Enum(1)));

        // A bare numeric index still resolves (C epicsParseUInt16 fallback).
        let rec = apply("sel", "SELM", "2").unwrap();
        assert_eq!(rec.get_field("SELM"), Some(EpicsValue::Enum(2)));

        // ai.LINR (shared menu menuConvert, SHORT-typed): "LINEAR" is index 2.
        // The old global table lacked menuConvert, so the load errored.
        let rec = apply("ai", "LINR", "LINEAR").unwrap();
        assert_eq!(rec.get_field("LINR"), Some(EpicsValue::Short(2)));

        // An unknown choice errors (C S_db_badChoice), never a silent mis-map.
        assert!(apply("sel", "SELM", "Bogus").is_err());
    }

    #[test]
    fn test_parse_breaktable_basic() {
        let input = r#"
breaktable(typeJdegC) {
    0    0
    365  67.0
    1000 178.0
}
record(ai, "T") { field(LINR, "typeJdegC") }
"#;
        let parsed = parse_db_with_breaktables(input, &HashMap::new()).unwrap();
        let breaktables = parsed.breaktables;
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(breaktables.len(), 1);
        let t = &breaktables[0];
        assert_eq!(t.name, "typeJdegC");
        assert_eq!(t.points.len(), 3);
        assert_eq!(t.points[0].raw, 0.0);
        assert_eq!(t.points[0].eng, 0.0);
        // slope[0] = (67-0)/(365-0).
        assert!((t.points[0].slope - (67.0 / 365.0)).abs() < 1e-12);
    }

    #[test]
    fn test_parse_breaktable_quoted_name_and_commas() {
        let input = r#"breaktable("tbl") { 0,0, 10,100 }"#;
        let breaktables = parse_db_with_breaktables(input, &HashMap::new())
            .unwrap()
            .breaktables;
        assert_eq!(breaktables.len(), 1);
        assert_eq!(breaktables[0].name, "tbl");
        assert_eq!(breaktables[0].points.len(), 2);
        assert!((breaktables[0].points[0].slope - 10.0).abs() < 1e-12);
    }

    #[test]
    fn test_parse_breaktable_odd_count_errors() {
        let input = r#"breaktable(bad) { 0 0 10 }"#;
        let Err(err) = parse_db_with_breaktables(input, &HashMap::new()) else {
            panic!("odd point count must be a parse error");
        };
        match err {
            CaError::DbParseError { message, .. } => {
                assert!(message.contains("Raw value missing"), "{message}")
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn test_resolve_linr_breaktable_names_rewrites_to_index() {
        use crate::server::cvt_bpt::{BreakTableRegistry, BrkTable};
        let mut reg = BreakTableRegistry::new();
        reg.insert(BrkTable::build("alpha", &[(0.0, 0.0), (1.0, 1.0)]).unwrap());

        // A registered non-standard table name on an ai/ao LINR is rewritten to
        // its index — the first user-table slot (15), since the standard
        // menuConvert names reserve 3..=14.
        let mut fields = vec![("LINR".to_string(), PvString::from("alpha"))];
        resolve_linr_breaktable_names("ai", &mut fields, &reg);
        assert_eq!(fields[0].1, "15");

        // A fixed menuConvert label matches no table name and is untouched.
        let mut fixed = vec![("LINR".to_string(), PvString::from("LINEAR"))];
        resolve_linr_breaktable_names("ai", &mut fixed, &reg);
        assert_eq!(fixed[0].1, "LINEAR");

        // A non-ai/ao record's field is never rewritten.
        let mut other = vec![("LINR".to_string(), PvString::from("alpha"))];
        resolve_linr_breaktable_names("bo", &mut other, &reg);
        assert_eq!(other[0].1, "alpha");
    }

    #[test]
    fn test_sub_record_register_and_call() {
        use crate::server::record::{Record, RecordInstance, SubroutineFn};
        use crate::server::records::sub_record::SubRecord;
        use std::sync::Arc;

        let mut rec = SubRecord::default();
        rec.put_field("SNAM", EpicsValue::String("double_val".into()))
            .unwrap();
        rec.put_field("VAL", EpicsValue::Double(5.0)).unwrap();

        let mut instance = RecordInstance::new("TEST_SUB".into(), rec);
        let sub_fn: SubroutineFn = Box::new(|record: &mut dyn Record| {
            if let Some(EpicsValue::Double(v)) = record.get_field("VAL") {
                record.put_field("VAL", EpicsValue::Double(v * 2.0))?;
            }
            Ok(0)
        });
        instance.subroutine = Some(Arc::new(sub_fn));

        instance.process_local().unwrap();

        match instance.record.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 10.0).abs() < 1e-10),
            other => panic!("expected Double(10.0), got {:?}", other),
        }
    }

    #[test]
    fn test_new_record_types_in_db() {
        let input = r#"
    record(calcout, "TEST_CO") {
    field(CALC, "A+1")
    }
    record(dfanout, "TEST_DF") {
    field(VAL, "5.0")
    }
    record(compress, "TEST_CMP") {
    field(DESC, "test compress")
    }
    record(histogram, "TEST_HIST") {
    field(DESC, "test hist")
    }
    record(sel, "TEST_SEL") {
    field(SELM, "0")
    }
    record(sub, "TEST_SUB") {
    field(SNAM, "my_sub")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records.len(), 6);
        // Verify they can all be created
        for def in &records {
            create_record(&def.record_type).unwrap();
        }
    }

    // ===== include / parse_db_file tests =====

    #[test]
    fn test_parse_include_directive() {
        // Normal include
        assert_eq!(
            parse_include_directive(r#"include "foo.template""#),
            Some("foo.template".to_string())
        );
        // With leading whitespace
        assert_eq!(
            parse_include_directive(r#"  include "bar.db""#),
            Some("bar.db".to_string())
        );
        // With trailing comment
        assert_eq!(
            parse_include_directive(r#"include "baz.template" # a comment"#),
            Some("baz.template".to_string())
        );
        // No quote — not an include
        assert_eq!(parse_include_directive("include something"), None);
        // Comment line
        assert_eq!(parse_include_directive(r#"# include "ignored.db""#), None);
        // Not an include keyword
        assert_eq!(parse_include_directive("record(ai, \"X\") {"), None);
        // "includes" is not "include"
        assert_eq!(parse_include_directive(r#"includes "nope.db""#), None);
    }

    #[test]
    fn test_commented_include_ignored() {
        assert_eq!(parse_include_directive(r#"# include "file.db""#), None);
        assert_eq!(parse_include_directive(r#"  # include "file.db""#), None);
    }

    #[test]
    fn test_expand_includes() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        // Create child.db
        let child_path = dir.path().join("child.db");
        let mut f = std::fs::File::create(&child_path).unwrap();
        writeln!(f, r#"record(ai, "CHILD") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "1.0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        // Create parent.db that includes child.db
        let parent_path = dir.path().join("parent.db");
        let mut f = std::fs::File::create(&parent_path).unwrap();
        writeln!(f, r#"record(ao, "PARENT") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "2.0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();
        writeln!(f, r#"include "child.db""#).unwrap();

        let config = DbLoadConfig {
            include_paths: vec![dir.path().to_path_buf()],
            max_include_depth: 32,
        };
        let result = expand_includes(
            &parent_path,
            &HashMap::new(),
            &config,
            &mut DbFaults::default(),
        )
        .unwrap();
        assert!(result.contains(r#"record(ao, "PARENT")"#));
        assert!(result.contains(r#"record(ai, "CHILD")"#));

        // Verify it parses correctly
        let records = parse_db(&result, &HashMap::new()).unwrap();
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn test_circular_include_error() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let a_path = dir.path().join("a.template");
        let b_path = dir.path().join("b.template");

        let mut fa = std::fs::File::create(&a_path).unwrap();
        writeln!(fa, r#"include "b.template""#).unwrap();

        let mut fb = std::fs::File::create(&b_path).unwrap();
        writeln!(fb, r#"include "a.template""#).unwrap();

        let config = DbLoadConfig {
            include_paths: vec![dir.path().to_path_buf()],
            max_include_depth: 32,
        };
        let result = expand_includes(&a_path, &HashMap::new(), &config, &mut DbFaults::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("circular include"), "error was: {err}");
    }

    #[test]
    fn test_duplicate_include_allowed() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let shared_path = dir.path().join("shared.db");
        let mut f = std::fs::File::create(&shared_path).unwrap();
        writeln!(f, r#"record(ai, "SHARED") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        // main.db includes shared.db twice (not circular, just duplicate)
        let main_path = dir.path().join("main.db");
        let mut f = std::fs::File::create(&main_path).unwrap();
        writeln!(f, r#"include "shared.db""#).unwrap();
        writeln!(f, r#"include "shared.db""#).unwrap();

        let config = DbLoadConfig {
            include_paths: vec![dir.path().to_path_buf()],
            max_include_depth: 32,
        };
        let result = expand_includes(
            &main_path,
            &HashMap::new(),
            &config,
            &mut DbFaults::default(),
        )
        .unwrap();
        // shared.db content appears twice
        assert_eq!(result.matches(r#"record(ai, "SHARED")"#).count(), 2);
    }

    #[test]
    fn test_include_depth_limit() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        // Create a chain: file0 -> file1 -> file2 -> ... -> file33
        for i in 0..34 {
            let path = dir.path().join(format!("file{i}.db"));
            let mut f = std::fs::File::create(&path).unwrap();
            if i < 33 {
                writeln!(f, r#"include "file{}.db""#, i + 1).unwrap();
            } else {
                writeln!(f, r#"record(ai, "DEEP") {{"#).unwrap();
                writeln!(f, r#"    field(VAL, "0")"#).unwrap();
                writeln!(f, r#"}}"#).unwrap();
            }
        }

        let config = DbLoadConfig {
            include_paths: vec![dir.path().to_path_buf()],
            max_include_depth: 32,
        };
        let result = expand_includes(
            &dir.path().join("file0.db"),
            &HashMap::new(),
            &config,
            &mut DbFaults::default(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("depth limit"), "error was: {err}");
    }

    /// R5-3: C `dbIncludeNew` (`dbLexRoutines.c:450-456`) reports an
    /// unopenable include with `yyerror(NULL)`, so the include is skipped
    /// and the rest of the file is still read. This test used to assert
    /// the opposite — that the whole expansion failed.
    #[test]
    fn test_include_not_found_is_recoverable() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let path = dir.path().join("main.db");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"record(ai, "SIM:A") {{ field(VAL, "1") }}"#).unwrap();
        writeln!(f, r#"include "nonexistent.db""#).unwrap();
        writeln!(f, r#"record(ai, "SIM:B") {{ field(VAL, "2") }}"#).unwrap();

        let config = DbLoadConfig::default();
        let mut faults = DbFaults::default();
        let text = expand_includes(&path, &HashMap::new(), &config, &mut faults)
            .expect("a missing include must not fail the expansion");
        assert!(text.contains("SIM:A") && text.contains("SIM:B"));
        let err = faults
            .status()
            .expect_err("the load's status must go non-zero");
        assert_eq!(err, "ERROR: Can't open include file 'nonexistent.db'");
    }

    #[test]
    fn test_include_with_macro_filename() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("sub");
        std::fs::create_dir(&subdir).unwrap();

        let child_path = subdir.join("child.db");
        let mut f = std::fs::File::create(&child_path).unwrap();
        writeln!(f, r#"record(ai, "CHILD") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        let main_path = dir.path().join("main.db");
        let mut f = std::fs::File::create(&main_path).unwrap();
        writeln!(f, r#"include "$(DIR)/child.db""#).unwrap();

        let mut macros = HashMap::new();
        macros.insert("DIR".to_string(), macro_safe_path(&subdir));

        let config = DbLoadConfig::default();
        let result =
            expand_includes(&main_path, &macros, &config, &mut DbFaults::default()).unwrap();
        assert!(result.contains(r#"record(ai, "CHILD")"#));
    }

    #[test]
    fn test_include_search_order() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let inc_dir = dir.path().join("inc");
        std::fs::create_dir(&inc_dir).unwrap();

        // Put file in include path only (not in current dir)
        let child_path = inc_dir.join("child.db");
        let mut f = std::fs::File::create(&child_path).unwrap();
        writeln!(f, r#"record(ai, "FROM_INC") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        let main_path = dir.path().join("main.db");
        let mut f = std::fs::File::create(&main_path).unwrap();
        writeln!(f, r#"include "child.db""#).unwrap();

        let config = DbLoadConfig {
            include_paths: vec![inc_dir.clone()],
            max_include_depth: 32,
        };
        let result = expand_includes(
            &main_path,
            &HashMap::new(),
            &config,
            &mut DbFaults::default(),
        )
        .unwrap();
        assert!(result.contains(r#"record(ai, "FROM_INC")"#));

        // I-R3-3: a same-named file next to the INCLUDING file must not
        // win. C `dbIncludeNew` (`dbLexRoutines.c:450`) goes through
        // `dbOpenFile`, which walks the path list and never looks at the
        // parent file's directory or the process CWD.
        let local_child = dir.path().join("child.db");
        let mut f = std::fs::File::create(&local_child).unwrap();
        writeln!(f, r#"record(ai, "FROM_LOCAL") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        let result = expand_includes(
            &main_path,
            &HashMap::new(),
            &config,
            &mut DbFaults::default(),
        )
        .unwrap();
        assert!(result.contains(r#"record(ai, "FROM_INC")"#));
        assert!(!result.contains(r#"record(ai, "FROM_LOCAL")"#));

        // …and a name that already carries a separator bypasses the list
        // entirely, exactly as C's `strchr(filename, '/')` gate does.
        let sep_main = dir.path().join("sep_main.db");
        let mut f = std::fs::File::create(&sep_main).unwrap();
        writeln!(f, r#"include "{}""#, local_child.display()).unwrap();
        let result = expand_includes(
            &sep_main,
            &HashMap::new(),
            &config,
            &mut DbFaults::default(),
        )
        .unwrap();
        assert!(result.contains(r#"record(ai, "FROM_LOCAL")"#));
    }

    #[test]
    fn test_addpath_directive_resolves_include() {
        // L-3: an `addpath` directive inside a .db file mutates the
        // include search path for subsequent `include` directives.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let inc_dir = dir.path().join("extra");
        std::fs::create_dir(&inc_dir).unwrap();

        // child.db lives ONLY in the extra/ dir.
        let child = inc_dir.join("child.db");
        let mut f = std::fs::File::create(&child).unwrap();
        writeln!(f, r#"record(ai, "FROM_ADDPATH") {{ field(VAL, "0") }}"#).unwrap();

        let main = dir.path().join("main.db");
        let mut f = std::fs::File::create(&main).unwrap();
        writeln!(f, r#"addpath "{}""#, inc_dir.display()).unwrap();
        writeln!(f, r#"include "child.db""#).unwrap();

        // No include path in config — only the addpath directive can
        // make this resolve.
        let config = DbLoadConfig::default();
        let result =
            expand_includes(&main, &HashMap::new(), &config, &mut DbFaults::default()).unwrap();
        assert!(result.contains(r#"record(ai, "FROM_ADDPATH")"#));
    }

    #[test]
    fn test_path_directive_replaces_search_path() {
        // L-3: `path` replaces the search path entirely.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let inc_dir = dir.path().join("p");
        std::fs::create_dir(&inc_dir).unwrap();

        let child = inc_dir.join("c.db");
        let mut f = std::fs::File::create(&child).unwrap();
        writeln!(f, r#"record(ai, "VIA_PATH") {{ field(VAL, "0") }}"#).unwrap();

        let main = dir.path().join("main.db");
        let mut f = std::fs::File::create(&main).unwrap();
        writeln!(f, r#"path "{}""#, inc_dir.display()).unwrap();
        writeln!(f, r#"include "c.db""#).unwrap();

        let config = DbLoadConfig::default();
        let result =
            expand_includes(&main, &HashMap::new(), &config, &mut DbFaults::default()).unwrap();
        assert!(result.contains(r#"record(ai, "VIA_PATH")"#));
    }

    /// A `DTYP=` macro is pure text substitution, exactly like every other
    /// macro: it reaches a record only where the `.db` wrote
    /// `field(DTYP,"$(DTYP)")`. It must NOT rewrite a record that spells its
    /// DTYP literally.
    ///
    /// This replaces `test_dtyp_override_existing_only`, which pinned the
    /// opposite (force-override) behaviour. C `dbLoadRecords` runs its macros
    /// through macLib during lexing (`dbLexRoutines.c`); there is no DTYP
    /// special case, and a force-override corrupts any multi-record file whose
    /// helper records carry a literal DTYP — e.g. the vendored
    /// `scaler-rs/db/scaler.db`, where two `bo` helpers are
    /// `field(DTYP,"Soft Channel")` and only the `scaler` record references
    /// `$(DTYP)`.
    #[test]
    fn dtyp_macro_substitutes_references_and_leaves_literals_alone() {
        let input = r#"
record(bo, "$(P)_calcEnable") {
    field(DTYP, "Soft Channel")
    field(ZNAM, "ENABLE")
}
record(scaler, "$(P)") {
    field(DTYP, "$(DTYP)")
    field(FREQ, "10000000")
}
record(ao, "$(P)_noDtyp") {
    field(VAL, "1")
}
"#;
        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "SCALER1".to_string());
        macros.insert("DTYP".to_string(), "Scaler-rs".to_string());

        let records = parse_db(input, &macros).unwrap();
        assert_eq!(records.len(), 3);

        let dtyp_of = |rec: &DbRecordDef| -> Option<String> {
            rec.fields
                .iter()
                .find(|(n, _)| n == "DTYP")
                .map(|(_, v)| v.as_str_lossy().into_owned())
        };

        // Literal DTYP: untouched by the macro. Force-override used to corrupt
        // this into "Scaler-rs", breaking the soft helper records.
        assert_eq!(dtyp_of(&records[0]).as_deref(), Some("Soft Channel"));
        // $(DTYP) reference: substituted.
        assert_eq!(dtyp_of(&records[1]).as_deref(), Some("Scaler-rs"));
        // No DTYP field: none added.
        assert_eq!(dtyp_of(&records[2]), None);
    }

    #[test]
    fn test_parse_db_file_no_includes() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let path = dir.path().join("simple.db");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"record(ai, "$(P)TEMP") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "25.0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "IOC:".to_string());

        let config = DbLoadConfig::default();
        let records = parse_db_file(&path, &macros, &config).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "IOC:TEMP");
    }

    // epics-base PR #78 — record-name validation regressions.

    #[test]
    fn name_validation_accepts_typical_names() {
        for n in [
            "IOC:TEMP",
            "MOTOR-1",
            "X[1]",
            "scan_5",
            "BL3:STD:01",
            "abc.xyz_record",
        ]
        .iter()
        .copied()
        // ".xyz" inside the bracket-allowed shape would actually fail
        // — the slash above is just to demonstrate the OK/FAIL split.
        {
            // Skip the deliberately-bad sample.
            if n.contains('.') {
                assert!(validate_record_name(n, 1, 1).is_err());
                continue;
            }
            validate_record_name(n, 1, 1).unwrap_or_else(|e| panic!("'{n}' should pass: {e:?}"));
        }
    }

    #[test]
    fn name_validation_rejects_empty() {
        assert!(validate_record_name("", 1, 1).is_err());
    }

    #[test]
    fn name_validation_rejects_bad_chars() {
        // Mirrors base: TAB (0x09) is non-printable so it falls into the
        // warn-and-continue branch, NOT the hard-error set. Only the
        // printable bad chars below produce a hard error.
        for bad in ["spa ce", "do.t", "qu\"ot", "ap'os", "do$llar"] {
            assert!(
                validate_record_name(bad, 1, 1).is_err(),
                "'{bad}' must be rejected"
            );
        }
    }

    #[test]
    fn name_validation_warns_but_passes_on_nonprintable() {
        // 0x09 (TAB) and 0x01 are < 0x20 so they only emit a warning.
        validate_record_name("ta\tb", 1, 1).expect("TAB is warn-only per base spec");
        validate_record_name("hello\x01world", 1, 1).expect("0x01 is warn-only");
    }

    #[test]
    fn name_validation_warns_on_leading_special_but_passes() {
        for warn in ["-x", "+y", "[arr", "{obj"] {
            validate_record_name(warn, 1, 1).expect("leading special is warn-only");
        }
    }

    /// R5-1: a bare `[...]` array constant is a `json_value`
    /// (`dbYacc.y:328-337`), so C loads this record. The port had no `[`
    /// branch in `read_field_value`, consumed `[1` as a bareword and
    /// stopped at the comma, so the whole file parsed to nothing. Both
    /// lines are transcribed from base's own test databases —
    /// `modules/database/test/std/rec/arrayOpTest.db:4` and
    /// `linkInitTest.db:15`.
    #[test]
    fn parse_db_accepts_a_bare_array_constant() {
        let src = r#"
record(waveform, "wfrec") {
    field(NELM, "10")
    field(FTVL, "LONG")
    field(INP, [1, 2, 3])
}
record(lsi, "longstr4") {
  field(SIZV, "100")
  field(INP, ["One","Two","Three","Four"])
}
"#;
        let recs = parse_db(src, &HashMap::new()).expect("base's own test database must load");
        assert_eq!(recs.len(), 2);
        let inp = |r: &DbRecordDef| {
            r.fields
                .iter()
                .find(|(n, _)| n == "INP")
                .map(|(_, v)| v.as_str_lossy().into_owned())
                .unwrap()
        };
        assert_eq!(inp(&recs[0]), "[1, 2, 3]");
        assert_eq!(inp(&recs[1]), r#"["One","Two","Three","Four"]"#);
    }

    #[test]
    fn parse_db_propagates_name_validation_error() {
        let bad = r#"record(ai, "BAD NAME") { }"#;
        let res = parse_db(bad, &HashMap::new());
        assert!(matches!(res, Err(CaError::DbParseError { .. })));
    }

    // epics-base PR #336 — alias parsing + name validation.

    #[test]
    fn parse_db_captures_aliases() {
        let src = r#"record(ai, "TARGET") {
            alias("ALIAS1")
            alias("ALIAS2")
            field(VAL, 42)
        }"#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "TARGET");
        assert_eq!(recs[0].aliases, vec!["ALIAS1", "ALIAS2"]);
        assert_eq!(recs[0].fields.len(), 1);
    }

    #[test]
    fn parse_db_rejects_alias_with_bad_name() {
        let src = r#"record(ai, "TARGET") {
            alias("BAD ALIAS")
        }"#;
        let res = parse_db(src, &HashMap::new());
        assert!(matches!(res, Err(CaError::DbParseError { .. })));
    }

    // L-2 — top-level directive grammar coverage.

    #[test]
    fn parse_db_accepts_path_and_addpath() {
        // `path`/`addpath` at file scope are accepted and skipped —
        // include resolution is the expansion layer's job.
        let src = r#"
            path "/opt/epics/db"
            addpath "/extra/db"
            record(ai, "REC") { field(VAL, "1") }
        "#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "REC");
    }

    #[test]
    fn parse_db_accepts_top_level_include() {
        // A bare `include` at file scope is accepted (grammar parity);
        // the file is not loaded at this layer.
        let src = r#"
            include "common.db"
            record(ai, "REC") { field(VAL, "1") }
        "#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        assert_eq!(recs.len(), 1);
    }

    #[test]
    fn parse_db_global_alias_two_arg() {
        // Standalone `alias("record","newname")` attaches the new name
        // to the target record's alias list.
        let src = r#"
            record(ai, "TARGET") { field(VAL, "1") }
            alias("TARGET", "TARGET_ALIAS")
        "#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].aliases, vec!["TARGET_ALIAS"]);
    }

    #[test]
    fn parse_db_global_alias_forward_reference() {
        // The alias directive may precede its target record.
        let src = r#"
            alias("TARGET", "EARLY_ALIAS")
            record(ai, "TARGET") { field(VAL, "1") }
        "#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        assert_eq!(recs[0].aliases, vec!["EARLY_ALIAS"]);
    }

    /// I-R3-5: C `dbYacc.y:237-241` puts `record_body: /* empty */`
    /// FIRST, ahead of `'{' '}'` and `'{' record_field_list '}'`, and
    /// `tokenRECORD` / `tokenGRECORD` (`:60-61`) share it — so a
    /// bodyless declaration is an all-defaults record. The port
    /// demanded `{` and the rejection discarded every record in the
    /// file.
    #[test]
    fn parse_db_record_without_a_body_is_valid() {
        let src = r#"
record(ai, "TEMP")
record(ai, "PRESS") { field(SCAN, "1 second") }
grecord(ai, "GTEMP")
record(ai, "EMPTY") { }
"#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        let names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["TEMP", "PRESS", "GTEMP", "EMPTY"]);
        assert!(recs[0].fields.is_empty(), "bodyless record takes defaults");
        assert_eq!(recs[1].fields.len(), 1);
        assert!(recs[2].fields.is_empty(), "grecord shares record_body");
        assert!(recs[3].fields.is_empty());

        // Boundary: the bodyless record is the last token in the file,
        // so there is nothing after the head at all.
        let recs = parse_db(r#"record(ai, "LAST")"#, &HashMap::new()).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "LAST");
    }

    /// I-R3-4: C `dbAlias` resolves the target against `savedPdbbase`
    /// (`dbLexRoutines.c:1508`), the accumulated database, so a target
    /// this text does not declare is not the parser's to reject — an
    /// earlier `dbLoadRecords` may own it. The parser hands it out
    /// unresolved and keeps every record it read; C's own failure path
    /// (`yyerror(NULL)`, `dbYacc.y:370-383`) also keeps them.
    #[test]
    fn parse_db_global_alias_unknown_record_is_deferred_not_rejected() {
        let src = r#"
            alias("NOSUCH", "X")
            record(ai, "KEPT") { field(VAL, "1") }
        "#;
        let parsed = parse_db_with_breaktables(src, &HashMap::new()).unwrap();
        assert_eq!(
            parsed.unresolved_aliases,
            vec![("NOSUCH".to_string(), "X".to_string())]
        );
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].name, "KEPT");
        // The records-only wrapper still yields what parsed.
        assert_eq!(parse_db(src, &HashMap::new()).unwrap().len(), 1);
    }

    // L-5 — unquoted field values are restricted to the C bareword set.

    #[test]
    fn parse_db_unquoted_bareword_value_ok() {
        let src = r#"record(ai, "REC") { field(VAL, 42) field(EGU, deg-C) }"#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        assert_eq!(recs[0].fields[0].1, "42");
        assert_eq!(recs[0].fields[1].1, "deg-C");
    }

    #[test]
    fn parse_db_unquoted_value_with_space_rejected() {
        // An unquoted multi-word value is two tokens in C — a parse
        // error. The author must quote it.
        let src = r#"record(ai, "REC") { field(DESC, hello world) }"#;
        let res = parse_db(src, &HashMap::new());
        assert!(
            matches!(res, Err(CaError::DbParseError { .. })),
            "unquoted value with space must be rejected, got {res:?}"
        );
    }

    #[test]
    fn parse_db_unquoted_value_with_illegal_char_rejected() {
        // `*` is outside the C bareword set.
        let src = r#"record(ai, "REC") { field(DESC, a*b) }"#;
        let res = parse_db(src, &HashMap::new());
        assert!(matches!(res, Err(CaError::DbParseError { .. })));
    }

    #[test]
    fn parse_db_record_without_alias_has_empty_aliases() {
        let src = r#"record(ai, "PLAIN") { field(VAL, 1) }"#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        assert!(recs[0].aliases.is_empty());
    }

    /// Regression: `info(asyn:READBACK, "1")` (unquoted tag,
    /// quoted value) is the form ad-core templates use. Base accepts
    /// it; an earlier parser fix tightened the grammar to require a
    /// quoted tag, which broke all NDOverlayN / NDFile / NDArrayBase
    /// templates and the mini-beamline/xrt-beamline IOCs that load
    /// commonPlugins.cmd.
    #[test]
    fn parse_db_info_accepts_unquoted_tag() {
        let src = r#"
record(ai, "REC") {
    field(VAL, "0")
    info(asyn:READBACK, "1")
    info("Q:group", "demo")
    info(autosaveFields, "VAL DESC")
}
"#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        assert_eq!(recs.len(), 1);
        let tags = &recs[0].info_tags;
        // Unquoted tag, quoted value.
        assert!(
            tags.iter().any(|(k, v)| k == "asyn:READBACK" && v == "1"),
            "unquoted tag must parse: {tags:?}"
        );
        // Quoted tag, quoted value (existing form).
        assert!(tags.iter().any(|(k, v)| k == "Q:group" && v == "demo"));
        // Unquoted tag, unquoted multi-word value.
        assert!(
            tags.iter()
                .any(|(k, v)| k == "autosaveFields" && v == "VAL DESC"),
            "unquoted multi-word value must parse: {tags:?}"
        );
    }

    /// C parity (modules/libcom/test/macLib.plt:52):
    ///   $(a)\$(b)  with a=foo  ->  foo\$(b)
    /// The `\` MUST block the macro-reference detection of the following
    /// `$` so `$(b)` survives as literal text. The `\` itself is
    /// preserved verbatim (macLib level 0 semantic — downstream
    /// caller-side escape passes may discard it).
    #[test]
    fn substitute_macros_backslash_escapes_dollar() {
        let mut macros = HashMap::new();
        macros.insert("a".to_string(), "foo".to_string());
        macros.insert("b".to_string(), "baz".to_string());

        // Anchor test: backslash before $ blocks expansion.
        assert_eq!(substitute_macros(r"$(a)\$(b)", &macros), r"foo\$(b)");

        // Backslash before brace form too.
        assert_eq!(substitute_macros(r"\${a}", &macros), r"\${a}");

        // Without backslash, both expand.
        assert_eq!(substitute_macros("$(a)$(b)", &macros), "foobaz");

        // Backslash escape consumes the IMMEDIATELY next char (one
        // step at a time, matching C macCore.c:741-744). So `\\$(a)`
        // emits the first `\` + second `\` literal (escape pair), then
        // resumes parsing at `$(a)` which expands. Result: `\\foo`.
        assert_eq!(substitute_macros(r"\\$(a)", &macros), r"\\foo");

        // Backslash NOT before $ / { passes through too (escape any next char).
        assert_eq!(
            substitute_macros(r"path\file $(a)", &macros),
            r"path\file foo"
        );
    }

    // Resolved macro values are re-expanded (chained).
    #[test]
    fn substitute_macros_chained_expansion() {
        // P=$(Q), Q=IOC:  →  $(P) expands to IOC:
        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "$(Q)".to_string());
        macros.insert("Q".to_string(), "IOC:".to_string());
        assert_eq!(substitute_macros("$(P)TEMP", &macros), "IOC:TEMP");
    }

    // `$(name,key=val,...)` scoped macro definitions.
    #[test]
    fn substitute_macros_scoped_definitions() {
        // INNER references A and B which are only defined for the
        // duration of this reference.
        let mut macros = HashMap::new();
        macros.insert("INNER".to_string(), "$(A)-$(B)".to_string());
        assert_eq!(substitute_macros("$(INNER,A=1,B=2)", &macros), "1-2");
    }

    // `expand_macros` reports every undefined macro so a hard-fail
    // caller (autosave) can surface it; the text still carries the
    // C `$(name,undefined)` placeholder for the no-fail callers.
    #[test]
    fn expand_macros_reports_undefined_names() {
        let macros = HashMap::new();
        let r = expand_macros("$(A)$(B=def)$(C)", &macros, MacroExpandOptions::default());
        assert_eq!(r.text, "$(A,undefined)def$(C,undefined)");
        // B had a default → not undefined; A and C are, in scan order.
        assert_eq!(r.undefined, vec!["A".to_string(), "C".to_string()]);
    }

    /// C `macCore.c:700-703` — "discard quotes and escapes if level
    /// is > 0 (i.e. if these aren't the user's quotes and escapes)".
    /// One case per boundary of that rule, both sides of level 0,
    /// measured against `macExpandString` from libCom 3.25.1.
    #[test]
    fn quotes_and_escapes_survive_only_at_level_zero() {
        let mut macros = HashMap::new();
        macros.insert("X".to_string(), "1".to_string());
        let l0 = |src: &str| expand_macros(src, &macros, MacroExpandOptions::default()).text;
        let l1 = |raw: &str| {
            let mut m = macros.clone();
            m.insert("M".to_string(), raw.to_string());
            expand_macros("$(M)", &m, MacroExpandOptions::default()).text
        };

        for (src, at_l0, at_l1) in [
            (r#""abc""#, r#""abc""#, "abc"),
            ("'abc'", "'abc'", "abc"),
            (r"a\b", r"a\b", "ab"),
            (r"a\\b", r"a\\b", r"a\b"),
            (r#""$(X)""#, r#""1""#, "1"),
            ("'$(X)'", "'$(X)'", "$(X)"),
            ("Beam's energy", "Beam's energy", "Beams energy"),
            (r#"a"b"#, r#"a"b"#, "ab"),
            ("$(X)'s", "1's", "1s"),
        ] {
            assert_eq!(l0(src), at_l0, "level 0 keeps the user's own: {src}");
            assert_eq!(l1(src), at_l1, "level 1 discards: {src}");
        }

        // A default value is translated at `level + 1` too
        // (`macCore.c:909`), so every quote in it goes — not just a
        // surrounding pair, which is what the port used to strip.
        assert_eq!(l0("$(NOSUCH=Beam's energy)"), "Beams energy");
        assert_eq!(l0(r"$(NOSUCH=a\b)"), "ab");
        assert_eq!(l0(r#"$(NOSUCH="q")"#), "q");
    }

    /// The `.db` shape the rule changes: a field default carrying an
    /// apostrophe. C loads `Beams energy` because the default is
    /// translated below level 0; the port used to load the apostrophe.
    #[test]
    fn a_db_field_default_loses_its_apostrophe_like_c() {
        let recs = parse_db(
            "record(ai,\"X:B\") { field(DESC, \"$(BEAM=Beam's energy)\") }",
            &HashMap::new(),
        )
        .expect("record must parse");
        assert_eq!(recs.len(), 1);
        let desc = recs[0]
            .fields
            .iter()
            .find(|(name, _)| name == "DESC")
            .map(|(_, value)| value.to_string());
        assert_eq!(desc.as_deref(), Some("Beams energy"));
    }

    // The default options match `.db` parse semantics: no env fallback,
    // and `$$` is NOT an escape (macLib leaves it verbatim).
    #[test]
    fn expand_macros_default_opts_leave_dollar_dollar_verbatim() {
        let macros = HashMap::new();
        assert_eq!(substitute_macros("$$100", &macros), "$$100");
        let r = expand_macros("$$100", &macros, MacroExpandOptions::default());
        assert_eq!(r.text, "$$100");
        assert!(r.undefined.is_empty());
    }

    // `env_fallback` resolves an otherwise-unset name from the process
    // environment (C `macCreateHandle(&h, environ)`), at the same level
    // as a defined macro — before any default.
    #[test]
    fn expand_macros_env_fallback_opt_in() {
        let var = "_EPICS_BASE_RS_MACRO_ENV_TEST";
        // Off by default: unset macro stays undefined even with the env set.
        unsafe { std::env::set_var(var, "FROM_ENV") };
        let macros = HashMap::new();
        let off = expand_macros(&format!("$({var})"), &macros, MacroExpandOptions::default());
        assert_eq!(off.text, format!("$({var},undefined)"));
        // On: resolves from the environment.
        let on = expand_macros(
            &format!("$({var})"),
            &macros,
            MacroExpandOptions {
                env_fallback: true,
                dollar_escape: false,
            },
        );
        assert_eq!(on.text, "FROM_ENV");
        assert!(on.undefined.is_empty());
        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn substitute_macros_scoped_not_leaking() {
        // A scoped macro must not leak past its reference.
        let mut macros = HashMap::new();
        macros.insert("INNER".to_string(), "$(A)".to_string());
        // After the scoped $(INNER,A=9), a bare $(A) is undefined.
        let out = substitute_macros("$(INNER,A=9)|$(A)", &macros);
        assert_eq!(out, "9|$(A,undefined)");
    }

    // Quote state resets at every newline (C expands one fgets line per
    // `macExpandString` call, dbLexRoutines.c:375-391): a quote character on
    // one line must not suppress expansion on the next, while suppression
    // WITHIN a line keeps working.
    #[test]
    fn substitute_macros_per_line_resets_quote_state_at_newline() {
        let mut macros = HashMap::new();
        macros.insert("X".to_string(), "VAL".to_string());
        // Whole-file expansion leaks the apostrophe's quote state forward…
        assert_eq!(
            substitute_macros("don't\n$(X)", &macros),
            "don't\n$(X)",
            "precondition: the whole-file engine suppresses across the newline"
        );
        // …the per-line form does not.
        assert_eq!(
            substitute_macros_per_line("don't\n$(X)", &macros),
            "don't\nVAL"
        );
        // Same-line suppression is unchanged.
        assert_eq!(
            substitute_macros_per_line("'$(X)' $(X)\n$(X)", &macros),
            "'$(X)' VAL\nVAL"
        );
    }

    // The formerly-bypassing path end to end: an apostrophe in a `#` comment
    // must not stop `$(P)` expanding on the record line below it.
    #[test]
    fn parse_db_expands_macros_after_a_comment_apostrophe() {
        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "T:".to_string());
        let db = "# this comment's apostrophe must stay harmless\n\
                  record(ai, \"$(P)X\") {\n}\n";
        let records = parse_db(db, &macros).expect("the comment must not break expansion");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "T:X");
    }

    // Macros are NOT expanded inside single quotes.
    #[test]
    fn substitute_macros_suppressed_in_single_quotes() {
        let mut macros = HashMap::new();
        macros.insert("X".to_string(), "VAL".to_string());
        // Single quotes suppress.
        assert_eq!(substitute_macros("'$(X)'", &macros), "'$(X)'");
        // Double quotes do NOT suppress.
        assert_eq!(substitute_macros("\"$(X)\"", &macros), "\"VAL\"");
    }

    // The reference name is macro-expanded before lookup.
    #[test]
    fn substitute_macros_indirect_name() {
        // $($(WHICH)) — WHICH selects which macro to read.
        let mut macros = HashMap::new();
        macros.insert("WHICH".to_string(), "SEL".to_string());
        macros.insert("SEL".to_string(), "chosen".to_string());
        assert_eq!(substitute_macros("$($(WHICH))", &macros), "chosen");
    }

    // L-4 — undefined macro with no default emits `$(name,undefined)`.
    #[test]
    fn substitute_macros_undefined_placeholder() {
        let macros = HashMap::new();
        assert_eq!(
            substitute_macros("$(MISSING)", &macros),
            "$(MISSING,undefined)"
        );
    }

    #[test]
    fn substitute_macros_default_with_comma_is_c_parity() {
        // C parity: `$(LIST=a,b,c)` — the name is LIST, the
        // default is `a` (terminates at the first top-level comma),
        // and `b`/`c` are bare scoped names that define nothing.
        let macros = HashMap::new();
        assert_eq!(substitute_macros("$(LIST=a,b,c)", &macros), "a");
    }

    #[test]
    fn substitute_macros_self_reference_terminates() {
        // Re-expansion must not recurse forever on `A=$(A)`.
        // The recursion guard emits the value once without re-scan.
        let mut macros = HashMap::new();
        macros.insert("A".to_string(), "$(A)".to_string());
        // Must terminate; the exact text is the cycle-broken value.
        let out = substitute_macros("$(A)", &macros);
        assert!(out.contains("A"), "self-ref expansion produced: {out}");
    }

    #[test]
    fn substitute_macros_mutual_reference_terminates() {
        // A=$(B), B=$(A) — mutual cycle must also terminate.
        let mut macros = HashMap::new();
        macros.insert("A".to_string(), "$(B)".to_string());
        macros.insert("B".to_string(), "$(A)".to_string());
        let _ = substitute_macros("$(A)", &macros); // must not hang
    }
}
