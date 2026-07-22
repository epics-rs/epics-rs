//! Bulk-PUT throughput benchmark (epics-rs PVA client + in-process
//! native server), mirroring the EPICS tech-talk `put_pvxs.cpp` shape:
//! create client → connect/resolve → issue N puts → wait completions.
//!
//! Purpose: empirically check whether epics-rs has the bulk-PUT
//! degradation reported for PVXS C++ (tech-talk 2026-04). It is a
//! benchmark, not a parity test, so it is `#[ignore]`d by default —
//! run it explicitly:
//!
//! ```text
//! cargo test --release -p epics-pva-rs --test put_bench \
//!     -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Two numbers are reported per run:
//! - COLD: fresh client, N concurrent puts. Folds in UDP-free resolve,
//!   single-flight TCP connect, N× CREATE_CHANNEL, and N× (PUT INIT then
//!   PUT). Directly comparable to the put_pvxs total.
//! - WARM: same client, channels already Active. Isolates the
//!   steady-state put pipeline (PUT INIT + PUT per call).
//!
//! The client uses `server_addr` (direct connect), so the UDP/name-server
//! SEARCH round-trip is excluded; epics-rs fires the initial SEARCH
//! immediately (search_engine.rs `SearchReason::Initial`), so on
//! localhost that phase is RTT-bound and small. The CREATE_CHANNEL cost
//! IS measured (it is the per-channel connect work).

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServer, PvaServerConfig};

// ── A minimal put-capable in-memory source ───────────────────────────

#[derive(Clone)]
struct BenchSource {
    values: Arc<Mutex<HashMap<String, PvField>>>,
}

impl BenchSource {
    fn with_scalars(names: &[String], v: f64) -> Self {
        let mut m = HashMap::with_capacity(names.len());
        for n in names {
            m.insert(n.clone(), make_nt_scalar(v));
        }
        Self {
            values: Arc::new(Mutex::new(m)),
        }
    }
}

fn make_nt_scalar(v: f64) -> PvField {
    let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
    s.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Double(v))));
    PvField::Structure(s)
}

fn nt_scalar_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
    }
}

impl ChannelSource for BenchSource {
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        let values = self.values.clone();
        async move { values.lock().await.keys().cloned().collect() }
    }
    fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let values = self.values.clone();
        let name = name.to_string();
        async move { values.lock().await.contains_key(&name) }
    }
    fn get_introspection(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        let values = self.values.clone();
        let name = name.to_string();
        async move {
            if values.lock().await.contains_key(&name) {
                Some(nt_scalar_desc())
            } else {
                None
            }
        }
    }
    fn get_value(&self, name: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let values = self.values.clone();
        let name = name.to_string();
        async move { values.lock().await.get(&name).cloned() }
    }
    fn put_value(
        &self,
        name: &str,
        value: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let values = self.values.clone();
        let name = name.to_string();
        async move {
            values.lock().await.insert(name, value);
            Ok(())
        }
    }
    async fn is_writable(&self, _name: &str) -> bool {
        true
    }
    async fn subscribe(
        &self,
        _name: &str,
    ) -> Option<epics_pva_rs::server_native::MonitorStream<PvField>> {
        // No monitors in this benchmark.
        None
    }
}

// ── Benchmark ─────────────────────────────────────────────────────────

const N: usize = 1000;

fn pv_names() -> Vec<String> {
    (0..N).map(|i| format!("BENCH:PV{i}")).collect()
}

/// Spawn N concurrent puts, return (issue_time, wait_time, ok_count).
async fn blast_puts(
    client: &PvaClient,
    names: &[String],
    base: f64,
) -> (Duration, Duration, usize) {
    let t_issue = Instant::now();
    let mut handles = Vec::with_capacity(names.len());
    for (i, name) in names.iter().enumerate() {
        let client = client.clone();
        let name = name.clone();
        let val = format!("{}", base + i as f64);
        handles.push(tokio::spawn(async move { client.pvput(&name, &val).await }));
    }
    let issue = t_issue.elapsed();

    let t_wait = Instant::now();
    let mut ok = 0usize;
    for h in handles {
        if let Ok(Ok(())) = h.await {
            ok += 1;
        }
    }
    let wait = t_wait.elapsed();
    (issue, wait, ok)
}

