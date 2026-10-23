//! Line splicing and tokenisation, after cpplib's `lex.cc` in C (gnu17)
//! mode.

use std::rc::Rc;

/// Whitespace or a comment precedes the token.
pub(super) const PREV_WHITE: u16 = 1;
/// First token of a logical line.
pub(super) const BOL: u16 = 1 << 1;
/// Spelled as a digraph (`%:`, `<:`, ...).
pub(super) const DIGRAPH: u16 = 1 << 2;
/// A `#param` in a macro body.
pub(super) const STRINGIFY_ARG: u16 = 1 << 3;
/// Followed by `##` in a macro body.
pub(super) const PASTE_LEFT: u16 = 1 << 4;
/// A macro name that must never expand again ("painted blue").
pub(super) const NO_EXPAND: u16 = 1 << 5;
/// The end-of-argument `Eof` that `collect_args` appends, which
/// `funlike_invocation_p` backs up over; a lexer `Eof` it does not.
pub(super) const END_ARG: u16 = 1 << 6;

/// Punctuator, by cpplib token type; a digraph shares its type and is
/// told apart by `DIGRAPH` and its spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum P {
    Eq,
    Not,
    Greater,
    Less,
    Plus,
    Minus,
    Mult,
    Div,
    Mod,
    And,
    Or,
    Xor,
    Rshift,
    Lshift,
    Compl,
    AndAnd,
    OrOr,
    Query,
    Colon,
    Comma,
    OpenParen,
    CloseParen,
    EqEq,
    NotEq,
    GreaterEq,
    LessEq,
    PlusEq,
    MinusEq,
    MultEq,
    DivEq,
    ModEq,
    AndEq,
    OrEq,
    XorEq,
    RshiftEq,
    LshiftEq,
    Hash,
    Paste,
    OpenSquare,
    CloseSquare,
    OpenBrace,
    CloseBrace,
    Semicolon,
    Ellipsis,
    PlusPlus,
    MinusMinus,
    Deref,
    Dot,
    Scope,
}

impl P {
    /// cpplib's `CPP_LAST_EQ`: the operators that paste with a following
    /// `=`.
    pub(super) fn pastes_with_eq(self) -> bool {
        matches!(
            self,
            P::Eq
                | P::Not
                | P::Greater
                | P::Less
                | P::Plus
                | P::Minus
                | P::Mult
                | P::Div
                | P::Mod
                | P::And
                | P::Or
                | P::Xor
                | P::Rshift
                | P::Lshift
        )
    }
}

/// Encoding prefix of a character constant or string literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Prefix {
    None,
    /// `L`
    Wide,
    /// `u`
    U16,
    /// `U`
    U32,
    /// `u8`
    Utf8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Name,
    Number,
    Char(Prefix),
    Str(Prefix),
    /// `<...>` in an `#include`.
    HeaderName,
    Punct(P),
    Other,
    /// Parameter `n` in a macro body.
    Param(u16),
    /// cpplib's `CPP_PADDING`. `src` is whether the token it stands for
    /// had `PREV_WHITE`; `None` is cpplib's null source.
    Padding {
        src: Option<bool>,
    },
    Eof,
}

/// A resolved location: the presumed file name (interned), presumed line
/// and byte column. Tokens of a macro expansion carry the location of the
/// outermost macro name, as cpplib's virtual locations resolve.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Loc {
    pub name: u32,
    pub line: u32,
    pub col: u32,
}

#[derive(Clone, Debug)]
pub(super) struct Tok {
    pub kind: Kind,
    pub flags: u16,
    pub text: Rc<[u8]>,
    pub loc: Loc,
}

impl Tok {
    pub(super) fn padding(src: Option<bool>) -> Tok {
        Tok {
            kind: Kind::Padding { src },
            flags: 0,
            text: Rc::from(&b""[..]),
            loc: Loc::default(),
        }
    }

    pub(super) fn eof(loc: Loc) -> Tok {
        Tok {
            kind: Kind::Eof,
            flags: 0,
            text: Rc::from(&b""[..]),
            loc,
        }
    }

    pub(super) fn prev_white(&self) -> bool {
        self.flags & PREV_WHITE != 0
    }

    pub(super) fn is(&self, p: P) -> bool {
        self.kind == Kind::Punct(p)
    }

    pub(super) fn is_padding(&self) -> bool {
        matches!(self.kind, Kind::Padding { .. })
    }

