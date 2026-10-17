//! `IocBuilder::build_ioc` registers its link sets at C `dbCaLinkInit`
//! (`iocInit.c:217`), ahead of `iocBuild_2`, so a PINI=YES record reading an
//! external link reads a CA link — one `dbCaRun` (`:260`) has not yet
//! connected, so the read raises LINK/INVALID (`dbCa.c:459`).
//! `realtime-ca-ioc` installed its CA link set after the build, so its PINI
//! pass found none.

use epics_base_rs::server::database::LinkSet;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;
use std::sync::Arc;

struct RemoteLset;

#[epics_base_rs::async_trait]
impl LinkSet for RemoteLset {
    fn is_connected(&self, _: &[u8]) -> bool {
        true
    }
    fn get_cached_value(&self, name: &[u8]) -> Option<EpicsValue> {
        (name == b"REMOTE:PV").then_some(EpicsValue::Double(7.0))
    }
    async fn get_value(&self, name: &[u8]) -> Option<EpicsValue> {
        self.get_cached_value(name)
    }
}

#[epics_macros_rs::epics_test]
async fn a_pini_record_reads_its_external_link_as_an_unconnected_ca_link() {
    let ioc = IocBuilder::new()
        .register_link_type("ca", |db| async move {
            db.register_link_set("ca", Arc::new(RemoteLset)).await;
            Vec::new()
        })
        .db_string(
            r#"record(ai, "P") { field(PINI, "YES") field(INP, "REMOTE:PV") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();

    let db = ioc.database();
    let get = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    assert_eq!(get("P.VAL"), 0.0, "PINI=YES runs before dbCaRun");
    assert_eq!(get("P.SEVR"), 3.0);
    assert_eq!(get("P.STAT"), 14.0, "LINK_ALARM");
}
