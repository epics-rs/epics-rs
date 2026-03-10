use clap::Parser;
use epics_base_rs::pva::client::PvaClient;

#[derive(Parser)]
#[command(name = "rpvaget", about = "Read an EPICS PV value via pvAccess")]
struct Args {
    /// PV name to read
    pv_name: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = PvaClient::new().expect("failed to create PVA client");

    match client.pvaget(&args.pv_name).await {
        Ok(structure) => {
            println!("{} {}", args.pv_name, structure);
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
