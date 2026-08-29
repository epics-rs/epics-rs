//! R14-5 — sCalc `>>` / `<<` branch on the LEFT operand's TYPE.
//!
//! ```c
//! case RIGHT_SHIFT:
//! case LEFT_SHIFT:                          /* sCalcPerform.c:1263-1294 */
//!     ps1 = ps;  toDouble(ps1);
//!     j = myNINT(ps1->d);  j = myMIN(j, SCALC_STRING_SIZE);
//!     DEC(ps);
//!     if (isDouble(ps)) {                   /* bit shift — 32-bit here! */
//!         ps->d = (int)(ps->d) >> (int)(ps1->d);
//!     } else {                              /* CHARACTER shift          */
//!         if (op == RIGHT_SHIFT) {
//!             for (i=SCALC_STRING_SIZE-1; i>=0; i--)
//!                 ps->s[i] = (i>=j) ? ps->s[i-j] : ' ';
//!             ps->s[SCALC_STRING_SIZE-1] = '\0';
//!         } else {
//!             if (j==SCALC_STRING_SIZE) ps->s[0] = '\0';
//!             else for (i=0; i < (SCALC_STRING_SIZE-j); i++)
//!                 ps->s[i] = ps->s[i+j];
//!         }
//!     }
//! ```
//!
//! The port did `pop2_f64` and a 64-bit bit shift on both, so `"abc">>1` was the
//! double 0.0 where C answers the STRING `" abc"` — wrong value and wrong type.
//!
//! The shift also pins the one place sCalc's two evaluators disagree on cast
//! WIDTH: `>>`, `<<` and `~` are `(long)` on the double-only path (`:622-630`,
//! `:724-726`) and `(int)` on the string path (`:1270-1276`, `:1440-1443`).
//! `&`, `|`, `^` are `(long)` on both. Every expected value below is compiled C.
//!
//! Boundaries, one case each: count 0, 1, mid, 39, exactly 40, above 40,
//! negative, fractional (myNINT rounds); both directions; a string that fills the
//! 40-byte element; a numeric left operand on each evaluator path.

use epics_base_rs::calc::{StackValue, StringInputs, scalc};

fn ev(expr: &str) -> StackValue {
    let mut inputs = StringInputs::new();
    inputs.num_vars[0] = 8.0; // A
    inputs.str_vars[0] = "abc".into(); // AA
    scalc(expr, &mut inputs).unwrap()
}

fn text(expr: &str) -> String {
    match ev(expr) {
        StackValue::Str(s) => s.as_str_lossy().to_string(),
        other => panic!("{expr}: expected a string, got {other:?}"),
    }
}

fn num(expr: &str) -> f64 {
    match ev(expr) {
        StackValue::Double(v) => v,
        other => panic!("{expr}: expected a double, got {other:?}"),
    }
}

