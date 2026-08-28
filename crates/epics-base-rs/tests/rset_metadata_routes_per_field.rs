//! C's `rset` metadata slots route PER FIELD, on `dbGetFieldIndex` — a
//! non-VAL field does not get VAL's limits.
//!
//! Every base record's `get_control_double` / `get_alarm_double` has the same
//! two-arm shape: a listed set of VAL-class field indices that take the
//! record's own limits (`calcRecord.c:248-249`), and a `default:` arm that
//! hands the field to `recGblGetControlDouble` / `recGblGetAlarmDouble` — the
//! field TYPE's numeric range (`recGbl.c:146-171`, table at `:372-419`) and
//! four NaN (`:155-162`). The port had no `default:` arm at all: it served the
//! record's VAL metadata cache to every field.
//!
//! Every expectation here is what a compiled `softIocPVX` actually answers for
//! `record(calc, "ORACLE:CALC") {}` — measured with `pvxget`, not read off the
//! C source:
//!
//! ```text
//! $ pvxget ORACLE:CALC.PHAS   ->  control.limitLow -32768  limitHigh 32767
//! $ pvxget ORACLE:CALC.A      ->  control.limitLow -1e+300 limitHigh 1e+300
//! $ pvxget ORACLE:CALC.VAL    ->  control.limitLow 0       limitHigh 0
//! $ pvxget ORACLE:CALC.HIHI   ->  control.limitLow 0       limitHigh 0
//! ```
//!
//! `.PHAS` is `DBF_SHORT` and `.A` is `DBF_DOUBLE`, so each serves ITS OWN
//! type's range — proof the answer is keyed on the field, not the record.
//! `.VAL` and `.HIHI` are both listed in `calcRecord.c`'s switch, so both
//! serve the record's unset `HOPR`/`LOPR` of 0/0.

use epics_base_rs::server::database::LinkBacking;
use epics_base_rs::server::record::{AlarmSeverity, RecordInstance};
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::asub_record::ASubRecord;
use epics_base_rs::server::records::bi::BiRecord;
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::server::records::int64in::Int64inRecord;
use epics_base_rs::server::records::sel::SelRecord;
use epics_base_rs::server::records::seq::SeqRecord;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::EpicsValue;

fn snap(inst: &RecordInstance, field: &str) -> Snapshot {
    inst.snapshot_for_field_with(field, LinkBacking::none())
        .unwrap_or_else(|| panic!("{field} has no snapshot"))
}

fn calc() -> RecordInstance {
    RecordInstance::new("T:CALC".to_string(), CalcRecord::new("0"))
}

/// The `default:` arm, keyed on the field's own STATIC dbd type. `recGbl.c`
/// reads `pdbFldDes->field_type` (`:123`, `:151`, `:169`) — the declared type,
/// not `dbAccess.c:345-348`'s runtime one.
#[test]
fn an_unlisted_field_serves_its_own_type_range_not_the_records_limits() {
    let inst = calc();

    // DBF_SHORT -> getMaxRangeValues' SHRT case (recGbl.c:384-387).
    assert_eq!(
        snap(&inst, "PHAS").control_limits(),
        Some((-32768.0, 32767.0)),
        "PHAS is DBF_SHORT: C serves the SHRT range, measured on softIocPVX"
    );

    // DBF_DOUBLE -> recGbl.c:413-416.
    assert_eq!(
        snap(&inst, "A").control_limits(),
        Some((-1e300, 1e300)),
        "A is DBF_DOUBLE: C serves the DOUBLE range, measured on softIocPVX"
    );
}

/// The listed arm. `calcRecord.c:248-249` sends VAL and the alarm-band
/// siblings to `prec->lopr`/`prec->hopr`, which is exactly what the port's
/// record-level cache already holds — so these keep it.
///
/// HIHI is the boundary that a VAL-ONLY flip would break: it is listed
/// alongside VAL, so defaulting it to the type range would serve ±1e300 where
/// C serves 0/0.
#[test]
fn a_listed_val_class_field_keeps_the_records_own_limits() {
    let inst = calc();

    for field in ["VAL", "HIHI", "HIGH", "LOW", "LOLO", "LALM", "ALST", "MLST"] {
        assert_eq!(
            snap(&inst, field).control_limits(),
            Some((0.0, 0.0)),
            "{field} is listed in calcRecord.c's switch: C serves the record's \
             unset HOPR/LOPR of 0/0, not the type range"
        );
    }
}

