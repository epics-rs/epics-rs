//! R15-79: a SCALAR source into an array VAL lands as ONE ELEMENT of the
//! FTVL-typed buffer — it must not replace the buffer with a scalar variant.
//!
//! C hands every VAL write a pointer to the FTVL-typed `bptr` with
//! `nRequest = NELM` (`aaoRecord.c:366` `dbGetLink(&prec->dol, prec->ftvl,
//! prec->bptr, 0, &nReq)`), so a scalar source converts INTO `bptr[0]` and
//! yields `nReq = 1` → `NORD = 1`. The pre-fix port's put_field fallback
//! (`other => { nord = 1; val = other }`) stored the scalar variant instead,
//! which broke three things at once on the everyday `DOL="SETPOINT"`
//! closed-loop aao: `array_content_bytes` has no scalar arm, so the On-Change
//! hash went empty and MPST/APST posting died; `resize_val_preserving` found no
//! array variant and reallocated; and the scalar propagated to the OUT target.
//!
//! Boundaries: scalar into VAL (put path), scalar through a closed-loop DOL
//! (link path), On-Change hash across successive scalar updates, and the OUT
//! target's received type.

// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::waveform::{ArrayKind, WaveformRecord};
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// A direct scalar put into an FTVL=DOUBLE, NELM=4 VAL: element 0, NORD=1,
/// still a DoubleArray.
#[test]
fn scalar_put_lands_in_element_zero_of_the_typed_buffer() {
    let mut wf = WaveformRecord::new(4, DbFieldType::Double);
    wf.put_field("VAL", EpicsValue::Double(7.5)).unwrap();

    assert_eq!(wf.nord, 1, "a scalar source is ONE element (C nReq = 1)");
    assert_eq!(
        wf.val,
        EpicsValue::DoubleArray(vec![7.5, 0.0, 0.0, 0.0]),
        "the FTVL-typed NELM buffer survives; the scalar lands in bptr[0]"
    );
    assert_eq!(
        wf.get_field("VAL"),
        Some(EpicsValue::DoubleArray(vec![7.5])),
        "clients see NORD=1 elements"
    );
}

/// FTVL conversion, as C's `dbFastConvert` into `bptr` does it: a Double
/// source into an FTVL=LONG buffer becomes a LONG element.
#[test]
fn scalar_put_converts_to_the_ftvl_element_type() {
    let mut wf = WaveformRecord::new(3, DbFieldType::Long);
    wf.put_field("VAL", EpicsValue::Double(42.9)).unwrap();

    assert_eq!(wf.nord, 1);
    assert_eq!(wf.val, EpicsValue::LongArray(vec![42, 0, 0]));
}

/// With the array invariant intact, the On-Change hash tracks successive
/// scalar updates — MPST="On Change" posts on each new value. The pre-fix
/// scalar variant made `array_content_bytes` return an empty byte slice, so
/// the hash was constant (0) and posting never fired again.
#[test]
fn onchange_hash_moves_across_scalar_updates() {
    const ONCHANGE: i16 = 1;
    let mut wf = WaveformRecord::new(4, DbFieldType::Double);
    wf.put_field("MPST", EpicsValue::Short(ONCHANGE)).unwrap();

    wf.put_field("VAL", EpicsValue::Double(1.0)).unwrap();
    let first = wf.array_monitor_post().expect("array kind posts");
    assert!(first.post_value, "first scalar value must post");
    assert!(first.hash_changed);
    let h1 = wf.hash;

    wf.put_field("VAL", EpicsValue::Double(2.0)).unwrap();
    let second = wf.array_monitor_post().expect("array kind posts");
    assert!(second.post_value, "a changed scalar must post again");
    assert_ne!(wf.hash, h1, "hash must move with the element content");

    // Same value again: no hash movement, no post (C `hash != prec->hash`).
    let third = wf.array_monitor_post().expect("array kind posts");
    assert!(
        !third.post_value,
        "an unchanged array must NOT post On Change"
    );
    assert!(!third.hash_changed);
}

/// End-to-end: `DOL="SETPOINT"` on a closed-loop aao (a scalar ao feeding an
/// array output). VAL must become a one-element DoubleArray in the NELM
/// buffer, and the OUT target must receive an ARRAY.
#[tokio::test]
async fn closed_loop_scalar_dol_feeds_an_array_out_target() {
    const DB: &str = r#"
record(ao, "SETPOINT") {
    field(VAL, "3.5")
}
record(waveform, "AAO:TGT") {
    field(FTVL, "DOUBLE")
    field(NELM, "4")
}
record(aao, "AAO:CL") {
    field(FTVL, "DOUBLE")
    field(NELM, "4")
    field(OMSL, "closed_loop")
    field(DOL, "SETPOINT")
    field(OUT, "AAO:TGT")
}
"#;
    let (db, _) = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("AAO:CL", &mut visited, 0)
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("AAO:CL").unwrap(),
        EpicsValue::DoubleArray(vec![3.5]),
        "a scalar DOL lands as element 0 of the array (NORD=1), not a scalar VAL"
    );
    assert_eq!(
        db.get_pv("AAO:CL.NORD").unwrap().to_f64(),
        Some(1.0),
        "C: nord = nReq = 1"
    );
    assert_eq!(
        db.get_pv("AAO:TGT").unwrap(),
        EpicsValue::DoubleArray(vec![3.5]),
        "the OUT target receives an ARRAY, not the scalar"
    );
}

/// The aao is still an array record: a subsequent scalar DOL update does not
/// destroy the buffer, and a longer array put afterwards still fills it.
#[test]
fn buffer_survives_scalar_then_array_updates() {
    let mut aao = WaveformRecord::new(4, DbFieldType::Double);
    aao.kind = ArrayKind::Aao;

    aao.put_field("VAL", EpicsValue::Double(9.0)).unwrap();
    assert_eq!(aao.nord, 1);

    aao.put_field("VAL", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
        .unwrap();
    assert_eq!(aao.nord, 3, "an array source refills the buffer head");
    assert_eq!(
        aao.get_field("VAL"),
        Some(EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
    );

    // NELM resize still preserves data (it found no array variant before).
    aao.put_field("NELM", EpicsValue::Long(6)).unwrap();
    assert_eq!(aao.nord, 3, "resize preserves NORD");
    assert_eq!(
        aao.get_field("VAL"),
        Some(EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0])),
        "resize preserves the element data"
    );
}
