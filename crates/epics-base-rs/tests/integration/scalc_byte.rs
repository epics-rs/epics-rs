//! R11-4 — C `BYTE` (`sCalcPerform.c:1528-1533`) is two lines:
//!
//! ```c
//! if (isString(ps)) { ps->d = ps->s[0]; ps->s = NULL; }
//! ```
//!
//! so it reads a `char` — SIGNED on the reference platform — and a double
//! operand passes through the `isString` guard unchanged. The cases below are
//! the sign boundary (0x7f/0x80/0xff), the empty string, and the double
//! passthrough; each was checked against compiled C.

use epics_base_rs::calc::{StackValue, StringInputs, scalc};

fn byte_of(bytes: &[u8]) -> f64 {
    let mut inp = StringInputs::new();
    inp.str_vars[0] = bytes.into();
    match scalc("BYTE(AA)", &mut inp).expect("st=0") {
        StackValue::Double(d) => d,
        StackValue::Str(s) => panic!("BYTE is a double, got {s:?}"),
    }
}

#[test]
fn the_high_bit_makes_the_byte_negative() {
    // Port before R11-4: 255 and 128 (an unsigned read).
    assert_eq!(byte_of(b"\xff"), -1.0);
    assert_eq!(byte_of(b"\x80"), -128.0);
    // The boundary: 0x7f is the last positive one.
    assert_eq!(byte_of(b"\x7f"), 127.0);
    assert_eq!(byte_of(b"A"), 65.0);
}

#[test]
fn the_empty_string_reads_its_nul_terminator() {
    assert_eq!(byte_of(b""), 0.0);
}

#[test]
fn only_the_first_byte_is_read() {
    assert_eq!(byte_of(b"\xffZ"), -1.0);
}

/// C's `if (isString(ps))` has no else: a double operand is left exactly as it
/// is. It is neither an error nor 0.
#[test]
fn a_double_operand_passes_through_unchanged() {
    let mut inp = StringInputs::new();
    // Port before R11-4: 0.0.
    match scalc("BYTE(-3.5)", &mut inp).expect("st=0") {
        StackValue::Double(d) => assert_eq!(d, -3.5),
        StackValue::Str(s) => panic!("BYTE is a double, got {s:?}"),
    }
}
