//! The CA status of a refused put is decided by the LAYER that refused it,
//! never by which database error came back.
//!
//! C `write_action` (`rsrv/camessage.c:773-789`) answers a failed
//! `dbChannel_put` with `ECA_PUTFAIL` whatever `dbStatus` was, and
//! `write_notify_reply` (`camessage.c:1386-1390`) maps every
//! `status != notifyOK` to that same `ECA_PUTFAIL`. `ECA_BADTYPE` is
//! produced only by the gates ABOVE the put — the DBR-type check and
//! `caNetConvert` (`camessage.c:753-759`) — which run before the database is
//! touched and tear the connection down.
//!
//! Measured on the C softIoc (`softIoc -S -d`, `record(ai,"MEAS:A") {}`):
//!
//! ```text
//! caput MEAS:A.PHAS 32768      -> "Channel write request failed"   (ECA_PUTFAIL)
//! caput MEAS:A.RVAL notanumber -> "Channel write request failed"   (ECA_PUTFAIL)
//! caput MEAS:A.STAT 3          -> "Write access denied"            (ECA_NOWTACCESS)
//! ```
//!
//! The port used to derive the reply from the error variant, so a value the
//! field's converter refused surfaced as `ECA_BADTYPE` — "The data type
//! specified is invalid" — for a put whose wire type was perfectly valid.
//! These cases are the boundary between the two layers: same channel, same
//! wire type, only the reason for the refusal differs.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use epics_base_rs::error::CaError;
use epics_ca_rs::client::{CaClient, CaClientConfig};
use epics_ca_rs::protocol::{ECA_BADTYPE, ECA_PUTFAIL};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// The ECA status the server put on the wire for a refused put.
fn eca_of(err: &CaError) -> u32 {
    match err {
        CaError::WriteFailed(code) | CaError::ServerError(code) => *code,
        other => panic!("expected a server-reported put failure, got {other:?}"),
    }
}

/// Host one `ai` record on a CA server bound to port 0, and return a client
/// pinned to the port the server actually bound.
async fn serve_one_ai() -> (CaClient, tokio::task::JoinHandle<()>) {
    let server = CaServer::builder()
        .port(0)
        .db_string(
            "record(ai, \"PUTSTAT:A\") {}",
            &std::collections::HashMap::new(),
        )
        .expect("load db")
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let handle = tokio::spawn(async move {
        let _ = server.run().await;
    });

    // SAFETY: the tests that touch the process-wide EPICS env are
    // `#[serial(epics_env)]`, so no other thread reads/writes these
    // concurrently.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let client = CaClient::new_with_config(CaClientConfig::default())
        .await
        .expect("CA client");
    (client, handle)
}

/// Every refusal that comes from INSIDE the database put is `ECA_PUTFAIL`,
/// whatever the record layer's own error was — C's `dbStatus < 0`.
///
/// Both cases send a valid `DBR_STRING` on a channel the peer may write, so
/// the wire gates and the access gate pass and only the field's converter
/// refuses: `32768` is one over `DBF_SHORT`'s maximum, and `notanumber` does
/// not parse at all. C answers both with "Channel write request failed".
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_value_the_field_refuses_is_putfail_not_badtype() {
    let (client, server) = serve_one_ai().await;

    for (field, value) in [("PHAS", "32768"), ("RVAL", "notanumber")] {
        let chan = client.create_channel(&format!("PUTSTAT:A.{field}"));
        chan.wait_connected(budget::FACT_BUDGET)
            .await
            .expect("connect");

        let err = chan
            .put_string(value)
            .await
            .expect_err("the field's converter must refuse this value");
        let eca = eca_of(&err);
        assert_ne!(
            eca, ECA_BADTYPE,
            ".{field} <- {value:?}: the wire type (DBR_STRING) is valid — a value \
             the converter refuses is not a type error (C caNetConvert passed)"
        );
        assert_eq!(
            eca, ECA_PUTFAIL,
            ".{field} <- {value:?}: C `write_action` answers every failed \
             dbChannel_put with ECA_PUTFAIL (camessage.c:773-789)"
        );
    }

    server.abort();
}

/// The refusal that comes from ABOVE the put keeps its own status, and it
/// never reaches the put at all: a `special(SPC_NOMOD)` field is advertised
/// without write access, so the CLIENT refuses the write from its cached
/// access rights — C libca does the same (`nciu::write`), which is why the C
/// `caput MEAS:A.STAT 3` measurement prints "Write access denied" rather than
/// a server-side "Channel write request failed".
///
/// This is the other side of the boundary: the two refusals must not collapse
/// onto one status. The server-side half of it — `ReadOnlyField` staying
/// `ECA_NOWTACCESS` while every value-level refusal becomes `ECA_PUTFAIL` — is
/// pinned on `PutStatus` itself, in `server::tcp`.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_field_the_channel_cannot_write_is_refused_before_the_put() {
    let (client, server) = serve_one_ai().await;

    let chan = client.create_channel("PUTSTAT:A.STAT");
    chan.wait_connected(budget::FACT_BUDGET)
        .await
        .expect("connect");

    let err = chan
        .put_string("3")
        .await
        .expect_err("a no-mod field must refuse the put");
    // The refusal is the access gate's, not the database's: it carries no
    // server put status, because no put was ever sent.
    assert!(
        !matches!(err, CaError::WriteFailed(_) | CaError::ServerError(_)),
        "STAT is special(SPC_NOMOD): the write must be refused by the access \
         gate above the put, not answered by the database; got {err:?}"
    );

    server.abort();
}

/// The accepted put still reports success — the owner must not turn every
/// put into a failure status.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_value_the_field_accepts_still_succeeds() {
    let (client, server) = serve_one_ai().await;

    let chan = client.create_channel("PUTSTAT:A.PHAS");
    chan.wait_connected(budget::FACT_BUDGET)
        .await
        .expect("connect");

    // 32767 is DBF_SHORT's maximum — the boundary value one below the
    // refused 32768 above.
    chan.put_string("32767")
        .await
        .expect("the maximum DBF_SHORT value is accepted");

    server.abort();
}

#[path = "common/budget.rs"]
mod budget;
