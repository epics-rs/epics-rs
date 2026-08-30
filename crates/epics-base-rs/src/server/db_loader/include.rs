use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{CaError, CaResult};

use crate::runtime::log::ERL_ERROR;

use super::{DbFaults, DbRecordDef, MacroDefs};

/// Configuration for file-based DB loading with include support.
pub struct DbLoadConfig {
    pub include_paths: Vec<PathBuf>,
    pub max_include_depth: usize,
}

impl Default for DbLoadConfig {
    fn default() -> Self {
        Self {
            include_paths: Vec::new(),
            max_include_depth: 32,
        }
    }
}

/// File-based entry point: expand includes, substitute macros, parse
/// records. Records-only, so an unresolved file-scope alias is reported
/// and dropped — see [`super::parse_db`].
pub fn parse_db_file(
    path: &Path,
    macros: impl Into<MacroDefs>,
    config: &DbLoadConfig,
) -> CaResult<Vec<DbRecordDef>> {
    let parsed = parse_db_file_with_breaktables(path, macros, config)?;
    for (target, alias) in &parsed.unresolved_aliases {
        eprintln!("{}", super::unknown_alias_message(alias, target));
    }
    Ok(parsed.records)
}

/// Like [`parse_db_file`] but returns the whole [`super::ParsedDb`], for
/// the `dbLoadRecords` runtime path that merges the breakpoint tables
/// into the database registry and resolves file-scope aliases against it.
pub fn parse_db_file_with_breaktables(
    path: &Path,
    macros: impl Into<MacroDefs>,
    config: &DbLoadConfig,
) -> CaResult<super::ParsedDb> {
    // The include layer and the record parser both report through the
    // same owner, so a recoverable failure in either reaches the caller
    // as one set — see [`DbFaults`].
    parse_db_opened_with_breaktables(&DbOpenedFile::taken_outright(path), macros, config)
}

/// [`parse_db_file_with_breaktables`] for a file that came through
/// [`db_open_file_located`], so its diagnostics name it the way the
/// operator wrote it rather than by the path this process resolved.
pub fn parse_db_opened_with_breaktables(
    opened: &DbOpenedFile,
    macros: impl Into<MacroDefs>,
    config: &DbLoadConfig,
) -> CaResult<super::ParsedDb> {
    // The include layer and the record parser both report through the
    // same owner, so a recoverable failure in either reaches the caller
    // as one set — see [`DbFaults`].
    let mut faults = DbFaults::default();
    // The expansion stage reports its OWN abort, because the parser's
    // funnel cannot: a text that never got built has no
    // [`super::DbSource`] to locate a diagnostic in, and these failures
    // carry `line: 0` for exactly that reason. Between this and
    // [`super::parse_db_expanded`] every rejection a `.db` read produces
    // is on the operator's stream before the error value leaves — which
    // is why no caller of this function prints one.
    let expanded = match expand_includes_mapped(opened, macros, config, &mut faults) {
        Ok(expanded) => expanded,
        Err(e) => {
            faults.abort(&e);
            return Err(e);
        }
    };
    // Already through `db_read_lines` once, inside the expansion — see
    // [`super::parse_db_expanded`].
    //
    // This is also why a load's read-time diagnostics (macLib's notices,
    // the per-line `WARNING: … has undefined macros`) all come out
    // before its first parse-time refusal, where C interleaves them per
    // line: C's lexer reads one line and the parser consumes it, while
    // the whole include tree is expanded here before the parser sees a
    // character. Same lines, same bytes, different grouping — an
    // intended consequence of flattening, not a defect to time away.
    let mut parsed = super::parse_db_expanded(&expanded.text, expanded.source)?;
    faults.absorb(parsed.faults);
    parsed.faults = faults;
    Ok(parsed)
}

/// The flattened include tree: the text the parser reads, and the map
/// that says which file and line each of its lines came from.
pub struct DbExpandedText {
    pub text: String,
    pub source: super::DbSource,
}

/// [`expand_includes`] keeping the per-line origin, which is what lets a
/// diagnostic name the file the operator actually wrote rather than an
/// offset into a string that exists only inside this process.
pub fn expand_includes_mapped(
    opened: &DbOpenedFile,
    macros: impl Into<MacroDefs>,
    config: &DbLoadConfig,
    faults: &mut DbFaults,
) -> CaResult<DbExpandedText> {
    // ONE table for the whole read, includes and all. C `dbReadCOM`
    // creates a single `MAC_HANDLE`, installs the caller's macros into it
    // and reads `dbQuietMacroWarnings` into it once at open
    // (`dbLexRoutines.c:256-300`), and `dbIncludeNew` pushes the included
    // file onto `inputFileList` without touching the handle
    // (`:443-461`) — so every line of every file in the tree is expanded
    // through the same table. msi does the same with one `macCreateHandle`
    // for the whole run (`msi.cpp:111`).
    let mut table = super::MacroTable::new(
        macros,
        super::MacroExpandOptions {
            suppress_warnings: super::db_quiet_macro_warnings(),
            ..super::MacroExpandOptions::default()
        },
    );
    let mut stack = Vec::new();
    let mut out = Lines::default();
    expand_includes_inner(opened, &mut table, config, &mut stack, faults, &mut out)?;
    Ok(DbExpandedText {
        text: out.text,
        source: super::DbSource::new(out.lines, out.frames),
    })
}

