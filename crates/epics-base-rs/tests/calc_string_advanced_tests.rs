#![allow(clippy::approx_constant)]

use epics_base_rs::calc::{CalcError, StackValue, StringInputs, scalc};

fn eval_str(expr: &str) -> StackValue {
    let mut inputs = StringInputs::new();
    scalc(expr, &mut inputs).unwrap()
}

// --- TR_ESC / ESC ---

#[test]
fn test_tr_esc_newline() {
    let result = eval_str(r#"TR_ESC("hello\\nworld")"#);
    assert_eq!(result, StackValue::Str("hello\nworld".into()));
}

#[test]
fn test_tr_esc_tab() {
    let result = eval_str(r#"TR_ESC("a\\tb")"#);
    assert_eq!(result, StackValue::Str("a\tb".into()));
}

#[test]
fn test_tr_esc_hex() {
    let result = eval_str(r#"TR_ESC("\\x41")"#);
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

// --- SSCANF ---

#[test]
fn test_sscanf_int() {
    let result = eval_str(r#"SSCANF("42", "%d")"#);
    assert_eq!(result, StackValue::Double(42.0));
}

#[test]
fn test_sscanf_float() {
    let result = eval_str(r#"SSCANF("3.15", "%f")"#);
    assert_eq!(result, StackValue::Double(3.15));
}

#[test]
fn test_sscanf_string() {
    let result = eval_str(r#"SSCANF("hello world", "%s")"#);
    assert_eq!(result, StackValue::Str("hello".into()));
}

// --- CRC16 ---

#[test]
fn test_crc16() {
    let result = eval_str(r#"CRC16("123456789")"#);
    assert_eq!(result, StackValue::Double(0x4B37 as f64));
}

#[test]
fn test_modbus_append() {
    // MODBUS appends CRC16 bytes to the string
    let mut inputs = StringInputs::new();
    let result = scalc(r#"MODBUS("AB")"#, &mut inputs).unwrap();
    match result {
        StackValue::Str(s) => {
            // First two chars are "AB"
            assert!(s.as_bytes().starts_with(b"AB"));
            // Followed by two CRC chars (may be multi-byte in UTF-8)
            assert!(s.len() > 2);
        }
        _ => panic!("expected string"),
    }
}

// --- LRC ---

#[test]
fn test_lrc() {
    let result = eval_str(r#"LRC("010203")"#);
    assert_eq!(result, StackValue::Str("FA".into()));
}

#[test]
fn test_amodbus_append() {
    // AMODBUS appends LRC hex string (2 chars)
    let result = eval_str(r#"LEN(AMODBUS("010203"))"#);
    // "010203" is 6 chars, plus "FA" = 8
    assert_eq!(result, StackValue::Double(8.0));
}

// --- XOR8 ---

#[test]
fn test_xor8() {
    // XOR of 0x01, 0x02, 0x03 = 0x00
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = vec![0x01u8, 0x02, 0x03].into();
    let result = scalc("XOR8(AA)", &mut inputs).unwrap();
    assert_eq!(result, StackValue::Double(0.0));
}

#[test]
fn test_xor8_ascii() {
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = "AB".into(); // 0x41 ^ 0x42 = 0x03
    let result = scalc("XOR8(AA)", &mut inputs).unwrap();
    assert_eq!(result, StackValue::Double(3.0));
}

#[test]
fn test_add_xor8_append() {
    // ADD_XOR8 appends XOR8 as one byte
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = "AB".into();
    let result = scalc("LEN(ADD_XOR8(AA))", &mut inputs).unwrap();
    // "AB" is 2 bytes + 1 XOR8 byte = 3
    assert_eq!(result, StackValue::Double(3.0));
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

/// A negative bound counts back from the end (C `:1878,1885`: `if (i < 0) i += k`).
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
/// (`aCalcoutRecord.md:405`, `:578`) and the only one that loops. C compiles
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

#[test]
fn test_lrc_invalid() {
    let mut inputs = StringInputs::new();
    let result = scalc(r#"LRC("0G")"#, &mut inputs);
    assert!(matches!(result, Err(CalcError::InvalidFormat)));
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
