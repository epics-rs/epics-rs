//! C `dbConvertJSON.c` (@`R7.0.10`) — the JSON text a put hands a typed
//! array buffer, and the yajl scan that reads it.
//!
//! One owner for two callers, exactly as in C: `dbpf` on a field whose
//! `no_elements > 1` (`dbTest.c:413-429`) and `dbConstLoadArray` on a
//! bracketed constant link (`dbConstLink.c:177-199`). Both feed the same
//! `dbPutConvertJSON`, so both accept and refuse the same texts.
//!
//! The scanner below is a port of base's bundled yajl — `yajl_lex.c`'s value
//! lexer and the array/scalar half of `yajl_parser.c`'s state machine —
//! because `dbPutConvertJSON` needs three things from it that no other
//! parser can supply. It needs yajl's ACCEPT SET, which is JSON5: `yajl_alloc`
//! hands back a handle whose flags are already
//! `yajl_allow_json5 | yajl_allow_comments` (`yajl.c:76`) and
//! `dbPutConvertJSON` never calls `yajl_config`, so `dbpf REC '[0x10]'`,
//! `[Infinity]`, `[1,2,]`, `['a']`, `[.5]` and `[/*c*/1]` all land a value on
//! a C IOC. It needs yajl's ERROR CLASSIFICATION, the `lexical error: …` /
//! `parse error: …` split whose text `dbConvertJSON.c:174` prints verbatim.
//! And it needs `hand->bytesConsumed`, which is the column the `(right here)`
//! caret points at — a byte offset produced by yajl's own token scan and by
//! nothing else.
//!
//! [`crate::json5`] remains the single reader of the same dialect for every
//! consumer that wants a `serde_json::Value`; it is a rewriter, and a rewriter
//! cannot answer any of the three. The two are not a split grammar: the
//! durable shape is for `relaxed_to_strict` to be re-expressed over this token
//! stream, which is a change to `json5.rs` and belongs to whoever owns it.

use std::fmt;

use crate::types::{DbFieldType, EpicsValue, PvString};

/// `dbValueSize(DBR_STRING) - 1` (`dbConvertJSON.c:85-86`) — the yajl string
/// callback truncates to this before writing the terminating NUL.
const DBR_STRING_CAPACITY: usize = 39;

/// What C prints when `dbPutConvertJSON` returns `S_db_badField`.
///
/// Two fields because C makes up to two `errlogPrintf` calls and a reader sees
/// two records: the callback's own refusal, when a callback is what stopped
/// the parse (`dbConvertJSON.c:31`, `:36`, `:79-80`, `:111`, `:119`), and then
/// always `dbConvertJSON: %s` with yajl's rendered block (`:174`). Joining
/// them into one string would make the field mean "one line, or two lines
/// glued" by context, which is the shape that loses the second record.
///
/// Both fields carry the terminator their C original carries, because the
/// errlog console writes a record's bytes verbatim and appends nothing
/// (C `errlog.c:795`, and [`crate::runtime::log::errlog_printf`] here). The
/// refusal literals end in `\n` (`dbConvertJSON.c:31` and friends) and yajl's
/// rendered block ends in its `(right here) ------^\n` arrow
/// (`yajl_parser.c:99`), so a caller emits each field as it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertJsonError {
    /// The refusing callback's line, when one refused.
    pub refusal: Option<String>,
    /// `dbConvertJSON: ` followed by yajl's three-line verbose block.
    pub diagnostic: String,
}

impl fmt::Display for ConvertJsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Each field already ends in C's newline, so the two records
        // concatenate — a `writeln!` here would insert a blank line between
        // them that C's pair of `errlogPrintf` calls does not produce.
        if let Some(refusal) = &self.refusal {
            f.write_str(refusal)?;
        }
        f.write_str(&self.diagnostic)
    }
}

impl std::error::Error for ConvertJsonError {}

// ---------------------------------------------------------------------------
// yajl_lex.c
// ---------------------------------------------------------------------------

/// `yajl_lex.h`'s token set, less the four a `dbcj` parse can never see.
///
/// There is no `comment` token here: `yajl_lex_lex` consumes a comment and
/// loops (`yajl_lex.c:730-737`), so `Comment` never escapes the lexer and
/// [`Lexer::lex_comment`] reports its outcome on its own enum instead. There
/// is no identifier token because identifiers are lexed only in key position
/// (`yajl_lex_key`, `:791`), and key position is unreachable — `dbcj_start_map`
/// cancels the parse at the `{` itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tok {
    Eof,
    Error,
    LeftBrace,
    RightBrace,
    LeftBracket,
    RightBracket,
    Comma,
    Colon,
    Bool,
    Null,
    Integer,
    Double,
    Str,
    StrWithEscapes,
}

/// `yajl_lex_error`, less the three that report a disabled dialect
/// (`yajl_lex_unallowed_comment`, `…_unallowed_hex_integer`,
/// `…_unallowed_special_number`). JSON5 and comments are both on for every
/// EPICS handle, so no input can reach them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LexError {
    None,
    StringInvalidUtf8,
    StringInvalidEscapedChar,
    StringInvalidJsonChar,
    StringInvalidHexUChar,
    StringInvalidHexXChar,
    InvalidChar,
    InvalidString,
    MissingIntegerAfterExponent,
    MissingIntegerAfterDecimal,
    MissingIntegerAfterMinus,
    MissingHexDigitAfter0x,
}

impl LexError {
    /// C `yajl_lex_error_to_string` (`yajl_lex.c:915-955`), verbatim — this
    /// text is what the user reads after `lexical error: `.
    fn text(self) -> &'static str {
        match self {
            Self::None => "ok, no error",
            Self::StringInvalidUtf8 => "invalid bytes in UTF8 string.",
            Self::StringInvalidEscapedChar => {
                "inside a string, '\\' occurs before a character which it may not."
            }
            Self::StringInvalidJsonChar => "invalid character inside string.",
            Self::StringInvalidHexUChar => {
                "invalid (non-hex) character occurs after '\\u' inside string."
            }
            Self::StringInvalidHexXChar => {
                "invalid (non-hex) character occurs after '\\x' inside string."
            }
            Self::InvalidChar => "invalid char in json text.",
            Self::InvalidString => "invalid string in json text.",
            Self::MissingIntegerAfterExponent => {
                "malformed number, a digit is required after the exponent."
            }
            Self::MissingIntegerAfterDecimal => {
                "malformed number, a digit is required after the decimal point."
            }
            Self::MissingIntegerAfterMinus => {
                "malformed number, a digit is required after the plus/minus sign."
            }
            Self::MissingHexDigitAfter0x => {
                "malformed number, a hex digit is required after the 0x/0X."
            }
        }
    }
}

/// How [`Lexer::lex_utf8_char`] ended (C returns a `yajl_tok` for this).
enum Utf8 {
    Ok,
    Eof,
    Error,
}

/// How [`Lexer::lex_comment`] ended.
enum CommentEnd {
    Done,
    Eof,
    Error,
}

/// C's `struct yajl_lexer_t` (`yajl_lex.c:69-95`), less the two flags EPICS
/// never varies.
///
/// `buf` is not an optimisation. yajl lexes across chunk boundaries, and
/// `dbPutConvertJSON` DOES cross one: `yajl_complete_parse` re-enters the
/// parser with a lone space (`yajl_parser.c:186`), and the token the text
/// ended mid-way through is carried across in this buffer. Drop it and a bare
/// `dbpf REC 7` yields nothing at all, because the number token is still open
/// when the text runs out.
#[derive(Default)]
struct Lexer {
    buf: Vec<u8>,
    buf_off: usize,
    buf_in_use: bool,
    error: LexError,
}

