//! EPICS `.substitutions` (msi / `dbLoadTemplate`) file support.
//!
//! Mirrors `modules/database/src/ioc/dbtemplate/dbLoadTemplate.y` and
//! its lexer `dbLoadTemplate_lex.l`. A substitutions file describes a
//! set of `dbLoadRecords(template, macros)` invocations:
//!
//! ```text
//! global { G=val }
//! file "template.db" {
//!     pattern { A, B }
//!         { "1", "2" }
//!         { "3", "4" }
//! }
//! file "other.db" {
//!     { A=1, B=2 }
//!     { A=3, B=4 }
//! }
//! ```
//!
//! Each substitution row expands to one template load with the macro
//! set `globals + row`. `global {}` blocks accumulate and apply to
//! every subsequent row (C `sub_locals` / `sub_collect`).
//!
//! Grammar rules implemented (from `dbLoadTemplate.y`):
//!   - `global { var=val, ... }`            — global definitions
//!   - `file WORD|QUOTE { ... }`            — template selection
//!   - `pattern { names } { row } ...`      — positional substitution
//!   - `file { { var=val,... } ... }`       — named substitution
//!   - the deprecated `WORD { ... }` row prefix is accepted (C warns
//!     and ignores the extraneous word).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{CaError, CaResult};

/// Token from the substitutions-file lexer (`dbLoadTemplate_lex.l`).
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Pattern,
    File,
    Global,
    /// Quoted or unquoted value. The lexer collapses both `WORD` and
    /// `QUOTE` into a string; `QUOTE` strips the surrounding quotes.
    Str(String),
    Equals,
    Comma,
    OBrace,
    CBrace,
}

/// Tokenize a substitutions file.
///
/// Lexer rules from `dbLoadTemplate_lex.l`:
///   - `pattern` / `file` / `global` keywords
///   - `bareword`: `[a-zA-Z0-9_\-+:./\[\]<>;]+` (note: also backslash)
///   - quoted `"..."` / `'...'` with `\.` escapes; quotes stripped
///   - `=` `,` `{` `}` punctuation
///   - `#`-to-EOL comments, whitespace skipped
fn lex(input: &str) -> CaResult<Vec<(Tok, usize)>> {
    let chars: Vec<char> = input.chars().collect();
    let mut out: Vec<(Tok, usize)> = Vec::new();
    let mut i = 0;
    let mut line = 1usize;

    let is_bareword = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '_' | '-' | '+' | ':' | '.' | '/' | '\\' | '[' | ']' | '<' | '>' | ';'
            )
    };

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\n' => {
                line += 1;
                i += 1;
            }
            ' ' | '\t' | '\r' => i += 1,
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '=' => {
                out.push((Tok::Equals, line));
                i += 1;
            }
            ',' => {
                out.push((Tok::Comma, line));
                i += 1;
            }
            '{' => {
                out.push((Tok::OBrace, line));
                i += 1;
            }
            '}' => {
                out.push((Tok::CBrace, line));
                i += 1;
            }
            '"' | '\'' => {
                // Quoted string: `\.` escapes; surrounding quotes are
                // stripped. A newline before the closing quote is a
                // lexer error (`dstringchar`/`sstringchar` exclude
                // `\n`).
                let quote = c;
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= chars.len() {
                        return Err(CaError::DbParseError {
                            line,
                            column: 0,
                            message: "unterminated string in substitutions file".into(),
                        });
                    }
                    let ch = chars[i];
                    if ch == quote {
                        i += 1;
                        break;
                    }
                    if ch == '\n' {
                        return Err(CaError::DbParseError {
                            line,
                            column: 0,
                            message: "newline in string in substitutions file".into(),
                        });
                    }
                    if ch == '\\' && i + 1 < chars.len() && chars[i + 1] != '\n' {
                        // Keep escape bytes raw — they are forwarded
                        // into the macro substitution string verbatim,
                        // matching C `dbmfStrdup(yytext+1)`.
                        //
                        // The `!= '\n'` guard is required: C `{escape}`
                        // is `{backslash}.` and flex `.` never matches a
                        // newline (`dstringchar`/`sstringchar` are
                        // `[^"\n\\]`), so a backslash immediately before a
                        // newline is NOT an escape. Skipping this branch
                        // lets the newline reach the `\n` check above,
                        // which is the lexer error C produces.
                        s.push('\\');
                        s.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    s.push(ch);
                    i += 1;
                }
                out.push((Tok::Str(s), line));
            }
            _ if is_bareword(c) => {
                let mut s = String::new();
                while i < chars.len() && is_bareword(chars[i]) {
                    s.push(chars[i]);
                    i += 1;
                }
                let tok = match s.as_str() {
                    "pattern" => Tok::Pattern,
                    "file" => Tok::File,
                    "global" => Tok::Global,
                    _ => Tok::Str(s),
                };
                out.push((tok, line));
            }
            other => {
                return Err(CaError::DbParseError {
                    line,
                    column: 0,
                    message: format!("invalid character '{other}' in substitutions file"),
                });
            }
        }
    }
    Ok(out)
}

