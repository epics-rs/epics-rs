//! throttle `special()` classifies the link that was put and leaves the
//! other link's status alone (`throttleRecord.c:341-374`): a CA link that
//! `checkLink` read connected stays `Ext PV OK`.

// RTEMS-EXEC-MODEL-ALLOW(3): checked, not waived — all 3 ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::{LinkRef, LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

const EXT_OK: i16 = 1;
const LOCAL: i16 = 2;

/// A CA link set whose every link is connected and reads 7.
struct Up;

#[epics_base_rs::async_trait]
impl LinkSet for Up {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        true
    }
    fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        Some(EpicsValue::Double(7.0))
    }
    async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        Some(EpicsValue::Double(7.0))
    }
}

fn status(db: &PvDatabase, pv: &str) -> i16 {
    db.get_pv(pv).unwrap().to_f64().unwrap() as i16
}

/// Boots `T` with `out` and `sinp` and waits for the CA one, `ca`, to read
/// connected.
async fn boot(out: &str, sinp: &str, ca: &str) -> Arc<PvDatabase> {
    std_rs::register_std_dbd();
    let db = format!(
        "record(ao, \"L\") {{}}\nrecord(throttle, \"T\") {{\n  field(OUT, \"{out}\")\n  field(SINP, \"{sinp}\")\n}}\n"
    );
    let ioc = IocBuilder::new()
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_link_type("ca", |db| async move {
            db.register_link_set("ca", Arc::new(Up)).await;
            Vec::new()
        })
        .db_string(&db, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    for _ in 0..400 {
        if status(&db, ca) == EXT_OK {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(status(&db, ca), EXT_OK, "{ca} never connected");
    db
}

async fn put(db: &PvDatabase, field: &str, value: EpicsValue) {
    db.put_record_field_from_ca_no_notify("T", field, value)
        .await
        .unwrap();
    // The put link's status comes from a classification task.
    tokio::time::sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn an_out_put_leaves_a_connected_siv() {
    let db = boot("L", "R:X CA", "T.SIV").await;
    put(&db, "OUT", EpicsValue::String("L".into())).await;
    assert_eq!(status(&db, "T.OV"), LOCAL);
    assert_eq!(status(&db, "T.SIV"), EXT_OK);
}

#[tokio::test]
async fn a_sinp_put_leaves_a_connected_ov() {
    let db = boot("R:X CA", "L", "T.OV").await;
    put(&db, "SINP", EpicsValue::String("L".into())).await;
    assert_eq!(status(&db, "T.SIV"), LOCAL);
    assert_eq!(status(&db, "T.OV"), EXT_OK);
}

/// C's SYNC arm syncs on any SIV but `Ext PV NC` (:376-389), so an OUT put
/// does not hold up a sync from a connected SINP.
#[tokio::test]
async fn a_sync_after_an_out_put_reads_a_connected_sinp() {
    let db = boot("L", "R:X CA", "T.SIV").await;
    put(&db, "OUT", EpicsValue::String("L".into())).await;
    put(&db, "SYNC", EpicsValue::Short(1)).await;
    assert_eq!(db.get_pv("T").unwrap().to_f64(), Some(7.0));
}
