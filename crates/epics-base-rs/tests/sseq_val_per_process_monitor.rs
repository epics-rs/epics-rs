//! sseq posts `VAL` on EVERY process, with no value-change dedup.
//!
//! `VAL` is `field(VAL,DBF_LONG){ pp(TRUE) }` "Used to trigger"
//! (sseqRecord.dbd:29-33) — a processing trigger, like `fanout`/`seq`. But
//! where `fanout`/`seq` post `VAL` ONLY with the alarm bits `recGblResetAlarms`
//! returns (`if (events) db_post_events(&prec->val, events)`), sseq's
//! `asyncFinish` (sseqRecord.c:474) ends every process cycle with
//!
//! ```c
//! MonitorMask = DBE_VALUE | recGblResetAlarms(pR);
//! if (MonitorMask) db_post_events(pR, &pR->val, MonitorMask);
//! ```
//!
//! so `DBE_VALUE` is raised unconditionally — never `DBE_LOG`, and with no
//! comparison to the previous value. A run `caput VAL 1;2;2;3` therefore posts
//! FOUR value events, the repeated `2` included; the port's default MDEL/ADEL
//! deadband path (`MDEL == 0` → "post on any change") deduped the unchanged
//! repeat to three. The record now returns `monitor_value_changed = Some(false)`
//! and `monitor_always_post = (true, false)`, encoding "value class always,
//! archive class never".

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

fn full() -> u16 {
    (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits()
}

/// A bare sseq with no links configured: a put to `VAL` starts and completes a
/// sequence with no active step, synchronously, so the `asyncFinish` VAL post
/// lands before the put returns.
async fn bare_sseq() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("SQV", Box::new(SseqRecord::new()))
        .await
        .unwrap();
    db
}

/// `caput VAL 1;2;2;3` posts four `DBE_VALUE` events — the repeated `2` posts a
/// second time, because C posts VAL every process regardless of value change.
#[epics_macros_rs::epics_test]
async fn sseq_val_posts_every_process_including_the_unchanged_repeat() {
    let db = bare_sseq().await;
    let inst = db.get_record("SQV").unwrap();

    let mut val_rx = inst
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Long, full())
        .unwrap();

    for v in [1i32, 2, 2, 3] {
        db.put_record_field_from_ca("SQV", "VAL", EpicsValue::Long(v))
            .await
            .unwrap();
    }

    let mut got = Vec::new();
    while let Ok(e) = val_rx.try_recv() {
        got.push(e);
    }

    assert_eq!(
        got.len(),
        4,
        "C posts VAL on every process (asyncFinish, sseqRecord.c:474); the \
         repeated 2 must post a second event, so four total — got {}",
        got.len()
    );

    let values: Vec<EpicsValue> = got.iter().map(|e| e.snapshot.value.clone()).collect();
    assert_eq!(
        values,
        vec![
            EpicsValue::Long(1),
            EpicsValue::Long(2),
            EpicsValue::Long(2),
            EpicsValue::Long(3),
        ],
        "each put's VAL is posted in order, the unchanged 2 included"
    );

    for (i, e) in got.iter().enumerate() {
        assert!(
            e.mask.contains(EventMask::VALUE),
            "post {i} carries DBE_VALUE (asyncFinish's unconditional bit)"
        );
        assert!(
            !e.mask.contains(EventMask::LOG),
            "post {i} must NOT carry DBE_LOG — asyncFinish's mask is \
             DBE_VALUE | recGblResetAlarms (alarm bits only), never the \
             archive class, so a DBE_LOG-only archiver sees no per-process VAL"
        );
    }
}
