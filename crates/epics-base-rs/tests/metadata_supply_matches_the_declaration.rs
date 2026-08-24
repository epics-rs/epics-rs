//! A record type that DECLARES an rset metadata slot must SUPPLY it.
//!
//! `PropertySupport` (the declaration) and the record-level metadata cache
//! (the supply) were two independent tables answering the same question, and
//! nothing kept them in step: `default_property_support` declares `units` and
//! `precision` for twenty-five record types where `populate_display_info`'s
//! `match rtype` had nine arms. The sixteen types in the gap put a marked leaf
//! on the wire carrying `""` and `0` — the declaration says the number is
//! authoritative, and the number is a default nobody supplied.
//!
//! Precision reaches further than `caget -d`: it is what the DBF_DOUBLE to
//! DBR_STRING conversion renders with, in C (`dbConvert.c:783-786` calls
//! `prset->get_precision` with no field-type gate) and here, so a plain
//! `caget T:SEL.A` printed `2` where C prints `1.500`.
//!
//! Every case below is written at the boundary where the answer DECIDES the
//! wire value: the record's own `PREC`/`EGU`/`HOPR`/`LOPR` on one side, the
//! `dbAccess.c` zero seed or `recGbl`'s type range on the other. The two never
//! coincide here, so a pass proves the supply, not a lucky agreement.
//!
//! The over-supply direction is the same defect and is pinned here too: C's
//! `get_units` tests the field before writing in eleven of the ported types,
//! and serving `EGU` for a field its rset skips is as much a wire divergence
//! as serving `""` for one it fills.

use epics_base_rs::server::record::{Record, RecordInstance};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::dfanout::DfanoutRecord;
use epics_base_rs::server::records::histogram::HistogramRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::sel::SelRecord;
use epics_base_rs::server::records::seq::SeqRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::server::records::sub_record::SubRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::server::records::waveform::{ArrayKind, WaveformRecord};
use epics_base_rs::types::EpicsValue;

/// A `.db` record with its metadata fields set — `field(PREC,"3")` and its
/// siblings.
///
/// Two homes, because the port has two: a record type that models the cell
/// takes the value through its own `put_field`, and one that does not — `sel`,
/// `sub` and `dfanout` model no `PREC`/`EGU`/`HOPR`/`LOPR` at all — gets a
/// `declared_overrides` entry, which is the db loader's own path and the only
/// one `resolve_field` can reach.
fn load<R: Record + 'static>(mut rec: R, fields: &[(&str, EpicsValue)]) -> RecordInstance {
    let mut unmodeled = Vec::new();
    for (name, value) in fields {
        if rec.put_field(name, value.clone()).is_err() {
            unmodeled.push((*name, value.clone()));
        }
    }
    let mut inst = RecordInstance::new("T:REC".to_string(), rec);
    for (name, value) in unmodeled {
        inst.put_common_field_db_load(name, value)
            .unwrap_or_else(|e| panic!("field({name}) failed to load: {e:?}"));
    }
    inst
}

fn prec(v: i16) -> EpicsValue {
    EpicsValue::Short(v)
}

fn egu(v: &str) -> EpicsValue {
    EpicsValue::String(v.into())
}

fn limit(v: f64) -> EpicsValue {
    EpicsValue::Double(v)
}

fn units(inst: &RecordInstance, field: &str) -> String {
    inst.snapshot_for_field(field)
        .unwrap_or_else(|| panic!("{field} has no snapshot"))
        .units()
        .unwrap_or_else(|| panic!("{field} serves no units leaf"))
        .as_str_lossy()
        .into_owned()
}

fn precision(inst: &RecordInstance, field: &str) -> i16 {
    inst.snapshot_for_field(field)
        .unwrap_or_else(|| panic!("{field} has no snapshot"))
        .precision()
        .unwrap_or_else(|| panic!("{field} serves no precision leaf"))
}

/// `(lower, upper)`, as [`Snapshot::display_limits`] returns them.
fn display(inst: &RecordInstance, field: &str) -> (f64, f64) {
    inst.snapshot_for_field(field)
        .unwrap_or_else(|| panic!("{field} has no snapshot"))
        .display_limits()
        .unwrap_or_else(|| panic!("{field} serves no display limits"))
}

fn control(inst: &RecordInstance, field: &str) -> (f64, f64) {
    inst.snapshot_for_field(field)
        .unwrap_or_else(|| panic!("{field} has no snapshot"))
        .control_limits()
        .unwrap_or_else(|| panic!("{field} serves no control limits"))
}

/// `recGbl.c` `getMaxRangeValues` for DBF_DOUBLE.
const DOUBLE_RANGE: (f64, f64) = (-1e300, 1e300);