    pub(super) fn text_lossy(&self) -> String {
        String::from_utf8_lossy(&self.text).into_owned()
    }
}

/// cpplib's `cpp_avoid_paste`: whether printing `b` right after `a` could
/// lex differently.
pub(super) fn avoid_paste(a: &Tok, b: &Tok) -> bool {
    let c = match b.kind {
        Kind::Punct(_) => b.text.first().copied(),
        _ => None,
    };
    if let Kind::Punct(p) = a.kind
        && p.pastes_with_eq()
        && c == Some(b'=')
    {
        return true;
    }
    let is_c = |set: &[u8]| c.is_some_and(|c| set.contains(&c));
    match a.kind {
        Kind::Punct(P::Greater) => is_c(b">"),
        Kind::Punct(P::Less) => is_c(b"<%:"),
        Kind::Punct(P::Plus) => is_c(b"+"),
        Kind::Punct(P::Minus) => is_c(b"->"),
        Kind::Punct(P::Div) => is_c(b"/*"),
        Kind::Punct(P::Mod) => is_c(b":>"),
        Kind::Punct(P::And) => is_c(b"&"),
        Kind::Punct(P::Or) => is_c(b"|"),
        Kind::Punct(P::Colon) => is_c(b":>"),
        Kind::Punct(P::Deref) => is_c(b"*"),
        Kind::Punct(P::Dot) => is_c(b".%") || b.kind == Kind::Number,
        Kind::Punct(P::Hash) => is_c(b"#%"),
        Kind::Punct(P::LessEq) => is_c(b">"),
        Kind::Name => {
            (b.kind == Kind::Number && b.text.iter().all(|&c| is_idchar(c)))
                || matches!(
                    b.kind,
                    Kind::Name | Kind::Char(Prefix::None) | Kind::Str(Prefix::None)
                )
        }
        Kind::Number => {
            matches!(b.kind, Kind::Number | Kind::Name | Kind::Char(Prefix::None)) || is_c(b".+-")
        }
        Kind::Other => a.text.first() == Some(&b'\\') && b.kind == Kind::Name,
        _ => false,
    }
}

pub(super) fn is_idchar(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

const NUL_IN_LITERAL: &str = "null character(s) preserved in literal";

fn is_nvspace(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | 0x0b | 0x0c | 0)
}

/// A file after translation phases 1-2: `\r\n` and lone `\r` read as
/// `\n`, backslash-newlines removed, and a final newline supplied. Each
/// byte keeps its physical line and column, which is what cpplib reports
/// for a token, a splice included.
pub(super) struct Source {
    pub text: Vec<u8>,
    pub line: Vec<u32>,
    pub col: Vec<u32>,
    /// In text order.
    pub notes: Vec<Note>,
    /// The splice note cpplib warns "backslash-newline at end of file"
    /// at: the first on a last logical line that the end of the file, not
    /// a newline, ends.
    pub eof_splice: Option<usize>,
}

/// cpplib's `_cpp_line_note`: what phases 1-2 saw at byte `at` of the
/// text, diagnosed only once the lexer passes it.
#[derive(Clone, Copy)]
pub(super) struct Note {
    pub at: usize,
    /// Physical position of the backslash or of the trigraph.
    pub line: u32,
    pub col: u32,
    pub kind: NoteKind,
}

#[derive(Clone, Copy)]
pub(super) enum NoteKind {
    /// A backslash-newline, with blanks between the two when `space`.
    Splice { space: bool },
    /// `??c`, which gnu17 leaves as is; `escapes_newline` for a `??/`
    /// that blanks and a newline follow, the one trigraph a comment does
    /// not hide.
    Trigraph { c: u8, escapes_newline: bool },
}

/// A diagnostic the lexer raised at a physical line and column.
pub(super) struct LexDiag {
    pub line: u32,
    pub col: u32,
    pub error: bool,
    pub msg: String,
}

