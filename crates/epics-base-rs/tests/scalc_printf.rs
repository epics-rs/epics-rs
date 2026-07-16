//! R11-5 — sCalc's PRINTF is C's `snprintf` behind C's conversion scan
//! (`sCalcPerform.c:1535-1567`), not a hand-rolled formatter.
//!
//! Every expectation below is the output of that C block — the scan and the
//! `snprintf` call, verbatim — compiled and run on this host.

use epics_base_rs::calc::{CalcError, StackValue, StringInputs, scalc};

fn pf(expr: &str) -> String {
    let mut inp = StringInputs::new();
    match scalc(expr, &mut inp).expect("st=0") {
        StackValue::Str(s) => s.as_str_lossy().into_owned(),
        StackValue::Double(d) => panic!("PRINTF is a string, got {d}"),
    }
}

fn pf_str(fmt: &str, arg: &str) -> String {
    let mut inp = StringInputs::new();
    inp.str_vars[0] = arg.into();
    match scalc(&format!("PRINTF(\"{fmt}\", AA)"), &mut inp).expect("st=0") {
        StackValue::Str(s) => s.as_str_lossy().into_owned(),
        StackValue::Double(d) => panic!("PRINTF is a string, got {d}"),
    }
}

/// The scan starts AFTER the last `%%`, so a conversion sitting before one is
/// not found — and with no conversion found, C copies the format RAW.
#[test]
fn a_format_with_no_live_conversion_is_copied_verbatim() {
    // Port before R11-5: "5%", "100%", "3.14%".
    assert_eq!(pf("PRINTF(\"%d%%\", 5)"), "%d%%");
    assert_eq!(pf("PRINTF(\"100%%\", 5)"), "100%%");
    assert_eq!(pf("PRINTF(\"%.2f%%\", 3.14159)"), "%.2f%%");
    assert_eq!(pf("PRINTF(\"50%% done\", 1)"), "50%% done");
    // No `%` at all, or no conversion character after it.
    assert_eq!(pf("PRINTF(\"no conv\", 1)"), "no conv");
    assert_eq!(pf("PRINTF(\"%\", 1)"), "%");
    assert_eq!(pf("PRINTF(\"%z\", 1)"), "%z");
}

/// ...but when the scan DOES find one, the whole format goes to snprintf, so the
/// earlier `%%` collapse after all.
#[test]
fn a_live_conversion_collapses_the_earlier_double_percents() {
    assert_eq!(pf("PRINTF(\"a%%b %5.2f!\", 3.14159)"), "a%b  3.14!");
}

#[test]
fn width_flags_and_precision_are_snprintfs() {
    // Port before R11-5: the width was dropped for floats ("3.14"), and %c/%u
    // were rejected outright.
    assert_eq!(pf("PRINTF(\"%5.2f\", 3.14159)"), " 3.14");
    assert_eq!(pf("PRINTF(\"%-8dX\", 42)"), "42      X");
    assert_eq!(pf("PRINTF(\"%05d\", 42)"), "00042");
    assert_eq!(pf("PRINTF(\"%+d\", 5)"), "+5");
    assert_eq!(pf("PRINTF(\"% d\", 5)"), " 5");
    assert_eq!(pf("PRINTF(\"%10.3e|\", 1234.5)"), " 1.234e+03|");
    assert_eq!(pf("PRINTF(\"%e\", 1234.5678)"), "1.234568e+03");
    assert_eq!(pf("PRINTF(\"%E\", 1234.5678)"), "1.234568E+03");
    assert_eq!(pf("PRINTF(\"%g\", 0.0001)"), "0.0001");
    assert_eq!(pf("PRINTF(\"%g\", 0.0000001)"), "1e-07");
    assert_eq!(pf("PRINTF(\"%G\", 0.0000001)"), "1E-07");
    assert_eq!(pf("PRINTF(\"%#g\", 1.5)"), "1.50000");
    assert_eq!(pf("PRINTF(\"%#.0f\", 2)"), "2.");
    // glibc's %f rounds half to even, like Rust's.
    assert_eq!(pf("PRINTF(\"%.0f\", 2.5)"), "2");
    assert_eq!(pf("PRINTF(\"%.0f\", 3.5)"), "4");
}

/// The integer conversions take `l = myNINT(d)` — rounded half away from zero —
/// and snprintf then reads an INT out of it, so `%x`/`%u` are 32-bit.
#[test]
fn the_integer_conversions_are_c_ints() {
    assert_eq!(pf("PRINTF(\"%d\", 2.6)"), "3");
    assert_eq!(pf("PRINTF(\"%d\", -2.6)"), "-3");
    // Port before R11-5: "ffffffffffffffff" (it formatted an i64 as u64).
    assert_eq!(pf("PRINTF(\"%x\", -1)"), "ffffffff");
    assert_eq!(pf("PRINTF(\"%u\", -1)"), "4294967295");
    assert_eq!(pf("PRINTF(\"%X\", 255)"), "FF");
    assert_eq!(pf("PRINTF(\"%#x\", 255)"), "0xff");
    assert_eq!(pf("PRINTF(\"%o\", 8)"), "10");
    // Port before R11-5: %c was an InvalidFormat error.
    assert_eq!(pf("PRINTF(\"%c\", 65)"), "A");
}

#[test]
fn the_string_conversion_takes_width_and_precision() {
    assert_eq!(pf_str("%s|", "abc"), "abc|");
    assert_eq!(pf_str("%8s|", "abc"), "     abc|");
    assert_eq!(pf_str("%-8s|", "abc"), "abc     |");
    // Port before R11-5: "abcdef|" — the precision was parsed and then ignored.
    assert_eq!(pf_str("%.2s|", "abcdef"), "ab|");
    assert_eq!(pf_str("x%%y%s", "abc"), "x%yabc");
}

/// C's `case '*': return(-1)` — an assign-suppressed conversion fails PRINTF.
#[test]
fn a_suppressed_conversion_fails_the_perform() {
    let mut inp = StringInputs::new();
    assert!(matches!(
        scalc("PRINTF(\"%*d\", 42)", &mut inp),
        Err(CalcError::InvalidFormat)
    ));
}

/// Negative control: the format operand must be a string (C `if (isDouble(ps))
/// return(-1)`), and the result is bounded — at THIRTY-EIGHT bytes, because
/// PRINTF copies its result back with `strNcpy(ps->s, tmpstr,
/// SCALC_STRING_SIZE-1)` (`sCalcPerform.c:1566`) and `strNcpy` stops at `N-1`.
/// Compiled sCalc: `PRINTF("%50.2f",1)` has strlen 38. R11-2 read this bound as
/// 39 (R12-8).
#[test]
fn the_result_is_still_a_bounded_scalc_string() {
    let mut inp = StringInputs::new();
    assert!(
        scalc("PRINTF(1, 2)", &mut inp).is_err(),
        "format must be a string"
    );
    assert_eq!(pf("PRINTF(\"%50.2f\", 1)").len(), 38);
}
