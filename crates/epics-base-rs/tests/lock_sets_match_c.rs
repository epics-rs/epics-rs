//! Defect test: this IOC must group records into C's lock sets, not one gate
//! per record.
//!
//! GROUND TRUTH — the built C `softIoc` (7.0.10.1-DEV,
//! `/home/stevek/work/epics-base/bin/linux-x86_64/softIoc`) on
//!
//! ```text
//! record(calc, "R:A") { field(INPA, "R:B") field(CALC, "A+1") field(FLNK, "R:B") }
//! record(calc, "R:B") { field(CALC, "1") }
//! record(ai,   "R:C") { }
//! ```
//!
//! before `iocInit`:
//!
//! ```text
//! epics> dbLockShowLocked(0)
//! Active lockSets: 0
//! Free lockSets: 0
//! ```
//!
//! and after it:
//!
//! ```text
//! epics> dbLockShowLocked(0)
//! Active lockSets: 2
//! Free lockSets: 1
//! epics> dblsr("*",0)
//! Lock Set 2 1 members 1 refs epicsMutexId 0x60c74b8a5ad0
//! Lock Set 3 2 members 2 refs epicsMutexId 0x60c74b8a5c10
//! ```
//!
//! Three records, two sets: the two joined by `INPA`/`FLNK` are behind one
//! mutex and the free-standing one behind another, and the third set the
//! per-record pass created is on the free list. `refs` equals the member count
//! because nothing holds a set.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// Three unlinked records, the starting state of the runtime-relink capture in
/// [`the runtime relink`](self) below.
const RELINK_DB: &str = r#"
record(calc, "L:A") { field(CALC, "A+1") }
record(calc, "L:B") { field(CALC, "1") }
record(calc, "L:C") { field(CALC, "1") }
"#;

/// C `dbpf` — `dbPutField`, which sends a DBF link field to `dbPutFieldLink`
/// (`dbAccess.c:1261`) and thence to `dbDbAddLink`/`dbDbRemoveLink`.
async fn dbpf(db: &PvDatabase, record: &str, field: &str, text: &str) {
    db.put_record_field_from_ca_no_notify(record, field, EpicsValue::String(text.into()))
        .await
        .unwrap();
}

/// `(grouping, free count)` — the two numbers the C capture pins.
fn partition(db: &PvDatabase) -> (Vec<Vec<String>>, usize) {
    let report = db.lock_set_report();
    (
        report.active.iter().map(|s| s.members.clone()).collect(),
        report.free,
    )
}

const ORACLE_DB: &str = r#"
record(calc, "R:A") { field(INPA, "R:B") field(CALC, "A+1") field(FLNK, "R:B") }
record(calc, "R:B") { field(CALC, "1") }
record(ai,   "R:C") { }
"#;

async fn ioc(db_text: &str) -> std::sync::Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

/// Member grouping and the active/free accounting, against the capture above.
#[epics_macros_rs::epics_test]
async fn linked_records_share_one_lock_set_and_free_the_spare() {
    let db = ioc(ORACLE_DB).await;
    let report = db.lock_set_report();

    let members: Vec<Vec<String>> = report.active.iter().map(|s| s.members.clone()).collect();
    assert_eq!(
        members,
        vec![
            vec!["R:A".to_string(), "R:B".to_string()],
            vec!["R:C".to_string()]
        ],
        "C merges R:A and R:B through INPA/FLNK and leaves R:C alone"
    );
    assert_eq!(report.active.len(), 2, "C: Active lockSets: 2");
    assert_eq!(
        report.free, 1,
        "C: Free lockSets: 1 — the merge freed one set"
    );

    for set in &report.active {
        assert_eq!(
            set.refs,
            set.members.len(),
            "an unheld set reports one ref per member, as the capture shows"
        );
        assert!(!set.locked, "nothing holds a set here");
        assert!(
            set.mutex.is_some(),
            "every lock set's mutex must be on the process mutex list, so \
             dblsr and dbLockShowLocked print the same epicsMutexId for it"
        );
    }
}

