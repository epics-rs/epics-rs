//! C `libCom` `epicsString.c` — the escape-translation half.
//!
//! [`raw_from_escaped`] is `epicsStrnRawFromEscaped` (epicsString.c:49-118),
//! which is also all that the deprecated `dbTranslateEscape` (`:41-47`) does.
//! It is the SINGLE owner of `\`-escape translation, because C has exactly one:
//! every caller that must turn a source-text escape into the byte it denotes
//! goes through it —
//!
//! * the `.db` loader, for field and info VALUES (`dbLexRoutines.c:1398-1403`
//!   and `:1435-1440`, both `dbTranslateEscape(value, value)`), and
//! * iocsh `echo` (`libComRegister.c:84-91`).
//!
//! Record/alias NAMES are deliberately NOT translated: the `.db` lexer reads
//! them with a different rule (`dbLex.l:88-92`), which strips the quotes and
//! keeps the escape bytes raw. Do not route a name through here.
//!
//! Verified against softIoc 7.0.10.1-DEV (`dbgf` re-escapes on print, so these
//! are the STORED bytes):
//!
//! ```text
//! field(DESC, "hex\x41end")  -> hexAend
//! field(VAL,  "d:\q.")       -> d:q.        (unknown escape: the char itself)
//! field(VAL,  "u:A.")   -> u:u0041.    (C has no \u here — the lexer
//!                                            accepts it, the translation does
//!                                            not implement it)
//! ```

/// Translate C escape sequences to the bytes they denote — C
/// `epicsStrnRawFromEscaped` (epicsString.c:49-118).
///
/// * `\a \b \f \n \r \t \v \\ \' \"` — the control/literal character.
/// * `\0` — NUL. This is the literal digit zero, not the start of an octal
///   escape: C has no octal escape here, and its `.db` lexer rejects `\1`..`\9`
///   outright (`escapedchar` is `{backslash}[^ux1-9]`, dbLex.l:25).
/// * `\xH` / `\xHH` — one or two hex digits, the byte they spell. A `\x` NOT
///   followed by a hex digit emits nothing and the offending character is
///   re-examined as ordinary input — so `\x\n` is a newline, exactly as C's
///   `goto input` does.
/// * Any other escaped character — the character itself (`\q` is `q`).
/// * A trailing lone `\` is dropped.
///
/// A `\xHH` denotes ONE byte — including `HH >= 0x80`, where C's `OUT(u)`
/// (`epicsString.c:106`) writes a single `char`. That is why this returns
/// BYTES: a `DBF_STRING` is a byte string with a 40-byte budget, and modelling
/// it as a Rust `String` turned `\xff` into the two UTF-8 bytes of U+00FF.
/// Measured on softIoc — `field(VAL,"h\xffz")` stores the three bytes `h`,
/// `0xFF`, `z` (`dbgf` prints `"h\xffz"`, ONE escape; a two-byte UTF-8
/// encoding would print `"h\xc3\xbfz"`).
pub fn raw_from_escaped(src: &str) -> Vec<u8> {
    raw_from_escaped_bytes(src.as_bytes())
}

/// [`raw_from_escaped`] on bytes — C takes a `char *`, and so does this.
pub fn raw_from_escaped_bytes(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut i = 0;

    while i < src.len() {
        let c = src[i];
        i += 1;

        if c != b'\\' {
            out.push(c);
            continue;
        }

        // A lone trailing backslash: C breaks out of the loop, emitting nothing.
        let Some(&esc) = src.get(i) else { break };
        i += 1;

        match esc {
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'\\' => out.push(b'\\'),
            b'\'' => out.push(b'\''),
            b'"' => out.push(b'"'),
            b'0' => out.push(0),
            b'x' => {
                let mut value: u32 = 0;
                let mut digits = 0;
                while digits < 2 {
                    match src.get(i).and_then(|c| (*c as char).to_digit(16)) {
                        Some(d) => {
                            value = value << 4 | d;
                            digits += 1;
                            i += 1;
                        }
                        None => break,
                    }
                }
                if digits == 0 {
                    // C `goto input`: the `\x` yields nothing and the character
                    // that followed it is re-read as ordinary input — which is
                    // what the outer loop does next, since `i` was not advanced
                    // past it.
                    continue;
                }
                // C `OUT(u)`: ONE byte, whatever the two hex digits spell.
                out.push(value as u8);
            }
            other => out.push(other),
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::raw_from_escaped;

    /// The softIoc transcripts quoted in the module docs.
    #[test]
    fn oracle_cases() {
        assert_eq!(raw_from_escaped("hex\\x41end"), b"hexAend");
        assert_eq!(raw_from_escaped("d:\\q."), b"d:q.");
        assert_eq!(raw_from_escaped("u:\\u0041."), b"u:u0041.");
        assert_eq!(raw_from_escaped("b:\\x4a."), b"b:J.");
        assert_eq!(raw_from_escaped("a \\\"b\\\" c"), b"a \"b\" c");
        assert_eq!(raw_from_escaped("x\\ty"), b"x\ty");
        assert_eq!(raw_from_escaped("sq:\\tx"), b"sq:\tx");
    }

    #[test]
    fn control_escapes() {
        assert_eq!(
            raw_from_escaped("\\a\\b\\f\\n\\r\\t\\v\\\\\\'"),
            b"\x07\x08\x0c\n\r\t\x0b\\'"
        );
        assert_eq!(raw_from_escaped("a\\0b"), b"a\0b");
    }

    /// A single hex digit is enough for the translation (the `.db` lexer
    /// demands two, but `echo` reaches the same function with no lexer).
    #[test]
    fn hex_escape_takes_one_or_two_digits() {
        assert_eq!(raw_from_escaped("\\x41"), b"A");
        assert_eq!(raw_from_escaped("\\x7"), b"\x07");
        assert_eq!(raw_from_escaped("\\x41x"), b"Ax");
    }

    /// R19-68: a `\xHH` at or above 0x80 is ONE byte, as C's `OUT(u)` writes
    /// one `char`. Modelled as a Rust `String` it was the TWO UTF-8 bytes of
    /// the Latin-1 code point, and every DBF_STRING carrying one was wrong on
    /// the wire and against the 40-byte budget.
    ///
    /// softIoc: `record(stringin,"X1"){field(VAL,"h\xffz")}` -> `dbgf X1.VAL`
    /// prints `"h\xffz"` — ONE escape. A two-byte UTF-8 encoding of U+00FF
    /// would print `"h\xc3\xbfz"`.
    #[test]
    fn a_high_hex_escape_is_one_byte() {
        assert_eq!(raw_from_escaped("h\\xffz"), vec![b'h', 0xFF, b'z']);
        assert_eq!(raw_from_escaped("\\x80"), vec![0x80]);
        assert_eq!(raw_from_escaped("\\xc3\\xa9"), vec![0xC3, 0xA9]);
    }

    /// `\x` with no hex digit at all: the `\x` disappears and the next
    /// character is re-read as input — C's `goto input`, so an escape starting
    /// there is still honoured.
    #[test]
    fn hex_escape_without_digits_reexamines_the_next_char() {
        assert_eq!(raw_from_escaped("\\xzz"), b"zz");
        assert_eq!(raw_from_escaped("a\\x\\tb"), b"a\tb");
    }

    #[test]
    fn trailing_backslash_is_dropped() {
        assert_eq!(raw_from_escaped("abc\\"), b"abc");
    }

    #[test]
    fn plain_text_is_untouched() {
        assert_eq!(raw_from_escaped("@asyn(PORT,0)"), b"@asyn(PORT,0)");
    }
}
