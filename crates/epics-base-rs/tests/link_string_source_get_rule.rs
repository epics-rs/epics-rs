//! C has ONE string-to-numeric rule for every read path; the port had two.
//!
//! `dbConvert.c` `getStringDouble` is
//! `if (*psrc == 0) *pdst++ = 0; else { status = epicsParseFloat64(...); if
//! (status) return status; }`, and the fast-link twin `dbFastLinkConv.c`
//! `cvt_st_d` is `if (*from == 0) { *to = 0.0; return 0; } return
//! epicsParseFloat64(from, to, &end);` — the identical empty-string
//! carve-out on both tables. `dbLink.c` `dbGetLink` then does
//! `status = dbTryGetLink(...); if (status == S_db_noLSET) return -1; if
//! (status) setLinkAlarm(plink);`, so status 0 means NO link alarm.
//!
//! The CA get path already obeyed that rule (`EpicsValue::get_convert` ->
//! `c_parse::get_string`). The DB-link path did not: it used
//! `EpicsValue::to_f64`, whose stricter parse answers `None` for the empty
//! string, so an empty `DBF_STRING` source produced `LinkFetch::Failed` and
//! a LINK/INVALID reader where C reads a successful `0.0` and raises
//! nothing. Both paths now come from one owner, `get_convert_f64`.
//!
//! The three boundaries of that rule — empty, parseable, unparseable — are
//! exercised on BOTH paths here, because one rule with two call sites is
//! only closed if both sites are pinned.
//!
//! The link half uses a `calc` multi-input link, not the cited
//! `links.rs` `LinkReadAs::Double` arm: `input_link_read_as` defaults to
//! `Native`, and its one `Double` producer (`sseq`) selects that arm only
//! for a source whose declared field type is already numeric, so the
//! reachable string-source site is the multi-input numeric funnel. The
//! fix is the same owner at both.
//!
//! The unparseable case pins the carve-out's WIDTH — it is the empty
//! string only, never "anything that fails to parse". The link read of an
//! unparseable source is still missing C's `setLinkAlarm`, which is a
//! different defect (the funnel splits the fetch from the conversion, so
//! no conversion status reaches the link), and is not claimed here.

use std::collections::{HashMap, HashSet};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, encode_dbr};

/// `DBR_DOUBLE`.
const DBR_DOUBLE: u16 = 6;

const DB: &str = r#"
record(ai,"S:EMPTY"){}
record(ai,"S:NUM"){field(DESC,"12.5")}
record(ai,"S:BAD"){field(DESC,"hello")}
record(calc,"C:EMPTY"){field(INPA,"S:EMPTY.DESC") field(CALC,"A")}
record(calc,"C:NUM"){field(INPA,"S:NUM.DESC") field(CALC,"A")}
record(calc,"C:BAD"){field(INPA,"S:BAD.DESC") field(CALC,"A")}
record(sel,"SEL:EMPTY"){field(NVL,"S:EMPTY.DESC") field(SELM,"Specified")}
"#;

/// A value the link read must overwrite, so "the funnel skipped the put"
/// and "the funnel stored a zero" are distinguishable.
const SENTINEL: f64 = 99.0;

async fn build() -> std::sync::Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

async fn seed_a(db: &PvDatabase, rec: &str) {
    db.put_pv(&format!("{rec}.A"), EpicsValue::Double(SENTINEL))
        .await
        .unwrap();
}

/// The get path's `f64`, or the ECA_GETFAIL that C's non-zero status becomes.
fn get_path(db: &PvDatabase, rec: &str) -> Option<f64> {
    let r = db.get_record(rec).unwrap();
    let snap = r.read().snapshot_for_field("DESC").expect("DESC snapshot");
    let body = encode_dbr(DBR_DOUBLE, &snap).ok()?;
    Some(f64::from_be_bytes(body[..8].try_into().unwrap()))
}

#[epics_macros_rs::epics_test]
async fn an_empty_string_source_is_a_successful_zero_on_both_paths() {
    let db = build().await;
    seed_a(&db, "C:EMPTY").await;

    process(&db, "C:EMPTY").await;
    assert_eq!(
        db.get_pv("C:EMPTY.A").unwrap().to_f64(),
        Some(0.0),
        "C's carve-out delivers 0.0, so the link read overwrites the sentinel"
    );

    assert_eq!(get_path(&db, "S:EMPTY"), Some(0.0));
}

#[epics_macros_rs::epics_test]
async fn a_parseable_string_source_reads_its_number_on_both_paths() {
    let db = build().await;
    seed_a(&db, "C:NUM").await;

    process(&db, "C:NUM").await;
    assert_eq!(db.get_pv("C:NUM.A").unwrap().to_f64(), Some(12.5));

    assert_eq!(get_path(&db, "S:NUM"), Some(12.5));
}

#[epics_macros_rs::epics_test]
async fn an_unparseable_string_source_is_not_swept_into_the_carve_out() {
    let db = build().await;
    seed_a(&db, "C:BAD").await;

    process(&db, "C:BAD").await;
    assert_eq!(
        db.get_pv("C:BAD.A").unwrap().to_f64(),
        Some(SENTINEL),
        "the carve-out is the empty string only; an unparseable source is a \
         failed conversion and must not fabricate a zero"
    );

    assert_eq!(
        get_path(&db, "S:BAD"),
        None,
        "the get path answers ECA_GETFAIL, not a fabricated zero"
    );
}

/// The SELN site of the same rule: `sel`'s NVL read converts through the
/// same owner, so an empty-string selector is index 0, not a dropped read.
#[epics_macros_rs::epics_test]
async fn the_seln_read_uses_the_same_rule() {
    let db = build().await;
    db.put_pv("SEL:EMPTY.SELN", EpicsValue::UShort(7))
        .await
        .unwrap();

    process(&db, "SEL:EMPTY").await;
    assert_eq!(db.get_pv("SEL:EMPTY.SELN").unwrap().to_f64(), Some(0.0));
}
