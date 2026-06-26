//! swait ODLY ("Output Execute Delay") defers the OUT-link write and the OEVT
//! software-event post to the delayed (watchdog) cycle — C
//! `swaitRecord.c::schedOutput` (lines 719-729): when `odly > 0` the OUT write,
//! forward link, and OEVT post are scheduled `odly` seconds later via the
//! watchdog, with the record held active (PACT=1); when `odly == 0` they fire
//! immediately. The Rust port models this with a bare `AsyncPending` (which
//! holds PACT, matching C's "RECORD REMAINS ACTIVE") plus `ReprocessAfter`
//! defer on the delaying cycle and an `output_wait` continuation branch that
//! emits exactly once.
//!
//! This test pins the framework-observable effect: on the delaying cycle the
//! OUT target keeps its seed and the OEVT event does NOT fire; on the
//! continuation the target is driven to OVAL and the event posts once. The
//! continuation is driven directly (`process_record_continuation`, as the
//! scalcout ODLY test does) so the assertion is deterministic and does not
//! race the real timer (ODLY=100s makes the timer unfireable here).

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record, ScanType};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

/// A `SCAN="Event"` sibling counting one process per OEVT fire.
struct ProcCounter {
    count: Arc<AtomicUsize>,
}

impl Record for ProcCounter {
    fn record_type(&self) -> &'static str {
        "swait_odly_proc_counter"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(0.0)),
            _ => None,
        }
    }
    fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
        Ok(())
    }
    fn field_list(&self) -> &'static [FieldDesc] {
        &[]
    }
}

async fn add_event_sibling(db: &PvDatabase, name: &str, evnt: &str, counter: Arc<AtomicUsize>) {
    db.add_record(name, Box::new(ProcCounter { count: counter }))
        .await
        .unwrap();
    {
        let r = db.get_record(name).await.unwrap();
        let mut inst = r.write().await;
        inst.common.scan = ScanType::Event;
        inst.common.evnt = evnt.to_string();
    }
    db.update_scan_index(name, ScanType::Passive, ScanType::Event, 0, 0)
        .await;
}

