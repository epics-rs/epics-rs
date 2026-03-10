use clap::Parser;
use epics_base_rs::pva::client::PvaClient;

#[derive(Parser)]
#[command(name = "rpvainfo", about = "Show EPICS PV type info via pvAccess")]
struct Args {
    /// PV name to query
    pv_name: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = PvaClient::new().expect("failed to create PVA client");

    match client.pvainfo(&args.pv_name).await {
        Ok(desc) => {
            println!("{}:", args.pv_name);
            print!("{desc}");
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
