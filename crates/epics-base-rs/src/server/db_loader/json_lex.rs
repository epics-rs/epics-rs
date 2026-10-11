//! A `.db` field value in C's `JSON` start condition: `dbLex.l:99-155` for
//! the tokens and `dbYacc.y:256-362` for `json_value`.
//!
//! The lexer and the grammar are one pass here, as they are in C: bison asks
//! for a token only when it needs one, so a character after the token that
//! breaks the grammar is never lexed and never reported. A lexer refusal
//! (`yyerrorAbort`) does not stop the parse: the token is dropped, the rest
//! of the buffered line is still lexed, and the next line read returns EOF
//! (`dbLexRoutines.c:374`), which is where an unfinished value draws its
//! `syntax error`.

use crate::error::{CaError, CaResult, JsonLexFault};

/// The lexer's `yyerrorAbort`s over one text (`dbLex.l:129-153`).
#[derive(Default)]
pub(super) struct LexAborts {
    /// The line the lexer held at the first refusal; every later one is on
    /// it too, since the next line read is EOF.
    pub(super) line: usize,
    pub(super) faults: Vec<JsonLexFault>,
    /// Where the text now ends: past the newline of [`Self::line`]
    /// (`db_yyinput` hands the lexer nothing more, `dbLexRoutines.c:374`).
    pub(super) eof: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// `jsonNULL` / `jsonTRUE` / `jsonFALSE` / `jsonNUMBER` / `jsonBARE` /
    /// `jsonSTRING`: the grammar treats them alike as a `json_value`, and
    /// as a `json_key` only when `key` is set.
    Scalar {
        key: bool,
    },
    Punct(char),
    Eof,
}

struct Token {
    kind: Kind,
    /// C `yytext`.
    text: String,
    line: usize,
}

struct Lexer<'a> {
    chars: &'a [char],
    pos: &'a mut usize,
    line: &'a mut usize,
    col: &'a mut usize,
    aborts: &'a mut LexAborts,
}

fn is_barechar(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '.')
}

/// `exp` of `dbLex.l`: `[eE]{sign}[0-9]+`, the whole of `s`.
fn is_exp(s: &[u8]) -> bool {
    let [b'e' | b'E', rest @ ..] = s else {
        return false;
    };
    let rest = match rest {
        [b'+' | b'-', r @ ..] => r,
        r => r,
    };
    !rest.is_empty() && rest.iter().all(u8::is_ascii_digit)
}

/// C's `number` rule (`dbLex.l:40-47`) matched against the whole of `s`.
fn is_json_number(s: &str) -> bool {
    if s == "NaN" {
        return true;
    }
    let b = s.as_bytes();
    let b = match b {
        [b'+' | b'-', r @ ..] => r,
        r => r,
    };
    if b == b"Infinity" {
        return true;
    }
    if let [b'0', b'x' | b'X', hex @ ..] = b {
        return !hex.is_empty() && hex.iter().all(u8::is_ascii_hexdigit);
    }
    let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
    let (int, rest) = b.split_at(digits);
    if int.is_empty() {
        // fracexp: `{sign}"."[0-9]+{exp}?`
        let [b'.', frac @ ..] = rest else {
            return false;
        };
        let n = frac.iter().take_while(|c| c.is_ascii_digit()).count();
        return n > 0 && (n == frac.len() || is_exp(&frac[n..]));
    }
    // `int` is `[0-9]` or `[1-9][0-9]+`.
    if int.len() > 1 && int[0] == b'0' {
        return false;
    }
    match rest {
        [] => true,
        [b'.', frac @ ..] => {
            let n = frac.iter().take_while(|c| c.is_ascii_digit()).count();
            // `jsonnum` with a fraction, or `intexp` with none.
            n == frac.len() || is_exp(&frac[n..])
        }
        exp => is_exp(exp),
    }
}

/// The interior of a quoted string is `jsondqchar*` / `jsonsqchar*`
/// (`dbLex.l:26-33`): no control character, no bare backslash escape of
/// `u`, `x` or `1`-`9` unless it is the full `\xHH` / `\uHHHH` form.
fn is_json_string_body(body: &[char]) -> bool {
    let mut i = 0;
    while i < body.len() {
        match body[i] {
            '\\' => {
                let hex = |n: usize| {
                    body.get(i + 2..i + 2 + n)
                        .is_some_and(|h| h.iter().all(char::is_ascii_hexdigit))
                };
                match body.get(i + 1) {
                    Some('u') if hex(4) => i += 6,
                    Some('x') if hex(2) => i += 4,
                    Some('u' | 'x' | '1'..='9') | None => return false,
                    Some(_) => i += 2,
                }
            }
            c if (c as u32) < 0x20 => return false,
            _ => i += 1,
        }
    }
    true
}