/// ODLY>0: the delaying cycle defers, the continuation drives OUT to OVAL and
/// posts OEVT exactly once.
#[tokio::test]
async fn swait_odly_defers_out_write_and_oevt_to_continuation() {
    let db = PvDatabase::new();
    let count = Arc::new(AtomicUsize::new(0));
    add_event_sibling(&db, "W_SIB", "9", count.clone()).await;

    // OUT target seeded 0.0 — must not be driven to OVAL while deferred.
    db.add_record("W_TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    // swait: CALC="A", A=42 → VAL=42=OVAL (DOPT=0); OOPT=Every → output due;
    // ODLY=100 (real timer cannot fire within the test); OUT→W_TGT; OEVT=9.
    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    w.put_field("A", EpicsValue::Double(42.0)).unwrap();
    w.put_field("OOPT", EpicsValue::Short(0)).unwrap();
    w.put_field("ODLY", EpicsValue::Float(100.0)).unwrap();
    w.put_field("OEVT", EpicsValue::UShort(9)).unwrap();
    db.add_record("W_ODLY", Box::new(w)).await.unwrap();
    // OUT/OUTN route through RecordInstance::put_common_field (populating
    // parsed_out for output dispatch), not the record's put_field.
    {
        let r = db.get_record("W_ODLY").await.unwrap();
        let mut inst = r.write().await;
        inst.put_common_field("OUT", EpicsValue::String("W_TGT".into()))
            .unwrap();
    }

    // Delaying cycle: ODLY>0 defers. OUT not written, OEVT not posted.
    let mut v1 = HashSet::new();
    db.process_record_with_links("W_ODLY", &mut v1, 0)
        .await
        .unwrap();
    // Give any erroneous spawned OEVT post time to land before asserting none.
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    assert_eq!(
        db.get_pv("W_TGT").await.unwrap().to_f64(),
        Some(0.0),
        "ODLY>0 delaying cycle must NOT write OUT (deferred to the watchdog)"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "ODLY>0 delaying cycle must NOT post OEVT (deferred with the OUT write)"
    );

    // Continuation (delayed watchdog cycle): OUT driven to OVAL=42, OEVT posts.
    let mut v2 = HashSet::new();
    db.process_record_continuation("W_ODLY", &mut v2, 0)
        .await
        .unwrap();
    // OEVT post is spawned (like dispatch_event_record) — poll then settle.
    for _ in 0..400 {
        if count.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(
        db.get_pv("W_TGT").await.unwrap().to_f64(),
        Some(42.0),
        "continuation must drive OUT to OVAL=42 after the ODLY delay"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "continuation must post OEVT exactly once after the ODLY delay"
    );
}

/// PACT is held during the ODLY delay (bare `AsyncPending`, not
/// `AsyncPendingNotify`): a foreign `dbProcess` inside the delay window BAILS at
/// the PACT entry guard (C `swaitRecord.c:716` "THE RECORD REMAINS ACTIVE WHILE
/// WAITING ON THE WATCHDOG") instead of re-entering the `output_wait`
/// continuation and firing the deferred OUT / OEVT early. Without the PACT hold
/// the foreign process drives the target to OVAL before the delay elapses.
#[tokio::test]
async fn swait_odly_holds_pact_foreign_process_does_not_fire_early() {
    let db = PvDatabase::new();
    let count = Arc::new(AtomicUsize::new(0));
    add_event_sibling(&db, "W3_SIB", "13", count.clone()).await;

    db.add_record("W3_TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    w.put_field("A", EpicsValue::Double(42.0)).unwrap();
    w.put_field("OOPT", EpicsValue::Short(0)).unwrap();
    w.put_field("ODLY", EpicsValue::Float(100.0)).unwrap();
    w.put_field("OEVT", EpicsValue::UShort(13)).unwrap();
    db.add_record("W3_ODLY", Box::new(w)).await.unwrap();
    {
        let r = db.get_record("W3_ODLY").await.unwrap();
        let mut inst = r.write().await;
        inst.put_common_field("OUT", EpicsValue::String("W3_TGT".into()))
            .unwrap();
    }

    // Delaying cycle: PACT held, output deferred.
    let mut v1 = HashSet::new();
    db.process_record_with_links("W3_ODLY", &mut v1, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("W3_TGT").await.unwrap().to_f64(),
        Some(0.0),
        "output deferred on the delaying cycle"
    );

    // Foreign dbProcess DURING the delay (is_continuation=false): must bail at
    // the PACT entry guard, NOT fire the deferred output early.
    let mut v2 = HashSet::new();
    db.process_record_with_links("W3_ODLY", &mut v2, 0)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    assert_eq!(
        db.get_pv("W3_TGT").await.unwrap().to_f64(),
        Some(0.0),
        "PACT held: a foreign dbProcess during the ODLY delay must NOT fire the \
         deferred OUT early (it bails at the entry guard, as C does)"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "PACT held: a foreign dbProcess must NOT post OEVT early"
    );

    // Continuation (bypasses the PACT guard): fires the deferred output once.
    let mut v3 = HashSet::new();
    db.process_record_continuation("W3_ODLY", &mut v3, 0)
        .await
        .unwrap();
    for _ in 0..400 {
        if count.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(
        db.get_pv("W3_TGT").await.unwrap().to_f64(),
        Some(42.0),
        "continuation drives OUT to OVAL=42 after the delay"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "continuation posts OEVT exactly once"
    );
}

/// ODLY=0: OUT write and OEVT both fire synchronously on the single cycle —
/// the regression guard that the defer is gated strictly on `odly > 0`.
#[tokio::test]
async fn swait_no_odly_writes_out_and_posts_oevt_synchronously() {
    let db = PvDatabase::new();
    let count = Arc::new(AtomicUsize::new(0));
    add_event_sibling(&db, "W2_SIB", "11", count.clone()).await;

    db.add_record("W2_TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    w.put_field("A", EpicsValue::Double(7.0)).unwrap();
    w.put_field("OOPT", EpicsValue::Short(0)).unwrap();
    // ODLY default 0 → synchronous.
    w.put_field("OEVT", EpicsValue::UShort(11)).unwrap();
    db.add_record("W2_ODLY", Box::new(w)).await.unwrap();
    {
        let r = db.get_record("W2_ODLY").await.unwrap();
        let mut inst = r.write().await;
        inst.put_common_field("OUT", EpicsValue::String("W2_TGT".into()))
            .unwrap();
    }

    let mut v = HashSet::new();
    db.process_record_with_links("W2_ODLY", &mut v, 0)
        .await
        .unwrap();
    for _ in 0..400 {
        if count.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(
        db.get_pv("W2_TGT").await.unwrap().to_f64(),
        Some(7.0),
        "ODLY=0: OUT written to OVAL=7 on the same cycle"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "ODLY=0: OEVT posts once on the same cycle"
    );
}
