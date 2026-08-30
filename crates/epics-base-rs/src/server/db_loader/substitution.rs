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

use std::sync::atomic::{AtomicI32, Ordering};

/// Token from the substitutions-file lexer (`dbLoadTemplate_lex.l`).
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Pattern,
    File,
    Global,
    /// A `WORD` or a `QUOTE`, with the quote character that delimited it.
    /// C keeps the two apart to the end: `variable_definition` and
    /// `pattern_value` have one rule per token kind and put the quotes
    /// back around a `QUOTE` when they build `sub_collect`
    /// (`dbLoadTemplate.y:220-255`, `:301-323`).
    Str {
        text: String,
        quote: Option<char>,
    },
    Equals,
    Comma,
    OBrace,
    CBrace,
}

/// The token text C would have in `yytext` when it reports a fault on
/// this token — bison's `yyerror` prints it verbatim
/// (`dbLoadTemplate.y:336`).
fn tok_text(tok: &Tok) -> String {
    match tok {
        Tok::Pattern => "pattern".into(),
        Tok::File => "file".into(),
        Tok::Global => "global".into(),
        // `yytext` is what flex matched, so a quoted token still wears
        // the quotes it was written with.
        Tok::Str {
            text,
            quote: Some(q),
        } => format!("{q}{text}{q}"),
        Tok::Str { text, quote: None } => text.clone(),
        Tok::Equals => "=".into(),
        Tok::Comma => ",".into(),
        Tok::OBrace => "{".into(),
        Tok::CBrace => "}".into(),
    }
}

/// One `yyerror` call. C prints exactly two lines for each
/// (`dbLoadTemplate.y:330-338`):
///
/// ```text
/// Substitution file error: <message>
/// line <line>: '<yytext>'
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct SubstitutionFault {
    pub line: usize,
    /// `None` is C's `yyerror(NULL)`, which prints `Substitution file
    /// error.` and no message of its own (`dbLoadTemplate.y:334-335`).
    pub message: Option<String>,
    /// C's `yytext` at the moment of the report.
    pub yytext: String,
}

impl SubstitutionFault {
    /// C `msiLoadRecords` (`dbLoadTemplate.y:49-57`): the row's
    /// `dbLoadRecords` returned non-zero, so `yyerror` reports and the
    /// action `YYABORT`s.
    ///
    /// `yytext` is `}` for every row, because all four row rules end in
    /// `C_BRACE` and bison reduces them without reading a lookahead — the
    /// lexer is still parked on the brace that closed the row. That is
    /// also why the line is the closing brace's and not the `file` line's
    /// or the next token's.
    pub fn row_failed(load: &TemplateLoad) -> Self {
        Self {
            line: load.line,
            message: Some("Error while reading included file".into()),
            yytext: tok_text(&Tok::CBrace),
        }
    }

    /// C's catch-all lexer rule, `dbLoadTemplate_lex.l:47-56`:
    /// `sprintf(message, "invalid character '%c'", yytext[0])`.
    fn invalid_character(line: usize, c: char) -> Self {
        Self {
            line,
            message: Some(format!("invalid character '{c}'")),
            yytext: c.to_string(),
        }
    }
}

/// One thing the substitutions file asks for, in the order C does it.
/// C interleaves the two: the lexer reports a stray character the moment
/// it reaches it, which is before the rows that follow it and after the
/// rows that precede it.
#[derive(Debug, Clone, PartialEq)]
pub enum SubstitutionEvent {
    /// A `dbLoadRecords` call — one substitution row.
    Load(TemplateLoad),
    /// A fault C reported and carried on from.
    Fault(SubstitutionFault),
    /// A notice C writes straight to stderr rather than through
    /// `yyerror`, so it has no `line <N>: '<yytext>'` frame under it.
    /// The text is C's, newlines included.
    Notice(String),
}

/// A parsed substitutions file: what C would do, in C's order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Substitutions {
    pub events: Vec<SubstitutionEvent>,
    /// The fault that ended the parse, if any. `Some` is C's non-zero
    /// `yyparse` (`dbLoadTemplate.y:417` returns it as the command's
    /// status); the events before it still happened, because C loads
    /// each row from the grammar action as it is reduced.
    pub stopped: Option<SubstitutionFault>,
}