/// The accumulator `expand_includes_inner` writes into: one entry per
/// line of the flattened text, so the text and its map cannot drift.
#[derive(Default)]
struct Lines {
    text: String,
    lines: Vec<String>,
    frames: Vec<std::sync::Arc<[super::DbIncludeFrame]>>,
    /// C's `inputFileList` while the read is running: the files open at
    /// this point, outermost first, each parked at the line its reader
    /// has reached.
    open: Vec<super::DbIncludeFrame>,
}

impl Lines {
    /// Append one expanded line, tagged with the include stack in force
    /// at it. C prints that stack innermost-first (`dbIncludePrint`), so
    /// it is reversed here — once, where the frame is built, rather than
    /// at each of the places that read it.
    fn push(&mut self, text: String) {
        self.text.push_str(&text);
        self.lines.push(text);
        self.frames.push(std::sync::Arc::from(
            self.open.iter().rev().cloned().collect::<Vec<_>>(),
        ));
    }
}

/// Expand `include "..."` directives recursively.
pub fn expand_includes(
    path: &Path,
    macros: impl Into<MacroDefs>,
    config: &DbLoadConfig,
    faults: &mut DbFaults,
) -> CaResult<String> {
    Ok(expand_includes_mapped(&DbOpenedFile::taken_outright(path), macros, config, faults)?.text)
}

/// One file on C's `inputFileList` while the read is running.
///
/// TWO names, because the file has two jobs and they need different
/// spellings: `identity` is the canonicalised path, which is what
/// decides whether two `include` directives name one file, and `named`
/// is what the operator wrote, which is the only one a diagnostic may
/// print. Keeping both on the stack is what stops the identity from
/// being the nearest string to hand when a message needs a file name —
/// the way `/tmp/…/a.db -> /tmp/…/b.db -> /tmp/…/a.db` used to reach an
/// operator who wrote `include "b.db"`.
struct OpenFile {
    identity: PathBuf,
    named: String,
}

