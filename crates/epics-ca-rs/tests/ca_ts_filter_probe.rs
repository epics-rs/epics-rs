//! A `ts` value mode claims the type and count its `channelRegisterPost`
//! probe ends at (`ts.c:343-362`, `camessage.c:1202`), and an autosize
//! request reads the field's own count (`camessage.c:512-514`). softIoc
//! R7.0.10 on `record(ai, "T") { field(VAL, "1") field(PINI, "YES") }`:
//!
//! ```text
//! $ cainfo ...                                   Native data type  Element count
//! T.VAL{"ts":{"num":"dbl"}}                      DBF_DOUBLE        1
//! T.VAL{"ts":{"num":"sec"}}                      DBF_DOUBLE        1
//! T.VAL{"ts":{"num":"nsec"}}                     DBF_DOUBLE        1
//! T.VAL{"ts":{"num":"ts"}}                       DBF_DOUBLE        2
//! T.VAL{"ts":{"str":"epics"}}                    DBF_STRING        1
//! T.VAL{"ts":{"str":"iso"}}                      DBF_STRING        1
//! T.VAL{"ts":{"num":"ts"},"arr":{"s":1,"e":1}}   DBF_DOUBLE        1
//! T.VAL{"arr":{"s":1,"e":1},"ts":{"num":"ts"}}   DBF_DOUBLE        2
//! $ caget ...
//! T.VAL{"ts":{"num":"ts"}} 2 1.16048e+09 2.75928e+08
//! T.VAL{"ts":{"str":"epics"}}    2026-10-10 20:38:07.275928
//! T.VAL{"ts":{"str":"iso"}}      2026-10-10T20:38:07.275928+0900
//! T.VAL{"ts":{"num":"ts"},"arr":{"s":1,"e":1}} 2.75928e+08
//! T.VAL{"arr":{"s":1,"e":1},"ts":{"num":"ts"}} 2 1.16048e+09 2.75928e+08
//! $ camonitor ...
//! T.VAL{"ts":{"num":"ts"}}       2026-10-10 20:38:07.275928 1.16048e+09
//! T.VAL{"arr":{"s":1,"e":1},"ts":{"num":"ts"}} 2026-10-10 20:38:07.275928 1.16048e+09
//! ```

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use epics_base_rs::types::DbFieldType;
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::{CaChannel, CaClient};
use epics_ca_rs::server::CaServer;

const DB: &str = r#"
record(ai, "T") { field(VAL, "1") field(PINI, "YES") field(PREC, "3") field(EGU, "mm") field(HOPR, "10") }
"#;

async fn connect(client: &CaClient, name: &str) -> CaChannel {
    let ch = client.create_channel(name);
    ch.wait_connected(fact_budget::FACT_BUDGET)
        .await
        .unwrap_or_else(|e| panic!("{name} connects: {e:?}"));
    ch
}

fn doubles(name: &str, value: &EpicsValue) -> Vec<f64> {
    match value {
        EpicsValue::DoubleArray(v) => v.to_vec(),
        EpicsValue::Double(v) => vec![*v],
        other => panic!("{name}: {other:?}"),
    }
}

fn text(name: &str, value: &EpicsValue) -> String {
    match value {
        EpicsValue::String(s) => s.to_string(),
        other => panic!("{name}: {other:?}"),
    }
}