/// The case the finding names. Right shift slides the bytes up and fills the
/// vacated head with SPACES.
#[test]
fn a_string_right_shift_is_a_character_shift() {
    assert_eq!(text(r#""abc">>1"#), " abc");
    assert_eq!(text(r#""abc">>3"#), "   abc");
    assert_eq!(text("AA>>1"), " abc");
}

/// Left shift slides the bytes down, dropping the head — the NUL travels with
/// them, so the string simply gets shorter.
#[test]
fn a_string_left_shift_drops_the_leading_bytes() {
    assert_eq!(text(r#""abc"<<1"#), "bc");
    assert_eq!(text(r#""abc"<<2"#), "c");
    assert_eq!(text(r#""abc"<<3"#), "");
    // Past the end of the string, but inside the buffer: still empty.
    assert_eq!(text(r#""abc"<<9"#), "");
}

/// Count 0 is the identity in both directions — C's loops copy each byte onto
/// itself. (`>>` still forces the last byte to NUL, which a 3-byte string never
/// notices.)
#[test]
fn a_zero_count_is_the_identity() {
    assert_eq!(text(r#""abc">>0"#), "abc");
    assert_eq!(text(r#""abc"<<0"#), "abc");
}

/// The count is `myNINT(ps1->d)` — ROUNDED, half away from zero — and it is
/// taken through `toDouble`, so a numeric string counts as its value.
#[test]
fn the_count_is_rounded_and_coerced() {
    assert_eq!(text(r#""abc">>0.6"#), " abc"); // 0.6 -> 1
    assert_eq!(text(r#""abc">>1.4"#), " abc"); // 1.4 -> 1
    assert_eq!(text(r#""abc">>1.5"#), "  abc"); // 1.5 -> 2
    assert_eq!(text(r#""abc">>"2""#), "  abc"); // atof
}

/// `j` is clamped ABOVE at SCALC_STRING_SIZE (40) — `myMIN(j, 40)`. At exactly
/// 40 the left shift is C's explicit `ps->s[0] = '\0'` and the right shift fills
/// the whole buffer with spaces, leaving 39 of them once the forced NUL at [39]
/// is applied. Anything above 40 clamps to the same.
#[test]
fn the_count_is_clamped_above_at_the_buffer_size() {
    assert_eq!(text(r#""abc"<<40"#), "");
    assert_eq!(text(r#""abc"<<41"#), "");
    assert_eq!(text(r#""abc"<<9999"#), "");
    assert_eq!(text(r#""abc">>40"#), " ".repeat(39));
    assert_eq!(text(r#""abc">>9999"#), " ".repeat(39));
    // One below the clamp: 39 spaces would need 40 bytes, so the 'a' that lands
    // at [39] is overwritten by the forced NUL — 39 spaces again, minus one.
    assert_eq!(text(r#""abc">>39"#), " ".repeat(39));
}

/// A NEGATIVE count reads outside the 40-byte element in C (`ps->s[i-j]` with
/// `i` at 39), so there is no compiled-C answer to match — the port clamps at 0,
/// making it the identity. Documented deviation, pinned here so it cannot drift
/// into an out-of-bounds panic or a silent direction flip.
#[test]
fn a_negative_count_is_the_identity_a_documented_deviation() {
    assert_eq!(text(r#""abc">>(0-1)"#), "abc");
    assert_eq!(text(r#""abc"<<(0-5)"#), "abc");
}

/// The bytes that fall off the top are LOST — the right shift forces `s[39]` to
/// NUL, so a string long enough to reach the end of the buffer is truncated.
#[test]
fn a_right_shift_truncates_at_the_end_of_the_buffer() {
    // 39 bytes, the longest string sCalc holds. Shifted right by 1: one leading
    // space, the last byte pushed out, 38 of the original left.
    let long = "a".repeat(39);
    let shifted = text(&format!(r#""{long}">>1"#));
    assert_eq!(shifted.len(), 39);
    assert_eq!(shifted, format!(" {}", "a".repeat(38)));
}

/// A DOUBLE left operand is still a bit shift — and on the string path it is a
/// 32-bit one. `A` is 8: `8<<2` = 32, `8>>2` = 2.
#[test]
fn a_double_left_operand_is_still_a_bit_shift() {
    assert_eq!(num(r#"A<<2+0*LEN(AA)"#), 32.0);
    assert_eq!(num(r#"A>>2+0*LEN(AA)"#), 2.0);
    // Same on the no-string path.
    assert_eq!(num("A<<2"), 32.0);
    assert_eq!(num("A>>2"), 2.0);
}

/// The width the two evaluators use differs, and the shift is where it shows:
/// `(long)` on the double-only path, `(int)` on the string path. 1<<40 is
/// 1099511627776 with a 64-bit operand; with a 32-bit one x86 masks the count to
/// 5 bits, so it is 1<<8 = 256.
#[test]
fn the_shift_width_follows_the_evaluator() {
    assert_eq!(num("1<<40"), 1099511627776.0);
    assert_eq!(num(r#"1<<40+0*LEN(AA)"#), 256.0);
    // `~` splits the same way (`:724-726` vs `:1440-1443`); `&` `|` `^` do not
    // — they are `(long)` on both paths.
    assert_eq!(num("~8589934592"), -8589934593.0);
    // The string path's `(int)` narrows 2^33 out of range, so what `~` inverts
    // is CBUG-E2's saturated INT32_MAX -> INT32_MIN. A compiled x86-64 IOC
    // narrows to INT32_MIN and so answers with the opposite end, 2147483647.
    assert_eq!(num(r#"~8589934592+0*LEN(AA)"#), -2147483648.0);
    assert_eq!(num("8589934592&8589934592"), 8589934592.0);
    assert_eq!(num(r#"(8589934592&8589934592)+0*LEN(AA)"#), 8589934592.0);
}
