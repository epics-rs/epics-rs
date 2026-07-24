//! `aai` is a SIOL-reading input in simulation, but the SIOL read lives in
//! its soft DEVICE support, not the record support. `aaiRecord.c::readValue`
//! (:348) raises `recGblSetSevr(prec, SIMM_ALARM, prec->sims)` (:364) then
//! calls `read_aai`, and `devAaiSoft.c::read_aai` (:88) reads
//! `prec->simm == menuYesNoYES ? &prec->siol : &prec->inp` — so SIMM=YES reads
//! the SIOL array INTO VAL, observably identical to `waveform`. (Reading only
//! the record support `readValue` misleads: it looks device-only, but the soft
//! device is what redirects to SIOL — exactly as `devAaoSoft.c::write_aao`
//! (:56) writes `simm == YES ? &siol : &out` for the `aao` OUTPUT twin.)
//!
//! `aai` declares the full SIML/SIOL/SIMM/SIMS block (`aaiRecord.dbd.pod`
//! SIML:374, SIOL:391 DBF_INLINK) so `has_sim_block()` is true, but it was
//! omitted from the simulation `is_input` whitelist — so a simulated `aai`
//! took the OUTPUT redirect and wrote its VAL array OUT to its DBF_INLINK SIOL
//! (direction inverted). Classifying it as an input pins the correct SIOL read.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::waveform::{ArrayKind, WaveformRecord};
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// A simulated `aai` reads its SIOL array INTO VAL (C `read_aai` ->
/// `dbGetLink(&siol)` when SIMM=YES), raises SIMM_ALARM at the SIMS severity,
/// and leaves the SIOL source untouched. Under the pre-fix output
/// misclassification the record instead wrote VAL OUT to the SIOL target.
#[epics_macros_rs::epics_test]
async fn sim_aai_reads_siol_array_into_val_and_raises_simm_alarm() {
    const SIMM_ALARM: i16 = 19;
    const MINOR: i16 = 1;

    let db = PvDatabase::new();
    // SIML switch -> SIMM=YES.
    db.add_record("AAI_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();

    // SIOL source carries a distinctive sentinel array; a simulated aai must
    // read it inward into VAL (and must NOT overwrite the source).
    let mut siol_src = WaveformRecord::new(8, DbFieldType::Double);
    let _ = siol_src.put_field("VAL", EpicsValue::DoubleArray(vec![10.0, 20.0, 30.0]));
    db.add_record("AAI_SIOL", Box::new(siol_src)).await.unwrap();

    let mut aai = WaveformRecord::new(8, DbFieldType::Double);
    aai.kind = ArrayKind::Aai;
    // VAL sentinel, distinct from the SIOL source array, to show it is replaced
    // by the inward SIOL read (not left stale, not written out).
    let _ = aai.put_field("VAL", EpicsValue::DoubleArray(vec![7.0, 7.0, 7.0]));
    aai.siml = "AAI_SW".to_string();
    aai.siol = "AAI_SIOL".to_string();
    aai.sims = MINOR;
    db.add_record("AAIIN", Box::new(aai)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("AAIIN", &mut v, 0)
        .await
        .unwrap();

    // 1. VAL read inward from SIOL (the soft device reads SIOL when SIMM=YES).
    let val = db.get_pv("AAIIN").unwrap();
    assert!(
        matches!(val, EpicsValue::DoubleArray(ref a) if a.as_slice() == [10.0, 20.0, 30.0]),
        "simulated aai read its SIOL array INTO VAL, got {val:?}"
    );

    // 2. SIOL source untouched (input direction, not a VAL->SIOL write).
    let siol = db.get_pv("AAI_SIOL").unwrap();
    assert!(
        matches!(siol, EpicsValue::DoubleArray(ref a) if a.as_slice() == [10.0, 20.0, 30.0]),
        "aai must NOT write VAL out to its SIOL target, got {siol:?}"
    );

    // 3. SIMM_ALARM raised at the SIMS severity.
    let sevr = db.get_pv("AAIIN.SEVR").unwrap();
    assert!(
        matches!(sevr, EpicsValue::Short(MINOR)),
        "simulated aai raises SIMM_ALARM at SIMS severity MINOR, got {sevr:?}"
    );
    let stat = db.get_pv("AAIIN.STAT").unwrap();
    assert!(
        matches!(stat, EpicsValue::Short(SIMM_ALARM)),
        "simulated aai STAT is SIMM_ALARM, got {stat:?}"
    );
}
