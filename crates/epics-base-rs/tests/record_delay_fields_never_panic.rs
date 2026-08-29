//! A record delay field the network can set must not abort the IOC.
//!
//! C `boRecord.c` process passes `(double)prec->high` to
//! `callbackRequestDelayed`, which reaches `epicsTimeAddSeconds`
//! (`epicsTime.cpp`) and its out-of-range `epicsInt64(seconds*1e9 + …)`
//! conversion: the deadline is garbage, the one-shot fires at the wrong
//! time, and every other PV keeps being served. `caput BO:TEST.HIGH inf`
//! is reachable — `epicsParseDouble` accepts `inf` because `strtod`
//! leaves `errno` unset for it — and the port's
//! `Duration::from_secs_f64(self.high)` panicked on exactly that value,
//! unwinding the record's processing task.
//!
//! `busy` is `bo` transcribed (`busyRecord.c:258-263` is `boRecord.c:257-262`
//! verbatim, module `busy` at `R1-7-4-6-g2dfe92d`), so it carries the same
//! DBF_DOUBLE HIGH and the same arming call. Both are driven through one
//! boundary table here: a fix that reaches only the record that was reported
//! leaves the other one panicking.
//!
//! **This file is coverage, not the fix.** Both records already route the
//! conversion through the saturating `runtime::time::duration_from_secs`, so
//! every case below passes on this file's own parent `33aadac8^`, where the
//! calls are `bo.rs:279`, `:309` and `busy.rs:282`, `:307`. They arrived
//! separately
//! and neither hash covers both: `busy.rs` in `a7c7913c`, `bo.rs` in
//! `0f1a5d33`, and at `a7c7913c` itself that `bo.rs` call is still the
//! panicking
//! `Duration::from_secs_f64`. What the table adds is the second record —
//! the earlier version asserted only `bo`,
//! leaving `busy`'s identical arming call unasserted — and a shape where the
//! next record type that arms a one-shot from a DBF_DOUBLE is one row rather
//! than a second file.

use epics_base_rs::server::record::ProcessAction;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::EpicsValue;

/// A record type that arms a one-shot from a `DBF_DOUBLE` HIGH, named for the
/// assertion messages, plus a constructor for a fresh instance of it.
type ArmingRecord = (&'static str, fn() -> Box<dyn Record>);

/// Every record type whose process arms a one-shot from a `DBF_DOUBLE` HIGH.
fn arming_records() -> [ArmingRecord; 2] {
    [
        ("bo", || Box::new(BoRecord::new(0))),
        ("busy", || Box::new(BusyRecord::new())),
    ]
}

/// Every non-representable `HIGH` a `caput` can deliver, one case per
/// boundary of `Duration::try_from_secs_f64`'s single rule.
#[test]
fn a_high_the_network_can_set_never_unwinds_process() {
    for (kind, make) in arming_records() {
        for high in [f64::INFINITY, 1e300, u64::MAX as f64, f64::MAX] {
            let mut rec = make();
            rec.put_field("HIGH", EpicsValue::Double(high))
                .expect("HIGH accepts any double, as C's dbPutField does");
            rec.put_field("VAL", EpicsValue::Long(1))
                .expect("VAL 1 arms the HIGH one-shot");

            let outcome = rec
                .process()
                .unwrap_or_else(|e| panic!("{kind} HIGH={high} must process, got {e}"));

            let delay = outcome
                .actions
                .iter()
                .find_map(|a| match a {
                    ProcessAction::DelayedCallbackAfter(d) => Some(*d),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{kind} HIGH={high} must still arm the one-shot"));
            assert_eq!(
                delay,
                std::time::Duration::MAX,
                "{kind} HIGH={high} is a deadline no comparison reaches — C's \
                 garbage deadline that never fires"
            );
        }

        // C arms the one-shot only under `(prec->high>0)`, which is false
        // for a negative and for NaN, so neither reaches the conversion at
        // all — on both sides.
        for high in [f64::NEG_INFINITY, f64::NAN, 0.0] {
            let mut rec = make();
            rec.put_field("HIGH", EpicsValue::Double(high)).unwrap();
            rec.put_field("VAL", EpicsValue::Long(1)).unwrap();
            let outcome = rec
                .process()
                .unwrap_or_else(|e| panic!("{kind} HIGH={high} must process, got {e}"));
            assert!(
                !outcome
                    .actions
                    .iter()
                    .any(|a| matches!(a, ProcessAction::DelayedCallbackAfter(_))),
                "{kind} HIGH={high} fails C's `high > 0` test and arms nothing"
            );
        }
    }
}
