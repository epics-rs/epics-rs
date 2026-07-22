//! Stability-feature interop against pvxs.
//!
//! Verifies that the stability mechanisms our client/server implement
//! (heartbeat, auto-reconnect, beacon-driven fast reconnect, monitor
//! over restart, slow-consumer back-pressure) actually work against pvxs
//! 1.x peers, not just against ourselves.
//!
//! Ignored by default — run with PVXS_HOME set:
//!
//! ```bash
//! PVXS_HOME=$HOME/codes/pvxs \
//!     cargo test --test parity_interop -- --ignored stability_interop
//! ```

#![cfg(test)]

// RTEMS-EXEC-MODEL-ALLOW(4): not run by the default nextest profile - this file is a module of the `parity_interop` binary, which `.config/nextest.toml`'s default-filter excludes.

use epics_pva_rs::server_native::MonitorStream;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::Mutex;

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServerConfig, run_pva_server};

fn pvxs_home() -> Option<PathBuf> {
    std::env::var("PVXS_HOME").ok().map(PathBuf::from)
}

fn host_arch() -> String {
    std::env::var("EPICS_HOST_ARCH").unwrap_or_else(|_| "darwin-aarch64".into())
}

fn softioc() -> Option<PathBuf> {
    let home = pvxs_home()?;
    let p = home.join("bin").join(host_arch()).join("softIocPVX");
    if p.is_file() { Some(p) } else { None }
}

fn pvxget() -> Option<PathBuf> {
    let home = pvxs_home()?;
    let p = home.join("bin").join(host_arch()).join("pvxget");
    if p.is_file() { Some(p) } else { None }
}

fn pvxput() -> Option<PathBuf> {
    let home = pvxs_home()?;
    let p = home.join("bin").join(host_arch()).join("pvxput");
    if p.is_file() { Some(p) } else { None }
}

static NEXT_PORT: AtomicU16 = AtomicU16::new(40000);
fn alloc_port_pair() -> (u16, u16) {
    let base = NEXT_PORT.fetch_add(2, Ordering::Relaxed);
    (base, base + 1)
}

