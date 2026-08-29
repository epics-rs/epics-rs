//! `TSEL` naming a `.TIME` field makes the record adopt the source's stamp,
//! and a channel filter on that link does not switch the rule off.
//!
//! C decides it on the raw pvname and then destroys the tail: `TSEL_modified`
//! (`dbLink.c:71-87`) does `strstr(ppv_link->pvname, ".TIME")` and writes a
//! NUL at the match, so `TSEL="SRC.TIME[0]"` sets `DBLINK_FLAG_TSELisTIME`
//! and the filter goes with everything after `.TIME`.
//! `recGblGetTimeStampSimm` (`recGbl.c:316-321`) then copies the link's time
//! AND utag through `dbGetTimeStampTag` and returns.
//!
//! The port asked the link's raw halves instead, and for a filtered link those
//! are the whole name with field `VAL`: the `.TIME` test was false, the flag
//! never set, and the record stamped itself from the clock. Measured on
//! `R7.0.10-146-g8f5015b66` softIoc with `SRC` processed three seconds before
//! its two consumers: C gives `PLAIN.TIME == FILT.TIME == SRC.TIME`, so the
//! filtered link adopts the stamp exactly as the plain one does.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use epics_base_rs::server::ioc_builder::IocBuilder;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

/// A stamp and tag no clock in this process produces, so a record carrying
/// them can only have taken them from `SRC`.
const SRC_STAMP: Duration = Duration::new(3_000_000, 333_333_333);
const SRC_UTAG: u64 = 0xC3C3_C3C3;

const DB_TEXT: &str = r#"
record(ai, "TSELF_SRC")   { field(DTYP, "Soft Channel") field(VAL, "1") }
record(ai, "TSELF_PLAIN") { field(DTYP, "Soft Channel") field(TSEL, "TSELF_SRC.TIME") }
record(ai, "TSELF_ARR")   { field(DTYP, "Soft Channel") field(TSEL, "TSELF_SRC.TIME[0]") }
record(ai, "TSELF_JSON")  { field(DTYP, "Soft Channel") field(TSEL, "TSELF_SRC.TIME{\"dbnd\":{\"d\":1}}") }
"#;

async fn proc(db: &Db, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

fn stamp_of(db: &Db, rec: &str) -> (SystemTime, u64) {
    let inst = db.get_record(rec).expect("record exists");
    let g = inst.read();
    (g.common.time, g.common.utag)
}

#[epics_macros_rs::epics_test]
async fn a_filtered_tsel_time_link_adopts_the_source_stamp() {
    let db: Db = IocBuilder::new()
        .db_string(DB_TEXT, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    // `TSELF_SRC` is never processed, so the planted pair stays put.
    {
        let rec = db.get_record("TSELF_SRC").expect("source exists");
        let mut inst = rec.write();
        inst.common.time = SystemTime::UNIX_EPOCH + SRC_STAMP;
        inst.common.utag = SRC_UTAG;
    }
    let want = (SystemTime::UNIX_EPOCH + SRC_STAMP, SRC_UTAG);

    for (rec, label) in [
        ("TSELF_PLAIN", "unfiltered, the control"),
        ("TSELF_ARR", "`[range]` after `.TIME`"),
        ("TSELF_JSON", "JSON filter after `.TIME`"),
    ] {
        proc(&db, rec).await;
        assert_eq!(stamp_of(&db, rec), want, "{label}: {rec}");
    }
}
