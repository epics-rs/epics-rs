//! `tableRecord.c::init_record` runs `checkLinks` (:383) after `dbInitLink`,
//! then reads the motor limits of every axis whose limit pair is usable
//! (:401, `GetMotorLimits`). `checkLinks` (2318-2350) counts a link unusable
//! only when it is `CONSTANT`, or a `CA_LINK` that is not connected; a PV
//! Access link is a `JSON_LINK` (`dbStaticLib.c:2280-2287`), so `init_record`
//! reads an `M0X` limit pair on PV Access. pvxs opens the channel only once
//! the IOC runs (`pvalink_lset.cpp:131`), so that read fails "Disconn"
//! (:263-268) and `GetMotorLimits` zeroes both limits (:1026, :1031).

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::{DynLinkSet, LinkPutOp, LinkRef, LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use optics_rs::records::table::TableRecord;

/// A PV Access server whose motor limits are ±10, out of reach at init.
struct Limits;

fn limit(link: LinkRef<'_>) -> Option<EpicsValue> {
    Some(EpicsValue::Double(if link.name.ends_with(b"HLM") {
        10.0
    } else {
        -10.0
    }))
}

#[epics_base_rs::async_trait]
impl LinkSet for Limits {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        true
    }
    fn get_cached_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        limit(link)
    }
    async fn get_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        limit(link)
    }
    async fn put_value(&self, _: LinkRef<'_>, _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
        Ok(())
    }
}

const DB: &str = r#"
record(table, "TBL") {
    field(GEOM, "SRI")
    field(LX, "200")
    field(LZ, "300")
    field(SX, "100")
    field(SY, "50")
    field(SZ, "150")
    field(H0XL, "{pva:\"M1.HLM\"}")
    field(L0XL, "{pva:\"M1.LLM\"}")
    field(H0X, "10")
    field(L0X, "-10")
}
"#;

async fn table() -> Arc<PvDatabase> {
    let lset: DynLinkSet = Arc::new(Limits);
    let ioc = IocBuilder::new()
        .register_record_type("table", || Box::new(TableRecord::default()))
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

/// Counted as `CONSTANT`, the `M0X` limits are not read and keep the loaded
/// ±10 with no alarm; counted usable, the failed reads zero them and raise
/// LINK/INVALID.
#[epics_base_rs::epics_test]
async fn a_pva_limit_pair_is_read_at_init() {
    let db = table().await;
    let get = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    assert_eq!(get("TBL.H0X"), 0.0, "H0XL was not read");
    assert_eq!(get("TBL.L0X"), 0.0, "L0XL was not read");
    assert_eq!(get("TBL.SEVR"), 3.0, "INVALID");
    assert_eq!(get("TBL.STAT"), 14.0, "LINK");
}