impl Lexer<'_> {
    fn advance(&mut self) {
        if self.chars[*self.pos] == '\n' {
            *self.line += 1;
            *self.col = 0;
        } else {
            *self.col += 1;
        }
        *self.pos += 1;
    }

    /// Record a refusal raised with the lexer on `line`, whose `yytext`
    /// began at `start`.
    fn refuse_at(&mut self, start: usize, line: usize, fault: JsonLexFault) {
        if self.aborts.eof.is_none() {
            self.aborts.line = line;
            self.aborts.eof = Some(
                self.chars[start..]
                    .iter()
                    .position(|&c| c == '\n')
                    .map_or(self.chars.len(), |at| start + at + 1),
            );
        }
        self.aborts.faults.push(fault);
    }

    /// [`Self::refuse_at`] for the one character just consumed.
    fn refuse(&mut self, fault: JsonLexFault) {
        self.refuse_at(*self.pos - 1, *self.line, fault);
    }

    fn aborted(&self) -> bool {
        self.aborts.eof.is_some()
    }

    /// C `yylex` in the `JSON` start condition.
    fn token(&mut self) -> Token {
        loop {
            // Whitespace and `#` comments.
            while let Some(&c) = self.chars.get(*self.pos) {
                if c == '\n' && self.aborted() {
                    // `yyAbort`: the next line read is EOF.
                    return self.eof();
                }
                if c == '#' {
                    while self.chars.get(*self.pos).is_some_and(|&c| c != '\n') {
                        self.advance();
                    }
                } else if matches!(c, ' ' | '\t' | '\r' | '\n') {
                    self.advance();
                } else {
                    break;
                }
            }
            let Some(&c) = self.chars.get(*self.pos) else {
                return self.eof();
            };
            let (start, line) = (*self.pos, *self.line);
            if matches!(c, ':' | ',' | '[' | ']' | '{' | '}') {
                self.advance();
                return Token {
                    kind: Kind::Punct(c),
                    text: c.to_string(),
                    line,
                };
            }
            if c == '"' || c == '\'' {
                match self.string_end(c) {
                    Some(end) => {
                        let text: String = self.chars[start..=end].iter().collect();
                        let valid = is_json_string_body(&self.chars[start + 1..end]);
                        while *self.pos <= end {
                            self.advance();
                        }
                        if valid {
                            return Token {
                                kind: Kind::Scalar { key: true },
                                text,
                                line,
                            };
                        }
                        self.refuse_at(start, line, JsonLexFault::BadString(text));
                    }
                    None => {
                        self.advance();
                        self.refuse(JsonLexFault::Invalid(c));
                    }
                }
                continue;
            }
            if is_barechar(c) {
                while self.chars.get(*self.pos).is_some_and(|&c| is_barechar(c)) {
                    self.advance();
                }
                let text: String = self.chars[start..*self.pos].iter().collect();
                let key =
                    !(matches!(text.as_str(), "null" | "true" | "false") || is_json_number(&text));
                return Token {
                    kind: Kind::Scalar { key },
                    text,
                    line,
                };
            }
            self.advance();
            self.refuse(JsonLexFault::Invalid(c));
        }
    }

    /// The index of the quote closing the string opened at `pos`, by the
    /// error rule `{q}({schar}|{escape})*{q}` (`dbLex.l:131-141`): it ends at
    /// the first unescaped quote on the line, and an escape needs a
    /// character that is not the newline.
    fn string_end(&self, quote: char) -> Option<usize> {
        let mut j = *self.pos + 1;
        loop {
            match *self.chars.get(j)? {
                '\n' => return None,
                '\\' => match self.chars.get(j + 1) {
                    Some('\n') | None => return None,
                    Some(_) => j += 2,
                },
                c if c == quote => return Some(j),
                _ => j += 1,
            }
        }
    }

    /// The token the grammar asks for once a value is complete: `C` has left
    /// the `JSON` start condition, so the rest of the buffered line is lexed
    /// by the `INITIAL` rules (`dbLex.l:76-95,131-133,143-155`). Refusals are
    /// recorded and scanning goes on, as the `yyerrorAbort` rules do, until a
    /// token the grammar can see or the end of the line. Returns without
    /// moving past that token.
    fn lookahead(&mut self) {
        loop {
            while let Some(&c) = self.chars.get(*self.pos) {
                if c == '\n' && self.aborted() {
                    return;
                }
                if c == '#' {
                    while self.chars.get(*self.pos).is_some_and(|&c| c != '\n') {
                        self.advance();
                    }
                } else if matches!(c, ' ' | '\t' | '\r' | '\n') {
                    self.advance();
                } else {
                    break;
                }
            }
            let Some(&c) = self.chars.get(*self.pos) else {
                return;
            };
            let start = *self.pos;
            if c == '"' {
                // `{doublequote}({dqschar}|{escape})*` then a quote (a token)
                // or a newline (the error rule, which takes the newline too).
                match self.string_end('"') {
                    Some(_) => return,
                    None => {
                        let line = *self.line;
                        while let Some(&c) = self.chars.get(*self.pos) {
                            self.advance();
                            if c == '\n' {
                                break;
                            }
                        }
                        let text: String = self.chars[start..*self.pos].iter().collect();
                        self.refuse_at(start, line, JsonLexFault::NewlineInString(text));
                        continue;
                    }
                }
            }
            let is_initial_token = matches!(c, '{' | '}' | '(' | ')' | ',' | '%')
                || c.is_ascii_alphanumeric()
                || matches!(c, '_' | '-' | '+' | ':' | '.' | '[' | ']' | '<' | '>' | ';');
            if is_initial_token {
                return;
            }
            self.advance();
            self.refuse(JsonLexFault::Invalid(c));
        }
    }

    fn eof(&self) -> Token {
        Token {
            kind: Kind::Eof,
            text: String::new(),
            line: *self.line,
        }
    }

    /// `json_value`, its first token already read, rebuilt as `dbYacc.y`'s
    /// actions rebuild it (`:300-362`): no whitespace, a bare value quoted, a
    /// bare key quoted when it holds `+`, `-` or `.`, a member's trailing
    /// comma dropped and an element's kept. The error is the token the
    /// grammar rejected.
    fn value(&mut self, first: Token) -> Result<String, Token> {
        match first.kind {
            Kind::Scalar { key: false } => Ok(first.text),
            Kind::Scalar { key: true } if first.text.starts_with(['"', '\'']) => Ok(first.text),
            Kind::Scalar { key: true } => Ok(format!("\"{}\"", first.text)),
            Kind::Punct('[') => {
                let mut out = String::from("[");
                let mut t = self.token();
                if t.kind != Kind::Punct(']') {
                    loop {
                        out.push_str(&self.value(t)?);
                        match self.token() {
                            Token {
                                kind: Kind::Punct(']'),
                                ..
                            } => break,
                            Token {
                                kind: Kind::Punct(','),
                                ..
                            } => {
                                out.push(',');
                                t = self.token();
                                if t.kind == Kind::Punct(']') {
                                    break;
                                }
                            }
                            other => return Err(other),
                        }
                    }
                }
                out.push(']');
                Ok(out)
            }
            Kind::Punct('{') => {
                let mut members = Vec::new();
                let mut t = self.token();
                if t.kind != Kind::Punct('}') {
                    loop {
                        if !matches!(t.kind, Kind::Scalar { key: true }) {
                            return Err(t);
                        }
                        let key = if !t.text.starts_with(['"', '\''])
                            && t.text.contains(['+', '-', '.'])
                        {
                            format!("\"{}\"", t.text)
                        } else {
                            t.text
                        };
                        let colon = self.token();
                        if colon.kind != Kind::Punct(':') {
                            return Err(colon);
                        }
                        let v = self.token();
                        members.push(format!("{key}:{}", self.value(v)?));
                        match self.token() {
                            Token {
                                kind: Kind::Punct('}'),
                                ..
                            } => break,
                            Token {
                                kind: Kind::Punct(','),
                                ..
                            } => {
                                t = self.token();
                                if t.kind == Kind::Punct('}') {
                                    break;
                                }
                            }
                            other => return Err(other),
                        }
                    }
                }
                Ok(format!("{{{}}}", members.join(",")))
            }
            _ => Err(first),
        }
    }
}

