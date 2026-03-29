use clap::Parser;
use epics_base_rs::pva::client::PvaClient;

#[derive(Parser)]
#[command(name = "rpvamonitor", about = "Monitor EPICS PVs via pvAccess")]
struct Args {
    /// PV names to monitor
    #[arg(required = true)]
    pv_names: Vec<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let mut handles = Vec::new();

    for pv_name in args.pv_names {
        let handle = tokio::spawn(async move {
            let client = PvaClient::new().expect("failed to create PVA client");
            let result = client
                .pvamonitor(&pv_name, |structure| {
                    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.6f");
                    println!("{pv_name} {now} {structure}");
                })
                .await;

            if let Err(e) = result {
                eprintln!("{pv_name}: {e}");
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.await;
    }
}