fn expand_includes_inner(
    opened: &DbOpenedFile,
    table: &mut super::MacroTable,
    config: &DbLoadConfig,
    stack: &mut Vec<OpenFile>,
    faults: &mut DbFaults,
    out: &mut Lines,
) -> CaResult<()> {
    let identity = opened
        .resolved
        .canonicalize()
        .unwrap_or_else(|_| opened.resolved.clone());
    let named = &opened.named;
    // Circular include detection
    if stack.iter().any(|f| f.identity == identity) {
        let chain: Vec<&str> = stack.iter().map(|f| f.named.as_str()).collect();
        return Err(CaError::DbParseError {
            line: 0,
            token: String::new(),
            message: format!("circular include: {} -> {named}", chain.join(" -> ")),
        });
    }

    // Depth limit
    if stack.len() >= config.max_include_depth {
        return Err(CaError::DbParseError {
            line: 0,
            token: String::new(),
            message: format!(
                "include depth limit ({}) exceeded at '{named}'",
                config.max_include_depth,
            ),
        });
    }

    let content = std::fs::read_to_string(&identity).map_err(|e| CaError::DbParseError {
        line: 0,
        token: String::new(),
        message: format!("cannot read '{named}': {e}"),
    })?;

    stack.push(OpenFile {
        identity,
        named: named.clone(),
    });

    // No per-file copy of the macros: a `substitute` inside an included
    // file writes the shared table and its definition outlives the
    // include, because msi shares one handle across `include` and pushes
    // no scope for it (`msi.cpp:305-358`). Measured — a file defining
    // `X=inner` under an outer `X=outer` leaves `after:[inner]` behind
    // it, where this port used to restore `outer`.

    // Include search path. Starts from the caller-supplied config and
    // is mutated by `path`/`addpath` directives encountered in the
    // file (L-3): C `dbPathCmd` replaces the path, `dbAddPathCmd`
    // appends to it (`dbLexRoutines.c:433-441`).
    let mut local_paths: Vec<PathBuf> = config.include_paths.clone();

    // This file joins C's `inputFileList`; every line pushed while it is
    // on top is attributed to it, at the line its own reader has reached.
    let file_name = opened.named.clone();
    out.open.push(super::DbIncludeFrame {
        path: opened.found_under.clone(),
        filename: Some(file_name.clone()),
        line: 0,
    });
    // A directive line contributes no text but still costs a line number
    // in the file the operator is reading, so it is replaced by an empty
    // one. C never has to do this — its directives ARE grammar
    // productions, so every source line reaches the lexer and its counter
    // never skips.
    let emit = |out: &mut Lines, n: u32, text: String| {
        if let Some(frame) = out.open.last_mut() {
            frame.line = n;
        }
        out.push(text);
    };
    for (i, raw) in content.split_inclusive('\n').enumerate() {
        let line_num = i as u32 + 1;
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        if let Some(subst_str) = parse_substitute_directive(line) {
            // msi is the reference for this one: C's `.db` grammar has no
            // `substitute` directive at all. `makeSubstitutions` matches
            // its two commands on the RAW line and sets `expand = 0`, so
            // a command line is never itself expanded (`msi.cpp:305-358`),
            // and `addMacroReplacements` hands the text between the outer
            // quotes to `macParseDefns` and straight on to
            // `macInstallMacros` (`:256-275`) — so what is installed is
            // the RAW value. This port expanded it first, which froze a
            // reference C leaves live.
            for (name, value) in parse_macro_defns(&subst_str) {
                match value {
                    Some(rawval) => table.define(&name, rawval),
                    // C `macPutValue( handle, name, NULL )`: a definition
                    // with no `=` deletes, uncovering an outer one.
                    None => table.undefine(&name),
                }
            }
            emit(out, line_num, String::from("\n"));
            continue;
        }
        // Every other line goes through the file's one table exactly
        // once, BEFORE anything looks at what it says. C `db_yyinput`
        // (`dbLexRoutines.c:375-391`) expands each `fgets` line and hands
        // the result to the lexer, so `include`, `path` and `addpath` are
        // grammar productions reading already-expanded text and carry the
        // same two per-line diagnostics as any other line. This port used
        // to match them on the raw line and expand only the argument,
        // which left an `include "$(NOPE)b.db"` line without the
        // `WARNING: … has undefined macros` C prints for it.
        let expanded = super::db_expand_line_in(table, raw, Some(&file_name), line_num);
        let line = expanded.strip_suffix('\n').unwrap_or(&expanded);
        if let Some(dirs) = parse_path_directive(line, "path") {
            // `path "a:b:c"` — replace the search path. C separates
            // entries with the OS path separator.
            local_paths = db_path(&dirs);
            emit(out, line_num, String::from("\n"));
        } else if let Some(dirs) = parse_path_directive(line, "addpath") {
            // `addpath "a:b"` — append to the search path.
            local_paths.extend(db_add_path(&dirs));
            emit(out, line_num, String::from("\n"));
        } else if let Some(expanded_filename) = parse_include_directive(line) {
            // C `dbIncludeNew` (`dbLexRoutines.c:450-456`) prints this
            // exact line and calls `yyerror(NULL)`, NOT `yyerrorAbort`:
            // the include is skipped, everything already read stays, and
            // the load's status goes non-zero at the end. The port used
            // to propagate the failure out of the whole expansion, which
            // discarded every record in the enclosing file.
            // The directive's own line is where this file's reader is
            // parked while the included one is read, which is what C's
            // outer `dbIncludePrint` frame reports.
            if let Some(frame) = out.open.last_mut() {
                frame.line = line_num;
            }
            let Some(included) = db_open_file_located(&expanded_filename, &local_paths) else {
                faults.recoverable(format!(
                    "{ERL_ERROR}: Can't open include file '{expanded_filename}'"
                ));
                continue;
            };
            expand_includes_inner(&included, table, config, stack, faults, out)?;
        } else {
            emit(out, line_num, expanded);
        }
    }

    out.open.pop();
    stack.pop();
    Ok(())
}

/// Parse an include directive line. Returns the filename if the line is an include directive.
pub(crate) fn parse_include_directive(line: &str) -> Option<String> {
    let trimmed = line.trim();
    // Comment lines are not include directives
    if trimmed.starts_with('#') {
        return None;
    }
    if !trimmed.starts_with("include") {
        return None;
    }
    // Must have whitespace or quote after "include"
    let rest = &trimmed["include".len()..];
    if rest.is_empty() {
        return None;
    }
    // SAFETY: `rest` is non-empty (checked by `is_empty()` above)
    let first = rest.chars().next().unwrap();
    if !first.is_whitespace() && first != '"' {
        return None;
    }
    // Extract quoted filename
    let quote_start = rest.find('"')?;
    let after_quote = &rest[quote_start + 1..];
    let quote_end = after_quote.find('"')?;
    Some(after_quote[..quote_end].to_string())
}

/// Parse a `substitute` directive line.
///
/// msi is the reference: C's `.db` grammar has no `substitute` at all.
/// `makeSubstitutions` (`msi.cpp:305-358`) scans for the closing quote
/// with `\"` skipped as a pair, and gives the text between the quotes to
/// `macParseDefns` WITHOUT unescaping it — so a value written
/// `A=\"1\"` reaches macLib as the six characters `\"1\"` and comes out
/// of the level-1 pass as `"1"`. Reading the closing quote as the first
/// `"` instead truncated the value at the escape.
///
/// After that quote msi allows only blanks before the end of the line;
/// anything else and the line is not a command at all, and is expanded as
/// ordinary text.
pub(crate) fn parse_substitute_directive(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with('#') {
        return None;
    }
    let rest = trimmed.strip_prefix("substitute")?;
    let first = rest.chars().next()?;
    if !first.is_whitespace() && first != '"' {
        return None;
    }
    let quote_start = rest.find('"')?;
    let after_quote = &rest[quote_start + 1..];
    let mut end = 0;
    let bytes = after_quote.as_bytes();
    while end < bytes.len() && bytes[end] != b'"' {
        end += if bytes[end] == b'\\' && end + 1 < bytes.len() && bytes[end + 1] == b'"' {
            2
        } else {
            1
        };
    }
    if end >= bytes.len() {
        return None;
    }
    if !after_quote[end + 1..].chars().all(|c| c == ' ') {
        return None;
    }
    Some(after_quote[..end].to_string())
}

