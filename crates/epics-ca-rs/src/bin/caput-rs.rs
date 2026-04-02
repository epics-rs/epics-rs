use clap::Parser;
use epics_ca_rs::client::CaClient;
use std::time::Duration;

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

    let ch = client.create_channel(&args.pv_name);
    if let Err(e) = ch.wait_connected(Duration::from_secs(3)).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }

    // Read old value and get native type in one call
    let (native_type, old_value) = match ch.get().await {
        Ok((t, val)) => (t, val.to_string()),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let value = match epics_ca_rs::EpicsValue::parse(native_type, &args.value) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let result = if args.callback {
        ch.put_with_timeout(&value, Duration::from_secs_f64(args.timeout))
            .await
    } else {
        ch.put_nowait(&value).await
    };

    match result {
        Ok(()) => {
            println!("Old : {} {}", args.pv_name, old_value);
            println!("New : {} {}", args.pv_name, args.value);
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