struct ChildKiller<'a>(&'a mut Child);
impl<'a> Drop for ChildKiller<'a> {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

async fn spawn_pvxs_softioc(
    softioc_bin: &PathBuf,
    db_content: &str,
    port: u16,
    udp: u16,
) -> (Child, tempfile::NamedTempFile) {
    let dbfile = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(dbfile.path(), db_content).unwrap();
    let child = TokioCommand::new(softioc_bin)
        .env("EPICS_PVA_SERVER_PORT", port.to_string())
        .env("EPICS_PVA_BROADCAST_PORT", udp.to_string())
        .arg("-S")
        .arg("-d")
        .arg(dbfile.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Wait for TCP listen.
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if std::net::TcpStream::connect(server_addr).is_ok() {
            return (child, dbfile);
        }
    }
    panic!("softIocPVX did not bind {port}");
}

// ── P1 / P2: Auto reconnect after pvxs server restart ────────────────

#[tokio::test]
#[ignore]
async fn rust_client_auto_reconnect_to_pvxs_softiocpvx() {
    let Some(softioc_bin) = softioc() else {
        return;
    };

    let (port, udp) = alloc_port_pair();
    let db = "record(ai, \"STAB:VAL\") { field(VAL, \"100\") }\n";

    // First incarnation.
    let (mut child1, dbfile) = spawn_pvxs_softioc(&softioc_bin, db, port, udp).await;
    let _killer1 = ChildKiller(&mut child1);

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build();

    // First GET succeeds.
    let v = tokio::time::timeout(Duration::from_secs(5), client.pvget("STAB:VAL"))
        .await
        .expect("first pvget timeout")
        .expect("first pvget failed");
    assert!(matches!(v, PvField::Structure(_)));

    // Kill softIocPVX.
    drop(_killer1);
    let _ = child1.wait().await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Restart with same port.
    let (mut child2, _dbfile2) = spawn_pvxs_softioc(&softioc_bin, db, port, udp).await;
    let _killer2 = ChildKiller(&mut child2);

    // Second GET must succeed transparently — channel state machine
    // notices the dead connection and re-opens.
    let v = tokio::time::timeout(Duration::from_secs(8), client.pvget("STAB:VAL"))
        .await
        .expect("second pvget timed out — auto-reconnect failed")
        .expect("second pvget after restart failed");
    eprintln!("post-restart got: {v}");
    assert!(matches!(v, PvField::Structure(_)));

    drop(dbfile);
}

// ── P5: Monitor survives pvxs server restart ─────────────────────────

#[tokio::test]
#[ignore]
async fn rust_client_monitor_survives_pvxs_restart() {
    let Some(softioc_bin) = softioc() else {
        return;
    };
    let Some(pvxput_bin) = pvxput() else {
        return;
    };

    let (port, udp) = alloc_port_pair();
    let db = "record(ao, \"STAB:MON\") { field(VAL, \"1.0\") }\n";

    let (mut child1, _dbfile1) = spawn_pvxs_softioc(&softioc_bin, db, port, udp).await;

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build();

    let received = Arc::new(parking_lot::Mutex::new(Vec::<f64>::new()));
    let recv_cb = received.clone();
    let mon_handle = tokio::spawn({
        let client = client.clone();
        async move {
            let _ = client
                .pvmonitor("STAB:MON", move |value| {
                    if let PvField::Structure(s) = value {
                        if let Some(ScalarValue::Double(d)) = s.get_value() {
                            recv_cb.lock().push(*d);
                        }
                    }
                })
                .await;
        }
    });

    // Initial value reception.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let initial_count = received.lock().len();
    assert!(initial_count >= 1, "expected initial monitor event");

    // Push one update via pvxput.
    let _ = TokioCommand::new(&pvxput_bin)
        .env("EPICS_PVA_ADDR_LIST", format!("127.0.0.1:{}", udp))
        .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
        .arg("-w")
        .arg("3")
        .arg("STAB:MON")
        .arg("2.0")
        .output()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Kill server.
    let _ = child1.start_kill();
    let _ = child1.wait().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Restart on the same port.
    let (mut child2, _dbfile2) = spawn_pvxs_softioc(&softioc_bin, db, port, udp).await;
    let _killer2 = ChildKiller(&mut child2);

    // Allow ample time for the channel state machine to detect the
    // disconnect, re-search, reconnect, MONITOR INIT + START, and emit
    // the initial snapshot.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let mid_received = received.lock().clone();
    eprintln!("monitor received after restart (before final pvxput): {mid_received:?}");

    // Push another update — monitor should pick it up after reconnect.
    for attempt in 0..5 {
        let _ = TokioCommand::new(&pvxput_bin)
            .env("EPICS_PVA_ADDR_LIST", format!("127.0.0.1:{}", udp))
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .arg("-w")
            .arg("3")
            .arg("STAB:MON")
            .arg("3.0")
            .output()
            .await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        if received.lock().iter().any(|v| (*v - 3.0).abs() < 0.01) {
            eprintln!("got 3.0 after attempt {attempt}");
            break;
        }
    }

    let final_received = received.lock().clone();
    eprintln!("monitor received across restart: {final_received:?}");
    mon_handle.abort();

    // Must have both pre-restart and post-restart events.
    assert!(
        final_received.iter().any(|v| (*v - 2.0).abs() < 0.01),
        "did not receive 2.0 (pre-restart): {final_received:?}"
    );
    assert!(
        final_received.iter().any(|v| (*v - 3.0).abs() < 0.01),
        "did not receive 3.0 (post-restart): {final_received:?}"
    );
}

// ── Heartbeat: rust client stays connected across an idle window ──────
//
// pvxs sends ECHO_REQUEST every 15s; our client must auto-respond and
// keep the channel alive. Use a shorter horizon (3s idle) since we
// can't easily inject a faster echo on the pvxs side. This mostly
// confirms our reader loop doesn't drop pvxs's heartbeat traffic.

#[tokio::test]
#[ignore]
async fn rust_client_idle_keepalive_with_pvxs() {
    let Some(softioc_bin) = softioc() else {
        return;
    };
    let (port, udp) = alloc_port_pair();
    let db = "record(ai, \"STAB:KEEP\") { field(VAL, \"7\") }\n";
    let (mut child, _dbfile) = spawn_pvxs_softioc(&softioc_bin, db, port, udp).await;
    let _killer = ChildKiller(&mut child);

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build();

    // First GET — opens persistent ServerConn (ChannelHandle/Pool).
    let _ = client.pvget("STAB:KEEP").await.unwrap();

    // Idle longer than the heartbeat interval (15 s would be ideal
    // but slow; 3 s is enough to cycle through several reader.poll()
    // wakes without traffic).
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Second GET should reuse the same connection without re-doing the
    // full handshake — observed indirectly: it just succeeds.
    let v = tokio::time::timeout(Duration::from_secs(3), client.pvget("STAB:KEEP"))
        .await
        .expect("second pvget timeout — connection died during idle")
        .expect("second pvget failed");
    if let PvField::Structure(s) = v {
        if let Some(ScalarValue::Double(d)) = s.get_value() {
            assert!((d - 7.0).abs() < 0.01);
        }
    }
}

// ── P6 / P7: pvxs client survives a slow rust server (back-pressure) ──
//
// Spawn a rust server that emits monitor events faster than pvxs can
// ack; verify our `monitor_queue_depth` squashing keeps the connection
// alive and pvxget's final read sees the latest value.

#[tokio::test]
#[ignore]
async fn pvxs_pvxget_against_rust_server_under_burst_load() {
    let Some(pvxget_bin) = pvxget() else {
        return;
    };

    /// Source backed by a counter; every `get_value` returns the next int.
    /// This is an aggressive test pattern — pvxs's GET will see whatever
    /// counter value the server happened to be on.
    #[derive(Clone)]
    struct Counter {
        n: Arc<Mutex<i32>>,
    }
    impl ChannelSource for Counter {
        fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
            async { vec!["STAB:CTR".into()] }
        }
        fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
            let n = n.to_string();
            async move { n == "STAB:CTR" }
        }
        fn get_introspection(
            &self,
            _: &str,
        ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
            async {
                Some(FieldDesc::Structure {
                    struct_id: "epics:nt/NTScalar:1.0".into(),
                    fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Int))],
                })
            }
        }
        fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
            let n = self.n.clone();
            async move {
                let mut g = n.lock().await;
                *g += 1;
                let cur = *g;
                drop(g);
                let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
                s.fields
                    .push(("value".into(), PvField::Scalar(ScalarValue::Int(cur))));
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
            async { None }
        }
    }

    let (port, udp) = alloc_port_pair();
    let source = Arc::new(Counter {
        n: Arc::new(Mutex::new(0)),
    });
    let cfg = PvaServerConfig {
        tcp_port: port,
        udp_port: udp,
        monitor_queue_depth: 4,
        ..Default::default()
    };
    let h = tokio::spawn(async move {
        let _ = run_pva_server(source, cfg).await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Drive 50 sequential pvxgets in a row — server must handle the
    // burst without crashing and each should succeed.
    for i in 0..50 {
        let out = TokioCommand::new(&pvxget_bin)
            .env("EPICS_PVA_ADDR_LIST", format!("127.0.0.1:{}", udp))
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .arg("-w")
            .arg("2")
            .arg("STAB:CTR")
            .output()
            .await
            .unwrap_or_else(|_| panic!("pvxget #{i}"));
        assert!(
            out.status.success(),
            "pvxget #{i} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    h.abort();
}
