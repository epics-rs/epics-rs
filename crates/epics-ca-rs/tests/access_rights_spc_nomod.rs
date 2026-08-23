//! R18-90: an `SPC_NOMOD` field must advertise `no write` in ACCESS_RIGHTS.
//!
//! C declares no-modify once and reads it in two unrelated places. The port had
//! only the first:
//!
//! * `dbPut` (`dbAccess.c:123-126`) — refuses the write. The port's gate.
//! * `rsrvCheckPut` (`rsrv/camessage.c:2608-2619`) —
//!   `if (dbChannelSpecial(pciu->dbch) == SPC_NOMOD) return 0;` — which feeds
//!   the `CA_PROTO_ACCESS_RIGHTS` write bit (`camessage.c:1154-1156`). Missing
//!   here, so the server advertised WRITE on ~15 dbCommon fields of every
//!   record; medm/CSS enable the write widget, the client sends the put, and
//!   the server-side gate refuses it with an async exception instead of C's
//!   clean client-side "Write access denied".
//!
//! Oracle — softIoc 7.0.10.1-DEV, `N1` (ai) and `CMP` (compress,
//! `BALG="LIFO Buffer"`):
//!
//! ```text
//! cainfo N1        Access: read, write
//! cainfo N1.SEVR   Access: read, no write
//! cainfo N1.NAME   Access: read, no write
//! cainfo N1.PACT   Access: read, no write
//! cainfo CMP       Access: read, no write     <- compressRecord.c:403-404,
//!                                                cvt_dbaddr raises SPC_NOMOD
//!                                                from record STATE
//! caput N1.SEVR 2  ERROR: Write access denied
//! caput CMP 1      ERROR: Write access denied
//! ```
//!
//! Both halves of the declaration are covered: the dbCommon set (which no
//! record's `field_list` declares) and the state-raised `field_no_mod`.

// Host/tokio-only: builds the async `CaClient`/`CaServer` stack in process.
// Under `rtems-exec-model` the `runtime::task` seam routes their `spawn`
// to the background executor, whose worker has no tokio reactor, so the
// listener/transport tasks panic. The RTEMS model serves from
// `BlockingCaServer` instead, so this path is inapplicable there.
#![cfg(not(feature = "rtems-exec-model"))]

use std::time::Duration;

use epics_ca_rs::client::CaClient;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

const DB: &str = r#"
record(ai, "N1") {
    field(VAL, "1")
}
record(compress, "CMP") {
    field(NSAM, "10")
    field(BALG, "LIFO Buffer")
}
"#;

fn point_client_at(port: u16) {
    // SAFETY: this file's tests are `#[serial]` and set the env before
    // `CaClient::new()` snapshots its resolver configuration.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

async fn serve() -> u16 {
    let server = CaServer::builder()
        .port(0)
        .db_string(DB, &std::collections::HashMap::new())
        .expect("load db")
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });
    port
}

/// `(read, write)` as the connected channel's ACCESS_RIGHTS reports them.
async fn access(client: &CaClient, pv: &str) -> (bool, bool) {
    let ch = client.create_channel(pv);
    ch.wait_connected(Duration::from_secs(3))
        .await
        .unwrap_or_else(|e| panic!("{pv} must connect: {e:?}"));
    let info = ch.info().await.expect("channel info");
    (info.access_rights.read, info.access_rights.write)
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn dbcommon_nomod_fields_advertise_no_write() {
    let port = serve().await;
    point_client_at(port);
    let client = CaClient::new().await.expect("client");

    assert_eq!(
        access(&client, "N1").await,
        (true, true),
        "VAL is writable — the gate must not over-reach"
    );

    // TIME is in the NOMOD set but is not a CA-resolvable channel in this port
    // (as in C, where it is DBF_NOACCESS and `dbNameToAddr` refuses it), so it
    // has no ACCESS_RIGHTS to assert.
    for field in ["SEVR", "STAT", "NAME", "PACT", "RPRO", "ACKS"] {
        let pv = format!("N1.{field}");
        assert_eq!(
            access(&client, &pv).await,
            (true, false),
            "{pv}: C's rsrvCheckPut returns 0 for an SPC_NOMOD field, so \
             ACCESS_RIGHTS carries read and NOT write"
        );
    }
}

/// The other half of the declaration: an SPC_NOMOD a record raises from its own
/// state (`compressRecord.c:403-404` — `if (prec->balg == bufferingALG_LIFO)
/// paddr->special = SPC_NOMOD;`), which no static field table can express.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn state_raised_nomod_advertises_no_write() {
    let port = serve().await;
    point_client_at(port);
    let client = CaClient::new().await.expect("client");

    assert_eq!(
        access(&client, "CMP").await,
        (true, false),
        "compress VAL under BALG=LIFO is SPC_NOMOD: `cainfo CMP` reports \
         `read, no write` on softIoc"
    );
}

/// And the wire bits agree with the gate: a client that ignores the advertised
/// bits still gets the put refused.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_put_to_a_nomod_field_is_still_refused() {
    let port = serve().await;
    point_client_at(port);
    let client = CaClient::new().await.expect("client");

    let ch = client.create_channel("N1.SEVR");
    ch.wait_connected(Duration::from_secs(3))
        .await
        .expect("connect");

    let put = ch.put(&epics_base_rs::types::EpicsValue::Short(2)).await;
    assert!(
        put.is_err(),
        "SPC_NOMOD refuses the write on every route; got {put:?}"
    );
}
