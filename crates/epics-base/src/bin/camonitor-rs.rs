use clap::Parser;
use epics_base_rs::client::{CaClient, ConnectionEvent};

#[derive(Parser)]
#[command(name = "rcamonitor", about = "Monitor EPICS PVs for changes")]
struct Args {
    /// PV names to monitor
    #[arg(required = true)]
    pv_names: Vec<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = CaClient::new().await.expect("failed to create CA client");

    let mut handles = Vec::new();

    for pv_name in args.pv_names {
        let channel = client.create_channel(&pv_name);
        let handle = epics_base_rs::runtime::task::spawn(async move {
            monitor_pv(channel, &pv_name).await;
        });
        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.await;
    }
}

async fn monitor_pv(channel: epics_base_rs::client::CaChannel, pv_name: &str) {
    // Connection state monitoring (separate task)
    let mut conn_rx = channel.connection_events();
    let pv = pv_name.to_string();
    epics_base_rs::runtime::task::spawn(async move {
        while let Ok(evt) = conn_rx.recv().await {
            if let ConnectionEvent::Disconnected = evt {
                let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.6f");
                eprintln!("{pv} {now} *** NOT CONNECTED ***");
            }
        }
    });

    // Subscribe — auto-restores on reconnection
    loop {
        match channel.subscribe().await {
            Ok(mut monitor) => {
                while let Some(result) = monitor.recv().await {
                    match result {
                        Ok(value) => {
                            let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.6f");
                            println!("{pv_name} {now} {value}");
                        }
                        Err(e) => {
                            eprintln!("{pv_name}: {e}");
                        }
                    }
                }
                // Monitor ended (disconnect) — wait for reconnection and re-subscribe
                let mut conn_rx = channel.connection_events();
                loop {
                    match conn_rx.recv().await {
                        Ok(ConnectionEvent::Connected) => break,
                        Ok(_) => continue,
                        Err(_) => return,
                    }
                }
            }
            Err(e) => {
                // Not connected yet, wait for connection
                let mut conn_rx = channel.connection_events();
                loop {
                    match conn_rx.recv().await {
                        Ok(ConnectionEvent::Connected) => break,
                        Ok(_) => continue,
                        Err(_) => {
                            eprintln!("{pv_name}: {e}");
                            return;
                        }
                    }
                }
            }
        }
    }
}
