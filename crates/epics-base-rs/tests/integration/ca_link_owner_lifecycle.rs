//! The `dbCaLink` worker is started by a build's `dbCaLinkInit`
//! (`iocInit.c:216`, `dbCa.c:339-355`) on every build path, and never by
//! record processing: a put staged before it runs waits in the queue, as a
//! put on C's `workList` waits for `dbCaTask`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::{LinkPutOp, LinkRef, LinkSet, PutAdmission, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// Records each put.
struct Lset(Arc<Mutex<Vec<EpicsValue>>>);

#[async_trait::async_trait]
impl LinkSet for Lset {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        true
    }
    fn put_admission(&self, _: LinkRef<'_>) -> PutAdmission {
        PutAdmission::Connected
    }
    fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
    async fn put_value(
        &self,
        _: LinkRef<'_>,
        value: EpicsValue,
        _: LinkPutOp,
    ) -> Result<(), String> {
        self.0.lock().unwrap().push(value);
        Ok(())
    }
}

fn worker_up() -> bool {
    let up = std::cell::Cell::new(false);
    epics_base_rs::runtime::taskwd::taskwd_show(1, &|line| {
        up.set(up.get() || line.contains("dbCaLink"))
    });
    up.get()
}

/// `IocBuilder::build` has no `iocBuild`, and still starts the worker
/// before any record can stage a put.
#[epics_macros_rs::epics_test]
async fn a_fixture_build_starts_the_worker() {
    assert!(!worker_up());
    let (_db, _) = IocBuilder::new()
        .db_string(r#"record(ao,"O"){}"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    assert!(worker_up());
}

/// An external link's open and put on a database no build has initialised
/// are staged, and start nothing under the record's hold; `dbCaLinkInit`
/// then services them.
#[epics_macros_rs::epics_test]
async fn an_external_put_starts_no_worker() {
    let puts = Arc::new(Mutex::new(Vec::new()));
    let db = PvDatabase::new();
    db.register_link_set("ca", Arc::new(Lset(Arc::clone(&puts))))
        .await;
    // iocInit's link pass: from here a relink adds its own link (dbAddLink).
    db.setup_cp_links().await;
    db.process_cp_links_on_connect().await;
    db.add_record(
        "O",
        Box::new(epics_base_rs::server::records::ao::AoRecord::new(0.0)),
    )
    .await
    .unwrap();
    db.put_pv_no_process("O.OUT", EpicsValue::String("X CA".into()))
        .await
        .unwrap();
    {
        let rec = db.get_record("O").unwrap();
        let mut inst = rec.write();
        inst.common.udf = 0;
        inst.record
            .put_field("VAL", EpicsValue::Double(4.0))
            .unwrap();
    }
    assert_eq!(
        db.setup_external_link_opens().await,
        0,
        "the OUT put staged the open, as dbCaAddLink does"
    );
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("O", &mut visited)
        .await
        .or_else(epics_base_rs::error::CaError::unless_process_status)
        .unwrap();
    // Long enough for a worker the put had started to register and run it.
    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!worker_up(), "the put started the worker");
    assert!(puts.lock().unwrap().is_empty());

    assert!(db.ca_link_init());
    db.sync_external_link_puts().await;
    assert_eq!(*puts.lock().unwrap(), [EpicsValue::Double(4.0)]);
}