/// A single resolved template load: a template filename plus the macro
/// set for that one `dbLoadRecords` invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateLoad {
    pub file: String,
    pub macros: Vec<(String, String)>,
}

/// Recursive-descent parser over the token stream.
struct Parser {
    toks: Vec<(Tok, usize)>,
    pos: usize,
    /// Accumulated `global {}` definitions (C `sub_locals` base).
    globals: Vec<(String, String)>,
    /// Resolved template loads, in file order.
    loads: Vec<TemplateLoad>,
}

impl Parser {
    fn new(toks: Vec<(Tok, usize)>) -> Self {
        Self {
            toks,
            pos: 0,
            globals: Vec::new(),
            loads: Vec::new(),
        }
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|(t, _)| t)
    }

    fn line(&self) -> usize {
        self.toks
            .get(self.pos)
            .or_else(|| self.toks.last())
            .map(|(_, l)| *l)
            .unwrap_or(0)
    }

    fn err(&self, msg: impl Into<String>) -> CaError {
        CaError::DbParseError {
            line: self.line(),
            column: 0,
            message: msg.into(),
        }
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).map(|(t, _)| t.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, want: &Tok) -> CaResult<()> {
        match self.next() {
            Some(ref got) if got == want => Ok(()),
            Some(got) => Err(self.err(format!("expected {want:?}, got {got:?}"))),
            None => Err(self.err(format!("expected {want:?}, got end of file"))),
        }
    }

    fn expect_str(&mut self) -> CaResult<String> {
        match self.next() {
            Some(Tok::Str(s)) => Ok(s),
            Some(got) => Err(self.err(format!("expected a name/value, got {got:?}"))),
            None => Err(self.err("expected a name/value, got end of file")),
        }
    }

    /// `substitution_file: (global_definitions | template_substitutions)*`
    fn parse(&mut self) -> CaResult<Vec<TemplateLoad>> {
        while let Some(tok) = self.peek() {
            match tok {
                Tok::Global => self.parse_global()?,
                Tok::File => self.parse_file()?,
                other => {
                    return Err(self.err(format!("expected 'global' or 'file', got {other:?}")));
                }
            }
        }
        Ok(std::mem::take(&mut self.loads))
    }

    /// `global { var=val, ... }` — accumulates into `self.globals`.
    fn parse_global(&mut self) -> CaResult<()> {
        self.expect(&Tok::Global)?;
        self.expect(&Tok::OBrace)?;
        let defs = self.parse_variable_definitions()?;
        self.expect(&Tok::CBrace)?;
        // C: globals accumulate (`sub_locals += strlen`). A repeated
        // name simply appends; later macLib expansion takes the last.
        self.globals.extend(defs);
        Ok(())
    }

    /// `file WORD|QUOTE { ... }`
    fn parse_file(&mut self) -> CaResult<()> {
        let entry_line = self.line();
        self.expect(&Tok::File)?;
        let filename = self.expect_str()?;
        let loads_before = self.loads.len();
        self.expect(&Tok::OBrace)?;
        // `file "x" {}` — empty body, no loads.
        if self.peek() == Some(&Tok::CBrace) {
            self.next();
        } else {
            match self.peek() {
                Some(Tok::Pattern) => self.parse_pattern_block(&filename)?,
                // variable_substitutions: a sequence of `{ var=val,... }`
                // rows, or nested `global {}` blocks.
                _ => self.parse_variable_substitutions(&filename)?,
            }
            self.expect(&Tok::CBrace)?;
        }
        // C dbLoadTemplate silently drops a `file` entry whose body
        // yields no rows (empty `{}`, row-less pattern, globals-only) —
        // epics-base#666. Upstream is undecided between expand-once and
        // a doc fix, so the semantics stay; only the silence goes.
        if self.loads.len() == loads_before {
            tracing::warn!(
                file = %filename,
                line = entry_line,
                "substitutions entry produced no template loads"
            );
        }
        Ok(())
    }

    /// `pattern { names } { row } { row } ...`
    fn parse_pattern_block(&mut self, filename: &str) -> CaResult<()> {
        self.expect(&Tok::Pattern)?;
        self.expect(&Tok::OBrace)?;
        let mut names: Vec<String> = Vec::new();
        while self.peek() != Some(&Tok::CBrace) {
            match self.peek() {
                Some(Tok::Comma) => {
                    self.next();
                }
                Some(Tok::Str(_)) => names.push(self.expect_str()?),
                Some(other) => {
                    return Err(self.err(format!("expected pattern name, got {other:?}")));
                }
                None => return Err(self.err("unterminated pattern name list")),
            }
        }
        self.expect(&Tok::CBrace)?;

        // pattern_definitions: each `{ row }` (or `global {}`).
        while let Some(tok) = self.peek() {
            match tok {
                Tok::Global => self.parse_global()?,
                Tok::OBrace => {
                    let row = self.parse_pattern_row()?;
                    self.emit_load(filename, self.pattern_macros(&names, &row));
                }
                // Deprecated `WORD { row }` form — C warns and skips
                // the extraneous leading word.
                Tok::Str(_) => {
                    let _extraneous = self.expect_str()?;
                    let row = self.parse_pattern_row()?;
                    self.emit_load(filename, self.pattern_macros(&names, &row));
                }
                Tok::CBrace => break,
                other => return Err(self.err(format!("expected substitution row, got {other:?}"))),
            }
        }
        Ok(())
    }

    /// `{ "v1", "v2", ... }` — a positional value row. Each value
    /// carries the line it was read from, because a surplus one is
    /// reported by line number.
    fn parse_pattern_row(&mut self) -> CaResult<Vec<(String, usize)>> {
        self.expect(&Tok::OBrace)?;
        let mut values: Vec<(String, usize)> = Vec::new();
        while self.peek() != Some(&Tok::CBrace) {
            match self.peek() {
                Some(Tok::Comma) => {
                    self.next();
                }
                Some(Tok::Str(_)) => {
                    let line = self.line();
                    values.push((self.expect_str()?, line));
                }
                Some(other) => {
                    return Err(self.err(format!("expected substitution value, got {other:?}")));
                }
                None => return Err(self.err("unterminated substitution row")),
            }
        }
        self.expect(&Tok::CBrace)?;
        Ok(values)
    }

    /// Zip pattern names with a value row. C `pattern_value` binds
    /// only while `sub_count < var_count` and reports every value past
    /// the last name (`dbLoadTemplate.y:233`, `:250`); `msi.cpp:933`
    /// reports the same surplus as "Warning, too many values given".
    /// Neither aborts the load, and a short row leaving the remaining
    /// names unbound is silent in C, so only the surplus is reported.
    fn pattern_macros(&self, names: &[String], row: &[(String, usize)]) -> Vec<(String, String)> {
        for (_, line) in row.iter().skip(names.len()) {
            tracing::warn!("dbLoadTemplate: Too many values given, line {line}.");
        }
        names
            .iter()
            .zip(row.iter())
            .map(|(n, (v, _))| (n.clone(), v.clone()))
            .collect()
    }

    /// `file "x" { { var=val,... } { ... } ... }` — named rows.
    fn parse_variable_substitutions(&mut self, filename: &str) -> CaResult<()> {
        while let Some(tok) = self.peek() {
            match tok {
                Tok::Global => self.parse_global()?,
                Tok::OBrace => {
                    self.next();
                    let defs = self.parse_variable_definitions()?;
                    self.expect(&Tok::CBrace)?;
                    self.emit_load(filename, defs);
                }
                // Deprecated `WORD { var=val }` form.
                Tok::Str(_) => {
                    let _extraneous = self.expect_str()?;
                    self.expect(&Tok::OBrace)?;
                    let defs = self.parse_variable_definitions()?;
                    self.expect(&Tok::CBrace)?;
                    self.emit_load(filename, defs);
                }
                Tok::CBrace => break,
                other => return Err(self.err(format!("expected substitution row, got {other:?}"))),
            }
        }
        Ok(())
    }

    /// `var=val, var2=val2, ...` — zero or more `WORD = WORD|QUOTE`
    /// pairs separated/terminated by optional commas.
    fn parse_variable_definitions(&mut self) -> CaResult<Vec<(String, String)>> {
        let mut defs: Vec<(String, String)> = Vec::new();
        while self.peek() != Some(&Tok::CBrace) {
            match self.peek() {
                Some(Tok::Comma) => {
                    self.next();
                }
                Some(Tok::Str(_)) => {
                    let name = self.expect_str()?;
                    self.expect(&Tok::Equals)?;
                    let value = self.expect_str()?;
                    defs.push((name, value));
                }
                Some(other) => {
                    return Err(self.err(format!("expected 'name=value', got {other:?}")));
                }
                None => return Err(self.err("unterminated definition block")),
            }
        }
        Ok(defs)
    }

    /// Record one resolved template load: `globals + row` macros.
    fn emit_load(&mut self, filename: &str, row: Vec<(String, String)>) {
        let mut macros = self.globals.clone();
        macros.extend(row);
        self.loads.push(TemplateLoad {
            file: filename.to_string(),
            macros,
        });
    }
}

