//! The one place EPICS's relaxed-JSON dialect is turned into strict JSON.
//!
//! Every JSON text an IOC reads — `.db` link bodies, channel-filter suffixes
//! and QSRV group definitions alike — is parsed by base's bundled yajl, and
//! `yajl_alloc` hands back a handle whose flags are already
//! `yajl_allow_json5 | yajl_allow_comments` (`modules/libcom/src/yajl/yajl.c:77`).
//! No caller turns them off: `dbJLinkParse` calls `yajl_alloc` and goes straight
//! to `yajl_parse` (`dbJLink.c:402-406`), and pvxs's group-config parser calls
//! the same `yajl_alloc` and then re-enables comments redundantly
//! (`ioc/groupconfigprocessor.cpp:825-828`). So JSON5 is not an optional
//! extension in EPICS, it is the default dialect, and base's own documentation
//! writes links that way — `links.dbd.pod:165` gives the canonical calc example
//! as `{calc: {expr:"A*B", args:[{pva:"record"}, 1.5], prec:3}}`, with unquoted
//! keys throughout, while pvxs ships group databases written with comments and
//! bare `+channel:` keys (`test/batch.db:5`, `iocBoot/iocimagedemo/image.json:1`).
//!
//! `serde_json` is strict, so anything in this workspace that hands EPICS JSON
//! to `serde_json` must come through here first. Two partial readers of this one
//! grammar is what made the documented calc-link form unparseable while the
//! channel filters accepted their documented form, and later made comments load
//! in group files but not in link bodies. There is one dialect, so there is one
//! reader.

use std::fmt;

/// The only way a relaxed-JSON text can fail *this* pass. Everything else —
/// unbalanced braces, a bad number, a missing value — is left for `serde_json`
/// to diagnose, because this pass is not a parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Json5Error {
    /// A `/*` with no closing `*/`. Byte offset of the opening `/`.
    ///
    /// yajl reports this as `premature EOF` when the comment interrupts a
    /// document (measured against `yajl_alloc` defaults: `{ "a":1 /* x` and
    /// `{ "a":1 // x` both fail). See the crate's parity note in
    /// `relaxed_to_strict` for the one case where yajl is laxer than this.
    UnterminatedBlockComment { offset: usize },
}

impl fmt::Display for Json5Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnterminatedBlockComment { offset } => {
                write!(f, "unterminated block comment '/*' at byte {offset}")
            }
        }
    }
}

impl std::error::Error for Json5Error {}

