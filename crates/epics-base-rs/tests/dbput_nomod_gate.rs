//! R16-79: the `SPC_NOMOD` gate lives in `dbPut`, BELOW every put route.
//!
//! C `dbAccess.c:1330-1332` (`special == SPC_ATTRIBUTE` → `S_db_noMod`) and
//! `dbAccess.c:123-126` (`dbPutSpecial` pass 0, `SPC_NOMOD` → `S_db_noMod`) sit
//! inside `dbPut` — under `dbPutField` (CA / `dbpf`) AND under `dbPutLink` (a
//! record's OUT link). A refused `dbPutLink` then raises the WRITER's alarm
//! (`dbLink.c:444-446` → `setLinkAlarm` → LINK_ALARM / INVALID_ALARM).
//!
//! softIoc (EPICS 7.0.10, linux-x86_64):
//!
//! ```text
//! record(waveform,"WF"){field(FTVL,"DOUBLE") field(NELM,"10")}
//! record(ao,"AO"){field(OUT,"WF.NELM PP")}
//!
//! dbpf AO.VAL 2   -> recGblDbaddrError: dbPut Attempt to modify noMod field PV: WF.NELM
//! dbgf WF.NELM    -> 10          (unchanged)
//! dbgf AO.STAT    -> "LINK"
//! dbgf AO.SEVR    -> "INVALID"
//! dbpf WF.NELM 4  -> refused, still 10
//! ```
//!
//! Pre-fix the port enforced `read_only` only on the CA route, so the OUT link
//! truncated NELM (and the data with it) and the writer stayed NO_ALARM.

use std::collections::HashSet;

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn build() -> PvDatabase {
    let db = PvDatabase::new();
    let wf = WaveformRecord::new(10, DbFieldType::Double);
    db.add_record("WF", Box::new(wf)).await.unwrap();

    db.add_record("AO", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.put_pv("AO.OUT", EpicsValue::String("WF.NELM PP".into()))
        .await
        .unwrap();

    db.put_pv_and_post("WF", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
        .await
        .unwrap();
    db
}

/// The OUT link is the route C gates in `dbPut` and the port did not: a record
/// writing `WF.NELM` must be refused, the waveform must keep its NELM *and its
/// data*, and the WRITER must go LINK/INVALID.
#[tokio::test]
async fn out_link_write_to_a_nomod_field_is_refused_and_alarms_the_writer() {
    let db = build().await;

    db.put_pv("AO.VAL", EpicsValue::Double(2.0)).await.unwrap();
    let mut v = HashSet::new();
    db.process_record_with_links("AO", &mut v, 0).await.unwrap();

    let wf = db.get_record("WF").await.unwrap();
    let wf = wf.read().await;
    assert_eq!(
        wf.record.get_field("NELM").unwrap(),
        EpicsValue::Long(10),
        "C: dbPut refuses the SPC_NOMOD NELM — the link cannot truncate a waveform"
    );
    assert_eq!(
        wf.record.get_field("NORD").unwrap(),
        EpicsValue::Long(3),
        "and the data the waveform already holds is untouched"
    );
    drop(wf);

    let ao = db.get_record("AO").await.unwrap();
    let ao = ao.read().await;
    assert_eq!(
        ao.common.stat,
        alarm_status::LINK_ALARM,
        "C `dbPutLink` (dbLink.c:444-446): a failed put alarms the WRITER"
    );
    assert_eq!(ao.common.sevr, AlarmSeverity::Invalid);
}

/// The gate is in the shared `dbPut` owner, so the internal `put_pv` /
/// `put_pv_and_post` routes are refused too — not only the CA route.
#[tokio::test]
async fn every_put_route_is_refused_on_a_nomod_field() {
    let db = build().await;

    for res in [
        db.put_pv("WF.NELM", EpicsValue::Long(4)).await,
        db.put_pv_and_post("WF.NELM", EpicsValue::Long(4)).await,
        db.put_record_field_from_ca_no_notify("WF", "NELM", EpicsValue::Long(4))
            .await,
        // NORD and FTVL are SPC_NOMOD in the same `.dbd`.
        db.put_pv("WF.NORD", EpicsValue::Long(1)).await,
        db.put_pv("WF.FTVL", EpicsValue::Short(0)).await,
        // dbCommon SPC_NOMOD triple.
        db.put_pv("WF.PACT", EpicsValue::Short(1)).await,
        db.put_pv("WF.LCNT", EpicsValue::Short(1)).await,
        db.put_pv("WF.PUTF", EpicsValue::Short(1)).await,
    ] {
        assert!(
            matches!(res, Err(CaError::ReadOnlyField(_))),
            "C S_db_noMod on every route, got {res:?}"
        );
    }

    let wf = db.get_record("WF").await.unwrap();
    let wf = wf.read().await;
    assert_eq!(wf.record.get_field("NELM").unwrap(), EpicsValue::Long(10));
    assert_eq!(wf.record.get_field("NORD").unwrap(), EpicsValue::Long(3));
}

/// The gate is a *runtime* gate: `dbLoadRecords` sets NELM through
/// `dbStaticLib`'s `dbPutString`, which never crosses `dbPut`. The load path
/// (`Record::put_field`) must therefore still size the array.
#[tokio::test]
async fn the_load_path_still_sets_nelm() {
    let db = PvDatabase::new();
    let mut wf = WaveformRecord::new(1, DbFieldType::Double);
    wf.put_field("NELM", EpicsValue::Long(7)).unwrap();
    db.add_record("WF2", Box::new(wf)).await.unwrap();

    let rec = db.get_record("WF2").await.unwrap();
    assert_eq!(
        rec.read().await.record.get_field("NELM").unwrap(),
        EpicsValue::Long(7)
    );
}
