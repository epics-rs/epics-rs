//! A `pva://` link is a `PV_LINK` to C: `dbParseLink` reads no scheme, so
//! `dbDbInitLink` finds no local record and `dbInitLink` makes it a
//! `CA_LINK` (`dbLink.c:76-110`). sseq (`sseqRecord.c:717`) and scalcout
//! (`devsCalcoutSoft.c:78`) therefore put to it with a callback and wait;
//! the callback must complete for WTGn and PACT to clear. The target's FLNK
//! is a calcout with a 1 s ODLY, so the blocking put stays outstanding long
//! enough to be seen.
//!
//! Tokio only: the PVA server is.
#![cfg(all(feature = "pvalink", tokio_backend))]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_bridge_rs::pvalink::install_pvalink_resolver;
use epics_pva_rs::server::PvDatabaseSource;
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};

const TARGET: &str = r#"
record(ao, "T") { field(FLNK, "D") }
record(calcout, "D") { field(ODLY, "1") field(CALC, "1") }
"#;

const WAITERS: &str = r#"
record(sseq, "S") { field(DO1, "3") field(LNK1, "pva://T") field(WAIT1, "Wait") }
record(scalcout, "W") { field(CALC, "7") field(OUT, "pva://T") field(WAIT, "Wait") }
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

/// The target IOC behind a PVA server and the waiting records on a second
/// IOC whose pva lset reaches it.
async fn iocs() -> (Arc<PvDatabase>, Arc<PvDatabase>, PvaServer) {
    let (remote, _) = IocBuilder::new()
        .db_string(TARGET, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let server = PvaServer::start(
        Arc::new(PvDatabaseSource::new(remote.clone())),
        PvaServerConfig::isolated(),
    )
    .expect("target PVA server starts");
    // SAFETY: before the PVA client exists; the server's threads read the
    // environment only through `std::env`.
    unsafe {
        std::env::set_var("EPICS_PVA_NAME_SERVERS", server.tcp_addr().to_string());
        std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
    }
    let (db, _) = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_link_type("pva", |db| async move {
            install_pvalink_resolver(&db).await;
            Vec::new()
        })
        .db_string(WAITERS, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    (remote, db, server)
}

/// Processes `rec` once its link reports `Ext PV OK` in `valid`, sees
/// `busy` raised while the target's chain runs, and sees it drop once the
/// put callback lands `want` on T.
async fn waits_and_completes(rec: &str, valid: &str, busy: &str, want: f64) {
    let (remote, db, _server) = iocs().await;
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
async fn sseq_waits_on_a_pva_url_link_until_its_put_completes() {
    waits_and_completes("S", "S.LNK1V", "S.WTG1", 3.0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn scalcout_waits_on_a_pva_url_link_until_its_put_completes() {
    waits_and_completes("W", "W.OUTV", "W.PACT", 7.0).await;
}