/// Parse a `.substitutions` file body into the list of template loads
/// it describes. Does not touch the filesystem — see
/// [`load_substitution_file`] for the loading entry point.
pub fn parse_substitutions(input: &str) -> CaResult<Vec<TemplateLoad>> {
    let toks = lex(input)?;
    Parser::new(toks).parse()
}

/// Resolve a `.substitutions` file into the ordered list of
/// `dbLoadRecords` calls it describes — one row per substitution, each
/// carrying the merged macro set (caller macros, then the row's own
/// globals + values overriding them).
///
/// Nothing here touches the filesystem beyond reading the
/// `.substitutions` text, and nothing batches: C `dbLoadTemplate` runs
/// `msiLoadRecords` from the `pattern_definition` action
/// (`dbLoadTemplate.y:186`), so every row's records are already in
/// `pdbbase` before the next row is read and a later failure loses only
/// the remainder. The caller — the only party that owns a database — is
/// therefore the one that must resolve, parse and install row by row.
pub fn substitution_rows(
    path: &Path,
    macros: &HashMap<String, String>,
) -> CaResult<Vec<(String, HashMap<String, String>)>> {
    let content = std::fs::read_to_string(path).map_err(|e| CaError::DbParseError {
        line: 0,
        column: 0,
        message: format!("cannot read substitutions file '{}': {}", path.display(), e),
    })?;
    Ok(parse_substitutions(&content)?
        .into_iter()
        .map(|load| {
            // macLib last-definition-wins: the caller's macros go in
            // first so a row entry of the same name overrides them.
            let mut merged = macros.clone();
            merged.extend(load.macros);
            (load.file, merged)
        })
        .collect())
}

