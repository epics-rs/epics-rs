//! A DB link whose target carries a channel filter names a LOCAL record.
//!
//! C `dbDbInitLink` (`dbDbLink.c:88-96`) decides locality by handing the whole
//! `pv_link.pvname` to `dbChannelCreate`, which is the one function that knows
//! where the record name stops and the filter begins (`dbChannel.c:448-530` →
//! `pvNameLookup`, `:311-329`). `src.[2]` therefore resolves to record `src`,
//! field `VAL`, chain `arr(2,2)`, stays a DB link, and `dbDbGetValue` runs the
//! chain on the way out (`dbDbLink.c:206-219`).
//!
//! The port judged locality on the link's raw record text instead, so every
//! filter shape missed the local lookup and `initialize_link_locality` rewrote
//! the link as CA. Measured against a built softIoc on this exact database
//! (`epics-base/modules/database/test/std/rec/linkFilterTest.db`): C reads
//! `ai` = 3 and `wf` = 3,4,5; the port read 0 for both with
//! `INP : CA_LINK src.[2] NPP NMS`, `STAT: LINK`, `SEVR: INVALID`, `UDF: 1`
//! and `dbcar` reporting one CA link, none connected.

use std::collections::HashSet;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// `linkFilterTest.db` verbatim.
const DB: &str = r#"
record(waveform, "src") {
    field(NELM, "10")
    field(FTVL, "SHORT")
    field(INP, [1, 2, 3, 4, 5, 6, 7, 8])
}
record(ai, "ai") {
    field(INP, "src.[2]") # expect 3
    field(PINI, "YES")
}
record(waveform, "wf") {
    field(NELM, "5")
    field(FTVL, "DOUBLE")
    field(INP,  "src.[2:4]") # expect 3,4,5
    field(PINI, "YES")
}
"#;

/// Build and run the iocInit pass that commits `dbInitLink`'s locality
/// decision — the pass that rewrote these links as CA.
async fn build() -> std::sync::Arc<epics_base_rs::server::database::PvDatabase> {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.initialize_link_locality().await;
    db
}

async fn process(db: &epics_base_rs::server::database::PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// `field(INP, "src.[2]")` on a scalar reader: one element, the third.
#[epics_macros_rs::epics_test]
async fn a_filtered_link_reads_the_selected_element() {
    let db = build().await;
    process(&db, "src").await;
    process(&db, "ai").await;

    assert_eq!(db.get_pv("ai").unwrap().to_f64().unwrap(), 3.0);
    let rec = db.get_record("ai").unwrap();
    let g = rec.read();
    assert_eq!(g.common.udf, 0, "a link that delivered leaves UDF clear");
    assert_eq!(
        g.common.sevr,
        epics_base_rs::server::record::AlarmSeverity::NoAlarm,
        "a resolved local link raises no LINK/INVALID alarm"
    );
}

/// `field(INP, "src.[2:4]")` into a 5-element waveform: three elements, and
/// NORD follows the FILTERED count, not the source's.
#[epics_macros_rs::epics_test]
async fn a_filtered_link_reads_the_selected_slice() {
    let db = build().await;
    process(&db, "src").await;
    process(&db, "wf").await;

    assert_eq!(
        db.get_pv("wf").unwrap(),
        EpicsValue::DoubleArray(vec![3.0, 4.0, 5.0])
    );
    assert_eq!(db.get_pv("wf.NORD").unwrap().to_f64().unwrap(), 3.0);
}

/// The unfiltered shapes the same split has to keep answering: a bare record,
/// an explicit field, and a filter on a named field.
#[epics_macros_rs::epics_test]
async fn the_unfiltered_shapes_are_unchanged() {
    let db = IocBuilder::new()
        .db_string(
            r#"
record(waveform, "src") {
    field(NELM, "10")
    field(FTVL, "SHORT")
    field(INP, [1, 2, 3, 4, 5, 6, 7, 8])
}
record(ai, "whole") { field(INP, "src") }
record(ai, "nord")  { field(INP, "src.NORD") }
record(ai, "filtered:field") { field(INP, "src.NORD{\"arr\":{\"s\":0}}") }
"#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.initialize_link_locality().await;
    process(&db, "src").await;

    process(&db, "whole").await;
    assert_eq!(
        db.get_pv("whole").unwrap().to_f64().unwrap(),
        1.0,
        "an unfiltered array link still delivers its first element to a scalar"
    );
    process(&db, "nord").await;
    assert_eq!(db.get_pv("nord").unwrap().to_f64().unwrap(), 8.0);
    process(&db, "filtered:field").await;
    assert_eq!(
        db.get_pv("filtered:field").unwrap().to_f64().unwrap(),
        8.0,
        "a filter on a named field keeps the field, not the record's VAL"
    );
}

/// C reaches `dbLockSetMerge(NULL, plink->precord, precord)` from
/// `dbDbInitLink` (`dbDbLink.c:94-109`) with the record the whole `pvname`
/// resolved to, so a filtered reader shares its source's lock set. Judged on
/// the raw link text, `ai` and `wf` each sat alone and their slice reads ran
/// outside the lock that serialises `src`'s own processing.
#[epics_macros_rs::epics_test]
async fn a_filtered_link_joins_its_source_lock_set() {
    let db = build().await;
    db.build_lock_sets();

    let set = db
        .lock_set_report()
        .active
        .into_iter()
        .find(|s| s.members.iter().any(|m| m == "src"))
        .expect("src is in a lock set");
    let mut members = set.members.clone();
    members.sort();
    assert_eq!(members, ["ai", "src", "wf"]);
}
