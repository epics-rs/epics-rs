//! Regression: a closed `SharedPV` whose `on_first_connect` opens it must
//! serve GET / MONITOR on the very channel whose CREATE_CHANNEL fired the
//! lazy open — pvxs `sharedpv.cpp:299-313` runs `onFirstConnect` on the
//! channel attach edge, and later operations read the PV's post-open
//! descriptor.
//!
//! The bug: CREATE_CHANNEL snapshotted the (closed → `None`) descriptor into
//! `ChannelState` *before* the attach hook ran, so the first channel kept a
//! `None` prototype and GET / PUT / MONITOR INIT replied "must provide
//! prototype" against a PV the hook had just opened. The fix obtains the
//! descriptor from the bound owner *after* the attach hook.
//!
//! This exercises the full TCP CREATE_CHANNEL path, which the direct
//! `notify_channel_open()` unit test (`shared_source_channel_open_close_\
//! drives_lazy_lifecycle`) cannot reach: that unit test calls the attach
//! hook and reads `get_value()` straight off the source, never freezing a
//! `None` prototype into a per-connection `ChannelState`.

#![cfg(test)]

// RTEMS-EXEC-MODEL-ALLOW(2): not run by the default nextest profile - this file is a module of the `parity_interop` binary, which `.config/nextest.toml`'s default-filter excludes.

use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig, SharedPV, SharedSource};

fn nt_int_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Int))],
    }
}

fn nt_int_value(v: i32) -> PvField {
    let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
    s.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Int(v))));
    PvField::Structure(s)
}

/// A registered-but-closed `SharedPV` whose `on_first_connect` opens it.
/// `has_pv()` is already true (registered), so CREATE_CHANNEL accepts the
/// channel and resolves a `None` descriptor before the attach hook opens
/// the PV.
fn lazy_source() -> Arc<SharedSource> {
    let pv = SharedPV::new();
    pv.on_first_connect(|p| {
        p.open(nt_int_desc(), nt_int_value(42))
            .expect("lazy open must succeed");
    });
    let src = SharedSource::new();
    src.add("dut", pv);
    Arc::new(src)
}

fn client_to(port: u16) -> PvaClient {
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build()
}

/// The first channel created for a lazy PV must serve GET — the channel
/// whose CREATE_CHANNEL fired `on_first_connect` must see the post-open
/// descriptor, not a frozen `None` prototype.
#[tokio::test]
async fn lazy_first_connect_pv_serves_get_on_creating_channel() {
    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        ..Default::default()
    };
    let server = PvaServer::start(lazy_source(), cfg).expect("test server must start");
    let port = server.report().tcp_port;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = client_to(port);
    let v = tokio::time::timeout(Duration::from_secs(3), client.pvget("dut"))
        .await
        .expect("pvget timed out")
        .expect("pvget against a lazily-opened SharedPV must succeed, not reply \"must provide prototype\"");
    match v {
        PvField::Structure(s) => assert_eq!(
            s.get_field("value"),
            Some(&PvField::Scalar(ScalarValue::Int(42))),
            "GET must return the value the on_first_connect hook opened"
        ),
        other => panic!("expected NTScalar structure, got {other:?}"),
    }

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}

/// Same lazy PV must also serve MONITOR on the creating channel: the
/// monitor INIT reads the channel prototype too.
#[tokio::test]
async fn lazy_first_connect_pv_serves_monitor_on_creating_channel() {
    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        ..Default::default()
    };
    let server = PvaServer::start(lazy_source(), cfg).expect("test server must start");
    let port = server.report().tcp_port;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = client_to(port);
    let (tx, rx) = std::sync::mpsc::channel::<PvField>();
    let _handle = tokio::time::timeout(
        Duration::from_secs(3),
        client.pvmonitor_handle(
            "dut",
            move |_: &FieldDesc, v: &PvField| {
                let _ = tx.send(v.clone());
            },
            |_| {},
        ),
    )
    .await
    .expect("pvmonitor_handle timed out")
    .expect("MONITOR against a lazily-opened SharedPV must succeed");

    // The initial monitor update carries the lazily-opened value.
    let got = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(3)))
        .await
        .expect("join")
        .expect("a monitor update must arrive for the lazily-opened PV");
    match got {
        PvField::Structure(s) => assert_eq!(
            s.get_field("value"),
            Some(&PvField::Scalar(ScalarValue::Int(42))),
            "MONITOR seed must carry the on_first_connect-opened value"
        ),
        other => panic!("expected NTScalar structure, got {other:?}"),
    }

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}
