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
/// A `\xHH` above 0x7F denotes a single byte in C. The `.db` loader models
/// values as Rust `String`, so it is emitted as the Latin-1 code point of that
/// byte; the character is preserved, its UTF-8 encoding is two bytes rather
/// than C's one.
pub fn raw_from_escaped(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        i += 1;

        if c != '\\' {
            out.push(c);
            continue;
        }

        // A lone trailing backslash: C breaks out of the loop, emitting nothing.
        let Some(&esc) = chars.get(i) else { break };
        i += 1;

        match esc {
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'f' => out.push('\x0c'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'v' => out.push('\x0b'),
            '\\' => out.push('\\'),
            '\'' => out.push('\''),
            '"' => out.push('"'),
            '0' => out.push('\0'),
            'x' => {
                let mut value: u32 = 0;
                let mut digits = 0;
                while digits < 2 {
                    match chars.get(i).and_then(|c| c.to_digit(16)) {
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
                out.push(char::from_u32(value).expect("two hex digits are always a code point"));
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
        assert_eq!(raw_from_escaped("hex\\x41end"), "hexAend");
        assert_eq!(raw_from_escaped("d:\\q."), "d:q.");
        assert_eq!(raw_from_escaped("u:\\u0041."), "u:u0041.");
        assert_eq!(raw_from_escaped("b:\\x4a."), "b:J.");
        assert_eq!(raw_from_escaped("a \\\"b\\\" c"), "a \"b\" c");
        assert_eq!(raw_from_escaped("x\\ty"), "x\ty");
        assert_eq!(raw_from_escaped("sq:\\tx"), "sq:\tx");
    }

    #[test]
    fn control_escapes() {
        assert_eq!(
            raw_from_escaped("\\a\\b\\f\\n\\r\\t\\v\\\\\\'"),
            "\x07\x08\x0c\n\r\t\x0b\\'"
        );
        assert_eq!(raw_from_escaped("a\\0b"), "a\0b");
    }

    /// A single hex digit is enough for the translation (the `.db` lexer
    /// demands two, but `echo` reaches the same function with no lexer).
    #[test]
    fn hex_escape_takes_one_or_two_digits() {
        assert_eq!(raw_from_escaped("\\x41"), "A");
        assert_eq!(raw_from_escaped("\\x7"), "\x07");
        assert_eq!(raw_from_escaped("\\x41x"), "Ax");
    }

    /// `\x` with no hex digit at all: the `\x` disappears and the next
    /// character is re-read as input — C's `goto input`, so an escape starting
    /// there is still honoured.
    #[test]
    fn hex_escape_without_digits_reexamines_the_next_char() {
        assert_eq!(raw_from_escaped("\\xzz"), "zz");
        assert_eq!(raw_from_escaped("a\\x\\tb"), "a\tb");
    }

    #[test]
    fn trailing_backslash_is_dropped() {
        assert_eq!(raw_from_escaped("abc\\"), "abc");
    }

    #[test]
    fn plain_text_is_untouched() {
        assert_eq!(raw_from_escaped("@asyn(PORT,0)"), "@asyn(PORT,0)");
    }
}
