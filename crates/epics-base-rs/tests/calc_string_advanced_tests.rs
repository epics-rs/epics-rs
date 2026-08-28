#![allow(clippy::approx_constant)]

use epics_base_rs::calc::{CalcError, StackValue, StringInputs, scalc};

fn eval_str(expr: &str) -> StackValue {
    let mut inputs = StringInputs::new();
    scalc(expr, &mut inputs).unwrap()
}

// --- TR_ESC / ESC ---

// The calc source carries ONE backslash. The sCalc lexer copies string literals
// raw (`sCalcPostfix.c:803-812`), so `TR_ESC` receives the two bytes `\` `n` and
// is the one and only thing that translates them. These used to be written with
// two backslashes to cancel out a translation the lexer was wrongly doing first
// (R13-3). Compiled C: `TR_ESC("a\tb")` is a TAB, `TR_ESC("a\\tb")` is the
// literal two bytes `\t`.

#[test]
fn test_tr_esc_newline() {
    let result = eval_str(r#"TR_ESC("hello\nworld")"#);
    assert_eq!(result, StackValue::Str("hello\nworld".into()));
}

#[test]
fn test_tr_esc_tab() {
    let result = eval_str(r#"TR_ESC("a\tb")"#);
    assert_eq!(result, StackValue::Str("a\tb".into()));
}

#[test]
fn test_tr_esc_hex() {
    let result = eval_str(r#"TR_ESC("\x41")"#);
    assert_eq!(result, StackValue::Str("A".into()));
}

#[test]
fn test_esc_newline() {
    // Create a string with actual newline, then escape it
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = "hello\nworld".into();
    let result = scalc("ESC(AA)", &mut inputs).unwrap();
    assert_eq!(result, StackValue::Str("hello\\nworld".into()));
}

#[test]
fn test_esc_tab() {
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = "a\tb".into();
    let result = scalc("ESC(AA)", &mut inputs).unwrap();
    assert_eq!(result, StackValue::Str("a\\tb".into()));
}

#[test]
fn test_tr_esc_esc_roundtrip() {
    // TR_ESC(ESC(original)) should preserve content
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = "hello\nworld\t!".into();
    let escaped = scalc("ESC(AA)", &mut inputs).unwrap();
    match escaped {
        StackValue::Str(s) => {
            inputs.str_vars[0] = s;
            let result = scalc("TR_ESC(AA)", &mut inputs).unwrap();
            assert_eq!(result, StackValue::Str("hello\nworld\t!".into()));
        }
        _ => panic!("expected string"),
    }
}

// --- PRINTF ---

#[test]
fn test_printf_int() {
    let result = eval_str(r#"PRINTF("%d", 42)"#);
    assert_eq!(result, StackValue::Str("42".into()));
}

#[test]
fn test_printf_float() {
    let result = eval_str(r#"PRINTF("%.2f", 3.14159)"#);
    assert_eq!(result, StackValue::Str("3.14".into()));
}

#[test]
fn test_printf_string() {
    let result = eval_str(r#"PRINTF("%s", "hello")"#);
    assert_eq!(result, StackValue::Str("hello".into()));
}

#[test]
fn test_printf_hex() {
    let result = eval_str(r#"PRINTF("%x", 255)"#);
    assert_eq!(result, StackValue::Str("ff".into()));
}

#[test]
fn test_printf_hex_upper() {
    let result = eval_str(r#"PRINTF("%X", 255)"#);
    assert_eq!(result, StackValue::Str("FF".into()));
}

/// C's `myNINT` (`sCalcPerform.c:40`) casts to `int` INSIDE the macro:
/// `#define myNINT(a) ((int)((a) >= 0 ? (a)+0.5 : (a)-0.5))`. So every integer
/// conversion sees a value that has ALREADY been narrowed to 32 bits, ONCE,
/// before the call site ever looks at it.
///
/// The bug this test was written for is that the port's `my_nint` returned an
/// `f64`, leaving each call site to invent its own narrowing: PRINTF wrapped
/// (`as i64`) and BIN_WRITE saturated (`as i32`), so the two disagreed BITWISE
/// on the same input. The fix is that there is ONE narrowing, in
/// `types::c_cast`, and every call site sees the same int — which is what the
/// PRINTF/WRITE agreement below actually pins.
///
/// WHICH int is out of range is CBUG-E2's call, not this test's: the `(int)`
/// cast is UB on an out-of-range double, so compiled C answers `INT32_MIN` on
/// x86-64 (`cvttsd2si`) and `INT32_MAX` on aarch64 (`fcvtzs`). The port
/// saturates, so `3e9` is `INT32_MAX` and `-3e9` is `INT32_MIN` — the two
/// out-of-range signs no longer collapse onto one indefinite value.
#[test]
fn test_my_nint_narrows_at_c_width_at_every_call_site() {
    // PRINTF: out of int32 range saturates toward the operand's own sign.
    assert_eq!(
        eval_str(r#"PRINTF("%d", 3e9)"#),
        StackValue::Str("2147483647".into())
    );
    assert_eq!(
        eval_str(r#"PRINTF("%d", -3e9)"#),
        StackValue::Str("-2147483648".into())
    );
    assert_eq!(
        eval_str(r#"PRINTF("%x", 3e9)"#),
        StackValue::Str("7fffffff".into())
    );
    // In range, `myNINT` still rounds half away from zero.
    assert_eq!(
        eval_str(r#"PRINTF("%d", 2.5)"#),
        StackValue::Str("3".into())
    );
    assert_eq!(
        eval_str(r#"PRINTF("%d", -2.5)"#),
        StackValue::Str("-3".into())
    );

    // BIN_WRITE ($W) takes the low bytes of the SAME int — so it must agree with
    // PRINTF bit for bit, which is what having one narrowing buys.
    assert_eq!(
        eval_str(r#"WRITE("%i", 3e9)"#),
        StackValue::Str(r"\xff\xff\xff\x7f".into()),
        "the four little-endian bytes of INT32_MAX"
    );
    assert_eq!(
        eval_str(r#"WRITE("%i", -3e9)"#),
        StackValue::Str(r"\0\0\0\x80".into()),
        "the four little-endian bytes of INT32_MIN, NUL escaped as \\0"
    );
    assert_eq!(
        eval_str(r#"WRITE("%c", 3e9)"#),
        StackValue::Str(r"\xff".into()),
        "the low byte of INT32_MAX"
    );
}

// --- SSCANF ---
//
// Every value below is what compiled sCalc answers (`sCalcPerform.c:1635`,
// `drv "SSCANF('<in>','<fmt>')"`). C hands the user's format to the C library's
// `sscanf` with ONE output object, so the whole of scanf is in scope, and
// `if (i != 1) return(-1)` makes a failed conversion an ERROR — never a 0.

#[test]
fn test_sscanf_int() {
    let result = eval_str(r#"SSCANF("42", "%d")"#);
    assert_eq!(result, StackValue::Double(42.0));
}

/// C's object for a bare `%f` is a `float`, not a double: 3.15 comes back as
/// the f32 rounding of itself. Only `%lf` gets a double.
#[test]
fn test_sscanf_bare_percent_f_is_a_float() {
    assert_eq!(
        eval_str(r#"SSCANF("3.15", "%f")"#),
        StackValue::Double(3.1500000953674316)
    );
    assert_eq!(
        eval_str(r#"SSCANF("3.15", "%lf")"#),
        StackValue::Double(3.15)
    );
}

#[test]
fn test_sscanf_string() {
    let result = eval_str(r#"SSCANF("hello world", "%s")"#);
    assert_eq!(result, StackValue::Str("hello".into()));
}

/// The port used to answer `Double(0.0)` with a healthy record for every
/// conversion its hand-rolled subset did not know, and for every failed one.
#[test]
fn test_sscanf_failed_conversion_is_an_error() {
    let mut inputs = StringInputs::new();
    for expr in [
        r#"SSCANF("abc", "%d")"#,    // no digits
        r#"SSCANF("", "%d")"#,       // empty input
        r#"SSCANF("", "%s")"#,       // empty input
        r#"SSCANF("zab", "%[^z]")"#, // scanset matches nothing
        r#"SSCANF("y=42", "x=%d")"#, // literal mismatch
        r#"SSCANF("abc", "%*d%s")"#, // the SUPPRESSED conversion fails
        r#"SSCANF("abc", "xyz")"#,   // no conversion at all
        r#"SSCANF("abc", "%p")"#,    // C refuses p/w/n/$ outright
    ] {
        assert!(scalc(expr, &mut inputs).is_err(), "{expr} must alarm");
    }
}

/// `%x`/`%o`/`%u`/`%i`/`%c`/`%[` were simply unimplemented — all six answered 0.
#[test]
fn test_sscanf_integer_bases() {
    assert_eq!(eval_str(r#"SSCANF("ff", "%x")"#), StackValue::Double(255.0));
    assert_eq!(
        eval_str(r#"SSCANF("0x1f", "%x")"#),
        StackValue::Double(31.0)
    );
    assert_eq!(eval_str(r#"SSCANF("017", "%o")"#), StackValue::Double(15.0));
    assert_eq!(eval_str(r#"SSCANF("017", "%i")"#), StackValue::Double(15.0));
    assert_eq!(
        eval_str(r#"SSCANF("-0x10", "%i")"#),
        StackValue::Double(-16.0)
    );
}

/// The output object's type is picked from the conversion char and `s[-1]`, so
/// the value is narrowed before it is widened to a double: `unsigned short` for
/// `%h[oux]`, `unsigned int` for a bare one, `unsigned long` for `%l[oux]`.
#[test]
fn test_sscanf_narrows_through_the_output_object() {
    assert_eq!(
        eval_str(r#"SSCANF("-5", "%u")"#),
        StackValue::Double(4294967291.0)
    );
    assert_eq!(
        eval_str(r#"SSCANF("70000", "%hd")"#),
        StackValue::Double(4464.0)
    );
    // `%lx` still parses HEX digits: 0x4294967295.
    assert_eq!(
        eval_str(r#"SSCANF("4294967295", "%lx")"#),
        StackValue::Double(285960729237.0)
    );
}

#[test]
fn test_sscanf_char_and_scanset() {
    // `%c` takes the character as-is — no leading-whitespace skip.
    assert_eq!(
        eval_str(r#"SSCANF("  abc", "%c")"#),
        StackValue::Str(" ".into())
    );
    assert_eq!(
        eval_str(r#"SSCANF("ff12 xy", "%5c")"#),
        StackValue::Str("ff12 ".into())
    );
    assert_eq!(
        eval_str(r#"SSCANF("hello", "%[a-l]")"#),
        StackValue::Str("hell".into())
    );
    assert_eq!(
        eval_str(r#"SSCANF("hello", "%[^l]")"#),
        StackValue::Str("he".into())
    );
    // `[]a-z]`: a leading `]` is a plain member of the set.
    assert_eq!(
        eval_str(r#"SSCANF("ab1", "%*[]a-z]%d")"#),
        StackValue::Double(1.0)
    );
}

/// `findConversionIndicator` (`sCalcPerform.c:105`) decides the format, and it
/// is stricter than scanf: its `%%` skip is GREEDY (it jumps past a `%%` found
/// anywhere ahead, losing any conversion in front of it) and it refuses a
/// second conversion that would be assigned.
#[test]
fn test_sscanf_rejects_a_second_live_conversion() {
    let mut inputs = StringInputs::new();
    assert!(scalc(r#"SSCANF("1 2", "%d%d")"#, &mut inputs).is_err());
    assert!(scalc(r#"SSCANF("abc def", "%s %s")"#, &mut inputs).is_err());
    // A trailing `%%` swallows the `%d` in front of it.
    assert!(scalc(r#"SSCANF("100%", "%d%%")"#, &mut inputs).is_err());

    // Suppressed conversions are fine — there is still only one assignment.
    assert_eq!(
        eval_str(r#"SSCANF("1 2", "%d %*d")"#),
        StackValue::Double(1.0)
    );
    assert_eq!(
        eval_str(r#"SSCANF("1 2", "%*d%d")"#),
        StackValue::Double(2.0)
    );
    // A `%%` AHEAD of the conversion is just a literal `%`.
    assert_eq!(
        eval_str(r#"SSCANF("%42", "%%%d")"#),
        StackValue::Double(42.0)
    );
}

/// BIN_READ shares `findConversionIndicator`, so it inherits both rules — it
/// used to accept `%d%%` and answer 42.
#[test]
fn test_bin_read_shares_the_conversion_scanner() {
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = r"*\0\0\0".into(); // escaped text for the four bytes of 42
    inputs.str_vars[1] = "%d".into();
    assert_eq!(
        scalc("READ(AA,BB)", &mut inputs).unwrap(),
        StackValue::Double(42.0)
    );
    inputs.str_vars[1] = "%d%%".into();
    assert!(scalc("READ(AA,BB)", &mut inputs).is_err());
    inputs.str_vars[1] = "%d %d".into();
    assert!(scalc("READ(AA,BB)", &mut inputs).is_err());
}

// --- CRC16 / MODBUS / XOR8 / ADD_XOR8 ---
//
// The digest is ESCAPED TEXT, not raw bytes. C's helpers end with a literal
// `sprintf(output, "\\x%02x\\x%02x", crc&0xff, (crc&0xff00)>>8)`
// (`sCalcPerform.c:227`, `:281`), so the frame stays escaped all the way to the
// octet layer — which is the thing that translates it. CRC16 and XOR8 REPLACE
// the operand with the digest; MODBUS and ADD_XOR8 APPEND it.

/// Compiled sCalc: `CRC16("123456789")` = `\x37\x4b` — the standard 0x4B37
/// MODBUS CRC, low byte first, as eight characters of escaped text. This test
/// used to pin `Double(0x4B37)`, which is not a value C can produce here.
#[test]
fn test_crc16_is_escaped_text_low_byte_first() {
    assert_eq!(
        eval_str(r#"CRC16("123456789")"#),
        StackValue::Str(r"\x37\x4b".into())
    );
    assert_eq!(
        eval_str(r#"CRC16("AB")"#),
        StackValue::Str(r"\xb1\xd1".into())
    );
}

/// Compiled sCalc: `MODBUS("AB")` = `AB\xb1\xd1`, ten characters.
#[test]
fn test_modbus_appends_the_escaped_crc() {
    assert_eq!(
        eval_str(r#"MODBUS("AB")"#),
        StackValue::Str(r"AB\xb1\xd1".into())
    );
    assert_eq!(eval_str(r#"LEN(MODBUS("AB"))"#), StackValue::Double(10.0));
}

/// Compiled sCalc: `XOR8("AB")` = `\x03`, `ADD_XOR8("AB")` = `AB\x03` (six
/// characters). A PRINTABLE digest byte is escaped too — `XOR8("A")` = `\x41`,
/// never `A` — because C's sprintf is unconditional, not the escape table.
#[test]
fn test_xor8_is_escaped_text() {
    assert_eq!(eval_str(r#"XOR8("AB")"#), StackValue::Str(r"\x03".into()));
    assert_eq!(eval_str(r#"XOR8("A")"#), StackValue::Str(r"\x41".into()));
    assert_eq!(
        eval_str(r#"ADD_XOR8("AB")"#),
        StackValue::Str(r"AB\x03".into())
    );
    assert_eq!(eval_str(r#"LEN(ADD_XOR8("AB"))"#), StackValue::Double(6.0));
}

/// The operand is ESCAPED text: the digest is taken over what `dbTranslateEscape`
/// makes of it (`sCalcPerform.c:198`, `:264`), not over its own characters.
/// Compiled sCalc: `CRC16("\x01\x03")` = `\x40\x21` — a CRC of TWO bytes, not of
/// the eight characters that spell them.
#[test]
fn test_checksum_operand_is_translated_first() {
    assert_eq!(
        eval_str(r#"CRC16("\x01\x03")"#),
        StackValue::Str(r"\x40\x21".into())
    );
    // XOR of the RAW bytes 0x01^0x02^0x03 = 0x00.
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = vec![0x01u8, 0x02, 0x03].into();
    assert_eq!(
        scalc("XOR8(AA)", &mut inputs).unwrap(),
        StackValue::Str(r"\x00".into())
    );
}

/// CBUG-F8 — a byte ≥ 0x80 gets the STANDARD CRC-16/MODBUS, deviating from C.
///
/// C's accumulator is a 32-bit `unsigned int` and its buffer is a SIGNED `char`,
/// so `crc ^= (unsigned int)tranInput[i]` (`sCalcPerform.c:193-212`)
/// sign-extends the byte into bits 16-31, which the eight `crc >>= 1` steps
/// shift back down into the digest. A real Modbus device validates against the
/// standard CRC, so C is the one that is wire-broken on binary frames; the port
/// emits the standard digest. This test previously pinned C's answers — the
/// values it asserted are named below so the deviation is auditable.
///
/// XOR8 masks its own sign-extension away (`:278`) and was never affected.
#[test]
fn test_crc16_high_bytes_are_standard_not_c() {
    assert_eq!(
        eval_str(r#"CRC16("\x80")"#),
        StackValue::Str(r"\xbe\xe0".into()),
        "standard CRC-16/MODBUS; compiled C says \\x41\\x1f"
    );
    assert_eq!(
        eval_str(r#"CRC16("\xff")"#),
        StackValue::Str(r"\xff\x00".into()) // C: \x00\xff
    );
    assert_eq!(
        eval_str(r#"CRC16("\x80\x80")"#),
        StackValue::Str(r"\x61\xd0".into()) // C: \x21\x90
    );
    // A real binary MODBUS frame: the case the operator exists for.
    assert_eq!(
        eval_str(r#"CRC16("\xf7\x03\x13\x89")"#),
        StackValue::Str(r"\x0e\xc6".into()) // C: \xc0\xb9
    );
    // MODBUS appends the same digest, so it carries the same deviation.
    assert_eq!(
        eval_str(r#"MODBUS("\x80")"#),
        StackValue::Str(r"\x80\xbe\xe0".into()) // C: \x80\x41\x1f
    );
    // XOR8 sign-extends too, but `sprintf`'s `xor8&0xff` throws it away.
    assert_eq!(eval_str(r#"XOR8("\x80")"#), StackValue::Str(r"\x80".into()));
}

/// C guards every checksum with `if (isString(ps))` and then with
/// `if (chk(...) == 0)`, and neither guard raises an error: a DOUBLE operand and
/// an operand that translates to nothing are both left EXACTLY as they are, with
/// st=0. The port raised `TypeMismatch` on the first and had no path for the
/// second. Compiled sCalc: `CRC16(4)` = 4, `MODBUS(4)` = 4, `CRC16(AA)` = "" for
/// an empty AA — all st=0.
#[test]
fn test_checksum_guards_are_not_errors() {
    assert_eq!(eval_str("CRC16(4)"), StackValue::Double(4.0));
    assert_eq!(eval_str("MODBUS(4)"), StackValue::Double(4.0));
    assert_eq!(eval_str("XOR8(4)"), StackValue::Double(4.0));
    assert_eq!(eval_str("ADD_XOR8(4)"), StackValue::Double(4.0));

    for expr in ["CRC16(AA)", "MODBUS(AA)", "XOR8(AA)", "ADD_XOR8(AA)"] {
        let mut inputs = StringInputs::new();
        inputs.str_vars[0] = "".into();
        assert_eq!(
            scalc(expr, &mut inputs).unwrap(),
            StackValue::Str("".into()),
            "{expr}"
        );
    }
}

// --- LRC / AMODBUS ---

#[test]
fn test_lrc() {
    let result = eval_str(r#"LRC("010203")"#);
    assert_eq!(result, StackValue::Str("FA".into()));
}

/// AMODBUS PREPENDS the ASCII-MODBUS start delimiter `:` as well as appending the
/// LRC (`sCalcPerform.c:1846-1850`). The port dropped it, so every frame it built
/// was missing its start character. Compiled sCalc:
///
///   AMODBUS("010203")        :010203FA        (9 chars)
///   AMODBUS("F7031389000A")  :F7031389000A60  (15 chars — the example in C's own
///                                              comment at `:1833`)
#[test]
fn test_amodbus_prepends_the_start_delimiter() {
    assert_eq!(
        eval_str(r#"AMODBUS("010203")"#),
        StackValue::Str(":010203FA".into())
    );
    assert_eq!(
        eval_str(r#"LEN(AMODBUS("010203"))"#),
        StackValue::Double(9.0)
    );
    assert_eq!(
        eval_str(r#"AMODBUS("F7031389000A")"#),
        StackValue::Str(":F7031389000A60".into())
    );
}

/// The 39-byte value bound bites the LRC, not the frame: C copies `":" + operand`
/// in first (`strNcpy(ps->s, tmpstr, SCALC_STRING_SIZE)`) and appends only what of
/// the LRC still fits. Compiled sCalc, with a 38-character operand: 39 characters
/// out, and the LRC entirely crowded out.
///
///   AMODBUS("0102030405060708090A0B0C0D0E0F10111213")
///     = :0102030405060708090A0B0C0D0E0F10111213     (39 chars, no LRC)
///   AMODBUS("0102030405060708090A0B0C0D0E0F1011")
///     = :0102030405060708090A0B0C0D0E0F101167       (37 chars, LRC "67" fits)
#[test]
fn test_amodbus_long_operand_crowds_out_the_lrc() {
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = "0102030405060708090A0B0C0D0E0F10111213".into(); // 38
    let r = scalc("AMODBUS(AA)", &mut inputs).unwrap();
    assert_eq!(
        r,
        StackValue::Str(":0102030405060708090A0B0C0D0E0F10111213".into())
    );
    match r {
        StackValue::Str(s) => assert_eq!(s.len(), 39),
        _ => panic!("expected a string"),
    }

    // Four characters shorter, and the LRC fits.
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = "0102030405060708090A0B0C0D0E0F1011".into(); // 34
    assert_eq!(
        scalc("AMODBUS(AA)", &mut inputs).unwrap(),
        StackValue::Str(":0102030405060708090A0B0C0D0E0F101167".into())
    );
}

// --- Subrange [] ---

/// C `sCalcPerform.c:1897` (`s1 <= s2`) — the upper bound is INCLUSIVE.
/// Compiled sCalc: `"hello"[1,4]` = "ello". This test previously pinned the
/// port's exclusive-end reading ("ell").
#[test]
fn test_subrange_basic() {
    let result = eval_str(r#""hello"[1,4]"#);
    assert_eq!(result, StackValue::Str("ello".into()));
}

#[test]
fn test_subrange_full() {
    let result = eval_str(r#""hello"[0,5]"#);
    assert_eq!(result, StackValue::Str("hello".into()));
}

#[test]
fn test_subrange_clamp() {
    let result = eval_str(r#""hello"[0,100]"#);
    assert_eq!(result, StackValue::Str("hello".into()));
}

/// `i == j` selects ONE character, not none — the bounds are inclusive.
/// Compiled sCalc: `"hello"[2,2]` = "l". An empty selection takes `i > j`.
#[test]
fn test_subrange_single_char() {
    let result = eval_str(r#""hello"[2,2]"#);
    assert_eq!(result, StackValue::Str("l".into()));
}

/// Compiled sCalc: `"hello"[3,1]` = "".
#[test]
fn test_subrange_inverted_is_empty() {
    let result = eval_str(r#""hello"[3,1]"#);
    assert_eq!(result, StackValue::Str("".into()));
}

/// A negative bound counts back from the end (C `:1877,1884`: `if (i < 0) i += k`).
/// Compiled sCalc: `"hello"[-2,-1]` = "lo", `"hello"[-3,10]` = "llo".
#[test]
fn test_subrange_negative_bounds_wrap() {
    assert_eq!(eval_str(r#""hello"[-2,-1]"#), StackValue::Str("lo".into()));
    assert_eq!(eval_str(r#""hello"[-3,10]"#), StackValue::Str("llo".into()));
}

// --- Replace {} ---

#[test]
fn test_replace_basic() {
    let result = eval_str(r#""abcabc"{"b","X"}"#);
    assert_eq!(result, StackValue::Str("aXcabc".into()));
}

#[test]
fn test_replace_no_match() {
    let result = eval_str(r#""hello"{"z","X"}"#);
    assert_eq!(result, StackValue::Str("hello".into()));
}

#[test]
fn test_replace_full() {
    let result = eval_str(r#""abc"{"abc","XYZ"}"#);
    assert_eq!(result, StackValue::Str("XYZ".into()));
}

// --- SubLast |- ---

#[test]
fn test_sublast_basic() {
    let result = eval_str(r#""abcabc" |- "b""#);
    assert_eq!(result, StackValue::Str("abcac".into()));
}

#[test]
fn test_sublast_no_match() {
    let result = eval_str(r#""hello" |- "z""#);
    assert_eq!(result, StackValue::Str("hello".into()));
}

// --- UNTIL loop ---

/// R10-9: C's UNTIL_END has runtime_effect 0 (`sCalcPostfix.c:782`) — it PEEKS
/// the condition (`sCalcPerform.c:1999`) and leaves it on the stack on the way
/// out. So the condition value is still there when the loop exits, and a source
/// that then pushes another value ends at depth 2 and does not compile.
///
/// Compiled sCalcPostfix: `UNTIL 1; 42`, `UNTIL A; A` and `UNTIL 0; 0` are all
/// CALC_ERR_INCOMPLETE. These three cases used to pin the port's -1 accounting,
/// which accepted them.
#[test]
fn test_until_with_a_value_body_is_incomplete() {
    for expr in ["UNTIL 1; 42", "UNTIL A; A", "UNTIL B; B", "UNTIL 0; 0"] {
        assert!(
            matches!(
                epics_base_rs::calc::scalc_compile(expr),
                Err(CalcError::Incomplete)
            ),
            "{expr}"
        );
    }
}

/// The form C accepts: the body is an ASSIGNMENT, whose -1 brings the program
/// back to depth 1 — the condition value, which is what it evaluates to.
/// Compiled sCalcPerform: `A:=0; UNTIL A>3; A:=A+1` gives VAL=0 (the condition)
/// with A=1.
#[test]
fn test_until_with_an_assignment_body() {
    let mut inputs = StringInputs::new();
    inputs.num_vars[0] = 3.0; // A: the condition is true at once
    let result = scalc("UNTIL A; A:=A+1", &mut inputs).unwrap();
    assert_eq!(result, StackValue::Double(3.0), "the condition value");
    assert_eq!(inputs.num_vars[0], 4.0, "the body ran");
}

/// `UNTIL(body; ... ; condition)` — the form the record documentation uses
/// (`aCalcoutRecord.md:372`, `:551`) and the only one that loops. C compiles
/// `UNTIL_END` from the OPERATOR stack, so the `;` INSIDE the parentheses cannot
/// close the loop: everything up to the `)` is the body and the last value is the
/// condition.
///
/// Compiled sCalcPerform, verbatim:
///   `UNTIL(1)`                    -> st=0, VAL=1
///   `B:=10;UNTIL(B:=B-1;B<1)`     -> st=0, VAL=1
///   `A:=0;UNTIL(A:=A+1;A>3)`      -> st=0, VAL=1, A=4
///   `UNTIL(1)+UNTIL(1)`           -> st=0, VAL=2
#[test]
fn test_until_parenthesised_loop() {
    let mut inputs = StringInputs::new();
    assert_eq!(
        scalc("UNTIL(1)", &mut inputs).unwrap(),
        StackValue::Double(1.0)
    );

    let mut inputs = StringInputs::new();
    let r = scalc("B:=10;UNTIL(B:=B-1;B<1)", &mut inputs).unwrap();
    assert_eq!(r, StackValue::Double(1.0), "the condition value");
    assert_eq!(inputs.num_vars[1], 0.0, "B counted down to 0");

    let mut inputs = StringInputs::new();
    let r = scalc("A:=0;UNTIL(A:=A+1;A>3)", &mut inputs).unwrap();
    assert_eq!(r, StackValue::Double(1.0));
    assert_eq!(inputs.num_vars[0], 4.0, "the body ran until A>3");

    let mut inputs = StringInputs::new();
    assert_eq!(
        scalc("UNTIL(1)+UNTIL(1)", &mut inputs).unwrap(),
        StackValue::Double(2.0)
    );
}

/// Running out of iterations is NOT an error in sCalc. C `sCalcPerform.c:1997`:
///
/// ```c
/// if (++loopsDone > sCalcLoopMax) break;   /* out of the switch, not the perform */
/// ```
///
/// so the loop simply stops, the perform returns 0, and the value is the last
/// condition it evaluated (which is false, or it would have exited on its own).
/// Compiled sCalcPerform, with the shipped `sCalcLoopMax` = 1000:
///   `A:=0;UNTIL(A:=A+1;0)`        -> st=0, VAL=0, A=1001
///   `A:=0;UNTIL(A:=A+1;A>2000)`   -> st=0, VAL=0, A=1001
///   `UNTIL 0; A:=1`               -> st=0, VAL=0, A=1
///
/// A=1001, not 1000: `loopsDone` is incremented on ARRIVAL at UNTIL_END, so the
/// body has already run one more time than the loop-back count.
#[test]
fn test_until_loop_max_stops_without_an_error() {
    let mut inputs = StringInputs::new();
    let r = scalc("A:=0;UNTIL(A:=A+1;0)", &mut inputs).unwrap();
    assert_eq!(
        r,
        StackValue::Double(0.0),
        "the last condition, not an error"
    );
    assert_eq!(inputs.num_vars[0], 1001.0);

    let mut inputs = StringInputs::new();
    let r = scalc("A:=0;UNTIL(A:=A+1;A>2000)", &mut inputs).unwrap();
    assert_eq!(r, StackValue::Double(0.0));
    assert_eq!(inputs.num_vars[0], 1001.0);

    // The un-parenthesised form: the loop body is the bare `0`, and `A:=1` sits
    // AFTER the UNTIL_END, so it runs exactly once, after the loop gives up.
    let mut inputs = StringInputs::new();
    let r = scalc("UNTIL 0; A:=1", &mut inputs).unwrap();
    assert_eq!(r, StackValue::Double(0.0));
    assert_eq!(inputs.num_vars[0], 1.0);
}

/// C `until_scratch[10]` with `if (i>9) {printf("too many UNTILs"); return(-1);}`
/// (`sCalcPerform.c:356-360`). Compiled sCalc: nine `UNTIL(1)` terms perform
/// (VAL=9); the tenth fails the perform.
#[test]
fn test_until_count_ceiling_is_nine() {
    let nine = ["UNTIL(1)"; 9].join("+");
    let mut inputs = StringInputs::new();
    assert_eq!(
        scalc(&nine, &mut inputs).unwrap(),
        StackValue::Double(9.0),
        "nine UNTILs perform"
    );

    let ten = ["UNTIL(1)"; 10].join("+");
    let mut inputs = StringInputs::new();
    assert!(
        scalc(&ten, &mut inputs).is_err(),
        "the tenth fails the perform"
    );
}

// --- READ / WRITE (C `BIN_READ` / `BIN_WRITE` opcodes) ---
//
// The two cases that lived here pinned the port's 1-operand op, which computed
// an unescape/escape — TR_ESC and ESC under another name. Their own comment
// recorded that C's READ/WRITE are 2-operand and called the gap an open
// finding. It is now closed: READ/WRITE are the binary field conversions they
// are in C, and tests/calc_string_bin_read_write.rs owns them, byte for byte
// against the compiled engine.

// --- Edge cases ---

/// A checksum NEVER fails the perform. C's call site is
/// `if (lrc(tmpstr10, ps->s) == 0) { ...write... }` (`sCalcPerform.c:1843`), so
/// the helper's `return(-1)` just means "write nothing" — the operand survives
/// and st stays 0. This test pinned `Err(InvalidFormat)`, which no sCalc path
/// produces.
///
/// C's `hex()` (`:231`) reads a non-hex character as 0 and its loop ignores a
/// trailing odd one, so LRC has no failure mode at all. Compiled sCalc, and now
/// the port (R12-9).
#[test]
fn test_lrc_accepts_every_operand_c_accepts() {
    assert_eq!(eval_str(r#"LRC("0G")"#), StackValue::Str("00".into()));
    assert_eq!(eval_str(r#"LRC("010")"#), StackValue::Str("FF".into()));
    assert_eq!(eval_str(r#"LRC("0")"#), StackValue::Str("00".into()));
    // AMODBUS runs the same helper, so it inherits both.
    assert_eq!(
        eval_str(r#"AMODBUS("0G")"#),
        StackValue::Str(":0G00".into())
    );
    assert_eq!(
        eval_str(r#"AMODBUS("010203")"#),
        StackValue::Str(":010203FA".into())
    );
}

#[test]
fn test_printf_no_spec() {
    let result = eval_str(r#"PRINTF("hello", 42)"#);
    assert_eq!(result, StackValue::Str("hello".into()));
}

#[test]
fn test_printf_percent_escape() {
    // R11-5: C's scan finds no conversion after the last `%%`, so PRINTF
    // `strcpy`s the RAW format — the `%%` is NOT collapsed. Compiled C:
    // PRINTF("100%%", 0) is `100%%`. (`%%` collapses only when a live
    // conversion sends the format through snprintf — see tests/scalc_printf.rs.)
    let result = eval_str(r#"PRINTF("100%%", 0)"#);
    assert_eq!(result, StackValue::Str("100%%".into()));
}

// --- the `toString` operands (R12-7) ---
//
// C `LEN` (`sCalcPerform.c:1520`) and `REPLACE` (`:1903`) open by coercing their
// operands with `toString(ps)` — the same macro TO_STRING, SUBRANGE and a string
// store use. The port demanded a string and answered 0 / TypeMismatch instead.

/// Compiled C: `LEN(4)` is 10 — the width of "4.00000000", the double's string
/// form at the precision `to_string` hardcodes. The port answered 0.
#[test]
fn test_len_of_a_double_measures_its_string_form() {
    assert_eq!(eval_str("LEN(4)"), StackValue::Double(10.0));
    assert_eq!(eval_str("LEN(0)"), StackValue::Double(10.0));
    assert_eq!(eval_str("LEN(3.14159265358979)"), StackValue::Double(10.0));
    assert_eq!(eval_str(r#"LEN("abc")"#), StackValue::Double(3.0));
    assert_eq!(eval_str(r#"LEN("")"#), StackValue::Double(0.0));
}

/// REPLACE (`{`) coerces ALL THREE operands, so a double in any position is
/// legal. Compiled C: `4{"4","x"}` is "x.00000000", `"a4c"{"4",4}` is
/// "a4.00000000c". The port raised TypeMismatch for every one of them.
#[test]
fn test_replace_coerces_every_operand() {
    assert_eq!(
        eval_str(r#"4{"4","x"}"#),
        StackValue::Str("x.00000000".into())
    );
    assert_eq!(
        eval_str(r#""a4c"{"4",4}"#),
        StackValue::Str("a4.00000000c".into())
    );
    // The find text is coerced too, so "4.00000000" is what is looked for —
    // and it is not in "a4c", which is therefore returned unchanged.
    assert_eq!(eval_str(r#""a4c"{4,"x"}"#), StackValue::Str("a4c".into()));
    // Only the first occurrence goes (C `strstr`).
    assert_eq!(
        eval_str(r#""hello"{"l","LL"}"#),
        StackValue::Str("heLLlo".into())
    );
}

/// The USES_STRING marker (`sCalcPostfix.c:447-475`) picks the whole evaluator,
/// and C's list opens with FETCH_AA..FETCH_LL — so merely reading `AA` marks the
/// program. The port's marker used to name an opcode its own compiler never emits
/// (a `StringOp::PushStringVar`, since deleted; `AA` compiles to
/// `CoreOp::PushDoubleVar`), so no `AA`-reading program was marked.
///
/// The marker is observable without a string in sight because C's two
/// evaluators cast their integer operands at different WIDTHS. MODULO used to be
/// the observable used here; CBUG-A2 now models sCalc MODULO with a single
/// `(long)` narrowing (the wider evaluator's path), so it can no longer probe
/// the evaluator split — the probe is now `<<`, which C type-branches: `(long)`
/// (`sCalcPerform.c:623-631`) in the numeric evaluator, `(int)`
/// (`:1270-1276`) in the string one. x86-64 masks the shift count to 6 bits at
/// 64-bit width and to 5 at 32-bit, so `1 << 40` is 2^40 in one evaluator and
/// 2^8 in the other — both in range, no out-of-range cast involved.
#[test]
fn test_fetch_aa_marks_the_program_uses_string() {
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = "0".into();
    assert_eq!(
        scalc("1<<40", &mut inputs).unwrap(),
        StackValue::Double(1_099_511_627_776.0),
        "no FETCH_AA → numeric evaluator → 64-bit shift"
    );
    assert_eq!(
        scalc("(1<<40)+AA", &mut inputs).unwrap(),
        StackValue::Double(256.0),
        "FETCH_AA marks USES_STRING → string evaluator → 32-bit shift"
    );
}
