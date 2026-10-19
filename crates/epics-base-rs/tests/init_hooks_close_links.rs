//! C `iocShutdown`'s `doCloseLinks` (`iocInit.c:665-704`): before
//! `initHookAfterCloseLinks` a record with a CA or JSON link, or a dset
//! with a `dsxt`, is left PACT; a DB link and a record with neither are not.
//! An isolated build (`iocBuildIsolated`, `:232-247`) runs no server, closes
//! DB links too (`:680`) and announces `initHookBeforeFree` (`:745-747`):
//! `isolated.out` is C's run of `close_links.db` through it.

mod init_hook_oracle;

use std::collections::HashMap;

use init_hook_oracle as oracle;

use epics_base_rs::server::ioc_app::{BuildMode, ioc_pause, ioc_run, ioc_shutdown};
use epics_base_rs::server::ioc_builder::IocBuilder;

const C: &str = include_str!("fixtures/softioc/close_links.out");
const C_ISOLATED: &str = include_str!("fixtures/softioc/isolated.out");

#[epics_macros_rs::epics_test]
async fn close_links_leaves_softioc_records_pact() {
    oracle::register();
    let builder = IocBuilder::new()
        .register_link_type("ca", oracle::installer)
        .db_file(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/softioc/close_links.db"
            ),
            &HashMap::new(),
        )
        .unwrap();
    let ioc = builder.build_ioc().await.unwrap();
    *oracle::WATCH.lock().unwrap() = Some(ioc.database().clone());
    assert!(ioc.run().await);
    epics_base_rs::runtime::exit::call_at_exits();
    assert_eq!(
        oracle::rs("initHookAtShutdown", "initHookAfterStopScan"),
        oracle::section(C.lines(), "initHookAtShutdown", "initHookAfterStopScan")
    );
    assert_eq!(oracle::rs_shutdown(), oracle::c_shutdown());
}

#[epics_macros_rs::epics_test]
async fn an_isolated_build_follows_c_iocbuildisolated() {
    oracle::register();
    let ioc = IocBuilder::new()
        .build_mode(BuildMode::Isolated)
        .register_link_type("ca", oracle::installer)
        .db_file(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/softioc/close_links.db"
            ),
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    *oracle::WATCH.lock().unwrap() = Some(ioc.database().clone());
    assert!(ioc.run().await);
    assert_eq!(ioc_pause(), 0);
    assert_eq!(ioc_run(), 0);
    assert_eq!(ioc_shutdown(), 0);
    assert_eq!(
        oracle::rs("initHookAtIocBuild", "initHookAfterShutdown"),
        oracle::section(
            C_ISOLATED.lines(),
            "initHookAtIocBuild",
            "initHookAfterShutdown"
        )
    );
}

/// Records each `removeLink` the lset is asked for.
struct Removals(std::sync::Mutex<Vec<(String, String, String)>>);

#[epics_base_rs::async_trait]
impl epics_base_rs::server::database::LinkSet for Removals {
    fn is_connected(&self, _name: &[u8]) -> bool {
        false
    }
    async fn get_value(&self, _name: &[u8]) -> Option<epics_base_rs::types::EpicsValue> {
        None
    }
    fn remove_link(&self, holder: epics_base_rs::server::database::LinkHolder<'_>, name: &[u8]) {
        self.0.lock().unwrap().push((
            String::from_utf8_lossy(holder.record).into_owned(),
            holder.field.to_string(),
            String::from_utf8_lossy(name).into_owned(),
        ));
    }
}

/// `doCloseLinks`' `dbRemoveLink` hands a JSON link to its lset's
/// `removeLink` (`dbLink.c:202-213`), once, at the shutdown.
#[epics_macros_rs::epics_test]
async fn close_links_removes_a_scheme_link_from_its_lset() {
    let db = std::sync::Arc::new(epics_base_rs::server::database::PvDatabase::new());
    let lset = std::sync::Arc::new(Removals(std::sync::Mutex::new(Vec::new())));
    db.register_link_set("pva", lset.clone()).await;
    let ioc = IocBuilder::new()
        .with_database(db)
        .db_string(
            r#"record(calc,"CL:pva"){field(INPA,"pva://X")}"#,
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    assert!(ioc.run().await);
    assert!(lset.0.lock().unwrap().is_empty());
    epics_base_rs::runtime::exit::call_at_exits();
    assert_eq!(
        *lset.0.lock().unwrap(),
        [("CL:pva".to_string(), "INPA".to_string(), "X".to_string())]
    );
}

/// An `epicsExit` from inside a record's processing reaches `doCloseLinks`
/// on the thread that holds that record. The shutdown completes and closes
/// every other record.
#[epics_macros_rs::epics_test]
async fn a_shutdown_from_a_record_s_own_processing_completes() {
    let db = std::sync::Arc::new(epics_base_rs::server::database::PvDatabase::new());
    let lset = std::sync::Arc::new(Removals(std::sync::Mutex::new(Vec::new())));
    db.register_link_set("pva", lset.clone()).await;
    let ioc = IocBuilder::new()
        .with_database(db)
        .register_subroutine("shutdown", |_| {
            assert_eq!(ioc_shutdown(), 0);
            Ok(0)
        })
        .db_string(
            r#"record(sub,"CL:self"){field(SNAM,"shutdown")}
record(calc,"CL:pva"){field(INPA,"pva://X")}"#,
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    db.process_record("CL:self").await.unwrap();
    assert_eq!(
        epics_base_rs::server::ioc_app::get_ioc_state(),
        epics_base_rs::server::ioc_app::IocState::Void
    );
    assert_eq!(
        *lset.0.lock().unwrap(),
        [("CL:pva".to_string(), "INPA".to_string(), "X".to_string())]
    );
}

/// `doCloseLinks` asks the BOUND dset for its `dsxt`
/// (`dbDSETtoDevSup(pdbRecordType, precord->dset)`, `iocInit.c:689-692`),
/// which a DTYP put after init does not change: the record bound to the soft
/// dset is closed and left PACT, the one bound to `General Time` (no
/// `devExtend`) is not.
#[epics_macros_rs::epics_test]
async fn close_links_asks_the_bound_dset_not_the_dtyp() {
    let ioc = IocBuilder::new()
        .db_string(
            r#"record(ai,"CL:was-soft"){field(INP,"1.5")}
record(ai,"CL:was-time"){field(DTYP,"General Time") field(INP,"@TIME")}"#,
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    for (rec, dtyp) in [
        ("CL:was-soft", "General Time"),
        ("CL:was-time", "Soft Channel"),
    ] {
        db.put_record_field_from_ca(
            rec,
            "DTYP",
            epics_base_rs::types::EpicsValue::String(dtyp.into()),
        )
        .await
        .unwrap();
    }
    epics_base_rs::runtime::exit::call_at_exits();
    let pact = |rec: &str| db.get_record(rec).unwrap().read().is_processing();
    assert_eq!((pact("CL:was-soft"), pact("CL:was-time")), (true, false));
}
