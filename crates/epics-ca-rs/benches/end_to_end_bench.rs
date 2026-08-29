//! End-to-end CA benchmarks.
//!
//! Spins up an in-process softioc with a handful of PVs, connects a
//! client, and times the operations that show up in real
//! workloads:
//!
//! - **search + connect**: cost of resolving a fresh PV name
//! - **caget**: cost of a one-shot read on an established channel
//! - **caput**: cost of a fire-and-forget write
//!
//! Results land in `target/criterion/`; pull the HTML report out of
//! the `report/` subdir for graphs. Numbers are the *per-operation*
//! cost averaged across many iterations, so stalls show up as
//! standard-deviation widening rather than visible spikes.
//!
//! Tracking baselines: see `BENCHMARKS.md` for the numbers that
//! were current when this file landed. Use them to spot regressions
//! when refactoring hot paths.
// On `exec_backend` this program's `main` refuses instead of running, so
// everything below it is unreachable in that configuration by construction.
// The lint is reporting the intent, not dead code: the default build still
// lints this file in full.
#![cfg_attr(exec_backend, allow(dead_code, unused_imports))]

use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use tokio::runtime::Runtime;

use epics_base_rs::types::EpicsValue;
use epics_ca_rs::client::CaClient;
#[cfg(tokio_backend)]
use epics_ca_rs::server::CaServer;

fn make_runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

#[cfg(tokio_backend)]
/// Spin up a softioc populated with N PVs of the given type on a
/// kernel-assigned port. Returns `(server_handle, udp_port)`. The
/// server is bound and accepting by the time this returns; it runs
/// forever, and the bench drops the handle when done so the runtime
/// cleans up.
async fn boot_softioc(n_pvs: usize) -> (tokio::task::JoinHandle<()>, u16) {
    let mut builder = CaServer::builder().port(0);
    for i in 0..n_pvs {
        builder = builder.pv(&format!("BENCH:PV:{i}"), EpicsValue::Double(i as f64));
    }
    let server = builder.build().await.expect("server build");
    let port = server.udp_port();
    let handle = tokio::spawn(async move {
        if let Err(e) = server.run().await {
            eprintln!("CA benchmark server exited: {e}");
        }
    });
    (handle, port)
}

