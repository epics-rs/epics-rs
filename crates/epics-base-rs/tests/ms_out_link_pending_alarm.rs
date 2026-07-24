//! R15-61 — an MS-class OUT link inherits the source's alarm from the cycle
//! that is writing it, not from the previous one.
//!
//! C `dbDbPutValue` (dbDbLink.c:372-393):
//!
//! ```c
//! long status = dbPut(paddr, dbrType, pbuffer, nRequest);
//! recGblInheritSevrMsg(ppv_link->pvlMask & pvlOptMsMode, pdest, psrce->nsta,
//!     psrce->nsev, psrce->namsg);
//! ```
//!
//! `dbPutLink` runs from inside the source's `process()`, BEFORE
//! `recGblResetAlarms` commits the cycle, so the fields C reads (`nsta`,
//! `nsev`, `namsg`) are the source's PENDING alarm — the one this cycle just
//! raised. A port reading the COMMITTED `stat`/`sevr` there hands every
//! MS target the PREVIOUS cycle's severity: the target goes INVALID one cycle
//! late, and stays INVALID for one cycle after the source has recovered.
//!
//! The INPUT side is the opposite and unchanged: `dbDbGetValue`
//! (dbDbLink.c:229-232) inherits `dbChannelRecord(chan)->stat`/`->sevr` — the
//! committed alarm of a foreign record that finished its own cycle.
//!
//! Boundaries below: the `WriteDbLink` process-action path (transform OUTx),
//! the `dispatch_multi_output` path (dfanout OUTx), the recovery cycle, and
//! NMS (no propagation at all).

// RTEMS-EXEC-MODEL-ALLOW(3): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::dfanout::DfanoutRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::EpicsValue;

/// SRC: ai with HIHI=100 / HHSV=INVALID — INVALID while VAL=200, clean once
/// VAL drops below the limit. A finite value, so nothing but the limit alarm
/// travels the link.
async fn add_source(db: &PvDatabase) {
    db.add_record("SRC", Box::new(AiRecord::new(200.0)))
        .await
        .unwrap();
    let rec = db.get_record("SRC").unwrap();
    let mut inst = rec.write();
    inst.put_common_field("HIHI", EpicsValue::Double(100.0))
        .unwrap();
    inst.put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Invalid as i16))
        .unwrap();
}

async fn process(db: &PvDatabase, name: &str) {
    let mut v = HashSet::new();
    db.process_record_with_links(name, &mut v, 0).await.unwrap();
}

async fn alarm_of(db: &PvDatabase, name: &str) -> (u16, AlarmSeverity) {
    let rec = db.get_record(name).expect("record exists");
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr)
}

async fn add_target(db: &PvDatabase, name: &str) {
    db.add_record(name, Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
}

/// Boundary 1 + 4 — the `WriteDbLink` action path (transform `OUTx`).
/// `OUTA` is MS, `OUTB` is bare (NMS). One process of TR must leave the MS
/// target INVALID and the NMS target clean.
#[tokio::test]
async fn r15_61_write_db_link_out_inherits_this_cycles_pending_severity() {
    let db = PvDatabase::new();
    add_source(&db).await;
    process(&db, "SRC").await;
    add_target(&db, "TGT_MS").await;
    add_target(&db, "TGT_NMS").await;

    let mut tr = TransformRecord::new();
    tr.put_field("INPA", EpicsValue::String("SRC MS".into()))
        .unwrap();
    tr.put_field("OUTA", EpicsValue::String("TGT_MS MS PP".into()))
        .unwrap();
    tr.put_field("OUTB", EpicsValue::String("TGT_NMS PP".into()))
        .unwrap();
    db.add_record("TR", Box::new(tr)).await.unwrap();

    process(&db, "TR").await;

    assert_eq!(
        alarm_of(&db, "TR").await.1,
        AlarmSeverity::Invalid,
        "TR's MS input link must make TR INVALID in this cycle"
    );
    assert_eq!(
        alarm_of(&db, "TGT_MS").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Invalid),
        "an MS OUT link inherits the source's PENDING severity — the INVALID \
         raised THIS cycle, not the previous cycle's committed NO_ALARM \
         (dbDbLink.c:382-383 reads psrce->nsev)"
    );
    assert_eq!(
        alarm_of(&db, "TGT_NMS").await,
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm),
        "NMS propagates nothing, pending or committed"
    );
}

/// Boundary 2 — the recovery cycle. The source clears, and the MS target must
/// clear in the SAME cycle. Reading the committed alarm would hand the target
/// the previous cycle's INVALID one cycle after the source recovered.
#[tokio::test]
async fn r15_61_ms_out_target_clears_in_the_cycle_the_source_clears() {
    let db = PvDatabase::new();
    add_source(&db).await;
    process(&db, "SRC").await;
    add_target(&db, "TGT_MS").await;

    let mut tr = TransformRecord::new();
    tr.put_field("INPA", EpicsValue::String("SRC MS".into()))
        .unwrap();
    tr.put_field("OUTA", EpicsValue::String("TGT_MS MS PP".into()))
        .unwrap();
    db.add_record("TR", Box::new(tr)).await.unwrap();

    process(&db, "TR").await;
    assert_eq!(
        alarm_of(&db, "TGT_MS").await.1,
        AlarmSeverity::Invalid,
        "precondition: the target is INVALID while the source is"
    );

    // SRC drops back under HIHI and commits NO_ALARM.
    db.put_pv("SRC", EpicsValue::Double(10.0)).await.unwrap();
    process(&db, "SRC").await;
    assert_eq!(
        alarm_of(&db, "SRC").await.1,
        AlarmSeverity::NoAlarm,
        "precondition: the source recovered"
    );

    process(&db, "TR").await;
    assert_eq!(
        alarm_of(&db, "TGT_MS").await,
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm),
        "the MS target must clear in the cycle the source's PENDING alarm is \
         clean — not one cycle later"
    );
}

/// Boundary 3 — the `dispatch_multi_output` path (dfanout `OUTx`). Same
/// snapshot, a different writer: dfanout's own HIHI/HHSV puts it INVALID this
/// cycle, and its MS `OUTA` must carry that INVALID out with the value.
#[tokio::test]
async fn r15_61_dfanout_ms_out_inherits_this_cycles_pending_severity() {
    let db = PvDatabase::new();
    add_target(&db, "DF_TGT").await;

    let mut df = DfanoutRecord::new(200.0);
    df.put_field("HIHI", EpicsValue::Double(100.0)).unwrap();
    df.put_field("HHSV", EpicsValue::Short(AlarmSeverity::Invalid as i16))
        .unwrap();
    df.put_field("OUTA", EpicsValue::String("DF_TGT MS PP".into()))
        .unwrap();
    db.add_record("DF", Box::new(df)).await.unwrap();

    process(&db, "DF").await;

    assert_eq!(
        alarm_of(&db, "DF").await.1,
        AlarmSeverity::Invalid,
        "precondition: VAL=200 over HIHI=100/HHSV=INVALID"
    );
    assert_eq!(
        alarm_of(&db, "DF_TGT").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Invalid),
        "dfanout's MS OUTn inherits the pending severity of the cycle that \
         pushed the value (dfanoutRecord.c:323 dbPutLink → dbDbPutValue)"
    );
}