/// `get_alarm_double`'s `default:` -> `recGblGetAlarmDouble` writes four NaN
/// (`recGbl.c:155-162`). The port served the record's HIHI/HIGH/LOW/LOLO.
///
/// The bands must be SET for this to measure anything: `alarm_limits()` is
/// severity-gated (`x ? limit : epicsNAN`, `calcRecord.c:264-267`), so an
/// unconfigured record's cache already holds four NaN and an unlisted field
/// would appear to route correctly while still reading the cache.
#[test]
fn an_unlisted_field_serves_nan_alarm_limits_not_the_records_bands() {
    let mut inst = calc();
    for (field, value) in [
        ("HIHI", EpicsValue::Double(90.0)),
        ("LOLO", EpicsValue::Double(10.0)),
        ("HHSV", EpicsValue::Short(AlarmSeverity::Major as i16)),
        ("LLSV", EpicsValue::Short(AlarmSeverity::Major as i16)),
    ] {
        inst.put_common_field(field, value)
            .unwrap_or_else(|e| panic!("put {field}: {e:?}"));
    }

    // The cache now holds real bands, so VAL — listed at `calcRecord.c:255`
    // — serves them.
    let (lolo, _, _, hihi) = snap(&inst, "VAL")
        .alarm_limits()
        .expect("calc supplies get_alarm_double");
    assert_eq!(
        (lolo, hihi),
        (10.0, 90.0),
        "VAL is listed: C serves the record's own bands"
    );

    // PHAS takes the `default:` arm, so the same read must NOT see them.
    let (lolo, low, high, hihi) = snap(&inst, "PHAS")
        .alarm_limits()
        .expect("calc supplies get_alarm_double");
    for (name, v) in [("lolo", lolo), ("low", low), ("high", high), ("hihi", hihi)] {
        assert!(
            v.is_nan(),
            "PHAS {name}: recGblGetAlarmDouble writes NaN, got {v} — the \
             record's own band leaked from the VAL cache"
        );
    }
}

/// A NULL rset slot makes `dbAccess.c:257` fail and CLEARS the option bit
/// (`:283`), so the leaf is never served — there is nothing to route, and
/// minting a value would put a fabricated number on the CA wire, which has no
/// marking layer to suppress it (`codec.rs` `get_limits` reads these structs
/// ungated).
///
/// `biRecord.c:57-76` NULLs every numeric slot.
#[test]
fn a_record_with_no_control_slot_mints_no_control_limits() {
    let inst = RecordInstance::new("T:BI".to_string(), BiRecord::new(0));

    assert_eq!(
        snap(&inst, "PHAS").control_limits(),
        None,
        "bi NULLs get_control_double: C clears the option bit, so no limits \
         reach the wire at all"
    );
}

/// The last arm is not the same for every record type. `aCalcoutRecord.c:793`
/// lists VAL/HIHI/HIGH/LOW/LOLO and the A-L / PA-PL ranges, then falls off the
/// end into `return(0)` with NO `recGblGetControlDouble` — so an unlisted
/// field keeps the `dbAccess.c:256` seed and C serves 0/0, not the type range.
///
/// Measured: before this fact existed, routing acalcout's unlisted fields to
/// the type range regressed 42 oracle cases from agreed to defect.
#[test]
fn a_record_whose_slot_falls_through_without_delegating_serves_the_seed() {
    let inst = RecordInstance::new("T:ACALCOUT".to_string(), AcalcoutRecord::new());

    // PHAS is DBF_SHORT: a delegating record would serve -32768/32767 here.
    assert_eq!(
        snap(&inst, "PHAS").control_limits(),
        Some((0.0, 0.0)),
        "aCalcoutRecord.c:793 writes nothing for an unlisted field, so C \
         serves the dbAccess.c:256 seed rather than the SHRT range"
    );
}

/// The converse boundary: `boRecord.c:59-61` KEEPS `get_control_double` (it
/// serves the HIGH field) while NULLing `get_graphic_double`. So bo's unlisted
/// fields do take the `default:` arm — the routing is gated on the slot, not
/// on whether the port's legacy cache happened to cover the record type. `bo`
/// has no arm in `populate_control_info`, so pre-fix it served nothing here.
#[test]
fn a_record_the_legacy_cache_never_covered_still_routes_its_type_range() {
    let inst = RecordInstance::new("T:BO".to_string(), BoRecord::new(0));

    assert_eq!(
        snap(&inst, "PHAS").control_limits(),
        Some((-32768.0, 32767.0)),
        "bo supplies get_control_double, so PHAS takes the default: arm"
    );
}