impl Default for LexError {
    fn default() -> Self {
        Self::None
    }
}

impl Lexer {
    /// C's `readChar` macro (`yajl_lex.c:97-99`): the carried-over buffer
    /// first, then the caller's text.
    ///
    /// Every C call site is guarded by a `*offset >= jsonTextLen` test, so the
    /// text index is always in range; reading a NUL rather than panicking on a
    /// porting slip keeps a malformed put a parse error instead of an abort.
    fn read_char(&mut self, txt: &[u8], off: &mut usize) -> u8 {
        if self.buf_in_use && self.buf_off < self.buf.len() {
            let c = self.buf[self.buf_off];
            self.buf_off += 1;
            return c;
        }
        let c = txt.get(*off).copied().unwrap_or(0);
        *off += 1;
        c
    }

    /// C's `unreadChar` macro (`yajl_lex.c:101`).
    fn unread_char(&mut self, off: &mut usize) {
        if *off > 0 {
            *off -= 1;
        } else {
            self.buf_off = self.buf_off.saturating_sub(1);
        }
    }

    /// C `yajl_lex_lex` (`yajl_lex.c:604-790`) — the VALUE lexer. Returns the
    /// token and the bytes the parser would see through `outBuf`/`outLen`.
    fn lex(&mut self, txt: &[u8], off: &mut usize) -> (Tok, Vec<u8>) {
        let mut start_offset = *off;
        let tok = loop {
            if *off >= txt.len() {
                break Tok::Eof;
            }
            let c = self.read_char(txt, off);
            match c {
                b'{' => break Tok::LeftBrace,
                b'}' => break Tok::RightBrace,
                b'[' => break Tok::LeftBracket,
                b']' => break Tok::RightBracket,
                b',' => break Tok::Comma,
                b':' => break Tok::Colon,
                b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' ' => start_offset += 1,
                b't' => break self.lex_want(txt, off, b"rue", Tok::Bool),
                b'f' => break self.lex_want(txt, off, b"alse", Tok::Bool),
                b'n' => break self.lex_want(txt, off, b"ull", Tok::Null),
                // JSON5's bare non-finites (`yajl_lex.c:673-693`).
                b'I' => break self.lex_want(txt, off, b"nfinity", Tok::Double),
                b'N' => break self.lex_want(txt, off, b"aN", Tok::Double),
                // `'` falls into the same call as `"` under JSON5 (`:695-699`).
                b'\'' | b'"' => break self.lex_string(txt, off, c),
                // `+` and `.` open a number only under JSON5 (`:702`).
                b'+' | b'.' | b'-' | b'0'..=b'9' => {
                    self.unread_char(off);
                    break self.lex_number(txt, off);
                }
                b'/' => match self.lex_comment(txt, off) {
                    // A comment is whitespace: forget it and keep scanning.
                    CommentEnd::Done => {
                        self.buf.clear();
                        self.buf_in_use = false;
                        start_offset = *off;
                    }
                    CommentEnd::Eof => break Tok::Eof,
                    CommentEnd::Error => break Tok::Error,
                },
                _ => {
                    self.error = LexError::InvalidChar;
                    break Tok::Error;
                }
            }
        };

        // C's `lexed:` epilogue (`yajl_lex.c:749-776`).
        let mut out = Vec::new();
        if tok == Tok::Eof || self.buf_in_use {
            if !self.buf_in_use {
                self.buf.clear();
            }
            self.buf_in_use = true;
            let span = txt.get(start_offset..*off).unwrap_or(&[]);
            self.buf.extend_from_slice(span);
            self.buf_off = 0;
            if tok != Tok::Eof {
                out = self.buf.clone();
                self.buf_in_use = false;
            }
        } else if tok != Tok::Error {
            out = txt.get(start_offset..*off).unwrap_or(&[]).to_vec();
        }
        // "special case for strings. skip the quotes." (`:772-776`)
        if matches!(tok, Tok::Str | Tok::StrWithEscapes) && out.len() >= 2 {
            out = out[1..out.len() - 1].to_vec();
        }
        (tok, out)
    }

    /// C's `LEX_WANT` macro (`yajl_lex.c:588-601`).
    fn lex_want(&mut self, txt: &[u8], off: &mut usize, want: &[u8], on_match: Tok) -> Tok {
        for &w in want {
            if *off >= txt.len() {
                return Tok::Eof;
            }
            if self.read_char(txt, off) != w {
                self.unread_char(off);
                self.error = LexError::InvalidString;
                return Tok::Error;
            }
        }
        on_match
    }

