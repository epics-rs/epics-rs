//! The `build_ioc` path's scan start, as C's: `iocBuild` runs `scanInit`
//! (`iocInit.c:186`) with the facility stopped, and only `iocRun`'s
//! `scanRun` (`:259`) lets a periodic record tick.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;
use std::time::Duration;

const DB: &str = r#"
record(calc, "PER") { field(SCAN, ".1 second") field(CALC, "VAL+1") }
"#;

#[epics_macros_rs::epics_test]
async fn a_built_ioc_scans_only_after_ioc_run() {
    let ioc = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = std::sync::Arc::clone(ioc.database());
    let per = || match db.get_record("PER").unwrap().read().record.get_field("VAL") {
        Some(EpicsValue::Double(v)) => v,
        other => panic!("PER.VAL is {other:?}"),
    };

    epics_base_rs::runtime::task::sleep(Duration::from_millis(500)).await;
    assert_eq!(per(), 0.0, "a built IOC does not scan");

    assert!(ioc.run().await, "a built IOC runs");
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1000)).await;
    assert!(per() >= 5.0, "a running IOC ticks .1 second: {}", per());
}
