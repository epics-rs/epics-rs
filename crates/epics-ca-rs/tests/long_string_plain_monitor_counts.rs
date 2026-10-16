//! A plain monitor of a long string carries `min(LEN, 1)` elements: `dbGet`
//! takes `no_elements` from `get_array_info` (`LEN`, `OLEN` for `OVAL`) and
//! clamps it to the `cvt_dbaddr` capacity of 1 (`dbAccess.c:936-1001`).
//! softIoc R7.0.10 on the records below, before any write except
//! `caput LW.VAL ""`:
//!
//! ```text
//! camonitor -tn LO.VAL        LO.VAL                          UDF INVALID
//! camonitor -# 1 -tn LO.VAL   LO.VAL                          1  UDF INVALID
//! camonitor -tn LW.VAL        LW.VAL
//! ```
//!
//! and the same for `LO.OVAL`, `LI.VAL`, `LI.OVAL`, `PF.VAL` (none), and
//! `LW.OVAL` (one): the autosize event of a `LEN` 0 field has no element, and
//! an explicit count of 1 is zero-filled.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::{CaChannel, CaClient, EnumReadback};
use epics_ca_rs::server::CaServer;

const DB: &str = r#"
record(lso, "LO") { field(SIZV, "100") }
record(lsi, "LI") { field(SIZV, "100") }
record(printf, "PF") { field(SIZV, "100") }
record(lso, "LW") { field(SIZV, "100") }
"#;

async fn connect(client: &CaClient, name: &str) -> CaChannel {
    let ch = client.create_channel(name);
    ch.wait_connected(fact_budget::FACT_BUDGET)
        .await
        .unwrap_or_else(|e| panic!("{name} connects: {e:?}"));
    ch
}

/// The strings of the first event a monitor with `count` (`None`: autosize)
/// receives.
async fn first_event(client: &CaClient, name: &str, count: Option<u32>) -> Vec<String> {
    let mut mon = connect(client, name)
        .await
        .subscribe_with_mask_readback_count(0.0, 0x5, EnumReadback::Native, false, count)
        .await
        .unwrap_or_else(|e| panic!("{name} subscribes: {e:?}"));
    let snap = tokio::time::timeout(fact_budget::FACT_BUDGET, mon.recv())
        .await
        .unwrap_or_else(|_| panic!("{name}: no event"))
        .expect("monitor open")
        .expect("event");
    match snap.value {
        EpicsValue::String(s) => vec![s.as_str_lossy().into_owned()],
        EpicsValue::StringArray(v) => v.iter().map(|s| s.as_str_lossy().into_owned()).collect(),
        other => panic!("{name}: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_monitor_carries_min_len_one_elements() {
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

    connect(&client, "LW.VAL")
        .await
        .put(&EpicsValue::String("".into()))
        .await
        .expect("put LW");

    let none: Vec<String> = Vec::new();
    for name in ["LO.VAL", "LO.OVAL", "LI.VAL", "LI.OVAL", "PF.VAL"] {
        assert_eq!(first_event(&client, name, None).await, none, "{name}");
        assert_eq!(
            first_event(&client, name, Some(1)).await,
            [""],
            "{name} -# 1"
        );
    }
    for name in ["LW.VAL", "LW.OVAL"] {
        assert_eq!(first_event(&client, name, None).await, [""], "{name}");
    }
}