/// `selRecord.c:136-143` (`get_units`), `:146-160` (`get_precision`),
/// `:169-201` / `:203-235` (the two limit slots) — all four answer from the
/// record. `selRecord.c:51` NULLs only `get_enum_str`, so `sel` declares the
/// other five and used to supply none of them.
#[test]
fn sel_val_serves_the_record_and_not_the_zero_seed() {
    let inst = load(
        SelRecord::default(),
        &[
            ("PREC", prec(3)),
            ("EGU", egu("mm")),
            ("HOPR", limit(10.0)),
            ("LOPR", limit(-10.0)),
        ],
    );

    assert_eq!(units(&inst, "VAL"), "mm");
    assert_eq!(precision(&inst, "VAL"), 3);
    assert_eq!(display(&inst, "VAL"), (-10.0, 10.0));
    assert_eq!(control(&inst, "VAL"), (-10.0, 10.0));
    // `selRecord.c:152-163` returns `prec->prec` for `A`..`L` too, and this is
    // the field a plain `caget` renders with it.
    assert_eq!(precision(&inst, "A"), 3);
}

/// `selRecord.c:138` tests `field_type == DBF_DOUBLE` before copying `EGU`, so
/// `SELN` (DBF_USHORT) keeps `dbAccess.c:378`'s zeroed buffer. Supplying the
/// record-level cache without this test would put `mm` on a channel C leaves
/// empty.
#[test]
fn sel_does_not_put_egu_on_a_non_double_field() {
    let inst = load(SelRecord::default(), &[("EGU", egu("mm"))]);

    assert_eq!(units(&inst, "VAL"), "mm");
    assert_eq!(units(&inst, "SELN"), "");
}

/// `subRecord.c:206-219` routes a link-backed field through `dbGetUnits`, and
/// an unset `INPA` is a CONSTANT link with no metadata getters — so `A` gets
/// nothing while `VAL`, which is not link-backed, gets `EGU`.
#[test]
fn sub_val_takes_egu_and_its_link_backed_arg_does_not() {
    let inst = load(
        SubRecord::default(),
        &[("PREC", prec(3)), ("EGU", egu("V"))],
    );

    assert_eq!(units(&inst, "VAL"), "V");
    assert_eq!(units(&inst, "A"), "");
    // `subRecord.c:227-228` returns before the link branch for VAL, and
    // `dbGetPrecision` on a constant link fails for `A`, leaving `prec->prec`.
    assert_eq!(precision(&inst, "VAL"), 3);
    assert_eq!(precision(&inst, "A"), 3);
}

/// `dfanoutRecord.c:155-163`, `:165-172`, `:175-195`, `:197-213`.
#[test]
fn dfanout_val_serves_all_four_slots() {
    let inst = load(
        DfanoutRecord::default(),
        &[
            ("PREC", prec(2)),
            ("EGU", egu("A")),
            ("HOPR", limit(3.0)),
            ("LOPR", limit(-3.0)),
        ],
    );

    assert_eq!(units(&inst, "VAL"), "A");
    assert_eq!(precision(&inst, "VAL"), 2);
    assert_eq!(display(&inst, "VAL"), (-3.0, 3.0));
    assert_eq!(control(&inst, "VAL"), (-3.0, 3.0));
}

/// `subArray` is absent from every arm the `waveform` family had, and
/// `waveform.rs`'s shared override answers `None` for `VAL`.
/// `subArrayRecord.c:206-227` and `:231-291` all answer from the record.
#[test]
fn subarray_val_serves_the_record_where_the_waveform_arm_skipped_it() {
    let fields = |ftvl: i16| {
        vec![
            ("FTVL", EpicsValue::Short(ftvl)),
            ("PREC", prec(4)),
            ("EGU", egu("mm")),
            ("HOPR", limit(7.0)),
            ("LOPR", limit(-7.0)),
        ]
    };
    // menuFtype is declared in DBF_ code order: 10 is DBF_DOUBLE.
    let inst = load(WaveformRecord::with_kind(ArrayKind::SubArray), &fields(10));

    assert_eq!(units(&inst, "VAL"), "mm");
    assert_eq!(display(&inst, "VAL"), (-7.0, 7.0));
    assert_eq!(control(&inst, "VAL"), (-7.0, 7.0));
    // `subArrayRecord.c:206-217` names three fields; `NELM` is not one of
    // them, so C leaves its units empty.
    assert_eq!(units(&inst, "NELM"), "");

    // `subArrayRecord.c:211-213` breaks out of the `VAL` case for a STRING or
    // ENUM element type, so the same record with `FTVL` 0 serves no units on
    // VAL while `HOPR` keeps them.
    let strings = load(WaveformRecord::with_kind(ArrayKind::SubArray), &fields(0));
    assert_eq!(units(&strings, "VAL"), "");
    assert_eq!(units(&strings, "HOPR"), "mm");
}

