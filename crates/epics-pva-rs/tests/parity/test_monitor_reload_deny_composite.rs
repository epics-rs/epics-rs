//! Monitor-reload deny (composite source) regression test.
//!
//! Invariant: every monitor event after an ACL version mismatch MUST
//! re-check READ access through the **inner** source's gate (the gate
//! that served the subscription), not the composite's own permissive
//! aggregator gate. Pre-fix tcp.rs called `src.access_gate().check(...)`
//! on the composite, which is an `open_with_aggregator` —
//! `acl_version()` correctly surfaced inner bumps, but the `check()`
//! always returned `ReadWrite`. The monitor loop kept forwarding
//! events after a child's `set_acf` flipped to deny.
//!
//! This test wires a `CompositeSource` over a single child whose
//! ACL initially allows the connecting user. Once the client has
//! received the initial snapshot, the child's ACF is swapped to a
//! deny-everyone-but-`alice` ASG and its `acl_version` is bumped.
//! The next event pushed through the child's subscribe channel
//! must NOT be forwarded — the server must emit MONITOR FINISH on
//! detecting the version mismatch via the matched inner gate's
//! `revalidate_read`.

#![cfg(test)]

// RTEMS-EXEC-MODEL-ALLOW(1): not run by the default nextest profile - this file is a module of the `parity_interop` binary, which `.config/nextest.toml`'s default-filter excludes.

use epics_pva_rs::server_native::MonitorStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, mpsc};

use epics_base_rs::server::access_security::{
    AccessChecked, AccessGate, AsgAslResolver, parse_acf,
};
use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{
    ChannelSource, CompositeSource, DynSource, OpError, PvaServer, PvaServerConfig,
};

/// Child source backing the composite. Its `subscribe_checked`
/// returns a fresh rx and stores the matching tx in a Mutex so the
/// test driver can push events after the initial snapshot has been
/// delivered.
struct VersionedChildSource {
    gate: AccessGate,
    acf_cell: epics_base_rs::server::access_security::AcfCell,
    tx_holder: Arc<Mutex<Option<mpsc::Sender<PvField>>>>,
}

impl ChannelSource for VersionedChildSource {
    fn access(&self) -> &AccessGate {
        &self.gate
    }
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        async { vec!["dut".into()] }
    }
    fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
        let n = n.to_string();
        async move { n == "dut" }
    }
    fn get_introspection(
        &self,
        _: &str,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        async {
            Some(FieldDesc::Structure {
                struct_id: "epics:nt/NTScalar:1.0".into(),
                fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
            })
        }
    }
    fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        async {
            let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
            s.fields
                .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
            Some(PvField::Structure(s))
        }
    }
    fn put_value(
        &self,
        _: &str,
        _: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        async { Err("read-only".into()) }
    }
    fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
        async { false }
    }
    fn subscribe(
        &self,
        _: &str,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        let holder = self.tx_holder.clone();
        async move {
            let (tx, rx) = mpsc::channel::<PvField>(8);
            *holder.lock().await = Some(tx);
            Some(rx.into())
        }
    }
    fn subscribe_checked(
        &self,
        _: AccessChecked,
        _: epics_pva_rs::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        // Delegate to the legacy `subscribe` so we exercise the same
        // tx_holder path. The composite's `subscribe_checked` already
        // re-mints `AccessChecked` against this gate, so the token
        // we receive here is correct under the *current* (pre-deny)
        // policy.
        let holder = self.tx_holder.clone();
        async move {
            let (tx, rx) = mpsc::channel::<PvField>(8);
            *holder.lock().await = Some(tx);
            Some(rx.into())
        }
    }
}

impl VersionedChildSource {
    fn new() -> Arc<Self> {
        let acf_cell = epics_base_rs::server::access_security::new_acf_cell(None);
        let resolver: AsgAslResolver =
            Arc::new(|_| Box::pin(async { ("DEFAULT".to_string(), 0u8) }));
        Arc::new(VersionedChildSource {
            gate: AccessGate::required(acf_cell.clone(), resolver),
            acf_cell,
            tx_holder: Arc::new(Mutex::new(None)),
        })
    }
}

#[tokio::test]
async fn composite_monitor_finishes_after_inner_deny_bump() {
    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        ..Default::default()
    };

    let child = VersionedChildSource::new();
    let comp = CompositeSource::new();
    comp.add_source("child", child.clone() as DynSource, 0)
        .unwrap();

    let server_comp = comp.clone();
    let server = PvaServer::start(server_comp, cfg).expect("test server must start");
    let port = server.report().tcp_port;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .user("intruder")
        .host("h.example")
        .build();

    let received = Arc::new(AtomicUsize::new(0));
    let received_clone = received.clone();
    let saw_post_deny = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw_post_deny_clone = saw_post_deny.clone();

    // After the client receives the initial snapshot, swap the
    // child's ACF to "only alice may read" and bump the gate's
    // ACL version. The next pushed event must trip the reload
    // re-check and produce MONITOR FINISH instead of a data frame.
    let driver_child = child.clone();
    let driver_handle = tokio::spawn(async move {
        // Wait long enough for the subscribe to register the tx and
        // for the initial snapshot to be delivered.
        tokio::time::sleep(Duration::from_millis(400)).await;

        let deny_cfg = parse_acf(
            r#"
UAG(ops) { alice }
ASG(DEFAULT) {
    RULE(0, READ) { UAG(ops) }
}
"#,
        )
        .expect("acf parse");
        driver_child.acf_cell.store(Some(Arc::new(deny_cfg)));
        driver_child.gate.bump_acl_version();

        // Push one event AFTER the policy flip. tcp.rs's recv loop
        // detects the version mismatch, calls revalidate_read
        // against the matched inner (child) gate which now denies,
        // and sends MONITOR FINISH instead of forwarding the value.
        let tx_opt = driver_child.tx_holder.lock().await.clone();
        if let Some(tx) = tx_opt {
            let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
            s.fields
                .push(("value".into(), PvField::Scalar(ScalarValue::Double(2.0))));
            let _ = tx.send(PvField::Structure(s)).await;
        }
    });

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        client
            .pvmonitor("dut", move |frame| {
                received_clone.fetch_add(1, Ordering::SeqCst);
                if let PvField::Structure(s) = &frame {
                    for (name, f) in &s.fields {
                        if name == "value" {
                            if let PvField::Scalar(ScalarValue::Double(d)) = f {
                                if (*d - 2.0).abs() < 1e-9 {
                                    saw_post_deny_clone.store(true, Ordering::SeqCst);
                                }
                            }
                        }
                    }
                }
            })
            .await
    })
    .await
    .expect("pvmonitor timed out");

    let _ = driver_handle.await;

    // FINISH carries Status::OK so pvmonitor returns Ok(()).
    result.expect("monitor should end cleanly with Ok(())");

    let n = received.load(Ordering::SeqCst);
    assert!(
        n >= 1,
        "client should have received at least the initial snapshot before FINISH; got {n}"
    );
    // The pre-fix bug: re-check went through the composite's
    // permissive aggregator gate, so Double(2.0) reached the
    // client and the stream never FINISHed. With the
    // `revalidate_read` owner API the inner gate denies and the
    // recv loop emits MONITOR FINISH instead of the value frame.
    assert!(
        !saw_post_deny.load(Ordering::SeqCst),
        "post-deny value (2.0) must NOT be forwarded after child ACL flips to deny"
    );

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}
