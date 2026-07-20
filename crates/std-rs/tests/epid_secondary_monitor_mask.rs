//! R11-64 — epid's secondary fields post `DBE_VALUE|DBE_LOG` with the alarm
//! bits DISCARDED.
//!
//! C `epidRecord.c:345-408` `monitor()`:
//!
//! ```c
//! monitor_mask = recGblResetAlarms(pepid);   /* the cycle's alarm bits */
//! ... MDEL -> |= DBE_VALUE ... ADEL -> |= DBE_LOG ...
//! if (monitor_mask) db_post_events(pepid, &pepid->val, monitor_mask);
//!
//! monitor_mask = DBE_LOG|DBE_VALUE;          /* :376 — REASSIGNED, not |= */
//! if (pepid->ovlp != pepid->oval) db_post_events(pepid, &pepid->oval, monitor_mask);
//! ...  /* P, I, D, CT, DT, ERR, CVAL — same literal mask */
//! ```
//!
//! VAL keeps the alarm bits; every secondary after line 376 does not. A
//! `DBE_ALARM`-only subscriber on `.OVAL`/`.P`/`.ERR`/... is notified on no
//! cycle at all, and a `DBE_VALUE|DBE_LOG` subscriber sees a mask with no
//! `DBE_ALARM` in it even on the cycle the record went into alarm.
//!
//! The port had no way to say that: `value_only_change_fields` strips `DBE_LOG`
//! and `fields_posted_with_monitor_mask` inherits VAL's deadband mask — both
//! keep the alarm bits. The generic change loop therefore posted every epid
//! secondary with `alarm_bits | DBE_VALUE | DBE_LOG`. The fix is a third
//! declaration, `Record::fields_posted_without_alarm_bits`, resolved by the same
//! single owner (`AuxPostMask`) as the other two.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{AlarmSeverity, EventMask, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use std_rs::EpidRecord;

/// The seven CA-visible secondaries of C's post-:376 list. `CT` is the eighth,
/// but it is `DBF_NOACCESS` (epidRecord.dbd:226) — no client can subscribe to
/// it, and the port has no such field.
const SECONDARIES: &[&str] = &["OVAL", "P", "I", "D", "DT", "ERR", "CVAL"];

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// A closed-loop epid (SMSL=1): the setpoint arrives through STPL, so putting
/// `SP` moves VAL on the NEXT PROCESS cycle — a HIHI crossing is then an alarm
/// transition of the process cycle itself (`recGblResetAlarms` returns
/// `DBE_ALARM` there), which is where `monitor()` runs. Putting VAL directly
/// would process the record inside the put and spend the transition on that
/// cycle instead.
///
/// Feedback is on and KP is non-zero, so ERR/P/OVAL follow the setpoint.
async fn closed_loop_epid() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("SP", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("RBV", Box::new(AiRecord::new(3.0)))
        .await
        .unwrap();

    let mut e = EpidRecord::default();
    for (f, v) in [
        ("SMSL", EpicsValue::Short(1)),
        ("STPL", EpicsValue::String("SP".into())),
        ("INP", EpicsValue::String("RBV".into())),
        ("KP", EpicsValue::Double(2.0)),
        ("MDT", EpicsValue::Double(0.0)),
        ("DRVH", EpicsValue::Double(200.0)),
        ("DRVL", EpicsValue::Double(-200.0)),
        ("HIHI", EpicsValue::Double(5.0)),
        ("HHSV", EpicsValue::Short(AlarmSeverity::Major as i16)),
        ("FBON", EpicsValue::Short(1)),
    ] {
        e.put_field(f, v).unwrap();
    }
    db.add_record("PID", Box::new(e)).await.unwrap();
    db
}

async fn set(db: &PvDatabase, rec: &str, v: f64) {
    db.put_record_field_from_ca(rec, "VAL", EpicsValue::Double(v))
        .await
        .unwrap();
}

