//! TSE=-2 (`epicsTimeEventDeviceTime`) says "the device stamped this record",
//! and `recGblGetTimeStampSimm` (`recGbl.c:324-342`) therefore leaves `time`
//! alone. For a `Soft Channel` input the device IS the INP link, so every one
//! of the 23 soft input dsets in `std/dev` fills it from the source, in the
//! same locked read that fetched the value — `devAiSoft.c:54-63`:
//!
//! ```c
//! static long readLocked(struct link *pinp, void *vvt)
//! {
//!     struct aivt *pvt = (struct aivt *) vvt;
//!     long status = dbGetLink(pinp, DBR_DOUBLE, &pvt->val, 0, 0);
//!
//!     if (!status && pvt->ptime)
//!         dbGetTimeStamp(pinp, pvt->ptime);
//!
//!     return status;
//! }
//! ```
//!
//! with `read_ai` (`:73-74`) supplying the gate:
//!
//! ```c
//! vt.ptime = (dbLinkIsConstant(&prec->tsel) &&
//!     prec->tse == epicsTimeEventDeviceTime) ? &prec->time : NULL;
//! ```
//!
//! Without it a TSE=-2 soft input never had its `time` written by anything, so
//! it served the `general_time` seed for the life of the IOC: every sample of a
//! 1 Hz chain carried one identical stamp and an archiver de-duplicated the lot.
//!
//! The invariant, asserted here at each of its boundaries: a soft input's TIME
//! is its INP source's TIME exactly when TSE is -2 and TSEL is constant, and
//! is the cycle's own time otherwise. `dbGetTimeStamp` is the `ptag == NULL`
//! spelling of `dbGetTimeStampTag` (`dbLink.c:413-416`), so the source's UTAG
//! is deliberately NOT adopted — only TSEL `.TIME` (`recGbl.c:317`) takes that.

use epics_base_rs::server::ioc_builder::IocBuilder;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// A stamp no clock in this process will ever produce, so an assertion that
/// finds it can only have got it from SRC.
const SENTINEL: Duration = Duration::new(1_000_000, 123_456_789);
const SENTINEL_UTAG: u64 = 0xDEAD_BEEF;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

