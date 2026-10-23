//! R9-76 — swait INAV..INLV / DOLV / OUTV, the `menu(swaitINAV)` PV status of
//! each link, and the `if (!dolv)` gate on the DOL fetch.
//!
//! C `swaitRecord.dbd:166-250` declares fourteen `DBF_MENU`/`SPC_NOMOD` status
//! fields — one per link, in the same order as the names INAN..INLN/DOLN/OUTN.
//! `init_record` (swaitRecord.c:338-373) sets `NO_PV` for a blank name and
//! `PV_NC` for any other, then `pvSearchCallback` (901-926) flips a connected
//! one to `PV_OK`. The port had none of the fields: a client read of INAV..OUTV
//! was a `FieldNotFound`.
//!
//! `execOutput` also reads DOL only `if (!pwait->dolv)` (:764). DOLV is the DOL
//! `recDynLink`'s own connection state, so an unset or unconnected DOL leaves
//! DOLD alone, and C writes the stale DOLD to OUT anyway.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

const PV_OK: u16 = 0;
const PV_NC: u16 = 1;
const NO_PV: u16 = 2;

async fn field(db: &PvDatabase, rec: &str, f: &str) -> Option<EpicsValue> {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field(f)
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

/// Wait until `rec.f` reads `want`: each link searches on its own task and
/// its status lands from a callback band.
async fn settle_until(db: &PvDatabase, rec: &str, f: &str, want: u16) {
    for _ in 0..400 {
        if field(db, rec, f).await == Some(EpicsValue::Enum(want)) {
            return;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!(
        "{rec}.{f} did not reach {want} before timeout (last {:?})",
        field(db, rec, f).await
    );
}

/// A resolvable name is PV_OK, an unresolvable one is PV_NC, a blank one is
/// NO_PV — C's three states, one per link.
#[epics_macros_rs::epics_test]
async fn r9_76_pv_status_classifies_each_link() {
    // C `callbackInit`: the links' events are applied on a callback band.
    epics_base_rs::runtime::task::background_init();
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AiRecord::new(7.0)))
        .await
        .unwrap();

    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    // INAN: resolves on this IOC. INBN: no such record. INCN: left blank.
    w.put_field("INAN", EpicsValue::String("SRC".into()))
        .unwrap();
    w.put_field("INBN", EpicsValue::String("NOSUCHREC".into()))
        .unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();
    settle_until(&db, "W", "INAV", PV_OK).await;

    assert_eq!(
        field(&db, "W", "INAV").await,
        Some(EpicsValue::Enum(PV_OK)),
        "INAN resolves to a local record — C's search connects, PV_OK"
    );
    assert_eq!(
        field(&db, "W", "INBV").await,
        Some(EpicsValue::Enum(PV_NC)),
        "INBN names a record that does not exist — C leaves it PV_NC"
    );
    assert_eq!(
        field(&db, "W", "INCV").await,
        Some(EpicsValue::Enum(NO_PV)),
        "INCN is blank — C `init_record` sets NO_PV"
    );
    assert_eq!(
        field(&db, "W", "DOLV").await,
        Some(EpicsValue::Enum(NO_PV)),
        "DOLN is blank — NO_PV"
    );
    assert_eq!(
        field(&db, "W", "OUTV").await,
        Some(EpicsValue::Enum(NO_PV)),
        "OUTN is blank — NO_PV"
    );
}

/// A runtime re-point re-runs the search: C `special()` (swaitRecord.c:507-559)
/// re-issues `recDynLinkAddInput` for the new name and clears to NO_PV when the
/// name is emptied.
#[epics_macros_rs::epics_test]
async fn r9_76_put_to_a_pv_name_reclassifies_it() {
    // C `callbackInit`: the links' events are applied on a callback band.
    epics_base_rs::runtime::task::background_init();
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AiRecord::new(7.0)))
        .await
        .unwrap();
    db.add_record("W", Box::new(SwaitRecord::default()))
        .await
        .unwrap();
    assert_eq!(
        field(&db, "W", "INAV").await,
        Some(EpicsValue::Enum(NO_PV)),
        "blank INAN starts at NO_PV"
    );

    db.put_pv("W.INAN", EpicsValue::String("SRC".into()))
        .await
        .unwrap();
    settle_until(&db, "W", "INAV", PV_OK).await;
    assert_eq!(
        field(&db, "W", "INAV").await,
        Some(EpicsValue::Enum(PV_OK)),
        "the re-pointed INAN now resolves"
    );

    db.put_pv("W.INAN", EpicsValue::String("".into()))
        .await
        .unwrap();
    settle_until(&db, "W", "INAV", NO_PV).await;
    assert_eq!(
        field(&db, "W", "INAV").await,
        Some(EpicsValue::Enum(NO_PV)),
        "a cleared name goes back to NO_PV"
    );
}