    /// C `yajl_lex_string` (`yajl_lex.c:274-390`). The `yajl_string_scan`
    /// fast path (`:257-270`) is left out: it skips exactly the characters the
    /// loop below would accept unexamined, so it changes speed and nothing else.
    fn lex_string(&mut self, txt: &[u8], off: &mut usize, quote: u8) -> Tok {
        let mut tok = Tok::Error;
        let mut has_escapes = false;
        'lex: loop {
            if *off >= txt.len() {
                tok = Tok::Eof;
                break 'lex;
            }
            let c = self.read_char(txt, off);
            if c == quote {
                tok = Tok::Str;
                break 'lex;
            }
            if c == b'\\' {
                has_escapes = true;
                if *off >= txt.len() {
                    tok = Tok::Eof;
                    break 'lex;
                }
                let esc = self.read_char(txt, off);
                if esc == b'u' || esc == b'x' {
                    // `\xNN` is JSON5's (`:341-352`); `\uNNNN` is plain JSON's.
                    let (n, err) = if esc == b'u' {
                        (4, LexError::StringInvalidHexUChar)
                    } else {
                        (2, LexError::StringInvalidHexXChar)
                    };
                    for _ in 0..n {
                        if *off >= txt.len() {
                            tok = Tok::Eof;
                            break 'lex;
                        }
                        if !self.read_char(txt, off).is_ascii_hexdigit() {
                            self.unread_char(off);
                            self.error = err;
                            break 'lex;
                        }
                    }
                } else if (b'1'..=b'9').contains(&esc) {
                    // Under JSON5 these are the ONLY illegal escapes
                    // (`:354-355`); every other character resolves to itself
                    // through `yajl_string_decode`'s default arm.
                    self.unread_char(off);
                    self.error = LexError::StringInvalidEscapedChar;
                    break 'lex;
                } else if esc == b'\r' {
                    // A CRLF line continuation counts as one break (`:361-365`).
                    if *off >= txt.len() {
                        tok = Tok::Eof;
                        break 'lex;
                    }
                    if self.read_char(txt, off) != b'\n' {
                        self.unread_char(off);
                    }
                }
            } else if c < 0x20 {
                // `IJC` is the control characters plus the backslash, and the
                // backslash arm above already claimed it (`charLookupTable`,
                // `:149-190`).
                self.unread_char(off);
                self.error = LexError::StringInvalidJsonChar;
                break 'lex;
            } else {
                // `validateUTF8` is on for every EPICS handle: `yajl.c:133`
                // passes `!(flags & yajl_dont_validate_strings)`.
                match self.lex_utf8_char(txt, off, c) {
                    Utf8::Ok => {}
                    Utf8::Eof => {
                        tok = Tok::Eof;
                        break 'lex;
                    }
                    Utf8::Error => {
                        self.error = LexError::StringInvalidUtf8;
                        break 'lex;
                    }
                }
            }
        }
        if has_escapes && tok == Tok::Str {
            Tok::StrWithEscapes
        } else {
            tok
        }
    }

    /// C `yajl_lex_utf8_char` (`yajl_lex.c:203-246`).
    fn lex_utf8_char(&mut self, txt: &[u8], off: &mut usize, cur: u8) -> Utf8 {
        let need = if cur <= 0x7f {
            return Utf8::Ok;
        } else if cur >> 5 == 0x6 {
            1
        } else if cur >> 4 == 0x0e {
            2
        } else if cur >> 3 == 0x1e {
            3
        } else {
            return Utf8::Error;
        };
        for _ in 0..need {
            if *off >= txt.len() {
                return Utf8::Eof;
            }
            if self.read_char(txt, off) >> 6 != 0x2 {
                return Utf8::Error;
            }
        }
        Utf8::Ok
    }

    /// C `yajl_lex_number` (`yajl_lex.c:423-549`).
    fn lex_number(&mut self, txt: &[u8], off: &mut usize) -> Tok {
        macro_rules! eof {
            () => {
                if *off >= txt.len() {
                    return Tok::Eof;
                }
            };
        }
        let mut tok = Tok::Integer;
        let mut num_rd = 0u32;

        eof!();
        let mut c = self.read_char(txt, off);

        // A leading `+` is JSON5's (`:438`).
        if c == b'-' || c == b'+' {
            eof!();
            c = self.read_char(txt, off);
        }

        // `-Infinity` / `+Infinity` reach the number lexer, bare `Infinity`
        // reaches `lex_want` instead (`:443-459`).
        if c == b'I' {
            for &w in b"nfinity" {
                eof!();
                if self.read_char(txt, off) != w {
                    self.unread_char(off);
                    self.error = LexError::InvalidString;
                    return Tok::Error;
                }
            }
            return Tok::Double;
        }

        let mut fraction = false;
        if c == b'0' {
            num_rd += 1;
            eof!();
            c = self.read_char(txt, off);
            if c == b'x' || c == b'X' {
                // JSON5 hex (`:465-470`), still an INTEGER token.
                return self.lex_hex(txt, off);
            }
        } else if (b'1'..=b'9').contains(&c) {
            loop {
                num_rd += 1;
                eof!();
                c = self.read_char(txt, off);
                if !c.is_ascii_digit() {
                    break;
                }
            }
        } else if c == b'.' {
            // A leading `.` is JSON5's (`:478`), and it enters the fraction
            // WITHOUT consuming the dot twice.
            fraction = true;
        } else {
            self.unread_char(off);
            self.error = LexError::MissingIntegerAfterMinus;
            return Tok::Error;
        }

        if fraction || c == b'.' {
            eof!();
            c = self.read_char(txt, off);
            // `if (!allowJson5) numRd = 0;` is skipped: keeping the integer
            // digit count is what lets `5.` lex (`:487-491`).
            while c.is_ascii_digit() {
                num_rd += 1;
                eof!();
                c = self.read_char(txt, off);
            }
            if num_rd == 0 {
                self.unread_char(off);
                self.error = LexError::MissingIntegerAfterDecimal;
                return Tok::Error;
            }
            tok = Tok::Double;
        }

        if c == b'e' || c == b'E' {
            eof!();
            c = self.read_char(txt, off);
            if c == b'+' || c == b'-' {
                eof!();
                c = self.read_char(txt, off);
            }
            if !c.is_ascii_digit() {
                self.unread_char(off);
                self.error = LexError::MissingIntegerAfterExponent;
                return Tok::Error;
            }
            loop {
                eof!();
                c = self.read_char(txt, off);
                if !c.is_ascii_digit() {
                    break;
                }
            }
            tok = Tok::Double;
        }

        // "we always go one too far" (`:546`).
        self.unread_char(off);
        tok
    }

    /// C's `got_hex:` arm (`yajl_lex.c:528-543`).
    fn lex_hex(&mut self, txt: &[u8], off: &mut usize) -> Tok {
        if *off >= txt.len() {
            return Tok::Eof;
        }
        if !self.read_char(txt, off).is_ascii_hexdigit() {
            self.unread_char(off);
            self.error = LexError::MissingHexDigitAfter0x;
            return Tok::Error;
        }
        loop {
            if *off >= txt.len() {
                return Tok::Eof;
            }
            if !self.read_char(txt, off).is_ascii_hexdigit() {
                break;
            }
        }
        self.unread_char(off);
        Tok::Integer
    }

    /// C `yajl_lex_comment` (`yajl_lex.c:551-586`).
    fn lex_comment(&mut self, txt: &[u8], off: &mut usize) -> CommentEnd {
        macro_rules! eof {
            () => {
                if *off >= txt.len() {
                    return CommentEnd::Eof;
                }
            };
        }
        eof!();
        match self.read_char(txt, off) {
            b'/' => loop {
                eof!();
                if self.read_char(txt, off) == b'\n' {
                    return CommentEnd::Done;
                }
            },
            b'*' => loop {
                eof!();
                if self.read_char(txt, off) == b'*' {
                    eof!();
                    if self.read_char(txt, off) == b'/' {
                        return CommentEnd::Done;
                    }
                    self.unread_char(off);
                }
            },
            _ => {
                self.error = LexError::InvalidChar;
                CommentEnd::Error
            }
        }
    }
}

// ---------------------------------------------------------------------------
// yajl_parser.c
// ---------------------------------------------------------------------------

/// C `yajl_parse_integer` (`yajl_parser.c:38-85`). `Err` is its `ERANGE`,
/// which the parser turns into `integer overflow`.
fn parse_integer(number: &[u8]) -> Result<i64, ()> {
    let mut ret: i64 = 0;
    let mut sign: i64 = 1;
    let mut base: i64 = 10;
    let mut pos = 0;

    match number.first() {
        Some(b'-') => {
            pos = 1;
            sign = -1;
        }
        Some(b'+') => pos = 1,
        _ => {}
    }
    if number.get(pos) == Some(&b'0') && matches!(number.get(pos + 1), Some(b'x' | b'X')) {
        base = 16;
        pos += 2;
    }
    let max = i64::MAX / base;

    while pos < number.len() {
        if ret > max {
            return Err(());
        }
        ret *= base;
        // The lexer has already rejected any non-digit, so the fold from
        // 'A'/'a' is unconditional above 9 (`yajl_parser.c:72-77`).
        let mut digit = i64::from(number[pos]) - i64::from(b'0');
        pos += 1;
        if digit > 9 {
            digit = (digit - i64::from(b'A' - b'0') + 10) & 0xf;
        }
        if i64::MAX - ret < digit {
            return Err(());
        }
        ret += digit;
    }
    Ok(sign * ret)
}

/// C's `epicsStrtod(buf, NULL)` plus the `ERANGE` test at
/// `yajl_parser.c:340-346`. `Err` is `numeric (floating point) overflow`.
fn parse_double(buf: &[u8]) -> Result<f64, ()> {
    let text = String::from_utf8_lossy(buf);
    let value: f64 = text.parse().unwrap_or(f64::NAN);
    // `strtod` signals overflow by returning ±HUGE_VAL with ERANGE set, which
    // is the only way C reaches that message; Rust's parser saturates to
    // infinity in silence. The two are told apart by the token: yajl's own
    // Infinity spellings are the only literals allowed to BE infinite.
    if value.is_infinite() && !text.trim_start_matches(['+', '-']).starts_with('I') {
        return Err(());
    }
    Ok(value)
}

