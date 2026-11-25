//! R10-13 — TR_ESC and ESC are epicsString.c's `dbTranslateEscape` /
//! `epicsStrSnPrintEscaped` (`sCalcPerform.c:1798-1815`), the same two functions
//! BIN_READ and BIN_WRITE use. The port had a second, smaller escape table for
//! these two operators (`\n`, `\t`, `\r`, `\\` and `\xHH` only) and rejected a
//! double operand that C leaves alone.
//!
//! Every expectation is the output of the C block compiled against the real
//! `epicsString.c` on this host.

use epics_base_rs::calc::{StackValue, StringInputs, scalc};

/// `TR_ESC` / `$T` — C's TR_ESC, i.e. `dbTranslateEscape`: escaped text in, raw
/// bytes out.
fn tr_esc(bytes: &[u8]) -> Vec<u8> {
    let mut inp = StringInputs::new();
    inp.str_vars[0] = bytes.into();
    match scalc("TR_ESC(AA)", &mut inp).expect("st=0") {
        StackValue::Str(s) => s.as_bytes().to_vec(),
        StackValue::Double(d) => panic!("expected a string, got {d}"),
    }
}

/// `ESC` / `$E` — C's ESC, i.e. `epicsStrSnPrintEscaped`: the other direction.
fn esc(bytes: &[u8]) -> Vec<u8> {
    let mut inp = StringInputs::new();
    inp.str_vars[0] = bytes.into();
    match scalc("ESC(AA)", &mut inp).expect("st=0") {
        StackValue::Str(s) => s.as_bytes().to_vec(),
        StackValue::Double(d) => panic!("expected a string, got {d}"),
    }
}

/// The escapes the port's table did not have.
#[test]
fn tr_esc_translates_the_whole_epics_string_table() {
    assert_eq!(tr_esc(br"bell:\a"), b"bell:\x07");
    assert_eq!(tr_esc(br"back:\b"), b"back:\x08");
    assert_eq!(tr_esc(br"ff:\f"), b"ff:\x0c");
    assert_eq!(tr_esc(br"vt:\v"), b"vt:\x0b");
    assert_eq!(tr_esc(br"q:\'"), b"q:'");
    assert_eq!(tr_esc(br#"dq:\""#), b"dq:\"");
    // ...and the ones it did.
    assert_eq!(tr_esc(br"a\nb"), b"a\nb");
    assert_eq!(tr_esc(br"a\tb\rc"), b"a\tb\rc");
    assert_eq!(tr_esc(br"bs:\\"), b"bs:\\");
    // An unknown escape yields the character itself.
    assert_eq!(tr_esc(br"unknown:\q"), b"unknown:q");
    // A trailing backslash yields nothing.
    assert_eq!(tr_esc(br"trailing:\"), b"trailing:");
}

/// `\0` produces a NUL, and a NUL ends a C string — so the value stops there.
#[test]
fn a_nul_escape_terminates_the_value() {
    assert_eq!(tr_esc(br"a\0b"), b"a");
}

/// C's `\x` handling goes through `goto input`, which re-enters the loop with the
/// non-hex character — the `x` is swallowed, not emitted.
#[test]
fn the_hex_escape_follows_cs_goto_input() {
    assert_eq!(tr_esc(br"hex:\x41\x7A"), b"hex:Az");
    // One hex digit is enough, and the next character is then ordinary.
    assert_eq!(tr_esc(br"hex1:\xA!"), b"hex1:\x0a!");
    // No hex digit at all: the `x` is gone and the `Z` is literal.
    assert_eq!(tr_esc(br"bad:\xZ"), b"bad:Z");
    // A trailing `\x` ends the translation.
    assert_eq!(tr_esc(br"trail:\x"), b"trail:");
}

#[test]
fn esc_escapes_the_whole_epics_string_table() {
    assert_eq!(esc(b"a\nb"), br"a\nb");
    assert_eq!(esc(b"\x07\x08\x0c\x0b"), br"\a\b\f\v");
    assert_eq!(esc(b"q'\"\\"), br#"q\'\"\\"#);
    // Non-printable bytes become lower-case \xHH.
    assert_eq!(esc(b"\xff\x01"), br"\xff\x01");
}

/// ESC's result is bounded at THIRTY-EIGHT bytes: C passes `SCALC_STRING_SIZE-1`
/// (39) as the destination length, and epicsStrSnPrintEscaped writes at most
/// `dstlen-1` bytes before its NUL.
#[test]
fn esc_is_bounded_one_byte_shorter_than_every_other_string() {
    // 20 newlines escape to 40 bytes; C returns 38.
    assert_eq!(esc(&[b'\n'; 20]).len(), 38);
    // 19 escape to exactly 38 — the boundary, and nothing is lost.
    let out = esc(&[b'\n'; 19]);
    assert_eq!(out.len(), 38);
    assert_eq!(out, br"\n".repeat(19));
}

/// C's `if (isString(ps))` has no else: a double operand passes through both
/// operators untouched. The port raised a type error.
#[test]
fn a_double_operand_is_a_no_op() {
    let mut inp = StringInputs::new();
    match scalc("TR_ESC(2.5)", &mut inp).expect("st=0") {
        StackValue::Double(d) => assert_eq!(d, 2.5),
        StackValue::Str(s) => panic!("expected the double back, got {s:?}"),
    }
    match scalc("ESC(2.5)", &mut inp).expect("st=0") {
        StackValue::Double(d) => assert_eq!(d, 2.5),
        StackValue::Str(s) => panic!("expected the double back, got {s:?}"),
    }
}