/// Parse a `path "..."` / `addpath "..."` directive line. `keyword`
/// is `"path"` or `"addpath"`. Returns the quoted directory list if
/// the line is that directive.
///
/// Mirrors C dbStatic (`dbYacc.y:71-81`): `tokenPATH`/`tokenADDPATH`
/// followed by a quoted string.
pub(crate) fn parse_path_directive(line: &str, keyword: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with('#') {
        return None;
    }
    let rest = trimmed.strip_prefix(keyword)?;
    // The keyword must be followed by whitespace or a quote, so
    // `pathological` is not mistaken for `path`.
    let first = rest.chars().next()?;
    if !first.is_whitespace() && first != '"' {
        return None;
    }
    let quote_start = rest.find('"')?;
    let after_quote = &rest[quote_start + 1..];
    let quote_end = after_quote.find('"')?;
    Some(after_quote[..quote_end].to_string())
}

/// C `OSI_PATH_LIST_SEPARATOR` (`osiFileName.h:22-28`) — ONE separator
/// per platform, never both. On Unix a `;` is an ordinary character in
/// a directory name and there is no `C:` drive prefix, so neither may
/// be given a second meaning here.
pub const PATH_LIST_SEPARATOR: char = if cfg!(windows) { ';' } else { ':' };

/// C `dbAddPath` (`dbStaticLib.c:663-735`) — the directories a path
/// list contributes to the search path, in order.
///
/// White space around an element is removed, and an EMPTY element
/// anywhere in a non-empty list — leading, doubled or trailing — means
/// the current directory, which C appends as a single `.` at the END
/// of the list rather than at the empty element's position. A list
/// that is entirely white space contributes nothing.
pub fn db_add_path(list: &str) -> Vec<PathBuf> {
    use crate::runtime::stdlib::c_isspace;

    let mut out: Vec<PathBuf> = Vec::new();
    let mut expecting_path = false;
    let mut saw_missing_path = false;
    let mut rest = list;

    while let Some(c) = rest.chars().next() {
        if c_isspace(c) {
            rest = &rest[c.len_utf8()..];
            continue;
        }
        match rest.find(PATH_LIST_SEPARATOR) {
            Some(0) => {
                saw_missing_path = true;
                rest = &rest[PATH_LIST_SEPARATOR.len_utf8()..];
            }
            Some(i) => {
                expecting_path = true;
                out.push(PathBuf::from(rest[..i].trim_end_matches(c_isspace)));
                rest = &rest[i + PATH_LIST_SEPARATOR.len_utf8()..];
            }
            None => {
                expecting_path = false;
                out.push(PathBuf::from(rest.trim_end_matches(c_isspace)));
                rest = "";
            }
        }
    }

    if expecting_path || saw_missing_path {
        out.push(PathBuf::from("."));
    }
    out
}

/// C `dbPath` (`dbStaticLib.c:655-661`) — the same split, but it
/// REPLACES the search path, and an empty string means `.`. C tests
/// `strlen(path)==0`, so a list that is blank rather than empty is
/// still handed to `dbAddPath`, which contributes nothing.
pub fn db_path(list: &str) -> Vec<PathBuf> {
    if list.is_empty() {
        return vec![PathBuf::from(".")];
    }
    db_add_path(list)
}

/// C `pdbbase->pathPvt` — the search path a load INSTALLED, which is what
/// `dbDumpPath` (`dbStaticLib.c:3262-3283`) reports.
///
/// C sets it in `dbReadCOM` (`dbLexRoutines.c:244-253`), the routine every
/// `dbLoadDatabase`/`dbLoadRecords` passes through, and reports `no path
/// defined` until the first load has run. The port resolves the same list
/// from the same environment variable, so the only thing it lacked was
/// somewhere to keep it: without this, a report could only print the list
/// the NEXT load would use, which is `.` on an IOC that has loaded nothing
/// where C prints `no path defined`.
///
/// Single owner: `db_load_config` resolves the list for both
/// `dbLoadRecords` and `dbLoadTemplate`, so it is the one caller of
/// [`set_loaded_path`] and no other path can install one.
static LOADED_PATH: Mutex<Option<Vec<PathBuf>>> = Mutex::new(None);

/// Record the path list this load resolved. C `dbPath`, called once per
/// `dbReadCOM`, REPLACES the list rather than adding to it.
pub fn set_loaded_path(paths: &[PathBuf]) {
    *LOADED_PATH.lock().unwrap_or_else(|e| e.into_inner()) = Some(paths.to_vec());
}