/// The two rset arms do NOT share one field list, and `.HIHI` is where they
/// provably disagree: `calcRecord.c:240-250` lists it in `get_control_double`
/// (so it takes the record's HOPR/LOPR) while `calcRecord.c:263` lists only
/// VAL in `get_alarm_double` (so it takes `recGblGetAlarmDouble`'s four NaN).
///
/// One shared VAL-class predicate cannot answer both: it either denies `.HIHI`
/// its control limits or hands it VAL's alarm limits. Measured on
/// `softIocPVX` for `record(calc,"ORACLE:CALC"){}`, `.HIHI` serves control
/// 0/0 (HOPR/LOPR) and valueAlarm all-NaN in the same read.
#[test]
fn the_alarm_arm_lists_val_alone_where_the_control_arm_lists_the_bands() {
    let inst = RecordInstance::new("T:CALC".to_string(), CalcRecord::new("A"));

    // Same field, same read, two different answers — that is the whole point.
    assert_eq!(
        snap(&inst, "HIHI").control_limits(),
        Some((0.0, 0.0)),
        "calcRecord.c:240-250 lists HIHI: control keeps the record's HOPR/LOPR"
    );
    let (hihi, high, low, lolo) = snap(&inst, "HIHI")
        .alarm_limits()
        .expect("calc supplies get_alarm_double, so the leaves are served");
    assert!(
        hihi.is_nan() && high.is_nan() && low.is_nan() && lolo.is_nan(),
        "calcRecord.c:263 lists VAL ALONE, so HIHI takes recGblGetAlarmDouble's \
         four NaN — not VAL's own alarm limits; got {hihi}/{high}/{low}/{lolo}"
    );

    // The seven bands all take the alarm default arm, not just HIHI.
    for band in ["HIGH", "LOW", "LOLO", "LALM", "ALST", "MLST"] {
        let (a, b, c, d) = snap(&inst, band)
            .alarm_limits()
            .unwrap_or_else(|| panic!("{band} serves no alarm limits"));
        assert!(
            a.is_nan() && b.is_nan() && c.is_nan() && d.is_nan(),
            "{band} is listed by get_control_double but NOT get_alarm_double, \
             so its alarm limits are the recGbl NaN; got {a}/{b}/{c}/{d}"
        );
    }
}

/// `int64in` is the one analog family whose C `get_alarm_double` VAL case is
/// UNCONDITIONAL (`int64inRecord.c:235-244` — no `hhsv ?` gate), so VAL serves
/// the raw limits. That must not leak to the bands: `.HIHI` is still unlisted
/// there and still takes the NaN default arm. This is the pair that made the
/// conflation visible on the wire, since int64in's cache holds 0 rather than
/// the gated NaN that masked the bug on `ai`.
#[test]
fn int64ins_unconditional_val_case_does_not_leak_to_its_alarm_bands() {
    let inst = RecordInstance::new("T:I64".to_string(), Int64inRecord::default());

    assert_eq!(
        snap(&inst, "VAL").alarm_limits(),
        Some((0.0, 0.0, 0.0, 0.0)),
        "int64inRecord.c:239-243 assigns hihi/high/low/lolo verbatim for VAL"
    );
    let (hihi, high, low, lolo) = snap(&inst, "HIHI")
        .alarm_limits()
        .expect("int64in supplies get_alarm_double");
    assert!(
        hihi.is_nan() && high.is_nan() && low.is_nan() && lolo.is_nan(),
        "HIHI is not VAL: it takes recGblGetAlarmDouble, so NaN — got \
         {hihi}/{high}/{low}/{lolo}"
    );
}

