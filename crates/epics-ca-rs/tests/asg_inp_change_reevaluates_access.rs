//! A1 — an ASG `INP*` value change must re-evaluate live access rights.
//!
//! C monitors every ASG input link over CA (`asCa.c:180-205`) and each update
//! runs
//!
//! ```c
//! pasg->inpChanged |= (1<<idx);
//! if(!caInitializing) asComputeAsg(pasg);
//! ```
//!
//! (`asCa.c:148-161`), reaching `asComputePvt` (`asLibRoutines.c:1049-1051`),
//! which fires `asClientCOAR` — the `CA_PROTO_ACCESS_RIGHTS` push — for every
//! client whose level moved.
//!
//! The port recomputed a channel's level only on an ACF reload or a write to a
//! record's `ASG` field, and cached it per channel at create time
//! (`tcp.rs` `state.channel_access`). Closing a `CALC`-gated interlock
//! therefore left every already-connected client holding the WRITE grant it
//! was handed while the gate was open — the interlock never closed for the
//! clients it existed to stop. The mirror case was equally live: a client that
//! connected while the gate was shut stayed read-only after it opened.
//!
//! Both directions are asserted from the client's cached access bits, which
//! move only when a `CA_PROTO_ACCESS_RIGHTS` frame arrives
//! (`client/transport.rs:2781`), so this is the wire and not the server's own
//! cache.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use std::time::Duration;

use epics_base_rs::types::EpicsValue;
use epics_ca_rs::client::{CaChannel, CaClient};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// `GATE` carries the interlock the ACF reads; `IFACE` is what it protects.
fn db(gate: i32) -> String {
    format!(
        r#"
record(ai, "GATE") {{
    field(VAL, "{gate}")
    field(ASG, "OPEN")
}}
record(ai, "IFACE") {{
    field(ASG, "GATED")
}}
"#
    )
}

/// `GATED` grants WRITE only while `GATE = 1`. `A=1` evaluates to 1.0 inside
/// C's `(0.99, 1.01)` truth band and to 0.0 outside it.
const ACF: &str = r#"
ASG(DEFAULT) {
    RULE(1, READ)
}
ASG(OPEN) {
    RULE(1, READ)
    RULE(1, WRITE)
}
ASG(GATED) {
    INPA("GATE")
    RULE(1, READ)
    RULE(1, WRITE) {
        CALC("A=1")
    }
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

/// Serve the database under `ACF` with `GATE` starting at `gate`, and return
/// the port.
async fn serve(gate: i32) -> u16 {
    let server = CaServer::builder()
        .port(0)
        .db_string(&db(gate), &std::collections::HashMap::new())
        .expect("load db")
        .acf(epics_base_rs::server::access_security::parse_acf(ACF).expect("parse acf"))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });
    port
}

async fn connect(client: &CaClient, pv: &str) -> CaChannel {
    let ch = client.create_channel(pv);
    ch.wait_connected(budget::FACT_BUDGET)
        .await
        .unwrap_or_else(|e| panic!("{pv} must connect: {e:?}"));
    ch
}

/// The client's cached write bit, which only an ACCESS_RIGHTS frame moves.
async fn write_bit(ch: &CaChannel) -> bool {
    ch.info().await.expect("channel info").access_rights.write
}

/// Wait for the wire to carry the expected transition.
async fn await_write_bit(ch: &CaChannel, want: bool, what: &str) {
    let deadline = std::time::Instant::now() + budget::FACT_BUDGET;
    loop {
        if write_bit(ch).await == want {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: no CA_PROTO_ACCESS_RIGHTS carrying write={want} within 5 s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Shutting the interlock under a connected client must strip its WRITE.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn closing_the_gate_revokes_a_live_write_grant() {
    let port = serve(1).await;
    point_client_at(port);
    let client = CaClient::new().await.expect("client");

    let iface = connect(&client, "IFACE").await;
    assert!(
        write_bit(&iface).await,
        "GATE=1 puts CALC(\"A=1\") inside the truth band, so the channel is \
         created with WRITE"
    );
    iface
        .put(&EpicsValue::Double(1.0))
        .await
        .expect("write is granted while the gate is open");

    let gate = connect(&client, "GATE").await;
    gate.put(&EpicsValue::Double(0.0))
        .await
        .expect("GATE is in the OPEN group");

    await_write_bit(&iface, false, "gate shut").await;
    assert!(
        iface.put(&EpicsValue::Double(2.0)).await.is_err(),
        "the interlock must refuse the write once the gate is shut"
    );
}

/// The mirror: opening the interlock must hand WRITE to a client that
/// connected while it was shut.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn opening_the_gate_grants_write_to_a_connected_client() {
    let port = serve(0).await;
    point_client_at(port);
    let client = CaClient::new().await.expect("client");

    let iface = connect(&client, "IFACE").await;
    assert!(
        !write_bit(&iface).await,
        "GATE=0 leaves CALC(\"A=1\") at 0, so the channel is created read-only"
    );

    let gate = connect(&client, "GATE").await;
    gate.put(&EpicsValue::Double(1.0))
        .await
        .expect("GATE is in the OPEN group");

    await_write_bit(&iface, true, "gate opened").await;
    iface
        .put(&EpicsValue::Double(3.0))
        .await
        .expect("the write must be granted once the gate is open");
}

#[path = "common/budget.rs"]
mod budget;