/// The path list the last load installed, or `None` when nothing has been
/// loaded — C's empty `pathPvt`.
pub fn loaded_path() -> Option<Vec<PathBuf>> {
    LOADED_PATH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Parse a `name=value,name2=value2,...` macro-definition string into the
/// `macPutValue` calls C would make for it, in order.
///
/// C `macParseDefns` (`macUtil.c:31-247`). A `,` or `=` is a separator
/// only outside `'...'`/`"..."` and not escaped by a backslash, and
/// whitespace around a name or a value is trimmed. Three of its rules
/// this port did not have, each measured through `msi`:
///
///   * quotes and escapes are removed from NAMES and left in VALUES
///     ("unlike values, they will not be re-parsed", `:199-227`), because
///     a value is re-read by `trans` at level 1 and a name never is. So
///     `'A'=1` defines `A`, while `A="1"` defines `A` as `"1"` and it is
///     the level-1 pass that strips those quotes. Measured with
///     `msi -M B=preset` on `substitute "'A'=1,B"`: `A=[1]`.
///   * a name with no `=` after it is a DELETION. `macParseDefns` leaves
///     the value pointer NULL (`del[i]`, `:105-110`) and
///     `macInstallMacros` passes it to `macPutValue` (`:275`), whose NULL
///     arm deletes the entry — so an OUTER definition of the same name is
///     uncovered. Same measurement: `B=[$(B)]`, undefined, not `preset`.
///     This port dropped such a definition and left `B` standing.
///   * an EMPTY name is a name. `=1` installs a macro called `""` and
///     `$()` resolves it; measured with `msi` on `substitute "=1,B=2"`:
///     `empty:[1]`.
///
/// `None` is C's NULL value: delete the name rather than define it.
pub(crate) fn parse_macro_defns(defns: &str) -> Vec<(String, Option<String>)> {
    /// C `macParseDefns`'s four states (`macUtil.c:57`), kept because the
    /// fall-through between them is what decides where a `,` with no `=`
    /// before it leaves the parse.
    enum St {
        PreName,
        InName,
        PreValue,
        InValue,
    }

    let chars: Vec<char> = defns.chars().collect();
    // C's `end[num]--` loop: trailing whitespace comes off the RAW field,
    // before any quote is removed from it.
    let trimmed = |from: usize, to: usize| -> String {
        let mut end = to;
        while end > from && chars[end - 1].is_whitespace() {
            end -= 1;
        }
        chars[from..end].iter().collect()
    };

    let mut pairs: Vec<(String, Option<String>)> = Vec::new();
    let mut name = String::new();
    let mut del = false;
    let mut start = 0;
    let mut state = St::PreName;
    let mut quote: Option<char> = None;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        // C updates `quote` BEFORE the state switch, so the OPENING quote
        // character is inside the quote and the CLOSING one is outside it.
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        }
        let quoted = quote.is_some();
        let escape = c == '\\' && i + 1 < chars.len();

        loop {
            match state {
                St::PreName => {
                    if !quoted && !escape && (c.is_whitespace() || c == ',') {
                        break;
                    }
                    start = i;
                    state = St::InName;
                }
                St::InName => {
                    if quoted || escape || (c != '=' && c != ',') {
                        break;
                    }
                    name = trimmed(start, i);
                    del = c == ',';
                    state = St::PreValue;
                    if c != ',' {
                        break;
                    }
                }
                St::PreValue => {
                    if !quoted && !escape && c.is_whitespace() {
                        break;
                    }
                    start = i;
                    state = St::InValue;
                }
                St::InValue => {
                    if quoted || escape || c != ',' {
                        break;
                    }
                    let value = trimmed(start, i);
                    pairs.push((dequote(&name), (!del).then_some(value)));
                    del = false;
                    state = St::PreName;
                    break;
                }
            }
        }
        i += if escape { 2 } else { 1 };
    }

    // C's "tidy up from state at end of string" (`macUtil.c:135-155`),
    // whose own fall-through is why a trailing name with no `=` still
    // produces a pair, and a trailing `,` produces none.
    match state {
        St::PreName => {}
        St::InName => pairs.push((dequote(&trimmed(start, chars.len())), None)),
        St::PreValue => pairs.push((dequote(&name), Some(String::new()))),
        St::InValue => {
            let value = trimmed(start, chars.len());
            pairs.push((dequote(&name), (!del).then_some(value)));
        }
    }
    pairs
}

