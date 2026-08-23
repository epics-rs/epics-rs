//! B2 — a `caput` to SNAM must rebind the live subroutine, not just store
//! the name.
//!
//! SNAM is `special(SPC_MOD)` (`aSubRecord.dbd.pod:430-436`), so `dbPutSpecial`
//! runs `special()` on every put. C `aSubRecord.c::special` (552-578):
//!
//! ```c
//! if (prec->snam[0] == 0)
//!     pfunc = 0;
//! else {
//!     pfunc = (GENFUNCPTR)registryFunctionFind(prec->snam);
//!     if (!pfunc) { status = S_db_BadSub; recGblRecordError(...); }
//! }
//! if (prec->sadr != pfunc && prec->cadr) { prec->cadr(prec); prec->cadr = NULL; }
//! prec->sadr = pfunc;
//! ```
//!
//! and `subRecord.c::special` (182-194) is the same rebind
//! (`prec->sadr = registryFunctionFind(prec->snam)`).
//!
//! The assignment is UNCONDITIONAL: an empty name and an unregistered name
//! both leave `sadr` NULL, and only the returned status differs. The port
//! stored the name and never touched `RecordInstance::subroutine`, so
//! `caput X.SNAM fnB` read back `fnB` while the record kept executing `fnA`
//! forever, and `caput X.SNAM ""` was inert.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{Record, SubroutineFn};
use epics_base_rs::server::records::asub_record::ASubRecord;
use epics_base_rs::server::records::sub_record::SubRecord;
use epics_base_rs::types::EpicsValue;

/// An aSub subroutine that stamps `marker` into VALA, so VALA names which
/// routine actually ran.
fn stamps(marker: f64) -> Arc<SubroutineFn> {
    Arc::new(Box::new(move |rec: &mut dyn Record| {
        rec.put_field("VALA", EpicsValue::Double(marker))?;
        Ok(0_i64)
    }) as SubroutineFn)
}

