//! `06 M4`: ao's `omod` flag and the forced `DBE_VALUE|DBE_LOG` on the
//! secondary fields.
//!
//! C `aoRecord.c:520-551` at `R7.0.10`:
//!
//! ```c
//! static void monitor(aoRecord *prec)
//! {
//!     unsigned monitor_mask = recGblResetAlarms(prec);
//!     recGblCheckDeadband(&prec->mlst, prec->val, prec->mdel, &monitor_mask, DBE_VALUE);
//!     recGblCheckDeadband(&prec->alst, prec->val, prec->adel, &monitor_mask, DBE_ARCHIVE);
//!     if (monitor_mask){
//!         db_post_events(prec,&prec->val,monitor_mask);
//!     }
//!     if(prec->omod) monitor_mask |= (DBE_VALUE|DBE_LOG);
//!     if(monitor_mask) {
//!         prec->omod = FALSE;
//!         db_post_events(prec,&prec->oval,monitor_mask);
//!         if(prec->oraw != prec->rval) {
//!             db_post_events(prec,&prec->rval, monitor_mask|DBE_VALUE|DBE_LOG);
//!             prec->oraw = prec->rval;
//!         }
//!         if(prec->orbv != prec->rbv) {
//!             db_post_events(prec,&prec->rbv, monitor_mask|DBE_VALUE|DBE_LOG);
//!             prec->orbv = prec->rbv;
//!         }
//!     }
//! }
//! ```
//!
//! `omod` is set in `convert` (`:482`): `prec->omod = (prec->oval != value)`,
//! where `value` is the OROC rate-limited output. So an output that is still
//! ramping forces a `DBE_VALUE|DBE_LOG` post of OVAL (and of RVAL/RBV when they
//! moved) on EVERY cycle, whatever VAL's own MDEL/ADEL deadbands decided.
//!
//! Two things the port was missing: the flag itself, and `oraw`'s ownership —
//! `convert()` assigned `self.oraw = self.rval` (`ao.rs:266`), which is the
//! very comparison `monitor()` needs, so `oraw != rval` could never be true.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// An ao that ramps: `OROC=1` limits the output to one unit per cycle, and a
/// wide `MDEL`/`ADEL` suppresses every VAL post, so the only thing that can
/// reach a subscriber is the forced mask C's `omod` installs.
async fn ramping_ao(db: &PvDatabase) {
    let mut a = AoRecord::new(0.0);
    a.put_field("OROC", EpicsValue::Double(1.0)).unwrap();
    a.put_field("MDEL", EpicsValue::Double(1000.0)).unwrap();
    a.put_field("ADEL", EpicsValue::Double(1000.0)).unwrap();
    db.add_record("AO", Box::new(a)).await.unwrap();
}

fn subscribe(db: &PvDatabase, field: &str, sid: u32, mask: EventMask) -> EventReader {
    let inst = db.get_record("AO").unwrap();
    let mut g = inst.write();
    g.add_subscriber(field, sid, DbFieldType::Double, mask.bits())
        .expect("subscription accepted")
}

fn read(db: &PvDatabase, field: &str) -> EpicsValue {
    let inst = db.get_record("AO").unwrap();
    let g = inst.read();
    g.resolve_field(field)
        .unwrap_or_else(|| panic!("AO.{field} resolves"))
}

fn drain(rx: &mut EventReader) -> Vec<EventMask> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev.mask);
    }
    out
}

/// A ramping output posts OVAL every cycle with `DBE_VALUE|DBE_LOG`, even
/// though VAL's deadbands suppressed the VAL post entirely.
#[epics_macros_rs::epics_test]
async fn a_ramping_oval_posts_with_value_and_log() {
    let db = PvDatabase::new();
    ramping_ao(&db).await;
    let mut oval_rx = subscribe(&db, "OVAL", 1, EventMask::VALUE | EventMask::LOG);
    let mut val_rx = subscribe(&db, "VAL", 2, EventMask::VALUE | EventMask::LOG);

    db.put_pv("AO", EpicsValue::Double(10.0)).await.unwrap();
    process(&db, "AO").await;

    let val_posts = drain(&mut val_rx);
    let oval_posts = drain(&mut oval_rx);
    assert!(
        val_posts.is_empty(),
        "MDEL/ADEL of 1000 suppress the VAL post entirely, so anything that \
         reaches a subscriber came from omod: {val_posts:?}"
    );
    assert!(
        !oval_posts.is_empty(),
        "omod forces DBE_VALUE|DBE_LOG onto the OVAL post"
    );
    assert!(
        oval_posts[0].contains(EventMask::VALUE | EventMask::LOG),
        "the forced mask is DBE_VALUE|DBE_LOG, got {:?}",
        oval_posts[0]
    );
}