/// Tokenize a substitutions file.
///
/// Lexer rules from `dbLoadTemplate_lex.l`:
///   - `pattern` / `file` / `global` keywords
///   - `bareword`: `[a-zA-Z0-9_\-+:./\[\]<>;]+` (note: also backslash)
///   - quoted `"..."` / `'...'` with `\.` escapes; quotes stripped
///   - `=` `,` `{` `}` punctuation
///   - `#`-to-EOL comments, whitespace skipped
///
/// Cannot fail. C's only lexer error is the catch-all `.` rule
/// (`dbLoadTemplate_lex.l:47-56`), which calls `yyerror`, returns NO token
/// and lets flex resume at the next character — so a stray byte costs one
/// character, not the file. Every fault this raises is one C recovered from.
fn lex(input: &str) -> Lexed {
    let chars: Vec<char> = input.chars().collect();
    let mut out: Vec<(Tok, usize)> = Vec::new();
    // Each fault remembers how many tokens the lexer had produced when
    // it hit it, which is what puts it back in C's order among the rows.
    let mut faults: Vec<(usize, SubstitutionFault)> = Vec::new();
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
                // C's QUOTE rule is ONE flex pattern that has to reach a
                // closing quote on the same line: `dstringchar`/`sstringchar`
                // are `[^"\n\\]`/`[^'\n\\]` and `{escape}` is
                // `{backslash}.`, where flex's `.` never matches a newline.
                // When the pattern cannot match, flex falls through to the
                // `.` rule, which consumes the quote CHARACTER alone. So an
                // unterminated string is not a distinct error in C — it is
                // `invalid character '"'` and the rest of the line lexes as
                // ordinary tokens. Measured on softIoc: `{ N="A1 }` reports
                // the quote and still loads the row as `N=A1`.
                match scan_quoted(&chars, i) {
                    Some((text, next)) => {
                        out.push((
                            Tok::Str {
                                text,
                                quote: Some(c),
                            },
                            line,
                        ));
                        i = next;
                    }
                    None => {
                        faults.push((out.len(), SubstitutionFault::invalid_character(line, c)));
                        i += 1;
                    }
                }
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
                    _ => Tok::Str {
                        text: s,
                        quote: None,
                    },
                };
                out.push((tok, line));
            }
            other => {
                // C `dbLoadTemplate_lex.l:47-56`. One character named, one
                // character dropped, lexing resumes: `{ N=A%1 }` yields the
                // tokens `N`, `=`, `A`, `1` exactly as C's does.
                faults.push((out.len(), SubstitutionFault::invalid_character(line, other)));
                i += 1;
            }
        }
    }
    Lexed {
        toks: out,
        faults,
        final_line: line,
    }
}

/// What one pass of the lexer produced.
struct Lexed {
    toks: Vec<(Tok, usize)>,
    /// Recovered faults, each tagged with the number of tokens the lexer
    /// had produced when it hit the offending character.
    faults: Vec<(usize, SubstitutionFault)>,
    /// `line_num` at end of input — the line C reports a fault at when
    /// there is no token left to name.
    final_line: usize,
}

/// C's `{doublequote}({dstringchar}|{escape})*{doublequote}` as flex applies
/// it: the whole token or nothing. `at` indexes the opening quote; the result
/// is the token text with the quotes stripped — C `dbmfStrdup(yytext+1)` then
/// truncating the last byte (`dbLoadTemplate_lex.l:26-31`) — and the index
/// just past the closing quote.
///
/// Escape bytes stay raw: they are forwarded into the macro substitution
/// string verbatim, which is what C's `dbmfStrdup` of `yytext` does.
fn scan_quoted(chars: &[char], at: usize) -> Option<(String, usize)> {
    let quote = chars[at];
    let mut text = String::new();
    let mut i = at + 1;
    loop {
        let ch = *chars.get(i)?;
        if ch == quote {
            return Some((text, i + 1));
        }
        if ch == '\n' {
            return None;
        }
        if ch == '\\' {
            // `{escape}` is a backslash and ONE character, and flex's `.`
            // does not match a newline, so a backslash at end of line is not
            // an escape and the pattern cannot match from here.
            let next = *chars.get(i + 1)?;
            if next == '\n' {
                return None;
            }
            text.push('\\');
            text.push(next);
            i += 2;
            continue;
        }
        text.push(ch);
        i += 1;
    }
}

/// A single resolved template load: a template filename plus the macro
/// set for that one `dbLoadRecords` invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateLoad {
    pub file: String,
    pub macros: Vec<RowMacro>,
    /// The line of the row's closing brace — `line_num` where C reports a
    /// failure of this row. See [`SubstitutionFault::row_failed`].
    pub line: usize,
}