/// `name`'s claimed type and count, its native read, and the first event
/// of an autosize monitor.
async fn probe(client: &CaClient, name: &str) -> (DbFieldType, u32, EpicsValue, EpicsValue) {
    let ch = connect(client, name).await;
    let info = ch.info().await.expect("info");
    let (_, read) = ch
        .get_with_timeout(fact_budget::FACT_BUDGET)
        .await
        .unwrap_or_else(|e| panic!("read {name}: {e:?}"));
    let mut mon = ch.subscribe().await.expect("subscribe");
    let event = tokio::time::timeout(fact_budget::FACT_BUDGET, mon.recv())
        .await
        .unwrap_or_else(|_| panic!("{name}: no event"))
        .expect("monitor open")
        .expect("event");
    (info.native_type, info.element_count, read, event.value)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ts_value_mode_claims_its_probe_type_and_count() {
    let server = CaServer::builder()
        .port(0)
        .db_string(DB, &Default::default())
        .expect("load db")
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });
    // SAFETY: the only test in this binary; set before `CaClient::new()`.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let client = CaClient::new().await.expect("client");

    let (ty, n, sec, _) = probe(&client, r#"T.VAL{"ts":{"num":"sec"}}"#).await;
    assert_eq!((ty, n), (DbFieldType::Double, 1));
    let sec = doubles("sec", &sec)[0];
    let (ty, n, nsec, _) = probe(&client, r#"T.VAL{"ts":{"num":"nsec"}}"#).await;
    assert_eq!((ty, n), (DbFieldType::Double, 1));
    let nsec = doubles("nsec", &nsec)[0];
    let (ty, n, dbl, _) = probe(&client, r#"T.VAL{"ts":{"num":"dbl"}}"#).await;
    assert_eq!((ty, n), (DbFieldType::Double, 1));
    assert_eq!(doubles("dbl", &dbl), [sec + nsec * 1e-9]);

    for name in [
        r#"T.VAL{"ts":{"num":"ts"}}"#,
        r#"T.VAL{"arr":{"s":1,"e":1},"ts":{"num":"ts"}}"#,
    ] {
        let (ty, n, read, event) = probe(&client, name).await;
        assert_eq!((ty, n), (DbFieldType::Double, 2), "{name}");
        assert_eq!(doubles(name, &read), [sec, nsec], "{name}");
        assert_eq!(doubles(name, &event), [sec], "{name}");
    }
    let name = r#"T.VAL{"ts":{"num":"ts"},"arr":{"s":1,"e":1}}"#;
    let (ty, n, read, event) = probe(&client, name).await;
    assert_eq!((ty, n), (DbFieldType::Double, 1));
    assert_eq!(doubles(name, &read), [nsec]);
    assert_eq!(doubles(name, &event), [nsec]);

    // A value-mode log is a DBF_ULONG: DBR_PRECISION is off, units and
    // limits stay (`dbAccess.c:387-394`). softIoc R7.0.10, `caget -d
    // DBR_CTRL_DOUBLE` with PREC 3, EGU mm, HOPR 10: `{"num":"sec"}`
    // `Units: mm` `Precision: 0` `Hi disp limit: 10`; `{"num":"dbl"}`
    // `Precision: 3`.
    for (name, prec) in [
        (r#"T.VAL{"ts":{"num":"sec"}}"#, 0),
        (r#"T.VAL{"ts":{"num":"dbl"}}"#, 3),
    ] {
        let ch = connect(&client, name).await;
        let got = ch
            .get_with_dbr_type(
                epics_base_rs::types::DBR_CTRL_DOUBLE,
                0u32,
                fact_budget::FACT_BUDGET,
            )
            .await
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(got.precision(), Some(prec), "{name}");
        let display = got.display.as_ref().expect("display");
        assert_eq!(display.units, "mm", "{name}");
        assert_eq!(display.upper_disp_limit, 10.0, "{name}");
    }

    for (name, sep) in [
        (r#"T.VAL{"ts":{"str":"epics"}}"#, ' '),
        (r#"T.VAL{"ts":{"str":"iso"}}"#, 'T'),
    ] {
        let (ty, n, read, event) = probe(&client, name).await;
        assert_eq!((ty, n), (DbFieldType::String, 1), "{name}");
        let read = text(name, &read);
        assert_eq!(read.as_bytes()[10] as char, sep, "{name}: {read}");
        assert_eq!(&read[19..20], ".", "{name}: {read}");
        assert_eq!(text(name, &event), read, "{name}");
    }
}