/// C `macParseDefns`'s in-place name cleanup (`macUtil.c:199-227`):
/// quotes are dropped and a backslash is dropped in favour of the
/// character after it.
///
/// The two interact the way C's loop does and not the way a reader
/// expects: the escape branch runs after the quote branch has already
/// declined the character, so `\"` yields a LITERAL `"` in the name
/// rather than opening a quoted region. That is why `substitute
/// "\"A\"=1"` defines a macro whose name is `"A"`, quotes included, and
/// `$(A)` stays undefined.
fn dequote(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len());
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
                i += 1;
                continue;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            i += 1;
            continue;
        }
        if c == '\\' && i + 1 < chars.len() {
            i += 1;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// C `dbOpenFile` (`dbLexRoutines.c:166-193`) — the single rule every
/// `.db` file name is resolved by, whether it arrives as the top-level
/// `dbLoadRecords` argument (`:282`), an `include` directive (`:450`
/// `dbIncludeNew`) or a `.substitutions` template (`dbLoadTemplate`
/// driving `dbLoadRecords`).
///
/// A bare open happens ONLY when the path list is empty or the name
/// already carries a separator; otherwise the list is walked in order
/// and the first hit wins. C never tries the process CWD and never
/// tries the including file's directory — with the list unset
/// `dbReadCOM` (`:244-253`) installs `"."`, which is the same file the
/// bare open reaches, so the empty-list branch here matches it.
///
/// Both C callers run `db_expand_file_name` (`macEnvExpand`) on the
/// name immediately before calling `dbOpenFile` — `dbReadCOM`
/// (`:276`) and `dbIncludeNew` (`:449`) — so the expansion is done
/// here, once, rather than at each caller.
pub fn db_open_file(filename: &str, path_list: &[PathBuf]) -> Option<PathBuf> {
    db_open_file_located(filename, path_list).map(|opened| opened.resolved)
}

/// A `.db` file the loader has opened, in the three parts C keeps
/// separate: what to read, what to CALL it, and the search-path entry it
/// was found under.
///
/// C never conflates them. `dbOpenFile` returns the directory it matched
/// and joins nothing into the caller's name; `dbReadCOM` stores that
/// directory as `inputFile.path` and the operator's own spelling as
/// `inputFile.filename` (`dbLexRoutines.c:274-283`), and every loader
/// diagnostic afterwards prints those two — `dbIncludePrint` writes
/// `path "." file "compressTest.db"`, never the joined path and never a
/// canonical one. A port that carries only the resolved `PathBuf` cannot
/// say what C says: it has already lost the operator's spelling, so it
/// would name a file in words the `st.cmd` does not contain.
#[derive(Clone, Debug)]
pub struct DbOpenedFile {
    /// The path to actually read. Nothing prints this.
    pub resolved: PathBuf,
    /// C `inputFile.filename` — the name as written, after
    /// `macEnvExpand`. Every diagnostic names the file with this.
    pub named: String,
    /// C `inputFile.path` — the search-path entry that matched, `None`
    /// when the name was taken outright (C's NULL `dbOpenFile` return).
    pub found_under: Option<String>,
}

impl DbOpenedFile {
    /// A path the caller resolved for itself, with no search-path entry
    /// behind it — C's `dbReadCOM` arm that is handed an open `FILE *`.
    pub fn taken_outright(path: &Path) -> Self {
        Self {
            resolved: path.to_path_buf(),
            named: path.display().to_string(),
            found_under: None,
        }
    }
}

/// [`db_open_file`] keeping everything C's caller keeps — see
/// [`DbOpenedFile`].
pub fn db_open_file_located(filename: &str, path_list: &[PathBuf]) -> Option<DbOpenedFile> {
    let named = db_expand_file_name(filename)?;
    let filename = named.as_str();
    if path_list.is_empty() || filename.contains('/') || filename.contains('\\') {
        let direct = PathBuf::from(filename);
        return direct.exists().then(|| DbOpenedFile {
            resolved: direct,
            named: named.clone(),
            found_under: None,
        });
    }
    path_list.iter().find_map(|dir| {
        let candidate = dir.join(filename);
        candidate.exists().then(|| DbOpenedFile {
            resolved: candidate,
            named: named.clone(),
            found_under: Some(dir.display().to_string()),
        })
    })
}

/// C `macEnvExpand` (`macEnv.c:21-30`) — `macDefExpand(str, NULL)`, i.e.
/// expansion against a handle whose only source is the process
/// environment. `None` is the C `NULL` return: `macExpandString`
/// reports a negative length as soon as one reference resolves to
/// nothing, and both callers treat that as "no file".
///
/// The `.db` reader's own macro pass runs first and leaves an
/// unresolved reference as the re-expandable placeholder
/// `$(NAME,undefined)` (`macCore.c:911-913`); because that placeholder
/// is still a reference, this second, environment-backed pass is what
/// lets `include "$(TOP)/db/common.db"` load when `TOP` is an
/// environment variable rather than a `dbLoadRecords` substitution.
pub fn db_expand_file_name(filename: &str) -> Option<String> {
    let expanded = super::expand_macros(
        filename,
        &HashMap::new(),
        super::MacroExpandOptions {
            env_fallback: true,
            ..Default::default()
        },
    );
    (!expanded.errored()).then_some(expanded.text)
}

#[cfg(test)]
mod macro_defns_tests {
    use super::*;

    /// `(name, Some(value))` written compactly.
    fn def(name: &str, value: &str) -> (String, Option<String>) {
        (name.to_string(), Some(value.to_string()))
    }

    /// `(name, None)` — C's NULL value, which deletes.
    fn del(name: &str) -> (String, Option<String>) {
        (name.to_string(), None)
    }

    #[test]
    fn simple_pairs() {
        assert_eq!(
            parse_macro_defns("A=1,B=2"),
            vec![def("A", "1"), def("B", "2")]
        );
    }

    #[test]
    fn whitespace_trimmed() {
        assert_eq!(
            parse_macro_defns(" A = 1 , B = 2 "),
            vec![def("A", "1"), def("B", "2")]
        );
    }

    /// a comma inside a quoted value does NOT split the pair, and the
    /// quotes stay on the value — `trans` at level 1 is what removes them.
    #[test]
    fn quoted_comma_not_split() {
        assert_eq!(
            parse_macro_defns(r#"MSG="a,b",N=1"#),
            vec![def("MSG", r#""a,b""#), def("N", "1")]
        );
    }

    /// an `=` inside a quoted value does not start a new value.
    #[test]
    fn quoted_equals_not_split() {
        assert_eq!(
            parse_macro_defns(r#"EXPR="x=y""#),
            vec![def("EXPR", r#""x=y""#)]
        );
    }

    /// An unquoted comma still splits, and what follows it has no `=`, so
    /// it is a DELETION of `b` rather than nothing at all.
    #[test]
    fn unquoted_comma_splits_and_the_tail_deletes() {
        assert_eq!(
            parse_macro_defns("MSG=a,b"),
            vec![def("MSG", "a"), del("b")]
        );
    }

    /// An escaped separator is literal, not a split point, and the
    /// backslash stays in the VALUE for the level-1 pass to consume.
    #[test]
    fn escaped_separator_is_literal() {
        assert_eq!(parse_macro_defns(r"K=a\,b"), vec![def("K", r"a\,b")]);
    }

    /// Quotes come OFF a name — measured with `msi -M B=preset` on
    /// `substitute "'A'=1,B"`, which prints `A=[1] B=[$(B)]`.
    #[test]
    fn quotes_are_removed_from_a_name_and_a_bare_name_deletes() {
        assert_eq!(parse_macro_defns("'A'=1,B"), vec![def("A", "1"), del("B")]);
    }

    /// An ESCAPED quote in a name is a literal quote character, because
    /// C's cleanup loop reaches the escape branch only after the quote
    /// branch has declined the character. `msi` on `substitute
    /// "\"A\"=1"` leaves `$(A)` undefined for exactly this reason.
    #[test]
    fn an_escaped_quote_stays_in_the_name() {
        assert_eq!(parse_macro_defns(r#"\"A\"=1"#), vec![def(r#""A""#, "1")]);
    }

    /// An empty name is a name: `msi` on `substitute "=1,B=2"` prints
    /// `empty:[1]` for `$()`.
    #[test]
    fn an_empty_name_is_still_a_definition() {
        assert_eq!(
            parse_macro_defns("=1,B=2"),
            vec![def("", "1"), def("B", "2")]
        );
    }

    /// A trailing comma closes the previous pair and opens nothing.
    #[test]
    fn a_trailing_comma_adds_nothing() {
        assert_eq!(parse_macro_defns("A=1,"), vec![def("A", "1")]);
    }

    /// A name with `=` and nothing after it defines the empty value, which
    /// is not the same as deleting.
    #[test]
    fn an_empty_value_is_not_a_deletion() {
        assert_eq!(parse_macro_defns("A="), vec![def("A", "")]);
    }

    #[test]
    fn empty_input() {
        assert!(parse_macro_defns("").is_empty());
    }
}

#[cfg(test)]
mod db_add_path_tests {
    use super::*;

    /// A list literal is written with `:`, and rendered here into the
    /// ONE separator the platform under test splits on. Writing `:`
    /// directly would assert the Unix build's separator on Windows,
    /// where `PATH_LIST_SEPARATOR` is `;` and a `:` is the ordinary
    /// character of a drive prefix.
    fn sep(list: &str) -> String {
        list.replace(':', &PATH_LIST_SEPARATOR.to_string())
    }

    fn p(list: &str) -> Vec<String> {
        db_add_path(&sep(list))
            .iter()
            .map(|d| d.display().to_string())
            .collect()
    }

    /// The boundaries C `dbAddPath` (`dbStaticLib.c:663-735`)
    /// distinguishes: where the empty element sits, and how many there
    /// are. All of them add exactly one `.`, and always last.
    #[test]
    fn an_empty_element_anywhere_appends_one_dot_at_the_end() {
        assert_eq!(p("a:b"), ["a", "b"]);
        assert_eq!(p(":a"), ["a", "."]);
        assert_eq!(p("a:"), ["a", "."]);
        assert_eq!(p("a::b"), ["a", "b", "."]);
        assert_eq!(p(":a::b:"), ["a", "b", "."]);
        assert_eq!(p(":"), ["."]);
    }

    /// White space around an element is removed, and white space
    /// standing alone between separators is an empty element.
    #[test]
    fn white_space_is_trimmed_and_a_blank_element_is_empty() {
        assert_eq!(p("  a  :\tb\t"), ["a", "b"]);
        assert_eq!(p("a : : b"), ["a", "b", "."]);
        assert!(p("   ").is_empty());
        assert!(p("").is_empty());
    }

    /// C splits on the ONE platform separator. On Unix that is `:`,
    /// so a `;` is an ordinary character and a `C:` prefix is two
    /// elements — the port's drive-letter special case was invented.
    #[cfg(not(windows))]
    #[test]
    fn only_the_platform_separator_splits() {
        assert_eq!(p("a;b"), ["a;b"]);
        assert_eq!(p(r"C:\epics\db"), ["C", r"\epics\db"]);
    }

    /// C `dbPath` maps the empty string to `.` before it splits, but
    /// tests `strlen`, so a blank list is not empty and contributes
    /// nothing.
    #[test]
    fn db_path_replaces_an_empty_list_with_the_current_directory() {
        assert_eq!(
            db_path(""),
            vec![PathBuf::from(".")],
            "an empty list is the current directory"
        );
        assert!(db_path("   ").is_empty(), "a blank list is not empty");
        assert_eq!(db_path(&sep("a:")), db_add_path(&sep("a:")));
    }
}

#[cfg(test)]
mod db_open_file_tests {
    use super::*;

    /// I-R3-3 boundary matrix for C `dbOpenFile`
    /// (`dbLexRoutines.c:174-175`): {name carries a separator, it does
    /// not} x {path list set, unset}. The process CWD is reachable only
    /// through the bare-open branch, never as a fallback after a failed
    /// list walk.
    #[test]
    fn db_open_file_gate_matches_dbopenfile() {
        // Cargo runs a crate's tests with the package root as the
        // working directory, so this name resolves against the CWD.
        assert!(
            Path::new("Cargo.toml").exists(),
            "test assumes CWD is the package root"
        );
        let on_list = tempfile::tempdir().unwrap();
        std::fs::write(on_list.path().join("Cargo.toml"), "").unwrap();
        let empty = tempfile::tempdir().unwrap();

        // No separator, list set: the list wins over the same-named
        // file in the CWD.
        assert_eq!(
            db_open_file("Cargo.toml", &[on_list.path().to_path_buf()]),
            Some(on_list.path().join("Cargo.toml"))
        );
        // No separator, list set but missing the name: not found. The
        // CWD copy must NOT rescue it.
        assert_eq!(
            db_open_file("Cargo.toml", &[empty.path().to_path_buf()]),
            None
        );
        // No separator, list unset: bare open, i.e. the CWD.
        assert_eq!(
            db_open_file("Cargo.toml", &[]),
            Some(PathBuf::from("Cargo.toml"))
        );
        // Separator present, list set: bare open, list not consulted —
        // so a name the list COULD have resolved is still opened bare.
        assert_eq!(
            db_open_file("./Cargo.toml", &[on_list.path().to_path_buf()]),
            Some(PathBuf::from("./Cargo.toml"))
        );
        let nested = on_list.path().join("sub");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("x.db"), "").unwrap();
        assert_eq!(
            db_open_file("sub/x.db", &[on_list.path().to_path_buf()]),
            None
        );
    }

    /// C runs `macEnvExpand` on the name before `dbOpenFile` on both
    /// paths (`dbLexRoutines.c:276` in `dbReadCOM`, `:449` in
    /// `dbIncludeNew`), so an environment reference in a `.db`,
    /// template or `include` name resolves. The second case is the one
    /// that matters in practice: the `.db` reader's own macro pass has
    /// already rewritten the reference to `$(TOP,undefined)`, and that
    /// placeholder is still a live reference for this pass.
    #[test]
    #[serial_test::serial(epics_env)]
    fn db_open_file_env_expands_the_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("common.db"), "").unwrap();
        let key = "EPICS_RS_TEST_DB_TOP";
        // SAFETY: single-threaded under the `epics_env` serial group.
        unsafe { std::env::set_var(key, super::super::macro_safe_path(dir.path())) };

        let raw = format!("$({key})/common.db");
        assert_eq!(
            db_open_file(&raw, &[]),
            Some(dir.path().join("common.db")),
            "environment reference in a bare name must expand"
        );
        let placeholder = format!("$({key},undefined)/common.db");
        assert_eq!(
            db_open_file(&placeholder, &[]),
            Some(dir.path().join("common.db")),
            "the .db reader's undefined-placeholder must re-expand here"
        );
        // Undefined everywhere is C's NULL return: no file, no open.
        assert_eq!(db_open_file("$(NO_SUCH_VAR_HERE)/common.db", &[]), None);

        // SAFETY: same serial group.
        unsafe { std::env::remove_var(key) };
    }

    /// The whole-file path: `include "$(VAR)/inc.db"` inside a `.db`
    /// reaches `dbIncludeNew`, whose `macEnvExpand` is what makes an
    /// environment-only reference resolve.
    #[test]
    #[serial_test::serial(epics_env)]
    fn include_directive_env_expands_the_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("inc.db"),
            "record(ai,\"SIM:INC\") { field(VAL,\"7\") }\n",
        )
        .unwrap();
        let main = dir.path().join("main.db");
        let key = "EPICS_RS_TEST_INC_TOP";
        std::fs::write(&main, format!("include \"$({key})/inc.db\"\n")).unwrap();
        // SAFETY: single-threaded under the `epics_env` serial group.
        unsafe { std::env::set_var(key, super::super::macro_safe_path(dir.path())) };

        let out = expand_includes(
            &main,
            &HashMap::new(),
            &DbLoadConfig::default(),
            &mut DbFaults::default(),
        )
        .expect("include with an environment reference must resolve");
        assert!(out.contains("SIM:INC"), "expanded output was {out:?}");

        // SAFETY: same serial group.
        unsafe { std::env::remove_var(key) };
    }
}
