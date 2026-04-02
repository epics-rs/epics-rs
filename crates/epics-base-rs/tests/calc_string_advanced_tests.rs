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
            assert!(s.starts_with("AB"));
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
    inputs.str_vars[0] = String::from_utf8(vec![0x01, 0x02, 0x03]).unwrap();
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

#[test]
fn test_subrange_basic() {
    let result = eval_str(r#""hello"[1,4]"#);
    assert_eq!(result, StackValue::Str("ell".into()));
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

#[test]
fn test_subrange_empty() {
    let result = eval_str(r#""hello"[2,2]"#);
    assert_eq!(result, StackValue::Str("".into()));
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

#[test]
fn test_until_immediate_exit() {
    // UNTIL with true condition should exit immediately
    let mut inputs = StringInputs::new();
    let compiled = epics_base_rs::calc::scalc_compile("UNTIL 1; 42").unwrap();
    // Debug: print opcodes
    for (i, op) in compiled.code.iter().enumerate() {
        eprintln!("  [{}] {:?}", i, op);
    }
    let result = epics_base_rs::calc::scalc_eval(&compiled, &mut inputs).unwrap();
    assert_eq!(result, StackValue::Double(42.0));
}

#[test]
fn test_until_counter() {
    // Test UNTIL with a simple counter using external state mutation.
    // Increment A each iteration via num_vars externally isn't possible in expression.
    // Test that UNTIL exits when condition is true and loops when false:
    // A starts at 3. UNTIL A; tests A. A=3 (non-zero) -> exit immediately.
    let mut inputs = StringInputs::new();
    inputs.num_vars[0] = 3.0;
    let result = scalc("UNTIL A; A", &mut inputs).unwrap();
    assert_eq!(result, StackValue::Double(3.0));

    // A starts at 0. UNTIL A; loops forever -> but we already test that via loop_limit.
    // Test multi-iteration: A starts at 0, B starts at 1.
    // Loop body: B (pushes B). Condition = B (non-zero -> exit).
    // First iteration: B=1 -> exit. Works.
    let mut inputs2 = StringInputs::new();
    inputs2.num_vars[1] = 1.0; // B
    let result2 = scalc("UNTIL B; B", &mut inputs2).unwrap();
    assert_eq!(result2, StackValue::Double(1.0));
}

#[test]
fn test_until_loop_limit() {
    // Condition always false (0), never exits -> LoopLimitExceeded
    let mut inputs = StringInputs::new();
    let result = scalc("UNTIL 0; 0", &mut inputs);
    assert!(matches!(result, Err(CalcError::LoopLimitExceeded)));
}

// --- BIN_READ / BIN_WRITE ---

#[test]
fn test_bin_read() {
    let result = eval_str(r#"BIN_READ("hello\\nworld")"#);
    assert_eq!(result, StackValue::Str("hello\nworld".into()));
}

#[test]
fn test_bin_write() {
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = "hello\nworld".into();
    let result = scalc("BIN_WRITE(AA)", &mut inputs).unwrap();
    assert_eq!(result, StackValue::Str("hello\\nworld".into()));
}

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
    let result = eval_str(r#"PRINTF("100%%", 0)"#);
    assert_eq!(result, StackValue::Str("100%".into()));
}
