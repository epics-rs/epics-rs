//! R18-96: stringin/stringout gate the VAL monitor on C's `strncmp(oval, val)`
//! plus MPST/APST, not on the analog MDEL/ADEL deadband.
//!
//! C `stringinRecord.c::monitor` (:176-188), `stringoutRecord.c::monitor`
//! (:211-227) — identical bodies:
//!
//! ```c
//! int monitor_mask = recGblResetAlarms(prec);
//! if (strncmp(prec->oval, prec->val, sizeof(prec->val))) {
//!     monitor_mask |= DBE_VALUE | DBE_LOG;
//!     strncpy(prec->oval, prec->val, sizeof(prec->oval));
//! }
//! if (prec->mpst == stringinPOST_Always) monitor_mask |= DBE_VALUE;
//! if (prec->apst == stringinPOST_Always) monitor_mask |= DBE_LOG;
//! if (monitor_mask) db_post_events(prec, prec->val, monitor_mask);
//! ```
//!
//! The port copied VAL→OVAL unconditionally in `process()` and left
//! `uses_monitor_deadband()` at its default `true`, so the string fell into the
//! analog deadband owner — whose `to_f64()` is `None` for a string, and whose
//! `None` arm posts EVERYTHING. MPST/APST were dead fields.
//!
//! Oracle — softIoc 7.0.10.1-DEV, three stringins at `SCAN="1 second"`, VAL
//! never written, `camonitor T:SI T:SIA T:SIN` for 6 s:
//!
//! ```text
//! T:SI   2026-07-13 21:48:12.525536 hello     <- connect update only
//! T:SIA  2026-07-13 21:48:12.525543 hello
//! T:SIN  2026-07-13 21:48:12.525545 12        <- connect update only
//! T:SIA  2026-07-13 21:48:13.525530 hello     <- MPST="Always": one per cycle
//! T:SIA  2026-07-13 21:48:14.525531 hello
//! T:SIA  2026-07-13 21:48:15.525521 hello
//! T:SIA  2026-07-13 21:48:16.525526 hello
//! T:SIA  2026-07-13 21:48:17.525522 hello
//! ```
//!
//! `T:SI` (MPST/APST = On Change) posts NOTHING across six process cycles;
//! `T:SIN`, whose VAL is the numeric-looking `"12"`, likewise posts nothing —
//! C compares strings, never a `to_f64()` deadband. The port emitted one VAL
//! event per subscriber per cycle for all three.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::server::records::stringout::StringoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// MPST/APST = Always (`menu(stringinPOST)` value 1).
const ALWAYS: i16 = 1;

// ---------------------------------------------------------------------------
// The gate itself, at the record boundary the framework consumes.
// ---------------------------------------------------------------------------

#[test]
fn stringin_monitor_gate_only_on_change() {
    let mut rec = StringinRecord::new("hello");

    rec.process().unwrap();
    assert_eq!(
        rec.monitor_value_changed(),
        Some(true),
        "first cycle: OVAL was empty, VAL is \"hello\" — strncmp differs"
    );
    rec.process().unwrap();
    assert_eq!(
        rec.monitor_value_changed(),
        Some(false),
        "second cycle: OVAL == VAL — C posts nothing"
    );

    rec.put_field("VAL", EpicsValue::String("world".into()))
        .unwrap();
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true), "hello -> world");
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(false), "unchanged again");

    assert!(
        !rec.uses_monitor_deadband(),
        "a string has no MDEL/ADEL deadband: the analog owner must not see it"
    );
}

/// The secondary defect: a numeric-LOOKING string went through the analog
/// deadband, where `"12"` and `"12.0"` compare equal. C's `strncmp` sees a
/// change; `to_f64()` does not.
#[test]
fn stringin_numeric_looking_value_is_compared_as_a_string() {
    let mut rec = StringinRecord::new("12");
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true));
    rec.process().unwrap();
    assert_eq!(
        rec.monitor_value_changed(),
        Some(false),
        "\"12\" unchanged — the oracle's T:SIN posts nothing after connect"
    );

    rec.put_field("VAL", EpicsValue::String("12.0".into()))
        .unwrap();
    rec.process().unwrap();
    assert_eq!(
        rec.monitor_value_changed(),
        Some(true),
        "\"12\" -> \"12.0\" is a strncmp change, whatever the MDEL deadband would say"
    );
}