/// The finding. On the alarm-transition cycle every secondary posts with a
/// LITERAL `DBE_VALUE|DBE_LOG` — no `DBE_ALARM`, even though the cycle raised
/// one. The negative control is VAL, whose own post (`epidRecord.c:371`) keeps
/// the alarm bits.
#[tokio::test]
async fn r11_64_epid_secondaries_post_without_the_cycles_alarm_bits() {
    let db = closed_loop_epid().await;
    let inst = db.get_record("PID").unwrap();

    let full = (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits();
    let mut readers = Vec::new();
    {
        let mut g = inst.write();
        for (i, f) in SECONDARIES.iter().enumerate() {
            let r = g
                .add_subscriber(f, i as u32 + 1, DbFieldType::Double, full)
                .expect("a secondary subscription must be accepted");
            readers.push((*f, r));
        }
    }
    let mut val_rx = inst
        .write()
        .add_subscriber("VAL", 100, DbFieldType::Double, full)
        .expect("a VAL subscription must be accepted");
    let mut stat_rx = inst
        .write()
        .add_subscriber("STAT", 101, DbFieldType::Short, EventMask::ALARM.bits())
        .expect("a STAT subscription must be accepted");

    // Warm-up cycle, setpoint 1 (below HIHI): it clears UDF, takes the FBON
    // off->on edge, and gives every field a posted baseline — so the alarm cycle
    // below posts exactly the fields that really changed, as C's per-field
    // `if (ovlp != oval)` gates do.
    process(&db, "PID").await;
    while val_rx.try_recv().is_ok() {}
    while stat_rx.try_recv().is_ok() {}
    for (_, rx) in &mut readers {
        while rx.try_recv().is_ok() {}
    }

    // Setpoint 10 > HIHI=5. STPL carries it into VAL inside the process cycle,
    // so this is the cycle that raises the alarm AND moves the PID error (and
    // with it every derived secondary).
    set(&db, "SP", 10.0).await;
    process(&db, "PID").await;

    // The cycle DID raise an alarm — without this the masks below would be
    // trivially alarm-free and the test would pass on a broken port.
    {
        let g = inst.read();
        assert_eq!(
            g.common.sevr,
            AlarmSeverity::Major,
            "the STPL setpoint 10 is above HIHI=5 with HHSV=MAJOR"
        );
    }
    assert!(
        stat_rx.try_recv().is_ok(),
        "the severity moved, so recGblResetAlarms posted STAT with DBE_ALARM — \
         the cycle's alarm bits are live"
    );
    let mut val_masks = Vec::new();
    while let Ok(e) = val_rx.try_recv() {
        val_masks.push(e.mask);
    }
    assert!(
        val_masks.iter().any(|m| m.contains(EventMask::ALARM)),
        "negative control: VAL's own post (epidRecord.c:371) keeps the cycle's \
         alarm bits — it is posted with `monitor_mask`, not with the literal \
         reassigned at :376. Got {val_masks:?}"
    );

    // ERR = VAL - CVAL moved (-2 -> 7), so P and OVAL moved with it, and DT is
    // re-measured every cycle. CVAL did not move (the readback is still 3), and
    // I/D stay 0 with KI=KD=0 — C's per-field `!=` gates post none of those.
    let changed = ["OVAL", "P", "ERR", "DT"];
    for (field, rx) in &mut readers {
        match rx.try_recv() {
            Ok(e) => {
                assert!(
                    changed.contains(field),
                    "{field} did not change this cycle; C's `if (pepid->cvlp != \
                     pepid->cval)` gate posts nothing"
                );
                assert_eq!(
                    e.mask,
                    EventMask::VALUE | EventMask::LOG,
                    "epidRecord.c:376 reassigns monitor_mask to a literal \
                     DBE_LOG|DBE_VALUE, so {field} carries no alarm bit"
                );
            }
            Err(_) => assert!(
                !changed.contains(field),
                "{field} changed this cycle and must post"
            ),
        }
    }
}

/// The same fact from the subscriber's side: a `DBE_ALARM`-only client on a
/// secondary is notified on no cycle at all — not even the one the record
/// alarmed on.
#[tokio::test]
async fn r11_64_an_alarm_only_subscriber_on_a_secondary_never_fires() {
    let db = closed_loop_epid().await;
    let inst = db.get_record("PID").unwrap();

    let mut alarm_only = Vec::new();
    {
        let mut g = inst.write();
        for (i, f) in SECONDARIES.iter().enumerate() {
            let r = g
                .add_subscriber(
                    f,
                    i as u32 + 1,
                    DbFieldType::Double,
                    EventMask::ALARM.bits(),
                )
                .expect("a secondary subscription must be accepted");
            alarm_only.push((*f, r));
        }
    }

    process(&db, "PID").await; // setpoint 1: no alarm
    set(&db, "SP", 10.0).await;
    process(&db, "PID").await; // NO_ALARM -> MAJOR
    process(&db, "PID").await; // held in MAJOR

    // ...and back out of alarm, the other alarm transition.
    set(&db, "SP", 1.0).await;
    process(&db, "PID").await; // MAJOR -> NO_ALARM

    {
        let g = inst.read();
        assert_eq!(g.common.sevr, AlarmSeverity::NoAlarm);
    }
    for (field, rx) in &mut alarm_only {
        assert!(
            rx.try_recv().is_err(),
            "{field} is posted with a literal DBE_LOG|DBE_VALUE \
             (epidRecord.c:376), so a DBE_ALARM-only subscriber receives \
             nothing on either alarm transition"
        );
    }
}

/// The hook narrows the MASK, it does not suppress the POST: an ordinary
/// `DBE_VALUE` client still sees every secondary change, alarm or no alarm.
#[tokio::test]
async fn r11_64_a_value_subscriber_still_sees_the_secondary_change() {
    let db = closed_loop_epid().await;
    let inst = db.get_record("PID").unwrap();

    let mut oval_rx = inst
        .write()
        .add_subscriber("OVAL", 1, DbFieldType::Double, EventMask::VALUE.bits())
        .expect("an OVAL subscription must be accepted");

    // The setpoint stays at 1, below HIHI: no alarm on any of these cycles.
    process(&db, "PID").await;
    let first = oval_rx.try_recv().expect("OVAL posts its first value");
    assert_eq!(first.mask, EventMask::VALUE | EventMask::LOG);

    // Move the readback: ERR changes, so OVAL changes, so C posts again.
    set(&db, "RBV", -4.0).await;
    process(&db, "PID").await;
    let second = oval_rx.try_recv().expect("a changed OVAL posts again");
    assert_ne!(
        second.snapshot.value, first.snapshot.value,
        "the readback moved, so the PID output moved"
    );
    assert_eq!(second.mask, EventMask::VALUE | EventMask::LOG);
    {
        let g = inst.read();
        assert_eq!(g.common.sevr, AlarmSeverity::NoAlarm, "no alarm was raised");
    }
}
