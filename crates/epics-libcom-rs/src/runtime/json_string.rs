//! The JSON string decoder a `.db` JSON-brace value goes through — C's yajl.
//!
//! A `.db` field value has two escape regimes, and they are NOT the same
//! function:
//!
//! * a QUOTED value (`field(VAL,"a\tb")`) — `dbLexRoutines.c:1398` strips the
//!   quotes and runs [`crate::runtime::epics_string::raw_from_escaped`]
//!   (C `dbTranslateEscape` → `epicsStrnRawFromEscaped`);
//! * a JSON-BRACE value (`field(INP,{const:"a\tb"})`) — `dbLexRoutines.c:1398`
//!   deliberately leaves it alone (the `if` tests for a leading quote), because
//!   the text is handed to `dbParseLink` → `dbJLinkInit` → `dbJLinkParse`, which
//!   feeds it to **yajl**; yajl's lexer flags a string with escapes and
//!   `yajl_parser.c:273-281` decodes it through `yajl_string_decode`
//!   (`yajl_encode.c:136-215`) before the jlif `string` callback ever sees it
//!   (`lnkConst.c:199`).
//!
//! The port carried the brace text verbatim all the way into the record, so
//! every JSON link carrying an escaped string — `{const:"…"}` seeds,
//! `{pva:{pv:"…"}}` targets — reached the record with its backslashes intact.
//!
//! Measured, softIoc 7.0.10 (linux-x86_64), `dbgf` (which re-escapes for
//! display, so a stored backslash prints DOUBLED):
//!
//! ```text
//! record(stringin,"J1"){field(DTYP,"Soft Channel") field(INP,{const:"a\tb"})}
//!     dbgf J1.VAL -> "a\tb"    i.e. stored a, TAB, b
//! record(stringin,"J2"){field(DTYP,"Soft Channel") field(INP,{const:"x\\ny"})}
//!     dbgf J2.VAL -> "x\\ny"   i.e. stored x, BACKSLASH, n, y
//! ```
//!
//! (`caget`/`caget-rs` are NOT oracles here: both print a DBF_STRING through
//! `epicsStrnEscapedFromRaw` — `tool_lib.c:135`, `cli.rs:834` — so a stored TAB
//! comes back on screen as `\t` and reads exactly like an untranslated escape.)

/// C `yajl_string_decode` (`yajl_encode.c:136-215`): decode the CONTENT of a
/// JSON string (no surrounding quotes).
///
/// Base's yajl is the JSON5-extended one, so beyond the standard escapes it
/// takes `\v`, `\0`, `\xHH`, and a backslash-newline line continuation. An
/// unknown escape yields the escaped character itself (yajl's `default` arm),
/// which is what makes `\"`, `\'` and `\/` work without their own cases.
///
/// KNOWN DEVIATION (R19-68, tracked separately): `\xHH` with `HH >= 0x80` is
/// ONE byte in C (`utf8Buf[0] = (char) codepoint`) and two UTF-8 bytes here,
/// because this returns a `String`. That is the same byte-model gap
/// [`crate::runtime::epics_string`] documents, and it is fixed in one place for
/// both decoders, not here.
pub fn decode_json_string(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;

    while i < b.len() {
        if b[i] != b'\\' {
            // Copy the next whole UTF-8 char (the source is a Rust str, so the
            // byte at `i` starts one).
            let start = i;
            i += 1;
            while i < b.len() && (b[i] & 0xC0) == 0x80 {
                i += 1;
            }
            out.push_str(&src[start..i]);
            continue;
        }
        i += 1;
        let Some(&c) = b.get(i) else { break };
        i += 1;
        match c {
            b'r' => out.push('\r'),
            b'n' => out.push('\n'),
            b'\\' => out.push('\\'),
            b'f' => out.push('\u{c}'),
            b'b' => out.push('\u{8}'),
            b't' => out.push('\t'),
            b'v' => out.push('\u{b}'),
            b'0' => out.push('\0'),
            // A backslash-newline is a line continuation: it contributes
            // nothing (yajl `case '\n'` / `case '\r'`).
            b'\n' => {}
            b'\r' => {
                if b.get(i) == Some(&b'\n') {
                    i += 1;
                }
            }
            b'u' => {
                let Some(cp) = hex_to_digit(b, i, 4) else {
                    // yajl's lexer rejects a short \u; the decoder is never
                    // reached. Keep the text rather than inventing a char.
                    out.push('u');
                    continue;
                };
                i += 4;
                // A high surrogate takes its low partner, exactly as yajl does.
                let ch = if (0xD800..0xDC00).contains(&cp) {
                    match (b.get(i), b.get(i + 1), hex_to_digit(b, i + 2, 4)) {
                        (Some(b'\\'), Some(b'u'), Some(lo)) if (0xDC00..0xE000).contains(&lo) => {
                            i += 6;
                            let scalar = 0x1_0000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                            char::from_u32(scalar)
                        }
                        _ => None,
                    }
                } else {
                    char::from_u32(cp)
                };
                // yajl emits '?' for an unpaired surrogate.
                out.push(ch.unwrap_or('?'));
            }
            b'x' => {
                let Some(byte) = hex_to_digit(b, i, 2) else {
                    out.push('x');
                    continue;
                };
                i += 2;
                out.push(byte as u8 as char);
            }
            // yajl's `default`: the escaped character itself — `\"`, `\'`, `\/`.
            other => out.push(other as char),
        }
    }
    out
}

