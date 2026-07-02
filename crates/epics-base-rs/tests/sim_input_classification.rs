//! `waveform` and `histogram` are `readValue` INPUT records — both call
//! `readValue` at the START of `process()` and read SIOL inward in SIMM mode
//! (`waveformRecord.c:139`->`:351` `dbGetLink(&siol, ftvl, bptr)`;
//! `histogramRecord.c:209`->`:384` `dbGetLink(&siol, DBR_DOUBLE, &sval)`). They
//! were omitted from the simulation `is_input` classification, so a simulated
//! waveform/histogram took the OUTPUT redirect: it ran the real device read and
//! wrote VAL OUT to SIOL (direction inverted, simulation defeated). These tests
//! pin the corrected input direction.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::histogram::HistogramRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// A simulated `waveform` reads its SIOL array INTO VAL (C `readValue` ->
/// `dbGetLink(&siol, ftvl, bptr)`), and leaves the SIOL source untouched. Under
/// the pre-fix output misclassification the record instead ran its body and
/// wrote VAL OUT to the SIOL target — so VAL would NOT carry the source array.
#[tokio::test]
async fn sim_waveform_reads_siol_array_into_val() {
    let db = PvDatabase::new();
    db.add_record("WFIN_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();

    // SIOL source: a waveform carrying a distinctive array.
    let mut src = WaveformRecord::new(8, DbFieldType::Double);
    let _ = src.put_field("VAL", EpicsValue::DoubleArray(vec![10.0, 20.0, 30.0]));
    db.add_record("WFIN_SRC", Box::new(src)).await.unwrap();

    let mut wf = WaveformRecord::new(8, DbFieldType::Double);
    wf.siml = "WFIN_SW".to_string();
    wf.siol = "WFIN_SRC".to_string();
    db.add_record("WFIN", Box::new(wf)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("WFIN", &mut v, 0)
        .await
        .unwrap();

    // VAL read inward from SIOL.
    let val = db.get_pv("WFIN").await.unwrap();
    assert!(
        matches!(val, EpicsValue::DoubleArray(ref a) if a.as_slice() == [10.0, 20.0, 30.0]),
        "simulated waveform read the SIOL array INTO VAL, got {val:?}"
    );
    // SIOL source untouched (the record read from it, did not write to it).
    let src = db.get_pv("WFIN_SRC").await.unwrap();
    assert!(
        matches!(src, EpicsValue::DoubleArray(ref a) if a.as_slice() == [10.0, 20.0, 30.0]),
        "SIOL source untouched (input direction, not VAL->SIOL write), got {src:?}"
    );
}

/// A simulated `histogram` must NOT run the real device read nor write VAL out
/// to its SIOL target. (It reads SIOL inward; the SIOL scalar does not land in
/// the bin-count array, so VAL stays put — the histogram SIOL->SVAL feed +
/// accumulation is a separate pre-existing gap.) Under the pre-fix output
/// misclassification the record ran `add_count` and wrote its VAL array out to
/// the SIOL target, overwriting it.
#[tokio::test]
async fn sim_histogram_does_not_write_val_out_to_siol() {
    let db = PvDatabase::new();
    db.add_record("HGIN_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    // SIOL source carries a sentinel; a VAL->SIOL write would overwrite it.
    db.add_record("HGIN_SRC", Box::new(AoRecord::new(42.0)))
        .await
        .unwrap();

    let mut hg = HistogramRecord::new(4, 0.0, 100.0);
    hg.siml = "HGIN_SW".to_string();
    hg.siol = "HGIN_SRC".to_string();
    db.add_record("HGIN", Box::new(hg)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("HGIN", &mut v, 0)
        .await
        .unwrap();

    // SIOL source untouched — the record read from it, not wrote to it.
    let src = db.get_pv("HGIN_SRC").await.unwrap();
    assert!(
        matches!(src, EpicsValue::Double(v) if (v - 42.0).abs() < 1e-10),
        "simulated histogram did NOT write VAL out to SIOL (input direction), got {src:?}"
    );
    // The bin-count array is unchanged (no accumulation from the sim read).
    let val = db.get_pv("HGIN").await.unwrap();
    assert!(
        matches!(val, EpicsValue::LongArray(ref a) if a.as_slice() == [0, 0, 0, 0]),
        "histogram bin array unchanged in sim (no real-device accumulation), got {val:?}"
    );
}
