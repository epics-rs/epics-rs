//! A PV Access link is C's `JSON_LINK` (`dbStaticLib.c:2280-2287`), and
//! `tableRecord.c::checkLinks` (2318-2350) asks a link only whether it is
//! `CONSTANT` and, for a `CA_LINK`, whether it is connected. Any other type
//! reports 1, so a sixth axis on a PV Access link counts as a motor even
//! while its channel is down. No C IOC with both the optics module and pvxs
//! links is available locally, so the expectation is read from those lines.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::{DynLinkSet, LinkPutOp, LinkRef, LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;
use optics_rs::records::table::TableRecord;

/// A PV Access channel that never connects.
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
    async fn put_value(&self, _: LinkRef<'_>, _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
        Err("channel disconnected".into())
    }
}

const DB: &str = r#"
record(ao, "M1") { }
record(ao, "M2") { }
record(ao, "M3") { }
record(ao, "M4") { }
record(ao, "M5") { }
record(table, "TBL") {
    field(GEOM, "SRI")
    field(LX, "200")
    field(LZ, "300")
    field(SX, "100")
    field(SY, "50")
    field(SZ, "150")
    field(M0XL, "M1.VAL PP MS")
    field(M0YL, "M2.VAL PP MS")
    field(M1YL, "M3.VAL PP MS")
    field(M2XL, "M4.VAL PP MS")
    field(M2YL, "M5.VAL PP MS")
    field(M2ZL, "{pva:\"M6\"}")
    field(R0XI, "M1.VAL NPP NMS")
    field(R0YI, "M2.VAL NPP NMS")
    field(R1YI, "M3.VAL NPP NMS")
    field(R2XI, "M4.VAL NPP NMS")
    field(R2YI, "M5.VAL NPP NMS")
    field(R2ZI, "{pva:\"M6\"}")
}
"#;

async fn table() -> Arc<PvDatabase> {
    let lset: DynLinkSet = Arc::new(Down);
    let ioc = IocBuilder::new()
        .register_record_type("table", || Box::new(TableRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_link_type("pva", move |db| async move {
            db.register_link_set("pva", lset).await;
            Vec::new()
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    db
}

/// The disconnected `CA_LINK` of `table_link_kind.rs` gives `can_RW_drive`
/// 0 and leaves `M2Z` undriven; the same axis on a PV Access link gives 1,
/// so `Z` drives `M2Z` as it does with six local motors.
#[epics_base_rs::epics_test]
async fn a_down_pva_sixth_axis_still_counts_as_a_motor() {
    let db = table().await;
    db.put_record_field_from_ca("TBL", "Z", EpicsValue::Double(5.0))
        .await
        .unwrap();
    epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
    assert_eq!(db.get_pv("TBL.M2Z").unwrap().to_f64(), Some(5.0));
}
