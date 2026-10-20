use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::CaResult;

use super::reader::Feed;
use super::{DbFaults, DbRecordDef, MacroDefs};

/// Configuration for file-based DB loading with include support.
#[derive(Clone)]
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

/// Build the DB include config the way `dbReadCOM` does
/// (`dbLexRoutines.c:245-253`): a non-empty `explicit_path` wins outright,
/// otherwise `EPICS_DB_INCLUDE_PATH`, otherwise `"."`. Only
/// `dbLoadDatabase` can supply the first arm — C hands `dbLoadRecords` a
/// hard `0` — which is why `explicit_path` had no caller and the arm was
/// missing until the command existed.
///
/// Resolving the list is also what INSTALLS it: C `dbPath` runs inside
/// `dbReadCOM`, so the path a load resolved is the path `dbDumpPath`
/// reports afterwards. This is the only routine that resolves one, so it
/// is the only writer of [`set_loaded_path`].
pub(crate) fn db_load_config(explicit_path: &str) -> DbLoadConfig {
    let include_paths = if explicit_path.is_empty() {
        crate::runtime::env::get("EPICS_DB_INCLUDE_PATH")
            .map_or_else(|| vec![std::path::PathBuf::from(".")], |val| db_path(&val))
    } else {
        db_path(explicit_path)
    };
    set_loaded_path(&include_paths);
    DbLoadConfig {
        include_paths,
        max_include_depth: 32,
    }
}

/// Resolve a load's file name through C `dbOpenFile`: the path list is
/// searched FIRST and the process CWD is never consulted for a bare name.
/// `None` is C's NULL return, which `dbReadCOM` turns into `Can't open
/// file` without ever reaching the parser.
///
/// `include_path` is `dbLoadDatabase`'s second argument, empty for every
/// other caller — see [`db_load_config`].
pub(crate) fn resolve_db_file(
    path: &str,
    include_path: &str,
) -> (DbLoadConfig, Option<DbOpenedFile>) {
    let config = db_load_config(include_path);
    // The located form, not the bare `PathBuf`: every diagnostic the load
    // raises names the file as the operator wrote it and the path entry it
    // was found under, which is what C's `inputFile` keeps — see
    // [`DbOpenedFile`].
    let opened = db_open_file_located(path, &config.include_paths);
    (config, opened)
}

/// File-based entry point: expand includes, substitute macros, parse
/// records. Records-only, so a file-scope alias with no target above it is
/// reported and dropped — see [`super::parse_db`].
pub fn parse_db_file(
    path: &Path,
    macros: impl Into<MacroDefs>,
    config: &DbLoadConfig,
) -> CaResult<Vec<DbRecordDef>> {
    Ok(parse_db_file_with_breaktables(path, macros, config)?.records_with_file_aliases())
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
    parse_db_opened_with_breaktables(&DbOpenedFile::taken_outright(path)?, macros, config)
}

/// [`parse_db_file_with_breaktables`] for a file that came through
/// [`db_open_file_located`], so its diagnostics name it the way the
/// operator wrote it rather than by the path this process resolved.
pub fn parse_db_opened_with_breaktables(
    opened: &DbOpenedFile,
    macros: impl Into<MacroDefs>,
    config: &DbLoadConfig,
) -> CaResult<super::ParsedDb> {
    parse_db_opened_held(opened, macros, config)?.raise_abort()
}

/// [`parse_db_opened_with_breaktables`] for an installer: a parse abort is
/// carried on the result, to be raised once the records above it are in.
pub(crate) fn parse_db_opened_held(
    opened: &DbOpenedFile,
    macros: impl Into<MacroDefs>,
    config: &DbLoadConfig,
) -> CaResult<super::ParsedDb> {
    let mut feed = Feed::files(macros, config, DbFaults::default());
    // A top-level file that cannot be read fails before the parser has a
    // line to stand on (`line: 0`), so it is reported here; every later
    // rejection leaves through the parser.
    if let Err(e) = feed.open(opened) {
        feed.faults.abort(&e);
        return Err(e);
    }
    Ok(super::parse_db_fed(feed).0)
}

