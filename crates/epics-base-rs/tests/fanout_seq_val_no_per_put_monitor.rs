//! Family A (monitor phase): `fanout.VAL` / `seq.VAL` are `pp(TRUE)` "trigger"
//! fields, and a run of `caput VAL` posts NO per-put value monitor.
//!
//! Their C `process()` posts VAL only with the alarm events `recGblResetAlarms`
//! returns:
//!
//! ```c
//! /* fanoutRecord.c:147-150, seqRecord.c:226-229 */
//! events = recGblResetAlarms(prec);
//! if (events)
//!     db_post_events(prec, &prec->val, events);   /* events = alarm bits only */
//! ```
//!
//! `events` never carries `DBE_VALUE | DBE_LOG`, so on a normal put (no alarm
//! transition) VAL is not posted at all — writing it fans out the forward links
//! / sequences the `DOn`→`LNKn` writes; the value itself is not a monitored
//! quantity. Against the consolidated oracle:
//!
//! ```text
//! camonitor REC & ; caput REC 1 ; caput REC 2 ; caput REC 2 ; caput REC 3
//! C:    monitor_count=1, events="1"          (the "1" is the connect snapshot)
//! port: monitor_count=3, events="1 -> 2 -> 3" (a value event per accepted put)
//! ```
//!
//! The port over-posted because its generic process cycle ran the VAL value
//! post (MDEL=0 deadband → post on every change). `Record::process_posts_value_
//! monitor() == false` (via `#[record(no_value_monitor)]`) suppresses the
//! value/archive classes, leaving only the alarm-driven post — exactly C's
//! `if (events)`. This test asserts ZERO value events across a put sequence
//! (the in-process subscribe seam emits no connect snapshot, so every event
//! here would be an over-post) while VAL still stores the last put.

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::fanout::FanoutRecord;
use epics_base_rs::server::records::seq::SeqRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// `caput REC.VAL <text>` over the CA put path (stores + pp reprocess).
async fn caput_val(db: &PvDatabase, text: &str) {
    db.put_record_field_from_ca("REC", "VAL", EpicsValue::String(text.into()))
        .await
        .unwrap_or_else(|e| panic!("caput VAL {text} must be accepted, got {e}"));
}

async fn stored_val(db: &PvDatabase) -> EpicsValue {
    let rec = db.get_record("REC").unwrap();
    let inst = rec.read();
    inst.record.get_field("VAL").unwrap()
}

/// Subscribe a VALUE|LOG monitor on `REC.VAL`, drive the oracle's put sequence,
/// and return the number of value events delivered (must be 0).
async fn per_put_value_events(db: &PvDatabase) -> usize {
    let inst = db.get_record("REC").unwrap();
    let mut rx = inst
        .write()
        .add_subscriber(
            "VAL",
            1,
            DbFieldType::Long,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .expect("a VAL subscription must be accepted");

    for text in ["1", "2", "2", "3"] {
        caput_val(db, text).await;
    }
    // VAL still stores the last put — the put arm is untouched, only the
    // monitor is suppressed.
    assert_eq!(
        stored_val(db).await,
        EpicsValue::Long(3),
        "the value-monitor suppression must not affect the stored VAL"
    );

    let mut count = 0;
    while rx.try_recv().is_ok() {
        count += 1;
    }
    count
}

async fn db_with(record: Box<dyn Record>) -> PvDatabase {
    // A bare record with SELM=All and no links: process is a no-op fan-out that
    // raises no alarm, so `events == 0` on every cycle — the value monitor is
    // the only thing that could fire, and it must not.
    let db = PvDatabase::new();
    db.add_record("REC", record).await.unwrap();
    db
}

#[tokio::test]
async fn fanout_val_posts_no_per_put_monitor() {
    let db = db_with(Box::new(FanoutRecord::new())).await;
    assert_eq!(
        per_put_value_events(&db).await,
        0,
        "fanout VAL is a trigger: a run of caput VAL must post no value monitor"
    );
}

#[tokio::test]
async fn seq_val_posts_no_per_put_monitor() {
    let db = db_with(Box::new(SeqRecord::new())).await;
    assert_eq!(
        per_put_value_events(&db).await,
        0,
        "seq VAL is a trigger: a run of caput VAL must post no value monitor"
    );
}
