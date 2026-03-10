use clap::Parser;
use epics_base_rs::client::CaClient;

#[derive(Parser)]
#[command(name = "rcaget", about = "Read an EPICS PV value")]
struct Args {
    /// PV name to read
    pv_name: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = CaClient::new().await.expect("failed to create CA client");

    match client.caget(&args.pv_name).await {
        Ok((_dbr_type, value)) => {
            println!("{} {}", args.pv_name, value);
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
