//! sseq (`sseqRecord.c:717`) and scalcout (`devsCalcoutSoft.c:78`) put to a
//! `CA_LINK` with a callback and wait; the callback must complete for WTGn
//! and PACT to clear. The target's FLNK is a calcout with a 1 s ODLY, so the
//! blocking put stays outstanding long enough to be seen.
#![cfg(feature = "client-core")]
#![cfg(tokio_backend)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_ca_rs::calink::calink_link_set_install;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

const TARGET: &str = r#"
record(ao, "T") { field(FLNK, "D") }
record(calcout, "D") { field(ODLY, "1") field(CALC, "1") }
"#;

const WAITERS: &str = r#"
record(sseq, "S") { field(DO1, "3") field(LNK1, "T CA") field(WAIT1, "Wait") }
record(scalcout, "W") { field(CALC, "7") field(OUT, "T CA") field(WAIT, "Wait") }
"#;

fn number(db: &PvDatabase, name: &str) -> f64 {
    db.get_pv(name)
        .ok()
        .and_then(|v| v.to_f64())
        .unwrap_or_else(|| panic!("{name} not readable as a number"))
}

async fn wait_for(db: &PvDatabase, name: &str, want: f64, within: Duration) {
    let deadline = Instant::now() + within;
    while number(db, name) != want {
        assert!(
            Instant::now() < deadline,
            "{name} stayed {} instead of {want}",
            number(db, name)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The target IOC behind a CA server and the waiting records on a second
/// IOC whose ca lset reaches it.
async fn iocs() -> (Arc<PvDatabase>, Arc<PvDatabase>) {
    let server = CaServer::builder()
        .port(0)
        .db_string(TARGET, &HashMap::new())
        .unwrap()
        .build()
        .await
        .expect("target CA server");
    let port = server.udp_port();
    let remote = server.database().clone();
    tokio::spawn(async move { server.run().await });
    // SAFETY: `#[serial(epics_env)]` keeps every env-touching test apart.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let reactor = epics_base_rs::runtime::task::Reactor::current().expect("test runtime");
    let ioc = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_link_type("ca", move |db| calink_link_set_install(db, reactor))
        .db_string(WAITERS, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = Arc::clone(ioc.database());
    assert!(ioc.run().await);
    (remote, db)
}

/// Processes `rec` once its link reports `Ext PV OK` in `valid`, sees
/// `busy` raised while the target's chain runs, and sees it drop once the
/// put callback lands `want` on T.
async fn waits_and_completes(rec: &str, valid: &str, busy: &str, want: f64) {
    let (remote, db) = iocs().await;
    wait_for(&db, valid, 1.0, Duration::from_secs(20)).await;
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .unwrap();
    wait_for(&db, busy, 1.0, Duration::from_millis(900)).await;
    wait_for(&db, busy, 0.0, Duration::from_secs(10)).await;
    assert_eq!(number(&remote, "T"), want);
    assert_eq!(number(&db, &format!("{rec}.PACT")), 0.0);
}

#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn sseq_waits_on_a_ca_link_until_its_put_completes() {
    waits_and_completes("S", "S.LNK1V", "S.WTG1", 3.0).await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn scalcout_waits_on_a_ca_link_until_its_put_completes() {
    waits_and_completes("W", "W.OUTV", "W.PACT", 7.0).await;
}