async fn build(db_text: &str) -> Db {
    IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// Stamp SRC by hand: SRC is never processed in these tests, so the stamp
/// stays put and any record carrying it took it from SRC.
fn stamp_src(db: &Db) {
    let src = db.get_record("SRC").unwrap();
    let mut inst = src.write();
    inst.common.time = SystemTime::UNIX_EPOCH + SENTINEL;
    inst.common.utag = SENTINEL_UTAG;
}

async fn proc(db: &Db, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

fn time_of(db: &Db, rec: &str) -> SystemTime {
    db.get_record(rec).unwrap().read().common.time
}

fn utag_of(db: &Db, rec: &str) -> u64 {
    db.get_record(rec).unwrap().read().common.utag
}

/// The whole family goes through one soft-input read owner, so one case per
/// record type is what proves the owner is the one being fixed rather than a
/// per-record arm: scalar (`devAiSoft`), integer (`devLiSoft`), string
/// (`devSiSoft`) and array (`devWfSoft`) all reach it.
#[epics_macros_rs::epics_test]
async fn every_soft_input_kind_adopts_the_inp_sources_time() {
    let db = build(
        r#"
record(ai, "SRC") { field(VAL, "7") }
record(ai,       "D_AI") { field(INP, "SRC") field(TSE, "-2") }
record(longin,   "D_LI") { field(INP, "SRC") field(TSE, "-2") }
record(stringin, "D_SI") { field(INP, "SRC") field(TSE, "-2") }
record(waveform, "D_WF") { field(INP, "SRC") field(TSE, "-2") field(FTVL, "DOUBLE") field(NELM, "4") }
"#,
    )
    .await;
    stamp_src(&db);

    for dst in ["D_AI", "D_LI", "D_SI", "D_WF"] {
        proc(&db, dst).await;
        assert_eq!(
            time_of(&db, dst),
            SystemTime::UNIX_EPOCH + SENTINEL,
            "{dst} must carry SRC's stamp"
        );
    }
}

/// `dbGetTimeStamp` is `dbGetTimeStampTag(link, stamp, NULL)`: the source's
/// UTAG is not part of what a soft dset adopts.
#[epics_macros_rs::epics_test]
async fn the_source_utag_is_not_adopted_with_its_time() {
    let db = build(
        r#"
record(ai, "SRC") { field(VAL, "7") }
record(ai, "DST") { field(INP, "SRC") field(TSE, "-2") }
"#,
    )
    .await;
    stamp_src(&db);
    proc(&db, "DST").await;

    assert_eq!(time_of(&db, "DST"), SystemTime::UNIX_EPOCH + SENTINEL);
    assert_eq!(utag_of(&db, "DST"), 0, "ptag is NULL on this path");
}

/// Boundary: TSE != -2. C computes `vt.ptime = NULL` and
/// `recGblGetTimeStampSimm` stamps from `epicsTimeGetEvent` instead.
#[epics_macros_rs::epics_test]
async fn a_default_tse_soft_input_keeps_its_own_cycle_time() {
    let db = build(
        r#"
record(ai, "SRC") { field(VAL, "7") }
record(ai, "DST") { field(INP, "SRC") }
"#,
    )
    .await;
    stamp_src(&db);
    proc(&db, "DST").await;

    assert_ne!(
        time_of(&db, "DST"),
        SystemTime::UNIX_EPOCH + SENTINEL,
        "TSE=0 stamps from the clock, not from INP"
    );
}

/// Boundary: the other half of C's `&&`. A non-constant TSEL disables the
/// adoption outright, and since TSE resolves to -2 the record is then left
/// with whatever time it already had — C stamps nothing at all here.
#[epics_macros_rs::epics_test]
async fn a_linked_tsel_disables_the_adoption() {
    let db = build(
        r#"
record(ai, "SRC")   { field(VAL, "7") }
record(ai, "TSESRC"){ field(VAL, "-2") }
record(ai, "DST")   { field(INP, "SRC") field(TSEL, "TSESRC") }
"#,
    )
    .await;
    stamp_src(&db);
    proc(&db, "DST").await;

    assert_eq!(
        db.get_record("DST").unwrap().read().common.tse,
        -2,
        "the linked TSEL did load TSE"
    );
    assert_ne!(
        time_of(&db, "DST"),
        SystemTime::UNIX_EPOCH + SENTINEL,
        "dbLinkIsConstant(&prec->tsel) is false, so ptime is NULL"
    );
}

/// Boundary: no read happened. C's `if (!status && pvt->ptime)` skips the
/// stamp, and `read_wf`/`read_sa` do not even call `readLocked` for a constant
/// INP (`devWfSoft.c:81-82`, `devSASoft.c:105-111`).
#[epics_macros_rs::epics_test]
async fn a_constant_inp_adopts_nothing() {
    let db = build(
        r#"
record(ai, "SRC") { field(VAL, "7") }
record(ai, "DST") { field(INP, "5.5") field(TSE, "-2") }
"#,
    )
    .await;
    stamp_src(&db);
    let before = time_of(&db, "DST");
    proc(&db, "DST").await;

    assert_eq!(
        time_of(&db, "DST"),
        before,
        "TSE=-2 with nothing to adopt leaves TIME exactly as it was"
    );
}

/// The operational shape the finding names: SRC advances, and DST's stamp must
/// advance with it rather than staying pinned at the seed.
#[epics_macros_rs::epics_test]
async fn the_adopted_stamp_tracks_the_source() {
    let db = build(
        r#"
record(ai, "SRC") { field(VAL, "7") }
record(ai, "DST") { field(INP, "SRC") field(TSE, "-2") }
"#,
    )
    .await;

    stamp_src(&db);
    proc(&db, "DST").await;
    let first = time_of(&db, "DST");

    let src = db.get_record("SRC").unwrap();
    src.write().common.time = SystemTime::UNIX_EPOCH + SENTINEL + Duration::from_secs(1);
    proc(&db, "DST").await;
    let second = time_of(&db, "DST");

    assert_eq!(
        second.duration_since(first).unwrap(),
        Duration::from_secs(1),
        "DST's stamp must move exactly as SRC's did"
    );
}

/// The value still arrives — the timestamp read must not have displaced it.
#[epics_macros_rs::epics_test]
async fn the_value_still_arrives_alongside_the_stamp() {
    let db = build(
        r#"
record(ai, "SRC") { field(VAL, "7") }
record(ai, "DST") { field(INP, "SRC") field(TSE, "-2") }
"#,
    )
    .await;
    stamp_src(&db);
    proc(&db, "DST").await;

    let v = db
        .get_record("DST")
        .unwrap()
        .read()
        .record
        .get_field("VAL")
        .and_then(|v| v.to_f64());
    assert_eq!(v, Some(7.0));
}
