//! R13-3 — an sCalc string literal is RAW BYTES. The lexer interprets no
//! backslash escape, and `$T` / `TR_ESC` is the only translator.
//!
//! C's `LITERAL_STRING` case is the whole story (`sCalcPostfix.c:803-812`):
//!
//! ```c
//! c = psrc[-1];                                  /* the " or ' that opened it */
//! while (*psrc != c && *psrc) *pout++ = *psrc++; /* byte-for-byte copy */
//! *pout++ = '\0';
//! if (*psrc) psrc++;
//! ```
//!
//! This is *why* sCalc has `TR_ESC`: the literal keeps its backslashes and the
//! expression translates them explicitly, once, where it wants to. The port
//! translated in the lexer, which made `$T` a DOUBLE translation and changed the
//! bytes on every path that never translates at all — including the byte-level
//! idiom a serial-device scalcout uses to build a command.
//!
//! Every expected value below is an output of the compiled upstream
//! `sCalcPostfix.c` + `sCalcPerform.c`.

use epics_base_rs::calc::{CalcError, StackValue, StringInputs, scalc};

fn ev(expr: &str) -> Result<StackValue, CalcError> {
    let mut inputs = StringInputs::new();
    scalc(expr, &mut inputs)
}

fn num(expr: &str) -> f64 {
    match ev(expr).unwrap() {
        StackValue::Double(v) => v,
        other => panic!("{expr}: expected a double, got {other:?}"),
    }
}

fn bytes(expr: &str) -> Vec<u8> {
    match ev(expr).unwrap() {
        StackValue::Str(s) => s.as_bytes().to_vec(),
        other => panic!("{expr}: expected a string, got {other:?}"),
    }
}

/// `BYTE` returns the first byte of its operand. Compiled C: `BYTE("\t")` = 92 —
/// the ASCII code of `\`, the backslash itself. The port answered 9, a TAB,
/// because its lexer had already turned the two characters into one.
#[test]
fn a_backslash_escape_is_not_interpreted_by_the_lexer() {
    assert_eq!(num(r#"BYTE("\t")"#), 92.0);
    assert_eq!(num(r#"BYTE("\n")"#), 92.0);
    assert_eq!(num(r#"BYTE("\\")"#), 92.0);
}

/// The literal's length is the number of SOURCE bytes between the quotes.
/// Compiled C: `LEN("a\tb")` = 4, `LEN("a\nb")` = 4, `LEN("\\\\")` = 4.
#[test]
fn a_literal_keeps_every_source_byte() {
    assert_eq!(num(r#"LEN("a\tb")"#), 4.0);
    assert_eq!(num(r#"LEN("a\nb")"#), 4.0);
    assert_eq!(num(r#"LEN("\\\\")"#), 4.0);
    assert_eq!(num(r#"LEN("a\n")"#), 3.0);
    // Single quotes are the same element (`sCalcPostfix.c:97-98`).
    assert_eq!(num(r#"LEN('a\tb')"#), 4.0);
}

/// The byte-level idiom this actually breaks. A scalcout building a serial
/// command writes `PRINTF("%d\n",5)` and hands the result to the device (or to
/// `$T` first). Compiled C: the result is the THREE bytes `5`, `\`, `n` — a
/// literal backslash and an `n`. The port produced two bytes, `5` and a real LF.
#[test]
fn printf_carries_the_backslash_through_to_its_output() {
    assert_eq!(bytes(r#"PRINTF("%d\n",5)"#), b"5\\n");
    assert_eq!(num(r#"LEN(PRINTF("%d\n",5))"#), 3.0);
}

/// `$T` / `TR_ESC` is the ONE translator, and it now translates exactly once.
/// Compiled C: `TR_ESC("a\tb")` is the 3 bytes `a`, TAB, `b`; and the
/// double-backslash form `TR_ESC("a\\tb")` is the 4 bytes `a`, `\`, `t`, `b`,
/// because `TR_ESC` turns `\\` into a single backslash.
///
/// The old lexer collapsed the escape first, so `$T` ran on already-translated
/// bytes and the two spellings came out swapped.
#[test]
fn tr_esc_is_the_only_translator_and_runs_once() {
    assert_eq!(bytes(r#"TR_ESC("a\tb")"#), b"a\tb");
    assert_eq!(bytes(r#"$T("a\nb")"#), b"a\nb");
    assert_eq!(bytes(r#"TR_ESC("\x41")"#), b"A");

    assert_eq!(bytes(r#"TR_ESC("a\\tb")"#), b"a\\tb");
    assert_eq!(bytes(r#"TR_ESC("\\x41")"#), b"\\x41");
}

/// There is no way to embed the quote character. C's copy loop stops at the first
/// `"`, leaving `b"` behind — `b` is the operand B, and then a second literal
/// opens and runs to the end of the source, so the expression is left with two
/// operands and no operator. Compiled C answers CALC_ERR_SYNTAX (11); the port
/// used to ACCEPT `LEN("a\"b")` and answer 3.
#[test]
fn the_quote_character_cannot_be_escaped() {
    assert_eq!(ev(r#"LEN("a\"b")"#), Err(CalcError::Syntax));
}

/// An unterminated literal is NOT an error: C's loop stops at the NUL and
/// `if (*psrc) psrc++` does nothing. Compiled C: `"abc` compiles and evaluates to
/// `abc`. (`LEN("abc` still fails, but at the `(` — CALC_ERR_PAREN_OPEN, 6 —
/// because the literal swallowed the rest of the source including the `)`.)
#[test]
fn an_unterminated_literal_runs_to_the_end_of_the_source() {
    assert_eq!(bytes(r#""abc"#), b"abc");
    assert_eq!(ev(r#"LEN("abc"#), Err(CalcError::ParenOpen));
}
