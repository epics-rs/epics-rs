//! Regression tests (W10-B3): an ALL-ZERO `epicsTimeStamp` prints
//! `<undefined>`, not the EPICS epoch.
//!
//! C's `epicsTimeToStrftime` short-circuits before it ever builds a `tm`
//! (`epicsTime.cpp:175-179`):
//!
//! ```c
//! // presume that EPOCH date is an uninitialized time stamp
//! if ( pTS->secPastEpoch == 0 && pTS->nsec == 0u ) {
//!     strncpy ( pBuff, "<undefined>", bufLength );
//! ```
//!
//! The test is on the stamp AS IT ARRIVED — seconds past the 1990 epoch,
//! before any timezone applies. Observed on the compiled C `caget` (EPICS
//! 7.0.10.1-DEV) against a never-processed record:
//!
//! ```text
//! caget -a TST:NEVER   C: TST:NEVER   <undefined> 0 UDF INVALID
//!                     RS: TST:NEVER   1990-01-01 09:00:00.000000 0 UDF INVALID
//! ```
//!
//! The port converted first and formatted second, so the sentinel had nowhere
//! to live — and the local timezone leaked into the date it printed instead.
//! Every zeroed stamp reaches this: `camonitor`, `caget -a`, `caget -d
//! DBR_TIME_*`, `caput -l`, and every timed-out synchronous readback
//! (`cli::zero_dbr_snapshot`).

#![cfg(all(feature = "client-core", not(epics_embedded_target)))]

use epics_base_rs::types::WallTime;
use epics_ca_rs::cli::{EPICS_EPOCH_UNIX_SECS, format_time};

/// C `epicsTime.cpp:176`.
const UNDEFINED_TIME: &str = "<undefined>";

#[test]
fn an_all_zero_stamp_is_undefined_not_the_epics_epoch() {
    assert_eq!(
        format_time(WallTime::from_unix(EPICS_EPOCH_UNIX_SECS, 0)),
        UNDEFINED_TIME,
        "secPastEpoch == 0 && nsec == 0"
    );
}

#[test]
fn only_an_all_zero_stamp_is_undefined() {
    // Either field non-zero is a real stamp, however close to the epoch.
    assert_ne!(
        format_time(WallTime::from_unix(EPICS_EPOCH_UNIX_SECS, 1)),
        UNDEFINED_TIME,
        "nsec != 0"
    );
    assert_ne!(
        format_time(WallTime::from_unix(EPICS_EPOCH_UNIX_SECS + 1, 0)),
        UNDEFINED_TIME,
        "secPastEpoch != 0"
    );
    // The µs rounding must not manufacture the sentinel either: nsec = 499
    // rounds to 0 microseconds, but the STAMP is not zero.
    assert_ne!(
        format_time(WallTime::from_unix(EPICS_EPOCH_UNIX_SECS, 499)),
        UNDEFINED_TIME,
        "the check is on the stamp, not on the rounded microseconds"
    );
}

/// The sentinel replaces the WHOLE rendering — C never prints a date beside
/// it, so nothing downstream may assume a fixed-width timestamp column.
#[test]
fn the_sentinel_replaces_the_entire_timestamp() {
    let s = format_time(WallTime::from_unix(EPICS_EPOCH_UNIX_SECS, 0));
    assert_eq!(s, "<undefined>");
    assert!(!s.contains("1990"), "no date leaks through: {s}");
}
