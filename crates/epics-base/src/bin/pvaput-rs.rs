use clap::Parser;
use epics_base_rs::pva::client::PvaClient;

#[derive(Parser)]
#[command(name = "rpvaput", about = "Write a value to an EPICS PV via pvAccess")]
struct Args {
    /// PV name to write to
    pv_name: String,
    /// Value to write
    value: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = PvaClient::new().expect("failed to create PVA client");

    match client.pvaput(&args.pv_name, &args.value).await {
        Ok(()) => {
            println!("{} <- {}", args.pv_name, args.value);
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
