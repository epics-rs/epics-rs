use clap::Parser;
use epics_base_rs::client::{CaClient, ConnectionEvent};

#[derive(Parser)]
#[command(name = "rcamonitor", about = "Monitor an EPICS PV for changes")]
struct Args {
    /// PV name to monitor
    pv_name: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = CaClient::new().await.expect("failed to create CA client");
    let channel = client.create_channel(&args.pv_name);

    // Connection state monitoring (separate task)
    let mut conn_rx = channel.connection_events();
    let pv = args.pv_name.clone();
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
                let pv = args.pv_name.clone();
                while let Some(result) = monitor.recv().await {
                    match result {
                        Ok(value) => {
                            let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.6f");
                            println!("{pv} {now} {value}");
                        }
                        Err(e) => {
                            eprintln!("{pv}: {e}");
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
                            eprintln!("error: {e}");
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
    }
}
