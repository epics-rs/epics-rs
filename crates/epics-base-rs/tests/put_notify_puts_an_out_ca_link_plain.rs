//! A put-notify chain leaves an output's CA link put plain: C `dbPutLink` on
//! a CA link is `dbCaPutLink`, which is `dbCaPutLinkCallback(..., 0, NULL)`
//! (`dbCa.c:627-631`), a `CA_PUT`. Only `dbPutLinkAsync` asks for the
//! callback. A `caput -c` to a Soft Channel ao whose OUT names a remote PV
//! used to send a `WRITE_NOTIFY` upstream.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::{LinkPutOp, LinkRef, LinkSet, PutAdmission, PvDatabase};
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

struct Lset(Arc<Mutex<Vec<LinkPutOp>>>);

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
    async fn put_value(&self, _: LinkRef<'_>, _: EpicsValue, op: LinkPutOp) -> Result<(), String> {
        self.0.lock().unwrap().push(op);
        Ok(())
    }
}

#[epics_macros_rs::epics_test]
async fn a_put_notify_puts_an_out_ca_link_plain() {
    let ops = Arc::new(Mutex::new(Vec::new()));
    let db = PvDatabase::new();
    db.ca_link_init();
    epics_base_rs::runtime::task::background_init();
    db.register_link_set("ca", Arc::new(Lset(Arc::clone(&ops))))
        .await;
    db.process_cp_links_on_connect().await;
    db.add_record("AO", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.put_pv_no_process("AO.OUT", EpicsValue::String("ca://REMOTE:OUT".into()))
        .await
        .unwrap();
    db.put_pv_no_process("AO.VAL", EpicsValue::Double(3.0))
        .await
        .unwrap();

    db.process_record_with_notify("AO")
        .await
        .expect("AO is loaded");
    db.sync_external_link_puts().await;

    assert_eq!(ops.lock().unwrap().as_slice(), [LinkPutOp::Plain]);
}