/// One `name=value` of a substitution row, as C keeps it: the value's
/// text, plus whether it arrived as a `QUOTE` rather than a `WORD`.
/// C decides the echo on the token kind alone — `pattern { A } { "1" }`
/// echoes `A="1"` and `pattern { A } { 1 }` echoes `A=1`, though both
/// substitute the same `1`.
#[derive(Debug, Clone, PartialEq)]
pub struct RowMacro {
    pub name: String,
    pub value: String,
    pub quoted: bool,
}

/// C `int dbTemplateMaxVars = 100;` (`dbLoadTemplate.y:45`), the size of
/// the `vars` array a `pattern` line fills. `dbCore.dbd:29` declares it
/// `variable(dbTemplateMaxVars,int)`, so a startup script raises it with
/// `var dbTemplateMaxVars 200` and the next `dbLoadTemplate` sees the new
/// value — C reads the global at the point of use, and so does this.
///
/// The `sub_collect` buffer C sizes from the same number —
/// `dbTemplateMaxVars * MAX_VAR_FACTOR` (`dbLoadTemplate.y:377`) — is NOT
/// modelled, because C never checks it: every `strcat` into `sub_locals`
/// (`:220-255`, `:301-323`) writes unguarded, so overrunning it is a
/// buffer overflow with no diagnostic to match. The substitution string is
/// a `String` here and has no ceiling — a closed question rather than an
/// open deviation: there is no C text to match, a `String` cannot overflow,
/// and no input can make the two disagree on anything observable.
static DB_TEMPLATE_MAX_VARS: AtomicI32 = AtomicI32::new(100);

/// Read the ceiling — the accessor half of the `var dbTemplateMaxVars`
/// knob ([`crate::server::iocsh::vars`]).
pub fn db_template_max_vars() -> i32 {
    DB_TEMPLATE_MAX_VARS.load(Ordering::Relaxed)
}

/// See [`db_template_max_vars`]. C's `varHandler` writes the global
/// through its raw pointer and validates nothing; `dbLoadTemplate` is
/// where a value below 1 is refused (`dbLoadTemplate.y:355-360`).
pub fn set_db_template_max_vars(value: i32) {
    DB_TEMPLATE_MAX_VARS.store(value, Ordering::Relaxed);
}

/// A `WORD`/`QUOTE` in value position, before it is paired with a name.
struct RowValue {
    text: String,
    quoted: bool,
}

/// Recursive-descent parser over the token stream.
struct Parser {
    toks: Vec<(Tok, usize)>,
    pos: usize,
    /// `line_num` at end of input, for a fault with no token to name.
    final_line: usize,
    /// Accumulated `global {}` definitions (C `sub_locals` base).
    globals: Vec<RowMacro>,
    /// Loads and notices in the order C produces them, each tagged with
    /// the token position it was produced at so the lexer's faults can be
    /// merged back into C's order.
    events: Vec<(usize, SubstitutionEvent)>,
    /// How many of `events` are loads — the `file` entry's row count.
    load_count: usize,
}

/// A parse that stopped: C's `yyparse` returning non-zero. Every row
/// reduced before it still ran its `msiLoadRecords` action, so the
/// loads collected so far are kept.
type ParseResult<T> = Result<T, SubstitutionFault>;

