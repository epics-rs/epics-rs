use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{CaError, CaResult};

use crate::runtime::log::ERL_ERROR;

use super::substitute_macros;
use super::{DbFaults, DbRecordDef};

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
    macros: &HashMap<String, String>,
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
    macros: &HashMap<String, String>,
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
    macros: &HashMap<String, String>,
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
    macros: &HashMap<String, String>,
    config: &DbLoadConfig,
    faults: &mut DbFaults,
) -> CaResult<DbExpandedText> {
    let mut stack = Vec::new();
    let mut out = Lines::default();
    expand_includes_inner(opened, macros, config, &mut stack, faults, &mut out)?;
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
    macros: &HashMap<String, String>,
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
    macros: &HashMap<String, String>,
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
            column: 0,
            message: format!("circular include: {} -> {named}", chain.join(" -> ")),
        });
    }

    // Depth limit
    if stack.len() >= config.max_include_depth {
        return Err(CaError::DbParseError {
            line: 0,
            column: 0,
            message: format!(
                "include depth limit ({}) exceeded at '{named}'",
                config.max_include_depth,
            ),
        });
    }

    let content = std::fs::read_to_string(&identity).map_err(|e| CaError::DbParseError {
        line: 0,
        column: 0,
        message: format!("cannot read '{named}': {e}"),
    })?;

    stack.push(OpenFile {
        identity,
        named: named.clone(),
    });

    // Local macro overrides from `substitute` directives.
    // These override the caller-provided macros for subsequent includes.
    let mut local_macros = macros.clone();

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
            // Apply substitute overrides to local macros. Quote- and
            // escape-aware splitting matches C `macParseDefns`
            // (macUtil.c): a `,` or `=` inside `'...'`/`"..."` or after
            // a `\` does not act as a separator.
            for (k, v) in parse_macro_defns(&subst_str) {
                let expanded_v = substitute_macros(&v, &local_macros);
                local_macros.insert(k, expanded_v);
            }
            emit(out, line_num, String::from("\n"));
        } else if let Some(dirs) = parse_path_directive(line, "path") {
            // `path "a:b:c"` — replace the search path. C separates
            // entries with the OS path separator.
            let expanded = substitute_macros(&dirs, &local_macros);
            local_paths = db_path(&expanded);
            emit(out, line_num, String::from("\n"));
        } else if let Some(dirs) = parse_path_directive(line, "addpath") {
            // `addpath "a:b"` — append to the search path.
            let expanded = substitute_macros(&dirs, &local_macros);
            local_paths.extend(db_add_path(&expanded));
            emit(out, line_num, String::from("\n"));
        } else if let Some(filename) = parse_include_directive(line) {
            let expanded_filename = substitute_macros(&filename, &local_macros);
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
            expand_includes_inner(&included, &local_macros, config, stack, faults, out)?;
        } else {
            // C `db_yyinput`: the macros in force at this line, and the
            // two diagnostics macLib and the loader raise for it.
            emit(
                out,
                line_num,
                super::db_expand_line(raw, &local_macros, Some(&file_name), line_num),
            );
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
/// EPICS DB files use `substitute "NAME=VALUE"` to override macros for subsequent
/// `include` directives. Returns the quoted content if the line is a substitute directive.
pub(crate) fn parse_substitute_directive(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with('#') {
        return None;
    }
    if !trimmed.starts_with("substitute") {
        return None;
    }
    let rest = &trimmed["substitute".len()..];
    if rest.is_empty() {
        return None;
    }
    // SAFETY: `rest` is non-empty (checked by `is_empty()` above)
    let first = rest.chars().next().unwrap();
    if !first.is_whitespace() && first != '"' {
        return None;
    }
    // Extract quoted content
    let quote_start = rest.find('"')?;
    let after_quote = &rest[quote_start + 1..];
    let quote_end = after_quote.find('"')?;
    Some(after_quote[..quote_end].to_string())
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

/// Parse a `name=value,name2=value2,...` macro-definition string into
/// `(name, value)` pairs, quote- and escape-aware.
///
/// Mirrors C `macParseDefns` (`macUtil.c`): a `,` or `=` is only a
/// separator when it is outside `'...'`/`"..."` and not immediately
/// preceded by a backslash. Whitespace around names/values is
/// trimmed; surrounding quotes are NOT stripped (macLib keeps them so
/// the value can carry literal separators). Definitions with no `=`
/// are dropped.
pub(crate) fn parse_macro_defns(defns: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = defns.chars().collect();
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut name = String::new();
    let mut value = String::new();
    let mut quote: Option<char> = None;
    // false = collecting name, true = collecting value
    let mut in_value = false;
    let mut has_eq = false;
    let mut i = 0;

    let flush = |name: &mut String,
                 value: &mut String,
                 has_eq: &mut bool,
                 pairs: &mut Vec<(String, String)>| {
        let k = name.trim();
        if *has_eq && !k.is_empty() {
            pairs.push((k.to_string(), value.trim().to_string()));
        }
        name.clear();
        value.clear();
        *has_eq = false;
    };

    while i < chars.len() {
        let c = chars[i];
        // Escape: backslash + next char are a literal 2-char unit.
        if c == '\\' && i + 1 < chars.len() {
            if in_value {
                value.push(c);
                value.push(chars[i + 1]);
            } else {
                name.push(c);
                name.push(chars[i + 1]);
            }
            i += 2;
            continue;
        }
        // Quote state.
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            if in_value {
                value.push(c);
            } else {
                name.push(c);
            }
            i += 1;
            continue;
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            if in_value {
                value.push(c);
            } else {
                name.push(c);
            }
            i += 1;
            continue;
        }
        // Unquoted separators.
        match c {
            '=' if !in_value => {
                in_value = true;
                has_eq = true;
            }
            ',' => {
                flush(&mut name, &mut value, &mut has_eq, &mut pairs);
                in_value = false;
            }
            _ if in_value => value.push(c),
            _ => name.push(c),
        }
        i += 1;
    }
    flush(&mut name, &mut value, &mut has_eq, &mut pairs);
    pairs
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

    #[test]
    fn simple_pairs() {
        assert_eq!(
            parse_macro_defns("A=1,B=2"),
            vec![("A".into(), "1".into()), ("B".into(), "2".into())]
        );
    }

    #[test]
    fn whitespace_trimmed() {
        assert_eq!(
            parse_macro_defns(" A = 1 , B = 2 "),
            vec![("A".into(), "1".into()), ("B".into(), "2".into())]
        );
    }

    /// a comma inside a quoted value does NOT split the pair.
    #[test]
    fn quoted_comma_not_split() {
        assert_eq!(
            parse_macro_defns(r#"MSG="a,b",N=1"#),
            vec![("MSG".into(), r#""a,b""#.into()), ("N".into(), "1".into())]
        );
    }

    /// an `=` inside a quoted value does not start a new value.
    #[test]
    fn quoted_equals_not_split() {
        assert_eq!(
            parse_macro_defns(r#"EXPR="x=y""#),
            vec![("EXPR".into(), r#""x=y""#.into())]
        );
    }

    /// An unquoted comma still splits (C parity for `MSG=a,b`).
    #[test]
    fn unquoted_comma_splits() {
        assert_eq!(
            parse_macro_defns("MSG=a,b"),
            // `b` has no `=` so it is dropped (not a definition).
            vec![("MSG".into(), "a".into())]
        );
    }

    /// An escaped separator is literal, not a split point.
    #[test]
    fn escaped_separator_is_literal() {
        assert_eq!(
            parse_macro_defns(r"K=a\,b"),
            vec![("K".into(), r"a\,b".into())]
        );
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