/// C `yajl_string_decode` (`yajl_encode.c:137-217`) — resolve the escapes in a
/// `yajl_tok_string_with_escapes` payload.
fn string_decode(src: &[u8]) -> Vec<u8> {
    /// C `hexToDigit` (`yajl_encode.c:98-109`).
    fn hex_to_digit(src: &[u8], at: usize, len: usize) -> u32 {
        let mut val = 0u32;
        for i in 0..len {
            let mut c = src.get(at + i).copied().unwrap_or(b'0');
            if c >= b'A' {
                c = (c & !0x20) - 7;
            }
            val = (val << 4) | u32::from(c - b'0');
        }
        val
    }
    /// C `Utf32toUtf8` (`yajl_encode.c:111-135`).
    fn utf32_to_utf8(cp: u32, out: &mut Vec<u8>) {
        match cp {
            0..=0x7f => out.push(cp as u8),
            0x80..=0x7ff => {
                out.push((cp >> 6) as u8 | 0xc0);
                out.push((cp & 0x3f) as u8 | 0x80);
            }
            0x800..=0xffff => {
                out.push((cp >> 12) as u8 | 0xe0);
                out.push(((cp >> 6) & 0x3f) as u8 | 0x80);
                out.push((cp & 0x3f) as u8 | 0x80);
            }
            0x10000..=0x1f_ffff => {
                out.push((cp >> 18) as u8 | 0xf0);
                out.push(((cp >> 12) & 0x3f) as u8 | 0x80);
                out.push(((cp >> 6) & 0x3f) as u8 | 0x80);
                out.push((cp & 0x3f) as u8 | 0x80);
            }
            _ => out.push(b'?'),
        }
    }

    let mut out = Vec::with_capacity(src.len());
    let mut beg = 0usize;
    let mut end = 0usize;
    while end < src.len() {
        if src[end] != b'\\' {
            end += 1;
            continue;
        }
        out.extend_from_slice(&src[beg..end]);
        end += 1;
        match src.get(end).copied().unwrap_or(0) {
            b'r' => out.push(b'\r'),
            b'n' => out.push(b'\n'),
            b'\\' => out.push(b'\\'),
            b'f' => out.push(0x0c),
            b'b' => out.push(0x08),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'u' => {
                end += 1;
                let mut cp = hex_to_digit(src, end, 4);
                end += 3;
                if cp & 0xfc00 == 0xd800 {
                    if src.get(end + 1) == Some(&b'\\') && src.get(end + 2) == Some(&b'u') {
                        end += 1;
                        let surrogate = hex_to_digit(src, end + 2, 4);
                        cp = ((cp & 0x3f) << 10)
                            | (((((cp >> 6) & 0xf) + 1) << 16) | (surrogate & 0x3ff));
                        end += 5;
                    } else {
                        out.push(b'?');
                        end += 1;
                        beg = end;
                        continue;
                    }
                }
                utf32_to_utf8(cp, &mut out);
            }
            // The JSON5-only escapes (`yajl_encode.c:185-207`).
            b'\n' => {
                end += 1;
                beg = end;
                continue;
            }
            b'\r' => {
                end += 1;
                if src.get(end) == Some(&b'\n') {
                    end += 1;
                }
                beg = end;
                continue;
            }
            b'0' => out.push(0),
            b'x' => {
                end += 1;
                out.push(hex_to_digit(src, end, 2) as u8);
                end += 1;
            }
            // "the character itself", which is what makes `\'` and `\/` work.
            other => out.push(other),
        }
        end += 1;
        beg = end;
    }
    out.extend_from_slice(&src[beg.min(src.len())..]);
    out
}

/// `yajl_state`, less the map states (`dbcj_start_map` cancels at the `{`, so
/// no key is ever lexed) and `got_value` (which needs
/// `yajl_allow_multiple_values`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Start,
    ParseComplete,
    ArrayStart,
    ArrayNeedVal,
    ArrayGotVal,
    ParseError,
    LexicalError,
}

/// `yajl_status`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Ok,
    Error,
    ClientCanceled,
}

/// C's `parseContext` (`dbConvertJSON.c:22-28`) — the `dbcj_*` callbacks and
/// the state they share.
struct Context {
    depth: i32,
    target: DbFieldType,
    elems: usize,
    tokens: Vec<Token>,
    refusal: Option<String>,
}

impl Context {
    /// A callback that returns 0 after saying why. Every one of these is a
    /// `_CC_CHK` cancellation upstream, so the caller sees this line AND
    /// yajl's `client cancelled parse` block.
    fn refuse(&mut self, message: String) -> bool {
        self.refusal = Some(message);
        false
    }

    /// C's `if (parser->elems > 0)` guard, shared by all three value
    /// callbacks — a token past the buffer is parsed, checked, and dropped.
    fn store(&mut self, token: Token) -> bool {
        if self.elems > 0 {
            self.tokens.push(token);
            self.elems -= 1;
        }
        true
    }

    /// `dbcj_null` (`dbConvertJSON.c:30-33`).
    fn null(&mut self) -> bool {
        self.refuse("dbConvertJSON: Null objects not supported\n".into())
    }

    /// `dbcj_boolean` (`:35-38`).
    fn boolean(&mut self) -> bool {
        self.refuse("dbConvertJSON: Boolean not supported\n".into())
    }

    /// `dbcj_start_map` (`:110-113`).
    fn start_map(&mut self) -> bool {
        self.refuse("dbConvertJSON: Map type not supported\n".into())
    }

    /// `dbcj_start_array` (`:115-122`) — legal exactly once. Note it speaks
    /// and then returns `depth == 1`, so the message is emitted by the same
    /// call that cancels.
    fn start_array(&mut self) -> bool {
        self.depth += 1;
        if self.depth > 1 {
            self.refusal = Some("dbConvertJSON: Embedded arrays not supported\n".into());
        }
        self.depth == 1
    }

    /// `dbcj_integer` (`:40-51`).
    fn integer(&mut self, value: i64) -> bool {
        self.store(Token::Int(value))
    }

    /// `dbcj_double` (`:53-63`).
    fn double(&mut self, value: f64) -> bool {
        self.store(Token::Double(value))
    }

    /// `dbcj_string` (`:71-93`): a string is legal ONLY into a `DBF_STRING`
    /// buffer. The comment at `:75-77` is why — nothing here knows a
    /// `DBF_CHAR` field's width, so `dbpf` never reaches this function for one
    /// (it sends the raw bytes instead). The type test precedes the capacity
    /// guard, so a string past the end still refuses the whole put.
    fn string(&mut self, bytes: &[u8]) -> bool {
        if self.target != DbFieldType::String {
            let shown = String::from_utf8_lossy(bytes);
            return self.refuse(format!(
                "dbConvertJSON: String \"{shown}\" provided, numeric value expected\n"
            ));
        }
        let len = bytes.len().min(DBR_STRING_CAPACITY);
        self.store(Token::Text(PvString::from_bytes(&bytes[..len])))
    }
}

/// C's `struct yajl_handle_t` reduced to the fields a `dbcj` parse touches.
struct Parser {
    lexer: Lexer,
    stack: Vec<State>,
    bytes_consumed: usize,
    parse_error: Option<&'static str>,
    ctx: Context,
}

impl Parser {
    fn new(ctx: Context) -> Self {
        Self {
            lexer: Lexer::default(),
            stack: vec![State::Start],
            bytes_consumed: 0,
            parse_error: None,
            ctx,
        }
    }

    /// `yajl_bs_current`.
    fn state(&self) -> State {
        *self.stack.last().expect("the base state is never popped")
    }

    /// `yajl_bs_set`.
    fn set(&mut self, state: State) {
        *self
            .stack
            .last_mut()
            .expect("the base state is never popped") = state;
    }

    /// `_CC_CHK` (`yajl_parser.c:174-181`).
    fn cancel(&mut self) -> Status {
        self.set(State::ParseError);
        self.parse_error = Some("client cancelled parse via callback return value");
        Status::ClientCanceled
    }

