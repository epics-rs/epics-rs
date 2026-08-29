//! R11-C5 — an aCalcout array field posts because the expression STORED into it,
//! not because its value changed.
//!
//! `afterCalc` (`aCalcoutRecord.c:294-298`) posts exactly the AMASK-flagged array
//! fields, with no value comparison anywhere in the loop:
//!
//! ```c
//! /* post array fields that aCalcPerform wrote to. */
//! for (j=0, panew=&pcalc->aa; j<ARRAY_MAX_FIELDS; j++, panew++) {
//!     if (*panew && (pcalc->amask & (1<<j))) {
//!         db_post_events(pcalc, *panew, DBE_VALUE|DBE_LOG);
//!     }
//! }
//! ```
//!
//! AMASK is this cycle's store set: `aCalcPerform` zeroes it at entry
//! (`aCalcPerform.c:326`) and sets bit i in `STORE_AA..STORE_LL` (`:487`). So a
//! store that happens to write the value the field already held STILL posts —
//! the case the port's change-detection loop dropped.
//!
//! The C `afterCalc` post is the record telling clients "I ran and I wrote this
//! array this cycle", which is what an aCalcout-driven waveform client waits on;
//! a second identical process must therefore not go silent.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// A 4-element aCalcout with the given CALC and BB seeded.
async fn acalcout(db: &PvDatabase, name: &str, calc: &str) {
    let mut a = AcalcoutRecord::new();
    a.put_field("NELM", EpicsValue::ULong(4)).unwrap();
    a.put_field("CALC", EpicsValue::String(calc.into()))
        .unwrap();
    a.special("CALC", true).unwrap();
    a.put_field("BB", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0]))
        .unwrap();
    db.add_record(name, Box::new(a)).await.unwrap();
}

/// The finding. `AA := BB*2` with BB held fixed writes AA=[2,4,6,8] on EVERY
/// process. The second process changes nothing, so the framework's change
/// detection posted nothing — but AMASK bit 0 is set on that cycle too, and C
/// posts AA from the bit alone.
#[epics_macros_rs::epics_test]
async fn r11_c5_a_stored_array_posts_even_when_the_value_did_not_change() {
    let db = PvDatabase::new();
    acalcout(&db, "P1", "AA:=BB*2;SUM(AA)").await;
    let inst = db.get_record("P1").unwrap();

    let mut aa_rx = inst
        .write()
        .add_subscriber(
            "AA",
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .expect("an AA subscription must be accepted");

    // First process: AA moves 0 -> [2,4,6,8]. Change detection would post this on
    // its own, so it proves nothing; drain it.
    process(&db, "P1").await;
    let first = aa_rx.try_recv().expect("AA moved on the first process");
    assert_eq!(
        first.snapshot.value,
        EpicsValue::DoubleArray(vec![2.0, 4.0, 6.0, 8.0])
    );

    // Second process: identical inputs, identical store, identical value.
    process(&db, "P1").await;

    let second = aa_rx
        .try_recv()
        .expect("AMASK bit 0 is set again, so afterCalc posts AA again (:294-298)");
    assert_eq!(
        second.snapshot.value,
        EpicsValue::DoubleArray(vec![2.0, 4.0, 6.0, 8.0])
    );
    assert_eq!(
        second.mask,
        EventMask::VALUE | EventMask::LOG,
        "C posts the flagged array with a literal DBE_VALUE|DBE_LOG"
    );
}

/// The mask is per-cycle, not sticky: an expression that stores nothing leaves
/// AMASK 0 (`aCalcPerform.c:326` zeroes it at entry), so the write-gated post
/// must stop with it. Without this the fix would post AA forever after one store.
#[epics_macros_rs::epics_test]
async fn r11_c5_an_array_that_was_not_stored_this_cycle_does_not_post() {
    let db = PvDatabase::new();
    acalcout(&db, "P2", "AA:=BB*2;SUM(AA)").await;
    let inst = db.get_record("P2").unwrap();

    let mut aa_rx = inst
        .write()
        .add_subscriber(
            "AA",
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .expect("an AA subscription must be accepted");

    process(&db, "P2").await;
    while aa_rx.try_recv().is_ok() {}

    // Swap in an expression with no store at all. AA keeps its value.
    {
        let mut g = inst.write();
        g.record
            .put_field("CALC", EpicsValue::String("SUM(BB)".into()))
            .unwrap();
        g.record.special("CALC", true).unwrap();
    }
    process(&db, "P2").await;

    assert_eq!(
        inst.read().record.get_field("AMASK").unwrap(),
        EpicsValue::ULong(0),
        "no store this cycle"
    );
    assert!(
        aa_rx.try_recv().is_err(),
        "AMASK is 0, so afterCalc's loop skips AA — an unstored, unchanged array \
         must post nothing"
    );
}

/// Only the flagged arrays post. `CC := AA` sets bit 2; BB (bit 1) is read, not
/// written, and must stay silent.
#[epics_macros_rs::epics_test]
async fn r11_c5_only_the_flagged_arrays_post() {
    let db = PvDatabase::new();
    acalcout(&db, "P3", "CC:=BB;SUM(CC)").await;
    let inst = db.get_record("P3").unwrap();

    let (mut bb_rx, mut cc_rx) = {
        let mut g = inst.write();
        let bb = g
            .add_subscriber(
                "BB",
                1,
                DbFieldType::Double,
                (EventMask::VALUE | EventMask::LOG).bits(),
            )
            .unwrap();
        let cc = g
            .add_subscriber(
                "CC",
                2,
                DbFieldType::Double,
                (EventMask::VALUE | EventMask::LOG).bits(),
            )
            .unwrap();
        (bb, cc)
    };

    process(&db, "P3").await;
    while bb_rx.try_recv().is_ok() {}
    while cc_rx.try_recv().is_ok() {}

    // Second, value-identical cycle.
    process(&db, "P3").await;

    assert_eq!(
        inst.read().record.get_field("AMASK").unwrap(),
        EpicsValue::ULong(0x4),
        "bit 2 = CC"
    );
    assert!(
        cc_rx.try_recv().is_ok(),
        "CC is flagged, so it posts on the unchanged cycle"
    );
    assert!(
        bb_rx.try_recv().is_err(),
        "BB was only READ; its bit is clear and it did not change"
    );
}