/// The graphic slot's three arms, on one record type each — the display twin
/// of the control routing above.
///
/// Measured on `softIocPVX` before the port was changed:
///
/// ```text
/// $ pvxget ORACLE:CALC.PHAS  ->  display.limitLow -32768  limitHigh 32767
/// $ pvxget ORACLE:ASUB.PHAS  ->  display.limitLow 0       limitHigh 0
/// $ pvxget ORACLE:CALC.A     ->  display.limitLow 0       limitHigh 0
/// ```
///
/// Three record types, one field name, three different answers — which is why
/// display cannot be a straight flip to the type range.
#[test]
fn the_graphic_slot_routes_recgbl_seed_and_link_arms_apart() {
    // RecGblRange: calc delegates, PHAS is DBF_SHORT and backs no link.
    let calc = RecordInstance::new("T:CALC".to_string(), CalcRecord::new("A"));
    assert_eq!(
        snap(&calc, "PHAS").display_limits(),
        Some((-32768.0, 32767.0)),
        "calcRecord.c:187-212 ends in recGblGetGraphicDouble: PHAS serves the \
         DBF_SHORT range"
    );

    // Seed: aSubRecord.c:350-368 has NO default: and NO recGbl call, so an
    // unlisted field is written by nothing and the dbAccess.c:216 seed stands.
    // Its OWN get_control_double is a bare recGblGetControlDouble (:372-376) —
    // the two slots disagree on the same record, same field.
    let asub = RecordInstance::new("T:ASUB".to_string(), ASubRecord::default());
    assert_eq!(
        snap(&asub, "PHAS").display_limits(),
        Some((0.0, 0.0)),
        "aSubRecord.c:350-368 writes nothing for PHAS: C serves the seed, not \
         the SHRT range"
    );
    assert_eq!(
        snap(&asub, "PHAS").control_limits(),
        Some((-32768.0, 32767.0)),
        "aSubRecord.c:372-376 IS a bare recGblGetControlDouble — the same field \
         takes the type range for control and the seed for graphic, which is \
         why the two slots need separate per-type bits"
    );

    // Link-backed: calc's A is DBF_DOUBLE, but get_linkNumber maps it to INPA
    // and dbGetGraphicLimits on an unset (constant) link writes nothing.
    // Serving the DBF_DOUBLE range of +-1e300 here would be the regression a
    // bare flip causes.
    assert_eq!(
        snap(&calc, "A").display_limits(),
        Some((0.0, 0.0)),
        "calcRecord.c:161-167 maps A to the INPA link: a constant link supplies \
         no graphic limits, so C serves 0/0, NOT the DBF_DOUBLE range"
    );
}

/// `CALCPERFORM_NARGS` is 21 (`postfix.h:29`), not 12: the calc family's link
/// range runs `A`..`U` and `LA`..`LU`, which `calcRecord.dbd.pod:801-985`
/// declares contiguously. `U` is the boundary that a 12-arg assumption gets
/// wrong — it would serve the DBF_DOUBLE range for a link-backed field.
#[test]
fn the_calc_arg_link_range_runs_to_u_not_to_l() {
    let calc = RecordInstance::new("T:CALC".to_string(), CalcRecord::new("A"));
    for arg in ["A", "L", "M", "U", "LA", "LL", "LU"] {
        assert_eq!(
            snap(&calc, arg).display_limits(),
            Some((0.0, 0.0)),
            "{arg} is inside CALCPERFORM_NARGS=21, so it is link-backed and \
             serves 0/0"
        );
    }
}

/// `sel` names its args `A`..`L` exactly as `calc` does, and answers them
/// completely differently: `selRecord.c:181-201` lists them as explicit
/// HOPR/LOPR cases and calls `dbGetGraphicLimits` NOWHERE. So the link
/// predicate must be per-type, not keyed on the field name.
#[test]
fn sels_args_are_explicit_limits_not_link_backed_like_calcs() {
    let sel = RecordInstance::new("T:SEL".to_string(), SelRecord::default());
    assert_eq!(
        snap(&sel, "A").display_limits(),
        Some((0.0, 0.0)),
        "selRecord.c:193 lists A..L on HOPR/LOPR"
    );
    assert_eq!(
        snap(&sel, "PHAS").display_limits(),
        Some((-32768.0, 32767.0)),
        "sel still delegates for an unlisted field"
    );
}

/// The two literal arms that belong to graphic alone. `seq`'s DLYn serves
/// `0..10` (`seqRecord.c:322-338`) while the SAME record's control serves
/// `0..seqDLYlimit` = 100000 (`:342-352`, `:81`) — reading one slot off the
/// other would be wrong by 4 orders of magnitude. `calcout`'s ODLY calls
/// recGbl and then overwrites the lower with 0 (`calcoutRecord.c:490-493`).
#[test]
fn the_graphic_literal_arms_are_not_the_control_ones() {
    let seq = RecordInstance::new("T:SEQ".to_string(), SeqRecord::default());
    assert_eq!(
        snap(&seq, "DLY0").display_limits(),
        Some((0.0, 10.0)),
        "seqRecord.c:327-328 hard-codes 0.0/10.0 for DLYn"
    );
    assert_eq!(
        snap(&seq, "DO0").display_limits(),
        Some((0.0, 0.0)),
        "seqRecord.c:331 routes DOn through its DOLn link"
    );

    let co = RecordInstance::new("T:CO".to_string(), CalcoutRecord::default());
    assert_eq!(
        snap(&co, "ODLY").display_limits(),
        Some((0.0, 1e300)),
        "calcoutRecord.c:490-493 takes the recGbl DBF_DOUBLE upper and forces \
         the lower to 0"
    );
}