    /// C `yajl_do_parse` (`yajl_parser.c:207-524`), minus the map arms.
    fn do_parse(&mut self, txt: &[u8]) -> Status {
        self.bytes_consumed = 0;
        loop {
            match self.state() {
                State::ParseComplete => {
                    // Neither `yajl_allow_multiple_values` nor
                    // `yajl_allow_trailing_garbage` is set on an EPICS handle.
                    if self.bytes_consumed == txt.len() {
                        return Status::Ok;
                    }
                    let mut off = self.bytes_consumed;
                    let (tok, _) = self.lexer.lex(txt, &mut off);
                    self.bytes_consumed = off;
                    if tok != Tok::Eof {
                        self.set(State::ParseError);
                        self.parse_error = Some("trailing garbage");
                    }
                }
                State::LexicalError | State::ParseError => return Status::Error,
                State::Start | State::ArrayStart | State::ArrayNeedVal => {
                    let mut off = self.bytes_consumed;
                    let (tok, buf) = self.lexer.lex(txt, &mut off);
                    self.bytes_consumed = off;
                    let mut push_array = false;
                    let accepted = match tok {
                        Tok::Eof => return Status::Ok,
                        Tok::Error => {
                            self.set(State::LexicalError);
                            continue;
                        }
                        Tok::Str => self.ctx.string(&buf),
                        Tok::StrWithEscapes => {
                            let decoded = string_decode(&buf);
                            self.ctx.string(&decoded)
                        }
                        Tok::Bool => self.ctx.boolean(),
                        Tok::Null => self.ctx.null(),
                        Tok::LeftBrace => self.ctx.start_map(),
                        Tok::LeftBracket => {
                            push_array = true;
                            self.ctx.start_array()
                        }
                        Tok::Integer => match parse_integer(&buf) {
                            Ok(value) => self.ctx.integer(value),
                            Err(()) => {
                                self.set(State::ParseError);
                                self.parse_error = Some("integer overflow");
                                self.restore_error_offset(buf.len());
                                continue;
                            }
                        },
                        Tok::Double => match parse_double(&buf) {
                            Ok(value) => self.ctx.double(value),
                            Err(()) => {
                                self.set(State::ParseError);
                                self.parse_error = Some("numeric (floating point) overflow");
                                self.restore_error_offset(buf.len());
                                continue;
                            }
                        },
                        Tok::RightBracket
                            if matches!(self.state(), State::ArrayStart | State::ArrayNeedVal) =>
                        {
                            // A trailing comma before `]` is JSON5's
                            // (`yajl_parser.c:357-360` admits `array_need_val`);
                            // `dbcj_callbacks` has no `end_array`.
                            self.stack.pop();
                            continue;
                        }
                        Tok::RightBracket | Tok::Colon | Tok::Comma | Tok::RightBrace => {
                            self.set(State::ParseError);
                            self.parse_error = Some("unallowed token at this point in JSON text");
                            continue;
                        }
                    };
                    if !accepted {
                        return self.cancel();
                    }
                    // "got a value. transition depends on the state we're in."
                    if self.state() == State::Start {
                        self.set(State::ParseComplete);
                    } else {
                        self.set(State::ArrayGotVal);
                    }
                    if push_array {
                        self.stack.push(State::ArrayStart);
                    }
                }
                State::ArrayGotVal => {
                    let mut off = self.bytes_consumed;
                    let (tok, _) = self.lexer.lex(txt, &mut off);
                    self.bytes_consumed = off;
                    match tok {
                        Tok::RightBracket => {
                            self.stack.pop();
                        }
                        Tok::Comma => self.set(State::ArrayNeedVal),
                        Tok::Eof => return Status::Ok,
                        Tok::Error => self.set(State::LexicalError),
                        _ => {
                            self.set(State::ParseError);
                            self.parse_error = Some("after array element, I expect ',' or ']'");
                        }
                    }
                }
            }
        }
    }

    /// C's "try to restore error offset" (`yajl_parser.c:320-321`) — back the
    /// caret up over the token that overflowed.
    fn restore_error_offset(&mut self, token_len: usize) {
        self.bytes_consumed = self.bytes_consumed.saturating_sub(token_len);
    }

    /// C `yajl_do_finish` (`yajl_parser.c:183-205`), reached through
    /// `yajl_complete_parse`. Re-entering the parser with a lone space is what
    /// closes a token the text ended in the middle of — and it resets
    /// `bytesConsumed`, which is why a `premature EOF` caret sits at column
    /// one of the ORIGINAL text rather than at its end.
    fn do_finish(&mut self) -> Status {
        let status = self.do_parse(b" ");
        if status != Status::Ok {
            return status;
        }
        match self.state() {
            State::ParseError | State::LexicalError => Status::Error,
            State::ParseComplete => Status::Ok,
            _ => {
                self.set(State::ParseError);
                self.parse_error = Some("premature EOF");
                Status::Error
            }
        }
    }

    /// C `yajl_render_error_string` with `verbose = 1`, which is what
    /// `dbPutConvertJSON` passes (`dbConvertJSON.c:172`).
    ///
    /// The window is emitted as bytes and only then read as text, so a caret
    /// that lands mid-codepoint costs a replacement character here where C
    /// writes the partial byte; nothing else differs.
    fn render_error(&self, txt: &[u8]) -> String {
        let (kind, detail) = match self.state() {
            State::ParseError => ("parse", self.parse_error),
            State::LexicalError => ("lexical", Some(self.lexer.error.text())),
            _ => ("unknown", None),
        };
        let mut out = format!("{kind} error");
        if let Some(detail) = detail {
            out.push_str(": ");
            out.push_str(detail);
        }
        out.push('\n');

        // "append as many spaces as needed to make sure the error falls at
        // char 41" (`yajl_parser.c:131-132`).
        let offset = self.bytes_consumed;
        let spaces = if offset < 30 { 40 - offset } else { 10 };
        let start = offset.saturating_sub(30);
        let end = (offset + 30).min(txt.len()).max(start);
        out.push_str(&" ".repeat(spaces));
        let window: Vec<u8> = txt[start..end]
            .iter()
            .map(|&b| if b == b'\n' || b == b'\r' { b' ' } else { b })
            .collect();
        out.push_str(&String::from_utf8_lossy(&window));
        out.push('\n');
        out.push_str("                     (right here) ------^\n");
        out
    }
}

// ---------------------------------------------------------------------------
// dbConvertJSON.c
// ---------------------------------------------------------------------------

/// One yajl callback's worth of input. The token's own kind picks the C
/// `dbFastPutConvertRoutine` row: `DBF_INT64` for `dbcj_integer`
/// (`dbConvertJSON.c:43`), `DBF_DOUBLE` for `dbcj_double` (`:55`), and the
/// verbatim bytes for `dbcj_string` (`:87-88`).
enum Token {
    Int(i64),
    Double(f64),
    Text(PvString),
}

impl Token {
    /// The `DBF_STRING` row of C's put table. A text token is `strncpy`'d
    /// (already truncated on the way in); a numeric one goes through
    /// [`EpicsValue::convert_to`], the port's single owner of that table.
    fn as_dbr_string(&self) -> PvString {
        match self {
            Token::Text(s) => s.clone(),
            Token::Int(i) => truncate(&EpicsValue::Int64(*i).convert_to(DbFieldType::String)),
            // `dbcj_double` hands the converter a NULL `paddr`
            // (`dbConvertJSON.c:58`), so `cvt_d_st` finds no rset and renders
            // at its own seeded precision of 6 — not Rust's `Display`, which
            // gave `1` where C gives `1.000000`.
            Token::Double(d) => truncate(&EpicsValue::String(
                crate::types::codec::cvt_double_to_string(*d, 6).into(),
            )),
        }
    }

