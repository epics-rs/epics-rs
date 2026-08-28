//! R15-1 — the engines are bounded by the CALLER's argument counts, not by the
//! size of their own arrays.
//!
//! C's `parg`/`psarg`/`p_dArg`/`pp_aArg` are bare pointers into the caller's
//! record, so `sCalcPerform`/`aCalcPerform` take the counts alongside them and
//! guard EVERY access:
//!
//! ```c
//! case FETCH_A ... FETCH_P:
//!     if (numArgs > (op - FETCH_A)) { ... } else { *++pd = 0.; }   /* :421-427 */
//! case STORE_A ... STORE_P:
//!     if (numArgs > (op - STORE_A)) parg[op - STORE_A] = ps->d;    /* :882-886 */
//! case FETCH_AA ... FETCH_LL:
//!     if (numSArgs > (op - FETCH_AA)) strncpy(...);                /* :871 */
//! case STORE_AA ... STORE_LL:
//!     if (numSArgs > (op - STORE_AA)) strncpy(...);                /* :891 */
//! case A_FETCH:  if (i >= numArgs  || i < 0) { ps->d = 0; } ...    /* :902, :1454 */
//! case A_SSTORE: if (i >= numSArgs || i < 0) { ... }               /* :914, :1471 */
//! ```
//!
//! and aCalc the same with `num_dArgs` / `num_aArgs` (`aCalcPerform.c:432`,
//! `:441`, `:457`, `:466`, `:494`, `:505`, `:1458`, `:1474`). Out of range: a
//! fetch is 0 / "" / a zero array, a store is a SILENT no-op — no error either
//! way.
//!
//! The counts come from the caller and they disagree:
//!
//! | caller    | numeric | string / array |                                  |
//! |-----------|---------|----------------|----------------------------------|
//! | scalcout  | 12      | 12             | `sCalcoutRecord.c:357,768`       |
//! | transform | 16      | **0**          | `transformRecord.c:593` (NULL)   |
//! | acalcout  | 12      | 12             | `aCalcoutRecord.c:1283,1288`     |
//!
//! The port's arrays hold [`CALC_NARGS`] (21) of each, so before this the engines
//! read and wrote slots no caller had supplied. The counts now travel WITH the
//! args, inside `StringInputs` / `ArrayInputs`, so the bound holds at the struct
//! and no access site has to remember it.
//!
//! One case per boundary: the last supplied index, the first unsupplied one, the
//! static name past the count, transform's zero string args, `@@` at the array
//! bound, and a negative index.

//! R19-92 extends the same rule to the NUMERIC engine, whose C has no count at
//! all: `calcPerform(double *parg, ...)` indexes all 21 args out of the caller's
//! pointer unconditionally. `swaitRecord` supplies only twelve (`swaitRecord.dbd`
//! declares A..L, then LA..LL), so in C `parg[12]` IS `&pwait->la` — `CALC="M"`
//! reads the previous A and `CALC="M:=5"` corrupts the record's change-detection
//! latch (CBUG-G3). The port does not reproduce that: swait hands the engine its
//! count, so M..U do not exist there — fetch 0, store nowhere, exactly as sCalc
//! and aCalc already spell out for their own short callers.
//!
//! | caller             | numeric args |                                       |
//! |--------------------|--------------|---------------------------------------|
//! | calc / calcout     | 21           | `calcRecord.dbd` declares A..U        |
//! | lnkCalc, asLib ASG | 21           | both allocate `CALCPERFORM_NARGS`     |
//! | swait              | **12**       | `swaitRecord.c:409` `&pwait->a` = A..L |

use std::collections::HashSet;

