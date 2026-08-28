//! A link-backed field's metadata is read LIVE, at the moment it is served.
//!
//! C has no choice about this: `get_units` and its three siblings call
//! `dbGetUnits` / `dbGetPrecision` / `dbGetGraphicLimits` / `dbGetAlarmLimits`
//! from inside the rset, and `dbDbLink.c:240-261` takes the TARGET record's
//! lock around a single `dbGet` for the duration. It is legal there because
//! `dbLock.c:725-760` merges every DB_LINK-connected record into ONE lock set
//! behind one recursive mutex, so the source's lock and the target's lock ARE
//! the same mutex.
//!
//! The port has one `RwLock` per record and no lock sets, so the resolve must
//! happen where no record lock is held — `PvDatabase::snapshot_for_field` for
//! a served read, and one resolve per process/put cycle for the monitor
//! posters. It used to happen at `iocInit` and at the head of each process
//! cycle instead, into a per-record cache, which left a window C does not
//! have: a Passive source that nothing processes served the metadata its
//! target had at init, for as long as it lived.
//!
//! Measured against a C softIoc before the fix (two IOCs, same `.db`), after
//! `caput X2:AI.PREC 4` with the calc never processed:
//!
//! ```text
//! C:  X2:CALC.A   5.0000 UDF INVALID
//! R:  X2:CALC.A   5.0    UDF INVALID
//! ```

use epics_base_rs::server::database::{LinkBacking, PvDatabase};
use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::EpicsValue;

/// The target of every link below: `mm`, PREC 1, range ±10.
async fn add_source(db: &PvDatabase, name: &str) {
    let mut src = AiRecord::new(1.0);
    src.egu = "mm".into();
    src.prec = 1;
    src.hopr = 10.0;
    src.lopr = -10.0;
    db.add_record(name, Box::new(src)).await.unwrap();
}

/// A calc whose OWN metadata is deliberately different from its target's, so
/// serving the record instead of the link is a visible failure and not a
/// coincidence.
async fn add_calc(db: &PvDatabase, name: &str, inpa: &str) {
    let mut calc = CalcRecord::default();
    calc.egu = "V".into();
    calc.prec = 7;
    calc.hopr = 100.0;
    calc.lopr = -100.0;
    calc.inpa = inpa.into();
    db.add_record(name, Box::new(calc)).await.unwrap();
}

fn served(db: &PvDatabase, record: &str, field: &str) -> Snapshot {
    let rec = db.get_record(record).expect("record exists");
    db.channel_snapshot_for_field(&rec, field, false)
        .unwrap_or_else(|| panic!("{record}.{field} served no snapshot"))
}

fn units_of(snap: &Snapshot) -> String {
    snap.units()
        .map(|u| u.as_str_lossy().into_owned())
        .unwrap_or_else(|| panic!("no units leaf"))
}

/// **The finding.** A runtime change to the target's PREC reaches a client
/// reading the SOURCE's link-backed field, with the source never processed.
///
/// This is what the cache could not do: `CALC` is Passive and nothing scans
/// it, so before the fix the only two refresh points (`iocInit`, and the head
/// of a process cycle) had both already passed.
#[epics_macros_rs::epics_test]
async fn a_runtime_change_to_the_target_reaches_a_get_with_no_source_cycle() {
    let db = PvDatabase::new();
    add_source(&db, "SRC").await;
    add_calc(&db, "CALC", "SRC").await;
    db.ioc_init().await;

    assert_eq!(served(&db, "CALC", "A").precision(), Some(1));

    db.put_pv("SRC.PREC", EpicsValue::Short(4)).await.unwrap();
    db.put_pv("SRC.EGU", EpicsValue::String("kV".into()))
        .await
        .unwrap();

    let snap = served(&db, "CALC", "A");
    assert_eq!(
        snap.precision(),
        Some(4),
        "CALC.A must serve SRC's CURRENT precision; CALC has not processed"
    );
    assert_eq!(units_of(&snap), "kV");
    // Not the calc's own PREC=7 / EGU=V, which is the other way to be wrong.
}

