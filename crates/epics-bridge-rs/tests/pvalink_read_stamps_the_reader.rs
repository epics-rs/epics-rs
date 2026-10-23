//! pvxs `pvaGetValue` latches the source's `timeStamp` and `userTag` into the
//! link's snapshot on every read and, for a `time:true` link, copies the pair
//! into the record holding the link, whichever field that is
//! (`pvalink_lset.cpp:268-272`, `:396-442`). `pvaGetTimeStampTag` answers
//! from the same snapshot for any valid link (`:585-601`), so a `TSE=-2` soft
//! input takes the source time without the option, and only the time
//! (`dbGetTimeStamp` passes no tag). softIocPVX (pvxs 1.5.3) on this database,
//! SRC served with `info(Q:time:tag, "nsec:lsb:20")`:
//!
//! ```text
//! C1  TIME SRC (split)   UTAG SRC low bits
//! C0  TIME <undefined>   UTAG 0
//! A1  TIME SRC (split)   UTAG 0
//! AT  TIME SRC (split)   UTAG SRC low bits
//! A0  TIME now           UTAG SRC low bits
//! AN  TIME <undefined>   UTAG 0      after a TSE=0 cycle, then TSE=-2
//! CN  TIME <undefined>   UTAG 0      the same
//! R   TIME X             UTAG SRC low bits   after INP moves to "X CA"
//! ```
#![cfg(all(feature = "pvalink", tokio_backend))]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::runtime::general_time::epics_epoch;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::{EpicsValue, WallTime};
use epics_bridge_rs::pvalink::install_pvalink_resolver;
use epics_pva_rs::server::PvDatabaseSource;
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};

const REMOTE: &str = r#"
record(ai, "SRC") { field(VAL, "3") info(Q:time:tag, "nsec:lsb:20") }
"#;

const DB: &str = r#"
record(calc, "C1") { field(TSE, "-2") field(INPA, {pva:{pv:"SRC", time:true}}) field(CALC, "A") }
record(calc, "C0") { field(TSE, "-2") field(INPA, {pva:{pv:"SRC"}}) field(CALC, "A") }
record(ai, "A1") { field(TSE, "-2") field(INP, {pva:{pv:"SRC"}}) }
record(ai, "AT") { field(TSE, "-2") field(INP, {pva:{pv:"SRC", time:true}}) }
record(ai, "A0") { field(INP, {pva:{pv:"SRC", time:true}}) }
record(ai, "AN") { field(INP, {pva:{pv:"NOWHERE", time:true}}) }
record(calc, "CN") { field(INPA, {pva:{pv:"NOWHERE", time:true}}) field(CALC, "A") }
record(ai, "X") { field(VAL, "4") }
record(ai, "R") { field(TSE, "-2") field(INP, {pva:{pv:"SRC", time:true}}) }
"#;

const LSB: u32 = (1 << 20) - 1;

async fn process(db: &PvDatabase, rec: &str) -> (WallTime, u64, AlarmSeverity) {
    db.process_record(rec)
        .await
        .or_else(epics_base_rs::error::CaError::unless_process_status)
        .unwrap();
    let cell = db.get_record(rec).unwrap();
    let inst = cell.read();
    (inst.common.time, inst.common.utag, inst.common.sevr)
}

async fn dbpf(db: &PvDatabase, rec: &str, field: &str, value: EpicsValue) {
    db.put_record_field_from_ca(rec, field, value)
        .await
        .unwrap();
}

/// Processes `rec` until it reads without an alarm.
async fn until_clean(db: &PvDatabase, rec: &str) -> (WallTime, u64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (time, utag, sevr) = process(db, rec).await;
        if sevr == AlarmSeverity::NoAlarm {
            return (time, utag);
        }
        assert!(Instant::now() < deadline, "{rec} never read cleanly");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pva_read_stamps_the_reader_as_pvxs_does() {
    let (remote, _) = IocBuilder::new()
        .db_string(REMOTE, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    // A source time whose low bits are a non-zero tag.
    let src = loop {
        let (time, ..) = process(&remote, "SRC").await;
        if time.subsec_nanos() & LSB != 0 {
            break time;
        }
    };
    let served = WallTime::from_unix(src.unix_secs(), src.subsec_nanos() & !LSB);
    let tag = u64::from(src.subsec_nanos() & LSB);
    let server = PvaServer::start(
        Arc::new(PvDatabaseSource::new(remote)),
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
    // dbCaRun, which a local "X CA" link waits for.
    db.process_cp_links_on_connect().await;
    for rec in ["C1", "C0", "A1", "AT", "A0", "R"] {
        until_clean(&db, rec).await;
    }
    let undefined = (epics_epoch(), 0);

    assert_eq!(until_clean(&db, "C1").await, (served, tag), "C1");
    assert_eq!(until_clean(&db, "C0").await, undefined, "C0");
    assert_eq!(until_clean(&db, "A1").await, (served, 0), "A1");
    assert_eq!(until_clean(&db, "AT").await, (served, tag), "AT");
    let (time, utag) = until_clean(&db, "A0").await;
    assert_ne!(time, served, "A0 TSE=0 restamps the time");
    assert_eq!(utag, tag, "A0 keeps the tag");

    for rec in ["AN", "CN"] {
        let (time, _, sevr) = process(&db, rec).await;
        assert_eq!(sevr, AlarmSeverity::Invalid, "{rec}");
        assert_ne!(time, epics_epoch(), "{rec} TSE=0 stamps the cycle");
        dbpf(&db, rec, "TSE", EpicsValue::Short(-2)).await;
        let (time, utag, _) = process(&db, rec).await;
        assert_eq!((time, utag), undefined, "{rec} never connected");
    }

    let (x, ..) = process(&db, "X").await;
    dbpf(&db, "R", "INP", EpicsValue::String("X CA".into())).await;
    assert_eq!(until_clean(&db, "R").await, (x, tag), "R");
}
