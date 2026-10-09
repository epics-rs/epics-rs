//! `recGblGetTimeStampSimm` (`recGbl.c:332-346`): with SIMM on and TSE -2 the
//! record takes its SIOL source's time, a constant SIOL stamps now, and
//! without SIMM TIME is left alone. softIoc `camonitor -ts`:
//! `AI` shares `TSRC`'s timestamp, `AIC` is current, `AIN` is `<undefined>`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The EPICS epoch, `<undefined>` to a client.
fn unstamped() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(631_152_000)
}

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = r#"
record(ai, "TSRC") { field(VAL, "5") }
record(ai, "AI") { field(SIMM, "YES") field(SIOL, "TSRC") field(TSE, "-2") }
record(ai, "AIC") { field(SIMM, "YES") field(SIOL, "3") field(TSE, "-2") }
record(ai, "AIN") { field(SIMM, "NO") field(TSE, "-2") }
"#;

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

fn time_of(db: &PvDatabase, rec: &str) -> std::time::SystemTime {
    db.get_record(rec).unwrap().read().common.time
}

#[epics_macros_rs::epics_test]
async fn a_simulated_record_stamps_from_its_siol_source() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for rec in ["TSRC", "AI", "AIC", "AIN"] {
        process(&db, rec).await;
    }
    assert_ne!(time_of(&db, "TSRC"), unstamped());
    assert_eq!(time_of(&db, "AI"), time_of(&db, "TSRC"));
    assert_ne!(time_of(&db, "AIC"), unstamped());
    assert_eq!(time_of(&db, "AIN"), unstamped());
}