/// Run a COLD then a WARM blast against `client`/`names`, print the
/// phase breakdown under `label`, and return
/// `(cold_ok, warm_ok, cold_tput, warm_tput)`.
async fn cold_warm_report(
    client: &PvaClient,
    names: &[String],
    label: &str,
) -> (usize, usize, f64, f64) {
    let n = names.len();

    // COLD: fresh channels — resolve + connect + CREATE_CHANNEL + PUT.
    let (cold_issue, cold_wait, cold_ok) = blast_puts(client, names, 1.0).await;
    let cold_total = cold_issue + cold_wait;

    // WARM: channels already Active — PUT INIT + PUT only.
    let (warm_issue, warm_wait, warm_ok) = blast_puts(client, names, 1_000_000.0).await;
    let warm_total = warm_issue + warm_wait;

    let cold_tput = n as f64 / cold_total.as_secs_f64();
    let warm_tput = n as f64 / warm_total.as_secs_f64();

    eprintln!("\n=== epics-rs PVA bulk-PUT benchmark: {n} PVs ({label}) ===");
    eprintln!("  --- COLD (resolve + connect + CREATE_CHANNEL + PUT INIT + PUT) ---");
    eprintln!(
        "    issue phase   : {:>10.3} ms",
        cold_issue.as_secs_f64() * 1e3
    );
    eprintln!(
        "    wait  phase   : {:>10.3} ms",
        cold_wait.as_secs_f64() * 1e3
    );
    eprintln!(
        "    total         : {:>10.3} ms   ({cold_ok}/{n} ok)",
        cold_total.as_secs_f64() * 1e3
    );
    eprintln!("    throughput    : {cold_tput:>10.1} puts/sec");
    eprintln!("  --- WARM (PUT INIT + PUT, channels already Active) ---");
    eprintln!(
        "    issue phase   : {:>10.3} ms",
        warm_issue.as_secs_f64() * 1e3
    );
    eprintln!(
        "    wait  phase   : {:>10.3} ms",
        warm_wait.as_secs_f64() * 1e3
    );
    eprintln!(
        "    total         : {:>10.3} ms   ({warm_ok}/{n} ok)",
        warm_total.as_secs_f64() * 1e3
    );
    eprintln!("    throughput    : {warm_tput:>10.1} puts/sec\n");

    (cold_ok, warm_ok, cold_tput, warm_tput)
}

/// In-process server + in-memory source: measures the client + protocol
/// pipeline ceiling (no record processing, loopback, direct connect).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark; run with --ignored --nocapture --test-threads=1"]
async fn bulk_put_throughput_1000_pvs() {
    let names = pv_names();
    let source = Arc::new(BenchSource::with_scalars(&names, 0.0));

    // Loopback, ephemeral ports, no beacons — like PvaServerConfig::isolated()
    // but with headroom for 1000 channels on one connection.
    let cfg = PvaServerConfig {
        max_connections: 64,
        max_channels_per_connection: 4096,
        max_ops_per_channel: 4096,
        write_queue_depth: 8192,
        idle_timeout: Duration::from_secs(120),
        ..PvaServerConfig::isolated()
    };
    let server = PvaServer::start(source, cfg).expect("server start");
    let tcp_port = server.report().tcp_port;
    assert_ne!(tcp_port, 0, "server must bind a real port");
    let addr = std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        tcp_port,
    );

    let t_create = Instant::now();
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(20))
        .server_addr(addr)
        .build();
    eprintln!(
        "\n  client creation : {:>10.3} ms",
        t_create.elapsed().as_secs_f64() * 1e3
    );

    let (cold_ok, warm_ok, cold_tput, _) =
        cold_warm_report(&client, &names, "in-process, direct connect").await;

    assert_eq!(cold_ok, N, "every COLD put must complete");
    assert_eq!(warm_ok, N, "every WARM put must complete");
    assert!(
        cold_tput > 100.0,
        "COLD throughput unexpectedly low: {cold_tput:.1} puts/sec"
    );
}

/// Real external IOC (e.g. `softIocPVA`): set `BENCH_PVA_ADDR=host:port`
/// (direct connect, bypasses search). Optional `BENCH_N` (default 1000)
/// and `BENCH_PREFIX` (default `BENCH:PV`) must match the IOC's DB. When
/// `BENCH_PVA_ADDR` is unset the test skips, so it never runs by
/// accident. Measures the client + protocol against real record
/// processing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "external-IOC benchmark; set BENCH_PVA_ADDR=host:port"]
async fn bulk_put_throughput_external_ioc() {
    let Ok(addr_str) = std::env::var("BENCH_PVA_ADDR") else {
        eprintln!("BENCH_PVA_ADDR unset — skipping external-IOC benchmark");
        return;
    };
    let addr: std::net::SocketAddr = addr_str
        .parse()
        .expect("BENCH_PVA_ADDR must be host:port (e.g. 127.0.0.1:5085)");
    let n: usize = std::env::var("BENCH_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(N);
    let prefix = std::env::var("BENCH_PREFIX").unwrap_or_else(|_| "BENCH:PV".into());
    let names: Vec<String> = (0..n).map(|i| format!("{prefix}{i}")).collect();

    eprintln!("\n  target IOC      : {addr}  ({n} PVs, prefix '{prefix}')");
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(20))
        .server_addr(addr)
        .build();

    let (cold_ok, warm_ok, _, _) =
        cold_warm_report(&client, &names, "external softIocPVA, direct connect").await;

    assert_eq!(cold_ok, n, "every COLD put must complete against the IOC");
    assert_eq!(warm_ok, n, "every WARM put must complete against the IOC");
}
