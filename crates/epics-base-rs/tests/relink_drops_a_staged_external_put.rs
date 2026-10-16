//! A relink retires the value its old link still had to write. C's link put
//! (`dbPutFieldLink`) is `dbRemoveLink` + `dbAddLink`: `dbCaRemoveLink` hands
//! the old `caLink` `CA_CLEAR_CHANNEL` (`dbCa.c:431-446`), which `dbCaTask`
//! services first and alone (`dbCa.c:1200-1203`), and the new link gets a
//! fresh `caLink` with nothing pending (`dbCa.c:398`). So a value staged
//! behind a write in flight is never written — not to the PV the link no
//! longer names, and not to the same PV through the new link.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::{LinkPutOp, LinkSet, PutAdmission, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// Every put is recorded on entry; the first then waits for `release`, which
/// holds the link IN FLIGHT so the next put stays staged behind it.
struct Remote {
    puts: Arc<Mutex<Vec<(String, f64)>>>,
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[epics_base_rs::async_trait]
impl LinkSet for Remote {
    fn is_connected(&self, _name: &[u8]) -> bool {
        true
    }
    fn put_admission(&self, _name: &[u8]) -> PutAdmission {
        PutAdmission::Connected
    }
    async fn get_value(&self, _name: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn put_value(
        &self,
        name: &[u8],
        value: EpicsValue,
        _op: LinkPutOp,
    ) -> Result<(), String> {
        self.puts.lock().unwrap().push((
            String::from_utf8_lossy(name).into_owned(),
            value.to_f64().unwrap(),
        ));
        let held = self.release.lock().unwrap().take();
        if let Some(rx) = held {
            let _ = rx.await;
        }
        Ok(())
    }
}

async fn relink_with_a_put_staged(new_out: &str) -> (Vec<(String, f64)>, Vec<(String, f64)>) {
    let (db, _) = IocBuilder::new()
        .db_string(r#"record(ao,"A"){field(OUT,"ca://P1")}"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let puts = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = tokio::sync::oneshot::channel();
    db.register_link_set(
        "ca",
        Arc::new(Remote {
            puts: puts.clone(),
            release: Mutex::new(Some(rx)),
        }),
    )
    .await;

    put(&db, "VAL", EpicsValue::Double(1.0)).await;
    while puts.lock().unwrap().is_empty() {
        epics_base_rs::runtime::task::yield_now().await;
    }
    put(&db, "VAL", EpicsValue::Double(2.0)).await;
    put(&db, "OUT", EpicsValue::String(new_out.into())).await;
    tx.send(()).unwrap();
    db.sync_external_link_puts().await;
    let before = puts.lock().unwrap().clone();

    put(&db, "VAL", EpicsValue::Double(3.0)).await;
    db.sync_external_link_puts().await;
    let after = puts.lock().unwrap()[before.len()..].to_vec();
    (before, after)
}

async fn put(db: &PvDatabase, field: &str, value: EpicsValue) {
    db.put_record_field_from_ca("A", field, value)
        .await
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn a_relink_to_the_same_name_drops_the_staged_value() {
    let (before, after) = relink_with_a_put_staged("ca://P1").await;
    assert_eq!(before, vec![("P1".to_string(), 1.0)], "2 is never written");
    assert_eq!(after, vec![("P1".to_string(), 3.0)], "the new link writes");
}

#[epics_macros_rs::epics_test]
async fn a_relink_to_another_name_drops_the_staged_value() {
    let (before, after) = relink_with_a_put_staged("ca://P2").await;
    assert_eq!(
        before,
        vec![("P1".to_string(), 1.0)],
        "2 reaches neither P1 nor P2"
    );
    assert_eq!(after, vec![("P2".to_string(), 3.0)], "the new link writes");
}