pub(super) fn clean(raw: &[u8]) -> Source {
    let raw = raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(raw);
    let mut s = Source {
        text: Vec::with_capacity(raw.len() + 1),
        line: Vec::with_capacity(raw.len() + 1),
        col: Vec::with_capacity(raw.len() + 1),
        notes: Vec::new(),
        eof_splice: None,
    };
    let (mut line, mut col) = (1u32, 1u32);
    // The first splice note of the current logical line.
    let mut first_splice = None;
    let mut i = 0;
    while i < raw.len() {
        let c = raw[i];
        if c == b'\\' {
            let mut j = i + 1;
            while j < raw.len() && is_nvspace(raw[j]) {
                j += 1;
            }
            // A backslash the end of the file follows stays: cpplib's
            // sentinel newline is never escaped.
            let newline = match raw.get(j) {
                Some(b'\n') => Some(1),
                Some(b'\r') => Some(if raw.get(j + 1) == Some(&b'\n') { 2 } else { 1 }),
                _ => None,
            };
            if let Some(n) = newline {
                first_splice.get_or_insert(s.notes.len());
                s.notes.push(Note {
                    at: s.text.len(),
                    line,
                    col,
                    kind: NoteKind::Splice { space: j > i + 1 },
                });
                i = j + n;
                line += 1;
                col = 1;
                continue;
            }
        }
        if c == b'?'
            && raw.get(i + 1) == Some(&b'?')
            && let Some(&t) = raw.get(i + 2).filter(|t| b"=(/)'<!>-".contains(t))
        {
            let mut j = i + 3;
            while j < raw.len() && is_nvspace(raw[j]) {
                j += 1;
            }
            s.notes.push(Note {
                at: s.text.len(),
                line,
                col,
                kind: NoteKind::Trigraph {
                    c: t,
                    escapes_newline: t == b'/' && matches!(raw.get(j), None | Some(b'\n' | b'\r')),
                },
            });
        }
        if c == b'\r' {
            s.text.push(b'\n');
            s.line.push(line);
            s.col.push(col);
            i += if raw.get(i + 1) == Some(&b'\n') { 2 } else { 1 };
            line += 1;
            col = 1;
            first_splice = None;
            continue;
        }
        s.text.push(c);
        s.line.push(line);
        s.col.push(col);
        if c == b'\n' {
            line += 1;
            col = 1;
            first_splice = None;
        } else {
            col += 1;
        }
        i += 1;
    }
    if s.text.last().is_some_and(|&c| c != b'\n') || first_splice.is_some() {
        s.eof_splice = first_splice;
        s.text.push(b'\n');
        s.line.push(line);
        s.col.push(col);
    }
    s
}

/// Lexer state the engine supplies per call.
pub(super) struct LexEnv {
    pub name: u32,
    pub delta: i64,
    pub angled_headers: bool,
    pub skipping: bool,
}

pub(super) struct Lexer {
    pub src: Source,
    pub pos: usize,
    /// The next token starts a fresh logical line.
    pub need_line: bool,
    /// The first note not yet diagnosed.
    note: usize,
}

impl Lexer {
    pub(super) fn new(src: Source) -> Self {
        Lexer {
            src,
            pos: 0,
            need_line: true,
            note: 0,
        }
    }

    pub(super) fn at_end(&self) -> bool {
        self.pos >= self.src.text.len()
    }

    fn at(&self, i: usize) -> u8 {
        self.src.text.get(i).copied().unwrap_or(b'\n')
    }

    /// Physical line and column of byte `i`.
    fn phys(&self, i: usize) -> (u32, u32) {
        let i = i.min(self.src.text.len().saturating_sub(1));
        match (self.src.line.get(i), self.src.col.get(i)) {
            (Some(&l), Some(&c)) => (l, c),
            _ => (1, 1),
        }
    }

    fn diag(&self, at: usize, error: bool, msg: impl Into<String>) -> LexDiag {
        let (line, col) = self.phys(at);
        LexDiag {
            line,
            col,
            error,
            msg: msg.into(),
        }
    }

    pub(super) fn loc(&self, i: usize, env: &LexEnv) -> Loc {
        let (line, col) = self.phys(i);
        Loc {
            name: env.name,
            line: (line as i64 + env.delta) as u32,
            col,
        }
    }

    /// Physical line of the newline that ended the last logical line.
    pub(super) fn last_newline_line(&self) -> u32 {
        match self.pos.checked_sub(1).and_then(|i| self.src.line.get(i)) {
            Some(&l) => l,
            None => 1,
        }
    }

