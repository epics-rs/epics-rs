//! `recGblGetPrec` is the shared tail of every C `get_precision`, and for a
//! field that can carry a precision at all it does exactly one thing.
//!
//! ```c
//! /* recGbl.c:135-139 */
//! case DBF_FLOAT:
//! case DBF_DOUBLE:
//!     if (*precision < 0 || *precision > 15)
//!         *precision = 15;
//!     break;
//! ```
//!
//! `dbAccess.c:388-389` gates `DBR_PRECISION` on the field being
//! `DBF_FLOAT`/`DBF_DOUBLE` before `get_precision` runs at all, so the switch's
//! integer arm (`*precision = 0`) is unreachable through `dbGet` — the clamp is
//! the whole observable effect.
//!
//! Which fields reach the tail is a per-FIELD question with a per-TYPE answer,
//! and one record answers it both ways: `aiRecord.c:238-240` is
//! `*precision = prec->prec; if (VAL) return 0; recGblGetPrec(...)`, so `.VAL`
//! serves a `PREC` of 20 as 20 and `.HOPR` serves it as 15.
//!
//! The tail had no caller — `recgbl::rec_gbl_get_prec` was transcribed and unit
//! tested against the C switch but never wired into the routing, so every
//! served field took the record's raw `PREC`.

mod module_records;

use epics_base_rs::server::database::{LinkBacking, PvDatabase};
use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;

/// The precision this record type serves for `field` with `PREC` set to
/// `prec` and every link left constant — i.e. `dbGetPrecision` on a backing
/// link fails, which is the arm each type resolves differently.
fn served(rtype: &str, prec: i16, field: &str) -> Option<i16> {
    let rec = module_records::create_any(rtype).unwrap_or_else(|e| panic!("{rtype}: {e:?}"));
    let mut inst = RecordInstance::new_boxed(format!("T:{rtype}"), rec);
    if inst
        .record
        .put_field("PREC", EpicsValue::Short(prec))
        .is_err()
    {
        match inst.put_common_field("PREC", EpicsValue::Short(prec)) {
            // `bo` and `busy` declare no PREC at all — their only precision is
            // the `HIGH` literal — so the seed is meaningless there and the
            // assertion below says what each must answer anyway.
            Ok(_) | Err(epics_base_rs::error::CaError::FieldNotFound(_)) => {}
            Err(e) => panic!("{rtype}.PREC: {e:?}"),
        }
    }
    inst.snapshot_for_field_with(field, LinkBacking::none())
        .unwrap_or_else(|| panic!("{rtype}.{field}: no snapshot"))
        .precision()
}

/// Boundary: the field C's `get_precision` names and returns on. `PREC` reaches
/// the client exactly as stored, however far out of range.
#[test]
fn the_named_value_field_returns_before_the_tail() {
    for rtype in [
        "ai", "ao", "calc", "calcout", "dfanout", "sel", "sub", "scalcout",
    ] {
        assert_eq!(
            served(rtype, 20, "VAL"),
            Some(20),
            "{rtype}.VAL: C returns before recGblGetPrec"
        );
        assert_eq!(
            served(rtype, -1, "VAL"),
            Some(-1),
            "{rtype}.VAL: and does so for a negative PREC too"
        );
    }
}

/// Boundary: a field of the SAME record that the switch does not name. This is
/// the pair the fix exists for — one record, two answers.
#[test]
fn a_field_the_switch_does_not_name_is_clamped_to_fifteen() {
    for (rtype, field) in [
        ("ai", "HOPR"),
        ("ai", "HIHI"),
        ("ao", "HOPR"),
        ("calc", "HOPR"),
        ("dfanout", "HOPR"),
        ("scalcout", "HOPR"),
    ] {
        assert_eq!(
            served(rtype, 20, field),
            Some(15),
            "{rtype}.{field}: recGbl.c:137-138 clamps a PREC above 15"
        );
        assert_eq!(
            served(rtype, -1, field),
            Some(15),
            "{rtype}.{field}: and clamps a negative one to 15, not to 0"
        );
    }
}

/// Boundary: the tail is an identity inside `0..=15`, so wiring it must not
/// move an ordinary PREC.
#[test]
fn an_in_range_prec_passes_the_tail_unchanged() {
    for prec in [0, 3, 15] {
        assert_eq!(served("ai", prec, "HOPR"), Some(prec), "PREC={prec}");
        assert_eq!(served("ai", prec, "VAL"), Some(prec), "PREC={prec}");
    }
}

/// Boundary: `ao` names three fields, not one — `case VAL: case OVAL:
/// case PVAL: break;` (`aoRecord.c:305-312`).
#[test]
fn aos_two_extra_named_fields_are_exempt_with_val() {
    for field in ["OVAL", "PVAL"] {
        assert_eq!(
            served("ao", 20, field),
            Some(20),
            "ao.{field}: named alongside VAL, so no tail"
        );
    }
}