/// The monitor half of the same root, on the path the wire measurement above
/// exercised: `caput CALC.A` posts a monitor, and that post carries the
/// target's precision as it is NOW.
#[epics_macros_rs::epics_test]
async fn a_put_posts_the_targets_current_precision() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    add_source(&db, "SRC").await;
    add_calc(&db, "CALC", "SRC").await;
    db.ioc_init().await;

    let rec = db.get_record("CALC").expect("record exists");
    let mut rx = rec
        .write()
        .add_subscriber("A", 1, DbFieldType::Double, EventMask::VALUE.bits())
        .expect("subscriber");

    db.put_pv("SRC.PREC", EpicsValue::Short(4)).await.unwrap();
    db.put_pv("CALC.A", EpicsValue::Double(5.0)).await.unwrap();

    let event = rx.try_recv().expect("the put posts A");
    assert_eq!(
        event.snapshot.precision(),
        Some(4),
        "the posted snapshot must carry SRC's current precision, not the one \
         cached when CALC last processed (measured: 5.0 where C gives 5.0000)"
    );
}

/// A link naming a record that does not exist resolves to nothing, and every
/// slot falls back to its own C seed — `dbAccess.c:376-385` (`""`),
/// `:386-394` (the record's own PREC, seeded before the fetch), `:241-242`
/// (0/0). Not a panic and not the last value ever seen.
#[epics_macros_rs::epics_test]
async fn an_absent_target_serves_the_dbaccess_seeds() {
    let db = PvDatabase::new();
    add_calc(&db, "CALC", "NO:SUCH:RECORD").await;
    db.ioc_init().await;

    let snap = served(&db, "CALC", "A");
    assert_eq!(units_of(&snap), "", "get_units memsets its buffer");
    assert_eq!(
        snap.precision(),
        Some(7),
        "get_precision seeds *pprecision = prec->prec BEFORE the fetch"
    );
    assert_eq!(snap.display_limits(), Some((0.0, 0.0)));
}

/// A link pointing back at the field it backs. C's guard is one flag per link
/// (`DBLINK_FLAG_VISITED`, `dbDbLink.c:253-257`) set across the inner fetch;
/// the re-entry returns without touching the buffer, so the seeds stand. The
/// port carries the same guard as a visited set through the resolve.
#[epics_macros_rs::epics_test]
async fn a_self_link_stops_at_the_visited_guard() {
    let db = PvDatabase::new();
    add_calc(&db, "CALC", "CALC.A").await;
    db.ioc_init().await;

    let snap = served(&db, "CALC", "A");
    assert_eq!(units_of(&snap), "");
    assert_eq!(snap.precision(), Some(7));
}

/// Two records whose link-backed fields point at each other, read
/// concurrently from two threads in opposite order. The resolve holds no
/// record lock, which is the whole reason this cannot deadlock; the guard is
/// what makes it terminate.
#[epics_macros_rs::epics_test]
async fn mutually_linked_records_resolve_concurrently_without_deadlock() {
    let db = std::sync::Arc::new(PvDatabase::new());
    add_calc(&db, "A:CALC", "B:CALC.A").await;
    add_calc(&db, "B:CALC", "A:CALC.A").await;
    db.ioc_init().await;

    let one = {
        let db = db.clone();
        std::thread::spawn(move || {
            for _ in 0..200 {
                let _ = served(&db, "A:CALC", "A");
            }
        })
    };
    let two = {
        let db = db.clone();
        std::thread::spawn(move || {
            for _ in 0..200 {
                let _ = served(&db, "B:CALC", "A");
            }
        })
    };
    one.join().expect("A-side reader finished");
    two.join().expect("B-side reader finished");
}

