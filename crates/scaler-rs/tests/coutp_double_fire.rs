//! R10-61 — a user stop writes COUTP twice; C has two independent put sites.
//!
//! C `special()` puts to COUTP on every CNT write that clears the redundant-
//! command guard (`scalerRecord.c:623-627`):
//!
//! ```c
//! case scalerRecordCNT:
//!     if (pscal->cnt && (pscal->us != USER_STATE_IDLE)) return(0);
//!     status = dbPutLink(&pscal->coutp, DBR_SHORT, &pscal->cnt, 1);
//! ```
//!
//! and `process()` puts to it AGAIN on the finish edge (`:455-468`):
//!
//! ```c
//! if (justStartedUserCount || justFinishedUserCount) {
//!     status = dbPutLink(&pscal->cout, DBR_SHORT, &pscal->cnt, 1);
//!     if (justFinishedUserCount) {
//!         status = dbPutLink(&pscal->coutp, DBR_SHORT, &pscal->cnt, 1);
//!     }
//! }
//! ```
//!
//! A user stop (`CNT 1->0`) runs both — `special()` on the CA put, then
//! `process()` with `justFinishedUserCount` — so the link is written twice with
//! the same value 0 and a record wired to .COUTP is processed twice. A user start
//! reaches only `special()`'s put, since `:463` is guarded by
//! `justFinishedUserCount`. A preset-completion finish reaches only `process()`'s.
//!
//! The port had one `fire_coutp` bool that both sites raised, so the stop
//! collapsed to a single write.
//!
//! R10-64 moved `special()`'s put onto its own channel
//! (`Record::take_special_actions`), which the framework executes inside the
//! put. `link_writes_for_put` below composes the two channels in C's order —
//! `special()` first (it runs in `dbPut`), then `process()` — which is what the
//! put owner does.

use epics_base_rs::server::record::{ProcessAction, Record};
use scaler_rs::records::scaler::ScalerRecord;

fn armed_record() -> ScalerRecord {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0;
    rec.init_record(1).unwrap();
    rec
}

/// The link fields written, in order.
fn write_fields(actions: &[ProcessAction]) -> Vec<&'static str> {
    actions
        .iter()
        .filter_map(|a| match a {
            ProcessAction::WriteDbLink { link_field, .. } => Some(*link_field),
            _ => None,
        })
        .collect()
}

fn coutp_values(actions: &[ProcessAction]) -> Vec<i16> {
    actions
        .iter()
        .filter_map(|a| match a {
            ProcessAction::WriteDbLink {
                link_field: "COUTP",
                value,
            } => value.to_f64().map(|v| v as i16),
            _ => None,
        })
        .collect()
}

/// One CA put to CNT, in the order the framework performs it: `special()` and
/// the link writes it queued (C runs them inside `dbPut`), then the `pp(TRUE)`
/// process cycle. The returned list is every link write the put made.
fn put_cnt(rec: &mut ScalerRecord, cnt: i16) -> Vec<ProcessAction> {
    rec.cnt = cnt;
    rec.special("CNT", true).unwrap();
    let mut actions = rec.take_special_actions();
    actions.extend(rec.process().unwrap().actions);
    actions
}

/// Start, then stop while counting: the stop writes COUTP twice.
#[test]
fn r10_61_a_user_stop_writes_coutp_twice() {
    let mut rec = armed_record();

    // Start: special() puts to COUTP, process() puts to COUT (justStarted).
    let start = put_cnt(&mut rec, 1);
    assert_eq!(
        write_fields(&start),
        vec!["COUTP", "COUT"],
        "a start reaches special()'s put (:624) and process()'s COUT (:457); the \
         second COUTP put (:463) is guarded by justFinishedUserCount"
    );

    // Stop while counting: special() puts to COUTP, then process() puts to COUT
    // and to COUTP a second time.
    let stop = put_cnt(&mut rec, 0);
    assert_eq!(
        write_fields(&stop),
        vec!["COUTP", "COUT", "COUTP"],
        "special() :624 fires first (it runs inside the put, before the record is \
         processed), then process() :457 COUT, then process() :463 COUTP"
    );
    assert_eq!(
        coutp_values(&stop),
        vec![0, 0],
        "both puts carry the same CNT = 0 — C's redundancy is the contract"
    );
}

/// A stop requested before counting actually began (us == REQSTART) still takes
/// both puts: C's `:463` guard is `justFinishedUserCount`, which `process()` sets
/// from `cnt != pcnt && !cnt` alone — it never consults `us`.
#[test]
fn r10_61_a_stop_before_counting_started_also_writes_coutp_twice() {
    let mut rec = armed_record();

    put_cnt(&mut rec, 1);
    let stop = put_cnt(&mut rec, 0);

    assert_eq!(coutp_values(&stop).len(), 2, "two independent puts");
}

/// A count that finishes on its own preset never touches `special()`, so C's
/// `:463` put is the only one: exactly one COUTP write.
#[test]
fn r10_61_a_preset_completion_writes_coutp_once() {
    let mut rec = armed_record();

    put_cnt(&mut rec, 1);

    // Device support reports acquisition complete (the dset `done()` return).
    rec.set_done();
    let finish = rec.process().unwrap();

    assert_eq!(
        write_fields(&finish.actions),
        vec!["COUT", "COUTP"],
        "no CNT was written, so special() never ran: only process()'s :457/:463 \
         puts fire"
    );
    assert!(
        rec.take_special_actions().is_empty(),
        "the special() channel is empty on a cycle no put drove"
    );
}

/// A redundant start (CNT=1 while already counting) is rejected by C's guard at
/// :622 before the put — no COUTP write at all.
#[test]
fn r10_61_a_redundant_start_writes_no_coutp() {
    let mut rec = armed_record();

    put_cnt(&mut rec, 1);

    // Second CNT=1 while us == COUNTING: C returns from special() immediately.
    let again = put_cnt(&mut rec, 1);

    assert!(
        coutp_values(&again).is_empty(),
        "the redundant-command guard (:622) returns before the put"
    );
}
