//! R9-8 — symbols the synApps element tables spell that the port never lexed.
//!
//! * aCalc: `@` (A_FETCH, `aCalcPostfix.c:93`), `@@` (A_AFETCH, `:94`), `AVAL`
//!   (`:118`), `ANEG`/`APOS` (`:153-154`), `LEN` (`:197`), `[` / `{`
//!   (SUBRANGE / SUBRANGE_IP, `:210-211`), `R2S`/`S2R` (`:184,193`).
//! * sCalc: `R2S`/`S2R` (`sCalcPostfix.c:136,173`), the `$E $P $R $S $T $W`
//!   aliases (`:176-194`), and `-|` (`:243`, the SUB opcode — "subtract first
//!   occurrence", the mirror of `|-`).
//!
//! An expression using any of them failed to compile (CALC_ERR_SYNTAX ->
//! CALC_ALARM/INVALID) where C compiles and runs it.
//!
//! Every expectation below is a line printed by `sCalcPostfix.c` +
//! `sCalcPerform.c` + `aCalcPostfix.c` + `aCalcPerform.c`, built standalone out
//! of `epics-modules/calc/calcApp/src` and driven with these expressions and
//! these inputs.

use epics_base_rs::calc::{
    ArrayInputs, ArrayStackValue, CalcError, StackValue, StringInputs, acalc, compile, scalc,
};

/// The C driver's aCalc inputs: arraySize 6, AA = [10..60], BB = [-1,2,-3,4,-5,6],
/// A = 1, B = 2, C = 3, and a previous AVAL (C `p_aresult`) of [7..12].
fn a_inputs() -> ArrayInputs {
    let mut inp = ArrayInputs::new(6);
    inp.arrays[0] = vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0];
    inp.arrays[1] = vec![-1.0, 2.0, -3.0, 4.0, -5.0, 6.0];
    inp.num_vars[0] = 1.0;
    inp.num_vars[1] = 2.0;
    inp.num_vars[2] = 3.0;
    inp.prev_aval = vec![7.0, 8.0, 9.0, 10.0, 11.0, 12.0];
    inp
}

fn arr(expr: &str) -> Vec<f64> {
    match acalc(expr, &mut a_inputs()).expect("aCalcPerform returns st=0 here") {
        ArrayStackValue::Array(v) => v.into_buf(),
        ArrayStackValue::Double(d) => panic!("{expr}: expected an array result, got {d}"),
    }
}

fn num(expr: &str) -> f64 {
    match acalc(expr, &mut a_inputs()).expect("aCalcPerform returns st=0 here") {
        ArrayStackValue::Double(d) => d,
        ArrayStackValue::Array(v) => panic!("{expr}: expected a scalar result, got {v:?}"),
    }
}

/// The C driver's sCalc inputs: AA = "abcabc", BB = "bc".
fn s(expr: &str) -> StackValue {
    let mut inp = StringInputs::new();
    inp.str_vars[0] = "abcabc".into();
    inp.str_vars[1] = "bc".into();
    scalc(expr, &mut inp).expect("sCalcPerform returns st=0 here")
}

fn s_str(expr: &str) -> String {
    match s(expr) {
        StackValue::Str(v) => v.as_str_lossy().into_owned(),
        StackValue::Double(d) => panic!("{expr}: expected a string result, got {d}"),
    }
}

fn s_num(expr: &str) -> f64 {
    match s(expr) {
        StackValue::Double(v) => v,
        StackValue::Str(v) => panic!("{expr}: expected a double result, got {v:?}"),
    }
}

// --- aCalc: @ and @@ ------------------------------------------------------

/// `@x` (A_FETCH, `aCalcPerform.c:1461-1477`) is the scalar argument x INDEXES:
/// `@1` is B. The index is rounded with `myNINT`, so `@1.6` is C.
/// C: `@0` -> 1, `@1` -> 2, `@1.6` -> 3.
#[test]
fn dyn_fetch_indexes_the_scalar_args() {
    assert_eq!(num("@0"), 1.0);
    assert_eq!(num("@1"), 2.0);
    assert_eq!(num("@1.6"), 3.0);
}

/// C `:1459-1462` prints "fetch index out of range" and answers 0 — it does not
/// fail the calculation (`perform` still returns 0).
#[test]
fn dyn_fetch_out_of_range_is_zero() {
    assert_eq!(num("@99"), 0.0);
    assert_eq!(num("@-1"), 0.0);
}

/// `@@x` (A_AFETCH, `:1468-1483`) is the same one dimension up: `@@0` is AA,
/// `@@1` is BB — and the result is an ARRAY even when the index misses
/// (C `toArray(ps,0)` before the range test).
#[test]
fn dyn_afetch_indexes_the_array_args() {
    assert_eq!(arr("@@0"), vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0]);
    assert_eq!(arr("@@1"), vec![-1.0, 2.0, -3.0, 4.0, -5.0, 6.0]);
    // C `:1479-1482` — an argument the record never allocated is all zeros.
    // myNINT(1.6) = 2, i.e. CC, which this driver leaves unset.
    assert_eq!(arr("@@1.6"), vec![0.0; 6]);
    assert_eq!(arr("@@99"), vec![0.0; 6]);
}

