use clap::Parser;
use epics_base_rs::client::CaClient;

#[derive(Parser)]
#[command(name = "rcaget", about = "Read EPICS PV values")]
struct Args {
    /// PV names to read
    #[arg(required = true)]
    pv_names: Vec<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = CaClient::new().await.expect("failed to create CA client");

    let mut failed = false;
    for pv_name in &args.pv_names {
        match client.caget(pv_name).await {
            Ok((_dbr_type, value)) => {
                println!("{pv_name} {value}");
            }
            Err(e) => {
                eprintln!("{pv_name}: {e}");
                failed = true;
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}
