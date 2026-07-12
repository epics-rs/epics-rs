//! sCalc's `READ`/`$R` (BIN_READ) and `WRITE`/`$W` (BIN_WRITE) are BINARY
//! operators, not unary ones: their element carries `runtime_effect` -1
//! (sCalcPostfix.c:177-195), exactly like `PRINTF` and `SSCANF`, so each pops
//! two operands and pushes one.
//!
//! The port declared them 1-arg and implemented them as TR_ESC / ESC in
//! disguise (bin_read = translate_escapes, bin_write = escape_string), so
//! `READ(AA)` silently computed an unescape instead of being the compile error
//! C reports, and the real binary conversion did not exist at all.
//!
//! Every expectation is the compiled synApps engine (sCalcPostfix.c +
//! sCalcPerform.c linked against the real epicsString.c, whose
//! `epicsStrnEscapedFromRaw` is what BIN_WRITE's output goes through).

use epics_base_rs::calc::{CalcError, StackValue, StringInputs, scalc};

fn write(fmt: &str, val: f64) -> Result<String, CalcError> {
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = fmt.to_string();
    inputs.num_vars[0] = val;
    match scalc("WRITE(AA,A)", &mut inputs)? {
        StackValue::Str(s) => Ok(s),
        StackValue::Double(d) => panic!("WRITE must produce a string, got {d}"),
    }
}

fn read(subject: &str, fmt: &str) -> Result<f64, CalcError> {
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = subject.to_string();
    inputs.str_vars[1] = fmt.to_string();
    match scalc("READ(AA,BB)", &mut inputs)? {
        StackValue::Double(d) => Ok(d),
        StackValue::Str(s) => panic!("READ must produce a double, got {s:?}"),
    }
}

/// The finding itself. One operand is CALC_ERR_INCOMPLETE (8) in C — "operand
/// missing" — because the element consumes two. The port compiled it happily.
///
/// C spells each of these twice, `READ`/`$R` and `WRITE`/`$W`, mapping to the
/// same BIN_READ/BIN_WRITE code. The `$` aliases are a separate gap (they are
/// in no port table yet, so they answer Syntax rather than Incomplete); when
/// they land they inherit this arity, because arity hangs off the opcode.
#[test]
fn one_operand_is_a_compile_error() {
    let mut inputs = StringInputs::new();
    for expr in ["READ(AA)", "WRITE(AA)"] {
        assert_eq!(
            scalc(expr, &mut inputs),
            Err(CalcError::Incomplete),
            "{expr}: READ/WRITE take two operands, so one is an incomplete expression"
        );
    }
}

/// Compiled C, `WRITE(AA,A)` with AA the format and A the value. The result is
/// the value's raw little-endian bytes, escaped by `epicsStrnEscapedFromRaw`:
/// NUL becomes `\0`, an unprintable byte becomes `\xNN`, a printable byte stays.
#[test]
fn write_lays_out_raw_little_endian_bytes() {
    // 65 as int32 -> 41 00 00 00; 0x41 is 'A'.
    assert_eq!(write("%d", 65.0).unwrap(), r"A\0\0\0");
    // `h` narrows to int16 -> 41 00.
    assert_eq!(write("%hd", 65.0).unwrap(), r"A\0");
    // 1 as int32 -> 01 00 00 00; 0x01 is unprintable.
    assert_eq!(write("%d", 1.0).unwrap(), r"\x01\0\0\0");
    // -1 as int32 -> ff ff ff ff.
    assert_eq!(write("%d", -1.0).unwrap(), r"\xff\xff\xff\xff");
    // 65 as float32 -> 00 00 82 42; 0x42 is 'B'.
    assert_eq!(write("%f", 65.0).unwrap(), r"\0\0\x82B");
    // 1.5 as float32 -> 00 00 c0 3f; 0x3f is '?'.
    assert_eq!(write("%f", 1.5).unwrap(), r"\0\0\xc0?");
    // `l` widens to a full 8-byte double -> 00 00 00 00 00 40 50 40.
    assert_eq!(write("%lf", 65.0).unwrap(), r"\0\0\0\0\0@P@");
    // 'c' is a single byte.
    assert_eq!(write("%c", 65.0).unwrap(), "A");
    // The unsigned conversions are int32-wide too.
    assert_eq!(write("%x", 65.0).unwrap(), r"A\0\0\0");
    assert_eq!(write("%hx", 65.0).unwrap(), r"A\0");
}

/// Compiled C: a format with no conversion character is a runtime failure
/// (`return(-1)`), and so is `%s`.
#[test]
fn write_rejects_a_format_it_cannot_lay_out() {
    assert!(write("abc", 1.0).is_err(), "no conversion character");
    assert!(write("%s", 1.0).is_err(), "C explicitly returns -1 for 's'");
}

/// Compiled C: BIN_READ un-escapes the subject back to raw bytes and reads the
/// field out of them, producing a DOUBLE. These are the exact round-trips the
/// driver reports: READ(WRITE(fmt, v), fmt) == v.
#[test]
fn read_takes_the_field_back_out_of_the_bytes() {
    assert_eq!(read(r"A\0", "%hd").unwrap(), 65.0);
    assert_eq!(read(r"A\0\0\0", "%d").unwrap(), 65.0);
    assert_eq!(read(r"\0\0\xc0?", "%f").unwrap(), 1.5);
    assert_eq!(read(r"\0\0\0\0\0@P@", "%lf").unwrap(), 65.0);
    assert_eq!(read("A", "%c").unwrap(), 65.0);
}

/// The narrow conversions sign-extend and the 4-byte one does NOT, because C's
/// `short h` and `char c` are exactly as wide as their field while `%d` is a
/// 4-byte `memcpy` into `long l = 0L` (sCalcPerform.c:321). Compiled C, subject
/// all-ones:  %hd -1,  %c -1,  %hx 65535,  %d 4294967295,  %x 4294967295.
///
/// So in C a 4-byte `%d` and a 4-byte `%x` are the same read. That is a defect
/// in C, and it is C's answer.
#[test]
fn only_the_narrow_conversions_sign_extend() {
    assert_eq!(read(r"\xff\xff", "%hd").unwrap(), -1.0);
    assert_eq!(read(r"\xff", "%c").unwrap(), -1.0);
    assert_eq!(read(r"\xff\xff", "%hx").unwrap(), 65535.0);
    assert_eq!(read(r"\xff\xff\xff\xff", "%d").unwrap(), 4294967295.0);
    assert_eq!(read(r"\xff\xff\xff\xff", "%x").unwrap(), 4294967295.0);
}

/// The round trip, written the way C's own test does it — as one expression, so
/// the two-operand stack discipline of both ops is exercised together.
#[test]
fn write_then_read_round_trips_in_one_expression() {
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = "%hd".to_string();
    inputs.num_vars[0] = 7.0;
    assert_eq!(
        scalc("READ(WRITE(AA,A),AA)", &mut inputs).unwrap(),
        StackValue::Double(7.0)
    );
}

/// C `findConversionIndicator` skips an assignment-suppressed conversion, and
/// BIN_READ then skips that many BYTES of the subject before reading. `%*2hd`
/// means "step over two 2-byte shorts", so the value read is the third short.
#[test]
fn read_skips_a_suppressed_conversion() {
    // Three int16s: 1, 2, 3.  `%*2hd%hd` steps over the first two.
    assert_eq!(read(r"\x01\0\x02\0\x03\0", "%*2hd%hd").unwrap(), 3.0);
}
