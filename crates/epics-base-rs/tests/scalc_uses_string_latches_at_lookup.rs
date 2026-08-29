//! R14-3 — USES_STRING is a property of the ELEMENTS THE COMPILER LOOKED UP,
//! not of the opcodes that survive compilation.
//!
//! C stamps the marker inside the element loop (`sCalcPostfix.c:447-471`), the
//! moment `get_element` hands back a string-typed element. `AA` is looked up as
//! `FETCH_AA` — which IS in the list — and only afterwards, when `:=` arrives,
//! is that fetch retracted from the postfix and rewritten as `STORE_AA`
//! (`:548-553`), which is NOT in the list. So `AA:="x";1` is USES_STRING even
//! though its finished postfix holds no string-typed opcode at all, and C runs
//! the string evaluator — the one whose STORE_AA writes the record's string
//! args and whose `<<`/`>>` shift at 32 bits rather than 64.
//!
//! A port that re-derives the marker from the finished opcodes cannot see this:
//! it sees only the store, concludes "no string", and runs the double-only
//! evaluator, whose `default: break` silently drops the store's operand — which
//! then fails the depth check, when it should have written AA.
//!
//! The boundary tested per case: does the string element SURVIVE compilation
//! (`AA+""`, `LEN(AA)`) or is it REWRITTEN away (`AA:=`)? Both must latch.

use epics_base_rs::calc::{StackValue, StringInputs, scalc_compile, scalc_eval};

fn run(expr: &str, setup: impl FnOnce(&mut StringInputs)) -> (StringInputs, StackValue) {
    let mut inputs = StringInputs::new();
    setup(&mut inputs);
    let compiled = scalc_compile(expr).unwrap();
    let top = scalc_eval(&compiled, &mut inputs).unwrap();
    (inputs, top)
}

/// The case the finding pins. The only string element is the `AA` that `:=`
/// consumed; the program must still take the string path, and the store must
/// land in the STRING args.
#[test]
fn a_rewritten_string_fetch_still_latches_the_marker() {
    assert!(scalc_compile(r#"AA:="x";1"#).unwrap().uses_string);

    let (inputs, top) = run(r#"AA:="xyz";1"#, |_| {});
    assert_eq!(inputs.str_vars[0].as_str_lossy(), "xyz");
    assert_eq!(top, StackValue::Double(1.0));
}

/// A store of a NUMBER into a string field is the same program shape — `AA` is
/// still the element that was looked up, so the marker still latches and C's
/// STORE_AA still coerces the double to text with `to_string`, which is
/// `cvtDoubleToString(d, s, 8)` (`sCalcPerform.c:90-96`): eight fractional
/// digits, not the record's PREC.
#[test]
fn storing_a_double_into_a_string_field_latches_too() {
    let (inputs, _) = run("AA:=12;1", |_| {});
    assert!(scalc_compile("AA:=12;1").unwrap().uses_string);
    assert_eq!(inputs.str_vars[0].as_str_lossy(), "12.00000000");
}

/// The distinguishing arithmetic between the two evaluators. MODULO used to be
/// the probe here; CBUG-A2 removed MODULO's operand narrowing, so the probe is
/// now `<<`, which C branches the same way and which the port still reproduces:
/// `(long)` on the double-only path (`sCalcPerform.c:623-631`), `(int)` on the
/// string path (`:1270-1276`). x86-64 masks the shift count to 6 bits at 64-bit
/// width and to 5 at 32-bit, so `1 << 40` is 2^40 in one evaluator and `1 << 8`
/// = 256 in the other.
#[test]
fn the_marker_selects_the_evaluator_that_the_shift_width_depends_on() {
    // The only string element is the `AA` that `:=` rewrote away — the program
    // is still USES_STRING, so the 32-bit shift runs.
    let (_, top) = run(r#"AA:="x";1 << 40"#, |_| {});
    assert_eq!(top, StackValue::Double(256.0));

    // No string element anywhere: the double-only path, 64-bit shift.
    let (_, top) = run("1 << 40", |_| {});
    assert_eq!(top, StackValue::Double(1_099_511_627_776.0));
}

/// A string element that SURVIVES compilation latches as it always did — the
/// fix must not narrow the set. One case per surviving-element family.
#[test]
fn surviving_string_elements_still_latch() {
    for expr in [
        "AA",                // FETCH_AA
        "SVAL",              // FETCH_SVAL
        r#""x""#,            // LITERAL_STRING
        "LEN(AA)",           // LEN
        r#"STR(1)"#,         // TO_STRING
        r#""abc"[0,1]"#,     // SUBRANGE
        r#""abc"{"a","b"}"#, // REPLACE
        "@@0",               // A_SFETCH
    ] {
        assert!(
            scalc_compile(expr).unwrap().uses_string,
            "{expr} must be USES_STRING"
        );
    }
}

/// And the four elements C leaves OUT of the list still do not latch: `DBL`,
/// `BYTE`, `|-` and `@`. (`|-` on the no-string path is R14-2's error case, so
/// only its marker is checked here.)
#[test]
fn the_elements_c_omits_still_do_not_latch() {
    for expr in ["DBL(A)", "BYTE(A)", "A|-B", "@1"] {
        assert!(
            !scalc_compile(expr).unwrap().uses_string,
            "{expr} must NOT be USES_STRING"
        );
    }
}
