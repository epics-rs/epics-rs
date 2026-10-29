//! The lexer (C `snl.re`, a re2c specification).
//!
//! Each state below is one re2c block of snl.re and picks what re2c
//! picks: the longest match, and among equally long matches the earliest
//! rule. As in C, the input gets a `'\n'` sentinel appended.
//!
//! Token text ends at the first NUL byte, as the C strings do, and is
//! decoded as UTF-8 with invalid bytes replaced.

use std::sync::Arc;

use crate::snc::driver::Snc;
use crate::snc::types::{Tok, Token};

const KEYWORDS: &[(&str, Tok)] = &[
    ("assign", Tok::Assign),
    ("break", Tok::Break),
    ("char", Tok::Char),
    ("const", Tok::Const),
    ("continue", Tok::Continue),
    ("double", Tok::Double),
    ("else", Tok::Else),
    ("entry", Tok::Entry),
    ("enum", Tok::Enum),
    ("evflag", Tok::Evflag),
    ("exit", Tok::Exit),
    ("float", Tok::Float),
    ("for", Tok::For),
    ("foreign", Tok::Foreign),
    ("if", Tok::If),
    ("int", Tok::Int),
    ("long", Tok::Long),
    ("monitor", Tok::Monitor),
    ("option", Tok::Option),
    ("program", Tok::Program),
    ("return", Tok::Return),
    ("short", Tok::Short),
    ("sizeof", Tok::Sizeof),
    ("ss", Tok::Ss),
    ("state", Tok::State),
    ("string", Tok::String),
    ("struct", Tok::Struct),
    ("syncQ", Tok::Syncq),
    ("syncq", Tok::Syncq),
    ("sync", Tok::Sync),
    ("to", Tok::To),
    ("typename", Tok::Typename),
    ("union", Tok::Union),
    ("unsigned", Tok::Unsigned),
    ("void", Tok::Void),
    ("when", Tok::When),
    ("while", Tok::While),
    ("int8_t", Tok::Int8t),
    ("uint8_t", Tok::Uint8t),
    ("int16_t", Tok::Int16t),
    ("uint16_t", Tok::Uint16t),
    ("int32_t", Tok::Int32t),
    ("uint32_t", Tok::Uint32t),
];

/// Longest first, so the first match is re2c's.
const OPERATORS: &[(&str, Tok)] = &[
    (">>=", Tok::Rsheq),
    ("<<=", Tok::Lsheq),
    ("+=", Tok::Addeq),
    ("-=", Tok::Subeq),
    ("*=", Tok::Muleq),
    ("/=", Tok::Diveq),
    ("%=", Tok::Modeq),
    ("&=", Tok::Andeq),
    ("^=", Tok::Xoreq),
    ("|=", Tok::Oreq),
    (">>", Tok::Rshift),
    ("<<", Tok::Lshift),
    ("++", Tok::Incr),
    ("--", Tok::Decr),
    ("->", Tok::Pointer),
    ("&&", Tok::Andand),
    ("||", Tok::Oror),
    ("<=", Tok::Le),
    (">=", Tok::Ge),
    ("==", Tok::Eq),
    ("!=", Tok::Ne),
    (";", Tok::Semicolon),
    ("{", Tok::Lbrace),
    ("}", Tok::Rbrace),
    (",", Tok::Comma),
    (":", Tok::Colon),
    ("=", Tok::Equal),
    ("(", Tok::Lparen),
    (")", Tok::Rparen),
    ("[", Tok::Lbracket),
    ("]", Tok::Rbracket),
    (".", Tok::Period),
    ("&", Tok::Ampersand),
    ("!", Tok::Not),
    ("~", Tok::Tilde),
    ("-", Tok::Sub),
    ("+", Tok::Add),
    ("*", Tok::Asterisk),
    ("/", Tok::Slash),
    ("%", Tok::Mod),
    ("<", Tok::Lt),
    (">", Tok::Gt),
    ("^", Tok::Caret),
    ("|", Tok::Vbar),
    ("?", Tok::Question),
];