/// Boundary: a link-backed argument over a CONSTANT link. C's link arm returns
/// whether or not `dbGetPrecision` answered (`calcRecord.c:194-201`,
/// `subRecord.c:231-238`), so the seed stands unclamped.
#[test]
fn a_link_backed_argument_never_reaches_the_tail() {
    for rtype in ["calc", "calcout", "sub"] {
        assert_eq!(
            served(rtype, 20, "A"),
            Some(20),
            "{rtype}.A: the link arm returns without recGblGetPrec"
        );
    }
}

/// Boundary: `seq` is the one type whose link arm falls INTO the tail. `case 2:`
/// returns only when `dbGetPrecision` succeeded; a `DOn` over a constant `DOLn`
/// drops out of the switch to `*pprecision = prec->prec; recGblGetPrec(...)`
/// (`seqRecord.c:310-317`) — the opposite answer to `calc.A` above.
#[test]
fn seqs_do_field_reaches_the_tail_when_its_dol_answers_nothing() {
    assert_eq!(
        served("seq", 20, "DO0"),
        Some(15),
        "seq.DO0 over a constant DOL0 is clamped where calc.A is not"
    );
    assert_eq!(
        served("seq", 3, "DO0"),
        Some(3),
        "and is untouched in range"
    );
}

/// Boundary: a literal arm returns before the tail, and every literal C uses is
/// already inside the clamp's range — so wiring the tail must not disturb one.
#[test]
fn the_literal_arms_still_win() {
    assert_eq!(served("seq", 20, "DLY0"), Some(2), "seqDLYprecision");
    assert_eq!(
        served("calcout", 20, "ODLY"),
        Some(2),
        "calcoutODLYprecision"
    );
    assert_eq!(served("bo", 20, "HIGH"), Some(2), "boHIGHprecision");
    assert_eq!(served("busy", 20, "HIGH"), Some(2), "busyRecord.c:281");
    assert_eq!(served("swait", 20, "ODLY"), Some(3), "swaitRecord.c:591");
}

/// Boundary: a type whose `get_precision` has no `recGblGetPrec` at all
/// (`swaitRecord.c:583-595`). Every field keeps PREC, out of range included.
#[test]
fn a_type_with_no_tail_keeps_prec_on_every_field() {
    for field in ["VAL", "A", "B"] {
        assert_eq!(
            served("swait", 20, field),
            Some(20),
            "swait.{field}: swait's body never calls recGblGetPrec"
        );
    }
}

/// Boundary: `sel`'s argument loop compares `paddr->pfield` against `&pvalue`
/// and `&plvalue` — the addresses of the two LOCAL pointers, not of the fields
/// they walk (`selRecord.c:158-163`) — so it never matches and every `sel`
/// argument DOES reach the tail. The port transcribes how C behaves, not how it
/// reads; `sel.VAL` above is the only exemption C actually delivers.
#[test]
fn sels_argument_loop_never_matches_so_its_args_are_clamped() {
    assert_eq!(
        served("sel", 20, "A"),
        Some(15),
        "selRecord.c:159 takes the address of the loop variable"
    );
}

/// Boundary: the switch's integer arm must stay unobservable. C never calls
/// `get_precision` for a non-float field (`dbAccess.c:388-389`), so a clamp
/// wired into the routing must not start answering `0` for one.
#[test]
fn an_integer_field_still_supplies_no_precision() {
    for (rtype, field) in [("ai", "RVAL"), ("ao", "RVAL"), ("seq", "SELN")] {
        assert_eq!(
            served(rtype, 20, field),
            None,
            "{rtype}.{field} is not DBF_FLOAT/DOUBLE"
        );
    }
}

/// Boundary: a link that DOES answer. `dbGetPrecision` returns the target's own
/// `get_precision` for its VAL — which is unclamped — and C's link arm returns
/// with it, so an out-of-range PREC crosses the link intact.
#[epics_macros_rs::epics_test]
async fn a_precision_that_arrives_over_a_connected_link_is_not_clamped() {
    let db = PvDatabase::new();
    let mut src = AiRecord::new(1.0);
    src.prec = 20;
    db.add_record("SRC", Box::new(src)).await.unwrap();

    let mut calc = epics_base_rs::server::records::calc::CalcRecord::default();
    calc.inpa = "SRC".into();
    calc.prec = 3;
    db.add_record("CALC", Box::new(calc)).await.unwrap();
    db.ioc_init().await;

    let rec = db.get_record("CALC").unwrap();
    let served = db
        .channel_snapshot_for_field(&rec, "A", false)
        .expect("CALC.A serves a snapshot")
        .precision();
    assert_eq!(
        served,
        Some(20),
        "the link's own answer stands; only the tail clamps"
    );
}
