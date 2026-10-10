//! A `$` monitor on a long string posts its `LEN` elements raw, the count
//! `get_array_info` reports (`lsiRecord.c:154`, `lsoRecord.c:196`). softIoc
//! R7.0.10 on the records below, `camonitor -tn` on each `.VAL$`, after
//! `caput -S LW.VAL$ ""`:
//!
//! ```text
//! LI.VAL$  UDF INVALID          (LEN 0: no element)
//! LO.VAL$  UDF INVALID
//! LW.VAL$  0                    (LEN 1: the NUL)
//! ```
//!
//! `devEnviron.c::read_lsi` sets `len = strlen`, leaving the NUL out; with
//! `LSPIN=hello`, `LE.VAL$  5 104 101 108 108 111`.
//!
//! A number constant INP loads no text and `LEN` 1 (`dbConvertJSON.c`
//! `dbLSConvertJSON`): `LN.VAL$  0 UDF INVALID`, one element.
//!
//! A raw `$` put with an embedded NUL, `caput -a LX.VAL$ 4 97 0 98 99`, keeps
//! `LEN = strlen + 1` (`lsiRecord.c`/`lsoRecord.c` `special`): `LX.VAL$  2 97 0`,
//! and the same for the lso `LY`.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

#[path = "common/budget.rs"]
mod budget;

use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::{CaChannel, CaClient};
use epics_ca_rs::server::CaServer;

const DB: &str = r#"
record(lsi, "LI") { field(SIZV, "100") }
record(lso, "LO") { field(SIZV, "100") }
record(lsi, "LW") { field(SIZV, "100") }
record(lsi, "LN") { field(INP, "5") field(SIZV, "100") }
record(lsi, "LX") { field(SIZV, "100") }
record(lso, "LY") { field(SIZV, "100") }
record(lsi, "LE") { field(DTYP, "getenv") field(INP, "@LSPIN") field(SIZV, "100") }
"#;

async fn connect(client: &CaClient, name: &str) -> CaChannel {
    let ch = client.create_channel(name);
    ch.wait_connected(budget::FACT_BUDGET)
        .await
        .unwrap_or_else(|e| panic!("{name} connects: {e:?}"));
    ch
}

/// The elements of the first event an autosize monitor (`camonitor`'s
/// default) receives.
async fn first_event(client: &CaClient, name: &str) -> Vec<u8> {
    let mut mon = connect(client, name)
        .await
        .subscribe()
        .await
        .unwrap_or_else(|e| panic!("{name} subscribes: {e:?}"));
    let snap = tokio::time::timeout(budget::FACT_BUDGET, mon.recv())
        .await
        .unwrap_or_else(|_| panic!("{name}: no event"))
        .expect("monitor open")
        .expect("event");
    match snap.value {
        EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) => b.to_vec(),
        EpicsValue::Char(c) | EpicsValue::UChar(c) => vec![c],
        other => panic!("{name}: {other:?}"),
    }
}

async fn put_chars(client: &CaClient, name: &str, chars: &[u8]) {
    connect(client, name)
        .await
        .put(&EpicsValue::CharArray(chars.to_vec().into()))
        .await
        .unwrap_or_else(|e| panic!("put {name}: {e:?}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dollar_monitor_posts_len_elements() {
    // SAFETY: the only test in this binary; set before the IOC reads it.
    unsafe { std::env::set_var("LSPIN", "hello") };
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

    put_chars(&client, "LW.VAL$", b"\0").await;

    assert_eq!(first_event(&client, "LI.VAL$").await, b"");
    assert_eq!(first_event(&client, "LO.VAL$").await, b"");
    assert_eq!(first_event(&client, "LW.VAL$").await, b"\0");
    assert_eq!(first_event(&client, "LN.VAL$").await, b"\0");
    for name in ["LX.VAL$", "LY.VAL$"] {
        put_chars(&client, name, b"a\0bc").await;
        assert_eq!(first_event(&client, name).await, b"a\0", "{name}");
    }
    connect(&client, "LE.PROC")
        .await
        .put(&EpicsValue::UChar(1))
        .await
        .expect("process LE");
    assert_eq!(first_event(&client, "LE.VAL$").await, b"hello");
}
