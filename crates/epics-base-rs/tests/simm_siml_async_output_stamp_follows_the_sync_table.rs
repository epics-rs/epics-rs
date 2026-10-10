//! An `"Async Soft Channel"` ao at `TSE=-2` stamps by the same table as the
//! synchronous ao: the first pass stamps from the SIMM before the SIML read,
//! and the completion pass (`aoRecord.c:228`) sees the SIMM that pass left —
//! a simulated write never goes async, so it cannot add a stamp the first
//! pass did not.
//!
//! softIoc `R7.0.10-146-g8f5015b66`, SIML from a longin, one `.PROC` per step:
//! undefined, undefined, new, new, same.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const STALE: Duration = Duration::new(3_000_000, 333_333_333);

const DB_TEXT: &str = r#"
record(longin, "SW") {}
record(ao, "SINK") {}
record(ao, "A_AO") { field(DTYP, "Async Soft Channel") field(OUT, "SINK") field(SIML, "SW") field(TSE, "-2") }
"#;

#[epics_macros_rs::epics_test]
async fn the_async_output_stamps_like_the_sync_one() {
    let db = IocBuilder::new()
        .db_string(DB_TEXT, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let stale = std::time::SystemTime::UNIX_EPOCH + STALE;
    let cell = db.get_record("A_AO").unwrap();
    cell.write().common.time = stale;
    let mut prev = stale;
    let mut got = Vec::new();
    for sw in [0, 1, 1, 0, 0] {
        db.put_record_field_from_ca("SW", "VAL", EpicsValue::Long(sw))
            .await
            .unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        db.process_record_with_links("A_AO", &mut visited)
            .await
            .unwrap();
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
        let t = cell.read().common.time;
        got.push(if t == stale {
            'U'
        } else if t == prev {
            'S'
        } else {
            'N'
        });
        prev = t;
    }
    assert_eq!(got.iter().collect::<String>(), "UUNNS");
}
