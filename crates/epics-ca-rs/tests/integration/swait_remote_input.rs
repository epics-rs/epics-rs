//! swait opens INAN..INLN with `recDynLinkAddInput` (`swaitRecord.c:358`), a
//! CA channel, so a name another IOC serves is searched for and connects:
//! INAV reads PV OK and the input carries the server's value.
//!
//! oracleSwaitIoc (calc R3-7-5), RM:X (11) and RM:Y served by a second
//! softIoc, RM:W read as `VAL INAV OUTV SEVR STAT`, then processed with
//! `caput -c`:
//!
//! ```text
//! pre   0 PV OK PV OK INVALID UDF
//! post  11 NO_ALARM NO_ALARM    RM:Y 11
//! ```

#![cfg(feature = "client-core")]
#![cfg(tokio_backend)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::{DBR_DOUBLE, EpicsValue};
use epics_ca_rs::calink::{install_calink_resolver, register_calink_commands};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

const PV_OK: f64 = 0.0;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn settle(db: &PvDatabase, done: impl Fn(&PvDatabase) -> bool) {
    let deadline = tokio::time::Instant::now() + fact_budget::FACT_BUDGET;
    while !done(db) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_name_another_ioc_serves_is_an_input() {
    let server = CaServer::builder()
        .port(0)
        .pv("RM:X", EpicsValue::Double(11.0))
        .pv("RM:Y", EpicsValue::Double(0.0))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });
    // SAFETY: `#[serial(epics_env)]` keeps every env-touching test apart.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let reactor = epics_base_rs::runtime::task::Reactor::current().expect("test runtime");
    let ioc = IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .register_link_type("ca", move |db| async move {
            register_calink_commands(install_calink_resolver(&db, reactor).await)
        })
        .db_string(
            r#"record(swait, "RM:W") { field(INAN, "RM:X") field(OUTN, "RM:Y") field(CALC, "A") field(OOPT, "Every Time") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = Arc::clone(ioc.database());
    assert!(ioc.run().await);
    settle(&db, |db| {
        num(db, "RM:W.INAV") == PV_OK && num(db, "RM:W.OUTV") == PV_OK
    })
    .await;
    assert_eq!(
        (
            num(&db, "RM:W.INAV"),
            num(&db, "RM:W.OUTV"),
            num(&db, "RM:W.SEVR")
        ),
        (PV_OK, PV_OK, 3.0),
        "INAV, OUTV, SEVR before RM:W processes"
    );
    // RM:X's value lands on the link's first monitor, after INAV reads PV OK.
    let deadline = tokio::time::Instant::now() + fact_budget::FACT_BUDGET;
    while num(&db, "RM:W.VAL") != 11.0 && tokio::time::Instant::now() < deadline {
        db.put_record_field_from_ca("RM:W", "PROC", EpicsValue::String("1".into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let y = db.ca_client().create_channel("RM:Y").await.expect("RM:Y");
    let mut y_val = f64::NAN;
    let deadline = tokio::time::Instant::now() + fact_budget::FACT_BUDGET;
    while y_val != 11.0 && tokio::time::Instant::now() < deadline {
        if let Ok(got) = y.get(DBR_DOUBLE, 1).await {
            y_val = got.value.to_f64().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        (
            num(&db, "RM:W.VAL"),
            num(&db, "RM:W.SEVR"),
            num(&db, "RM:W.STAT"),
            y_val
        ),
        (11.0, 0.0, 0.0, 11.0),
        "RM:W VAL SEVR STAT and RM:Y after RM:W processed"
    );
    // SAFETY: `#[serial(epics_env)]`.
    unsafe {
        std::env::remove_var("EPICS_CA_SERVER_PORT");
    }
}
