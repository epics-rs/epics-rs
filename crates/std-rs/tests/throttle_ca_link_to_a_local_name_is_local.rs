//! throttle classifies OUT and SINP by `dbNameToAddr`, as the synApps calcs
//! do: a `CA` link to a record of this IOC reads `Local PV`, and one to an
//! unknown name `Ext PV NC` (`throttleRecord.c` `checkLinks`).

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.
use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = r#"
record(ao, "T:O") {}
record(throttle, "T") { field(OUT, "T:O CA") field(SINP, "T:O CA") }
record(throttle, "TX") { field(OUT, "T:NOSUCH CA") field(SINP, "T:NOSUCH CA") }
"#;

#[tokio::test]
async fn a_ca_link_to_a_local_name_is_local() {
    let (db, _) = IocBuilder::new()
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let read = |pv: &str| db.get_pv(pv).ok().and_then(|v| v.to_f64());
    for _ in 0..400 {
        if read("T.OV") == Some(2.0) {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    for (pv, want) in [
        ("T.OV", 2.0),
        ("T.SIV", 2.0),
        ("TX.OV", 0.0),
        ("TX.SIV", 0.0),
    ] {
        assert_eq!(read(pv), Some(want), "{pv}");
    }
}