fn is_spc(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

fn is_let(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_oct(c: u8) -> bool {
    matches!(c, b'0'..=b'7')
}

/// Token text from scanned bytes.
fn text(bytes: &[u8]) -> Arc<str> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    Arc::from(String::from_utf8_lossy(&bytes[..end]))
}

/// The re2c blocks of snl.re, by label.
enum St {
    Snl,
    StringConst,
    StringCat,
    LineMarker,
    LineMarkerStr,
    LineMarkerSkip,
    Comment,
    CCode,
    CCodeLine,
}

/// C `Scanner`.
pub(crate) struct Scanner {
    /// input and sentinel
    buf: Vec<u8>,
    /// saved scan position between calls to scan()
    cur: usize,
    /// source file name
    file: Option<Arc<str>>,
    line: i32,
}

impl Scanner {
    pub fn new(input: &[u8], src_file: &str) -> Self {
        let mut buf = Vec::with_capacity(input.len() + 1);
        buf.extend_from_slice(input);
        buf.push(b'\n');
        Scanner {
            buf,
            cur: 0,
            file: Some(Arc::from(src_file)),
            line: 1,
        }
    }

    fn at(&self, i: usize) -> Option<u8> {
        self.buf.get(i).copied()
    }

    /// One past the sentinel (C `s->eof`).
    fn eof(&self) -> usize {
        self.buf.len()
    }

    /// The end of the run of bytes from `i` that satisfy `f`.
    fn run(&self, mut i: usize, f: impl Fn(u8) -> bool) -> usize {
        while self.at(i).is_some_and(&f) {
            i += 1;
        }
        i
    }

    /// The end of `.*` from `i`.
    fn rest_of_line(&self, i: usize) -> usize {
        self.run(i, |c| c != b'\n')
    }

    /// `LINE = SPC* "#" (SPC* "line")? SPC+`
    fn line_end(&self, i: usize) -> Option<usize> {
        let hash = self.run(i, is_spc);
        if self.at(hash) != Some(b'#') {
            return None;
        }
        let after = hash + 1;
        let word = self.run(after, is_spc);
        if self.buf[word..].starts_with(b"line") {
            let end = self.run(word + 4, is_spc);
            if end > word + 4 {
                return Some(end);
            }
        }
        (word > after).then_some(word)
    }

    /// `ESC = [\\] ([abfnrtv?'"\\] | "x" HEX+ | OCT+)` at `i`.
    fn esc_end(&self, i: usize) -> Option<usize> {
        if self.at(i) != Some(b'\\') {
            return None;
        }
        match self.at(i + 1)? {
            b'a' | b'b' | b'f' | b'n' | b'r' | b't' | b'v' | b'?' | b'\'' | b'"' | b'\\' => {
                Some(i + 2)
            }
            b'x' => {
                let end = self.run(i + 2, |c| c.is_ascii_hexdigit());
                (end > i + 2).then_some(end)
            }
            c if is_oct(c) => Some(self.run(i + 1, is_oct)),
            _ => None,
        }
    }

    /// `[q] (ESC|[^\n\\q])* [q]` at `i`.
    fn quoted_end(&self, i: usize, q: u8) -> Option<usize> {
        let mut j = i + 1;
        loop {
            match self.at(j)? {
                c if c == q => return Some(j + 1),
                b'\n' => return None,
                b'\\' => j = self.esc_end(j)?,
                _ => j += 1,
            }
        }
    }

    /// The INTCON rule at `i`.
    fn intcon_end(&self, i: usize) -> Option<usize> {
        let is = |j| self.run(j, |c| matches!(c, b'u' | b'U' | b'l' | b'L'));
        let mut best = None;
        if self.at(i) == Some(b'0') {
            if matches!(self.at(i + 1), Some(b'x' | b'X')) {
                let hex = self.run(i + 2, |c| c.is_ascii_hexdigit());
                if hex > i + 2 {
                    best = best.max(Some(is(hex)));
                }
            }
            let oct = self.run(i + 1, is_oct);
            if oct > i + 1 {
                best = best.max(Some(is(oct)));
            }
        }
        let dec = self.run(i, |c| c.is_ascii_digit());
        if dec > i {
            best = best.max(Some(is(dec)));
        }
        if self.at(i) == Some(b'\'') {
            best = best.max(self.quoted_end(i, b'\''));
        }
        best
    }

    /// The FPCON rule at `i`.
    fn fpcon_end(&self, i: usize) -> Option<usize> {
        let exp = |j: usize| {
            if !matches!(self.at(j), Some(b'e' | b'E')) {
                return None;
            }
            let k = j + 1 + usize::from(matches!(self.at(j + 1), Some(b'+' | b'-')));
            let end = self.run(k, |c| c.is_ascii_digit());
            (end > k).then_some(end)
        };
        let fs = |j: usize| j + usize::from(matches!(self.at(j), Some(b'f' | b'F' | b'l' | b'L')));
        let mut best = None;
        let dec = self.run(i, |c| c.is_ascii_digit());
        // DEC+ EXP FS?
        if dec > i {
            best = best.max(exp(dec).map(fs));
        }
        if self.at(dec) == Some(b'.') {
            let frac = self.run(dec + 1, |c| c.is_ascii_digit());
            // DEC* "." DEC+ EXP? FS?, DEC+ "." DEC* EXP? FS?
            if frac > dec + 1 || dec > i {
                best = best.max(Some(fs(exp(frac).unwrap_or(frac))));
            }
        }
        best
    }

    /// C `scan_report`.
    fn scan_report(&self, snc: &mut Snc, args: std::fmt::Arguments<'_>) {
        snc.report_loc(self.file.as_deref(), self.line);
        snc.report(format_args!("lexical error: "));
        snc.report(args);
    }

    /// C `scan`: the next token.
    pub fn scan(&mut self, snc: &mut Snc) -> Token {
        let mut cursor = self.cur;
        let mut in_c_code = false;
        // start of the current line marker part
        let mut line_marker_part = 0;
        let mut tok = cursor;
        let mut end = cursor;
        // contents of a string constant, its literals concatenated
        let mut string = Vec::new();
        let mut t_line = self.line;
        let mut t_file = self.file.clone();
        let mut st = St::Snl;
        let (symbol, str): (Tok, Arc<str>) = loop {
            match st {
                St::Snl => {
                    if in_c_code {
                        st = St::CCode;
                        continue;
                    }
                    t_line = self.line;
                    t_file = self.file.clone();
                    tok = cursor;
                    // Past the sentinel only after a line marker ate it; C
                    // reads beyond its buffer there.
                    let Some(c) = self.at(cursor) else {
                        break (Tok::Eoi, Arc::from(""));
                    };
                    if c == b'\n' {
                        cursor += 1;
                        if cursor == self.eof() {
                            break (Tok::Eoi, Arc::from(""));
                        }
                        self.line += 1;
                        continue;
                    }
                    if let Some(e) = self.line_end(cursor) {
                        cursor = e;
                        line_marker_part = cursor;
                        st = St::LineMarker;
                        continue;
                    }
                    let next = self.at(cursor + 1);
                    match (c, next) {
                        (b'"', _) => {
                            cursor += 1;
                            tok = cursor;
                            string.clear();
                            st = St::StringConst;
                            continue;
                        }
                        (b'/', Some(b'*')) => {
                            cursor += 2;
                            st = St::Comment;
                            continue;
                        }
                        (b'/', Some(b'/')) => {
                            cursor = self.rest_of_line(cursor + 2);
                            continue;
                        }
                        (b'%', Some(b'{')) => {
                            cursor += 2;
                            tok = cursor;
                            in_c_code = true;
                            st = St::CCode;
                            continue;
                        }
                        (b'%', Some(b'%')) => {
                            cursor = self.run(cursor + 2, is_spc);
                            tok = cursor;
                            end = cursor;
                            st = St::CCodeLine;
                            continue;
                        }
                        _ => {}
                    }
                    if is_let(c) {
                        cursor = self.run(cursor, |c| is_let(c) || c.is_ascii_digit());
                        let word = &self.buf[tok..cursor];
                        if let Some(&(kw, t)) =
                            KEYWORDS.iter().find(|(kw, _)| kw.as_bytes() == word)
                        {
                            break (t, Arc::from(kw));
                        }
                        if word.starts_with(b"seqg_") {
                            let word = text(word);
                            self.scan_report(
                                snc,
                                format_args!("identifier '{word}'starts with reserved prefix\n"),
                            );
                            break (Tok::Eoi, Arc::from(""));
                        }
                        break (Tok::Name, text(word));
                    }
                    let int = self.intcon_end(cursor);
                    let fp = self.fpcon_end(cursor);
                    if int.is_some() || fp.is_some() {
                        let (t, e) = if int >= fp {
                            (Tok::Intcon, int)
                        } else {
                            (Tok::Fpcon, fp)
                        };
                        cursor = e.expect("literal end");
                        break (t, text(&self.buf[tok..cursor]));
                    }
                    if let Some(&(op, t)) = OPERATORS
                        .iter()
                        .find(|(op, _)| self.buf[cursor..].starts_with(op.as_bytes()))
                    {
                        cursor += op.len();
                        break (t, Arc::from(op));
                    }
                    if matches!(c, b' ' | b'\t' | 0x0b | 0x0c) {
                        cursor = self.run(cursor, |c| matches!(c, b' ' | b'\t' | 0x0b | 0x0c));
                        continue;
                    }
                    cursor += 1;
                    self.scan_report(snc, format_args!("invalid character\n"));
                    break (Tok::Eoi, Arc::from(""));
                }
                St::StringConst => match self.at(cursor) {
                    Some(b'"') => {
                        cursor += 1;
                        string.extend_from_slice(&self.buf[tok..cursor - 1]);
                        st = St::StringCat;
                    }
                    Some(b'\n') | None => {
                        cursor += 1;
                        self.scan_report(
                            snc,
                            format_args!("invalid character in string constant\n"),
                        );
                        break (Tok::Eoi, Arc::from(""));
                    }
                    Some(b'\\') => match self.esc_end(cursor) {
                        Some(e) => cursor = e,
                        None => {
                            cursor += 1;
                            self.scan_report(
                                snc,
                                format_args!("invalid character in string constant\n"),
                            );
                            break (Tok::Eoi, Arc::from(""));
                        }
                    },
                    Some(_) => cursor += 1,
                },
                St::StringCat => match self.at(cursor) {
                    Some(c) if is_spc(c) => cursor = self.run(cursor, is_spc),
                    Some(b'\n') => {
                        cursor += 1;
                        if cursor == self.eof() {
                            cursor -= 1;
                            break (Tok::Strcon, text(&string));
                        }
                        self.line += 1;
                    }
                    Some(b'"') => {
                        cursor += 1;
                        tok = cursor;
                        st = St::StringConst;
                    }
                    _ => break (Tok::Strcon, text(&string)),
                },
                St::LineMarker => {
                    let digits = self.run(cursor, |c| c.is_ascii_digit());
                    if digits > cursor {
                        self.line = atoi(&self.buf[line_marker_part..digits]).wrapping_sub(1);
                        cursor = self.run(digits, is_spc);
                        line_marker_part = cursor;
                        st = St::LineMarkerStr;
                    } else {
                        cursor += 1;
                        st = St::LineMarkerSkip;
                    }
                }
                St::LineMarkerStr => {
                    match self.at(cursor) {
                        Some(b'"') => match self.quoted_end(cursor, b'"') {
                            Some(e) => {
                                cursor = e;
                                self.file = Some(text(&self.buf[line_marker_part + 1..cursor - 1]));
                            }
                            None => cursor += 1,
                        },
                        Some(b'\n') => {}
                        _ => cursor += 1,
                    }
                    st = St::LineMarkerSkip;
                }
                St::LineMarkerSkip => {
                    cursor = self.rest_of_line(cursor);
                    st = St::Snl;
                }
                St::Comment => match self.at(cursor) {
                    Some(b'*') if self.at(cursor + 1) == Some(b'/') => {
                        cursor += 2;
                        st = St::Snl;
                    }
                    Some(b'\n') => {
                        cursor += 1;
                        if cursor == self.eof() {
                            self.scan_report(snc, format_args!("at eof: unterminated comment\n"));
                            break (Tok::Eoi, Arc::from(""));
                        }
                        self.line += 1;
                    }
                    _ => cursor += 1,
                },
                St::CCode => {
                    if self.buf[cursor..].starts_with(b"}%") {
                        cursor += 2;
                        break (Tok::Ccode, text(&self.buf[tok..cursor - 2]));
                    }
                    if let Some(e) = self.line_end(cursor) {
                        cursor = e;
                        line_marker_part = cursor;
                        st = St::LineMarker;
                        continue;
                    }
                    match self.at(cursor) {
                        Some(b'\n') => {
                            cursor += 1;
                            if cursor == self.eof() {
                                self.scan_report(
                                    snc,
                                    format_args!("at eof: unterminated literal c-code section\n"),
                                );
                                break (Tok::Eoi, Arc::from(""));
                            }
                            self.line += 1;
                        }
                        Some(_) => cursor += 1,
                        // past the sentinel, as in St::Snl
                        None => break (Tok::Eoi, Arc::from("")),
                    }
                }
                St::CCodeLine => {
                    let spc = self.run(cursor, is_spc);
                    match self.at(spc) {
                        Some(b'\n') => {
                            cursor = spc + 1;
                            if cursor == self.eof() {
                                cursor -= 1;
                            }
                            self.line += 1;
                            if end > tok {
                                break (Tok::Ccode, text(&self.buf[tok..end]));
                            }
                            st = St::Snl;
                        }
                        _ => {
                            cursor += 1;
                            end = cursor;
                        }
                    }
                }
            }
        };
        self.cur = cursor;
        Token {
            symbol,
            str: Some(str),
            file: t_file,
            line: t_line,
        }
    }
}

/// C `atoi` on a run of decimal digits: `strtol` saturates, then the
/// conversion to `int` keeps the low 32 bits.
fn atoi(digits: &[u8]) -> i32 {
    let mut v: i64 = 0;
    for &d in digits {
        v = v.saturating_mul(10).saturating_add(i64::from(d - b'0'));
    }
    v as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(input: &str) -> (Vec<(Tok, String, i32, String)>, String) {
        let mut snc = Snc::capturing();
        let mut s = Scanner::new(input.as_bytes(), "in.st");
        let mut out = Vec::new();
        loop {
            let t = s.scan(&mut snc);
            out.push((
                t.symbol,
                t.text().to_string(),
                t.line,
                t.file.as_deref().unwrap_or("").to_string(),
            ));
            if t.symbol == Tok::Eoi {
                break;
            }
        }
        (out, String::from_utf8(snc.diagnostics().to_vec()).unwrap())
    }

    fn toks(input: &str) -> Vec<(Tok, String)> {
        scan_all(input)
            .0
            .into_iter()
            .map(|(t, s, _, _)| (t, s))
            .collect()
    }

    fn t(tok: Tok, s: &str) -> (Tok, String) {
        (tok, s.to_string())
    }

    #[test]
    fn keywords_operators_and_names() {
        assert_eq!(
            toks("syncQ syncq sync synch int8_t x>>=y->z"),
            [
                t(Tok::Syncq, "syncQ"),
                t(Tok::Syncq, "syncq"),
                t(Tok::Sync, "sync"),
                t(Tok::Name, "synch"),
                t(Tok::Int8t, "int8_t"),
                t(Tok::Name, "x"),
                t(Tok::Rsheq, ">>="),
                t(Tok::Name, "y"),
                t(Tok::Pointer, "->"),
                t(Tok::Name, "z"),
                t(Tok::Eoi, ""),
            ]
        );
    }

    #[test]
    fn number_literals_take_the_longest_match() {
        assert_eq!(
            toks("0x1fUL 08 1. .5 1.e5f 1e+ 'a' '' '\\x41' 0x . 1..2"),
            [
                t(Tok::Intcon, "0x1fUL"),
                t(Tok::Intcon, "08"),
                t(Tok::Fpcon, "1."),
                t(Tok::Fpcon, ".5"),
                t(Tok::Fpcon, "1.e5f"),
                t(Tok::Intcon, "1"),
                t(Tok::Name, "e"),
                t(Tok::Add, "+"),
                t(Tok::Intcon, "'a'"),
                t(Tok::Intcon, "''"),
                t(Tok::Intcon, "'\\x41'"),
                t(Tok::Intcon, "0"),
                t(Tok::Name, "x"),
                t(Tok::Period, "."),
                t(Tok::Fpcon, "1."),
                t(Tok::Fpcon, ".2"),
                t(Tok::Eoi, ""),
            ]
        );
    }

    #[test]
    fn strings_concatenate_across_lines() {
        let (out, diag) = scan_all("\"a\\n\"  \n \"b\" x \"c\"");
        assert_eq!(diag, "");
        assert_eq!(out[0], (Tok::Strcon, "a\\nb".into(), 1, "in.st".into()));
        assert_eq!(out[1], (Tok::Name, "x".into(), 2, "in.st".into()));
        assert_eq!(out[2], (Tok::Strcon, "c".into(), 2, "in.st".into()));
        assert_eq!(out[3].0, Tok::Eoi);
    }

    #[test]
    fn line_markers_set_line_and_file() {
        let (out, _) = scan_all("# 10 \"a.st\" 2\nx\n#line 20\ny\n  # 7 \"b\\\"c\"\nz");
        assert_eq!(out[0], (Tok::Name, "x".into(), 10, "a.st".into()));
        assert_eq!(out[1], (Tok::Name, "y".into(), 20, "a.st".into()));
        assert_eq!(out[2], (Tok::Name, "z".into(), 7, "b\\\"c".into()));
    }

    /// `# ` followed by a newline eats the newline and the next line.
    #[test]
    fn empty_line_marker_swallows_next_line() {
        let (out, _) = scan_all("# \nhidden\nx");
        assert_eq!(out[0], (Tok::Name, "x".into(), 2, "in.st".into()));
    }

    #[test]
    fn c_code_keeps_line_markers_in_text() {
        let (out, _) = scan_all("%{\n# 5 \"f.h\"\nint a;\n}%\nx\n%% y;  \nz");
        assert_eq!(
            out[0],
            (
                Tok::Ccode,
                "\n# 5 \"f.h\"\nint a;\n".into(),
                1,
                "in.st".into()
            )
        );
        assert_eq!(out[1], (Tok::Name, "x".into(), 7, "f.h".into()));
        assert_eq!(out[2], (Tok::Ccode, "y;".into(), 8, "f.h".into()));
        assert_eq!(out[3], (Tok::Name, "z".into(), 9, "f.h".into()));
    }

    #[test]
    fn lexical_errors() {
        let cases = [
            ("x $", "in.st:1: lexical error: invalid character\n"),
            (
                "\"ab\ncd\"",
                "in.st:1: lexical error: invalid character in string constant\n",
            ),
            (
                "\"a\\q\"",
                "in.st:1: lexical error: invalid character in string constant\n",
            ),
            (
                "/* x\n",
                "in.st:2: lexical error: at eof: unterminated comment\n",
            ),
            (
                "%{ x\n",
                "in.st:2: lexical error: at eof: unterminated literal c-code section\n",
            ),
            (
                "a seqg_x1 b",
                "in.st:1: lexical error: identifier 'seqg_x1'starts with reserved prefix\n",
            ),
            ("a\r\n", "in.st:1: lexical error: invalid character\n"),
        ];
        for (input, want) in cases {
            let (out, diag) = scan_all(input);
            assert_eq!(diag, want, "{input:?}");
            assert_eq!(out.last().unwrap().0, Tok::Eoi);
        }
    }

    #[test]
    fn line_number_overflow_wraps_as_c() {
        let (out, _) = scan_all("# 4294967296\nx\n# 99999999999999999999\ny");
        assert_eq!(out[0].2, 0);
        assert_eq!(out[1].2, -1);
    }
}
