use clap::Parser;
use epics_base_rs::client::CaClient;

#[derive(Parser)]
#[command(name = "rcaput", about = "Write a value to an EPICS PV")]
struct Args {
    /// PV name to write to
    pv_name: String,
    /// Value to write
    value: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = CaClient::new().await.expect("failed to create CA client");

    match client.caput(&args.pv_name, &args.value).await {
        Ok(()) => {
            println!("{} <- {}", args.pv_name, args.value);
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
