//! Spec §5 test 15: an OUT link that is a CA link takes none of the dbPut
//! path. A local target named with `CA` is written through the IOC's own
//! CA server and processed with its own alarm; an unresolved one (`CA`, or
//! a bare unknown name, which C makes a CA link) alarms the source
//! LINK/INVALID "field OUT" (`dbCa.c:558-560`, `dbLink.c:443-446`). Pinned
//! to oracleStdIoc (C ground truth §5.15, its DB verbatim).
#![cfg(feature = "client-core")]
#![cfg(tokio_backend)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::calink::install_calink_resolver;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

const DB: &str = r#"
record(ao,"TT"){field(HIHI,"10") field(HHSV,"MAJOR")}
record(ao,"SC"){field(OUT,"TT CA")}
record(ao,"SN"){field(OUT,"NOPE CA")}
record(ao,"SN2"){field(OUT,"NOPE2")}
"#;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// `(STAT, SEVR)`.
fn alarm(db: &PvDatabase, rec: &str) -> (f64, f64) {
    (
        num(db, &format!("{rec}.STAT")),
        num(db, &format!("{rec}.SEVR")),
    )
}

fn text(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{pv} = {other:?}"),
    }
}

/// Retries the put until the link is up, then waits for `want` on `pv`.
async fn put_until(db: &PvDatabase, src: &str, v: f64, pv: &str, want: f64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let _ = db
            .put_record_field_from_ca(src, "VAL", EpicsValue::Double(v))
            .await;
        for _ in 0..20 {
            if num(db, pv) == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(Instant::now() < deadline, "{pv} never reached {want}");
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_ca_out_link_writes_a_local_target_and_alarms_on_an_unresolved_one() {
    let server = CaServer::builder()
        .port(0)
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let port = server.udp_port();
    // SAFETY: `#[serial(epics_env)]` keeps every env-touching test apart.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let db = server.database().clone();
    let resolver = install_calink_resolver(
        &db,
        epics_base_rs::runtime::task::Reactor::current().expect("test runtime"),
    )
    .await;
    // iocInit's open pass, which the build ran before the link set existed:
    // C opens every CA link at `dbCaAddLink`, so the first put through an
    // unresolved one is already refused (`dbCa.c:558-560`).
    db.setup_external_link_opens().await;
    let _server = tokio::spawn(async move { server.run().await });

    put_until(&db, "SC", 5.0, "TT", 5.0).await;
    assert_eq!(alarm(&db, "TT"), (0.0, 0.0));
    assert_eq!(alarm(&db, "SC"), (0.0, 0.0));

    put_until(&db, "SC", 20.0, "TT", 20.0).await;
    assert_eq!(alarm(&db, "TT"), (3.0, 2.0), "TT's own HIHI/MAJOR");
    assert_eq!(alarm(&db, "SC"), (0.0, 0.0));

    let deadline = Instant::now() + Duration::from_secs(10);
    while resolver.link_count() < 3 {
        assert!(Instant::now() < deadline, "the CA link opens never ran");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for src in ["SN", "SN2"] {
        let _ = db
            .put_record_field_from_ca(src, "VAL", EpicsValue::Double(5.0))
            .await;
        assert_eq!(alarm(&db, src), (14.0, 3.0), "{src}");
        assert_eq!(text(&db, &format!("{src}.AMSG")), "field OUT", "{src}");
    }
}