/// Resolve a template filename through C `dbOpenFile`, which owns the
/// `macEnvExpand` pass `dbReadCOM` (`dbLexRoutines.c:276`) runs on
/// every name `dbLoadRecords` is handed — and a `.substitutions`
/// `file` name is handed straight to `dbLoadRecords`
/// (`dbLoadTemplate.y:51`) without any earlier expansion, so the
/// environment is the only thing that can resolve it.
pub(crate) fn resolve_template(filename: &str, include_paths: &[PathBuf]) -> CaResult<PathBuf> {
    super::include::db_open_file(filename, include_paths).ok_or_else(|| CaError::DbParseError {
        line: 0,
        column: 0,
        message: format!("template file not found: '{filename}'"),
    })
}

#[cfg(test)]
mod tests {
    use super::super::include::{DbLoadConfig, parse_db_file_with_breaktables};
    use super::*;

    /// Drive a `.substitutions` file the way `dbLoadTemplate` does:
    /// resolve, parse and collect one row at a time. The command
    /// installs each row's records here; these tests only need the
    /// records, so they concatenate them instead.
    fn load_rows(
        subs: &Path,
        macros: &HashMap<String, String>,
        config: &DbLoadConfig,
    ) -> Vec<super::super::DbRecordDef> {
        substitution_rows(subs, macros)
            .unwrap()
            .into_iter()
            .flat_map(|(file, merged)| {
                let template = resolve_template(&file, &config.include_paths).unwrap();
                parse_db_file_with_breaktables(&template, &merged, config)
                    .unwrap()
                    .records
            })
            .collect()
    }

