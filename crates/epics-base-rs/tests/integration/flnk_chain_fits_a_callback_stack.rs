//! A long FLNK chain run from a callback thread fits its `epicsThreadStackBig`
//! stack in a debug build too: C processes a 21-record chain off a calcout's
//! ODLY on cbLow (`calcoutRecord.c:315-317`, `dbScanFwdLink` recursion).

use std::collections::HashMap;
use std::fmt::Write;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const HOPS: usize = 30;

#[epics_macros_rs::epics_test]
async fn a_long_flnk_chain_runs_on_a_callback_thread() {
    let mut db = String::from(
        r#"record(calcout, "HEAD") { field(CALC, "1") field(ODLY, "0.01") field(FLNK, "C1") }
"#,
    );
    for i in 1..=HOPS {
        let next = if i < HOPS {
            format!("C{}", i + 1)
        } else {
            String::new()
        };
        writeln!(
            db,
            r#"record(calc, "C{i}") {{ field(CALC, "VAL+1") field(FLNK, "{next}") }}"#
        )
        .unwrap();
    }
    let (db, _) = IocBuilder::new()
        .db_string(&db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    db.process_record("HEAD").await.unwrap();
    let last = format!("C{HOPS}");
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        if db.get_pv(&last).unwrap() == EpicsValue::Double(1.0) {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    panic!("{last} never processed");
}
