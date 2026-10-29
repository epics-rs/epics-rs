//! throttle's SINP `special()` sets SIV before the put returns, and its SYNC
//! arm syncs by SIV (`throttleRecord.c:341-389`), so a SYNC put right after a
//! SINP put goes by the new link's status. The expected values are the C
//! oracle's for the same two puts.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::{LinkRef, LinkSet, PutSplit, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

const EXT_NC: f64 = 0.0;
const LOCAL: f64 = 2.0;

/// A CA link set whose links never connect.
struct Down;

#[epics_base_rs::async_trait]
impl LinkSet for Down {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        false
    }
    fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
}

fn get(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// Boots `T` reading `sinp`, beside an `L` holding 5, then puts SINP=`to`
/// and SYNC=Process.
async fn sync_right_after_a_sinp_put(sinp: &str, to: &str) -> Arc<PvDatabase> {
    std_rs::register_std_dbd();
    let db = format!(
        "record(ao, \"L\") {{\n  field(VAL, \"5\")\n}}\nrecord(throttle, \"T\") {{\n  field(SINP, \"{sinp}\")\n}}\n"
    );
    let ioc = IocBuilder::new()
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_link_type("ca", |db| async move {
            db.register_link_set("ca", Arc::new(Down)).await;
            Vec::new()
        })
        .db_string(&db, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    // Both under one hold, as a pvxs group put makes them: no task can
    // post between the two.
    db.put_many_locked(&["T"], 2, |held| {
        let t = held.record(0).unwrap();
        for (field, value) in [
            ("SINP", EpicsValue::String(to.into())),
            ("SYNC", EpicsValue::Short(1)),
        ] {
            held.put_record_field_from_ca_no_notify(t, field, value, PutSplit::DbPutField)
                .unwrap();
        }
    });
    db
}

#[epics_base_rs::epics_test]
async fn a_sync_right_after_a_local_sinp_put_reads_it() {
    let db = sync_right_after_a_sinp_put("R:X CA", "L").await;
    assert_eq!(get(&db, "T.SIV"), LOCAL);
    assert_eq!(get(&db, "T"), 5.0);
    assert_eq!(get(&db, "T.SYNC"), 0.0, "SYNC Idle");
    assert_eq!(get(&db, "T.STS"), 2.0, "STS Success");
}

#[epics_base_rs::epics_test]
async fn a_sync_right_after_an_unconnected_sinp_put_reads_nothing() {
    let db = sync_right_after_a_sinp_put("L", "R:X CA").await;
    assert_eq!(get(&db, "T.SIV"), EXT_NC);
    assert_eq!(get(&db, "T"), 0.0);
    assert_eq!(get(&db, "T.SYNC"), 1.0, "SYNC Process");
    assert_eq!(get(&db, "T.STS"), 0.0, "STS Unknown");
}