    #[test]
    fn parse_pattern_substitution() {
        let src = r#"
file "rec.db" {
    pattern { P, N }
        { "IOC:", "1" }
        { "IOC:", "2" }
}
"#;
        let loads = parse_substitutions(src).unwrap();
        assert_eq!(loads.len(), 2);
        assert_eq!(loads[0].file, "rec.db");
        assert_eq!(
            loads[0].macros,
            vec![("P".into(), "IOC:".into()), ("N".into(), "1".into())]
        );
        assert_eq!(
            loads[1].macros,
            vec![("P".into(), "IOC:".into()), ("N".into(), "2".into())]
        );
    }

    #[test]
    fn parse_pattern_with_comma_separated_names() {
        // pattern names may be comma-separated or whitespace-separated.
        let src = r#"file "x.db" { pattern {A,B,C} {"1","2","3"} }"#;
        let loads = parse_substitutions(src).unwrap();
        assert_eq!(loads.len(), 1);
        assert_eq!(
            loads[0].macros,
            vec![
                ("A".into(), "1".into()),
                ("B".into(), "2".into()),
                ("C".into(), "3".into())
            ]
        );
    }

    #[test]
    fn parse_variable_substitution() {
        let src = r#"
file "rec.db" {
    { A=1, B=2 }
    { A=3, B=4 }
}
"#;
        let loads = parse_substitutions(src).unwrap();
        assert_eq!(loads.len(), 2);
        assert_eq!(
            loads[0].macros,
            vec![("A".into(), "1".into()), ("B".into(), "2".into())]
        );
        assert_eq!(
            loads[1].macros,
            vec![("A".into(), "3".into()), ("B".into(), "4".into())]
        );
    }

    #[test]
    fn parse_global_block_applies_to_all_rows() {
        let src = r#"
global { G="gval" }
file "rec.db" {
    pattern { N }
        { "1" }
        { "2" }
}
"#;
        let loads = parse_substitutions(src).unwrap();
        assert_eq!(loads.len(), 2);
        // Globals prepended to every row.
        assert_eq!(
            loads[0].macros,
            vec![("G".into(), "gval".into()), ("N".into(), "1".into())]
        );
        assert_eq!(
            loads[1].macros,
            vec![("G".into(), "gval".into()), ("N".into(), "2".into())]
        );
    }

    #[test]
    fn parse_quoted_filename() {
        let src = r#"file "path/to/rec.db" { { A=1 } }"#;
        let loads = parse_substitutions(src).unwrap();
        assert_eq!(loads[0].file, "path/to/rec.db");
    }

    #[test]
    fn parse_bare_filename() {
        let src = r#"file rec.db { { A=1 } }"#;
        let loads = parse_substitutions(src).unwrap();
        assert_eq!(loads[0].file, "rec.db");
    }

    #[test]
    fn parse_empty_file_body() {
        let src = r#"file "rec.db" { }"#;
        let loads = parse_substitutions(src).unwrap();
        assert!(loads.is_empty());
    }

    /// Parse `src` with a `WARN`-level subscriber installed and return
    /// everything it wrote.
    fn captured(src: &str) -> String {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Buf {
                self.clone()
            }
        }