impl Parser {
    fn new(toks: Vec<(Tok, usize)>, final_line: usize) -> Self {
        Self {
            toks,
            pos: 0,
            final_line,
            globals: Vec::new(),
            events: Vec::new(),
            load_count: 0,
        }
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|(t, _)| t)
    }

    fn line(&self) -> usize {
        self.toks
            .get(self.pos)
            .map(|(_, l)| *l)
            .unwrap_or(self.final_line)
    }

    /// Report at the lookahead token, the way bison does: the token that
    /// could not be shifted is still the current one, so it supplies both
    /// `line_num` and `yytext`.
    ///
    /// The sentence is bison's constant, and takes no parameter so that no
    /// grammar site can invent one: C's grammar sets no `parse.error
    /// verbose`, so every way a `.substitutions` file can fail the grammar
    /// reaches `yyerror` with the one string `syntax error`
    /// (`dbLoadTemplate.y:330-338`). Measured over ten failing files on
    /// `bin/linux-x86_64/softIoc` (R7.0.10-146-g8f5015b663d764ad75df), C
    /// emits no other sentence, and the `line <N>: '<yytext>'` frame under
    /// it already names the offending token — so an expectation set here
    /// would only cost a site script the sentence it greps for.
    fn syntax_error(&self) -> SubstitutionFault {
        SubstitutionFault {
            line: self.line(),
            message: Some("syntax error".into()),
            // At end of input C prints whatever `yytext` still points at,
            // which is a freed lex buffer. Measured over eleven files that
            // end mid-grammar, three runs each: the sentence and the line
            // number came out identical every time, and the `yytext` under
            // them was empty in six, the last matched token in one, and
            // uninitialised heap in four — different bytes on every run of
            // the same file. C's second line is therefore not a value to
            // reproduce; its first line is, and is the one a script
            // matches. The empty string is the stand-in for the rest.
            yytext: self.peek().map(tok_text).unwrap_or_default(),
        }
    }

    /// C `yyerror(NULL)`: the position frame with no message above it.
    fn err_unnamed(&self) -> SubstitutionFault {
        SubstitutionFault {
            line: self.line(),
            message: None,
            // Empty at end of input for the reason
            // [`Parser::syntax_error`] gives: C's `yytext` is a freed
            // buffer there.
            yytext: self.peek().map(tok_text).unwrap_or_default(),
        }
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).map(|(t, _)| t.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, want: &Tok) -> ParseResult<()> {
        match self.peek() {
            Some(got) if got == want => {
                self.pos += 1;
                Ok(())
            }
            _ => Err(self.syntax_error()),
        }
    }

    /// A `WORD` or `QUOTE`, where C's grammar takes either —
    /// `template_filename: DBFILE WORD | DBFILE QUOTE`
    /// (`dbLoadTemplate.y:117-137`).
    fn expect_str(&mut self) -> ParseResult<String> {
        Ok(self.expect_value()?.text)
    }

    /// A `WORD` where C's grammar takes ONLY a `WORD`: a `pattern_name`
    /// (`dbLoadTemplate.y:154`), the name half of a `variable_definition`
    /// (`:301`, `:312`), and the extraneous word of a deprecated row
    /// (`:197`, `:279`). Quoting one of those is a syntax error on a real
    /// IOC, so accepting it here would only move the failure to the site.
    fn expect_word(&mut self) -> ParseResult<String> {
        if let Some(Tok::Str { quote: Some(_), .. }) = self.peek() {
            return Err(self.syntax_error());
        }
        self.expect_str()
    }

    /// A `WORD` or `QUOTE` used as a substitution VALUE, where C's two
    /// grammar rules differ: the quotes go back on for the echo.
    fn expect_value(&mut self) -> ParseResult<RowValue> {
        match self.peek() {
            Some(Tok::Str { text, quote }) => {
                let value = RowValue {
                    text: text.clone(),
                    quoted: quote.is_some(),
                };
                self.pos += 1;
                Ok(value)
            }
            _ => Err(self.syntax_error()),
        }
    }

    /// `substitution_file: (global_definitions | template_substitutions)*`
    fn parse(&mut self) -> ParseResult<()> {
        while let Some(tok) = self.peek() {
            match tok {
                Tok::Global => self.parse_global()?,
                Tok::File => self.parse_file()?,
                _ => return Err(self.syntax_error()),
            }
        }
        Ok(())
    }

    /// `global { var=val, ... }` — accumulates into `self.globals`.
    fn parse_global(&mut self) -> ParseResult<()> {
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
    fn parse_file(&mut self) -> ParseResult<()> {
        let entry_line = self.line();
        self.expect(&Tok::File)?;
        let filename = self.expect_str()?;
        let loads_before = self.load_count;
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
        if self.load_count == loads_before {
            tracing::warn!(
                file = %filename,
                line = entry_line,
                "substitutions entry produced no template loads"
            );
        }
        Ok(())
    }

    /// `pattern { names } { row } { row } ...`
    fn parse_pattern_block(&mut self, filename: &str) -> ParseResult<()> {
        self.expect(&Tok::Pattern)?;
        self.expect(&Tok::OBrace)?;
        let mut names: Vec<String> = Vec::new();
        while self.peek() != Some(&Tok::CBrace) {
            match self.peek() {
                Some(Tok::Comma) => {
                    self.next();
                }
                Some(Tok::Str { .. }) => {
                    // C `pattern_name` (`dbLoadTemplate.y:154-171`): a name
                    // past the ceiling is reported and DROPPED — the guard
                    // leaves `var_count` alone and `yyerror(NULL)` does not
                    // abort, so each further name reports again and the row
                    // binds only the names that fit.
                    let ceiling = db_template_max_vars();
                    if names.len() as i32 >= ceiling {
                        let fault = self.err_unnamed();
                        self.notice(format!(
                            "More than dbTemplateMaxVars = {ceiling} macro variables used"
                        ));
                        self.events
                            .push((self.pos, SubstitutionEvent::Fault(fault)));
                        self.next();
                        continue;
                    }
                    names.push(self.expect_word()?)
                }
                _ => return Err(self.syntax_error()),
            }
        }
        self.expect(&Tok::CBrace)?;

        // pattern_definitions: each `{ row }` (or `global {}`).
        while let Some(tok) = self.peek() {
            match tok {
                Tok::Global => self.parse_global()?,
                Tok::OBrace => {
                    let row = self.parse_pattern_row()?;
                    let macros = self.pattern_macros(&names, &row);
                    self.emit_load(filename, macros);
                }
                // Deprecated `WORD { row }` form — C warns, drops the
                // extraneous leading word and loads the row anyway.
                Tok::Str { .. } => {
                    let extraneous = self.expect_word()?;
                    let row = self.parse_pattern_row()?;
                    // C reports the surplus values from inside the row and
                    // the deprecation from the row's action, in that order.
                    let macros = self.pattern_macros(&names, &row);
                    self.deprecated_row_notice(&extraneous);
                    self.emit_load(filename, macros);
                }
                Tok::CBrace => break,
                _ => return Err(self.syntax_error()),
            }
        }
        Ok(())
    }

    /// `{ "v1", "v2", ... }` — a positional value row. Each value
    /// carries the line it was read from, because a surplus one is
    /// reported by line number.
    fn parse_pattern_row(&mut self) -> ParseResult<Vec<(RowValue, usize)>> {
        self.expect(&Tok::OBrace)?;
        let mut values: Vec<(RowValue, usize)> = Vec::new();
        while self.peek() != Some(&Tok::CBrace) {
            match self.peek() {
                Some(Tok::Comma) => {
                    self.next();
                }
                Some(Tok::Str { .. }) => {
                    let line = self.line();
                    values.push((self.expect_value()?, line));
                }
                _ => return Err(self.syntax_error()),
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
    fn pattern_macros(&mut self, names: &[String], row: &[(RowValue, usize)]) -> Vec<RowMacro> {
        // C reports the surplus from `pattern_value` as the value is
        // reduced (`dbLoadTemplate.y:233`, `:250`), so `line_num` is that
        // value's own line and not the row's closing brace: measured, a
        // row whose third value sits on line 3 and whose brace closes on
        // line 4 reports line 3.
        for (_, line) in row.iter().skip(names.len()) {
            self.notice(format!(
                "dbLoadTemplate: Too many values given, line {line}."
            ));
        }
        names
            .iter()
            .zip(row.iter())
            .map(|(name, (value, _))| RowMacro {
                name: name.clone(),
                value: value.text.clone(),
                quoted: value.quoted,
            })
            .collect()
    }

    /// `file "x" { { var=val,... } { ... } ... }` — named rows.
    fn parse_variable_substitutions(&mut self, filename: &str) -> ParseResult<()> {
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
                Tok::Str { .. } => {
                    let extraneous = self.expect_word()?;
                    self.expect(&Tok::OBrace)?;
                    let defs = self.parse_variable_definitions()?;
                    self.expect(&Tok::CBrace)?;
                    self.deprecated_row_notice(&extraneous);
                    self.emit_load(filename, defs);
                }
                Tok::CBrace => break,
                _ => return Err(self.syntax_error()),
            }
        }
        Ok(())
    }

    /// `var=val, var2=val2, ...` — zero or more `WORD = WORD|QUOTE`
    /// pairs separated/terminated by optional commas.
    fn parse_variable_definitions(&mut self) -> ParseResult<Vec<RowMacro>> {
        let mut defs: Vec<RowMacro> = Vec::new();
        while self.peek() != Some(&Tok::CBrace) {
            match self.peek() {
                Some(Tok::Comma) => {
                    self.next();
                }
                Some(Tok::Str { .. }) => {
                    let name = self.expect_word()?;
                    self.expect(&Tok::Equals)?;
                    let value = self.expect_value()?;
                    defs.push(RowMacro {
                        name,
                        value: value.text,
                        quoted: value.quoted,
                    });
                }
                _ => return Err(self.syntax_error()),
            }
        }
        Ok(defs)
    }

    /// Record one resolved template load: `globals + row` macros. The
    /// token position is where C's grammar action fires — right after the
    /// row's closing brace, with no lookahead read yet.
    fn emit_load(&mut self, filename: &str, row: Vec<RowMacro>) {
        let mut macros = self.globals.clone();
        macros.extend(row);
        self.load_count += 1;
        self.events.push((
            self.pos,
            SubstitutionEvent::Load(TemplateLoad {
                file: filename.to_string(),
                macros,
                line: self.last_line(),
            }),
        ));
    }

    /// One of C's plain `fprintf(stderr, ...)` notices, in stream order.
    fn notice(&mut self, text: String) {
        self.events
            .push((self.pos, SubstitutionEvent::Notice(text)));
    }

    /// The line of the token just consumed — where C's lexer stands while
    /// a row's grammar action runs, the row having ended in `C_BRACE`.
    fn last_line(&self) -> usize {
        self.toks[self.pos - 1].1
    }

    /// C's deprecated `WORD { … }` row (`dbLoadTemplate.y:199-203`,
    /// `:281-285`), which it accepts and reports. The line is `line_num`
    /// at the row's action, so — despite what the sentence says about the
    /// string — it is the row's closing brace and not the word's own line:
    /// measured, a word on line 3 before a row that closes on line 4 is
    /// reported as line 4.
    fn deprecated_row_notice(&mut self, word: &str) {
        let line = self.last_line();
        self.notice(format!(
            "dbLoadTemplate: Substitution file uses deprecated syntax.\n    \
             the string '{word}' on line {line} that comes just before the\n    \
             '{{' character is extraneous and should be removed."
        ));
    }
}

/// Parse a `.substitutions` file body into the sequence of loads and
/// faults C would produce from it, in C's order. Does not touch the
/// filesystem — the caller owns the database and runs each load.
///
/// Never fails as a whole: C's lexer recovers from a stray character
/// (`dbLoadTemplate_lex.l:47-56`) and its parser has already run the
/// grammar action — the `dbLoadRecords` — of every row it reduced before
/// stopping, so a broken file still loads the rows it got through.
pub fn parse_substitutions(input: &str) -> Substitutions {
    let Lexed {
        toks,
        faults,
        final_line,
    } = lex(input);
    let mut parser = Parser::new(toks, final_line);
    let stopped = parser.parse().err();

    // C reads no further than the token it failed on, so a stray
    // character past that point is never reached and never reported.
    let last_read = parser.pos;
    let mut faults = faults
        .into_iter()
        .filter(|(at, _)| *at <= last_read)
        .peekable();

    let mut events = Vec::new();
    for (at, event) in std::mem::take(&mut parser.events) {
        while faults.peek().is_some_and(|(fault_at, _)| *fault_at < at) {
            events.push(SubstitutionEvent::Fault(faults.next().unwrap().1));
        }
        events.push(event);
    }
    events.extend(faults.map(|(_, fault)| SubstitutionEvent::Fault(fault)));

    Substitutions { events, stopped }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use super::super::include::{DbLoadConfig, parse_db_opened_with_breaktables};
    use super::*;

    /// A row's macros as plain `name=value`, for the tests that do not
    /// care which of them arrived quoted.
    fn pairs(macros: &[RowMacro]) -> Vec<(String, String)> {
        macros
            .iter()
            .map(|m| (m.name.clone(), m.value.clone()))
            .collect()
    }

    /// The rows of a parse, faults and all.
    fn loads_of_events(subs: &Substitutions) -> Vec<&TemplateLoad> {
        subs.events
            .iter()
            .filter_map(|ev| match ev {
                SubstitutionEvent::Load(load) => Some(load),
                _ => None,
            })
            .collect()
    }

    /// The rows of a file that parses cleanly.
    fn loads_of(src: &str) -> Vec<TemplateLoad> {
        let subs = parse_substitutions(src);
        assert_eq!(subs.stopped, None, "unexpected parse fault");
        subs.events
            .into_iter()
            .map(|ev| match ev {
                SubstitutionEvent::Load(load) => load,
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    /// Drive a `.substitutions` file the way `dbLoadTemplate` does:
    /// resolve, parse and collect one row at a time. The command
    /// installs each row's records here; these tests only need the
    /// records, so they concatenate them instead.
    fn load_rows(
        subs: &Path,
        macros: &HashMap<String, String>,
        config: &DbLoadConfig,
    ) -> Vec<super::super::DbRecordDef> {
        let text = std::fs::read_to_string(subs).unwrap();
        loads_of(&text)
            .into_iter()
            .flat_map(|load| {
                // macLib last-definition-wins: the caller's macros go in
                // first so a row entry of the same name overrides them.
                let mut merged = macros.clone();
                merged.extend(pairs(&load.macros));
                let template =
                    super::super::include::db_open_file_located(&load.file, &config.include_paths)
                        .unwrap();
                parse_db_opened_with_breaktables(&template, &merged, config)
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
        let loads = loads_of(src);
        assert_eq!(loads.len(), 2);
        assert_eq!(loads[0].file, "rec.db");
        assert_eq!(
            pairs(&loads[0].macros),
            vec![("P".into(), "IOC:".into()), ("N".into(), "1".into())]
        );
        assert_eq!(
            pairs(&loads[1].macros),
            vec![("P".into(), "IOC:".into()), ("N".into(), "2".into())]
        );
    }

    #[test]
    fn parse_pattern_with_comma_separated_names() {
        // pattern names may be comma-separated or whitespace-separated.
        let src = r#"file "x.db" { pattern {A,B,C} {"1","2","3"} }"#;
        let loads = loads_of(src);
        assert_eq!(loads.len(), 1);
        assert_eq!(
            pairs(&loads[0].macros),
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
        let loads = loads_of(src);
        assert_eq!(loads.len(), 2);
        assert_eq!(
            pairs(&loads[0].macros),
            vec![("A".into(), "1".into()), ("B".into(), "2".into())]
        );
        assert_eq!(
            pairs(&loads[1].macros),
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
        let loads = loads_of(src);
        assert_eq!(loads.len(), 2);
        // Globals prepended to every row.
        assert_eq!(
            pairs(&loads[0].macros),
            vec![("G".into(), "gval".into()), ("N".into(), "1".into())]
        );
        assert_eq!(
            pairs(&loads[1].macros),
            vec![("G".into(), "gval".into()), ("N".into(), "2".into())]
        );
    }

    #[test]
    fn parse_quoted_filename() {
        let src = r#"file "path/to/rec.db" { { A=1 } }"#;
        let loads = loads_of(src);
        assert_eq!(loads[0].file, "path/to/rec.db");
    }

    #[test]
    fn parse_bare_filename() {
        let src = r#"file rec.db { { A=1 } }"#;
        let loads = loads_of(src);
        assert_eq!(loads[0].file, "rec.db");
    }

    #[test]
    fn parse_empty_file_body() {
        let src = r#"file "rec.db" { }"#;
        let loads = loads_of(src);
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
        parse_substitutions(src);
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

    fn notices_of(subs: &Substitutions) -> Vec<&str> {
        subs.events
            .iter()
            .filter_map(|ev| match ev {
                SubstitutionEvent::Notice(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
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
        let subs = parse_substitutions(
            "file \"rec.db\" {\n  pattern { A, B }\n  { \"1\", \"2\", \"3\" }\n}",
        );
        let out = notices_of(&subs).join("\n");
        assert!(
            out.contains("Too many values given, line 3."),
            "the surplus value must be reported with its line, got: {out:?}"
        );
        assert_eq!(
            out.matches("Too many values given").count(),
            1,
            "one report per surplus value, got: {out:?}"
        );

        let loads = loads_of_events(&subs);
        assert_eq!(
            pairs(&loads[0].macros),
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2".to_string())
            ],
            "the surplus value is dropped, the rest still bind"
        );

        let short = parse_substitutions("file \"rec.db\" {\n  pattern { A, B }\n  { \"1\" }\n}");
        assert!(
            notices_of(&short).is_empty(),
            "a short row leaves B unbound without a diagnostic, as in C"
        );
    }

    #[test]
    fn parse_empty_pattern_row() {
        // `pattern {N} {}` — empty row is a valid load with no row
        // macros (C `pattern_definition: O_BRACE C_BRACE`).
        let src = r#"file "rec.db" { pattern {N} {} }"#;
        let loads = loads_of(src);
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
        let loads = loads_of(src);
        assert_eq!(loads.len(), 1);
    }

    #[test]
    fn parse_deprecated_word_prefix_row() {
        // The deprecated `WORD { ... }` row form is accepted (C warns
        // then ignores the extraneous word).
        let src = r#"file "rec.db" { pattern {N} extra {"1"} }"#;
        let subs = parse_substitutions(src);
        let loads = loads_of_events(&subs);
        assert_eq!(loads.len(), 1);
        assert_eq!(pairs(&loads[0].macros), vec![("N".into(), "1".into())]);
        assert_eq!(
            notices_of(&subs),
            vec![
                "dbLoadTemplate: Substitution file uses deprecated syntax.\n    \
                 the string 'extra' on line 1 that comes just before the\n    \
                 '{' character is extraneous and should be removed."
            ]
        );
    }

    #[test]
    fn parse_multiple_files() {
        let src = r#"
file "a.db" { { X=1 } }
file "b.db" { { Y=2 } }
"#;
        let loads = loads_of(src);
        assert_eq!(loads.len(), 2);
        assert_eq!(loads[0].file, "a.db");
        assert_eq!(loads[1].file, "b.db");
    }

    fn faults_of(subs: &Substitutions) -> Vec<&SubstitutionFault> {
        subs.events
            .iter()
            .filter_map(|ev| match ev {
                SubstitutionEvent::Fault(f) => Some(f),
                _ => None,
            })
            .collect()
    }

    /// An unterminated string is not a distinct error in C: the QUOTE
    /// pattern cannot match, so the quote falls to the `.` rule and the
    /// rest of the line lexes as barewords. Measured on softIoc, this
    /// file reports one bad character and still loads `N=oops`.
    #[test]
    fn an_unterminated_string_degrades_to_one_bad_character() {
        let src = "file \"rec.db\" { { N=\"oops\nB=1 } }";
        let subs = parse_substitutions(src);
        assert_eq!(subs.stopped, None);
        assert_eq!(
            faults_of(&subs),
            vec![&SubstitutionFault {
                line: 1,
                message: Some("invalid character '\"'".into()),
                yytext: "\"".into(),
            }]
        );
        let loads = loads_of_events(&subs);
        assert_eq!(loads.len(), 1);
        // Both definitions survive: with the quote gone, `oops` and
        // `B=1` are ordinary tokens and the row reads `{ N=oops B=1 }`.
        assert_eq!(
            pairs(&loads[0].macros),
            vec![("N".into(), "oops".into()), ("B".into(), "1".into())]
        );
    }

    /// C `{escape}` is `{backslash}.` and flex `.` never matches a
    /// newline (`dstringchar` is `[^"\n\\]`), so a backslash immediately
    /// before a newline is NOT an escape — the string cannot continue
    /// across the line. Both quotes therefore fall to the `.` rule, and
    /// the `B` left without a value ends the parse. Measured on softIoc.
    #[test]
    fn a_backslash_before_a_newline_does_not_continue_the_string() {
        let src = "file \"rec.db\" { { N=\"oops\\\nB\" } }";
        let subs = parse_substitutions(src);
        assert_eq!(
            faults_of(&subs)
                .iter()
                .map(|f| (f.line, f.message.clone().unwrap_or_default()))
                .collect::<Vec<_>>(),
            vec![
                (1, "invalid character '\"'".to_string()),
                (2, "invalid character '\"'".to_string()),
            ]
        );
        assert!(loads_of_events(&subs).is_empty());
        let stopped = subs
            .stopped
            .as_ref()
            .expect("the dangling name must stop the parse");
        assert_eq!((stopped.line, stopped.yytext.as_str()), (2, "}"));
    }

    #[test]
    fn parse_rejects_missing_keyword() {
        let src = r#"{ A=1 }"#;
        let stopped = parse_substitutions(src).stopped.expect("must stop");
        assert_eq!((stopped.line, stopped.yytext.as_str()), (1, "{"));
    }

    #[test]
    fn template_loads_drive_one_dbloadrecords_per_row() {
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
        assert_eq!(recs[0].fields[0].value, "1");
        assert_eq!(recs[1].name, "IOC:2");
        assert_eq!(recs[1].fields[0].value, "2");
    }

    /// I-R3-3: a template that also exists next to the `.substitutions`
    /// file must still be taken from the path list. `dbLoadTemplate`
    /// (`dbLoadTemplate.y:51`) hands each `file` entry to
    /// `dbLoadRecords`, whose `dbOpenFile` never looks at the
    /// substitutions file's own directory.
    #[test]
    fn template_loads_take_templates_from_the_path_list() {
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
    fn template_loads_env_expand_the_template_name() {
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
        unsafe { std::env::set_var(key, super::super::macro_safe_path(tpl_dir.path())) };
        let recs = load_rows(&subs, &HashMap::new(), &DbLoadConfig::default());
        // SAFETY: same serial group.
        unsafe { std::env::remove_var(key) };

        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "FROM_ENV");
    }

    #[test]
    fn template_loads_let_the_row_override_caller_macros() {
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