use epics_base_rs::calc::{
    ArrayInputs, ArrayStackValue, CALC_NARGS, NumericInputs, ScalcString, StackValue, StringInputs,
    acalc, calc, scalc,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::swait::SwaitRecord;

/// scalcout's counts: `MAX_FIELDS` / `STRING_MAX_FIELDS`, both 12.
fn scalcout_inputs() -> StringInputs {
    StringInputs::with_counts(12, 12)
}

/// transform's counts: 16 numeric channels, and NO string args at all.
fn transform_inputs() -> StringInputs {
    StringInputs::with_counts(16, 0)
}

/// acalcout's counts: `MAX_FIELDS` / `ARRAY_MAX_FIELDS`, both 12, over a
/// 4-element array window.
fn acalcout_inputs() -> ArrayInputs {
    ArrayInputs::with_counts(4, 12, 12)
}

fn d(v: StackValue) -> f64 {
    v.to_double()
}

// ---------------------------------------------------------------------------
// sCalc — numeric args, index == count-1 (in) vs == count (out)
// ---------------------------------------------------------------------------

/// The LAST supplied numeric arg — `@11` is L, index 11, and 12 > 11. C stores
/// and fetches it: `@11:=5;@11` → 5.
#[test]
fn scalc_dynamic_numeric_arg_at_the_last_supplied_index_is_stored() {
    let mut inputs = scalcout_inputs();
    assert_eq!(d(scalc("@11:=5;@11", &mut inputs).unwrap()), 5.0);
    assert_eq!(inputs.num_vars[11], 5.0);
}

/// The FIRST unsupplied one. C: `12 >= numArgs` — the store is dropped and the
/// fetch answers 0, so `@12:=5;@12` is 0. The port wrote a phantom slot and
/// answered 5.
#[test]
fn scalc_dynamic_numeric_arg_past_the_count_stores_nothing_and_fetches_zero() {
    let mut inputs = scalcout_inputs();
    assert_eq!(d(scalc("@12:=5;@12", &mut inputs).unwrap()), 0.0);
    assert_eq!(inputs.num_vars[12], 0.0, "the store must not land");
}

/// The STATIC name obeys the same count — C guards `FETCH_A..P` / `STORE_A..P`
/// with it too (`:858`, `:881`). `M` is index 12, one past scalcout's 12.
#[test]
fn scalc_static_numeric_name_past_the_count_stores_nothing_and_fetches_zero() {
    let mut inputs = scalcout_inputs();
    assert_eq!(d(scalc("M:=5;M", &mut inputs).unwrap()), 0.0);
    assert_eq!(inputs.num_vars[12], 0.0, "the store must not land");
}

/// ...and `L`, the last one that IS supplied, still works — the bound is at 12,
/// not below it.
#[test]
fn scalc_static_numeric_name_at_the_last_supplied_index_is_stored() {
    let mut inputs = scalcout_inputs();
    assert_eq!(d(scalc("L:=5;L", &mut inputs).unwrap()), 5.0);
    assert_eq!(inputs.num_vars[11], 5.0);
}

/// A negative index was already refused (`i < 0` in every C guard) — kept as the
/// low boundary of the same rule.
#[test]
fn scalc_negative_dynamic_index_stores_nothing_and_fetches_zero() {
    let mut inputs = scalcout_inputs();
    assert_eq!(d(scalc("@-1:=5;@-1", &mut inputs).unwrap()), 0.0);
    assert_eq!(inputs.num_vars[0], 0.0);
}

// ---------------------------------------------------------------------------
// sCalc — string args, and transform's numSArgs == 0
// ---------------------------------------------------------------------------

/// scalcout supplies 12 string args, so `@@11` (LL) is the last one that exists.
#[test]
fn scalc_dynamic_string_arg_at_the_last_supplied_index_is_stored() {
    let mut inputs = scalcout_inputs();
    let out = scalc("@@11:=\"zz\";@@11", &mut inputs).unwrap();
    assert_eq!(out, StackValue::Str(ScalcString::from_c(b"zz")));
    assert_eq!(inputs.str_vars[11], ScalcString::from_c(b"zz"));
}

/// `@@12` is past `numSArgs`: C stores nothing and the fetch is the EMPTY string
/// (C empties the cell before the range test, `:1468-1470`).
#[test]
fn scalc_dynamic_string_arg_past_the_count_stores_nothing_and_fetches_empty() {
    let mut inputs = scalcout_inputs();
    let out = scalc("@@12:=\"zz\";@@12", &mut inputs).unwrap();
    assert_eq!(out, StackValue::Str(ScalcString::new()));
    assert_eq!(inputs.str_vars[12], ScalcString::new(), "no phantom store");
}

/// transform passes `psarg = NULL, numSArgs = 0` (`transformRecord.c:593`): the
/// record HAS no string fields. So `AA:="x"` stores nowhere and `LEN(AA)` is 0 —
/// C's `numSArgs > 0` is false for every string arg. The port answered 1.
#[test]
fn transform_has_zero_string_args_so_a_static_string_store_lands_nowhere() {
    let mut inputs = transform_inputs();
    assert_eq!(d(scalc("AA:=\"x\";LEN(AA)", &mut inputs).unwrap()), 0.0);
    assert_eq!(inputs.str_vars[0], ScalcString::new(), "no phantom store");
}

/// The dynamic form under the same count — `@@0` is AA, and AA does not exist.
#[test]
fn transform_has_zero_string_args_so_a_dynamic_string_store_lands_nowhere() {
    let mut inputs = transform_inputs();
    assert_eq!(d(scalc("@@0:=\"x\";LEN(@@0)", &mut inputs).unwrap()), 0.0);
    assert_eq!(inputs.str_vars[0], ScalcString::new(), "no phantom store");
}

/// transform's SIXTEEN numeric args are all real — `P` is index 15, and 16 > 15.
/// The string count being 0 does not narrow the numeric one.
#[test]
fn transform_supplies_sixteen_numeric_args() {
    let mut inputs = transform_inputs();
    assert_eq!(d(scalc("P:=5;P", &mut inputs).unwrap()), 5.0);
    assert_eq!(inputs.num_vars[15], 5.0);
}

// ---------------------------------------------------------------------------
// aCalc — num_dArgs / num_aArgs, and the AMASK bit that must not be set
// ---------------------------------------------------------------------------

/// `@@11` is LL, the last array arg acalcout supplies: the store lands and C
/// flags it in `*amask` (`aCalcPerform.c:523-524`), which is how the record knows
/// to post the field.
#[test]
fn acalc_array_store_at_the_last_supplied_index_sets_its_amask_bit() {
    let mut inputs = acalcout_inputs();
    acalc("@@11:=1;0", &mut inputs).unwrap();
    assert_eq!(inputs.arrays[11], vec![1.0; 4]);
    assert_eq!(inputs.amask, 1 << 11);
}

/// `@@12` is past `num_aArgs`. C's store AND its `*amask |= 1<<i` both sit inside
/// the guard, so a refused store must leave the mask at 0 — the port set bit 12
/// and the record would have posted an array field that does not exist.
#[test]
fn acalc_array_store_past_the_count_stores_nothing_and_sets_no_amask_bit() {
    let mut inputs = acalcout_inputs();
    acalc("@@12:=1;0", &mut inputs).unwrap();
    assert!(inputs.arrays[12].is_empty(), "no phantom store");
    assert_eq!(inputs.amask, 0, "a refused store must not flag the field");
}

/// The static name is the same store with a constant index (`STORE_AA..LL`,
/// `:461-486`) — and aCalc names only AA..LL, so its bound is reached through the
/// dynamic form. Here: the static one at the last supplied index still works.
#[test]
fn acalc_static_array_store_at_the_last_supplied_index_sets_its_amask_bit() {
    let mut inputs = acalcout_inputs();
    acalc("LL:=1;0", &mut inputs).unwrap();
    assert_eq!(inputs.arrays[11], vec![1.0; 4]);
    assert_eq!(inputs.amask, 1 << 11);
}

/// An array arg past the count FETCHES as a zero buffer of `arraySize` — C's
/// `toArray(ps,0)` runs before the `num_aArgs` test, so the cell is an ARRAY
/// either way, never a scalar (`:438-449`).
#[test]
fn acalc_array_fetch_past_the_count_is_a_zero_array_not_a_scalar() {
    let mut inputs = acalcout_inputs();
    inputs.arrays[12] = vec![7.0; 4]; // present in the struct, NOT supplied by the caller
    match acalc("@@12", &mut inputs).unwrap() {
        ArrayStackValue::Array(cell) => assert_eq!(cell.into_buf(), vec![0.0; 4]),
        other => panic!("an out-of-range @@ must stay an array, got {other:?}"),
    }
}

/// aCalc's scalar args carry the same bound as sCalc's: `@11` in, `@12` out.
#[test]
fn acalc_scalar_arg_past_the_count_stores_nothing_and_fetches_zero() {
    let mut inputs = acalcout_inputs();
    match acalc("@12:=5;@12", &mut inputs).unwrap() {
        ArrayStackValue::Double(v) => assert_eq!(v, 0.0),
        other => panic!("expected a Double, got {other:?}"),
    }
    assert_eq!(inputs.num_vars[12], 0.0, "the store must not land");

    let mut inputs = acalcout_inputs();
    match acalc("@11:=5;@11", &mut inputs).unwrap() {
        ArrayStackValue::Double(v) => assert_eq!(v, 5.0),
        other => panic!("expected a Double, got {other:?}"),
    }
    assert_eq!(inputs.num_vars[11], 5.0);
}

// ---------------------------------------------------------------------------
// The default: a caller that supplies everything
// ---------------------------------------------------------------------------

/// `new()` is "all [`CALC_NARGS`] args supplied", which is what a caller with no
/// record behind it means — the bound is a fact about the CALLER, and this one
/// hands over the whole array.
#[test]
fn default_inputs_supply_every_arg() {
    let mut inputs = StringInputs::new();
    let last = CALC_NARGS - 1;
    assert_eq!(
        d(scalc(&format!("@{last}:=5;@{last}"), &mut inputs).unwrap()),
        5.0
    );
    assert_eq!(inputs.num_vars[last], 5.0);

    // ...and one past the array itself is still refused, by the array's own end.
    let mut inputs = StringInputs::new();
    assert_eq!(
        d(scalc(&format!("@{CALC_NARGS}:=5;@{CALC_NARGS}"), &mut inputs).unwrap()),
        0.0
    );
}

// ---------------------------------------------------------------------------
// R19-92 — the NUMERIC engine under a short caller (swait's twelve)
// ---------------------------------------------------------------------------

/// swait's count: `&pwait->a` spans A..L (`swaitRecord.c:409`).
fn swait_inputs() -> NumericInputs {
    NumericInputs::with_counts(12)
}

/// `L` is index 11, and 12 > 11 — the last arg swait supplies is a real one.
#[test]
fn numeric_arg_at_the_last_supplied_index_is_stored() {
    let mut inputs = swait_inputs();
    assert_eq!(calc("L:=5;L", &mut inputs).unwrap(), 5.0);
    assert_eq!(inputs.vars[11], 5.0);
}

/// `M` is index 12, the first one swait does NOT supply. C would write
/// `pwait->la` here; the port stores nowhere and fetches 0.
#[test]
fn numeric_arg_past_the_count_stores_nothing_and_fetches_zero() {
    let mut inputs = swait_inputs();
    assert_eq!(calc("M:=5;M", &mut inputs).unwrap(), 0.0);
    assert_eq!(inputs.vars[12], 0.0, "the store must not land");
}

/// The fetch is 0 because the arg does not EXIST, not because the slot happens
/// to be zero: a caller-preloaded slot past the count is still invisible.
#[test]
fn numeric_arg_past_the_count_fetches_zero_even_when_the_slot_holds_a_value() {
    let mut inputs = swait_inputs();
    inputs.vars[12] = 7.0; // present in the struct, NOT supplied by the caller
    assert_eq!(calc("M", &mut inputs).unwrap(), 0.0);
    assert_eq!(inputs.vars[12], 7.0, "and the engine did not touch it");
}

/// calc/calcout DO declare A..U (`calcRecord.dbd`), so the full-count caller
/// keeps every one of the 21 — the bound narrows swait, not the engine.
#[test]
fn a_full_count_caller_keeps_every_numeric_arg() {
    let mut inputs = NumericInputs::new();
    assert_eq!(calc("U:=5;U", &mut inputs).unwrap(), 5.0);
    assert_eq!(inputs.vars[CALC_NARGS - 1], 5.0);
}

/// A count above the engine's array clamps to it — `num_args <= vars.len()` holds
/// by construction, so no access site can be handed an index the array lacks.
#[test]
fn a_count_above_the_array_clamps_to_it() {
    let mut inputs = NumericInputs::with_counts(CALC_NARGS + 9);
    assert_eq!(calc("U:=5;U", &mut inputs).unwrap(), 5.0);
    assert_eq!(inputs.vars[CALC_NARGS - 1], 5.0);
}

/// The record-level case: swait must hand the engine its count, so an `M` in a
/// live swait's CALC is inert. In C this same database reads and rewrites LA.
#[epics_macros_rs::epics_test]
async fn a_swait_calc_cannot_reach_past_l() {
    let (db, _) = IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(
            r#"
record(swait, "W:M") {
    field(CALC, "M:=5;M")
}
record(swait, "W:L") {
    field(CALC, "L:=5;L")
}
"#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let process = async |name: &str| {
        let mut visited = HashSet::new();
        db.process_record_with_links(name, &mut visited, 0)
            .await
            .unwrap();
    };

    process("W:M").await;
    assert_eq!(
        db.get_pv("W:M").unwrap().to_f64().unwrap(),
        0.0,
        "M is not one of swait's twelve args: the store is dropped and the fetch is 0"
    );

    process("W:L").await;
    assert_eq!(
        db.get_pv("W:L").unwrap().to_f64().unwrap(),
        5.0,
        "L is the last one that exists"
    );
    assert_eq!(db.get_pv("W:L.L").unwrap().to_f64().unwrap(), 5.0);
}