/// Wait until `rec.f` reads `want` — the OUT write is a CA put.
async fn settle_val(db: &PvDatabase, rec: &str, f: &str, want: f64) {
    for _ in 0..400 {
        if field(db, rec, f).await == Some(EpicsValue::Double(want)) {
            return;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!(
        "{rec}.{f} did not reach {want} before timeout (last {:?})",
        field(db, rec, f).await
    );
}

/// Process `rec` until `target.VAL` reads `want`: a link's value lands on its
/// first monitor, after its status reads PV_OK, and shows nowhere else.
async fn process_until(db: &PvDatabase, rec: &str, target: &str, want: f64) {
    for _ in 0..400 {
        process(db, rec).await;
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
        if field(db, target, "VAL").await == Some(EpicsValue::Double(want)) {
            return;
        }
    }
    panic!(
        "{target}.VAL did not reach {want} before timeout (last {:?})",
        field(db, target, "VAL").await
    );
}

/// DOPT="Use DOL" with a DOL that did NOT connect (PV_NC): C skips the
/// `recDynLinkGet` (`if (!pwait->dolv)`), so DOLD keeps its client-put value —
/// and that stale DOLD is what C writes to OUT.
#[epics_macros_rs::epics_test]
async fn r9_76_unresolvable_dol_keeps_dold_and_still_writes_it() {
    // C `callbackInit`: the links' events are applied on a callback band.
    epics_base_rs::runtime::task::background_init();
    let db = PvDatabase::new();
    db.add_record("SINK", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("1".into())).unwrap();
    w.put_field("DOPT", EpicsValue::Short(1)).unwrap(); // Use DOL
    w.put_field("DOLN", EpicsValue::String("NOSUCHREC".into()))
        .unwrap();
    w.put_field("OUTN", EpicsValue::String("SINK".into()))
        .unwrap();
    w.put_field("DOLD", EpicsValue::Double(42.0)).unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();
    // `init_record` adds the DOL and OUT links.
    db.ioc_init().await;
    settle_until(&db, "W", "OUTV", PV_OK).await;

    assert_eq!(
        field(&db, "W", "DOLV").await,
        Some(EpicsValue::Enum(PV_NC)),
        "DOLN names a record that does not exist"
    );

    process(&db, "W").await;

    // DOPT=Use DOL still writes DOLD to OUT — the missing PV suppresses the
    // fetch, not the write.
    settle_val(&db, "SINK", "VAL", 42.0).await;
    assert_eq!(
        field(&db, "W", "DOLD").await,
        Some(EpicsValue::Double(42.0)),
        "the DOL name does not resolve — DOLD keeps its put value, as under C's !dolv"
    );
}

/// The other side: a DOL that DOES connect is fetched, and the fetched value is
/// what reaches OUT.
#[epics_macros_rs::epics_test]
async fn r9_76_connected_dol_is_fetched_at_output_time() {
    // C `callbackInit`: the links' events are applied on a callback band.
    epics_base_rs::runtime::task::background_init();
    let db = PvDatabase::new();
    db.add_record("DOLSRC", Box::new(AiRecord::new(5.0)))
        .await
        .unwrap();
    db.add_record("SINK", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("1".into())).unwrap();
    w.put_field("DOPT", EpicsValue::Short(1)).unwrap();
    w.put_field("DOLN", EpicsValue::String("DOLSRC".into()))
        .unwrap();
    w.put_field("OUTN", EpicsValue::String("SINK".into()))
        .unwrap();
    w.put_field("DOLD", EpicsValue::Double(42.0)).unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();
    db.ioc_init().await;
    settle_until(&db, "W", "OUTV", PV_OK).await;
    settle_until(&db, "W", "DOLV", PV_OK).await;
    assert_eq!(
        field(&db, "W", "DOLV").await,
        Some(EpicsValue::Enum(PV_OK)),
        "DOLSRC resolves"
    );

    // The freshly fetched DOLD is written to OUT.
    process_until(&db, "W", "SINK", 5.0).await;
    assert_eq!(
        field(&db, "W", "DOLD").await,
        Some(EpicsValue::Double(5.0)),
        "PV_OK — C runs the DOL get and DOLD takes the link's value"
    );
}
