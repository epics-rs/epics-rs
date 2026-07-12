//! R9-76 — swait INAV..INLV / DOLV / OUTV, the `menu(swaitINAV)` PV status of
//! each link, and the `if (!dolv)` gate on the DOL fetch.
//!
//! C `swaitRecord.dbd:166-250` declares fourteen `DBF_MENU`/`SPC_NOMOD` status
//! fields — one per link, in the same order as the names INAN..INLN/DOLN/OUTN.
//! `init_record` (swaitRecord.c:338-373) sets `NO_PV` for a blank name and
//! `PV_NC` for any other, then `pvSearchCallback` (900-928) flips a connected
//! one to `PV_OK`. The port had none of the fields: a client read of INAV..OUTV
//! was a `FieldNotFound`.
//!
//! `execOutput` also reads DOL only `if (!pwait->dolv)` (:765). That guard's
//! observable effect — an unset or unresolvable DOL leaves DOLD alone, and C
//! writes the stale DOLD to OUT anyway — is what the framework's output-time
//! fetch already produces by dropping a failed read, so the port does not gate
//! on DOLV; see `r9_76_dol_fetch_does_not_wait_for_the_classification_task` for
//! why gating on it would be wrong.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

const PV_OK: u16 = 0;
const PV_NC: u16 = 1;
const NO_PV: u16 = 2;

async fn field(db: &PvDatabase, rec: &str, f: &str) -> Option<EpicsValue> {
    let inst = db.get_record(rec).await.unwrap();
    let g = inst.read().await;
    g.record.get_field(f)
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// Give the spawned classification task a turn to publish.
async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

/// A resolvable name is PV_OK, an unresolvable one is PV_NC, a blank one is
/// NO_PV — C's three states, one per link.
#[tokio::test]
async fn r9_76_pv_status_classifies_each_link() {
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
    settle().await;

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

/// A runtime re-point re-runs the search: C `special()` (swaitRecord.c:507-553)
/// re-issues `recDynLinkAddInput` for the new name and clears to NO_PV when the
/// name is emptied.
#[tokio::test]
async fn r9_76_put_to_a_pv_name_reclassifies_it() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AiRecord::new(7.0)))
        .await
        .unwrap();
    db.add_record("W", Box::new(SwaitRecord::default()))
        .await
        .unwrap();
    settle().await;
    assert_eq!(
        field(&db, "W", "INAV").await,
        Some(EpicsValue::Enum(NO_PV)),
        "blank INAN starts at NO_PV"
    );

    db.put_pv("W.INAN", EpicsValue::String("SRC".into()))
        .await
        .unwrap();
    settle().await;
    assert_eq!(
        field(&db, "W", "INAV").await,
        Some(EpicsValue::Enum(PV_OK)),
        "the re-pointed INAN now resolves"
    );

    db.put_pv("W.INAN", EpicsValue::String("".into()))
        .await
        .unwrap();
    settle().await;
    assert_eq!(
        field(&db, "W", "INAV").await,
        Some(EpicsValue::Enum(NO_PV)),
        "a cleared name goes back to NO_PV"
    );
}

/// DOPT="Use DOL" with a DOL that did NOT connect (PV_NC): C skips the
/// `recDynLinkGet` (`if (!pwait->dolv)`), so DOLD keeps its client-put value —
/// and that stale DOLD is what C writes to OUT. The port reaches the same
/// observable state through the failed read (the framework writes DOLD only on
/// a successful fetch), so DOLV reports the bad status without gating on it.
#[tokio::test]
async fn r9_76_unresolvable_dol_keeps_dold_and_still_writes_it() {
    let db = PvDatabase::new();
    db.add_record("SINK", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("1".into())).unwrap();
    w.put_field("DOPT", EpicsValue::Short(1)).unwrap(); // Use DOL
    w.put_field("DOLN", EpicsValue::String("NOSUCHREC".into()))
        .unwrap();
    w.put_field("DOLD", EpicsValue::Double(42.0)).unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();
    // OUT routes through RecordInstance::put_common_field (populates parsed_out).
    db.get_record("W")
        .await
        .unwrap()
        .write()
        .await
        .put_common_field("OUT", EpicsValue::String("SINK".into()))
        .unwrap();
    settle().await;

    assert_eq!(
        field(&db, "W", "DOLV").await,
        Some(EpicsValue::Enum(PV_NC)),
        "DOLN names a record that does not exist"
    );

    process(&db, "W").await;

    assert_eq!(
        field(&db, "W", "DOLD").await,
        Some(EpicsValue::Double(42.0)),
        "the DOL name does not resolve — DOLD keeps its put value, as under C's !dolv"
    );
    assert_eq!(
        field(&db, "SINK", "VAL").await,
        Some(EpicsValue::Double(42.0)),
        "DOPT=Use DOL still writes DOLD to OUT — the missing PV suppresses the fetch, not the write"
    );
}

/// The other side: a DOL that DOES connect is fetched, and the fetched value is
/// what reaches OUT.
#[tokio::test]
async fn r9_76_connected_dol_is_fetched_at_output_time() {
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
    w.put_field("DOLD", EpicsValue::Double(42.0)).unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();
    // OUT routes through RecordInstance::put_common_field (populates parsed_out).
    db.get_record("W")
        .await
        .unwrap()
        .write()
        .await
        .put_common_field("OUT", EpicsValue::String("SINK".into()))
        .unwrap();
    settle().await;

    assert_eq!(
        field(&db, "W", "DOLV").await,
        Some(EpicsValue::Enum(PV_OK)),
        "DOLSRC resolves"
    );

    process(&db, "W").await;

    assert_eq!(
        field(&db, "W", "DOLD").await,
        Some(EpicsValue::Double(5.0)),
        "PV_OK — C runs the DOL get and DOLD takes the link's value"
    );
    assert_eq!(
        field(&db, "SINK", "VAL").await,
        Some(EpicsValue::Double(5.0)),
        "the freshly fetched DOLD is written to OUT"
    );
}

/// The status fields are published by a spawned classification task, so they
/// are not a gate: a cycle that runs BEFORE that task lands (no `settle()`
/// here, DOLV still reads NO_PV) must still fetch a DOL that resolves. Gating
/// the fetch on `DOLV == PV_OK` made this cycle skip a get C performs.
#[tokio::test]
async fn r9_76_dol_fetch_does_not_wait_for_the_classification_task() {
    let db = PvDatabase::new();
    db.add_record("DOLSRC", Box::new(AiRecord::new(7.0)))
        .await
        .unwrap();

    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("1".into())).unwrap();
    w.put_field("DOPT", EpicsValue::Short(1)).unwrap();
    w.put_field("DOLN", EpicsValue::String("DOLSRC".into()))
        .unwrap();
    w.put_field("DOLD", EpicsValue::Double(42.0)).unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();

    // No settle(): process on the heels of add_record, while DOLV is still the
    // init NO_PV.
    assert_eq!(
        field(&db, "W", "DOLV").await,
        Some(EpicsValue::Enum(NO_PV)),
        "the classification has not published yet"
    );

    process(&db, "W").await;

    assert_eq!(
        field(&db, "W", "DOLD").await,
        Some(EpicsValue::Double(7.0)),
        "the DOL resolves, so the get runs — regardless of what DOLV reads yet"
    );
}