/// A `sub` subroutine that writes `marker` into VAL.
fn writes_val(marker: f64) -> Arc<SubroutineFn> {
    Arc::new(Box::new(move |rec: &mut dyn Record| {
        rec.put_field("VAL", EpicsValue::Double(marker))?;
        Ok(0_i64)
    }) as SubroutineFn)
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

async fn field(db: &PvDatabase, rec: &str, f: &str) -> Option<EpicsValue> {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field(f)
}

/// `fnA` and `fnB` both registered; a put swapping SNAM from `fnA` to `fnB`
/// must make the next process run `fnB`.
#[epics_macros_rs::epics_test]
async fn asub_snam_put_swaps_the_running_subroutine() {
    let db = PvDatabase::new();
    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert("fnA".into(), stamps(1.0));
    registry.insert("fnB".into(), stamps(2.0));
    db.install_subroutine_registry(registry.clone()).await;

    let mut seed = ASubRecord::default();
    seed.put_field("SNAM", EpicsValue::String("fnA".into()))
        .unwrap();
    db.add_record("X", Box::new(seed)).await.unwrap();
    // Bind as iocInit would.
    db.get_record("X").unwrap().write().subroutine = registry.get("fnA").cloned();

    process(&db, "X").await;
    assert_eq!(
        field(&db, "X", "VALA").await,
        Some(EpicsValue::Double(1.0)),
        "fnA is bound"
    );

    db.put_record_field_from_ca("X", "SNAM", EpicsValue::String("fnB".into()))
        .await
        .expect("fnB is registered, so the put succeeds");
    assert_eq!(
        field(&db, "X", "SNAM").await,
        Some(EpicsValue::String("fnB".into()))
    );

    process(&db, "X").await;
    assert_eq!(
        field(&db, "X", "VALA").await,
        Some(EpicsValue::Double(2.0)),
        "C special() assigned prec->sadr = fnB — the record must now run fnB"
    );
}

/// `caput X.SNAM ""` — C resolves an empty name to `pfunc = 0` and assigns it,
/// so the put succeeds and the record stops calling anything. aSub's `do_sub`
/// then short-circuits on the empty SNAM and returns 0.
#[epics_macros_rs::epics_test]
async fn asub_empty_snam_put_unbinds_the_subroutine() {
    let db = PvDatabase::new();
    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert("fnA".into(), stamps(1.0));
    db.install_subroutine_registry(registry.clone()).await;

    let mut seed = ASubRecord::default();
    seed.put_field("SNAM", EpicsValue::String("fnA".into()))
        .unwrap();
    db.add_record("X", Box::new(seed)).await.unwrap();
    db.get_record("X").unwrap().write().subroutine = registry.get("fnA").cloned();

    process(&db, "X").await;
    assert_eq!(field(&db, "X", "VALA").await, Some(EpicsValue::Double(1.0)));

    db.put_record_field_from_ca("X", "SNAM", EpicsValue::String("".into()))
        .await
        .expect("an empty name is pfunc = 0 with no error");

    // VALA must not move again: nothing runs.
    db.get_record("X")
        .unwrap()
        .write()
        .record
        .put_field("VALA", EpicsValue::Double(0.0))
        .unwrap();
    process(&db, "X").await;
    assert_eq!(
        field(&db, "X", "VALA").await,
        Some(EpicsValue::Double(0.0)),
        "C assigned sadr = 0, so do_sub calls nothing"
    );
}

/// An unregistered name is refused (`S_db_BadSub` → `ECA_PUTFAIL`), but C
/// assigns `prec->sadr = pfunc` BEFORE returning that status — the old routine
/// is unbound all the same, and the stored name is the new one.
#[epics_macros_rs::epics_test]
async fn asub_rejected_snam_put_still_unbinds_and_still_stores() {
    let db = PvDatabase::new();
    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert("fnA".into(), stamps(1.0));
    db.install_subroutine_registry(registry.clone()).await;

    let mut seed = ASubRecord::default();
    seed.put_field("SNAM", EpicsValue::String("fnA".into()))
        .unwrap();
    db.add_record("X", Box::new(seed)).await.unwrap();
    db.get_record("X").unwrap().write().subroutine = registry.get("fnA").cloned();

    process(&db, "X").await;
    assert_eq!(field(&db, "X", "VALA").await, Some(EpicsValue::Double(1.0)));

    db.put_record_field_from_ca("X", "SNAM", EpicsValue::String("nope".into()))
        .await
        .expect_err("special() returns S_db_BadSub for an unregistered name");
    assert_eq!(
        field(&db, "X", "SNAM").await,
        Some(EpicsValue::String("nope".into())),
        "C stores prec->snam before special() runs and never rolls it back"
    );

    db.get_record("X")
        .unwrap()
        .write()
        .record
        .put_field("VALA", EpicsValue::Double(0.0))
        .unwrap();
    process(&db, "X").await;
    assert_eq!(
        field(&db, "X", "VALA").await,
        Some(EpicsValue::Double(0.0)),
        "sadr = NULL was assigned before the status was returned"
    );
}

/// `subRecord.c::special` performs the identical rebind, so a `sub` SNAM put
/// must swap its routine too.
#[epics_macros_rs::epics_test]
async fn sub_snam_put_swaps_the_running_subroutine() {
    let db = PvDatabase::new();
    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert("fnA".into(), writes_val(1.0));
    registry.insert("fnB".into(), writes_val(2.0));
    db.install_subroutine_registry(registry.clone()).await;

    let mut seed = SubRecord::default();
    seed.put_field("SNAM", EpicsValue::String("fnA".into()))
        .unwrap();
    db.add_record("Y", Box::new(seed)).await.unwrap();
    db.get_record("Y").unwrap().write().subroutine = registry.get("fnA").cloned();

    process(&db, "Y").await;
    assert_eq!(field(&db, "Y", "VAL").await, Some(EpicsValue::Double(1.0)));

    db.put_record_field_from_ca("Y", "SNAM", EpicsValue::String("fnB".into()))
        .await
        .expect("fnB is registered");
    process(&db, "Y").await;
    assert_eq!(
        field(&db, "Y", "VAL").await,
        Some(EpicsValue::Double(2.0)),
        "subRecord.c:188: prec->sadr = registryFunctionFind(prec->snam)"
    );
}

/// The rebind is `dbPutSpecial(paddr, 1)`, which `dbPut` runs on EVERY entry
/// path — `dbPutField` for CA/`dbpf`, `dbPutLink` for an OUT link, and a bare
/// internal `dbPut`. The port ran it on the CA route only, so `put_pv` — the
/// `dbPut` analogue that every OUT link and every internal writer goes through
/// — stored an unregistered SNAM, returned success where C returns
/// `S_db_BadSub`, and left the previous routine bound and running.
#[epics_macros_rs::epics_test]
async fn internal_put_of_an_unregistered_snam_is_refused_and_unbinds() {
    let db = PvDatabase::new();
    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert("fnA".into(), stamps(1.0));
    db.install_subroutine_registry(registry.clone()).await;

    let mut seed = ASubRecord::default();
    seed.put_field("SNAM", EpicsValue::String("fnA".into()))
        .unwrap();
    db.add_record("X", Box::new(seed)).await.unwrap();
    db.get_record("X").unwrap().write().subroutine = registry.get("fnA").cloned();

    process(&db, "X").await;
    assert_eq!(field(&db, "X", "VALA").await, Some(EpicsValue::Double(1.0)));

    db.put_pv("X.SNAM", EpicsValue::String("nope".into()))
        .await
        .expect_err("dbPut returns special()'s S_db_BadSub on every entry path");
    assert_eq!(
        field(&db, "X", "SNAM").await,
        Some(EpicsValue::String("nope".into())),
        "the name is stored before special() runs and is never rolled back"
    );

    db.get_record("X")
        .unwrap()
        .write()
        .record
        .put_field("VALA", EpicsValue::Double(0.0))
        .unwrap();
    process(&db, "X").await;
    assert_eq!(
        field(&db, "X", "VALA").await,
        Some(EpicsValue::Double(0.0)),
        "sadr = NULL was assigned before the status was returned"
    );
}

/// `put_pv_and_post` is the third `dbPut` route and takes the same owner.
#[epics_macros_rs::epics_test]
async fn posting_internal_put_of_an_unregistered_snam_is_refused_and_unbinds() {
    let db = PvDatabase::new();
    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert("fnA".into(), writes_val(1.0));
    db.install_subroutine_registry(registry.clone()).await;

    let mut seed = SubRecord::default();
    seed.put_field("SNAM", EpicsValue::String("fnA".into()))
        .unwrap();
    db.add_record("Y", Box::new(seed)).await.unwrap();
    db.get_record("Y").unwrap().write().subroutine = registry.get("fnA").cloned();

    process(&db, "Y").await;
    assert_eq!(field(&db, "Y", "VAL").await, Some(EpicsValue::Double(1.0)));

    db.put_pv_and_post("Y.SNAM", EpicsValue::String("nope".into()))
        .await
        .expect_err("subRecord.c:193 returns S_db_BadSub");

    db.get_record("Y")
        .unwrap()
        .write()
        .record
        .put_field("VAL", EpicsValue::Double(0.0))
        .unwrap();
    process(&db, "Y").await;
    assert_eq!(
        field(&db, "Y", "VAL").await,
        Some(EpicsValue::Double(0.0)),
        "subRecord.c:188 assigned sadr = NULL before returning the status"
    );
}

/// An internal put of a REGISTERED name still succeeds and still rebinds, so
/// the refusal above is the status and not a blanket rejection of the route.
#[epics_macros_rs::epics_test]
async fn internal_put_of_a_registered_snam_swaps_the_routine() {
    let db = PvDatabase::new();
    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert("fnA".into(), stamps(1.0));
    registry.insert("fnB".into(), stamps(2.0));
    db.install_subroutine_registry(registry.clone()).await;

    let mut seed = ASubRecord::default();
    seed.put_field("SNAM", EpicsValue::String("fnA".into()))
        .unwrap();
    db.add_record("X", Box::new(seed)).await.unwrap();
    db.get_record("X").unwrap().write().subroutine = registry.get("fnA").cloned();

    db.put_pv("X.SNAM", EpicsValue::String("fnB".into()))
        .await
        .expect("fnB is registered");
    process(&db, "X").await;
    assert_eq!(field(&db, "X", "VALA").await, Some(EpicsValue::Double(2.0)));
}

/// C resolves `prec->snam` AFTER the store, so the name looked up is the one
/// `putStringString` left in the field: sub's SNAM is `size(40)`, so a 45-byte
/// put is truncated to 39 bytes and it is THAT name `registryFunctionFind`
/// sees. Resolving the value the caller sent instead refuses a put C accepts,
/// and would bind a routine whose name the record does not hold. Measured on
/// this port: sub stores 39 bytes, aSub 40, matching their `size(40)`/`size(41)`.
#[epics_macros_rs::epics_test]
async fn snam_resolves_the_stored_name_not_the_one_the_caller_sent() {
    let long_name = format!("fn_{}", "x".repeat(42));
    assert_eq!(long_name.len(), 45);
    let stored_name: String = long_name.chars().take(39).collect();

    let db = PvDatabase::new();
    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert("seed".into(), writes_val(1.0));
    registry.insert(stored_name.clone(), writes_val(7.0));
    db.install_subroutine_registry(registry.clone()).await;

    // A `sub` born with an empty SNAM is PACT-parked at init
    // (`subRecord.c:119-123`), which defers every put; seed a real name so this
    // case is about the truncation and nothing else.
    let mut seed = SubRecord::default();
    seed.put_field("SNAM", EpicsValue::String("seed".into()))
        .unwrap();
    db.add_record("Y", Box::new(seed)).await.unwrap();
    db.get_record("Y").unwrap().write().subroutine = registry.get("seed").cloned();

    db.put_record_field_from_ca("Y", "SNAM", EpicsValue::String(long_name.as_str().into()))
        .await
        .expect("the TRUNCATED name is registered, so C's special() finds it");
    assert_eq!(
        field(&db, "Y", "SNAM").await,
        Some(EpicsValue::String(stored_name.as_str().into())),
        "putStringString caps a size(40) DBF_STRING at 39 bytes"
    );

    process(&db, "Y").await;
    assert_eq!(
        field(&db, "Y", "VAL").await,
        Some(EpicsValue::Double(7.0)),
        "the routine bound is the one the stored name spells"
    );
}
