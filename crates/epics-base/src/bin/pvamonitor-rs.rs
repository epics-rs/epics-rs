use clap::Parser;
use epics_base_rs::pva::client::PvaClient;

#[derive(Parser)]
#[command(name = "rpvamonitor", about = "Monitor an EPICS PV via pvAccess")]
struct Args {
    /// PV name to monitor
    pv_name: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = PvaClient::new().expect("failed to create PVA client");

    let pv_name = args.pv_name.clone();
    let result = client
        .pvamonitor(&args.pv_name, move |structure| {
            let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.6f");
            println!("{pv_name} {now} {structure}");
        })
        .await;

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