fn point_addr_list_at(port: u16) {
    // SAFETY: bench runs serially; we set env vars before any client
    // is constructed.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

#[cfg(tokio_backend)]
fn bench_caget(c: &mut Criterion) {
    let rt = make_runtime();
    let (_server, port) = rt.block_on(boot_softioc(8));
    point_addr_list_at(port);
    let client = Arc::new(rt.block_on(async { CaClient::new().await.expect("client") }));

    // Warm up — establish channels before timing.
    rt.block_on(async {
        for i in 0..8 {
            let _ = client.caget(&format!("BENCH:PV:{i}")).await;
        }
    });

    c.bench_function("e2e_caget_warm_8pvs", |b| {
        let client = client.clone();
        b.to_async(&rt).iter(|| {
            let client = client.clone();
            async move {
                for i in 0..8 {
                    let _ = client.caget(&format!("BENCH:PV:{i}")).await;
                }
            }
        });
    });
}

#[cfg(tokio_backend)]
/// Parallel reads — exercises the Option C direct-dispatch path.
/// Pre-Phase-A this benchmark measured ~1.8 ms wall against a
/// localhost IOC for 20 channels because every read serialised
/// through the coordinator's `tokio::select!` loop. Phase A
/// (direct in-flight registry) + Phase B (snapshot sidecar) drive
/// this near the network round-trip floor.
fn bench_bulk_caget(c: &mut Criterion) {
    let rt = make_runtime();
    let (_server, port) = rt.block_on(boot_softioc(20));
    point_addr_list_at(port);
    let client = Arc::new(rt.block_on(async { CaClient::new().await.expect("client") }));

    // Warm up — establish channels before timing so we measure the
    // hot read path, not connect.
    rt.block_on(async {
        let mut handles = Vec::with_capacity(20);
        for i in 0..20 {
            let c = client.clone();
            handles.push(tokio::spawn(async move {
                let _ = c.caget(&format!("BENCH:PV:{i}")).await;
            }));
        }
        for h in handles {
            let _ = h.await;
        }
    });

    c.bench_function("e2e_bulk_caget_parallel_20pvs", |b| {
        let client = client.clone();
        b.to_async(&rt).iter(|| {
            let client = client.clone();
            async move {
                let mut handles = Vec::with_capacity(20);
                for i in 0..20 {
                    let c = client.clone();
                    handles.push(tokio::spawn(async move {
                        let _ = c.caget(&format!("BENCH:PV:{i}")).await;
                    }));
                }
                for h in handles {
                    let _ = h.await;
                }
            }
        });
    });
}

#[cfg(tokio_backend)]
/// Bulk reads over persistent channels using the batched `get_many`
/// API. This is the closest Rust-side analogue to libca's "queue N
/// reads, flush once, then collect completions" path.
fn bench_bulk_get_many(c: &mut Criterion) {
    let rt = make_runtime();
    let (_server, port) = rt.block_on(boot_softioc(100));
    point_addr_list_at(port);
    let client = Arc::new(rt.block_on(async { CaClient::new().await.expect("client") }));

    let channels = rt.block_on(async {
        let mut channels = Vec::with_capacity(100);
        for i in 0..100 {
            channels.push(client.create_channel(&format!("BENCH:PV:{i}")));
        }
        let connected = futures_util::future::join_all(
            channels
                .iter()
                .map(|ch| ch.wait_connected(Duration::from_secs(3))),
        )
        .await;
        for result in connected {
            result.expect("channel connected");
        }
        channels
    });

    c.bench_function("e2e_bulk_get_many_100pvs", |b| {
        let client = client.clone();
        let channels = channels.clone();
        b.to_async(&rt).iter(|| {
            let client = client.clone();
            let channels = channels.clone();
            async move {
                let _ = client.get_many(&channels).await;
            }
        });
    });
}

#[cfg(tokio_backend)]
/// Bulk-read scaling bench: same in-process softIoc, vary N over
/// {10, 20, 50, 100} to expose super-linear scaling on the server's
/// response path. Measures the same `get_many` route as
/// `bench_bulk_get_many` so the only difference is N.
fn bench_bulk_get_many_scaling(c: &mut Criterion) {
    let rt = make_runtime();
    let (_server, port) = rt.block_on(boot_softioc(100));
    point_addr_list_at(port);
    let client = Arc::new(rt.block_on(async { CaClient::new().await.expect("client") }));

    let channels = rt.block_on(async {
        let mut channels = Vec::with_capacity(100);
        for i in 0..100 {
            channels.push(client.create_channel(&format!("BENCH:PV:{i}")));
        }
        let connected = futures_util::future::join_all(
            channels
                .iter()
                .map(|ch| ch.wait_connected(Duration::from_secs(3))),
        )
        .await;
        for result in connected {
            result.expect("channel connected");
        }
        channels
    });

    let mut group = c.benchmark_group("e2e_bulk_get_many_scaling");
    for &n in &[10usize, 20, 50, 100] {
        let subset: Vec<_> = channels.iter().take(n).cloned().collect();
        group.bench_function(format!("{n:03}pvs"), |b| {
            let client = client.clone();
            let subset = subset.clone();
            b.to_async(&rt).iter(|| {
                let client = client.clone();
                let subset = subset.clone();
                async move {
                    let _ = client.get_many(&subset).await;
                }
            });
        });
    }
    group.finish();
}

#[cfg(tokio_backend)]
/// Bulk reads by PV name. First warm call populates CaClient's
/// one-shot channel cache; timed iterations should then take the same
/// hot batched-read path as `get_many`.
fn bench_bulk_caget_many(c: &mut Criterion) {
    let rt = make_runtime();
    let (_server, port) = rt.block_on(boot_softioc(100));
    point_addr_list_at(port);
    let client = Arc::new(rt.block_on(async { CaClient::new().await.expect("client") }));
    let names: Vec<String> = (0..100).map(|i| format!("BENCH:PV:{i}")).collect();

    rt.block_on(async {
        let results = client
            .caget_many_with_timeout(&names, Duration::from_secs(3))
            .await;
        for result in results {
            result.expect("warm caget_many connected");
        }
    });

    c.bench_function("e2e_bulk_caget_many_cached_100pvs", |b| {
        let client = client.clone();
        let names = names.clone();
        b.to_async(&rt).iter(|| {
            let client = client.clone();
            let names = names.clone();
            async move {
                let _ = client.caget_many(&names).await;
            }
        });
    });
}

#[cfg(tokio_backend)]
fn bench_caput(c: &mut Criterion) {
    let rt = make_runtime();
    let (_server, port) = rt.block_on(boot_softioc(1));
    point_addr_list_at(port);
    let client = Arc::new(rt.block_on(async { CaClient::new().await.expect("client") }));
    rt.block_on(async {
        let _ = client.caput("BENCH:PV:0", "1.0").await;
    });

    c.bench_function("e2e_caput_warm", |b| {
        let client = client.clone();
        b.to_async(&rt).iter(|| {
            let client = client.clone();
            async move {
                let _ = client.caput("BENCH:PV:0", "1.0").await;
            }
        });
    });
}

#[cfg(tokio_backend)]
criterion_group! {
    name = e2e;
    // Lower sample size — each iteration is an actual TCP round-trip,
    // not a microsecond op.
    config = Criterion::default()
        .sample_size(20)
        .measurement_time(Duration::from_secs(8))
        .warm_up_time(Duration::from_secs(2));
    targets = bench_caget, bench_bulk_caget, bench_bulk_get_many, bench_bulk_get_many_scaling, bench_bulk_caget_many, bench_caput
}
#[cfg(tokio_backend)]
criterion_main!(e2e);

/// The `exec_backend` arm. Every benchmark here drives a live in-process
/// softIoc over the async CA front-end, which is `tokio_backend`-only, so on
/// the reactor-free backend there is nothing to measure.
#[cfg(exec_backend)]
fn main() {
    eprintln!(
        "end_to_end_bench: needs the tokio backend; this build selects \
         EPICS_RS_BUILD_EXEC_BACKEND=thread."
    );
}