#[test]
fn stringin_mpst_apst_always_override_the_change_gate() {
    let mut rec = StringinRecord::new("hello");
    assert_eq!(
        rec.monitor_always_post(),
        (false, false),
        "default MPST/APST are On Change"
    );

    rec.put_field("MPST", EpicsValue::Short(ALWAYS)).unwrap();
    assert_eq!(
        rec.monitor_always_post(),
        (true, false),
        "MPST=Always OR-adds DBE_VALUE on an unchanged cycle"
    );
    rec.put_field("APST", EpicsValue::Short(ALWAYS)).unwrap();
    assert_eq!(
        rec.monitor_always_post(),
        (true, true),
        "APST=Always OR-adds DBE_LOG"
    );
}

#[test]
fn stringout_monitor_gate_only_on_change() {
    let mut rec = StringoutRecord::new("hello");

    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true));
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(false));

    rec.put_field("VAL", EpicsValue::String("world".into()))
        .unwrap();
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true));

    assert!(!rec.uses_monitor_deadband());
    rec.put_field("MPST", EpicsValue::Short(ALWAYS)).unwrap();
    assert_eq!(rec.monitor_always_post(), (true, false));
}

// ---------------------------------------------------------------------------
// End to end: what a `camonitor` subscriber actually receives.
// ---------------------------------------------------------------------------

/// Number of VAL events a `camonitor`-like subscriber RECEIVES across six
/// scan cycles.
///
/// The reader is drained after every cycle because that is what the oracle
/// transcript above is: a live `camonitor` whose event task delivers each post
/// before the next scan. It matters for a string field — `dbChannelFieldSize`
/// 40 exceeds `sizeof(union native_value)` 8, so C queues a stringin monitor
/// BY REFERENCE and an undrained one holds a single entry however many posts
/// arrive (`dbEvent.c:493-500`, `:794-800`). Counting an undrained queue would
/// measure the queue's early-drop, not the record's posting, which is what
/// these two cases are about.
async fn cycles_posted(mpst: i16, name: &str) -> usize {
    let db = PvDatabase::new();
    let mut rec = StringinRecord::new("hello");
    rec.put_field("MPST", EpicsValue::Short(mpst)).unwrap();
    db.add_record(name, Box::new(rec)).await.unwrap();

    let mut rx = {
        let inst = db.get_record(name).unwrap();
        let mut inst = inst.write();
        inst.add_subscriber("VAL", 1, DbFieldType::String, EventMask::VALUE.bits())
            .unwrap()
    };

    // Six scan cycles with VAL never written — the oracle's 6 s window.
    let mut events = 0;
    for _ in 0..6 {
        let mut visited = std::collections::HashSet::new();
        db.process_record_with_links(name, &mut visited, 0)
            .await
            .unwrap();
        while rx.try_recv().is_ok() {
            events += 1;
        }
    }
    events
}

/// The whole finding, as the client sees it: six process cycles on an
/// unchanging stringin deliver ZERO VAL monitor events.
#[epics_macros_rs::epics_test]
async fn unchanging_stringin_posts_no_val_events_across_scan_cycles() {
    // The first cycle is a real change (OVAL starts empty, as in C, where
    // init_record leaves oval NUL and the first process posts once).
    let events = cycles_posted(0, "SI:ONCHANGE").await;
    assert_eq!(
        events, 1,
        "MPST=On Change: one event for the initial \"\" -> \"hello\", then silence \
         across five unchanging cycles (the port posted six)"
    );
}

/// And MPST=Always is not a dead field: it posts every cycle, as `T:SIA` does.
#[epics_macros_rs::epics_test]
async fn mpst_always_stringin_posts_every_cycle() {
    let events = cycles_posted(ALWAYS, "SI:ALWAYS").await;
    assert_eq!(
        events, 6,
        "MPST=Always: one VAL event per process cycle, unchanged value or not"
    );
}
