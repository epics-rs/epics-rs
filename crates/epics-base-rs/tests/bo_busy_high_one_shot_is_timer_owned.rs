//! The `bo`/`busy` HIGH one-shot belongs to its timer, not to the next
//! `process()`.
//!
//! C `boRecord.c:257-262` arms `callbackRequestDelayed` on every process that
//! leaves VAL at 1, but the only writer of the one-shot's `prec->val = 0` is
//! `myCallbackFunc` (:116) — the timer body. `busyRecord.c:258-262` + `:107-124`
//! is the same code. A scan, a caput or a FLNK landing inside the HIGH window
//! therefore re-arms the timer in C and never releases the pulse early.
//!
//! Boundaries: a foreign process inside the window (VAL must survive), the
//! timer's own fire with PACT clear (VAL must drop), the timer's fire with PACT
//! set (C re-arms and changes nothing), and the fire that finds nothing left to
//! do. `busy` carries every one of them because BUSY=1 is what gates a synApps
//! scan step.

use std::collections::HashSet;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DelayedCallbackOutcome, ProcessAction, Record};
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(longin, "T") { }
record(bo, "B") {
    field(HIGH, "5")
    field(OUT, "T.VAL PP")
}
record(longin, "TB") { }
record(busy, "BSY") {
    field(HIGH, "5")
    field(OUT, "TB.VAL PP")
}
"#;

/// The lead's trigger, without the wall clock: `caput B 1` starts a 5 s pulse,
/// then a second process arrives (a 1 s SCAN tick, a FLNK, another caput). C
/// holds B=1/T=1 for the full 5 s; the port used to drop to 0 on that second
/// process, turning a 5 s momentary pulse into one scan period.
#[epics_macros_rs::epics_test]
async fn a_foreign_process_inside_the_high_window_does_not_release_the_pulse() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    // `caput B 1` — the put, then the process it triggers.
    db.put_pv("B", EpicsValue::Enum(1)).await.unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("B", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(db.get_pv("B").unwrap(), EpicsValue::Enum(1));
    assert_eq!(db.get_pv("T").unwrap(), EpicsValue::Long(1));

    // The second process is NOT the timer — it must leave the pulse alone.
    let mut visited = HashSet::new();
    db.process_record_with_links("B", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("B").unwrap(),
        EpicsValue::Enum(1),
        "a foreign process inside the HIGH window must not consume the one-shot"
    );
    assert_eq!(
        db.get_pv("T").unwrap(),
        EpicsValue::Long(1),
        "the momentary output must still read 1"
    );
}

/// Same boundary on `busy`, where an early release ends a scan step that has
/// not finished.
#[epics_macros_rs::epics_test]
async fn a_foreign_process_does_not_clear_a_busy_flag_early() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    db.put_pv("BSY", EpicsValue::Enum(1)).await.unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("BSY", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(db.get_pv("BSY").unwrap(), EpicsValue::Enum(1));

    let mut visited = HashSet::new();
    db.process_record_with_links("BSY", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("BSY").unwrap(),
        EpicsValue::Enum(1),
        "BUSY must stay set for the whole HIGH window"
    );
    assert_eq!(db.get_pv("TB").unwrap(), EpicsValue::Long(1));
}

/// The other half of the invariant: the timer still releases the pulse, and it
/// carries the output back down through OUT.
#[epics_macros_rs::epics_test]
async fn the_timer_releases_the_pulse_and_drives_the_output_back_to_zero() {
    const SHORT: &str = r#"
record(longin, "ST") { }
record(bo, "SB") {
    field(HIGH, "0.15")
    field(OUT, "ST.VAL PP")
}
"#;
    let db = IocBuilder::new()
        .db_string(SHORT, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    db.put_pv("SB", EpicsValue::Enum(1)).await.unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("SB", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(db.get_pv("ST").unwrap(), EpicsValue::Long(1));

    epics_base_rs::runtime::task::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        db.get_pv("SB").unwrap(),
        EpicsValue::Enum(0),
        "the HIGH timer must still drive the momentary bo back to Done"
    );
    assert_eq!(db.get_pv("ST").unwrap(), EpicsValue::Long(0));
}

/// `process()` arms and nothing else: the action it emits is the delayed
/// callback, and the record keeps no cell for a later cycle to consume.
#[test]
fn process_arms_the_delayed_callback_and_leaves_val_alone() {
    for (label, rec) in [
        ("bo", Box::new(BoRecord::new(1)) as Box<dyn Record>),
        ("busy", {
            let mut b = BusyRecord::default();
            b.put_field("VAL", EpicsValue::Enum(1)).unwrap();
            Box::new(b) as Box<dyn Record>
        }),
    ] {
        let mut rec = rec;
        rec.put_field("HIGH", EpicsValue::Double(5.0)).unwrap();
        let outcome = rec.process().unwrap();
        assert!(
            outcome
                .actions
                .iter()
                .any(|a| matches!(a, ProcessAction::DelayedCallbackAfter(_))),
            "{label}: VAL=1 with HIGH>0 must arm the delayed callback"
        );
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::Enum(1)),
            "{label}: arming must not touch VAL"
        );
        // Re-arming is idempotent — C calls `callbackRequestDelayed` again.
        rec.process().unwrap();
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::Enum(1)),
            "{label}: a second process must not release the pulse"
        );
    }
}

