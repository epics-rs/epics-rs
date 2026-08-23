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
/// Covers the JSON5 extensions base's and pvxs's documented grammars actually
/// use:
///
///   * **line and block comments**, which yajl treats as whitespace and which
///     therefore SEPARATE the tokens around them;
///   * **unquoted identifier keys** — a bareword in key position is quoted,
///     while a bareword in value position (`true` / `false` / `null`) is left
///     alone;
///   * **unquoted `+`-prefixed keys**, the QSRV group option spelling
///     (`+channel`, `+type`, `+trigger`, `+putorder`).
///
/// Two deliberate deviations from yajl, both measured against base's own
/// sources compiled standalone rather than inferred:
///
///   * yajl REJECTS a bare `+ident` key — `+` opens a number token
///     (`yajl_lex.c:702`) and the identifier start set is `$ _ A-Z a-z`
///     (`yajl_lex.c:856`), so `{+channel:"VAL"}` is `invalid char in json
///     text`. pvxs nonetheless ships databases written that way
///     (`test/batch.db:5`, `test/testpvalink.db:161`), which only its
///     pre-`EPICS_YAJL_VERSION` branch (`groupconfigprocessor.cpp:820-825`)
///     can be reading. This pass accepts them, so it is a superset here.
///   * yajl ACCEPTS an unterminated `/*` that begins after a complete
///     top-level value (`{"a":1} /* x` parses), because nothing examines
///     trailing bytes once the document is closed. This pass runs before any
///     parse and so cannot know the document ended; it rejects uniformly. It
///     is a subset there.
///
/// Single-quoted strings are RECOGNISED but not rewritten: yajl lexes `'…'`
/// and `"…"` with the same routine (`yajl_lex_string` takes the quote char),
/// so both must be skipped intact or a `:` inside one gets read as structure —
/// which is exactly what broke `{pva: 'invalid:pv:name'}`. They are not
/// converted to double quotes, so a single-quoted string still reaches
/// `serde_json` as-is; the link arms that accept the shorthand strip the
/// quotes themselves.
///
/// Still NOT covered, all accepted by `yajl_allow_json5` and all rejected
/// downstream by `serde_json`: trailing commas, hex literals, leading-`+` and
/// leading-`.` numbers, and `Infinity` / `NaN`.
///
/// Comments are removed first and keys quoted second, as two passes rather
/// than one state machine, because a comment is whitespace: `{expr/*c*/:1}`
/// puts one between a key and its `:`, and a single pass that scanned for the
/// `:` while also consuming comments would have to special-case that. Removing
/// them first makes the key rule uniform.
pub fn relaxed_to_strict(src: &str) -> Result<String, Json5Error> {
    Ok(quote_bare_keys(&strip_comments(src)?))
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

/// Quote every bareword that stands in key position, i.e. whose next
/// non-whitespace character is `:`. Runs on comment-free text.
fn quote_bare_keys(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 16);
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            quote @ ('"' | '\'') => {
                // Copy a string literal verbatim, honouring `\`-escapes so an
                // embedded quote or `:` is never mistaken for structure.
                out.push(quote);
                while let Some(sc) = chars.next() {
                    out.push(sc);
                    if sc == '\\' {
                        if let Some(esc) = chars.next() {
                            out.push(esc);
                        }
                    } else if sc == quote {
                        break;
                    }
                }
            }
            // yajl's identifier start set (`yajl_lex.c:856`), plus the `+` that
            // QSRV group options are written with.
            c if is_ident_start(c) || c == '+' => {
                let mut word = String::new();
                word.push(c);
                while let Some(&pc) = chars.peek() {
                    if is_ident_continue(pc) {
                        word.push(pc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                // Buffer trailing whitespace so a `key :` form still resolves
                // as a key without dropping the spacing.
                let mut ws = String::new();
                while let Some(&pc) = chars.peek() {
                    if pc.is_whitespace() {
                        ws.push(pc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                // A lone `+` is a number sign, not a key; only quote when the
                // bareword has a body AND sits in key position.
                let quotable = chars.peek() == Some(&':') && word.len() > usize::from(c == '+');
                if quotable {
                    out.push('"');
                    out.push_str(&word);
                    out.push('"');
                } else {
                    out.push_str(&word);
                }
                out.push_str(&ws);
            }
            other => out.push(other),
        }
    }
    out
}

/// yajl `yajl_lex.c:856`: `$`, `_`, `A-Z`, `a-z`.
fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

/// yajl's `VIC` class (`yajl_lex.c:149-178`): the start set plus `0-9`.
fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

#[cfg(test)]
mod tests {
    use super::{Json5Error, relaxed_to_strict};

    fn strict(src: &str) -> String {
        relaxed_to_strict(src).expect("no unterminated comment in this case")
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

    /// BOUNDARY: a lone `+` in value position is a number sign
    /// (`yajl_lex.c:438`), not a key, and must not be quoted.
    #[test]
    fn a_plus_sign_before_a_number_is_not_a_key() {
        assert_eq!(strict(r#"{"a":+5}"#), r#"{"a":+5}"#);
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

    /// BOUNDARY: a single-quoted string is a string. yajl lexes it with the
    /// same routine as a double-quoted one (`yajl_lex.c:695-700`), so a `:`
    /// inside it is payload — rewriting it produced `'"invalid":"pv":name'`
    /// from the pva shorthand `{pva: 'invalid:pv:name'}`.
    #[test]
    fn a_colon_inside_a_single_quoted_string_is_not_a_key() {
        let src = r#"{pva: 'invalid:pv:name'}"#;
        assert_eq!(strict(src), r#"{"pva": 'invalid:pv:name'}"#);
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
}
