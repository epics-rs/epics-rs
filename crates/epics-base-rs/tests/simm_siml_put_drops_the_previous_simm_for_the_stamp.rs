//! After a cycle that read SIML, `dbpf AO.SIML ""` then `dbpf AO.SIMM 1` then
//! one PROC stamps the current time: the SIMM of the cycle that read SIML is
//! not carried past the SIML put. softIoc `R7.0.10-146-g8f5015b66`, ao,
//! `TSE=-2`, SIML "SM" processed once with SM=0.

use epics_base_rs::types::WallTime;
use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const STALE: Duration = Duration::new(3_000_000, 333_333_333);

const DB_TEXT: &str = r#"
record(longin, "SM") { field(VAL, "0") }
record(ao, "AO") { field(SIML, "SM") field(TSE, "-2") }
"#;

#[epics_macros_rs::epics_test]
async fn a_siml_put_forgets_the_simm_read_before_it() {
    let db = IocBuilder::new()
        .db_string(DB_TEXT, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let cell = db.get_record("AO").unwrap();
    let process = || async {
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        db.process_record_with_links("AO", &mut visited)
            .await
            .unwrap();
    };
    process().await;
    db.put_record_field_from_ca("AO", "SIML", EpicsValue::String("".into()))
        .await
        .unwrap();
    db.put_record_field_from_ca("AO", "SIMM", EpicsValue::Short(1))
        .await
        .unwrap();
    cell.write().common.time = WallTime::UNIX_EPOCH + STALE;
    let before = WallTime::now();
    process().await;
    let t = cell.read().common.time;
    assert!(t >= before, "TIME not stamped: {t:?}");
}
