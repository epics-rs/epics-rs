//! `waveform` and `histogram` are `readValue` INPUT records — both call
//! `readValue` at the START of `process()` and read SIOL inward in SIMM mode
//! (`waveformRecord.c:139`->`:351` `dbGetLink(&siol, ftvl, bptr)`;
//! `histogramRecord.c:209`->`:384` `dbGetLink(&siol, DBR_DOUBLE, &sval)`). They
//! were omitted from the simulation `is_input` classification, so a simulated
//! waveform/histogram took the OUTPUT redirect: it ran the real device read and
//! wrote VAL OUT to SIOL (direction inverted, simulation defeated). These tests
//! pin the corrected input direction.

// RTEMS-EXEC-MODEL-ALLOW(3): checked - these run and pass in the feature-ON suite.

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
    let val = db.get_pv("WFIN").unwrap();
    assert!(
        matches!(val, EpicsValue::DoubleArray(ref a) if a.as_slice() == [10.0, 20.0, 30.0]),
        "simulated waveform read the SIOL array INTO VAL, got {val:?}"
    );
    // SIOL source untouched (the record read from it, did not write to it).
    let src = db.get_pv("WFIN_SRC").unwrap();
    assert!(
        matches!(src, EpicsValue::DoubleArray(ref a) if a.as_slice() == [10.0, 20.0, 30.0]),
        "SIOL source untouched (input direction, not VAL->SIOL write), got {src:?}"
    );
}

/// W10-E8 — a simulated `histogram` reads SIOL INWARD, lands the scalar in
/// SGNL (`histogramRecord.c:384-387` `dbGetLink(&siol, DBR_DOUBLE, &sval)`;
/// `sgnl = sval`) and bins it (`:218-219` `if (status == 0) add_count(prec)`).
/// It must not write its VAL array out to the SIOL target.
///
/// SIOL = 42.0 with LLIM=0, ULIM=100, NELM=4 → WDTH=25, and C's
/// `for (i = 1; i <= nelm; i++) if (temp <= i*wdth) break;` picks i=2, so
/// `bptr[1]` is incremented: VAL = [0, 1, 0, 0].
#[tokio::test]
async fn sim_histogram_lands_siol_in_sgnl_and_bins_it() {
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
    let src = db.get_pv("HGIN_SRC").unwrap();
    assert!(
        matches!(src, EpicsValue::Double(v) if (v - 42.0).abs() < 1e-10),
        "simulated histogram did NOT write VAL out to SIOL (input direction), got {src:?}"
    );
    // C `:385-386`: the SIOL scalar lands in SVAL, then in SGNL.
    assert_eq!(db.get_pv("HGIN.SVAL").unwrap(), EpicsValue::Double(42.0));
    assert_eq!(
        db.get_pv("HGIN.SGNL").unwrap(),
        EpicsValue::Double(42.0),
        "C `prec->sgnl = prec->sval` (histogramRecord.c:385)"
    );

    // C `:219` `add_count(prec)`: 42.0 with WDTH=25 falls in bin 1.
    let val = db.get_pv("HGIN").unwrap();
    assert!(
        matches!(val, EpicsValue::ULongArray(ref a) if a.as_slice() == [0, 1, 0, 0]),
        "the simulated signal is binned exactly once, got {val:?}"
    );
}

/// A FAILED SIOL read leaves C's `status != 0`, so `readValue` never runs
/// `prec->sgnl = prec->sval` (`:385`, gated on `status == 0`) and `process`
/// never runs `add_count` (`:218-219`, the same gate). No bin moves.
#[tokio::test]
async fn sim_histogram_with_a_failed_siol_read_bins_nothing() {
    let db = PvDatabase::new();
    db.add_record("HGF_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();

    let mut hg = HistogramRecord::new(4, 0.0, 100.0);
    hg.siml = "HGF_SW".to_string();
    hg.siol = "NO:SUCH:RECORD".to_string();
    hg.sval = 42.0; // would land in bin 1 if the read were treated as OK
    db.add_record("HGF", Box::new(hg)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("HGF", &mut v, 0)
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("HGF.SGNL").unwrap(),
        EpicsValue::Double(0.0),
        "C gates `prec->sgnl = prec->sval` on status == 0 — a failed read assigns nothing"
    );
    let val = db.get_pv("HGF").unwrap();
    assert!(
        matches!(val, EpicsValue::ULongArray(ref a) if a.as_slice() == [0, 0, 0, 0]),
        "add_count is gated on the same status == 0, got {val:?}"
    );
}
