//! `get_units` / `get_precision` have literal arms, and they are per-field.
//!
//! The two slots are switches over a field list, exactly like the three
//! `get_*_double` slots — and four ported types hard-code a constant for one
//! field of their own instead of serving the record's EGU/PREC. Each literal
//! belongs to one type and one field; none is derivable from another slot:
//!
//! ```text
//! seqRecord.c:282-297       DLYn (x16) -> "s"    :299-319 -> seqDLYprecision      (= 2, :78)
//! calcoutRecord.c:425-444   ODLY       -> "s"    :446-465 -> calcoutODLYprecision (= 2, :89)
//! histogramRecord.c:409-416 SDEL       -> "s"    :418-436 -> histogramSDELprecision (= 2, :88)
//! boRecord.c:294-299        HIGH       -> "s"    :301-308 -> boHIGHprecision      (= 2, :85)
//! busyRecord.c  (no units arm)         HIGH             :277-284 -> 2 (bare literal)
//! swaitRecord.c (no units arm)         ODLY                      -> 3 (bare literal)
//! ```
//!
//! The last two are precision-only: neither rset supplies `get_units`, and
//! neither constant is exported as a settable variable the way
//! `boHIGHprecision` is.
//!
//! Measured on `softIocPVX`: before these arms existed the oracle reported 37
//! differing leaves over 19 cases — C served `"s"`/`2` where the port served
//! `""`/`0` on the 16 `seq.DLYn`, `calcout.ODLY`, `histogram.SDEL` and
//! `bo.HIGH` (units only; see `bo_high_serves_the_units_literal_the_wire_can_carry`).
//!
//! Every expectation below differs from what the surrounding default arm would
//! answer (`""` from EGU, `0` from PREC on an empty record), so a passing test
//! proves the literal arm fired rather than two paths coinciding.

use epics_base_rs::server::database::LinkBacking;
use epics_base_rs::server::record::{Record, RecordInstance};
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::server::records::histogram::HistogramRecord;
use epics_base_rs::server::records::seq::SeqRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::server::snapshot::Snapshot;

fn snap(inst: &RecordInstance, field: &str) -> Snapshot {
    inst.snapshot_for_field_with(field, LinkBacking::none())
        .unwrap_or_else(|| panic!("{field} has no snapshot"))
}

/// `(units, precision)` as the display block carries them.
fn display(inst: &RecordInstance, field: &str) -> (String, i16) {
    let d = snap(inst, field)
        .display
        .unwrap_or_else(|| panic!("{field} carries no display block"));
    (d.units.to_string(), d.precision)
}

fn inst<R: Record + 'static>(name: &str, rec: R) -> RecordInstance {
    RecordInstance::new(name.to_string(), rec)
}

/// `seqRecord.c` switches on `(fieldIndex - indexof(DLY0)) & 3`, so the DLYn
/// slot of all 16 link groups takes the literal — not just `DLY0`.
#[test]
fn every_one_of_seqs_sixteen_dly_fields_takes_the_literal_arm() {
    let inst = inst("T:SEQ", SeqRecord::default());

    for suffix in "0123456789ABCDEF".chars() {
        let field = format!("DLY{suffix}");
        assert_eq!(
            display(&inst, &field),
            ("s".to_string(), 2),
            "seqRecord.c:282-297/:299-319 answer {field} with \"s\"/seqDLYprecision"
        );
    }
}

/// The `& 3 == 2` slot (`DOn`) reads the DOLn link instead. An unset link
/// supplies no units, which is the `""` the default arm already serves — so
/// the DLYn literal must not leak onto its neighbours in the same group.
#[test]
fn the_dly_literal_does_not_leak_onto_the_link_backed_do_fields() {
    let inst = inst("T:SEQ", SeqRecord::default());

    for field in ["DO0", "DO1", "DOF"] {
        let (units, _) = display(&inst, field);
        assert_eq!(
            units, "",
            "seqRecord.c:290-292 reads {field}'s units from the DOLn link, and an \
             unset link supplies none"
        );
    }
}

/// `get_units`/`get_precision` test ODLY FIRST and return early, so the
/// record's EGU/PREC never reach it — a calcout with EGU/PREC set still serves
/// the literal.
#[test]
fn calcouts_odly_literal_beats_the_records_own_egu_and_prec() {
    let mut rec = CalcoutRecord::default();
    rec.put_field("EGU", epics_base_rs::types::EpicsValue::String("V".into()))
        .expect("put EGU");
    rec.put_field("PREC", epics_base_rs::types::EpicsValue::Short(7))
        .expect("put PREC");
    let inst = inst("T:CALCOUT", rec);

    assert_eq!(
        display(&inst, "ODLY"),
        ("s".to_string(), 2),
        "calcoutRecord.c:431-434/:452-455 return early for ODLY, before the EGU/PREC arms"
    );
}

/// The same early-return proof one field over: VAL has no literal arm, so it
/// takes the EGU/PREC the ODLY arm bypassed.
#[test]
fn calcouts_val_still_takes_egu_and_prec() {
    let mut rec = CalcoutRecord::default();
    rec.put_field("EGU", epics_base_rs::types::EpicsValue::String("V".into()))
        .expect("put EGU");
    rec.put_field("PREC", epics_base_rs::types::EpicsValue::Short(7))
        .expect("put PREC");
    let inst = inst("T:CALCOUT", rec);

    assert_eq!(
        display(&inst, "VAL"),
        ("V".to_string(), 7),
        "calcoutRecord.c:436-442/:457-459 serve VAL from the record's own EGU/PREC"
    );
}

