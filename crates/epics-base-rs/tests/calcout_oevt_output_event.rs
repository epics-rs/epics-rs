//! OEVT ("Event To Issue") output-event posting for the calc-output family
//! (`calcout` / `scalcout` / `acalcout`).
//!
//! C `calcoutRecord.c` / `sCalcoutRecord.c` / `aCalcoutRecord.c` `execOutput`
//! posts the OEVT software event immediately after `writeValue`, in every
//! branch that drives OUT and never on IVOA `Don't_drive`. A downstream
//! `SCAN="Event"` / `EVNT="<name>"` record is woken once per output cycle —
//! regardless of whether the OUT link is connected (C posts after the
//! `writeValue`, which is itself a no-op for an unconnected OUT).
//!
//! These tests pin that wiring across the family: the positive case for each
//! record (calcout names the event by string; scalcout/acalcout by number),
//! plus the negative — an INVALID cycle with IVOA=Don't_drive must NOT post,
//! because the OUT write and its event are both suppressed (C execOutput
//! `nsev >= INVALID` → `break`, no `postEvent`).

// RTEMS-EXEC-MODEL-ALLOW(5): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{AlarmSeverity, FieldDesc, ProcessOutcome, Record, ScanType};
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

/// A `SCAN="Event"` sibling that counts how many times it is processed — one
/// increment per `process()`. The OEVT-triggered event must wake it exactly
/// once per output cycle.
struct ProcCounter {
    count: Arc<AtomicUsize>,
}

impl Record for ProcCounter {
    fn record_type(&self) -> &'static str {
        "oevt_proc_counter"
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
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
}

/// Register `name` as a `SCAN="Event"` record on event `evnt`, counting its
/// processes into `counter`.
async fn add_event_sibling(db: &PvDatabase, name: &str, evnt: &str, counter: Arc<AtomicUsize>) {
    db.add_record(name, Box::new(ProcCounter { count: counter }))
        .await
        .unwrap();
    {
        let r = db.get_record(name).unwrap();
        let mut inst = r.write();
        inst.common.scan = ScanType::Event;
        inst.common.evnt = evnt.to_string();
    }
    db.update_scan_index(name, ScanType::Passive, ScanType::Event, 0, 0);
}