        let buf = Buf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        parse_substitutions(src).unwrap();
        String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned()
    }

    /// epics-base#666: the zero-load entry stays legal (pinned above),
    /// but must no longer be silent. All three row-less shapes warn;
    /// an entry with rows does not.
    #[test]
    fn zero_load_file_entry_warns() {
        for src in [
            r#"file "rec.db" { }"#,
            r#"file "rec.db" { pattern {N} }"#,
            r#"file "rec.db" { global {A=1} }"#,
        ] {
            let out = captured(src);
            assert!(
                out.contains("no template loads") && out.contains("rec.db"),
                "row-less entry {src:?} must warn, got: {out:?}"
            );
        }
        assert_eq!(
            captured(r#"file "rec.db" { { A=1 } }"#),
            "",
            "an entry with rows must not warn"
        );
    }

    /// C `pattern_value` reports every value past the last pattern name
    /// (`dbLoadTemplate.y:233`, `:250`; `msi.cpp:933`) and drops it. The
    /// port dropped it silently, so a `.substitutions` row that had
    /// drifted out of step with its `pattern` line loaded a template
    /// with the wrong macro set and said nothing. The short row is the
    /// other half of the same rule and stays silent: C leaves the
    /// unmatched name unbound without a diagnostic.
    #[test]
    fn a_value_past_the_last_pattern_name_is_reported() {
        let out = captured("file \"rec.db\" {\n  pattern { A, B }\n  { \"1\", \"2\", \"3\" }\n}");
        assert!(
            out.contains("Too many values given, line 3."),
            "the surplus value must be reported with its line, got: {out:?}"
        );
        assert_eq!(
            out.matches("Too many values given").count(),
            1,
            "one report per surplus value, got: {out:?}"
        );

        let loads = parse_substitutions(
            "file \"rec.db\" {\n  pattern { A, B }\n  { \"1\", \"2\", \"3\" }\n}",
        )
        .unwrap();
        assert_eq!(
            loads[0].macros,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2".to_string())
            ],
            "the surplus value is dropped, the rest still bind"
        );

        assert_eq!(
            captured("file \"rec.db\" {\n  pattern { A, B }\n  { \"1\" }\n}"),
            "",
            "a short row leaves B unbound without a diagnostic, as in C"
        );
    }

    #[test]
    fn parse_empty_pattern_row() {
        // `pattern {N} {}` — empty row is a valid load with no row
        // macros (C `pattern_definition: O_BRACE C_BRACE`).
        let src = r#"file "rec.db" { pattern {N} {} }"#;
        let loads = parse_substitutions(src).unwrap();
        assert_eq!(loads.len(), 1);
        assert!(loads[0].macros.is_empty());
    }

    #[test]
    fn parse_comments_and_whitespace() {
        let src = r#"
# header comment
file "rec.db" {   # trailing comment
    pattern { N }
        { "1" }   # row comment
}
"#;
        let loads = parse_substitutions(src).unwrap();
        assert_eq!(loads.len(), 1);
    }

    #[test]
    fn parse_deprecated_word_prefix_row() {
        // The deprecated `WORD { ... }` row form is accepted (C warns
        // then ignores the extraneous word).
        let src = r#"file "rec.db" { pattern {N} extra {"1"} }"#;
        let loads = parse_substitutions(src).unwrap();
        assert_eq!(loads.len(), 1);
        assert_eq!(loads[0].macros, vec![("N".into(), "1".into())]);
    }

    #[test]
    fn parse_multiple_files() {
        let src = r#"
file "a.db" { { X=1 } }
file "b.db" { { Y=2 } }
"#;
        let loads = parse_substitutions(src).unwrap();
        assert_eq!(loads.len(), 2);
        assert_eq!(loads[0].file, "a.db");
        assert_eq!(loads[1].file, "b.db");
    }

    #[test]
    fn parse_rejects_unterminated_string() {
        let src = "file \"rec.db\" { { A=\"oops\nB=1 } }";
        assert!(parse_substitutions(src).is_err());
    }

    #[test]
    fn parse_rejects_backslash_before_newline_in_string() {
        // C `{escape}` is `{backslash}.` and flex `.` never matches a
        // newline (`dstringchar` is `[^"\n\\]`), so a backslash
        // immediately before a newline is NOT an escape — the string
        // cannot continue across the line and the lexer errors. The
        // backslash must not "swallow" the newline.
        let src = "file \"rec.db\" { { A=\"oops\\\nB\" } }";
        assert!(parse_substitutions(src).is_err());
    }

    #[test]
    fn parse_rejects_missing_keyword() {
        let src = r#"{ A=1 }"#;
        assert!(parse_substitutions(src).is_err());
    }

    #[test]
    fn substitution_rows_drives_template_loads() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let tmpl = dir.path().join("rec.db");
        let mut f = std::fs::File::create(&tmpl).unwrap();
        writeln!(f, r#"record(ai, "$(P)$(N)") {{ field(VAL, "$(N)") }}"#).unwrap();

        let subs = dir.path().join("test.substitutions");
        let mut f = std::fs::File::create(&subs).unwrap();
        writeln!(f, r#"global {{ P="IOC:" }}"#).unwrap();
        writeln!(f, r#"file "rec.db" {{"#).unwrap();
        writeln!(f, r#"    pattern {{ N }}"#).unwrap();
        writeln!(f, r#"        {{ "1" }}"#).unwrap();
        writeln!(f, r#"        {{ "2" }}"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        // The template goes through `dbLoadRecords`, so it is found on
        // the path list — not next to the substitutions file.
        let config = DbLoadConfig {
            include_paths: vec![dir.path().to_path_buf()],
            max_include_depth: 32,
        };
        let recs = load_rows(&subs, &HashMap::new(), &config);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].name, "IOC:1");
        assert_eq!(recs[0].fields[0].1, "1");
        assert_eq!(recs[1].name, "IOC:2");
        assert_eq!(recs[1].fields[0].1, "2");
    }

    /// I-R3-3: a template that also exists next to the `.substitutions`
    /// file must still be taken from the path list. `dbLoadTemplate`
    /// (`dbLoadTemplate.y:51`) hands each `file` entry to
    /// `dbLoadRecords`, whose `dbOpenFile` never looks at the
    /// substitutions file's own directory.
    #[test]
    fn substitution_rows_takes_templates_from_the_path_list() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path_dir = tempfile::tempdir().unwrap();

        let mut f = std::fs::File::create(path_dir.path().join("rec.db")).unwrap();
        writeln!(f, r#"record(ai, "FROM_PATH") {{ }}"#).unwrap();
        let mut f = std::fs::File::create(dir.path().join("rec.db")).unwrap();
        writeln!(f, r#"record(ai, "FROM_SUBS_DIR") {{ }}"#).unwrap();

        let subs = dir.path().join("t.substitutions");
        let mut f = std::fs::File::create(&subs).unwrap();
        writeln!(f, r#"file "rec.db" {{ {{ }} }}"#).unwrap();

        let config = DbLoadConfig {
            include_paths: vec![path_dir.path().to_path_buf()],
            max_include_depth: 32,
        };
        let recs = load_rows(&subs, &HashMap::new(), &config);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "FROM_PATH");
    }

    /// A `.substitutions` `file` name is never touched by the iocsh
    /// expansion — only `dbReadCOM`'s `macEnvExpand`
    /// (`dbLexRoutines.c:276`) can resolve it, so `$(TOP)/x.template`
    /// loads exactly when `TOP` is an environment variable.
    #[test]
    #[serial_test::serial(epics_env)]
    fn substitution_rows_env_expands_the_template_name() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let tpl_dir = tempfile::tempdir().unwrap();
        let key = "EPICS_RS_TEST_SUBS_TOP";

        let mut f = std::fs::File::create(tpl_dir.path().join("t.template")).unwrap();
        writeln!(f, r#"record(ai, "FROM_ENV") {{ }}"#).unwrap();

        let subs = dir.path().join("t.substitutions");
        let mut f = std::fs::File::create(&subs).unwrap();
        writeln!(f, r#"file "$({key})/t.template" {{ {{ }} }}"#).unwrap();

        // SAFETY: single-threaded under the `epics_env` serial group.
        unsafe { std::env::set_var(key, tpl_dir.path()) };
        let recs = load_rows(&subs, &HashMap::new(), &DbLoadConfig::default());
        // SAFETY: same serial group.
        unsafe { std::env::remove_var(key) };

        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "FROM_ENV");
    }

    #[test]
    fn substitution_rows_caller_macros_overridden_by_row() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let tmpl = dir.path().join("rec.db");
        let mut f = std::fs::File::create(&tmpl).unwrap();
        writeln!(f, r#"record(ai, "$(N)") {{ field(VAL, "0") }}"#).unwrap();

        let subs = dir.path().join("v.substitutions");
        let mut f = std::fs::File::create(&subs).unwrap();
        writeln!(f, r#"file "rec.db" {{ {{ N=ROW }} }}"#).unwrap();

        // Caller passes N=CALLER; the row's N=ROW must win.
        let mut macros = HashMap::new();
        macros.insert("N".to_string(), "CALLER".to_string());
        let config = DbLoadConfig {
            include_paths: vec![dir.path().to_path_buf()],
            max_include_depth: 32,
        };
        let recs = load_rows(&subs, &macros, &config);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "ROW");
    }
}
