//! `tableRecord.c::init_record` runs `checkLinks` (:383) after `dbInitLink`,
//! then derives the user limits from the motor limits of every axis whose
//! limit pair is usable (:401, `CalcLocalUserLimits`). `checkLinks`
//! (2318-2350) counts a link unusable only when it is `CONSTANT`, or a
//! `CA_LINK` that is not connected; a PV Access link is a `JSON_LINK`
//! (`dbStaticLib.c:2280-2287`), so an `M0X` limit pair on PV Access limits
//! `X` from the first `init_record`, before any `process()`.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::{DynLinkSet, LinkPutOp, LinkRef, LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use optics_rs::records::table::TableRecord;

/// A PV Access server whose motor limits are ±10.
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

/// Counted as `CONSTANT`, the `M0X` limits are skipped and `X` is pinned to
/// `u ± SMALL` (`tableRecord.c:2070-2076`); counted usable, the ±10 motor
/// limits bound it.
#[epics_base_rs::epics_test]
async fn a_pva_limit_pair_limits_x_at_init() {
    let db = table().await;
    let hlx = db.get_pv("TBL.HLX").unwrap().to_f64().unwrap();
    let llx = db.get_pv("TBL.LLX").unwrap().to_f64().unwrap();
    assert!(hlx > 1.0, "HLX {hlx} is not bounded by H0X");
    assert!(llx < -1.0, "LLX {llx} is not bounded by L0X");
}
