//! `props.alarm_double` ⟹ the snapshot carries ASSIGNED alarm limits.
//!
//! `getProperties` assigns the four `valueAlarm.*Limit` leaves under exactly
//! one condition — the record's rset supplies `get_alarm_double`
//! (`nt::qsrv_marks::property_leaves`). So whenever those leaves reach the
//! wire, something must have decided their value. `route_field_metadata` is
//! that owner, for both of C's arms.
//!
//! The invariant was previously enforceable only by convention: the explicit
//! VAL arm was left to `populate_display_info`'s `match rtype`, which covered
//! `ai`/`ao`/`calc`/`calcout`/`longin`/`longout`/`int64in`/`int64out` and no
//! other type that supplies the slot. `dfanout`, `sel` and `sub` fell through
//! it to `snap.display == None`, and the four leaves reached the wire holding
//! the NT's structural 0 — measured on `softIocPVX`, which serves NaN:
//!
//! ```text
//! $ pvxget ORACLE:DFANOUT.VAL  ->  valueAlarm.highAlarmLimit nan   (port: 0)
//! $ pvxget ORACLE:SEL.VAL      ->  valueAlarm.highAlarmLimit nan   (port: 0)
//! $ pvxget ORACLE:SUB.VAL      ->  valueAlarm.highAlarmLimit nan   (port: 0)
//! ```
//!
//! These cases are per-boundary, not per-record: the boundaries are the arm
//! (`Gated` / `Unconditional` / no explicit arm at all), the storage home of
//! the limits (`common.analog_alarm` vs the record's own struct), and
//! severity-set vs severity-unset.

use epics_base_rs::server::record::{AlarmSeverity, Record, RecordInstance};
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::dfanout::DfanoutRecord;
use epics_base_rs::server::records::int64in::Int64inRecord;
use epics_base_rs::server::records::sel::SelRecord;
use epics_base_rs::server::records::seq::SeqRecord;
use epics_base_rs::server::records::sub_record::SubRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::EpicsValue;

fn snap(inst: &RecordInstance, field: &str) -> Snapshot {
    inst.snapshot_for_field(field)
        .unwrap_or_else(|| panic!("{field} has no snapshot"))
}

/// `(lolo, low, high, hihi)` as the served snapshot carries them.
fn limits(inst: &RecordInstance, field: &str) -> (f64, f64, f64, f64) {
    snap(inst, field)
        .alarm_limits()
        .unwrap_or_else(|| panic!("{field} serves no alarm limits"))
}

fn inst<R: Record + 'static>(rec: R) -> RecordInstance {
    RecordInstance::new("T:REC".to_string(), rec)
}

/// Set HIHI/LOLO and enable their severities, through whichever store the type
/// keeps them in — `put_field` for a record with its own fields (dfanout/sel),
/// `put_common_field` for one with the analog ladder.
fn arm_bands(inst: &mut RecordInstance) {
    for (field, value) in [
        ("HIHI", EpicsValue::Double(90.0)),
        ("LOLO", EpicsValue::Double(10.0)),
        ("HHSV", EpicsValue::Short(AlarmSeverity::Major as i16)),
        ("LLSV", EpicsValue::Short(AlarmSeverity::Major as i16)),
    ] {
        if inst.record.put_field(field, value.clone()).is_err() {
            inst.put_common_field(field, value)
                .unwrap_or_else(|e| panic!("put {field}: {e:?}"));
        }
    }
}

/// The regression the invariant exists for. Every type whose rset supplies the
/// slot must serve ASSIGNED limits on VAL — never the structural 0 that a
/// missing `populate_display_info` arm used to leave behind.
#[test]
fn the_three_types_the_cache_missed_serve_nan_not_a_structural_zero() {
    let cases: Vec<(&str, RecordInstance)> = vec![
        ("dfanout", inst(DfanoutRecord::new(0.0))),
        ("sel", inst(SelRecord::default())),
        ("sub", inst(SubRecord::default())),
    ];

    for (name, inst) in cases {
        assert!(
            snap(&inst, "VAL").properties.alarm_double,
            "{name} supplies get_alarm_double, so the four leaves are assigned"
        );
        let (lolo, low, high, hihi) = limits(&inst, "VAL");
        for (band, v) in [("lolo", lolo), ("low", low), ("high", high), ("hihi", hihi)] {
            assert!(
                v.is_nan(),
                "{name}.VAL {band}: severities are unset, so C's gate \
                 (`prec->hhsv ? prec->hihi : epicsNAN`) serves NaN, not 0"
            );
        }
    }
}

