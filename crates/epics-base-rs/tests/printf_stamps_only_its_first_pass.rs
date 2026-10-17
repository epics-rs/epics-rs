//! C printf calls `recGblGetTimeStamp` inside `if (!pact)`
//! (`printfRecord.c:378-383`), ahead of its device write, so the pass an
//! asynchronous `write_string` completes keeps the TIME the first pass took.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::{
    LinkDbfType, LinkMetadata, LinkPutOp, LinkSet, PutAdmission, PvDatabase,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, WallTime};

/// One remote channel whose put takes a while to land.
struct Slow;

#[epics_base_rs::async_trait]
impl LinkSet for Slow {
    fn is_connected(&self, _name: &[u8]) -> bool {
        true
    }
    fn put_admission(&self, _name: &[u8]) -> PutAdmission {
        PutAdmission::Connected
    }
    fn get_cached_value(&self, _name: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _name: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn put_value(
        &self,
        _name: &[u8],
        _value: EpicsValue,
        _op: LinkPutOp,
    ) -> Result<(), String> {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
        Ok(())
    }
    fn link_metadata(&self, _name: &[u8]) -> Option<LinkMetadata> {
        Some(LinkMetadata {
            dbf_type: Some(LinkDbfType::String),
            element_count: Some(1),
            ..Default::default()
        })
    }
}

const DB: &str = r#"
record(printf, "A") { field(DTYP, "Async Soft Channel") field(FMT, "x") field(OUT, "ca://R") }
record(printf, "S") { field(FMT, "x") }
"#;

async fn ioc() -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.register_link_set("ca", Arc::new(Slow)).await;
    db.process_cp_links_on_connect().await;
    db
}

fn time(db: &PvDatabase, name: &str) -> WallTime {
    db.get_record(name).unwrap().read().common.time
}

fn pact(db: &PvDatabase, name: &str) -> bool {
    db.get_record(name).unwrap().read().is_processing()
}

#[epics_macros_rs::epics_test]
async fn the_completing_pass_keeps_the_first_pass_time() {
    let db = ioc().await;
    let before = time(&db, "A");
    db.process_record("A").await.unwrap();
    assert!(pact(&db, "A"), "A waits for its put");
    let first = time(&db, "A");
    assert_ne!(first, before, "the first pass stamps ahead of the write");
    for _ in 0..100 {
        if !pact(&db, "A") {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    assert!(!pact(&db, "A"), "the put completed A");
    assert_eq!(time(&db, "A"), first, "the completing pass stamps nothing");
}

#[epics_macros_rs::epics_test]
async fn a_synchronous_pass_stamps() {
    let db = ioc().await;
    let before = time(&db, "S");
    db.process_record("S").await.unwrap();
    assert_ne!(time(&db, "S"), before);
}
