//! R15-62 — a failed seq `LNKn` put alarms in the cycle that issued it.
//!
//! C `seqRecord.c` puts each group's value from `processCallback`
//! (`dbPutLink(&pgrp->lnk, DBR_DOUBLE, &pgrp->dov, 1)`, :264) and commits the
//! cycle's alarm only when the last group is done, in `asyncFinish`
//! (`recGblResetAlarms(prec)`, :227). Every put therefore precedes the commit,
//! even though seq is an async record: a `dbPutLink` failure's
//! `LINK_ALARM`/`INVALID` (`setLinkAlarm` inside `dbPutLink`, dbLink.c:444-446)
//! lands in `nsev` and is committed and posted by THAT cycle.
//!
//! The port drove seq's `LNKn` from the post-commit forward-link tail, so the
//! alarm the put raised sat in `nsev` until the NEXT process — a one-shot
//! passive seq never showed it at all. seq now dispatches in the pre-commit
//! output stage with dfanout; fanout stays in the tail, where it belongs: its
//! `LNK0..LNKF` are `DBF_FWDLINK` (`dbScanFwdLink`), driving no value and
//! raising no put alarm.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::fanout::FanoutRecord;
use epics_base_rs::server::records::seq::SeqRecord;
use epics_base_rs::types::EpicsValue;

async fn process(db: &PvDatabase, name: &str) {
    let mut v = HashSet::new();
    db.process_record_with_links(name, &mut v, 0).await.unwrap();
}

async fn alarm_of(db: &PvDatabase, name: &str) -> (u16, AlarmSeverity) {
    let rec = db.get_record(name).expect("record exists");
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr)
}

/// Boundary 1 — the LNKn target does not exist, so the put fails. ONE process
/// of the seq must leave LINK/INVALID committed: this is the whole point of
/// the finding, and it is what a one-shot passive seq (processed once by a
/// client put, never scanned again) can observe.
#[epics_macros_rs::epics_test]
async fn r15_62_failed_seq_lnk_put_alarms_in_the_same_cycle() {
    let db = PvDatabase::new();
    db.add_record("SEQ_SRC", Box::new(AoRecord::new(7.0)))
        .await
        .unwrap();

    let mut seq = SeqRecord::new();
    seq.selm = 0; // All
    seq.dol1 = "SEQ_SRC".to_string();
    seq.lnk1 = "NO_SUCH_TARGET".to_string();
    db.add_record("SEQ_BAD", Box::new(seq)).await.unwrap();

    process(&db, "SEQ_BAD").await;

    assert_eq!(
        alarm_of(&db, "SEQ_BAD").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Invalid),
        "a failed LNKn dbPutLink raises LINK/INVALID into nsev, and seq's \
         asyncFinish commits it in the SAME cycle (seqRecord.c:227/264)"
    );
}

/// Boundary 2 — the recovery cycle: every LNKn put succeeds, so the seq must
/// commit NO_ALARM and the target must hold the driven value. Guards against
/// a fix that simply pins the alarm on.
#[epics_macros_rs::epics_test]
async fn r15_62_successful_seq_lnk_put_leaves_no_alarm() {
    let db = PvDatabase::new();
    db.add_record("SEQ_SRC", Box::new(AoRecord::new(7.0)))
        .await
        .unwrap();
    db.add_record("SEQ_DST", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut seq = SeqRecord::new();
    seq.selm = 0;
    seq.dol1 = "SEQ_SRC".to_string();
    seq.lnk1 = "SEQ_DST".to_string();
    db.add_record("SEQ_OK", Box::new(seq)).await.unwrap();

    process(&db, "SEQ_OK").await;

    assert_eq!(
        alarm_of(&db, "SEQ_OK").await,
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm),
        "a seq whose puts all succeed commits no alarm"
    );
    assert_eq!(
        db.get_pv("SEQ_DST").unwrap(),
        EpicsValue::Double(7.0),
        "the LNKn put still drives the target from the pre-commit stage"
    );
}

/// Boundary 3 — fanout is unchanged: its `LNKn` are `DBF_FWDLINK`, dispatched
/// with `dbScanFwdLink` (fanoutRecord.c:110). A link naming a record that does
/// not exist scans nothing and raises NOTHING — there is no put and so no
/// `setLinkAlarm`. It must not be dragged into the value-put phase.
#[epics_macros_rs::epics_test]
async fn r15_62_fanout_forward_links_still_raise_no_put_alarm() {
    let db = PvDatabase::new();
    db.add_record("FO_DST", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut fanout = FanoutRecord::new();
    fanout.selm = 0; // All
    fanout.lnk1 = "FO_DST".to_string();
    fanout.lnk2 = "NO_SUCH_TARGET".to_string();
    db.add_record("FO", Box::new(fanout)).await.unwrap();

    process(&db, "FO").await;

    assert_eq!(
        alarm_of(&db, "FO").await,
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm),
        "a fanout FWDLINK to a missing record is a no-op scan, not a failed \
         put — no LINK_ALARM (dbDbScanFwdLink, dbDbLink.c:425-432)"
    );
    assert_eq!(
        db.get_pv("FO_DST").unwrap(),
        EpicsValue::Double(0.0),
        "a fanout drives no value into its targets"
    );
}