/// The other side of the same boundary: with records present but `iocInit`
/// not run, C has no lock sets at all, because `dbLockInitRecords` has not
/// run yet. Nothing may create one behind its back.
#[epics_macros_rs::epics_test]
async fn a_loaded_but_uninitialised_database_has_no_lock_sets() {
    use epics_base_rs::server::records::ai::AiRecord;

    let db = PvDatabase::new();
    for name in ["R:A", "R:B", "R:C"] {
        db.add_record(name, Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
    }

    let report = db.lock_set_report();
    assert_eq!(report.active.len(), 0, "C: Active lockSets: 0");
    assert_eq!(report.free, 0, "C: Free lockSets: 0");
    assert!(
        db.lock_set_of("R:A").is_none(),
        "dblsr returns before printing for a record with no lset (dbLock.c:900)"
    );

    // And the pass that C runs at `iocInit` is what creates them.
    db.ioc_init().await;
    assert_eq!(db.lock_set_report().active.len(), 3, "one set per record");
}

/// Every member of one set reaches the SAME set, which is the property the
/// grouping exists for — a write to R:A and a write to R:B are serialised
/// against each other in C and now here.
#[epics_macros_rs::epics_test]
async fn every_member_of_a_set_reaches_that_set() {
    let db = ioc(ORACLE_DB).await;
    let a = db
        .lock_set_of("R:A")
        .expect("R:A has a lock set after init");
    let b = db
        .lock_set_of("R:B")
        .expect("R:B has a lock set after init");
    let c = db
        .lock_set_of("R:C")
        .expect("R:C has a lock set after init");

    assert_eq!(a.id, b.id, "INPA/FLNK merged R:A and R:B into one set");
    assert_ne!(a.id, c.id, "no link reaches R:C");
    assert_eq!(a.members, vec!["R:A".to_string(), "R:B".to_string()]);
    assert_eq!(c.members, vec!["R:C".to_string()]);
}

/// A link that C does NOT merge on. `dbDbInitLink` runs only for a link that
/// resolved locally; a `ca://` target goes to `dbCaAddLink` instead
/// (`dbLink.c:118-130`) even when the name happens to be a local record, so
/// the set must not widen.
#[epics_macros_rs::epics_test]
async fn a_ca_link_to_a_local_record_does_not_merge() {
    let db = ioc(r#"
record(calc, "C:A") { field(INPA, "ca://C:B") field(CALC, "A+1") }
record(calc, "C:B") { field(CALC, "1") }
"#)
    .await;
    let report = db.lock_set_report();
    assert_eq!(
        report.active.len(),
        2,
        "a CA link merges nothing, so both records keep their own set"
    );
    assert_eq!(report.free, 0, "nothing merged, so nothing was freed");
}

/// The `+1` C's `dbScanLockMany` adds for its locked list (`dbLock.c:404`),
/// and its absence for a plain `dbScanLock`, which drops its transient
/// reference as soon as it holds the mutex (`:220-222`).
#[epics_macros_rs::epics_test]
async fn an_epoch_adds_one_ref_per_set_and_a_plain_write_adds_none() {
    let db = ioc(ORACLE_DB).await;
    let baseline = db.lock_set_of("R:A").unwrap().refs;
    assert_eq!(baseline, 2, "two members, nothing held");

    {
        let _epoch = db.lock_records(["R:A"]);
        let held = db.lock_set_of("R:A").unwrap();
        assert_eq!(held.refs, baseline + 1, "the locked list holds one ref");
        assert!(held.locked, "the set's mutex cannot be taken right now");
    }
    assert_eq!(db.lock_set_of("R:A").unwrap().refs, baseline);

    {
        let _write = db.lock_record("R:A");
        assert_eq!(
            db.lock_set_of("R:A").unwrap().refs,
            baseline,
            "dbScanLock leaves the count at one-per-member"
        );
    }
}

/// An epoch naming two records of ONE set takes that set once, not twice —
/// `dbScanLockMany` skips the duplicates its sort groups together
/// (`dbLock.c:399-402`).
#[epics_macros_rs::epics_test]
async fn an_epoch_over_two_members_of_one_set_takes_it_once() {
    let db = ioc(ORACLE_DB).await;
    let _epoch = db.lock_records(["R:A", "R:B"]);
    assert_eq!(
        db.lock_set_of("R:A").unwrap().refs,
        3,
        "two members plus ONE locked-list ref, not two"
    );
}

// ---------------------------------------------------------------------------
// The runtime relink.
//
// GROUND TRUTH — the same C `softIoc`, on `RELINK_DB` after `iocInit`, driving
// each edit with `dbpf` and reading `dblsr("*",1)` / `dbLockShowLocked(0)`
// after it. Verbatim, headers and mutex rows elided:
//
// ```text
// epics> dblsr("*",1)                    Active 3 / Free 0
// Lock Set 2 1 members 1 refs / L:A
// Lock Set 3 1 members 1 refs / L:B
// Lock Set 4 1 members 1 refs / L:C
// epics> dbpf("L:A.INPA","L:B")          Active 2 / Free 1
// Lock Set 2 2 members 2 refs / L:A / L:B
// Lock Set 4 1 members 1 refs / L:C
// epics> dbpf("L:A.FLNK","L:B")          Active 2 / Free 1   (unchanged)
// epics> dbpf("L:A.INPA","0")            Active 2 / Free 1   (unchanged)
// epics> dbpf("L:A.FLNK","0")            Active 3 / Free 0
// Lock Set 2 1 members 1 refs / L:A
// Lock Set 4 1 members 1 refs / L:C
// Lock Set 3 1 members 1 refs / L:B
// epics> dbpf("L:A.INPA","L:C")          Active 2 / Free 1
// Lock Set 2 2 members 2 refs / L:A / L:C
// Lock Set 3 1 members 1 refs / L:B
// ```
//
// Four separate facts live in that sequence, and each has a test below: the
// merge keeps `pfirst`'s set (L:A's id 2 survives, L:B's id 3 is freed); a
// second link between records already together changes nothing; removing ONE
// of two links does NOT split, which is C's `goto nosplit` (`dbLock.c:826`);
// and removing the LAST one does, handing the split-off side a set back off
// the free list — id 3 returns, and `Free lockSets` drops to 0.
//
// The listing ORDER differs and is allowed to: C appends to `lockSetsActive`,
// so the reused set prints last; this port lists by id.

/// Creating a DB link at runtime merges the two records' sets, and the set the
/// merge empties goes on the free list rather than vanishing.
#[epics_macros_rs::epics_test]
async fn a_link_written_after_ioc_init_merges_the_two_sets() {
    let db = ioc(RELINK_DB).await;
    assert_eq!(
        partition(&db).1,
        0,
        "three records, three sets, nothing free"
    );
    let a_before = db.lock_set_of("L:A").unwrap().id;

    dbpf(&db, "L:A", "INPA", "L:B").await;

    let (members, free) = partition(&db);
    assert_eq!(
        members,
        vec![
            vec!["L:A".to_string(), "L:B".to_string()],
            vec!["L:C".to_string()]
        ]
    );
    assert_eq!(free, 1, "C: Free lockSets: 1");
    assert_eq!(
        db.lock_set_of("L:A").unwrap().id,
        a_before,
        "C's dbLockSetMerge keeps pfirst's set, and pfirst is the record whose \
         link moved"
    );
    assert_eq!(db.lock_set_of("L:B").unwrap().id, a_before);
}

/// A second link between records that are already one set is C's `if(A==B)
/// return;` (`dbLock.c:612`): no merge, no free-list movement, no new id.
#[epics_macros_rs::epics_test]
async fn a_second_link_between_the_same_two_records_changes_nothing() {
    let db = ioc(RELINK_DB).await;
    dbpf(&db, "L:A", "INPA", "L:B").await;
    let before = partition(&db);
    let id = db.lock_set_of("L:A").unwrap().id;

    dbpf(&db, "L:A", "FLNK", "L:B").await;

    assert_eq!(partition(&db), before, "already merged: nothing to do");
    assert_eq!(db.lock_set_of("L:A").unwrap().id, id);
}

/// C's `goto nosplit` (`dbLock.c:826`): the breadth-first walk from the former
/// target still reaches the record that held the link, so removing one of two
/// links between them creates no set.
#[epics_macros_rs::epics_test]
async fn removing_one_of_two_links_does_not_split_the_set() {
    let db = ioc(RELINK_DB).await;
    dbpf(&db, "L:A", "INPA", "L:B").await;
    dbpf(&db, "L:A", "FLNK", "L:B").await;
    let before = partition(&db);

    dbpf(&db, "L:A", "INPA", "0").await;

    assert_eq!(
        partition(&db),
        before,
        "FLNK still joins them, so the component is unchanged"
    );
}

/// Removing the LAST link does split, and the side that leaves takes a set off
/// the free list — C `dbLockSetSplit`'s `makeSet()` (`dbLock.c:797`), which is
/// why `Free lockSets` goes back to 0 rather than staying at 1.
#[epics_macros_rs::epics_test]
async fn removing_the_last_link_splits_and_reuses_a_freed_set() {
    let db = ioc(RELINK_DB).await;
    let a_id = db.lock_set_of("L:A").unwrap().id;
    let b_id = db.lock_set_of("L:B").unwrap().id;
    dbpf(&db, "L:A", "INPA", "L:B").await;
    dbpf(&db, "L:A", "FLNK", "L:B").await;
    dbpf(&db, "L:A", "INPA", "0").await;
    assert_eq!(partition(&db).1, 1, "one set is on the free list");

    dbpf(&db, "L:A", "FLNK", "0").await;

    let (members, free) = partition(&db);
    assert_eq!(
        members,
        vec![
            vec!["L:A".to_string()],
            vec!["L:B".to_string()],
            vec!["L:C".to_string()]
        ]
    );
    assert_eq!(free, 0, "C: Free lockSets: 0 — makeSet took the spare back");
    assert_eq!(db.lock_set_of("L:A").unwrap().id, a_id, "L:A keeps its set");
    assert_eq!(
        db.lock_set_of("L:B").unwrap().id,
        b_id,
        "the split-off side gets the freed set back, id and mutex intact"
    );
}

/// Retarget: one edit that is a split and a merge at once. The former target
/// leaves, the new one joins, and the record holding the link keeps its set.
#[epics_macros_rs::epics_test]
async fn retargeting_a_link_moves_the_old_target_out_and_the_new_one_in() {
    let db = ioc(RELINK_DB).await;
    dbpf(&db, "L:A", "INPA", "L:B").await;

    dbpf(&db, "L:A", "INPA", "L:C").await;

    let (members, free) = partition(&db);
    assert_eq!(
        members,
        vec![
            vec!["L:A".to_string(), "L:C".to_string()],
            vec!["L:B".to_string()]
        ]
    );
    assert_eq!(free, 1);
}

/// The relink is symmetric, because C's merge is: `dbLockSetMerge` puts both
/// endpoints behind one mutex whichever record's field was written, and
/// `dbLockSetSplit` walks `bklnk` as well as the record's own links.
#[epics_macros_rs::epics_test]
async fn a_link_written_on_the_target_side_merges_the_same_way() {
    let db = ioc(RELINK_DB).await;
    dbpf(&db, "L:B", "INPA", "L:A").await;

    let (members, free) = partition(&db);
    assert_eq!(
        members,
        vec![
            vec!["L:A".to_string(), "L:B".to_string()],
            vec!["L:C".to_string()]
        ]
    );
    assert_eq!(free, 1);
    assert_eq!(
        db.lock_set_of("L:B").unwrap().id,
        db.lock_set_of("L:A").unwrap().id
    );
}

/// The point of the whole facility, stated as locking rather than as a report:
/// two records joined at runtime must now serialise against each other, which
/// means ONE set is taken for the pair and not two.
#[epics_macros_rs::epics_test]
async fn records_joined_at_runtime_are_locked_as_one_set() {
    let db = ioc(RELINK_DB).await;
    {
        let _epoch = db.lock_records(["L:A", "L:B"]);
        assert_eq!(db.lock_set_of("L:A").unwrap().refs, 2, "1 member + 1 ref");
        assert_eq!(db.lock_set_of("L:B").unwrap().refs, 2, "its own set");
    }

    dbpf(&db, "L:A", "INPA", "L:B").await;

    let _epoch = db.lock_records(["L:A", "L:B"]);
    assert_eq!(
        db.lock_set_of("L:A").unwrap().refs,
        3,
        "two members plus ONE locked-list ref: dbScanLockMany saw one set, not \
         two, so a write to L:A and a write to L:B are now serialised"
    );
}

/// A link field written while the database is still loading merges nothing:
/// C reaches `dbLockSetMerge` only from `dbDbInitLink`/`dbDbAddLink`, and
/// neither can run before `dbLockInitRecords` has given the records a
/// `lockRecord` to merge.
///
/// It is the merge that must not happen, not the set: taking the write gate
/// for a record with no set still mints one here, which is this port's
/// deliberate deviation for a programmatic database (see `Registry::set_of`)
/// and is why `dbLoadRecords` alone still shows C's 0/0 — the loader writes
/// fields through the creation sink, not through the gate.
#[epics_macros_rs::epics_test]
async fn a_link_written_before_ioc_init_merges_nothing() {
    use epics_base_rs::server::records::calc::CalcRecord;

    let db = PvDatabase::new();
    for name in ["L:A", "L:B"] {
        db.add_record(name, Box::new(CalcRecord::new("A")))
            .await
            .unwrap();
    }
    db.put_pv_no_process("L:A.INPA", EpicsValue::String("L:B".into()))
        .await
        .unwrap();

    assert_eq!(partition(&db).1, 0, "no merge means nothing freed");
    assert_ne!(
        db.lock_set_of("L:A").map(|s| s.id),
        db.lock_set_of("L:B").map(|s| s.id),
        "the link text is there but the graph is not built yet"
    );

    db.ioc_init().await;
    assert_eq!(
        partition(&db).0,
        vec![vec!["L:A".to_string(), "L:B".to_string()]],
        "and the init pass then merges them, link text and all"
    );
}

/// Bypass regression — record deletion. C `dbDeleteRecord` frees the record's
/// `lockRecord`, so the set loses a member and what it was holding together
/// falls apart.
#[epics_macros_rs::epics_test]
async fn deleting_a_record_removes_it_from_its_set_and_splits_what_it_joined() {
    let db = ioc(r#"
record(calc, "D:A") { field(INPA, "D:MID") field(CALC, "A") }
record(calc, "D:MID") { field(CALC, "1") }
record(calc, "D:Z") { field(INPA, "D:MID") field(CALC, "A") }
"#)
    .await;
    assert_eq!(
        partition(&db).0,
        vec![vec![
            "D:A".to_string(),
            "D:MID".to_string(),
            "D:Z".to_string()
        ]],
        "D:MID joins all three"
    );

    assert!(db.remove_record("D:MID").await);

    // Sorted, because which component keeps the old id and which takes one off
    // the free list decides the listing order, and C has no `dbDeleteRecord`
    // after `iocInit` to pin that against. The grouping is the claim.
    let mut members = partition(&db).0;
    members.sort();
    assert_eq!(
        members,
        vec![vec!["D:A".to_string()], vec!["D:Z".to_string()]],
        "the record that joined them is gone, and so is its membership"
    );
    assert!(
        db.lock_set_of("D:MID").is_none(),
        "a deleted record must leave the partition, not keep a set alive"
    );
}

/// Bypass regression — a record created after `iocInit` gets a set of its own
/// and merges through its links, in both directions.
#[epics_macros_rs::epics_test]
async fn a_record_added_after_ioc_init_joins_the_set_its_links_reach() {
    use epics_base_rs::server::records::calc::CalcRecord;

    let db = ioc(RELINK_DB).await;
    db.add_record("L:NEW", Box::new(CalcRecord::new("A")))
        .await
        .unwrap();
    assert_eq!(
        partition(&db).0.len(),
        4,
        "its own set, as createLockRecord gives"
    );

    dbpf(&db, "L:NEW", "INPA", "L:A").await;
    assert_eq!(
        db.lock_set_of("L:NEW").unwrap().members,
        vec!["L:A".to_string(), "L:NEW".to_string()]
    );
}

/// Bypass regression — an alias. A link naming a name that is not yet an alias
/// resolves to nothing and merges nothing; registering the alias makes the
/// same link text reach a record, so the sets must merge then.
#[epics_macros_rs::epics_test]
async fn an_alias_registered_after_ioc_init_merges_the_link_that_names_it() {
    let db = ioc(r#"
record(calc, "A:SRC") { field(INPA, "A:NICK") field(CALC, "A") }
record(calc, "A:REAL") { field(CALC, "1") }
"#)
    .await;
    assert_eq!(
        partition(&db).0,
        vec![vec!["A:REAL".to_string()], vec!["A:SRC".to_string()]],
        "A:NICK names no record yet, so the link is not a DB link"
    );

    db.add_alias("A:NICK", "A:REAL").await.unwrap();

    assert_eq!(
        partition(&db).0,
        vec![vec!["A:REAL".to_string(), "A:SRC".to_string()]],
        "the alias made the link resolve, and C merges on a resolved DB link"
    );
}
