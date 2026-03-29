use clap::Parser;
use epics_base_rs::client::CaClient;

#[derive(Parser)]
#[command(name = "rcaput", about = "Write a value to an EPICS PV")]
struct Args {
    /// Wait for completion callback (like caput -c)
    #[arg(short = 'c', long = "callback")]
    callback: bool,

    /// Callback timeout in seconds (default: 30)
    #[arg(short = 'w', long = "timeout", default_value = "30")]
    timeout: f64,

    /// PV name to write to
    pv_name: String,

    /// Value to write
    value: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = CaClient::new().await.expect("failed to create CA client");

    if args.callback {
        match client.caput_callback(&args.pv_name, &args.value, args.timeout).await {
            Ok(()) => {
                println!("{} <- {}", args.pv_name, args.value);
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    } else {
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
}