/// Poll until `cond` holds — the OEVT post is spawned (like
/// `dispatch_event_record`), so it lands after the triggering process
/// returns — then a short settle window so a spurious second post would also
/// have landed before we assert the exact count.
async fn settle(cond: impl Fn() -> bool) {
    for _ in 0..400 {
        if cond() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("OEVT event did not fire within timeout");
}

/// `calcout`: OOPT=Every_Time + OEVT="e1" wakes a `SCAN="Event"`/`EVNT="e1"`
/// sibling exactly once — with NO OUT link, proving the event posts on the
/// output cycle independent of OUT connection (C posts after `writeValue`).
#[tokio::test]
async fn calcout_oevt_posts_string_event_on_output() {
    let db = PvDatabase::new();
    let count = Arc::new(AtomicUsize::new(0));
    add_event_sibling(&db, "SIB", "e1", count.clone()).await;

    let mut c = CalcoutRecord::default();
    c.put_field("CALC", EpicsValue::String("1".into())).unwrap();
    c.special("CALC", true).unwrap(); // VAL=1, finite
    c.put_field("OEVT", EpicsValue::String("e1".into()))
        .unwrap();
    // oopt default 0 = Every_Time → output is due. No OUT link configured.
    db.add_record("CALC_OEVT", Box::new(c)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("CALC_OEVT", &mut v, 0)
        .await
        .unwrap();
    settle(|| count.load(Ordering::SeqCst) >= 1).await;

    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "calcout OOPT=Every + OEVT=\"e1\" must wake the EVNT=\"e1\" sibling exactly once"
    );
}

/// `calcout`: an INVALID cycle (CALC="0/0" → NaN → UDF) with IVOA=Don't_drive
/// must NOT post OEVT — the OUT write and its event are both suppressed
/// (C execOutput `nsev >= INVALID` → `break`, no `postEvent`).
#[tokio::test]
async fn calcout_oevt_suppressed_on_dont_drive_invalid() {
    let db = PvDatabase::new();
    let count = Arc::new(AtomicUsize::new(0));
    add_event_sibling(&db, "SIB_DD", "e2", count.clone()).await;

    let mut c = CalcoutRecord::default();
    c.put_field("CALC", EpicsValue::String("0/0".into()))
        .unwrap();
    c.special("CALC", true).unwrap(); // NaN → UDF → INVALID
    c.put_field("IVOA", EpicsValue::Short(1)).unwrap(); // Don't_drive
    c.put_field("OEVT", EpicsValue::String("e2".into()))
        .unwrap();
    db.add_record("CALC_DD", Box::new(c)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("CALC_DD", &mut v, 0)
        .await
        .unwrap();

    // Precondition: INVALID cycle, and output WOULD be due — so only the IVOA
    // gate stands between OEVT and the post.
    {
        let rec = db.get_record("CALC_DD").unwrap();
        let inst = rec.read();
        assert_eq!(
            inst.common.sevr,
            AlarmSeverity::Invalid,
            "CALC=0/0 → NaN → UDF must drive the cycle INVALID"
        );
        assert!(
            inst.record.should_output(),
            "OOPT=Every_Time always requests output"
        );
    }

    // Give any erroneous spawned post time to land, then assert none did.
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "IVOA=Don't_drive on an INVALID cycle must suppress the OEVT post \
         (C execOutput nsev>=INVALID → break)"
    );
}

/// `scalcout`: numeric OEVT=5 wakes a `SCAN="Event"`/`EVNT="5"` sibling once.
#[tokio::test]
async fn scalcout_oevt_posts_numeric_event_on_output() {
    let db = PvDatabase::new();
    let count = Arc::new(AtomicUsize::new(0));
    add_event_sibling(&db, "S_SIB", "5", count.clone()).await;

    let mut s = ScalcoutRecord::default();
    s.put_field("CALC", EpicsValue::String("1".into())).unwrap();
    s.special("CALC", true).unwrap();
    s.put_field("OEVT", EpicsValue::UShort(5)).unwrap();
    s.oopt = 0; // Every_Time → output is due.
    db.add_record("S_OEVT", Box::new(s)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("S_OEVT", &mut v, 0)
        .await
        .unwrap();
    settle(|| count.load(Ordering::SeqCst) >= 1).await;

    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "scalcout OEVT=5 must wake the EVNT=\"5\" sibling exactly once (numeric event)"
    );
}

/// `acalcout`: numeric OEVT=7 wakes a `SCAN="Event"`/`EVNT="7"` sibling once.
#[tokio::test]
async fn acalcout_oevt_posts_numeric_event_on_output() {
    let db = PvDatabase::new();
    let count = Arc::new(AtomicUsize::new(0));
    add_event_sibling(&db, "A_SIB", "7", count.clone()).await;

    let mut a = AcalcoutRecord::default();
    a.put_field("CALC", EpicsValue::String("1".into())).unwrap();
    a.special("CALC", true).unwrap();
    a.put_field("OEVT", EpicsValue::UShort(7)).unwrap();
    a.put_field("OOPT", EpicsValue::Short(0)).unwrap(); // Every_Time
    db.add_record("A_OEVT", Box::new(a)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("A_OEVT", &mut v, 0)
        .await
        .unwrap();
    settle(|| count.load(Ordering::SeqCst) >= 1).await;

    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "acalcout OEVT=7 must wake the EVNT=\"7\" sibling exactly once (numeric event)"
    );
}

/// `swait`: numeric OEVT=3 wakes a `SCAN="Event"`/`EVNT="3"` sibling once.
/// swait has no IVOA field, so OEVT posts whenever output fires — C
/// swaitRecord.c:797 posts unconditionally after the OUT write / forward
/// link, the 4th member of the OEVT-output family.
#[tokio::test]
async fn swait_oevt_posts_numeric_event_on_output() {
    let db = PvDatabase::new();
    let count = Arc::new(AtomicUsize::new(0));
    add_event_sibling(&db, "W_SIB", "3", count.clone()).await;

    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("1".into())).unwrap();
    w.special("CALC", true).unwrap();
    w.put_field("OEVT", EpicsValue::UShort(3)).unwrap();
    w.put_field("OOPT", EpicsValue::Short(0)).unwrap(); // Every_Time
    db.add_record("W_OEVT", Box::new(w)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("W_OEVT", &mut v, 0)
        .await
        .unwrap();
    settle(|| count.load(Ordering::SeqCst) >= 1).await;

    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "swait OEVT=3 must wake the EVNT=\"3\" sibling exactly once (numeric event)"
    );
}