/// RVAL moves with the ramp, so C posts it too — with `monitor_mask` OR-ed
/// with a further literal `DBE_VALUE|DBE_LOG`.
#[epics_macros_rs::epics_test]
async fn a_ramping_rval_posts_with_value_and_log() {
    let db = PvDatabase::new();
    ramping_ao(&db).await;
    let mut rval_rx = subscribe(&db, "RVAL", 1, EventMask::VALUE | EventMask::LOG);

    db.put_pv("AO", EpicsValue::Double(10.0)).await.unwrap();
    process(&db, "AO").await;

    let posts = drain(&mut rval_rx);
    assert!(!posts.is_empty(), "oraw != rval, so C posts RVAL");
    assert!(posts[0].contains(EventMask::VALUE | EventMask::LOG));
}

/// The gap. On an alarm-transition cycle `monitor_mask` is the alarm bits
/// alone, `omod` is false (the output did not move), and C still posts OVAL —
/// its post sits INSIDE `if (monitor_mask)` with no test of OVAL's own value.
/// A `DBE_ALARM`-only subscriber on OVAL therefore observes the alarm moment
/// in C. The port's aux path is change-detected, so an unchanged OVAL reached
/// nobody.
#[epics_macros_rs::epics_test]
async fn an_alarm_cycle_posts_an_unchanged_oval() {
    let db = PvDatabase::new();
    let mut a = AoRecord::new(0.0);
    a.put_field("VAL", EpicsValue::Double(5.0)).unwrap();
    db.add_record("AO", Box::new(a)).await.unwrap();
    // HIGH/HSV are dbCommon, so they are set through the instance — the same
    // place a `.db`'s `field(HIGH,"1")` lands. VAL=5 is above HIGH=1, so the
    // record alarms MINOR from its first cycle onward without VAL ever moving.
    {
        let rec = db.get_record("AO").unwrap();
        let mut inst = rec.write();
        inst.put_common_field("HIGH", EpicsValue::Double(1.0))
            .unwrap();
        inst.put_common_field("HSV", EpicsValue::Short(1)).unwrap();
    }
    process(&db, "AO").await;

    // Second cycle: VAL and OVAL are settled, the alarm transition is what
    // moves. Subscribe AFTER the first cycle so the change post is not what we
    // are measuring.
    let mut oval_rx = subscribe(&db, "OVAL", 1, EventMask::ALARM);
    let mut val_dbg = subscribe(&db, "VAL", 2, EventMask::ALARM);
    {
        let rec = db.get_record("AO").unwrap();
        let mut inst = rec.write();
        inst.common.sevr = epics_base_rs::server::record::AlarmSeverity::NoAlarm;
        inst.common.stat = 0;
    }
    process(&db, "AO").await;

    let posts = drain(&mut oval_rx);
    // VAL is posted on this cycle too (C's own `if (monitor_mask)` VAL post);
    // the point of the case is that OVAL is reached as well, unchanged.
    assert!(
        !drain(&mut val_dbg).is_empty(),
        "the cycle really is an alarm-transition cycle"
    );
    assert!(
        !posts.is_empty(),
        "C posts OVAL from inside `if (monitor_mask)` with no test of its own \
         value, so an alarm-only cycle reaches a DBE_ALARM subscriber"
    );
    assert!(
        posts[0].contains(EventMask::ALARM),
        "the mask is monitor_mask — the alarm bits — got {:?}",
        posts[0]
    );
}