/// `histogramRecord.c:420-439` is a bare switch with NO `prec->prec` seed
/// ahead of it, so its `default:` arm really does leave zero — `SDLY` is the
/// one DBF_DOUBLE field outside the named set and must stay 0 while its five
/// siblings take `PREC`.
#[test]
fn histogram_names_five_precision_fields_and_sdly_is_not_one() {
    let inst = load(
        HistogramRecord::new(10, 0.0, 10.0),
        &[
            ("PREC", prec(3)),
            ("HOPR", limit(40.0)),
            ("LOPR", limit(5.0)),
        ],
    );

    for field in ["ULIM", "LLIM", "SGNL", "SVAL", "WDTH"] {
        assert_eq!(precision(&inst, field), 3, "{field} takes prec->prec");
    }
    assert_eq!(precision(&inst, "SDLY"), 0);
    // `histogramRecord.c:411-417` writes `"s"` for SDEL and nothing else;
    // histogram has no EGU field at all.
    assert_eq!(units(&inst, "SDEL"), "s");
    assert_eq!(units(&inst, "VAL"), "");
    assert_eq!(display(&inst, "VAL"), (5.0, 40.0));
}

/// `swaitRecord.c:583-595` has no `recGblGetPrec` fall-through, so every field
/// keeps `pwait->prec`; `:597-606` lists `VAL` ALONE, so `ALST`/`MLST` take
/// the type range where a seven-band list gave them the record's HOPR/LOPR.
#[test]
fn swait_supplies_prec_everywhere_and_lists_val_alone_for_limits() {
    let inst = load(
        SwaitRecord::default(),
        &[
            ("PREC", prec(2)),
            ("HOPR", limit(20.0)),
            ("LOPR", limit(-20.0)),
        ],
    );

    assert_eq!(precision(&inst, "A"), 2);
    assert_eq!(display(&inst, "VAL"), (-20.0, 20.0));
    assert_eq!(display(&inst, "ALST"), DOUBLE_RANGE);
    // `swaitRecord.c:591-593` overwrites the seed with a literal 3 for ODLY
    // alone — the one field the record-level PREC must not answer.
    assert_eq!(precision(&inst, "ODLY"), 3);
}

/// `sseqRecord.c:810-821` answers `pR->prec` for every record-specific field.
#[test]
fn sseq_supplies_prec_for_its_delays() {
    let inst = load(SseqRecord::new(), &[("PREC", prec(3))]);

    assert_eq!(precision(&inst, "DLY1"), 3);
}

/// `seqRecord.c:299-317`: a `DOn` whose `DOLn` is a constant fails
/// `dbGetPrecision`, falls out of the switch and lands on `prec->prec`.
#[test]
fn seq_supplies_prec_for_a_constant_backed_do() {
    let inst = load(SeqRecord::new(), &[("PREC", prec(3))]);

    assert_eq!(precision(&inst, "DO0"), 3);
    // seq has no EGU field, and `seqRecord.c:282-297` writes units only for
    // DLYn and DOn — never from the record.
    assert_eq!(units(&inst, "VAL"), "");
}

/// `sCalcoutRecord.c:603-609` copies `pcalc->egu` with NO field test at all,
/// which is the opposite boundary from `sel`: here even a non-double field
/// carries the units.
#[test]
fn scalcout_copies_egu_with_no_field_test() {
    let inst = load(
        ScalcoutRecord::new(),
        &[("PREC", prec(3)), ("EGU", egu("cts"))],
    );

    assert_eq!(units(&inst, "VAL"), "cts");
    assert_eq!(units(&inst, "PREC"), "cts");
    assert_eq!(precision(&inst, "A"), 3);
}

/// `aiRecord.c:223-226` skips `ASLO`/`AOFF`/`SMOO` — the raw-conversion
/// fields carry no engineering units. `ai` has had a display arm all along, so
/// this is the over-supply the arm never tested for.
#[test]
fn ai_skips_the_raw_conversion_fields_when_supplying_units() {
    let inst = load(AiRecord::new(0.0), &[("EGU", egu("V"))]);

    assert_eq!(units(&inst, "VAL"), "V");
    for field in ["ASLO", "AOFF", "SMOO"] {
        assert_eq!(units(&inst, field), "", "aiRecord.c:223-226 skips {field}");
    }
}

/// `busyRecord.c:277-284` is the shape no record-level rule can reach: the
/// answer for `HIGH` is a literal, and busy declares no `PREC` field to carry
/// one. `busyRecord.c:54-61` NULLs the other four numeric slots, so precision
/// is the only leaf to get right.
#[test]
fn busy_high_serves_its_literal_precision() {
    let inst = load(BusyRecord::default(), &[]);

    assert_eq!(precision(&inst, "HIGH"), 2);
}