    /// `_cpp_process_line_notes`: diagnose the notes the lexer has
    /// reached; inside a comment neither a spliced space nor most
    /// trigraphs are.
    fn line_notes(&mut self, in_comment: bool, diags: &mut Vec<LexDiag>) {
        while let Some(&n) = self.src.notes.get(self.note) {
            if n.at > self.pos {
                break;
            }
            let k = self.note;
            self.note += 1;
            let mut warn = |msg: &str| {
                diags.push(LexDiag {
                    line: n.line,
                    col: n.col,
                    error: false,
                    msg: msg.into(),
                })
            };
            match n.kind {
                NoteKind::Splice { space } => {
                    if space && !in_comment {
                        warn("backslash and newline separated by space");
                    }
                    if self.src.eof_splice == Some(k) {
                        warn("backslash-newline at end of file");
                    }
                }
                NoteKind::Trigraph { c, escapes_newline } => {
                    if !in_comment || escapes_newline {
                        warn(&format!(
                            "trigraph ??{} ignored, use -trigraphs to enable [-Wtrigraphs]",
                            c as char
                        ));
                    }
                }
            }
        }
    }

    /// Lex one token of the current logical line, or `None` once its
    /// newline is consumed (`need_line` is then set).
    pub(super) fn lex(
        &mut self,
        mut flags: u16,
        env: &LexEnv,
        diags: &mut Vec<LexDiag>,
    ) -> Option<Tok> {
        loop {
            self.line_notes(false, diags);
            let c = self.at(self.pos);
            if self.pos >= self.src.text.len() {
                self.need_line = true;
                return None;
            }
            if is_nvspace(c) {
                // skip_whitespace: one warning for a run, where it starts.
                let start = self.pos;
                while is_nvspace(self.at(self.pos)) {
                    self.pos += 1;
                }
                if self.src.text[start..self.pos].contains(&0) {
                    diags.push(self.diag(start, false, "null character(s) ignored"));
                }
                flags |= PREV_WHITE;
            } else if c == b'\n' {
                self.pos += 1;
                self.need_line = true;
                return None;
            } else if c == b'/' && self.at(self.pos + 1) == b'*' {
                let start = self.pos;
                self.pos += 2;
                loop {
                    if self.pos >= self.src.text.len() {
                        self.line_notes(true, diags);
                        diags.push(self.diag(start, true, "unterminated comment"));
                        self.need_line = true;
                        return None;
                    }
                    if self.src.text[self.pos] == b'*' && self.at(self.pos + 1) == b'/' {
                        self.pos += 2;
                        break;
                    }
                    self.pos += 1;
                }
                self.line_notes(true, diags);
                flags |= PREV_WHITE;
            } else if c == b'/' && self.at(self.pos + 1) == b'/' {
                while self.at(self.pos) != b'\n' {
                    self.pos += 1;
                }
                self.line_notes(true, diags);
                flags |= PREV_WHITE;
            } else {
                break;
            }
        }

        let start = self.pos;
        let c = self.src.text[start];
        self.pos += 1;
        let kind = match c {
            b'0'..=b'9' => {
                self.lex_number();
                Kind::Number
            }
            b'.' => {
                if self.at(self.pos).is_ascii_digit() {
                    self.lex_number();
                    Kind::Number
                } else if self.at(self.pos) == b'.' && self.at(self.pos + 1) == b'.' {
                    self.pos += 2;
                    Kind::Punct(P::Ellipsis)
                } else {
                    Kind::Punct(P::Dot)
                }
            }
            b'L' | b'u' | b'U' | b'R' => match self.literal_prefix(c) {
                Some((prefix, raw)) => {
                    if raw {
                        return Some(self.raw_string(start, flags, env, diags));
                    }
                    self.lex_string(start, prefix, env, diags)
                }
                None => {
                    self.lex_ident();
                    Kind::Name
                }
            },
            b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$' => {
                self.lex_ident();
                Kind::Name
            }
            b'\'' | b'"' => {
                self.pos = start;
                self.lex_string(start, Prefix::None, env, diags)
            }
            b'<' => {
                if env.angled_headers && self.header_name() {
                    if self.src.text[start..self.pos].contains(&0) {
                        diags.push(self.diag(start, false, NUL_IN_LITERAL));
                    }
                    Kind::HeaderName
                } else {
                    match self.at(self.pos) {
                        b'=' => self.punct(1, P::LessEq),
                        b'<' if self.at(self.pos + 1) == b'=' => self.punct(2, P::LshiftEq),
                        b'<' => self.punct(1, P::Lshift),
                        b':' => self.digraph(&mut flags, P::OpenSquare),
                        b'%' => self.digraph(&mut flags, P::OpenBrace),
                        _ => Kind::Punct(P::Less),
                    }
                }
            }
            b'>' => match self.at(self.pos) {
                b'=' => self.punct(1, P::GreaterEq),
                b'>' if self.at(self.pos + 1) == b'=' => self.punct(2, P::RshiftEq),
                b'>' => self.punct(1, P::Rshift),
                _ => Kind::Punct(P::Greater),
            },
            b'%' => match self.at(self.pos) {
                b'=' => self.punct(1, P::ModEq),
                b':' => {
                    if self.at(self.pos + 1) == b'%' && self.at(self.pos + 2) == b':' {
                        flags |= DIGRAPH;
                        self.punct(3, P::Paste)
                    } else {
                        self.digraph(&mut flags, P::Hash)
                    }
                }
                b'>' => self.digraph(&mut flags, P::CloseBrace),
                _ => Kind::Punct(P::Mod),
            },
            b'+' => match self.at(self.pos) {
                b'+' => self.punct(1, P::PlusPlus),
                b'=' => self.punct(1, P::PlusEq),
                _ => Kind::Punct(P::Plus),
            },
            b'-' => match self.at(self.pos) {
                b'>' => self.punct(1, P::Deref),
                b'-' => self.punct(1, P::MinusMinus),
                b'=' => self.punct(1, P::MinusEq),
                _ => Kind::Punct(P::Minus),
            },
            b'&' => match self.at(self.pos) {
                b'&' => self.punct(1, P::AndAnd),
                b'=' => self.punct(1, P::AndEq),
                _ => Kind::Punct(P::And),
            },
            b'|' => match self.at(self.pos) {
                b'|' => self.punct(1, P::OrOr),
                b'=' => self.punct(1, P::OrEq),
                _ => Kind::Punct(P::Or),
            },
            b':' => match self.at(self.pos) {
                b':' => self.punct(1, P::Scope),
                b'>' => self.digraph(&mut flags, P::CloseSquare),
                _ => Kind::Punct(P::Colon),
            },
            b'*' => match self.at(self.pos) {
                b'=' => self.punct(1, P::MultEq),
                _ => Kind::Punct(P::Mult),
            },
            b'=' => match self.at(self.pos) {
                b'=' => self.punct(1, P::EqEq),
                _ => Kind::Punct(P::Eq),
            },
            b'!' => match self.at(self.pos) {
                b'=' => self.punct(1, P::NotEq),
                _ => Kind::Punct(P::Not),
            },
            b'^' => match self.at(self.pos) {
                b'=' => self.punct(1, P::XorEq),
                _ => Kind::Punct(P::Xor),
            },
            b'#' => match self.at(self.pos) {
                b'#' => self.punct(1, P::Paste),
                _ => Kind::Punct(P::Hash),
            },
            b'/' => match self.at(self.pos) {
                b'=' => self.punct(1, P::DivEq),
                _ => Kind::Punct(P::Div),
            },
            b'?' => Kind::Punct(P::Query),
            b'~' => Kind::Punct(P::Compl),
            b',' => Kind::Punct(P::Comma),
            b'(' => Kind::Punct(P::OpenParen),
            b')' => Kind::Punct(P::CloseParen),
            b'[' => Kind::Punct(P::OpenSquare),
            b']' => Kind::Punct(P::CloseSquare),
            b'{' => Kind::Punct(P::OpenBrace),
            b'}' => Kind::Punct(P::CloseBrace),
            b';' => Kind::Punct(P::Semicolon),
            b'\\' => {
                if !env.skipping && matches!(self.at(self.pos), b'u' | b'U') {
                    diags.push(self.diag(
                        start,
                        true,
                        "universal character names are not supported",
                    ));
                }
                Kind::Other
            }
            0x80.. => {
                while (0x80..0xc0).contains(&self.at(self.pos)) {
                    self.pos += 1;
                }
                if !env.skipping {
                    diags.push(self.diag(
                        start,
                        true,
                        "non-ASCII character outside a literal or comment is not supported",
                    ));
                }
                Kind::Other
            }
            _ => Kind::Other,
        };
        Some(Tok {
            kind,
            flags,
            text: Rc::from(&self.src.text[start..self.pos]),
            loc: self.loc(start, env),
        })
    }

    fn punct(&mut self, extra: usize, p: P) -> Kind {
        self.pos += extra;
        Kind::Punct(p)
    }

    fn digraph(&mut self, flags: &mut u16, p: P) -> Kind {
        *flags |= DIGRAPH;
        self.punct(1, p)
    }

    fn lex_number(&mut self) {
        loop {
            let c = self.at(self.pos);
            if is_idchar(c)
                || c == b'.'
                || (matches!(c, b'+' | b'-')
                    && matches!(self.src.text[self.pos - 1], b'e' | b'E' | b'p' | b'P'))
            {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn lex_ident(&mut self) {
        while is_idchar(self.at(self.pos)) {
            self.pos += 1;
        }
    }

    /// After the first byte `c` of `L`/`u`/`U`/`R`: the prefix of the
    /// string or character literal that starts here, and whether it is
    /// raw, with `self.pos` left at the opening quote of one that is not.
    /// `u8'x'` is not a literal before C2X.
    fn literal_prefix(&mut self, c: u8) -> Option<(Prefix, bool)> {
        let n = self.at(self.pos);
        let n1 = self.at(self.pos + 1);
        let n2 = self.at(self.pos + 2);
        let prefix = match c {
            b'L' => Prefix::Wide,
            b'u' => Prefix::U16,
            b'U' => Prefix::U32,
            _ => Prefix::None,
        };
        if (n == b'\'' && c != b'R') || n == b'"' {
            return Some((prefix, c == b'R'));
        }
        if n == b'R' && c != b'R' && n1 == b'"' {
            return Some((prefix, true));
        }
        if n == b'8' && c == b'u' {
            if n1 == b'"' {
                self.pos += 1;
                return Some((Prefix::Utf8, false));
            }
            if n1 == b'R' && n2 == b'"' {
                return Some((Prefix::Utf8, true));
            }
        }
        None
    }

    /// Raw string literals are not supported: diagnose and swallow the
    /// rest of the line, so nothing inside is taken for a directive.
    fn raw_string(
        &mut self,
        start: usize,
        flags: u16,
        env: &LexEnv,
        diags: &mut Vec<LexDiag>,
    ) -> Tok {
        diags.push(self.diag(start, true, "raw string literals are not supported"));
        while self.at(self.pos) != b'\n' {
            self.pos += 1;
        }
        Tok {
            kind: Kind::Other,
            flags,
            text: Rc::from(&self.src.text[start..self.pos]),
            loc: self.loc(start, env),
        }
    }

    /// `lex_string`: `self.pos` is at the opening quote.
    fn lex_string(
        &mut self,
        start: usize,
        prefix: Prefix,
        env: &LexEnv,
        diags: &mut Vec<LexDiag>,
    ) -> Kind {
        let term = self.src.text[self.pos];
        self.pos += 1;
        let mut saw_nul = false;
        let terminated = loop {
            let c = self.at(self.pos);
            self.pos += 1;
            if c == b'\\' && !env.angled_headers && self.at(self.pos) != b'\n' {
                self.pos += 1;
            } else if c == term {
                break true;
            } else if c == b'\n' {
                self.pos -= 1;
                break false;
            } else if c == 0 {
                saw_nul = true;
            }
        };
        if saw_nul && !env.skipping {
            diags.push(self.diag(start, false, NUL_IN_LITERAL));
        }
        if !terminated {
            diags.push(self.diag(
                start,
                false,
                format!("missing terminating {} character", term as char),
            ));
            return Kind::Other;
        }
        if term == b'"' {
            Kind::Str(prefix)
        } else {
            Kind::Char(prefix)
        }
    }

    /// `<...>` in an include directive; on a newline before `>` the `<`
    /// is an ordinary token.
    fn header_name(&mut self) -> bool {
        let mut i = self.pos;
        loop {
            match self.at(i) {
                b'>' => {
                    self.pos = i + 1;
                    return true;
                }
                b'\n' => return false,
                _ => i += 1,
            }
        }
    }
}

/// Lex `text` as exactly one token, as `paste_tokens` and
/// `builtin_macro` do; `None` when it does not form one.
pub(super) fn lex_one(text: &[u8], loc: Loc) -> Option<Tok> {
    let mut buf = text.to_vec();
    buf.push(b'\n');
    let n = buf.len();
    let mut lexer = Lexer::new(Source {
        line: vec![1; n],
        col: (1..=n as u32).collect(),
        text: buf,
        notes: Vec::new(),
        eof_splice: None,
    });
    lexer.need_line = false;
    let env = LexEnv {
        name: loc.name,
        delta: 0,
        angled_headers: false,
        skipping: false,
    };
    let mut diags = Vec::new();
    let mut tok = lexer.lex(0, &env, &mut diags)?;
    if lexer.pos != n - 1 || diags.iter().any(|d| d.error) {
        return None;
    }
    tok.loc = loc;
    Some(tok)
}
