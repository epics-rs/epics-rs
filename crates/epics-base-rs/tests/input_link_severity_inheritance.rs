//! R16-61 / R16-62: `dbDbGetValue` ends with `recGblInheritSevrMsg` on EVERY
//! healthy read — and never for a link that points back at the reader.
//!
//! ```c
//! /* dbDbLink.c:228-232 */
//! if (!status && precord != dbChannelRecord(chan))
//!     recGblInheritSevrMsg(plink->value.pv_link.pvlMask & pvlOptMsMode,
//!         plink->precord, dbChannelRecord(chan)->stat,
//!         dbChannelRecord(chan)->sevr, dbChannelRecord(chan)->amsg);
//! ```
//!
//! softIoc (EPICS 7.0.10, linux-x86_64):
//!
//! ```text
//! record(ai,"SRC"){field(VAL,"7") field(HIGH,"1") field(HSV,"MAJOR")}
//! record(compress,"CMP"){field(INP,"SRC MS") field(NSAM,"4")}
//! record(calc,"SELF"){field(INPA,"SELF.VAL MS") field(CALC,"A")
//!                     field(HIGH,"1") field(HSV,"MAJOR")}
//!
//! SRC.PROC -> SRC MAJOR/HIGH
//! CMP.PROC -> CMP.STAT = LINK, CMP.SEVR = MAJOR        (R16-61: port had NO_ALARM)
//! SELF.PROC (VAL=5, HIGH=1)  -> SELF MAJOR/HIGH
//! SELF.HIGH=100; SELF.PROC   -> SELF NO_ALARM/NO_ALARM (R16-62: port latched MAJOR)
//! ```
//!
//! The port folded MS only in the multi-input fetch stage, so the ReadDbLink
//! path (compress INP, aao closed-loop DOL, epid OUTL) dropped it — and it
//! folded a record's OWN committed severity back into its pending alarm, which
//! `recGblResetAlarms` then re-committed: a self-sustaining latch.

// RTEMS-EXEC-MODEL-ALLOW(3): checked - these run and pass in the feature-ON suite.

use std::collections::HashMap;
use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

async fn build(db_text: &str) -> std::sync::Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut v = HashSet::new();
    db.process_record_with_links(rec, &mut v, 0).await.unwrap();
}

async fn alarm(db: &PvDatabase, rec: &str) -> (u16, AlarmSeverity) {
    let r = db.get_record(rec).unwrap();
    let inst = r.read();
    (inst.common.stat, inst.common.sevr)
}

/// R16-61: the ReadDbLink reader (compress INP) inherits MS from a healthy read.
#[tokio::test]
async fn a_read_db_link_input_inherits_ms_severity() {
    let db = build(
        r#"
        record(ai, "SRC") { field(VAL, "7") field(HIGH, "1") field(HSV, "MAJOR") }
        record(compress, "CMP") { field(INP, "SRC MS") field(NSAM, "4") }
        "#,
    )
    .await;

    process(&db, "SRC").await;
    assert_eq!(
        alarm(&db, "SRC").await,
        (alarm_status::HIGH_ALARM, AlarmSeverity::Major),
        "the source is in a MAJOR HIGH alarm"
    );

    process(&db, "CMP").await;
    assert_eq!(
        alarm(&db, "CMP").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Major),
        "C dbDbGetValue inherits the source severity under LINK_ALARM on the \
         healthy read (dbDbLink.c:228-232) — the reader was NO_ALARM pre-fix"
    );
}

/// NMS (the default) still inherits nothing — the MS class is what selects it.
#[tokio::test]
async fn a_read_db_link_input_without_ms_inherits_nothing() {
    let db = build(
        r#"
        record(ai, "SRC2") { field(VAL, "7") field(HIGH, "1") field(HSV, "MAJOR") }
        record(compress, "CMP2") { field(INP, "SRC2") field(NSAM, "4") }
        "#,
    )
    .await;

    process(&db, "SRC2").await;
    process(&db, "CMP2").await;
    assert_eq!(
        alarm(&db, "CMP2").await,
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm),
        "NMS: recGblInheritSevrMsg propagates nothing"
    );
}

/// R16-62: a link that reads the reader's OWN field must not fold the reader's
/// committed severity back in — C's `precord != dbChannelRecord(chan)` guard.
/// Without it the alarm latches: reset_alarms commits it, next cycle inherits
/// it again, forever.
#[tokio::test]
async fn a_self_referencing_ms_link_does_not_latch_the_record_alarm() {
    let db = build(
        r#"record(calc, "SELF") { field(INPA, "SELF.VAL MS") field(CALC, "A")
                                 field(HIGH, "1") field(HSV, "MAJOR") }"#,
    )
    .await;

    db.put_pv("SELF.VAL", EpicsValue::Double(5.0))
        .await
        .unwrap();
    process(&db, "SELF").await;
    assert_eq!(
        alarm(&db, "SELF").await,
        (alarm_status::HIGH_ALARM, AlarmSeverity::Major),
        "VAL=5 > HIGH=1: the record's OWN limit alarm"
    );

    // Raise the limit: the alarm condition is gone, so the record must come
    // back to NO_ALARM. Pre-fix, the self MS link folded the committed MAJOR
    // into the pending alarm every cycle and it never cleared.
    db.put_pv("SELF.HIGH", EpicsValue::Double(100.0))
        .await
        .unwrap();
    process(&db, "SELF").await;
    assert_eq!(
        alarm(&db, "SELF").await,
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm),
        "C excludes the self-link from inheritance, so the alarm clears"
    );

    // And it stays clear — no residue re-arms it on the next cycle.
    process(&db, "SELF").await;
    assert_eq!(
        alarm(&db, "SELF").await,
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm)
    );
}
