//! C gates the process-time UDF assignment on the read status:
//!
//! ```c
//! if (status == 0) {          /* mbbiDirectRecord.c:155-164 */
//!     epicsUInt32 rval = prec->rval;
//!     if (prec->shft > 0) rval >>= prec->shft;
//!     prec->val = rval;
//!     prec->udf = FALSE;
//! }
//! else if (status == 2)
//!     status = 0;
//!
//! if (prec->udf)              /* :168-169 */
//!     recGblSetSevr(prec, UDF_ALARM, INVALID_ALARM);
//! ```
//!
//! The clear is inside the success arm — a failed read leaves UDF where it was,
//! which is the only thing that makes the UDF_ALARM two lines down reachable.
//! Same shape, each at its own extent: `aiRecord.c:161`, `longinRecord.c:148`
//! and `int64inRecord.c:144` are one-line guarded clears, while `bi` and `mbbi`
//! put the clear inside a braced conversion arm — `biRecord.c:136-140` with the
//! clear at `:139`, `mbbiRecord.c:168-191` with the clear at `:174`.
//!
//! The exceptions are enumerated, not assumed: `waveformRecord.c:144` and
//! `aaiRecord.c:173` clear UDF on the line after `readValue` whatever it
//! returned, `compressRecord.c:342-366` folds a failed `dbGetLink` into
//! `status = 0` and clears anyway, and `subArrayRecord.c:147` assigns
//! `udf = !!status`. Those four keep deriving; the six above must not.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const DB: &str = r#"
record(mbbiDirect, "MBD") { field(INP, "NOSUCHREC CA") }
record(ai,         "AI")  { field(INP, "NOSUCHREC CA") }
record(bi,         "BI")  { field(INP, "NOSUCHREC CA") }
record(mbbi,       "MBI") { field(INP, "NOSUCHREC CA") }
record(longin,     "LI")  { field(INP, "NOSUCHREC CA") }
record(int64in,    "I64") { field(INP, "NOSUCHREC CA") }

record(waveform, "WF")  { field(FTVL, "DOUBLE") field(NELM, "3") field(INP, "NOSUCHREC CA") }
record(aai,      "AAI") { field(FTVL, "DOUBLE") field(NELM, "3") field(INP, "NOSUCHREC CA") }
record(compress, "CMP") { field(NSAM, "3") field(INP, "NOSUCHREC CA") }

record(ai,     "SRC") { }
record(longin, "OK")  { field(INP, "SRC.VAL") }
"#;

/// Every record type whose C leaves UDF alone when the read failed.
const STATUS_GATED: [&str; 6] = ["MBD", "AI", "BI", "MBI", "LI", "I64"];
/// Every record type whose C assigns UDF anyway.
const STATUS_BLIND: [&str; 3] = ["WF", "AAI", "CMP"];

async fn build() -> Arc<epics_base_rs::server::database::PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &epics_base_rs::server::database::PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

fn udf(db: &epics_base_rs::server::database::PvDatabase, rec: &str) -> u8 {
    db.get_record(rec).unwrap().read().common.udf
}

fn sevr(db: &epics_base_rs::server::database::PvDatabase, rec: &str) -> AlarmSeverity {
    db.get_record(rec).unwrap().read().common.sevr
}

/// Boundary: undefined record, read fails. C never reached its clear.
#[epics_macros_rs::epics_test]
async fn a_failed_read_leaves_the_status_gated_records_undefined() {
    let db = build().await;
    for rec in STATUS_GATED {
        assert_eq!(udf(&db, rec), 1, "{rec}: undefined before the first read");
        process(&db, rec).await;
        assert_eq!(
            udf(&db, rec),
            1,
            "{rec}: C's clear sits inside `if (status == 0)`"
        );
        assert_eq!(
            sevr(&db, rec),
            AlarmSeverity::Invalid,
            "{rec}: a broken INP is INVALID either way"
        );
    }
}

/// Boundary: the same failure on the records whose C clears regardless.
#[epics_macros_rs::epics_test]
async fn a_failed_read_still_defines_the_status_blind_records() {
    let db = build().await;
    for rec in STATUS_BLIND {
        process(&db, rec).await;
        assert_eq!(
            udf(&db, rec),
            0,
            "{rec}: C assigns UDF after readValue whatever it returned"
        );
    }
}

/// Boundary: defined record, read fails. C leaves UDF where it was — which
/// here means DEFINED, so the gate must not re-derive in the other direction.
#[epics_macros_rs::epics_test]
async fn a_failed_read_leaves_an_already_defined_record_defined() {
    let db = build().await;
    for rec in STATUS_GATED {
        db.put_pv(rec, EpicsValue::Long(1)).await.unwrap();
        assert_eq!(udf(&db, rec), 0, "{rec}: dbPut on a value field defines it");
        process(&db, rec).await;
        assert_eq!(udf(&db, rec), 0, "{rec}: the put stands, the read cannot");
    }
}

/// Boundary: the read SUCCEEDS. The gate exists to spare the failure path, not
/// to disable the clear.
#[epics_macros_rs::epics_test]
async fn a_successful_read_still_clears_udf() {
    let db = build().await;
    assert_eq!(udf(&db, "OK"), 1);
    process(&db, "OK").await;
    assert_eq!(udf(&db, "OK"), 0, "longinRecord.c:148 with status 0");
    assert_eq!(sevr(&db, "OK"), AlarmSeverity::NoAlarm);
}