// --- aCalc: AVAL ---------------------------------------------------------

/// `AVAL` (FETCH_AVAL, `:529-534`) pushes `p_aresult` — the record's previous
/// ARRAY result, the counterpart of `VAL`.
#[test]
fn aval_pushes_the_previous_array_result() {
    assert_eq!(arr("AVAL"), vec![7.0, 8.0, 9.0, 10.0, 11.0, 12.0]);
    assert_eq!(arr("AVAL+1"), vec![8.0, 9.0, 10.0, 11.0, 12.0, 13.0]);
}

// --- aCalc: ANEG / APOS --------------------------------------------------

/// `ANEG` zeroes the NEGATIVE elements (`:772` array, `:1046` scalar) and `APOS`
/// the POSITIVE ones (`:773`, `:1036`) — the name says which sign it REMOVES.
#[test]
fn aneg_and_apos_zero_the_named_sign() {
    assert_eq!(arr("ANEG(BB)"), vec![0.0, 2.0, 0.0, 4.0, 0.0, 6.0]);
    assert_eq!(arr("APOS(BB)"), vec![-1.0, 0.0, -3.0, 0.0, -5.0, 0.0]);
    assert_eq!(num("ANEG(-5)"), 0.0);
    assert_eq!(num("ANEG(5)"), 5.0);
    assert_eq!(num("APOS(-5)"), -5.0);
    assert_eq!(num("APOS(5)"), 0.0);
}

// --- aCalc: LEN ----------------------------------------------------------

/// aCalc's `LEN` is in the element table (`aCalcPostfix.c:199`) but
/// `aCalcPerform`'s switch has no `case LEN` and no `default:` — C's own table
/// comment is "Array length not implemented". The opcode falls straight through
/// and leaves the operand untouched.
///
/// Compiled C: `LEN(AA)` is AA, and `LEN(A)` is A. It is a no-op, not a length,
/// and not an error.
#[test]
fn acalc_len_is_a_no_op() {
    assert_eq!(arr("LEN(AA)"), vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0]);
    assert_eq!(num("LEN(A)"), 1.0);
}

// --- aCalc: [ ] and { } --------------------------------------------------

/// SUBRANGE (`aCalcPerform.c:1536-1541`): BOTH bounds are inclusive, the
/// selected elements are
/// SHIFTED DOWN to index 0, and the tail is zero-filled — the result is still a
/// full `arraySize` buffer.
#[test]
fn subrange_shifts_the_selection_to_the_front() {
    assert_eq!(arr("AA[1,3]"), vec![20.0, 30.0, 40.0, 0.0, 0.0, 0.0]);
    // The bounds are truncating `(int)` casts (`:1516,1520`), not myNINT.
    assert_eq!(arr("AA[1.7,3.9]"), vec![20.0, 30.0, 40.0, 0.0, 0.0, 0.0]);
}

/// SUBRANGE_IP (`:1543-1547`): the selection stays WHERE IT IS and everything
/// outside it is zeroed. This is what makes `{` different from `[`.
#[test]
fn subrange_in_place_keeps_the_positions() {
    assert_eq!(arr("AA{1,3}"), vec![0.0, 20.0, 30.0, 40.0, 0.0, 0.0]);
    assert_eq!(arr("AA{2,-1}"), vec![0.0, 0.0, 30.0, 40.0, 50.0, 60.0]);
}

/// A negative bound counts back from the end (`:1517,1521`: `if (i < 0) i += arraySize`).
#[test]
fn a_negative_subrange_bound_wraps_to_the_end() {
    assert_eq!(arr("AA[-2,5]"), vec![50.0, 60.0, 0.0, 0.0, 0.0, 0.0]);
    assert_eq!(arr("AA[-2,-1]"), vec![50.0, 60.0, 0.0, 0.0, 0.0, 0.0]);
    assert_eq!(arr("AA[2,-1]"), vec![30.0, 40.0, 50.0, 60.0, 0.0, 0.0]);
}

/// `i > j` selects nothing: C's copy loop never runs and the zero fill covers
/// the whole buffer. Both forms.
#[test]
fn an_inverted_subrange_is_all_zeros() {
    assert_eq!(arr("AA[3,1]"), vec![0.0; 6]);
    assert_eq!(arr("AA{3,1}"), vec![0.0; 6]);
}

/// C `toArray(ps,1)` (`:1525`) promotes a SCALAR operand first, so `A[0,2]` is a
/// legal subrange of the array A broadcasts to.
#[test]
fn a_scalar_is_promoted_before_the_subrange() {
    assert_eq!(arr("A[0,2]"), vec![1.0, 1.0, 1.0, 0.0, 0.0, 0.0]);
}