/// Rewrite the relaxed JSON that EPICS accepts into the strict JSON
/// `serde_json` parses.
///
/// The acceptance set is yajl's under the flags `dbJLinkParse` passes, and it
/// is closed over base's own GENERATOR: every token `yajl_gen` can write with
/// `yajl_gen_json5` set is a token this reads back. Each form below names the
/// yajl line that accepts it.
///
/// Structure and whitespace:
///
///   * **line and block comments**, which yajl treats as whitespace and which
///     therefore SEPARATE the tokens around them;
///   * **unquoted identifier keys** — a bareword in key position is quoted,
///     while a bareword in value position (`true` / `false` / `null`) is left
///     alone (`yajl_lex.c:854-856`, and `yajl_gen.c:273-285` emits keys this
///     way whenever `yajl_string_validate_identifier` passes);
///   * **unquoted `+`-prefixed keys**, the QSRV group option spelling
///     (`+channel`, `+type`, `+trigger`, `+putorder`);
///   * **trailing commas** before `]` and `}` (`yajl_parser.c:357-362` on
///     `yajl_state_array_need_val`, `:430-434` on `yajl_state_map_need_key`).
///
/// Strings — one double-quoted strict token comes out, whichever quote went in:
///
///   * **single-quoted strings** (`yajl_lex.c:695-699`, where `'` falls
///     through into the same `yajl_lex_string` call as `"`);
///   * the NUL escape (`yajl_encode.c:193-197`) and the vertical-tab escape
///     (`:198`), which the generator writes for those two control characters
///     (`yajl_encode.c:44,71,76`), re-spelled `\u0000` and `\u000B`;
///   * `\xNN` (`yajl_lex.c:340-352`, decoded at `yajl_encode.c:200-207`),
///     re-spelled `\u00NN`;
///   * a backslash-newline **line continuation** (`yajl_encode.c:188-192`),
///     which contributes nothing;
///   * any other escaped character, which yajl's `default` arm
///     (`yajl_encode.c:208-210`) resolves to the character itself — that is
///     what makes `\'` and `\/` work without cases of their own.
///
/// Numbers:
///
///   * **hex integers** `0x…` / `0X…` (`yajl_lex.c:467-468`, converted at base
///     16 by `yajl_parse_integer`, `yajl_parser.c:57-62`), emitted decimal;
///   * a **leading `+`** (`yajl_lex.c:438`), dropped;
///   * a **leading `.`** (`yajl_lex.c:478`) and a **trailing `.`** (allowed
///     because `yajl_lex.c:491` keeps the integer digit count in JSON5 mode),
///     each given the `0` strict JSON requires;
///   * `NaN` (`yajl_lex.c:684-693`), `Infinity` (`:673-682`) and the signed
///     spellings `+Infinity` / `-Infinity` (`:438`, `:443-459`) — which is
///     exactly what `yajl_gen_double` writes for a non-finite double
///     (`yajl_gen.c:228-232`).
///
/// Three deviations from yajl, all measured against base's own sources rather
/// than inferred:
///
///   * yajl REJECTS a bare `+ident` key — `+` opens a number token
///     (`yajl_lex.c:702`) and the identifier start set is `$ _ A-Z a-z`
///     (`yajl_lex.c:856`), so `{+channel:"VAL"}` is `invalid char in json
///     text`. pvxs nonetheless ships databases written that way
///     (`test/batch.db:5`, `test/testpvalink.db:161`), which only its
///     pre-`EPICS_YAJL_VERSION` branch (`groupconfigprocessor.cpp:820-825`)
///     can be reading. This pass accepts them, so it is a superset here — and
///     for the same reason it accepts `NaN`/`Infinity` in KEY position, which
///     yajl lexes as `yajl_tok_double` and its map-key state then refuses.
///   * yajl ACCEPTS an unterminated `/*` that begins after a complete
///     top-level value (`{"a":1} /* x` parses), because nothing examines
///     trailing bytes once the document is closed. This pass runs before any
///     parse and so cannot know the document ended; it rejects uniformly. It
///     is a subset there.
///   * **LOSSY, and the only lossy rewrite here:** a non-finite number becomes
///     `null`. `serde_json::Value` has no non-finite variant and strict JSON
///     has no spelling for one — `1e999` is `number out of range` — so `null`
///     is the token `serde_json`'s own serialiser writes for `f64::NAN`, and
///     using it keeps every text the generator can emit READABLE. The value
///     does not survive; a consumer that needs the distinction must read the
///     text before this pass. The one consumer where that costs parity is the
///     channel-filter parser: C accepts a non-finite filter argument all the
///     way to the option (`yajl_lex.c:673-682`/`:684-693` lex it as
///     `yajl_tok_double`, `yajl_parser.c:329-353` converts it without tripping
///     the `ERANGE` guard, and `store_double_value` writes the raw double with
///     no finite check, `chfPlugin.c:206-209`), so `{"dbnd":{"d":Infinity}}`
///     builds a channel there and is refused here. That refusal is a recorded
///     deviation, not an oversight — reversing it needs a second reader for
///     that one path, which is the split this module exists to close. Pinned
///     by `filters::parser`'s
///     `a_non_finite_filter_argument_is_refused_where_c_accepts_it`.
///
/// Callers get strict JSON and nothing else. No call site dequotes, unescapes
/// or re-spells a dialect token for itself: this replaces the earlier split
/// where the link arms stripped their own quotes, which is what left
/// `{calc:{expr:'A+5'}}` — base's OWN shipped test database, at
/// `modules/database/test/std/rec/linkRetargetLink.db:20-23` — reaching
/// `serde_json` unchanged and being rejected.
///
/// Comments are removed first and the rest rewritten second, as two passes
/// rather than one state machine, because a comment is whitespace:
/// `{expr/*c*/:1}` puts one between a key and its `:`, and a single pass that
/// scanned for the `:` while also consuming comments would have to
/// special-case that. Removing them first makes the key rule uniform.
pub fn relaxed_to_strict(src: &str) -> Result<String, Json5Error> {
    Ok(rewrite_tokens(&strip_comments(src)?))
}

