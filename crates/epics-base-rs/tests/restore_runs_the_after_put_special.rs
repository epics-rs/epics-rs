//! The autosave-restore write must run BOTH `special()` passes, as every
//! other `dbPut` route does.
//!
//! C line numbers below resolve at epics-base tag `R7.0.10`, not at this
//! machine's working tree (`R7.0.10-146-g8f5015b66`), where PR #944 puts
//! `dbPut`'s header 3 lines lower and its body, from the first `if (special)`
//! on, 5 lower.
//!
//! C's autosave `reboot_restore` writes through `dbPutField`, and both of
//! that function's branches run the pair. For an ordinary field
//! (`dbAccess.c` `dbPut():1345-1348`, `:1399-1403` with the status adopted
//! at `:1404`; the block is quoted below with the `:1398` comment above it):
//!
//! ```c
//!     if (special) {
//!         status = dbPutSpecial(paddr, 0);
//!         if (status) return status;
//!     }
//!     ...
//!     /* Always do special processing if needed */
//!     if (special) {
//!         long status2 = dbPutSpecial(paddr, 1);
//!         if (status2)
//!             status = status2;
//!     }
//!     if (status) goto done;
//! ```
//!
//! and for a link field `dbPutFieldLink():1174,1178` runs the same pair. The
//! after pass is what re-derives the field's DEPENDENT state —
//! `calcRecord.c` `special():146-152` recompiles RPCL from CALC,
//! `subRecord.c:188` and `aSubRecord.c:563-575` re-resolve SNAM through the
//! registry — and `dbPut` ADOPTS its status, so an unresolvable name is a
//! failed put and not a silent one.
//!
//! `put_pv_no_process` ran only the before pass, so a restored CALC left the
//! record evaluating the OLD expression and a restored SNAM left it running
//! the OLD routine, with the restore reporting success either way.
//!
//! Boundaries, one case each: the record's own half of the after pass (CALC ->
//! RPCL), the registry half (SNAM -> subroutine), and the status the after
//! pass returns (unregistered SNAM -> Err).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{Record, SubroutineFn};
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::sub_record::SubRecord;
use epics_base_rs::types::EpicsValue;

/// A `sub` subroutine that writes `marker` into VAL, so VAL names which
/// routine actually ran.
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

async fn val(db: &PvDatabase, name: &str) -> Option<EpicsValue> {
    let inst = db.get_record(name).unwrap();
    let g = inst.read();
    g.record.get_field("VAL")
}

/// The record's own half of `dbPutSpecial(paddr, 1)`: `calcRecord.c:146-152`
/// recompiles RPCL from the stored CALC. A restore that skips it leaves the
/// record evaluating the expression it had before the restore.
#[epics_macros_rs::epics_test]
async fn a_restored_calc_recompiles_the_expression() {
    let db = PvDatabase::new();
    let mut seed = CalcRecord::default();
    seed.put_field("CALC", EpicsValue::String("A+1".into()))
        .unwrap();
    seed.put_field("A", EpicsValue::Double(1.0)).unwrap();
    db.add_record("C", Box::new(seed)).await.unwrap();
    db.ioc_init().await;

    process(&db, "C").await;
    assert_eq!(
        val(&db, "C").await,
        Some(EpicsValue::Double(2.0)),
        "the seeded expression evaluates before the restore"
    );

    db.put_pv_no_process("C.CALC", EpicsValue::String("A+10".into()))
        .await
        .expect("a legal CALC restores");

    process(&db, "C").await;
    assert_eq!(
        val(&db, "C").await,
        Some(EpicsValue::Double(11.0)),
        "the restored CALC must be the one that runs — a skipped after-put \
         special leaves RPCL compiled from the old expression"
    );
}

/// The registry half of the same pass (`subRecord.c:188`
/// `prec->sadr = registryFunctionFind(prec->snam)`): a restored SNAM must
/// rebind the live routine, not just store the name.
#[epics_macros_rs::epics_test]
async fn a_restored_snam_rebinds_the_subroutine() {
    let db = PvDatabase::new();
    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert("fnA".into(), writes_val(1.0));
    registry.insert("fnB".into(), writes_val(2.0));
    db.install_subroutine_registry(registry.clone()).await;

    let mut seed = SubRecord::default();
    seed.put_field("SNAM", EpicsValue::String("fnA".into()))
        .unwrap();
    db.add_record("S", Box::new(seed)).await.unwrap();
    // Bind as iocInit would — `PvDatabase::ioc_init` does not run the registry
    // lookup, that is `IocApp`/`IocBuilder`'s pass.
    db.get_record("S").unwrap().write().subroutine = registry.get("fnA").cloned();

    process(&db, "S").await;
    assert_eq!(
        val(&db, "S").await,
        Some(EpicsValue::Double(1.0)),
        "the seeded routine runs before the restore"
    );

    db.put_pv_no_process("S.SNAM", EpicsValue::String("fnB".into()))
        .await
        .expect("a registered SNAM restores");

    process(&db, "S").await;
    assert_eq!(
        val(&db, "S").await,
        Some(EpicsValue::Double(2.0)),
        "the restored SNAM must be the routine that runs — a skipped \
         after-put special leaves the old binding live"
    );
}

/// The status the after pass returns. C `dbPut` adopts it
/// (`if (status2) status = status2;`), so restoring a name the registry does
/// not carry is a FAILED put — `S_db_BadSub` — where the port reported
/// success and left the record bound to whatever it had.
#[epics_macros_rs::epics_test]
async fn a_restored_unregistered_snam_fails_the_put() {
    let db = PvDatabase::new();
    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert("fnA".into(), writes_val(1.0));
    db.install_subroutine_registry(registry.clone()).await;

    let mut seed = SubRecord::default();
    seed.put_field("SNAM", EpicsValue::String("fnA".into()))
        .unwrap();
    db.add_record("S", Box::new(seed)).await.unwrap();
    db.get_record("S").unwrap().write().subroutine = registry.get("fnA").cloned();

    let err = db
        .put_pv_no_process("S.SNAM", EpicsValue::String("neverRegistered".into()))
        .await
        .expect_err("an unregistered SNAM is S_db_BadSub, not a silent success");
    assert!(
        format!("{err}").contains("Subroutine not found"),
        "the failure must be the SNAM lookup, got {err}"
    );

    // C stores the name either way — the `goto done` skips the udf clear and
    // the monitor post, not the store that already happened.
    let inst = db.get_record("S").unwrap();
    assert_eq!(
        inst.read().record.get_field("SNAM"),
        Some(EpicsValue::String("neverRegistered".into())),
        "the failed put still leaves the stored name behind, as C does"
    );
}
