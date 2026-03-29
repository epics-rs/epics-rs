use clap::Parser;
use epics_base_rs::pva::client::PvaClient;

#[derive(Parser)]
#[command(name = "rpvaget", about = "Read EPICS PV values via pvAccess")]
struct Args {
    /// PV names to read
    #[arg(required = true)]
    pv_names: Vec<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = PvaClient::new().expect("failed to create PVA client");

    let mut failed = false;
    for pv_name in &args.pv_names {
        match client.pvaget(pv_name).await {
            Ok(structure) => {
                println!("{pv_name} {structure}");
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
