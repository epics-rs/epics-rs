//! sCalcout `special()` classifies the link that was put and leaves every
//! other status alone (`sCalcoutRecord.c:505-569`): a CA input that
//! `checkLinks` read connected stays `Ext PV OK`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::{LinkRef, LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

const EXT_OK: i16 = 1;
const LOCAL: i16 = 2;

const DB: &str = r#"
record(ao, "L") {}
record(scalcout, "S") {
  field(INPA, "R:X CA")
}
"#;

/// A CA link set whose every link is connected.
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

#[epics_macros_rs::epics_test]
async fn an_input_put_leaves_a_connected_input_status() {
    let ioc = IocBuilder::new()
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_link_type("ca", |db| async move {
            db.register_link_set("ca", Arc::new(Up)).await;
            Vec::new()
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        if status(&db, "S.INAV") == EXT_OK {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(status(&db, "S.INAV"), EXT_OK, "INPA never connected");

    db.put_record_field_from_ca_no_notify("S", "INPB", EpicsValue::String("L".into()))
        .await
        .unwrap();
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    assert_eq!(status(&db, "S.INBV"), LOCAL);
    assert_eq!(status(&db, "S.INAV"), EXT_OK);
}
