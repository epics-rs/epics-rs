//! `transformRecord.c::monitor` posts EVERY channel on the first cycle, not
//! only the ones that moved.
//!
//! ```c
//! /* transformRecord.c:793-807 */
//! monitor_mask = recGblResetAlarms(ptran);
//! monitor_mask = DBE_VALUE | DBE_LOG;
//! for (i = 0, pnew = &ptran->a, pprev = &ptran->la; i < MAX_FIELDS; i++, pnew++, pprev++) {
//!     if ((*pnew != *pprev) || (prpvt->firstCalcPosted == 0)) {
//!         db_post_events(ptran, pnew, monitor_mask);
//!         *pprev = *pnew;
//!     }
//! }
//! prpvt->firstCalcPosted = 1;
//! ```
//!
//! `rpvt` is `calloc`'d, the flag is set unconditionally after the loop and
//! never cleared, and `init_record` copies `*plvalue = *pvalue` — so the flag
//! is the ONLY reason an unchanged channel is ever posted, and it fires on
//! exactly one process cycle per IOC lifetime. Without it a `DBE_LOG`
//! archiver on a quiet transform takes no initial sample.
//!
//! Boundaries: the first cycle with nothing moving, the second cycle with
//! nothing moving, a channel that DID move on a later cycle, and the mask —
//! C overwrites `recGblResetAlarms`'s, so no alarm bit reaches these posts.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn transform_db() -> PvDatabase {
    let db = PvDatabase::new();
    let mut t = TransformRecord::default();
    t.put_field("A", EpicsValue::Double(5.0)).unwrap();
    db.add_record("T", Box::new(t)).await.unwrap();
    db
}

fn subscribe(db: &PvDatabase, field: &str, id: u32) -> EventReader {
    let inst = db.get_record("T").unwrap();
    let mut g = inst.write();
    let full = (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits();
    g.add_subscriber(field, id, DbFieldType::Double, full)
        .unwrap_or_else(|_| panic!("a {field} subscription must be accepted"))
}

async fn process(db: &PvDatabase) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("T", &mut visited)
        .await
        .unwrap();
}

/// The lead's trigger: a bare transform, no CLCx and no INPx, `camonitor T.A
/// T.B`, then `caput T.PROC 1`. C emits a second update on both.
#[epics_macros_rs::epics_test]
async fn the_first_cycle_posts_every_channel_though_none_moved() {
    let db = transform_db().await;
    let mut a = subscribe(&db, "A", 1);
    let mut b = subscribe(&db, "B", 2);

    process(&db).await;

    assert_eq!(
        a.try_recv()
            .map(|e| e.value().as_value().cloned().unwrap())
            .ok(),
        Some(EpicsValue::Double(5.0)),
        "firstCalcPosted == 0 posts A whether or not it moved"
    );
    assert_eq!(
        b.try_recv()
            .map(|e| e.value().as_value().cloned().unwrap())
            .ok(),
        Some(EpicsValue::Double(0.0)),
        "and every other channel with it, at its unchanged value"
    );
}

/// The flag is set unconditionally and never cleared, so cycle two is silent.
#[epics_macros_rs::epics_test]
async fn the_second_cycle_posts_nothing() {
    let db = transform_db().await;
    let mut a = subscribe(&db, "A", 1);

    process(&db).await;
    assert!(a.try_recv().is_ok(), "cycle 1 posts");

    process(&db).await;
    assert!(
        a.try_recv().is_err(),
        "nothing clears firstCalcPosted, so an unchanged channel is silent"
    );
}

/// oracleStdIoc, `record(transform,"TR"){field(CLCB,"A+1")}` under
/// `camonitor -m va TR.A`, `caput TR.A 7`: two posts, dbPut's and then the
/// `pp(TRUE)` cycle's — `monitor()` compares A against LA, which the put did
/// not move, not against what was last published.
#[epics_macros_rs::epics_test]
async fn a_put_to_a_posts_from_the_put_and_from_its_cycle() {
    let db = transform_db().await;
    let mut a = subscribe(&db, "A", 1);
    process(&db).await;
    while a.try_recv().is_ok() {}

    for v in [9.0, 11.0] {
        db.put_record_field_from_ca_no_notify("T", "A", EpicsValue::Double(v))
            .await
            .unwrap();
        let mut got = Vec::new();
        while let Ok(e) = a.try_recv() {
            got.push(e.value().as_value().cloned().unwrap());
        }
        assert_eq!(got, vec![EpicsValue::Double(v); 2], "caput T.A {v}");
        process(&db).await;
        assert!(
            a.try_recv().is_err(),
            "LA caught up, so the next cycle is silent"
        );
    }
}

/// C's `monitor_mask = DBE_VALUE|DBE_LOG` OVERWRITES `recGblResetAlarms`'s
/// return, so the first-cycle posts carry no alarm bit — a `DBE_ALARM`-only
/// subscriber sees nothing.
#[epics_macros_rs::epics_test]
async fn the_first_cycle_posts_carry_no_alarm_bit() {
    let db = transform_db().await;
    let mut alarm_only = {
        let inst = db.get_record("T").unwrap();
        let mut g = inst.write();
        g.add_subscriber("A", 3, DbFieldType::Double, EventMask::ALARM.bits())
            .expect("an A subscription must be accepted")
    };

    process(&db).await;

    assert!(
        alarm_only.try_recv().is_err(),
        "transformRecord.c:794 throws the alarm mask away"
    );
}
