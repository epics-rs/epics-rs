//! R14-61: the scalcout OUT write buffer is chosen by the TARGET field's DBF
//! type, not fixed to OVAL.
//!
//! C `devsCalcoutSoft.c::write_scalcout` (66-144) resolves the target
//! (`dbNameToAddr` for a DB link, `dbCaGetLinkDBFtype`/`dbCaGetNelements` for
//! a CA link) and then switches:
//!
//! * `DBF_STRING`/`ENUM`/`MENU`/`DEVICE`/`INLINK`/`OUTLINK`/`FWDLINK`
//!   → `dbPutLink(DBR_STRING, &osv)` — the computed OCAL string;
//! * `DBF_CHAR`/`DBF_UCHAR` with `n_elements > 1`
//!   → `dbPutLink(DBF_CHAR, &sval, min(n, 40))`;
//! * everything else → `dbPutLink(DBR_DOUBLE, &oval)`.
//!
//! The port drove OVAL into every target, so a string-valued scalcout wrote
//! the *numeric* where C writes the *string*. The per-type switch now lives in
//! `ScalcoutRecord::multi_output_buffer` (the C device support's job) and the
//! target resolution in `PvDatabase::resolve_out_target` (C's `dbNameToAddr` /
//! `dbCaGet*`); the boundaries of the switch itself are covered by the unit
//! tests in `records::scalcout`. This file proves the framework owner feeds it
//! the real target metadata end to end.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::stringout::StringoutRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// CALC yields SVAL, OCAL yields OSV, OVAL is numeric — the three C buffers
/// all distinct, so the target-type routing cannot pass by coincidence.
fn scalcout_with_out(out: &str) -> ScalcoutRecord {
    let mut sc = ScalcoutRecord::new();
    sc.put_field("CALC", EpicsValue::String("AA".into()))
        .unwrap();
    sc.special("CALC", true).unwrap();
    sc.put_field("AA", EpicsValue::String("calc-str".into()))
        .unwrap();
    sc.put_field("DOPT", EpicsValue::Short(1)).unwrap(); // Use OCAL
    sc.put_field("OCAL", EpicsValue::String("BB".into()))
        .unwrap();
    sc.special("OCAL", true).unwrap();
    sc.put_field("BB", EpicsValue::String("ocal-str".into()))
        .unwrap();
    sc.put_field("OUT", EpicsValue::String(out.into())).unwrap();
    sc
}

#[epics_macros_rs::epics_test]
async fn r14_61_string_target_receives_osv_not_oval() {
    let db = PvDatabase::new();
    db.add_record("SO_TGT", Box::new(StringoutRecord::new("seed")))
        .await
        .unwrap();
    db.add_record("SC_STR", Box::new(scalcout_with_out("SO_TGT")))
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SC_STR", &mut visited, 0)
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("SO_TGT").unwrap(),
        EpicsValue::String("ocal-str".into()),
        "a DBF_STRING target takes DBR_STRING from OSV (devsCalcoutSoft.c:131-134)"
    );
}

#[epics_macros_rs::epics_test]
async fn r14_61_char_array_target_receives_sval_bytes() {
    let db = PvDatabase::new();
    db.add_record(
        "WF_TGT",
        Box::new(WaveformRecord::new(12, DbFieldType::Char)),
    )
    .await
    .unwrap();
    db.add_record("SC_WF", Box::new(scalcout_with_out("WF_TGT")))
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SC_WF", &mut visited, 0)
        .await
        .unwrap();

    // Synchronous branch (a DB link is never async in C): the buffer is SVAL,
    // the CALC string — NOT OSV. NELM=12 bytes, the string NUL-padded out.
    let mut want = b"calc-str".to_vec();
    want.resize(12, 0);
    assert_eq!(
        db.get_pv("WF_TGT").unwrap(),
        EpicsValue::CharArray(want),
        "a DBF_CHAR array target takes DBF_CHAR from SVAL (devsCalcoutSoft.c:136-138)"
    );
}

#[epics_macros_rs::epics_test]
async fn r14_61_numeric_target_still_receives_oval() {
    let db = PvDatabase::new();
    db.add_record("AI_TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    let mut sc = scalcout_with_out("AI_TGT");
    // OCAL = "3+4" so OVAL is a defined 7 while OSV stays a numeric string.
    sc.put_field("OCAL", EpicsValue::String("3+4".into()))
        .unwrap();
    sc.special("OCAL", true).unwrap();
    db.add_record("SC_NUM", Box::new(sc)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SC_NUM", &mut visited, 0)
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("AI_TGT").unwrap(),
        EpicsValue::Double(7.0),
        "a numeric target keeps the DBR_DOUBLE OVAL put (devsCalcoutSoft.c:140)"
    );
}
