use clap::Parser;
use epics_base_rs::pva::client::PvaClient;

#[derive(Parser)]
#[command(name = "rpvainfo", about = "Show EPICS PV type info via pvAccess")]
struct Args {
    /// PV names to query
    #[arg(required = true)]
    pv_names: Vec<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = PvaClient::new().expect("failed to create PVA client");

    let mut failed = false;
    for pv_name in &args.pv_names {
        match client.pvainfo(pv_name).await {
            Ok(desc) => {
                println!("{pv_name}:");
                print!("{desc}");
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