/// C `toDouble(ps1)` (`:1526`) collapses an ARRAY bound to its first element —
/// BB[0] is -1, which wraps to 5, so `AA[BB,3]` selects nothing.
#[test]
fn an_array_subrange_bound_collapses_to_its_first_element() {
    assert_eq!(arr("AA[BB,3]"), vec![0.0; 6]);
}

/// The zero fill is part of the value: the buffer stays `arraySize` long, so a
/// following element-wise operator sees the zeros.
/// C: `AA[1,3]+1` -> [21,31,41,1,1,1].
#[test]
fn the_subrange_zero_fill_is_part_of_the_buffer() {
    assert_eq!(arr("AA[1,3]+1"), vec![21.0, 31.0, 41.0, 1.0, 1.0, 1.0]);
}

// --- R2S / S2R -----------------------------------------------------------

/// `S2R` = PI/(180*3600), `R2S` = its reciprocal — arcseconds <-> radians
/// (`sCalcPerform.c:952-962`, `aCalcPerform.c:559-569`). BOTH synApps engines
/// have them; base does not.
#[test]
fn arcsecond_constants_in_both_synapps_engines() {
    let s2r = std::f64::consts::PI / (180.0 * 3600.0);
    let r2s = (180.0 * 3600.0) / std::f64::consts::PI;
    assert!((s_num("S2R") - s2r).abs() < 1e-18, "sCalc S2R");
    assert!((s_num("R2S") - r2s).abs() < 1e-9, "sCalc R2S");
    assert!((num("S2R") - s2r).abs() < 1e-18, "aCalc S2R");
    assert!((num("R2S") - r2s).abs() < 1e-9, "aCalc R2S");
    // The C driver's printed values.
    assert!((s_num("R2S") - 206264.806247).abs() < 1e-5);
    assert!((s_num("S2R") - 4.84814e-06).abs() < 1e-10);
}

/// Base's element table (`postfix.c`) has neither, so base still rejects them —
/// and the port's shared tokenizer must not leak them in.
#[test]
fn base_has_no_arcsecond_constants() {
    assert_eq!(compile("R2S").err(), Some(CalcError::Syntax));
    assert_eq!(compile("S2R").err(), Some(CalcError::Syntax));
    // ...nor any of aCalc's array symbols.
    assert_eq!(compile("@1").err(), Some(CalcError::Syntax));
    assert_eq!(compile("AVAL").err(), Some(CalcError::Syntax));
}

// --- sCalc: -| and the $ aliases -----------------------------------------

/// `-|` (`sCalcPostfix.c:243`) compiles to the SUB opcode — the same one `-`
/// does — so it removes the FIRST occurrence, exactly like `-`. `|-` (SUBLAST,
/// `:244`) removes the last.
/// C: AA="abcabc", BB="bc" -> `AA-|BB` = "aabc" (= `AA-BB`), `AA|-BB` = "abca".
#[test]
fn scalc_sub_first_occurrence_operator() {
    assert_eq!(s_str("AA-|BB"), "aabc");
    assert_eq!(s_str("AA-BB"), "aabc");
    assert_eq!(s_str("AA|-BB"), "abca");
}

/// `$T $P $S $E $R $W` (`sCalcPostfix.c:176-194`) are aliases of TR_ESC, PRINTF,
/// SSCANF, ESC, BIN_READ and BIN_WRITE — the same opcodes the long names emit.
#[test]
fn scalc_dollar_aliases() {
    // The calc source is `$T("a\nb")` — ONE backslash. The lexer copies the
    // literal raw (sCalcPostfix.c:803-812) and `$T` is the only translator.
    assert_eq!(s_str("$T(\"a\\nb\")"), "a\nb"); // TR_ESC
    assert_eq!(s_str("$P(\"%d\",65)"), "65"); // PRINTF
    assert_eq!(s_num("$S(\"12\",\"%d\")"), 12.0); // SSCANF
    assert_eq!(s_str("$E(\"a\tb\")"), "a\\tb"); // ESC
    // BIN_READ / BIN_WRITE: pinned at the compile boundary — this test's subject
    // is that the alias LEXES; their run-time behaviour is the long name's.
    assert!(epics_base_rs::calc::scalc_compile("$R(\"AB\",\"%2c\")").is_ok());
    assert!(epics_base_rs::calc::scalc_compile("$W(65,\"%c\")").is_ok());
}

/// sCalc's `LEN` is still the STRING length (`sCalcPostfix.c:202`), and its
/// `[`/`{` are still the string slice and the string replace — the two engines
/// spell the same symbols differently and each keeps its own meaning.
#[test]
fn scalc_keeps_its_string_meanings() {
    assert_eq!(s_num("LEN(AA)"), 6.0);
    assert_eq!(s_str("AA[1,3]"), "bca");
    assert_eq!(s_str("AA{\"bc\",\"X\"}"), "aXabc");
}
