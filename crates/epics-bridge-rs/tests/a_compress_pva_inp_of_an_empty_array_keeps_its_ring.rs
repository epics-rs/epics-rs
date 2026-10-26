//! compress refuses a PVA INP whose array is empty before it touches the
//! ring: pvxs `pvaGetElements` answers the array's size, 0
//! (`pvalink_lset.cpp:243-258`), and `nelements <= 0` raises LINK/INVALID
//! with neither the reset nor the read (`compressRecord.c:328-331`).
//! softIocPVX (pvxs 1.5.3), W and E waveforms DOUBLE NELM=16, E never
//! written:
//!
//! ```text
//! CE                   : INPN=0 NUSE=0 VAL=(empty)  LINK/INVALID
//! W=[1,2,3]  CP        : INPN=3 NUSE=3 VAL=1 2 3    NO_ALARM
//! W=[]       CP        : INPN=3 NUSE=3 VAL=1 2 3    LINK/INVALID
//! ```
#![cfg(all(feature = "pvalink", tokio_backend))]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;
use epics_bridge_rs::pvalink::install_pvalink_resolver;
use epics_pva_rs::server::PvDatabaseSource;
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};

const REMOTE: &str = r#"
record(waveform, "W") { field(FTVL, "DOUBLE") field(NELM, "16") }
record(waveform, "E") { field(FTVL, "DOUBLE") field(NELM, "16") }
"#;

const DB: &str = r#"
record(compress, "CP") { field(INP, {pva:"W"}) field(ALG, "Circular Buffer") field(NSAM, "40") }
record(compress, "CE") { field(INP, {pva:"E"}) field(ALG, "Circular Buffer") field(NSAM, "40") }
"#;

type State = (i64, i64, EpicsValue, AlarmSeverity, u16);

async fn process(db: &PvDatabase, rec: &str) -> State {
    db.process_record(rec)
        .await
        .or_else(epics_base_rs::error::CaError::unless_process_status)
        .unwrap();
    let num = |f: &str| db.get_pv(format!("{rec}.{f}")).unwrap().to_f64().unwrap() as i64;
    let inst = db.get_record(rec).unwrap();
    let c = &inst.read().common;
    (
        num("INPN"),
        num("NUSE"),
        db.get_pv(rec).unwrap(),
        c.sevr,
        c.stat,
    )
}

/// Waits until `rec.INP` reports `count` elements.
async fn until_count(db: &PvDatabase, rec: &str, count: i64) {
    let (_, _, inp) = db
        .record_link_fields(rec)
        .into_iter()
        .find(|(field, _, _)| field == "INP")
        .unwrap();
    let inp = inp.external().expect("a PVA link");
    let deadline = Instant::now() + Duration::from_secs(20);
    while db.external_link_metadata(inp).and_then(|m| m.element_count) != Some(count) {
        assert!(Instant::now() < deadline, "{rec}.INP never counted {count}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn state(inpn: i64, val: &[f64], (sevr, stat): (AlarmSeverity, u16)) -> State {
    (
        inpn,
        val.len() as i64,
        EpicsValue::DoubleArray(val.to_vec().into()),
        sevr,
        stat,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_pva_source_is_refused_before_the_ring() {
    let (remote, _) = IocBuilder::new()
        .db_string(REMOTE, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let put = |value: Vec<f64>| {
        let remote = remote.clone();
        async move {
            remote
                .put_record_field_from_ca("W", "VAL", EpicsValue::DoubleArray(value.into()))
                .await
                .unwrap();
        }
    };
    put(vec![1.0, 2.0, 3.0]).await;
    let server = PvaServer::start(
        Arc::new(PvDatabaseSource::new(remote.clone())),
        PvaServerConfig::isolated(),
    )
    .expect("source PVA server starts");
    // SAFETY: before the reader IOC builds its PVA client; nothing else in
    // this test binary touches the environment.
    unsafe {
        std::env::set_var("EPICS_PVA_NAME_SERVERS", server.tcp_addr().to_string());
        std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
    }

    let (db, _) = IocBuilder::new()
        .register_link_type("pva", |db| async move {
            install_pvalink_resolver(&db).await;
            Vec::new()
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db.setup_external_link_opens().await;
    let invalid = (AlarmSeverity::Invalid, alarm_status::LINK_ALARM);
    let clean = (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM);

    until_count(&db, "CE", 0).await;
    assert_eq!(process(&db, "CE").await, state(0, &[], invalid));

    until_count(&db, "CP", 3).await;
    assert_eq!(process(&db, "CP").await, state(3, &[1.0, 2.0, 3.0], clean));

    put(vec![]).await;
    until_count(&db, "CP", 0).await;
    assert_eq!(
        process(&db, "CP").await,
        state(3, &[1.0, 2.0, 3.0], invalid)
    );
}