/// The text a load of `path` hands its parser: every file of the include
/// tree, expanded, in the order C's lexer reads it. `Err` is the abort
/// that ended the load, past which nothing was read.
pub fn expand_includes(
    path: &Path,
    macros: impl Into<MacroDefs>,
    config: &DbLoadConfig,
    faults: &mut DbFaults,
) -> CaResult<Vec<u8>> {
    let opened = DbOpenedFile::taken_outright(path)?;
    let mut feed = Feed::files(macros, config, std::mem::take(faults));
    if let Err(e) = feed.open(&opened) {
        *faults = feed.into_parts().1;
        return Err(e);
    }
    let (mut parsed, text) = super::parse_db_fed(feed);
    *faults = std::mem::take(&mut parsed.faults);
    match parsed.aborted {
        Some(abort) => Err(abort.error),
        None => Ok(text),
    }
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
pub(crate) fn parse_substitute_directive(line: &[u8]) -> Option<&[u8]> {
    let trimmed = line.trim_ascii();
    if trimmed.starts_with(b"#") {
        return None;
    }
    let rest = trimmed.strip_prefix(b"substitute")?;
    let &first = rest.first()?;
    if !first.is_ascii_whitespace() && first != b'"' {
        return None;
    }
    let quote_start = rest.iter().position(|&b| b == b'"')?;
    let after_quote = &rest[quote_start + 1..];
    let mut end = 0;
    let bytes = after_quote;
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
    if !after_quote[end + 1..].iter().all(|&c| c == b' ') {
        return None;
    }
    Some(&after_quote[..end])
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
pub fn db_add_path(list: impl AsRef<OsStr>) -> Vec<PathBuf> {
    use crate::runtime::stdlib::c_isspace;

    // Split as bytes: the list may be `getenv`'s, and C splits it unchecked.
    let space = |b: &u8| c_isspace(*b as char);
    let sep = PATH_LIST_SEPARATOR as u8;
    let dir = |d: &[u8]| {
        let end = d.len() - d.iter().rev().take_while(|b| space(b)).count();
        // SAFETY: `d` is cut from an `OsStr`'s encoded bytes only at ASCII
        // bytes (the separator, white space), which keeps it valid.
        PathBuf::from(unsafe { OsStr::from_encoded_bytes_unchecked(&d[..end]) })
    };

    let mut out: Vec<PathBuf> = Vec::new();
    let mut expecting_path = false;
    let mut saw_missing_path = false;
    let mut rest = list.as_ref().as_encoded_bytes();

    while let Some(c) = rest.first() {
        if space(c) {
            rest = &rest[1..];
            continue;
        }
        match rest.iter().position(|&b| b == sep) {
            Some(0) => {
                saw_missing_path = true;
                rest = &rest[1..];
            }
            Some(i) => {
                expecting_path = true;
                out.push(dir(&rest[..i]));
                rest = &rest[i + 1..];
            }
            None => {
                expecting_path = false;
                out.push(dir(rest));
                rest = &[];
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
pub fn db_path(list: impl AsRef<OsStr>) -> Vec<PathBuf> {
    let list = list.as_ref();
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
pub(crate) fn parse_macro_defns(defns: &[u8]) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    /// C `macParseDefns`'s four states (`macUtil.c:57`), kept because the
    /// fall-through between them is what decides where a `,` with no `=`
    /// before it leaves the parse.
    enum St {
        PreName,
        InName,
        PreValue,
        InValue,
    }

    let chars = defns;
    let is_space = |c: u8| crate::runtime::stdlib::c_isspace(char::from(c));
    // C's `end[num]--` loop: trailing whitespace comes off the RAW field,
    // before any quote is removed from it.
    let trimmed = |from: usize, to: usize| -> Vec<u8> {
        let mut end = to;
        while end > from && is_space(chars[end - 1]) {
            end -= 1;
        }
        chars[from..end].to_vec()
    };

    let mut pairs: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
    let mut name = Vec::new();
    let mut del = false;
    let mut start = 0;
    let mut state = St::PreName;
    let mut quote: Option<u8> = None;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        // C updates `quote` BEFORE the state switch, so the OPENING quote
        // character is inside the quote and the CLOSING one is outside it.
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if c == b'\'' || c == b'"' {
            quote = Some(c);
        }
        let quoted = quote.is_some();
        let escape = c == b'\\' && i + 1 < chars.len();

        loop {
            match state {
                St::PreName => {
                    if !quoted && !escape && (is_space(c) || c == b',') {
                        break;
                    }
                    start = i;
                    state = St::InName;
                }
                St::InName => {
                    if quoted || escape || (c != b'=' && c != b',') {
                        break;
                    }
                    name = trimmed(start, i);
                    del = c == b',';
                    state = St::PreValue;
                    if c != b',' {
                        break;
                    }
                }
                St::PreValue => {
                    if !quoted && !escape && is_space(c) {
                        break;
                    }
                    start = i;
                    state = St::InValue;
                }
                St::InValue => {
                    if quoted || escape || c != b',' {
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
        St::PreValue => pairs.push((dequote(&name), Some(Vec::new()))),
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
fn dequote(name: &[u8]) -> Vec<u8> {
    let chars = name;
    let mut out = Vec::with_capacity(name.len());
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
                i += 1;
                continue;
            }
        } else if c == b'\'' || c == b'"' {
            quote = Some(c);
            i += 1;
            continue;
        }
        if c == b'\\' && i + 1 < chars.len() {
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
/// `dbIncludeNew`) or a `.substitutions` file (`dbLoadTemplate.y:369`).
///
/// A bare open happens ONLY when the path list is empty or the name
/// already carries a separator; otherwise the list is walked in order
/// and the first hit wins. C never tries the process CWD and never
/// tries the including file's directory — with the list unset
/// `dbReadCOM` (`:244-253`) installs `"."`, which is the same file the
/// bare open reaches, so the empty-list branch here matches it.
///
/// A hit is a file that opens, C's `fopen`: one that is there but will
/// not open is passed over for the next entry. `Err` is the last open's
/// error, the `errno` `dbLoadTemplate` prints.
pub fn db_open_file(filename: &str, path_list: &[PathBuf]) -> std::io::Result<DbOpenedFile> {
    let open = |resolved: PathBuf, found_under: Option<String>| {
        Ok(DbOpenedFile {
            file: File::open(&resolved)?,
            resolved,
            named: filename.to_owned(),
            found_under,
        })
    };
    if path_list.is_empty() || filename.contains('/') || filename.contains('\\') {
        return open(PathBuf::from(filename), None);
    }
    // the list is not empty, so the first entry's open replaces this
    let none_tried = Err(std::io::ErrorKind::NotFound.into());
    path_list.iter().fold(none_tried, |opened, dir| {
        opened.or_else(|_| open(dir.join(filename), Some(dir.display().to_string())))
    })
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
#[derive(Debug)]
pub struct DbOpenedFile {
    /// C's `FILE *`, which is what makes the name found.
    pub file: File,
    /// The path that was opened. Nothing prints this.
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
    pub fn taken_outright(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            resolved: path.to_path_buf(),
            named: path.display().to_string(),
            found_under: None,
        })
    }
}

/// The open of `dbReadCOM` (`dbLexRoutines.c:276`) and `dbIncludeNew`
/// (`:449`): `macEnvExpand` the name, then [`db_open_file`]. `None` is
/// either one's NULL.
pub fn db_open_file_located(filename: &str, path_list: &[PathBuf]) -> Option<DbOpenedFile> {
    db_open_file(&db_expand_file_name(filename)?, path_list).ok()
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
        MacroDefs::new(),
        super::MacroExpandOptions {
            env_fallback: true,
            ..Default::default()
        },
    );
    (!expanded.errored()).then_some(expanded.text)
}

#[cfg(test)]
mod macro_defns_tests {
    fn parse_macro_defns(defns: &str) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        super::parse_macro_defns(defns.as_bytes())
    }

    /// `(name, Some(value))` written compactly.
    fn def(name: &str, value: &str) -> (Vec<u8>, Option<Vec<u8>>) {
        (name.into(), Some(value.into()))
    }

    /// `(name, None)` — C's NULL value, which deletes.
    fn del(name: &str) -> (Vec<u8>, Option<Vec<u8>>) {
        (name.into(), None)
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
        db_add_path(sep(list))
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
        assert_eq!(db_path(sep("a:")), db_add_path(sep("a:")));
    }
}

#[cfg(test)]
mod db_open_file_tests {
    use super::*;

    /// The tests read the expansion as text; every input here is ASCII.
    fn expand_includes(
        path: &std::path::Path,
        macros: impl Into<MacroDefs>,
        config: &DbLoadConfig,
        faults: &mut DbFaults,
    ) -> CaResult<String> {
        super::expand_includes(path, macros, config, faults)
            .map(|b| String::from_utf8(b).expect("ASCII test input"))
    }

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
        let db_open_file =
            |name, list: &[PathBuf]| db_open_file(name, list).ok().map(|o| o.resolved);

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

    /// `EPICS_DB_INCLUDE_PATH` is `getenv`'s bytes; C opens a directory
    /// whose name is not UTF-8.
    #[cfg(unix)]
    #[test]
    fn db_path_keeps_a_non_utf8_directory_s_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let list = OsStr::from_bytes(b" /a\xff :/b");
        assert_eq!(
            db_path(list),
            vec![
                PathBuf::from(OsStr::from_bytes(b"/a\xff")),
                PathBuf::from("/b")
            ]
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
            db_open_file_located(&raw, &[]).map(|o| o.resolved),
            Some(dir.path().join("common.db")),
            "environment reference in a bare name must expand"
        );
        let placeholder = format!("$({key},undefined)/common.db");
        assert_eq!(
            db_open_file_located(&placeholder, &[]).map(|o| o.resolved),
            Some(dir.path().join("common.db")),
            "the .db reader's undefined-placeholder must re-expand here"
        );
        // Undefined everywhere is C's NULL return: no file, no open.
        assert!(db_open_file_located("$(NO_SUCH_VAR_HERE)/common.db", &[]).is_none());

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
            MacroDefs::new(),
            &DbLoadConfig::default(),
            &mut DbFaults::default(),
        )
        .expect("include with an environment reference must resolve");
        assert!(out.contains("SIM:INC"), "expanded output was {out:?}");

        // SAFETY: same serial group.
        unsafe { std::env::remove_var(key) };
    }
}