    /// `Some` only for `dbcj_integer`'s tokens, so a whole-batch `collect()`
    /// answers "is the `DBF_INT64` row enough", which keeps integers past
    /// 2^53 exact instead of routing them through a double.
    fn int(&self) -> Option<i64> {
        match self {
            Token::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// The `DBF_DOUBLE` row's source value.
    fn real(&self) -> f64 {
        match self {
            Token::Int(i) => *i as f64,
            Token::Double(d) => *d,
            Token::Text(s) => match EpicsValue::String(s.clone()).convert_to(DbFieldType::Double) {
                EpicsValue::Double(d) => d,
                _ => f64::NAN,
            },
        }
    }
}

fn truncate(v: &EpicsValue) -> PvString {
    let text = v.to_string();
    let bytes = text.as_bytes();
    PvString::from_bytes(&bytes[..bytes.len().min(DBR_STRING_CAPACITY)])
}

/// C `dbPutConvertJSON` (`dbConvertJSON.c:130-182`).
///
/// `capacity` is C's incoming `*pnRequest` — the buffer `dbpf` calloc'd at
/// `addr.no_elements`, or the record's `NELM` under `dbConstLoadArray`. C
/// drops every element past it in silence (`if (parser->elems > 0)` guards
/// each callback), and reports what it actually wrote by subtracting the
/// leftovers (`:167`); here that count is the returned array's length.
///
/// An empty text is C's `!jlen` short-circuit (`:144-147`): zero elements,
/// status 0 — `dbpf B:WS ""` on a `LONG` waveform leaves `dbgf` printing
/// `DBF_LONG[0]: (empty)`.
///
/// `Err` is C's `S_db_badField`, carrying both lines C writes to the errlog.
pub fn db_put_convert_json(
    json: &str,
    target: DbFieldType,
    capacity: usize,
) -> Result<EpicsValue, ConvertJsonError> {
    if json.is_empty() {
        return Ok(EpicsValue::DoubleArray(Vec::new()).convert_to(target));
    }

    let txt = json.as_bytes();
    let mut parser = Parser::new(Context {
        depth: 0,
        target,
        elems: capacity,
        tokens: Vec::new(),
        refusal: None,
    });

    let mut status = parser.do_parse(txt);
    if status == Status::Ok {
        status = parser.do_finish();
    }
    if status != Status::Ok {
        return Err(ConvertJsonError {
            refusal: parser.ctx.refusal.take(),
            diagnostic: format!("dbConvertJSON: {}", parser.render_error(txt)),
        });
    }

    // The `conv(&val, parser->pdest, NULL)` loop (`:46`, `:58`, `:87`) — every
    // element through C's put table into one typed buffer.
    let tokens = parser.ctx.tokens;
    if target == DbFieldType::String {
        return Ok(EpicsValue::StringArray(
            tokens.iter().map(Token::as_dbr_string).collect(),
        ));
    }
    Ok(
        match tokens.iter().map(Token::int).collect::<Option<Vec<_>>>() {
            Some(ints) => EpicsValue::Int64Array(ints),
            None => EpicsValue::DoubleArray(tokens.iter().map(Token::real).collect()),
        }
        .convert_to(target),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every arm measured on `softIoc` @`R7.0.10` through
    /// `dbpf("B:WL", …)` on a `LONG` waveform of `NELM=4`, which is
    /// `dbPutConvertJSON(text, DBR_LONG, buf, &4)`.
    #[test]
    fn long_array_conversions_match_softioc() {
        let conv = |json| db_put_convert_json(json, DbFieldType::Long, 4);

        // A bare scalar: `dbcj_start_array` never fires, one element lands.
        assert_eq!(conv("7").unwrap(), EpicsValue::LongArray(vec![7]));
        assert_eq!(
            conv("[1,2,3]").unwrap(),
            EpicsValue::LongArray(vec![1, 2, 3])
        );
        // `if (parser->elems > 0)` drops the tail in silence (`:45`, `:57`).
        assert_eq!(
            conv("[1,2,3,4,5]").unwrap(),
            EpicsValue::LongArray(vec![1, 2, 3, 4])
        );
        // `!jlen` (`:144-147`) — zero elements, status 0.
        assert_eq!(conv("").unwrap(), EpicsValue::LongArray(vec![]));
        // `dbcj_double` → `dbFastPutConvertRoutine[DBF_DOUBLE][DBF_LONG]`
        // truncates toward zero, it does not round.
        assert_eq!(
            conv("[1.9,2.1]").unwrap(),
            EpicsValue::LongArray(vec![1, 2])
        );
    }

    /// The `DBF_STRING` target is the one that takes text, and it truncates
    /// at `dbValueSize(DBR_STRING) - 1` (`:85-86`).
    #[test]
    fn string_target_takes_text_and_truncates_at_39() {
        assert_eq!(
            db_put_convert_json("[\"a\",\"bb\"]", DbFieldType::String, 3).unwrap(),
            EpicsValue::StringArray(vec!["a".into(), "bb".into()])
        );
        let long = "x".repeat(50);
        let EpicsValue::StringArray(v) =
            db_put_convert_json(&format!("[\"{long}\"]"), DbFieldType::String, 1).unwrap()
        else {
            panic!("a DBF_STRING target yields a StringArray");
        };
        assert_eq!(v[0].as_str_lossy().len(), DBR_STRING_CAPACITY);
        // A numeric token is legal here too — C runs it through
        // `dbFastPutConvertRoutine[DBF_INT64][DBF_STRING]`.
        assert_eq!(
            db_put_convert_json("[1,\"b\"]", DbFieldType::String, 2).unwrap(),
            EpicsValue::StringArray(vec!["1".into(), "b".into()])
        );
    }

    /// A REAL token into a `DBF_STRING` target renders at precision 6, not at
    /// the record's PREC and not through Rust's `Display`: `dbcj_double`
    /// passes `conv(&num, parser->pdest, NULL)` (`dbConvertJSON.c:58`), and
    /// with a NULL `paddr` `cvt_d_st` finds no rset and keeps its own seed
    /// (`dbFastLinkConv.c:1339-1344`).
    ///
    /// softIoc @`R7.0.10`, `T:WS` a `STRING` waveform with `NELM=4` and
    /// `PREC=3`:
    ///
    /// ```text
    /// dbpf T:WS "[1.0, 2.5, 1.23456789]"
    /// dbgf T:WS -> DBF_STRING[3]: "1.000000"  "2.500000"  "1.234568"
    /// ```
    #[test]
    fn a_real_token_into_a_string_target_renders_at_precision_six() {
        assert_eq!(
            db_put_convert_json("[1.0, 2.5, 1.23456789]", DbFieldType::String, 4).unwrap(),
            EpicsValue::StringArray(vec![
                "1.000000".into(),
                "2.500000".into(),
                "1.234568".into()
            ])
        );
    }

    /// An all-integer literal keeps C's `DBF_INT64` row, so a value past
    /// 2^53 survives instead of being rounded through a double.
    #[test]
    fn integers_past_two_to_the_53_stay_exact() {
        assert_eq!(
            db_put_convert_json("[9007199254740993]", DbFieldType::Int64, 1).unwrap(),
            EpicsValue::Int64Array(vec![9007199254740993])
        );
    }

    /// `yajl_alloc` sets `yajl_allow_json5 | yajl_allow_comments` (`yajl.c:76`)
    /// and `dbPutConvertJSON` never calls `yajl_config`, so THIS is the dialect
    /// a `dbpf` accepts. Every case was measured on softIoc @`R7.0.10`; every
    /// one of them is a `serde_json` syntax error.
    #[test]
    fn the_json5_dialect_every_epics_handle_carries() {
        let long = |j| db_put_convert_json(j, DbFieldType::Long, 4).unwrap();
        let real = |j| db_put_convert_json(j, DbFieldType::Double, 4).unwrap();
        let text = |j| db_put_convert_json(j, DbFieldType::String, 4).unwrap();

        // Trailing comma before `]` (`yajl_parser.c:357-360`).
        assert_eq!(long("[1,2,]"), EpicsValue::LongArray(vec![1, 2]));
        assert_eq!(real("[1,]"), EpicsValue::DoubleArray(vec![1.0]));
        // Hex integers, either case of the `x` (`yajl_lex.c:465-470`).
        assert_eq!(long("[0x10]"), EpicsValue::LongArray(vec![16]));
        assert_eq!(long("[0X1f]"), EpicsValue::LongArray(vec![31]));
        // Leading `+` (`:438`), leading `.` (`:478`), trailing `.` (`:491`).
        assert_eq!(long("[+5]"), EpicsValue::LongArray(vec![5]));
        assert_eq!(real("[.5]"), EpicsValue::DoubleArray(vec![0.5]));
        assert_eq!(real("[5.]"), EpicsValue::DoubleArray(vec![5.0]));
        // Comments are whitespace, so they SEPARATE tokens (`:728-737`).
        assert_eq!(long("[/*c*/1]"), EpicsValue::LongArray(vec![1]));
        assert_eq!(long("[1 // tail\n]"), EpicsValue::LongArray(vec![1]));
        // Single-quoted strings (`:695-699`), bare or bracketed.
        assert_eq!(
            text("['a','b']"),
            EpicsValue::StringArray(vec!["a".into(), "b".into()])
        );
        assert_eq!(text("'abc'"), EpicsValue::StringArray(vec!["abc".into()]));
        // The non-finites the GENERATOR writes for a non-finite double
        // (`yajl_gen.c:228-232`), read back by `:673-693`.
        assert_eq!(
            real("[Infinity]"),
            EpicsValue::DoubleArray(vec![f64::INFINITY])
        );
        assert_eq!(
            real("[-Infinity]"),
            EpicsValue::DoubleArray(vec![f64::NEG_INFINITY])
        );
        let EpicsValue::DoubleArray(nan) = real("[NaN]") else {
            panic!("a DBF_DOUBLE target yields a DoubleArray");
        };
        assert!(nan[0].is_nan());
    }

    /// The two texts whose acceptance turns on a leading zero, which is the
    /// boundary between "a single zero" and "a series of decimal digits"
    /// (`yajl_lex.c:462-472`). `[01]` lexes `0` and then `1`, so it is the
    /// PARSER that refuses it, one token later than a reader would guess.
    #[test]
    fn a_leading_zero_ends_the_number_token() {
        assert_eq!(
            db_put_convert_json("[0]", DbFieldType::Long, 4).unwrap(),
            EpicsValue::LongArray(vec![0])
        );
        let e = db_put_convert_json("[01]", DbFieldType::Long, 4).unwrap_err();
        assert!(
            e.diagnostic
                .contains("after array element, I expect ',' or ']'")
        );
    }

    // Generated from the softIoc @R7.0.10 A/B run; every string below is
    // what the C IOC wrote to stderr for that exact `dbpf` argument.
    const C_REFUSALS: &[(&str, DbFieldType, Option<&str>, &str)] = &[
        (
            "[1,2,zz]",
            DbFieldType::Long,
            None,
            "dbConvertJSON: lexical error: invalid char in json text.\n                                  [1,2,zz]\n                     (right here) ------^\n",
        ),
        (
            "[1 2]",
            DbFieldType::Long,
            None,
            "dbConvertJSON: parse error: after array element, I expect ',' or ']'\n                                    [1 2]\n                     (right here) ------^\n",
        ),
        (
            "[1",
            DbFieldType::Long,
            None,
            "dbConvertJSON: parse error: premature EOF\n                                       [1\n                     (right here) ------^\n",
        ),
        (
            "[1] x",
            DbFieldType::Long,
            None,
            "dbConvertJSON: parse error: trailing garbage\n                                   [1] x\n                     (right here) ------^\n",
        ),
        (
            "{}",
            DbFieldType::Long,
            Some("dbConvertJSON: Map type not supported\n"),
            "dbConvertJSON: parse error: client cancelled parse via callback return value\n                                       {}\n                     (right here) ------^\n",
        ),
        (
            "[[1]]",
            DbFieldType::Long,
            Some("dbConvertJSON: Embedded arrays not supported\n"),
            "dbConvertJSON: parse error: client cancelled parse via callback return value\n                                      [[1]]\n                     (right here) ------^\n",
        ),
        (
            "[null]",
            DbFieldType::Long,
            Some("dbConvertJSON: Null objects not supported\n"),
            "dbConvertJSON: parse error: client cancelled parse via callback return value\n                                   [null]\n                     (right here) ------^\n",
        ),
        (
            "[true]",
            DbFieldType::Long,
            Some("dbConvertJSON: Boolean not supported\n"),
            "dbConvertJSON: parse error: client cancelled parse via callback return value\n                                   [true]\n                     (right here) ------^\n",
        ),
        (
            "['a']",
            DbFieldType::Long,
            Some("dbConvertJSON: String \"a\" provided, numeric value expected\n"),
            "dbConvertJSON: parse error: client cancelled parse via callback return value\n                                    ['a']\n                     (right here) ------^\n",
        ),
        (
            "[1e]",
            DbFieldType::Long,
            None,
            "dbConvertJSON: lexical error: malformed number, a digit is required after the exponent.\n                                     [1e]\n                     (right here) ------^\n",
        ),
        (
            "[-]",
            DbFieldType::Long,
            None,
            "dbConvertJSON: lexical error: malformed number, a digit is required after the plus/minus sign.\n                                      [-]\n                     (right here) ------^\n",
        ),
        (
            "[01]",
            DbFieldType::Long,
            None,
            "dbConvertJSON: parse error: after array element, I expect ',' or ']'\n                                     [01]\n                     (right here) ------^\n",
        ),
        (
            "[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,zz]",
            DbFieldType::Long,
            None,
            "dbConvertJSON: lexical error: invalid char in json text.\n          1,12,13,14,15,16,17,18,19,20,zz]\n                     (right here) ------^\n",
        ),
        (
            "['a\\1b']",
            DbFieldType::String,
            None,
            "dbConvertJSON: lexical error: inside a string, '\\' occurs before a character which it may not.\n                                    ['a\\1b']\n                     (right here) ------^\n",
        ),
        (
            "['a\\uZZZZ']",
            DbFieldType::String,
            None,
            "dbConvertJSON: lexical error: invalid (non-hex) character occurs after '\\u' inside string.\n                                   ['a\\uZZZZ']\n                     (right here) ------^\n",
        ),
        (
            "['a\\xZZ']",
            DbFieldType::String,
            None,
            "dbConvertJSON: lexical error: invalid (non-hex) character occurs after '\\x' inside string.\n                                   ['a\\xZZ']\n                     (right here) ------^\n",
        ),
        (
            "['ab",
            DbFieldType::String,
            None,
            "dbConvertJSON: parse error: premature EOF\n                                       ['ab\n                     (right here) ------^\n",
        ),
        (
            "[1e",
            DbFieldType::Long,
            None,
            "dbConvertJSON: lexical error: malformed number, a digit is required after the exponent.\n                                        [1e\n                     (right here) ------^\n",
        ),
        (
            "}",
            DbFieldType::Long,
            None,
            "dbConvertJSON: parse error: unallowed token at this point in JSON text\n                                       }\n                     (right here) ------^\n",
        ),
        (
            "[,1]",
            DbFieldType::Long,
            None,
            "dbConvertJSON: parse error: unallowed token at this point in JSON text\n                                      [,1]\n                     (right here) ------^\n",
        ),
        (
            "[1,,2]",
            DbFieldType::Long,
            None,
            "dbConvertJSON: parse error: unallowed token at this point in JSON text\n                                    [1,,2]\n                     (right here) ------^\n",
        ),
        (
            "[99999999999999999999]",
            DbFieldType::Long,
            None,
            "dbConvertJSON: parse error: integer overflow\n                                       [99999999999999999999]\n                     (right here) ------^\n",
        ),
        (
            "[1e999]",
            DbFieldType::Double,
            None,
            "dbConvertJSON: parse error: numeric (floating point) overflow\n                                       [1e999]\n                     (right here) ------^\n",
        ),
        (
            "[0x]",
            DbFieldType::Long,
            None,
            "dbConvertJSON: lexical error: malformed number, a hex digit is required after the 0x/0X.\n                                     [0x]\n                     (right here) ------^\n",
        ),
        (
            "[/*c",
            DbFieldType::Long,
            None,
            "dbConvertJSON: parse error: premature EOF\n                                       [/*c\n                     (right here) ------^\n",
        ),
        (
            "[//c",
            DbFieldType::Long,
            None,
            "dbConvertJSON: parse error: premature EOF\n                                       [//c\n                     (right here) ------^\n",
        ),
        (
            "{a:1}",
            DbFieldType::Long,
            Some("dbConvertJSON: Map type not supported\n"),
            "dbConvertJSON: parse error: client cancelled parse via callback return value\n                                       {a:1}\n                     (right here) ------^\n",
        ),
        (
            "[tru]",
            DbFieldType::Long,
            None,
            "dbConvertJSON: lexical error: invalid string in json text.\n                                    [tru]\n                     (right here) ------^\n",
        ),
        (
            "[nul]",
            DbFieldType::Long,
            None,
            "dbConvertJSON: lexical error: invalid string in json text.\n                                    [nul]\n                     (right here) ------^\n",
        ),
        (
            "[Inf]",
            DbFieldType::Long,
            None,
            "dbConvertJSON: lexical error: invalid string in json text.\n                                    [Inf]\n                     (right here) ------^\n",
        ),
        (
            "[NaM]",
            DbFieldType::Double,
            None,
            "dbConvertJSON: lexical error: invalid string in json text.\n                                     [NaM]\n                     (right here) ------^\n",
        ),
        (
            "[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,{}]",
            DbFieldType::Long,
            Some("dbConvertJSON: Map type not supported\n"),
            "dbConvertJSON: parse error: client cancelled parse via callback return value\n          1,12,13,14,15,16,17,18,19,20,{}]\n                     (right here) ------^\n",
        ),
        (
            "[",
            DbFieldType::Long,
            None,
            "dbConvertJSON: parse error: premature EOF\n                                       [\n                     (right here) ------^\n",
        ),
    ];

    /// The whole point of the yajl port: byte-for-byte the two errlog records
    /// a C IOC writes. The window and the caret are `hand->bytesConsumed`,
    /// which only a token scan can produce — the `offset < 30` switch is
    /// covered by the two 55-byte cases, whose window is the 10-space form.
    #[test]
    fn the_error_block_is_the_one_softioc_prints() {
        for (json, target, refusal, diagnostic) in C_REFUSALS {
            let e =
                db_put_convert_json(json, *target, 4).expect_err(&format!("C refuses {json:?}"));
            assert_eq!(
                (e.refusal.as_deref(), e.diagnostic.as_str()),
                (*refusal, *diagnostic),
                "{json:?}"
            );
        }
    }

    /// `Display` is what a caller with one output slot gets; the two-record
    /// shape is what `dbpf` uses, because C makes two `errlogPrintf` calls.
    #[test]
    fn a_callback_refusal_carries_both_records() {
        let e = db_put_convert_json("[null]", DbFieldType::Long, 4).unwrap_err();
        assert_eq!(
            e.refusal.as_deref(),
            Some("dbConvertJSON: Null objects not supported\n")
        );
        // Concatenated, not joined: both records already carry C's newline.
        assert_eq!(
            e.to_string(),
            format!("{}{}", e.refusal.unwrap(), e.diagnostic)
        );

        // A lexical failure reaches no callback, so there is only one record.
        let e = db_put_convert_json("[zz]", DbFieldType::Long, 4).unwrap_err();
        assert_eq!(e.refusal, None);
        assert_eq!(e.to_string(), e.diagnostic);
    }

    /// C's overflow tests are `errno == ERANGE` after `epicsStrtod`, and
    /// `yajl_parser.c:334-346` never clears `errno` first. Measured on softIoc
    /// @`R7.0.10`: a fresh IOC accepts `[Infinity]`, and after ONE `[1e999]`
    /// anywhere in the process it refuses every Infinity literal for the rest
    /// of that IOC's life — `[NaN]` keeps working, because `nan != HUGE_VAL`.
    /// That is an upstream defect, not a contract, and it is not ported: this
    /// converter decides on the literal in front of it and nothing else.
    #[test]
    fn overflow_is_decided_by_the_literal_not_by_history() {
        let real = |j| db_put_convert_json(j, DbFieldType::Double, 4);
        assert!(real("[1e999]").is_err());
        assert_eq!(
            real("[Infinity]").unwrap(),
            EpicsValue::DoubleArray(vec![f64::INFINITY])
        );
        // Underflow sets ERANGE too, but C tests the VALUE against ±HUGE_VAL,
        // so a denormal-to-zero literal is accepted on both sides.
        assert_eq!(
            real("[1e-999]").unwrap(),
            EpicsValue::DoubleArray(vec![0.0])
        );
        // The integer row overflows at `LLONG_MAX`, not at the double's range.
        assert!(db_put_convert_json("[9223372036854775808]", DbFieldType::Int64, 1).is_err());
        assert_eq!(
            db_put_convert_json("[9223372036854775807]", DbFieldType::Int64, 1).unwrap(),
            EpicsValue::Int64Array(vec![i64::MAX])
        );
    }

    /// C `yajl_string_decode` (`yajl_encode.c:137-217`). `\x41` is measured
    /// (softIoc printed `"aAb"`); the rest are the arms that share its shape.
    #[test]
    fn escapes_resolve_the_way_yajl_decodes_them() {
        let text = |j: &str| {
            let EpicsValue::StringArray(v) =
                db_put_convert_json(j, DbFieldType::String, 4).unwrap()
            else {
                panic!("a DBF_STRING target yields a StringArray");
            };
            v[0].as_str_lossy().into_owned()
        };
        assert_eq!(text(r"['a\x41b']"), "aAb");
        assert_eq!(text(r"['aBb']"), "aBb");
        assert_eq!(text(r"['a\nb']"), "a\nb");
        assert_eq!(text(r"['a\tb']"), "a\tb");
        // "the character itself" — the default arm is why `\'` and `\/` work
        // without cases of their own (`yajl_encode.c:208-210`).
        assert_eq!(text(r"['a\'b']"), "a'b");
        assert_eq!(text(r"['a\/b']"), "a/b");
        // A backslash-newline line continuation contributes nothing (`:188-192`).
        assert_eq!(text("['a\\\nb']"), "ab");
        // `\0` is the NUL the generator writes for a control character; the
        // truncating `strncpy` in `dbcj_string` is what a caller sees after.
        assert_eq!(text(r"['a\0b']").as_bytes(), b"a\0b");
    }
}
