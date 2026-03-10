use clap::Parser;
use epics_base_rs::client::CaClient;

#[derive(Parser)]
#[command(name = "rcainfo", about = "Show EPICS PV channel information")]
struct Args {
    /// PV name to query
    pv_name: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = CaClient::new().await.expect("failed to create CA client");

    match client.cainfo(&args.pv_name).await {
        Ok(info) => {
            println!("{}:", info.pv_name);
            println!("    Server:         {}", info.server_addr);
            println!("    Type:           {:?}", info.native_type);
            println!("    Element count:  {}", info.element_count);
            println!("    Access:         {}", info.access_rights);
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