/// A two-hop chain: `TAIL.A` <- `HEAD.A` <- `SRC`. C recurses the same way —
/// the inner `dbGet` on `HEAD.A` runs `calcRecord`'s rset, which routes
/// through `HEAD`'s own `INPA` — and the visited guard stops a cycle without
/// stopping a chain.
#[epics_macros_rs::epics_test]
async fn a_two_hop_chain_resolves_through_both_links() {
    let db = PvDatabase::new();
    add_source(&db, "SRC").await;
    add_calc(&db, "HEAD", "SRC").await;
    add_calc(&db, "TAIL", "HEAD.A").await;
    db.ioc_init().await;

    let snap = served(&db, "TAIL", "A");
    assert_eq!(units_of(&snap), "mm", "both hops, not TAIL's own V");
    assert_eq!(snap.precision(), Some(1));

    // And the live property survives the second hop.
    db.put_pv("SRC.PREC", EpicsValue::Short(4)).await.unwrap();
    assert_eq!(served(&db, "TAIL", "A").precision(), Some(4));
}

/// A field no link backs is untouched by any of this: `CALC.VAL` serves the
/// calc's own EGU/PREC, whatever `INPA` points at (`calcRecord.c:172-181`,
/// the `else` arm a link-backed field never takes).
#[epics_macros_rs::epics_test]
async fn a_field_no_link_backs_still_serves_the_record() {
    let db = PvDatabase::new();
    add_source(&db, "SRC").await;
    add_calc(&db, "CALC", "SRC").await;
    db.ioc_init().await;

    let snap = served(&db, "CALC", "VAL");
    assert_eq!(units_of(&snap), "V");
    assert_eq!(snap.precision(), Some(7));
}

/// The structural gate. `RecordInstance` cannot serve a link-backed field at
/// all: it has no way to reach the target's lock, so the answer is `None`
/// rather than a plausible wrong number. The only way to a value is
/// `PvDatabase`, which resolves first — or the explicit
/// `LinkBacking::none()`, which says "nothing was resolved" and gets the C
/// seeds.
#[test]
fn the_record_alone_cannot_serve_a_link_backed_field() {
    let mut calc = CalcRecord::default();
    calc.prec = 7;
    calc.inpa = "SRC".into();
    let inst = RecordInstance::new_boxed("T:CALC".to_string(), Box::new(calc));

    assert!(
        inst.snapshot_for_field("A").is_none(),
        "a link-backed field has no record-only answer"
    );
    assert!(
        inst.snapshot_for_field("VAL").is_some(),
        "every other field is unaffected"
    );
    assert_eq!(
        inst.snapshot_for_field_with("A", LinkBacking::none())
            .expect("the seeds are still a snapshot")
            .precision(),
        Some(7)
    );
}

/// The `DBE_PROPERTY` sweep, which is a THIRD poster and reaches the same
/// consumer the other two do.
///
/// C's `dbPut` tail posts `DBE_PROPERTY` to every monitor on the record —
/// `db_post_events(precord, NULL, DBE_PROPERTY)`, `dbAccess.c:1395-1396` — so
/// a `caput CALC.PREC` (prop(YES), `calcRecord.dbd.pod:689-693`) reaches a
/// client monitoring `CALC.A`. That client's snapshot is a link-backed one,
/// and only a resolved backing can fill it: `CALC`'s own PREC is exactly the
/// wrong answer, and it is the answer an unresolved post gives.
#[epics_macros_rs::epics_test]
async fn the_property_sweep_posts_a_link_backed_field_with_its_targets_metadata() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    add_source(&db, "SRC").await;
    add_calc(&db, "CALC", "SRC").await;
    db.ioc_init().await;

    let rec = db.get_record("CALC").expect("record exists");
    let mut rx = rec
        .write()
        .add_subscriber("A", 1, DbFieldType::Double, EventMask::PROPERTY.bits())
        .expect("subscriber");

    db.put_pv("CALC.PREC", EpicsValue::Short(3)).await.unwrap();

    let event = rx.try_recv().expect("the property sweep posts A");
    assert_eq!(
        event.snapshot.precision(),
        Some(1),
        "A is link-backed: the sweep must post SRC's precision, never CALC's own"
    );
}