/// Replace every comment with a single space, leaving string literals alone.
///
/// One space, not nothing: yajl treats a comment as a run of whitespace, so it
/// separates the tokens it stands between. Deleting `/*x*/` from `{a:1/*x*/2}`
/// would splice `1` and `2` into the single token `12`, turning text yajl
/// rejects into a different valid document.
fn strip_comments(src: &str) -> Result<String, Json5Error> {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            quote @ (b'"' | b'\'') => {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                // `i` can overshoot on a trailing backslash; clamp so the slice
                // stays on a char boundary and the unterminated literal is
                // handed to `serde_json` verbatim, which is what diagnoses it.
                let end = i.min(bytes.len());
                out.push_str(&src[start..end]);
                i = end;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                let opened = i;
                i += 2;
                let closed = loop {
                    if i + 1 >= bytes.len() {
                        break false;
                    }
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break true;
                    }
                    i += 1;
                };
                if !closed {
                    return Err(Json5Error::UnterminatedBlockComment { offset: opened });
                }
                out.push(' ');
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                out.push(' ');
            }
            _ => {
                // Step by whole characters so multi-byte UTF-8 inside an
                // unquoted region is copied intact.
                let ch = src[i..].chars().next().expect("index is a char boundary");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    Ok(out)
}

/// Walk comment-free relaxed JSON token by token, emitting the strict spelling
/// of each. Structural characters, whitespace and anything unrecognised are
/// copied through: this is a rewriter, not a validator, and a text yajl would
/// reject must still reach `serde_json` for the diagnosis.
fn rewrite_tokens(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len() + 16);
    // Byte offset in `out` of a `,` with nothing but whitespace emitted after
    // it, i.e. one that is still a candidate trailing comma. Cleared by any
    // other token.
    let mut pending_comma: Option<usize> = None;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            out.push(c as char);
            i += 1;
            continue;
        }
        match c {
            b',' => {
                pending_comma = Some(out.len());
                out.push(',');
                i += 1;
            }
            b'}' | b']' => {
                // JSON5 lets the last element carry a comma
                // (`yajl_parser.c:357-362`, `:430-434`); strict JSON does not.
                if let Some(at) = pending_comma.take() {
                    out.remove(at);
                }
                out.push(c as char);
                i += 1;
            }
            b'"' | b'\'' => {
                pending_comma = None;
                i = push_string(&mut out, src, i);
            }
            // `+` opens a number token in yajl, but the QSRV option keys are
            // written `+channel:`; the identifier after it decides which.
            b'+' if b.get(i + 1).copied().is_some_and(is_ident_start_byte) => {
                pending_comma = None;
                i = push_word(&mut out, src, i);
            }
            b'0'..=b'9' | b'+' | b'-' | b'.' => {
                pending_comma = None;
                i = push_number(&mut out, src, i);
            }
            _ if is_ident_start_byte(c) => {
                pending_comma = None;
                i = push_word(&mut out, src, i);
            }
            _ => {
                pending_comma = None;
                let ch = src[i..].chars().next().expect("index is a char boundary");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// Emit one string literal as a strict double-quoted token, whichever quote
/// opened it, and return the offset just past its closing quote.
///
/// The two quote styles do not share an escape set: inside `'…'` a bare `"` is
/// payload and `\'` is an escape, so a delimiter swap alone would corrupt both.
fn push_string(out: &mut String, src: &str, start: usize) -> usize {
    let b = src.as_bytes();
    let quote = b[start];
    let mut i = start + 1;
    out.push('"');
    while i < b.len() {
        match b[i] {
            b'\\' => {
                i += 1;
                let Some(esc) = src[i..].chars().next() else {
                    // Trailing backslash: the literal never closes. Hand the
                    // backslash on and let `serde_json` say so.
                    out.push('\\');
                    return b.len();
                };
                i += esc.len_utf8();
                i = push_escape(out, src, esc, i);
            }
            q if q == quote => {
                out.push('"');
                return i + 1;
            }
            b'"' => {
                // Only reachable inside `'…'`, where a bare `"` is payload.
                out.push_str("\\\"");
                i += 1;
            }
            _ => {
                let ch = src[i..].chars().next().expect("index is a char boundary");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    // Unterminated: no closing quote to emit, so `serde_json` reports the EOF.
    i
}

/// Emit the strict spelling of one escape. `i` is the offset just past the
/// escaped character; the return value is the offset just past whatever else
/// the escape consumed.
fn push_escape(out: &mut String, src: &str, esc: char, mut i: usize) -> usize {
    let b = src.as_bytes();
    match esc {
        // Spelled the same in both dialects.
        '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' => {
            out.push('\\');
            out.push(esc);
        }
        'u' => {
            out.push_str("\\u");
            let mut n = 0;
            while n < 4 && b.get(i).copied().is_some_and(|c| c.is_ascii_hexdigit()) {
                out.push(b[i] as char);
                i += 1;
                n += 1;
            }
        }
        // `yajl_encode.c:200-207`: `\xNN` is the single BYTE 0xNN. `\u00NN` is
        // the same character for NN < 0x80, which is the whole range base's own
        // generator writes (`yajl_encode.c:80-84` escapes control characters
        // only). NN >= 0x80 is the byte-model gap `runtime::json_string`
        // already records: one byte in C, one codepoint here.
        'x' => {
            let hi = b.get(i).copied().filter(|c| c.is_ascii_hexdigit());
            let lo = b.get(i + 1).copied().filter(|c| c.is_ascii_hexdigit());
            match (hi, lo) {
                (Some(hi), Some(lo)) => {
                    out.push_str("\\u00");
                    out.push(hi as char);
                    out.push(lo as char);
                    i += 2;
                }
                // Short `\xN`: yajl's lexer rejects it (`yajl_lex.c:346-350`),
                // so leave it malformed for `serde_json` too.
                _ => out.push_str("\\x"),
            }
        }
        // `yajl_encode.c:193-197` and `:198`. Strict JSON has neither escape.
        '0' => out.push_str("\\u0000"),
        'v' => out.push_str("\\u000B"),
        // `yajl_encode.c:188-192`: a line continuation contributes nothing.
        '\n' => {}
        '\r' => {
            if b.get(i) == Some(&b'\n') {
                i += 1;
            }
        }
        // yajl REJECTS these even in JSON5 (`yajl_lex.c:354-355`); keep them
        // malformed so `serde_json` rejects them too.
        '1'..='9' => {
            out.push('\\');
            out.push(esc);
        }
        // yajl's `default` arm (`yajl_encode.c:208-210`): the character itself.
        // That is how `\'` becomes a bare `'`, which needs no escape here.
        other => push_json_char(out, other),
    }
    i
}

/// Append one decoded character in its strict-JSON spelling.
fn push_json_char(out: &mut String, c: char) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    match c {
        '"' => out.push_str("\\\""),
        '\\' => out.push_str("\\\\"),
        c if (c as u32) < 0x20 => {
            out.push_str("\\u00");
            out.push(HEX[(c as usize) >> 4] as char);
            out.push(HEX[(c as usize) & 0xf] as char);
        }
        c => out.push(c),
    }
}

/// Emit an identifier token: a bareword key (quoted), a JSON keyword (verbatim)
/// or one of the JSON5 special numbers (`null`).
///
/// `start` may be the `+` of a QSRV option key. Key position is decided the way
/// yajl's parser decides it, by the `:` that follows.
fn push_word(out: &mut String, src: &str, start: usize) -> usize {
    let b = src.as_bytes();
    let mut i = start;
    if b[i] == b'+' {
        i += 1;
    }
    let body_start = i;
    while i < b.len() && is_ident_continue_byte(b[i]) {
        i += 1;
    }
    let body = &src[body_start..i];
    // Look past whitespace for the `:`, without consuming it: the caller's loop
    // copies that whitespace on the next turn.
    let mut j = i;
    while j < b.len() && b[j].is_ascii_whitespace() {
        j += 1;
    }
    // A lone `+` is a number sign, not a key.
    if !body.is_empty() && b.get(j) == Some(&b':') {
        out.push('"');
        out.push_str(&src[start..i]);
        out.push('"');
        return i;
    }
    if body == "Infinity" || body == "NaN" {
        out.push_str("null");
        return i;
    }
    out.push_str(&src[start..i]);
    i
}

/// Emit one number token in its strict spelling and return the offset just past
/// it. A token that turns out not to be a number at all — a lone sign, a stray
/// `.` — yields one character so the caller's loop makes progress.
fn push_number(out: &mut String, src: &str, start: usize) -> usize {
    let b = src.as_bytes();
    let mut i = start;
    let negative = b[i] == b'-';
    if negative || b[i] == b'+' {
        i += 1;
    }
    // `-Infinity` / `+Infinity` (`yajl_lex.c:438`, `:443-459`), the spelling
    // `yajl_gen_double` writes for an infinite double (`yajl_gen.c:232`).
    if src[i..].starts_with("Infinity") {
        out.push_str("null");
        return i + "Infinity".len();
    }
    // Hex integer (`yajl_lex.c:467-468`), converted at base 16 by
    // `yajl_parse_integer` (`yajl_parser.c:57-62`). C saturates at
    // `LLONG_MAX`/`LLONG_MIN` with `ERANGE`; a literal too wide for `i128` here
    // is copied through instead, so `serde_json` refuses it rather than this
    // pass inventing a bound.
    if b.get(i) == Some(&b'0') && matches!(b.get(i + 1), Some(b'x' | b'X')) {
        let digits_at = i + 2;
        let mut end = digits_at;
        while b.get(end).copied().is_some_and(|c| c.is_ascii_hexdigit()) {
            end += 1;
        }
        if end > digits_at
            && let Ok(v) = i128::from_str_radix(&src[digits_at..end], 16)
        {
            if negative {
                out.push('-');
            }
            out.push_str(&v.to_string());
            return end;
        }
    }
    let int_at = i;
    while b.get(i).copied().is_some_and(|c| c.is_ascii_digit()) {
        i += 1;
    }
    let int_digits = &src[int_at..i];
    // yajl accepts a missing integer part (`.5`, `yajl_lex.c:478`) and, in
    // JSON5 mode, a missing fraction (`5.`, because `yajl_lex.c:491` keeps the
    // integer digit count); strict JSON wants a digit on both sides of the dot.
    let mut frac: Option<&str> = None;
    if b.get(i) == Some(&b'.') {
        let frac_at = i + 1;
        let mut end = frac_at;
        while b.get(end).copied().is_some_and(|c| c.is_ascii_digit()) {
            end += 1;
        }
        frac = Some(&src[frac_at..end]);
        i = end;
    }
    if int_digits.is_empty() && frac.is_none_or(str::is_empty) {
        let ch = src[start..]
            .chars()
            .next()
            .expect("index is a char boundary");
        out.push(ch);
        return start + ch.len_utf8();
    }
    // The exponent grammar is the same in both dialects (`yajl_lex.c:507-521`),
    // so it is copied verbatim — but only once it is complete, or the `e` of a
    // bareword following a number would be swallowed.
    let exp_at = i;
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut end = i + 1;
        if matches!(b.get(end), Some(b'+' | b'-')) {
            end += 1;
        }
        let digits_at = end;
        while b.get(end).copied().is_some_and(|c| c.is_ascii_digit()) {
            end += 1;
        }
        if end > digits_at {
            i = end;
        }
    }
    if negative {
        out.push('-');
    }
    out.push_str(if int_digits.is_empty() {
        "0"
    } else {
        int_digits
    });
    if let Some(f) = frac {
        out.push('.');
        out.push_str(if f.is_empty() { "0" } else { f });
    }
    out.push_str(&src[exp_at..i]);
    i
}

/// yajl `yajl_lex.c:854-856`: `$`, `_`, `A-Z`, `a-z`.
fn is_ident_start_byte(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'$'
}

/// yajl's `VIC` class (`yajl_lex.c:149-178`): the start set plus `0-9`.
fn is_ident_continue_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

#[cfg(test)]
mod tests {
    use super::{Json5Error, relaxed_to_strict};

    fn strict(src: &str) -> String {
        relaxed_to_strict(src).expect("no unterminated comment in this case")
    }

    /// Every case in this module must land on text `serde_json` reads, which is
    /// the whole point of the pass.
    fn parses(src: &str) -> serde_json::Value {
        let out = strict(src);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{src} -> {out}: {e}"))
    }

    #[test]
    fn strict_json_round_trips_unchanged() {
        let src = r#"{"arr":{"s":2,"i":2,"e":8}}"#;
        assert_eq!(strict(src), src);
    }

    #[test]
    fn string_values_are_not_quoted_again() {
        // Only bareword KEYS are quoted; quoted string values (and the
        // `:` inside them) are preserved verbatim.
        let src = r#"{"sync":{"m":"after","s":"SYS:TRIG"}}"#;
        assert_eq!(strict(src), src);
    }

    /// The documented link form: unquoted keys at both nesting levels.
    /// `links.dbd.pod:165`.
    #[test]
    fn bareword_keys_are_quoted_at_every_depth() {
        assert_eq!(
            strict(r#"{expr:"A*B", args:[3, 1.5], prec:3}"#),
            r#"{"expr":"A*B", "args":[3, 1.5], "prec":3}"#
        );
    }

    /// A bareword in VALUE position is a JSON keyword, not a key, and must
    /// survive unquoted or `serde_json` would read `true` as the string
    /// `"true"`.
    #[test]
    fn bareword_values_are_left_alone() {
        assert_eq!(
            strict(r#"{pipeline:true, x:null}"#),
            r#"{"pipeline":true, "x":null}"#
        );
    }

    /// BOUNDARY: the QSRV option spelling. pvxs `test/batch.db:5`.
    #[test]
    fn plus_prefixed_keys_are_quoted() {
        assert_eq!(
            strict(r#"{+channel:"VAL", +putorder:0}"#),
            r#"{"+channel":"VAL", "+putorder":0}"#
        );
    }

    /// BOUNDARY: a comment is whitespace, so it SEPARATES its neighbours.
    /// Deleting it would splice `1` and `2` into `12`.
    #[test]
    fn a_block_comment_becomes_one_space() {
        assert_eq!(strict(r#"{a:1/*x*/2}"#), r#"{"a":1 2}"#);
    }

    /// BOUNDARY: a comment standing between a key and its `:` must not stop
    /// the key from being recognised — yajl reads `{expr/*c*/:1}` as `expr`.
    #[test]
    fn a_comment_between_a_key_and_its_colon_still_leaves_a_key() {
        assert_eq!(strict(r#"{expr/*c*/:1}"#), r#"{"expr" :1}"#);
    }

    #[test]
    fn a_line_comment_becomes_one_space() {
        assert_eq!(strict("{a:1 // trailing\n, b:2}"), "{\"a\":1  \n, \"b\":2}");
    }

    /// BOUNDARY: comment markers and a leading `+` inside a string literal are
    /// payload, not syntax.
    #[test]
    fn markers_inside_string_literals_survive() {
        let src = r#"{"c":"A/*x*/B//y","i":"+literal"}"#;
        assert_eq!(strict(src), src);
    }

    /// BOUNDARY: unterminated `/*` is an error, not a silent strip to EOF.
    #[test]
    fn an_unterminated_block_comment_is_an_error() {
        assert_eq!(
            relaxed_to_strict(r#"{"a":1 /* never closed"#),
            Err(Json5Error::UnterminatedBlockComment { offset: 7 })
        );
    }

    /// BOUNDARY: an unterminated line comment is not an error — it ends at
    /// EOF, exactly as it ends at a newline.
    #[test]
    fn an_unterminated_line_comment_is_not_an_error() {
        assert_eq!(strict(r#"{"a":1} // never closed"#), r#"{"a":1}  "#);
    }

    // --- single-quoted strings (`yajl_lex.c:695-699`) ----------------------

    /// The highest-value case in the whole acceptance set: base's OWN shipped
    /// test database writes its calc link with a single-quoted expression, at
    /// `modules/database/test/std/rec/linkRetargetLink.db:20-23`.
    #[test]
    fn base_s_own_single_quoted_calc_link_loads() {
        let src = "{calc:{ expr:'A+5', args:5 }}";
        assert_eq!(strict(src), r#"{"calc":{ "expr":"A+5", "args":5 }}"#);
        assert_eq!(parses(src)["calc"]["expr"], "A+5");
    }

    /// BOUNDARY: a single-quoted string is a string. yajl lexes it with the
    /// same routine as a double-quoted one, so a `:` inside it is payload —
    /// rewriting it produced `'"invalid":"pv":name'` from the pva shorthand.
    #[test]
    fn a_colon_inside_a_single_quoted_string_is_not_a_key() {
        assert_eq!(
            strict(r#"{pva: 'invalid:pv:name'}"#),
            r#"{"pva": "invalid:pv:name"}"#
        );
    }

    /// BOUNDARY: the two quote styles carry DIFFERENT escape sets. Inside
    /// `'…'` a bare `"` is payload and `\'` is an escape, so swapping the
    /// delimiters alone corrupts both directions.
    #[test]
    fn the_two_quote_escape_sets_are_translated_not_swapped() {
        assert_eq!(strict(r"{a:'it\'s'}"), r#"{"a":"it's"}"#);
        assert_eq!(strict(r#"{a:'say "hi"'}"#), r#"{"a":"say \"hi\""}"#);
        assert_eq!(parses(r"{a:'it\'s'}")["a"], "it's");
        assert_eq!(parses(r#"{a:'say "hi"'}"#)["a"], "say \"hi\"");
    }

    // --- string escapes the GENERATOR writes -------------------------------

    /// `yajl_encode.c:193-197` and `:198`: JSON5 has `\0` and `\v`, strict JSON
    /// has neither. `format.rs`'s `json_string` writes both.
    #[test]
    fn nul_and_vertical_tab_escapes_become_unicode_escapes() {
        assert_eq!(strict(r#"{a:"x\0y"}"#), r#"{"a":"x\u0000y"}"#);
        assert_eq!(strict(r#"{a:"x\vy"}"#), r#"{"a":"x\u000By"}"#);
        assert_eq!(parses(r#"{a:"x\0y"}"#)["a"], "x\0y");
        assert_eq!(parses(r#"{a:"x\vy"}"#)["a"], "x\u{b}y");
    }

    /// `yajl_lex.c:340-352`, decoded at `yajl_encode.c:200-207`. The generator
    /// writes `\xNN` for every other control character.
    #[test]
    fn hex_byte_escapes_become_unicode_escapes() {
        assert_eq!(strict(r#"{a:"x\x41y"}"#), r#"{"a":"x\u0041y"}"#);
        assert_eq!(parses(r#"{a:"x\x41y"}"#)["a"], "xAy");
        assert_eq!(parses(r#"{a:"\x1B["}"#)["a"], "\u{1b}[");
    }

    /// `yajl_encode.c:188-193`: a backslash before a newline contributes
    /// nothing at all.
    #[test]
    fn a_line_continuation_contributes_nothing() {
        assert_eq!(strict("{a:\"x\\\ny\"}"), r#"{"a":"xy"}"#);
        assert_eq!(strict("{a:\"x\\\r\ny\"}"), r#"{"a":"xy"}"#);
    }

    /// `yajl_encode.c:208-210`: an unknown escape is the character itself,
    /// which is what makes `\'` and `\/` work without cases of their own.
    #[test]
    fn an_unknown_escape_is_the_character_itself() {
        assert_eq!(strict(r#"{a:"x\qy"}"#), r#"{"a":"xqy"}"#);
        assert_eq!(strict(r#"{a:"x\/y"}"#), r#"{"a":"x\/y"}"#);
        assert_eq!(parses(r#"{a:"x\/y"}"#)["a"], "x/y");
    }

    /// A `\u` escape is already strict and must not be re-spelled.
    #[test]
    fn unicode_escapes_pass_through() {
        assert_eq!(
            strict(r#"{a:"\u0041\ud83d\ude00"}"#),
            r#"{"a":"\u0041\ud83d\ude00"}"#
        );
        assert_eq!(parses(r#"{a:"\u0041"}"#)["a"], "A");
    }

    // --- structure ---------------------------------------------------------

    /// `yajl_parser.c:357-362` (`yajl_state_array_need_val`) and `:430-434`
    /// (`yajl_state_map_need_key`).
    #[test]
    fn trailing_commas_are_dropped_before_both_closers() {
        assert_eq!(strict("{a:1,}"), r#"{"a":1}"#);
        assert_eq!(strict("[1,2,]"), "[1,2]");
        // Both commas are trailing: the inner one before `]`, the outer one
        // before `}`.
        assert_eq!(strict("{a:[1,] , }"), r#"{"a":[1]  }"#);
        assert_eq!(parses("{a:[1,2,],}")["a"][1], 2);
    }

    /// BOUNDARY: a comma inside a string is payload, and a comma with a real
    /// value after it stays.
    #[test]
    fn only_a_structural_trailing_comma_is_dropped() {
        assert_eq!(strict(r#"{a:"x,"}"#), r#"{"a":"x,"}"#);
        assert_eq!(strict("[1,2]"), "[1,2]");
    }

    // --- numbers -----------------------------------------------------------

    /// `yajl_lex.c:467-468`, converted at base 16 by `yajl_parse_integer`
    /// (`yajl_parser.c:57-62`).
    #[test]
    fn hex_integers_become_decimal() {
        assert_eq!(strict("{a:0x1F}"), r#"{"a":31}"#);
        assert_eq!(strict("{a:0XfF}"), r#"{"a":255}"#);
        assert_eq!(strict("{a:-0x10}"), r#"{"a":-16}"#);
        assert_eq!(parses("{a:0x1F}")["a"], 31);
    }

    /// BOUNDARY: `yajl_lex.c:438`. The old pass left the `+` in place, which
    /// `serde_json` rejects outright.
    #[test]
    fn a_leading_plus_is_dropped() {
        assert_eq!(strict(r#"{"a":+5}"#), r#"{"a":5}"#);
        assert_eq!(parses(r#"{"a":+5}"#)["a"], 5);
    }

    /// `yajl_lex.c:478` for the missing integer part, `:491` for the missing
    /// fraction; strict JSON wants a digit on both sides of the dot.
    #[test]
    fn a_bare_dot_gets_the_digit_strict_json_wants() {
        assert_eq!(strict("{a:.5}"), r#"{"a":0.5}"#);
        assert_eq!(strict("{a:-.5}"), r#"{"a":-0.5}"#);
        assert_eq!(strict("{a:+.5}"), r#"{"a":0.5}"#);
        assert_eq!(strict("{a:5.}"), r#"{"a":5.0}"#);
        assert_eq!(parses("{a:.5}")["a"], 0.5);
        assert_eq!(parses("{a:5.}")["a"], 5.0);
    }

    /// An exponent is spelled the same in both dialects and must survive, sign
    /// and all — `yajl_gen_double` writes `%.17g`, which produces `1e+30`.
    #[test]
    fn exponents_survive_verbatim() {
        assert_eq!(
            strict("{a:1e+30, b:-2.5E-3}"),
            r#"{"a":1e+30, "b":-2.5E-3}"#
        );
        assert_eq!(parses("{a:1e+30}")["a"], 1e30);
    }

    /// LOSSY, and deliberately so: `serde_json::Value` has no non-finite
    /// variant. `yajl_gen_double` writes these for every non-finite double
    /// (`yajl_gen.c:228-232`), so refusing them would make the generator's own
    /// output unreadable.
    #[test]
    fn non_finite_numbers_become_null() {
        assert_eq!(
            strict("{a:NaN, b:Infinity, c:-Infinity, d:+Infinity}"),
            r#"{"a":null, "b":null, "c":null, "d":null}"#
        );
        assert!(parses("{a:NaN}")["a"].is_null());
    }

    /// BOUNDARY: `Infinity` and `NaN` in KEY position are keys, not numbers.
    /// (yajl refuses them there; this pass is a superset, as it is for
    /// `+ident`.)
    #[test]
    fn infinity_in_key_position_is_a_key() {
        assert_eq!(strict("{NaN:1, Infinity:2}"), r#"{"NaN":1, "Infinity":2}"#);
    }

    /// BOUNDARY: a token that is not a number must not be eaten by the number
    /// lexer — the loop has to make progress and leave the text for
    /// `serde_json` to diagnose.
    #[test]
    fn a_lone_sign_is_copied_through() {
        assert_eq!(strict("{a:-}"), r#"{"a":-}"#);
        assert_eq!(strict("{a:+}"), r#"{"a":+}"#);
        assert_eq!(strict("{a:.}"), r#"{"a":.}"#);
    }
}