/// Read one field value as C's `json_value` would, rebuilt (see
/// [`Lexer::value`]). A lexer refusal lands in `aborts` and the value still
/// completes if the grammar lets it; only the grammar's own rejection is an
/// error, its `syntax error`.
pub(super) fn read_json_value(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
    aborts: &mut LexAborts,
) -> CaResult<String> {
    let start_line = *line;
    let close = if chars.get(*pos) == Some(&'[') {
        ']'
    } else {
        '}'
    };
    let mut lx = Lexer {
        chars,
        pos,
        line,
        col,
        aborts,
    };
    let first = lx.token();
    match lx.value(first) {
        Ok(text) => {
            lx.look_past();
            Ok(text)
        }
        // The file ended inside the value, with nothing refused on the way.
        Err(t) if t.kind == Kind::Eof && !lx.aborted() => Err(CaError::DbParseError {
            line: start_line,
            token: String::new(),
            message: format!("unterminated JSON value (missing '{close}')"),
        }),
        Err(t) => Err(CaError::DbParseError {
            line: t.line,
            token: t.text,
            message: super::SYNTAX_ERROR.to_string(),
        }),
    }
}

/// What the lexer refuses between a value that parsed and the token that
/// follows it, which it does not consume.
pub(super) fn after_value(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
    aborts: &mut LexAborts,
) {
    Lexer {
        chars,
        pos,
        line,
        col,
        aborts,
    }
    .look_past();
}

impl Lexer<'_> {
    /// [`Self::lookahead`], moving past only what it refused.
    fn look_past(&mut self) {
        let at = (*self.pos, *self.line, *self.col);
        let refused = self.aborts.faults.len();
        self.lookahead();
        if self.aborts.faults.len() == refused {
            (*self.pos, *self.line, *self.col) = at;
        }
    }
}