/// The other side of the same gate: with the severities ENABLED, the gated arm
/// must serve the record's real bands. This is what a structural 0 could never
/// be distinguished from if only the unset case were tested.
#[test]
fn the_gated_arm_serves_the_bands_once_their_severities_are_enabled() {
    for (name, mut inst) in [
        ("dfanout", inst(DfanoutRecord::new(0.0))),
        ("sel", inst(SelRecord::default())),
        ("sub", inst(SubRecord::default())),
        ("ai", inst(AiRecord::new(0.0))),
    ] {
        arm_bands(&mut inst);
        let (lolo, _, _, hihi) = limits(&inst, "VAL");
        assert_eq!(
            (lolo, hihi),
            (10.0, 90.0),
            "{name}.VAL: HHSV/LLSV are enabled, so C serves the record's own bands"
        );
    }
}

/// `dfanout`/`sel` keep their eight alarm fields on their own struct and have
/// no `common.analog_alarm` slot at all. Reading the slot instead of the
/// record's fields would answer NaN here no matter what HIHI was set to — so
/// this pins the storage-home boundary, not just the arm.
#[test]
fn the_limits_come_from_the_records_own_fields_not_the_analog_ladder_slot() {
    let mut inst = inst(DfanoutRecord::new(0.0));
    arm_bands(&mut inst);

    assert_eq!(
        inst.resolve_field("HIHI").and_then(|v| v.to_f64()),
        Some(90.0),
        "dfanout stores HIHI on its own struct"
    );
    let (_, _, _, hihi) = limits(&inst, "VAL");
    assert_eq!(
        hihi, 90.0,
        "dfanout has no analog-ladder slot, so only reading the record's own \
         field can find its HIHI"
    );
}

/// `int64in` and `acalcout` are `Unconditional`: C sends the four limits
/// verbatim with no severity test, so an unset record serves 0 — the one place
/// a zero IS the correct answer.
#[test]
fn the_unconditional_arm_ignores_the_severities_entirely() {
    for (name, mut inst) in [
        ("int64in", inst(Int64inRecord::default())),
        ("acalcout", inst(AcalcoutRecord::default())),
    ] {
        // Bands set, severities left at NO_ALARM.
        for (field, value) in [
            ("HIHI", EpicsValue::Double(90.0)),
            ("LOLO", EpicsValue::Double(10.0)),
        ] {
            if inst.record.put_field(field, value.clone()).is_err() {
                inst.put_common_field(field, value)
                    .unwrap_or_else(|e| panic!("put {field}: {e:?}"));
            }
        }

        let (lolo, _, _, hihi) = limits(&inst, "VAL");
        assert_eq!(
            (lolo, hihi),
            (10.0, 90.0),
            "{name}'s rset assigns pad->upper_alarm_limit = prec->hihi with no \
             severity test, so the bands are served even at NO_ALARM"
        );
    }
}

/// `seq`/`aSub`/`swait` list NOTHING: even VAL takes the `recGblGetAlarmDouble`
/// arm. `swaitRecord.c:608-612` is a bare `recGblGetAlarmDouble(paddr, pad)`.
/// If VAL were treated as explicit for these, an armed record would serve its
/// bands where C serves NaN.
#[test]
fn the_types_that_list_nothing_route_even_val_to_the_default_arm() {
    for (name, mut inst) in [
        ("seq", inst(SeqRecord::default())),
        ("swait", inst(SwaitRecord::default())),
    ] {
        arm_bands(&mut inst);

        let (lolo, low, high, hihi) = limits(&inst, "VAL");
        for (band, v) in [("lolo", lolo), ("low", low), ("high", high), ("hihi", hihi)] {
            assert!(
                v.is_nan(),
                "{name}'s rset lists no field, so even VAL takes recGblGetAlarmDouble's \
                 four NaN — {band} served {v} instead"
            );
        }
    }
}

/// The non-VAL boundary, unchanged by this owner move: a listed type's OTHER
/// fields still take the default arm, so `.HIHI` does not carry VAL's bands.
#[test]
fn an_unlisted_field_of_a_listed_type_still_takes_the_default_arm() {
    let mut inst = inst(DfanoutRecord::new(0.0));
    arm_bands(&mut inst);

    let (lolo, low, high, hihi) = limits(&inst, "HIHI");
    for (band, v) in [("lolo", lolo), ("low", low), ("high", high), ("hihi", hihi)] {
        assert!(
            v.is_nan(),
            "dfanoutRecord.c lists VAL alone: .HIHI's own alarm limits are the \
             recGbl NaN — {band} served {v}"
        );
    }
}
