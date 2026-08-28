//! Long-running soak test for the CA client.
//!
//! Connects to a configurable list of PVs, drives a steady stream of
//! reads/writes/monitors, periodically prints diagnostics, and runs
//! until interrupted. Used to verify behaviour parity with libca over
//! hours-long timescales.
//!
//! Usage:
//!   ca-soak --pv FOO --pv BAR --writes-per-sec 5 --duration 3600
//!
//! Environment:
//!   EPICS_CA_ADDR_LIST, EPICS_CA_AUTO_ADDR_LIST, EPICS_CA_SERVER_PORT
//!   are honoured exactly as for any libca client.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clap::Parser;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::client::CaClient;

#[derive(Parser, Debug)]
#[command(about = "CA client soak test")]
struct Args {
    /// PV names to subscribe to. Repeat for multiple.
    #[arg(long = "pv", required = true)]
    pvs: Vec<String>,

    /// Test duration in seconds (0 = run forever).
    #[arg(long, default_value_t = 3600)]
    duration: u64,

    /// Writes per second (0 = read-only soak).
    #[arg(long, default_value_t = 0u32)]
    writes_per_sec: u32,

    /// Diagnostic print interval in seconds.
    #[arg(long, default_value_t = 30)]
    report_interval: u64,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args = Args::parse();
    let client = CaClient::new().await.expect("CA client");

    let monitors_received = Arc::new(AtomicU64::new(0));
    let writes_done = Arc::new(AtomicU64::new(0));
    let writes_failed = Arc::new(AtomicU64::new(0));
    let reads_done = Arc::new(AtomicU64::new(0));
    let reads_failed = Arc::new(AtomicU64::new(0));

    let mut tasks = Vec::new();
    let start = Instant::now();
    let stop_at = if args.duration == 0 {
        None
    } else {
        Some(start + Duration::from_secs(args.duration))
    };

    // One subscriber + one reader/writer per PV.
    for pv_name in &args.pvs {
        let ch = client.create_channel(pv_name);
        if let Err(e) = ch.wait_connected(Duration::from_secs(10)).await {
            eprintln!("connect {pv_name}: {e:?}");
            continue;
        }

        let mut monitor = ch.subscribe().await.expect("subscribe");
        let mon_count = monitors_received.clone();
        let pv_for_mon = pv_name.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(item) = monitor.recv().await {
                if item.is_ok() {
                    mon_count.fetch_add(1, Ordering::Relaxed);
                } else {
                    eprintln!("monitor {pv_for_mon}: error");
                }
            }
        }));

        // Periodic read regardless of writes_per_sec — exercises READ_NOTIFY
        // path independently of monitor delivery.
        let reads = reads_done.clone();
        let reads_err = reads_failed.clone();
        let pv_for_read = pv_name.clone();
        let ch_for_read = client.create_channel(pv_name);
        ch_for_read
            .wait_connected(Duration::from_secs(10))
            .await
            .expect("connect reader");
        tasks.push(tokio::spawn(async move {
            loop {
                match ch_for_read.get_with_timeout(Duration::from_secs(2)).await {
                    Ok(_) => {
                        reads.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        reads_err.fetch_add(1, Ordering::Relaxed);
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                if let Some(deadline) = stop_at
                    && Instant::now() >= deadline
                {
                    break;
                }
            }
            let _ = pv_for_read; // keep PV name alive for any future logging
        }));

        // Optional write stream.
        if args.writes_per_sec > 0 {
            let writes = writes_done.clone();
            let writes_err = writes_failed.clone();
            let ch_for_write = client.create_channel(pv_name);
            ch_for_write
                .wait_connected(Duration::from_secs(10))
                .await
                .expect("connect writer");
            let interval = Duration::from_secs_f64(1.0 / args.writes_per_sec as f64);
            tasks.push(tokio::spawn(async move {
                let mut counter: i32 = 0;
                loop {
                    counter = counter.wrapping_add(1);
                    match ch_for_write.put(&EpicsValue::Long(counter)).await {
                        Ok(()) => {
                            writes.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            writes_err.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    tokio::time::sleep(interval).await;
                    if let Some(deadline) = stop_at
                        && Instant::now() >= deadline
                    {
                        break;
                    }
                }
            }));
        }
    }

    // Reporter task — prints rolling stats + diagnostics snapshot.
    // Main loop: periodic reporter. Runs in the foreground task instead
    // of spawning a separate task so we can keep client.diagnostics()
    // accessible without restructuring ownership.
    let report_interval = Duration::from_secs(args.report_interval);
    let mut next = epics_base_rs::runtime::time::deadline_from_now(report_interval);
    let interrupted = tokio::signal::ctrl_c();
    tokio::pin!(interrupted);
    loop {
        let sleep = tokio::time::sleep_until(next.into());
        tokio::pin!(sleep);
        tokio::select! {
            _ = &mut sleep => {
                next += report_interval;
                let elapsed = start.elapsed().as_secs_f64();
                let snap = client.diagnostics();
                eprintln!(
                    "[soak +{:>6.1}s] mons={} reads={} (err {}) writes={} (err {})",
                    elapsed,
                    monitors_received.load(Ordering::Relaxed),
                    reads_done.load(Ordering::Relaxed),
                    reads_failed.load(Ordering::Relaxed),
                    writes_done.load(Ordering::Relaxed),
                    writes_failed.load(Ordering::Relaxed),
                );
                eprintln!(
                    "  diag: conns={} disconns={} reconns={} \
                     unresp={} drop_mon={} beacon_anom={}",
                    snap.connections,
                    snap.disconnections,
                    snap.reconnections,
                    snap.unresponsive_events,
                    snap.dropped_monitors,
                    snap.beacon_anomalies,
                );
                if let Some(deadline) = stop_at
                    && Instant::now() >= deadline
                {
                    break;
                }
            }
            _ = &mut interrupted => {
                eprintln!("\nSIGINT received, stopping...");
                break;
            }
        }
    }

    for t in &tasks {
        t.abort();
    }

    let final_diag = client.diagnostics();
    println!("\n=== Soak summary ===");
    println!("Duration:       {:.1}s", start.elapsed().as_secs_f64());
    println!(
        "Monitor events: {}",
        monitors_received.load(Ordering::Relaxed)
    );
    println!(
        "Reads:          {} ({} err)",
        reads_done.load(Ordering::Relaxed),
        reads_failed.load(Ordering::Relaxed)
    );
    println!(
        "Writes:         {} ({} err)",
        writes_done.load(Ordering::Relaxed),
        writes_failed.load(Ordering::Relaxed)
    );
    println!("\n{}", final_diag);
}