/// `histogramRecord.c:419-438` is a switch: SDEL takes the literal while
/// ULIM/LLIM/SGNL/SVAL/WDTH take `prec->prec`. Pin that the literal is SDEL's
/// alone and does not leak onto the switch's other cases.
///
/// The siblings are asserted on units only. Their precision is C's
/// `prec->prec`, and this port's `HistogramRecord` carries no `PREC` field at
/// all (C `histogramRecord.dbd.pod:206` declares one), so it serves a
/// hard 0 there. That agrees with C only while PREC is unset — which is every
/// record the oracle builds, so the gap is real but unmeasured. It is the
/// missing-field defect, not this one, and asserting 0 here would pin it.
#[test]
fn histograms_sdel_takes_the_literal_where_its_siblings_do_not() {
    let inst = inst("T:HIST", HistogramRecord::default());

    assert_eq!(
        display(&inst, "SDEL"),
        ("s".to_string(), 2),
        "histogramRecord.c:413/:431 answer SDEL with \"s\"/histogramSDELprecision"
    );
    for sibling in ["ULIM", "LLIM", "SGNL", "SVAL", "WDTH"] {
        assert_eq!(
            display(&inst, sibling).0,
            "",
            "histogramRecord.c:409-416 answers only SDEL: {sibling} gets no units"
        );
    }
}

/// bo's rset serves BOTH literals for HIGH, but only units reaches the wire:
/// `property_leaves` nests `display.precision` inside `graphic_double`
/// (`iocsource.cpp:288-291`) and bo serves no graphic limits for HIGH.
/// The snapshot carries the rset's full answer; the marking model is what
/// narrows it. Measured: `softIocPVX` prints `display.units "s"` and no
/// `display.precision` for `bo.HIGH`.
#[test]
fn bo_high_serves_the_units_literal_the_wire_can_carry() {
    let inst = inst("T:BO", BoRecord::default());

    assert_eq!(
        display(&inst, "HIGH"),
        ("s".to_string(), 2),
        "boRecord.c:296/:304 answer HIGH with \"s\"/boHIGHprecision"
    );

    // The narrowing itself lives in `epics-pva-rs`
    // (`nt::qsrv_marks::property_leaves`, pinned there by
    // `integer_record_marks_limits_but_not_precision`); this crate can only pin
    // its input — no graphic limits for HIGH means no precision leaf.
    assert!(
        !snap(&inst, "HIGH").properties.graphic_double,
        "bo's get_graphic_double serves no limits for HIGH; if this ever changes, \
         display.precision starts reaching the wire and the oracle must be re-measured"
    );
}

/// bo's only literal is HIGH's. VAL is DBF_ENUM, so `get_units` writes nothing
/// for it — the override must not conjure units into being for a field the
/// rset never answers. (The display block itself now always exists, carrying
/// dbCommon DESC — UI-106 — so the assertion is on `units` staying empty.)
#[test]
fn bos_units_literal_is_highs_alone() {
    let inst = inst("T:BO", BoRecord::default());

    assert!(
        snap(&inst, "VAL").display.unwrap().units.is_empty(),
        "boRecord.c:294-299 answers only HIGH; VAL gets no units"
    );
}

/// `busyRecord.c:277-284` is the bare form of bo's arm — `if(paddr->pfield ==
/// (void *)&prec->high) *precision=2; else recGblGetPrec(paddr,precision);` —
/// and busy's rset NULLs `get_units`, so HIGH carries the 2 with no units.
#[test]
fn busy_high_takes_the_precision_literal_with_no_units() {
    let inst = inst("T:BUSY", BusyRecord::default());

    assert_eq!(
        display(&inst, "HIGH"),
        (String::new(), 2),
        "busyRecord.c:281 answers HIGH with a literal 2; :69 NULLs get_units"
    );
}

/// The `else recGblGetPrec` arm: on a busy that is nothing but zeros, every
/// other field must still read 0, so a passing HIGH proves the arm fired
/// rather than a blanket default coinciding.
#[test]
fn busys_precision_literal_is_highs_alone() {
    let inst = inst("T:BUSY", BusyRecord::default());

    for sibling in ["VAL", "OVAL", "IVOV"] {
        assert_eq!(
            display(&inst, sibling).1,
            0,
            "busyRecord.c:281 hands {sibling} to recGblGetPrec, which leaves the              memset zero"
        );
    }
}

/// `swaitRecord.c`'s `get_precision` answers ODLY with 3 — the only value in
/// this family that is not 2, so it cannot pass by coincidence with any other
/// arm. ODLY is `DBF_FLOAT`, which clears `dbAccess.c:387-388`'s gate.
#[test]
fn swait_odly_takes_the_precision_literal() {
    let inst = inst("T:SWAIT", SwaitRecord::default());

    assert_eq!(
        display(&inst, "ODLY").1,
        3,
        "swaitRecord.c answers ODLY with a literal 3, not with PREC"
    );
}
