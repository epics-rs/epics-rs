//! throttle's SYNC arm runs `checkLink` when SIV reads `Ext PV NC` and syncs
//! only if SINP then reads connected (`throttleRecord.c:376-389`).

// RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — both ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::{LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const EXT_NC: i16 = 0;
const EXT_OK: i16 = 1;

/// A CA link set whose links read 7, connected or not as `up` says.
struct Ca {
    up: bool,
}

#[epics_base_rs::async_trait]
impl LinkSet for Ca {
    fn is_connected(&self, _: &[u8]) -> bool {
        self.up
    }
    fn get_cached_value(&self, _: &[u8]) -> Option<EpicsValue> {
        self.up.then_some(EpicsValue::Double(7.0))
    }
    async fn get_value(&self, _: &[u8]) -> Option<EpicsValue> {
        self.up.then_some(EpicsValue::Double(7.0))
    }
}

fn get(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn put(db: &PvDatabase, field: &str, value: EpicsValue) {
    db.put_record_field_from_ca_no_notify("T", field, value)
        .await
        .unwrap();
    // The put link's status comes from a classification task.
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// Re-points SINP at a CA link, so SIV reads `Ext PV NC` until the 0.5 s
/// recheck, then puts SYNC=Process inside that window.
async fn sync_from_a_fresh_ca_sinp(up: bool) -> Arc<PvDatabase> {
    std_rs::register_std_dbd();
    let ioc = IocBuilder::new()
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .register_link_type("ca", move |db| async move {
            db.register_link_set("ca", Arc::new(Ca { up })).await;
            Vec::new()
        })
        .db_string("record(throttle, \"T\") {}\n", &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    put(&db, "SINP", EpicsValue::String("R:X CA".into())).await;
    assert_eq!(get(&db, "T.SIV") as i16, EXT_NC);
    put(&db, "SYNC", EpicsValue::Short(1)).await;
    db
}

#[tokio::test]
async fn a_sync_connects_an_ext_nc_sinp_and_reads_it() {
    let db = sync_from_a_fresh_ca_sinp(true).await;
    assert_eq!(get(&db, "T.SIV") as i16, EXT_OK);
    assert_eq!(get(&db, "T"), 7.0);
}

#[tokio::test]
async fn a_sync_leaves_a_still_down_sinp_unread() {
    let db = sync_from_a_fresh_ca_sinp(false).await;
    assert_eq!(get(&db, "T.SIV") as i16, EXT_NC);
    assert_eq!(get(&db, "T"), 0.0);
}
