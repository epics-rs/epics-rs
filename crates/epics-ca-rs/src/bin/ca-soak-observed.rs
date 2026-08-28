//! Variant of ca-soak that demonstrates the optional observability stack:
//! structured tracing + Prometheus /metrics endpoint.
//!
//! Build with:  cargo build --features observability --bin ca-soak-observed
//!
//! Then in another shell:
//!   curl http://127.0.0.1:9090/metrics

#[cfg(not(feature = "observability"))]
fn main() {
    eprintln!("ca-soak-observed requires --features observability");
    std::process::exit(2);
}

#[cfg(feature = "observability")]
mod inner {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use clap::Parser;
    use epics_base_rs::types::EpicsValue;
    use epics_ca_rs::client::CaClient;
    use epics_ca_rs::observability;

    #[derive(Parser, Debug)]
    #[command(about = "CA soak with tracing + Prometheus")]
    pub struct Args {
        #[arg(long = "pv", required = true)]
        pvs: Vec<String>,

        /// Test duration (sec). 0 = run forever.
        #[arg(long, default_value_t = 0u64)]
        duration: u64,

        /// Writes per second to drive (0 = read-only).
        #[arg(long, default_value_t = 5u32)]
        writes_per_sec: u32,

        /// Bind address for /metrics endpoint.
        #[arg(long, default_value = "127.0.0.1:9090")]
        metrics_addr: String,
    }

    pub async fn run() {
        let args = Args::parse();

        observability::init_tracing();
        if let Err(e) =
            observability::serve_prometheus(args.metrics_addr.parse().expect("metrics addr"))
        {
            tracing::error!("failed to start prometheus exporter: {e}");
            std::process::exit(1);
        }
        tracing::info!(addr = %args.metrics_addr, "prometheus /metrics serving");

        let client = CaClient::new().await.expect("CaClient");
        let writes_done = Arc::new(AtomicU64::new(0));
        let stop_at = if args.duration == 0 {
            None
        } else {
            Some(epics_base_rs::runtime::time::deadline_from_now(
                Duration::from_secs(args.duration),
            ))
        };

        let mut tasks = Vec::new();
        for pv_name in args.pvs.clone() {
            let ch = client.create_channel(&pv_name);
            ch.wait_connected(Duration::from_secs(10))
                .await
                .expect("connect");
            let mut mon = ch.subscribe().await.expect("subscribe");
            tasks.push(tokio::spawn(
                async move { while mon.recv().await.is_some() {} },
            ));

            if args.writes_per_sec > 0 {
                let writer_ch = client.create_channel(&pv_name);
                writer_ch
                    .wait_connected(Duration::from_secs(10))
                    .await
                    .expect("writer");
                let writes = writes_done.clone();
                let interval = Duration::from_secs_f64(1.0 / args.writes_per_sec as f64);
                tasks.push(tokio::spawn(async move {
                    let mut counter: i32 = 0;
                    loop {
                        counter = counter.wrapping_add(1);
                        if writer_ch.put(&EpicsValue::Long(counter)).await.is_ok() {
                            writes.fetch_add(1, Ordering::Relaxed);
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

        if let Some(deadline) = stop_at {
            tokio::time::sleep_until(deadline.into()).await;
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
        for t in &tasks {
            t.abort();
        }
        tracing::info!(
            writes = writes_done.load(Ordering::Relaxed),
            "soak complete; final diagnostics:\n{}",
            client.diagnostics()
        );
    }
}

#[cfg(feature = "observability")]
#[tokio::main(flavor = "multi_thread")]
async fn main() {
    inner::run().await;
}
