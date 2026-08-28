//! R18-107: histogram's MDEL is a COUNT deadband on the VAL monitor, and the
//! post is what zeroes MCNT.
//!
//! `histogramRecord.c:282-296`:
//!
//! ```c
//! static void monitor(histogramRecord *prec) {
//!     unsigned short monitor_mask = recGblResetAlarms(prec);
//!     /* post events for count change */
//!     if (prec->mcnt > prec->mdel) {
//!         monitor_mask |= DBE_VALUE | DBE_LOG;
//!         /* reset counts since monitor */
//!         prec->mcnt = 0;
//!     }
//!     if (monitor_mask)
//!         db_post_events(prec, &prec->val, monitor_mask);
//! }
//! ```
//!
//! The port had no gate: an array VAL has no `to_f64`, so the generic deadband
//! answered "always post". MDEL was inert — every process shipped the whole bin
//! array to every subscriber — and MCNT grew without bound instead of meaning
//! "counts since the last posted VAL", which also corrupted the SDEL watchdog's
//! view of the shared counter.
//!
//! softIoc (`bin/linux-x86_64`), `field(MDEL,"3")`, `dbgf HG.MCNT` after every
//! second `dbpf HG.PROC 1`:
//!
//! ```text
//! processes: 0   2   4   6
//! MCNT:      0   2   0   2      <- zeroed by the post on cycle 4
//! ```
//!
//! What is counted here is `db_post_events` CALLS, not monitor deliveries. VAL
//! is an array field, so C queues it by reference and refuses a second entry for
//! the same monitor (`dbEvent.c:794-800`) — an undrained C monitor sees one
//! delivery however many times the record posts, and reads the current bins when
//! it is finally delivered. The port's queue does the same, so a burst of posts
//! shows up as one held entry plus `ncollapse`.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(histogram, "HG") {
    field(SVL,  "2.5")
    field(NELM, "4")
    field(LLIM, "0")
    field(ULIM, "4")
    field(MDEL, "3")
}
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

async fn mcnt(db: &PvDatabase, rec: &str) -> f64 {
    db.get_pv(&format!("{rec}.MCNT")).unwrap().to_f64().unwrap()
}

/// The softIoc transcript, value for value: MCNT counts up to MDEL+1, the post
/// zeroes it, and it counts up again.
#[epics_macros_rs::epics_test]
async fn mcnt_is_counts_since_the_last_posted_val() {
    let db = build().await;

    assert_eq!(mcnt(&db, "HG").await, 0.0, "fresh record");

    process(&db, "HG").await;
    process(&db, "HG").await;
    assert_eq!(mcnt(&db, "HG").await, 2.0, "2 counts, MDEL=3 — no post yet");

    process(&db, "HG").await;
    process(&db, "HG").await;
    assert_eq!(
        mcnt(&db, "HG").await,
        0.0,
        "cycle 4: mcnt(4) > mdel(3) — monitor() posts VAL and zeroes MCNT"
    );

    process(&db, "HG").await;
    process(&db, "HG").await;
    assert_eq!(mcnt(&db, "HG").await, 2.0, "and it counts up again from 0");
}

/// The traffic half: MDEL is the histogram's only VAL-monitor rate limiter.
/// Six processes with MDEL=3 produce exactly ONE VAL event (on the 4th); the
/// port produced six.
#[epics_macros_rs::epics_test]
async fn mdel_rate_limits_the_val_monitor() {
    let db = build().await;
    let rec = db.get_record("HG").unwrap();
    let mut val_rx = rec
        .write()
        .add_subscriber("VAL", 1, DbFieldType::ULong, EventMask::VALUE.bits())
        .expect("VAL subscription accepted");

    for _ in 0..6 {
        process(&db, "HG").await;
    }

    let collapsed = val_rx.queue().ncollapse(1);
    let mut events = Vec::new();
    while let Ok(ev) = val_rx.try_recv() {
        events.push(ev);
    }
    assert_eq!(
        events.len() as u64 + collapsed,
        1,
        "MDEL=3 over 6 processes posts VAL once (on the cycle where mcnt=4 > 3)"
    );
    assert_eq!(
        events[0].snapshot.value,
        EpicsValue::ULongArray(vec![0, 0, 4, 0]),
        "the posted VAL carries the four counts binned so far"
    );
}

/// MDEL=0 (the dbd default) posts every cycle: the first count already makes
/// `mcnt (1) > mdel (0)`. The gate must not suppress a default-configured
/// histogram.
#[epics_macros_rs::epics_test]
async fn the_default_mdel_of_zero_posts_every_process() {
    let db = IocBuilder::new()
        .db_string(
            r#"record(histogram, "HG0") {
                 field(SVL,"2.5") field(NELM,"4") field(LLIM,"0") field(ULIM,"4")
               }"#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    let rec = db.get_record("HG0").unwrap();
    let mut val_rx = rec
        .write()
        .add_subscriber("VAL", 1, DbFieldType::ULong, EventMask::VALUE.bits())
        .expect("VAL subscription accepted");

    for _ in 0..3 {
        process(&db, "HG0").await;
    }

    let collapsed = val_rx.queue().ncollapse(1);
    let mut n = 0u64;
    while val_rx.try_recv().is_ok() {
        n += 1;
    }
    assert_eq!(
        n + collapsed,
        3,
        "MDEL=0: every process posts (the array monitor holds one entry and \
         counts the rest as collapses, exactly as C's by-reference queue does)"
    );
    assert_eq!(mcnt(&db, "HG0").await, 0.0, "each post zeroes MCNT");
}