/// The `oraw` ownership half, measured live against C `softIoc`
/// `R7.0.10-146`: `caput AO1 5`, then `caput AO1.ASLO 2`, then
/// `caput AO1.PROC 1`.
///
/// C ends at `VAL=5 RVAL=3 ORAW=5` and emits NOTHING on the second cycle:
/// VAL did not move, no alarm moved, and the output did not move, so
/// `monitor_mask` is 0, the whole `if(monitor_mask)` block at
/// `aoRecord.c:536` is skipped, and `prec->oraw = prec->rval` at `:542` — the
/// line INSIDE the post — never runs. ORAW stays at 5 while RVAL is 3, and
/// the next cycle that does open the guard is the one that posts RVAL.
///
/// The port ended at `VAL=5 RVAL=3 ORAW=3` and emitted `AO1.RVAL 3`: it
/// change-detected RVAL with no guard at all, and `convert()` assigned
/// `oraw = rval` eagerly, so C's own `oraw != rval` test could never be true.
#[epics_macros_rs::epics_test]
async fn a_conversion_field_put_between_cycles_posts_nothing_and_leaves_oraw_stale() {
    let db = PvDatabase::new();
    db.add_record("AO", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    db.put_pv("AO", EpicsValue::Double(5.0)).await.unwrap();
    process(&db, "AO").await;
    assert_eq!(read(&db, "RVAL"), EpicsValue::Long(5));
    assert_eq!(
        read(&db, "ORAW"),
        EpicsValue::Long(5),
        "the first cycle DOES open the guard (VAL moved), so C posts RVAL and \
         advances ORAW with it"
    );

    // Subscribe after the settled cycle: what we measure is the SECOND cycle.
    let mut rval_rx = subscribe(&db, "RVAL", 1, EventMask::VALUE | EventMask::LOG);

    // A client `caput AO.ASLO 2` — a conversion field, no process of its own.
    db.put_pv_no_process("AO.ASLO", EpicsValue::Double(2.0))
        .await
        .unwrap();
    // ...then `caput AO.PROC 1`. VAL is unchanged, so VAL's monitor mask is
    // empty; OVAL is unchanged, so `omod` is false; nothing alarms.
    process(&db, "AO").await;

    assert_eq!(
        read(&db, "RVAL"),
        EpicsValue::Long(3),
        "convert() still runs — 5/ASLO=2 rounds to 3"
    );
    assert!(
        drain(&mut rval_rx).is_empty(),
        "C's RVAL post sits inside `if (monitor_mask)`, and this cycle's \
         monitor_mask is 0"
    );
    assert_eq!(
        read(&db, "ORAW"),
        EpicsValue::Long(5),
        "C advances oraw only from the line after the post it guards, so an \
         unposted RVAL move must leave ORAW stale"
    );

    // The debt is paid by the next cycle that DOES open the guard: move VAL
    // far enough that the new RVAL differs from the STALE ORAW=5 (20/2 = 10),
    // and C posts RVAL once, with the forced DBE_VALUE|DBE_LOG. A VAL that
    // happened to reconvert back to 5 would post nothing, because C's guard
    // is `oraw != rval` and 5 == 5.
    db.put_pv("AO", EpicsValue::Double(20.0)).await.unwrap();
    process(&db, "AO").await;
    let posts = drain(&mut rval_rx);
    assert_eq!(
        posts.len(),
        1,
        "one RVAL event, on the cycle whose guard opened: {posts:?}"
    );
    assert!(posts[0].contains(EventMask::VALUE | EventMask::LOG));
    assert_eq!(read(&db, "ORAW"), read(&db, "RVAL"));
}

/// C never calls `db_post_events` on `oraw` or `orbv` — they are the
/// bookkeeping the RVAL/RBV posts compare against, and `aoRecord.c` contains
/// no post of either. The port change-detected them like any other auxiliary
/// field, so `caput AO 5` emitted an `AO.ORAW` event C does not send (measured
/// on the same run: the port's camonitor printed `AO1.ORAW 5`, C's printed
/// nothing).
#[epics_macros_rs::epics_test]
async fn a_processing_cycle_never_posts_oraw() {
    let db = PvDatabase::new();
    db.add_record("AO", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let mut oraw_rx = subscribe(&db, "ORAW", 1, EventMask::VALUE | EventMask::LOG);
    let mut rval_rx = subscribe(&db, "RVAL", 2, EventMask::VALUE | EventMask::LOG);

    db.put_pv("AO", EpicsValue::Double(5.0)).await.unwrap();
    process(&db, "AO").await;

    assert!(
        !drain(&mut rval_rx).is_empty(),
        "the cycle really did post RVAL"
    );
    assert!(
        drain(&mut oraw_rx).is_empty(),
        "no C `monitor()` posts oraw — the events C sends on that cycle are \
         VAL, OVAL and RVAL"
    );
}