/// A JSON string TOKEN — quotes included — decoded. `None` when `tok` is not a
/// quoted string (a number, `true`, an object, …): C's jlif `string` callback
/// is simply not the one yajl calls for those.
///
/// This is the ONE place a `.db` JSON string is turned into text. Every site
/// that used to strip the quotes by hand went through the escapes untranslated.
///
/// The token is DOUBLE-quoted, always. The dialect's single-quoted spelling
/// (`yajl_lex.c:695-699`) is rewritten by `epics_base_rs::json5`, which owns
/// every dialect form; accepting it a second time here would put two owners
/// back on the same grammar.
pub fn decode_json_string_token(tok: &str) -> Option<String> {
    let t = tok.trim();
    let inner = t.strip_prefix('"').and_then(|r| r.strip_suffix('"'))?;
    Some(decode_json_string(inner))
}

/// `hexToDigit(&cp, n, src)` — `n` hex digits, or `None` if they are not there.
fn hex_to_digit(b: &[u8], at: usize, n: usize) -> Option<u32> {
    let mut v: u32 = 0;
    for k in 0..n {
        let d = (*b.get(at + k)? as char).to_digit(16)?;
        v = v << 4 | d;
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The escapes yajl names, one case each.
    #[test]
    fn yajl_escapes() {
        assert_eq!(decode_json_string(r"a\tb"), "a\tb");
        assert_eq!(decode_json_string(r"a\nb"), "a\nb");
        assert_eq!(decode_json_string(r"a\rb"), "a\rb");
        assert_eq!(decode_json_string(r"a\fb"), "a\u{c}b");
        assert_eq!(decode_json_string(r"a\bb"), "a\u{8}b");
        assert_eq!(decode_json_string(r"a\vb"), "a\u{b}b");
        assert_eq!(decode_json_string(r"x\\ny"), "x\\ny");
        assert_eq!(decode_json_string(r#"a\"b"#), "a\"b");
        assert_eq!(decode_json_string(r"a\0b"), "a\0b");
        assert_eq!(decode_json_string(r"a\x41b"), "aAb");
        assert_eq!(decode_json_string(r"aAb"), "aAb");
        // An unknown escape is the character itself (yajl's `default`).
        assert_eq!(decode_json_string(r"a\/b"), "a/b");
        assert_eq!(decode_json_string(r"a\qb"), "aqb");
    }

    /// A backslash-newline is a line continuation, not a character.
    #[test]
    fn line_continuation_contributes_nothing() {
        assert_eq!(decode_json_string("a\\\nb"), "ab");
        assert_eq!(decode_json_string("a\\\r\nb"), "ab");
    }

    /// A surrogate pair is one scalar; an unpaired high surrogate is yajl's `?`.
    #[test]
    fn unicode_escapes_and_surrogate_pairs() {
        assert_eq!(decode_json_string("\\u0041"), "A");
        assert_eq!(decode_json_string("\\uD83D\\uDE00"), "\u{1F600}");
        assert_eq!(decode_json_string("\\uD83Dx"), "?x");
    }

    /// Text with no escapes is untouched — the common case must not be mangled.
    #[test]
    fn plain_text_is_untouched() {
        assert_eq!(decode_json_string("hi there"), "hi there");
        assert_eq!(decode_json_string("REC:AI.VAL"), "REC:AI.VAL");
    }

    /// The token form: only a quoted string is a string.
    #[test]
    fn only_a_quoted_token_is_a_string() {
        assert_eq!(
            decode_json_string_token(r#""a\tb""#).as_deref(),
            Some("a\tb")
        );
        // Single-quoted text never reaches here: `json5::relaxed_to_strict`
        // has already rewritten it to the double-quoted spelling.
        assert_eq!(decode_json_string_token(r"'a\tb'"), None);
        assert_eq!(decode_json_string_token("5"), None);
        assert_eq!(decode_json_string_token("true"), None);
        assert_eq!(decode_json_string_token("{a:1}"), None);
    }
}