/// The three arms of C `myCallbackFunc`, by boundary rather than by story.
#[test]
fn the_timer_body_matches_my_callback_func() {
    for label in ["bo", "busy"] {
        let mut rec: Box<dyn Record> = if label == "bo" {
            Box::new(BoRecord::new(1))
        } else {
            let mut b = BusyRecord::default();
            b.put_field("VAL", EpicsValue::Enum(1)).unwrap();
            Box::new(b)
        };
        rec.put_field("HIGH", EpicsValue::Double(5.0)).unwrap();

        // PACT set, VAL=1, HIGH>0 -> C's `if (prec->pact)` branch: re-arm, no zero.
        assert_eq!(
            rec.delayed_callback_fire(true),
            DelayedCallbackOutcome::Rearm(Duration::from_secs_f64(5.0)),
            "{label}: a PACT-held fire re-arms"
        );
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::Enum(1)),
            "{label}: a PACT-held fire must not zero VAL"
        );

        // PACT clear -> C's else branch: zero and reprocess.
        assert_eq!(
            rec.delayed_callback_fire(false),
            DelayedCallbackOutcome::Reprocess,
            "{label}: a PACT-clear fire reprocesses"
        );
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::Enum(0)),
            "{label}: the timer body is the one-shot's only VAL=0 writer"
        );

        // PACT set with nothing left to release -> neither.
        assert_eq!(
            rec.delayed_callback_fire(true),
            DelayedCallbackOutcome::Drop,
            "{label}: a PACT-held fire on VAL=0 does nothing at all"
        );
    }
}

/// The HIGH boundary that `Duration` cannot represent. HIGH is `DBF_DOUBLE`
/// (`boRecord.dbd.pod`, `busyRecord.dbd.pod`) and C stores the whole range —
/// `caput B.HIGH 1e300` succeeds, and `callbackRequestDelayed(cb, 1e300)`
/// schedules a callback past any run's lifetime rather than failing. The port's
/// re-arm converted that HIGH with `Duration::from_secs_f64`, which PANICS
/// above `u64::MAX` seconds, so a value C accepts aborted the timer thread.
/// Dropping the one-shot is C's observable: the callback never fires either
/// way. The two representable neighbours are pinned beside it so the fix is a
/// boundary and not a blanket drop.
#[test]
fn a_high_beyond_duration_drops_the_one_shot_instead_of_panicking() {
    for label in ["bo", "busy"] {
        let build = |high: f64| -> Box<dyn Record> {
            let mut rec: Box<dyn Record> = if label == "bo" {
                Box::new(BoRecord::new(1))
            } else {
                let mut b = BusyRecord::default();
                b.put_field("VAL", EpicsValue::Enum(1)).unwrap();
                Box::new(b)
            };
            rec.put_field("HIGH", EpicsValue::Double(high)).unwrap();
            rec
        };

        // Representable: still re-arms.
        assert_eq!(
            build(1e18).delayed_callback_fire(true),
            DelayedCallbackOutcome::Rearm(Duration::from_secs_f64(1e18)),
            "{label}: 1e18 s is inside Duration and must still re-arm"
        );
        // Past u64::MAX seconds: no representable deadline.
        assert_eq!(
            build(1e300).delayed_callback_fire(true),
            DelayedCallbackOutcome::Drop,
            "{label}: HIGH=1e300 must drop the one-shot, not panic"
        );
        assert_eq!(
            build(f64::INFINITY).delayed_callback_fire(true),
            DelayedCallbackOutcome::Drop,
            "{label}: HIGH=inf must drop the one-shot, not panic"
        );
    }
}
