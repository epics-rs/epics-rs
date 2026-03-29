use clap::Parser;
use epics_base_rs::client::CaClient;

#[derive(Parser)]
#[command(name = "rcainfo", about = "Show EPICS PV channel information")]
struct Args {
    /// PV names to query
    #[arg(required = true)]
    pv_names: Vec<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = CaClient::new().await.expect("failed to create CA client");

    let mut failed = false;
    for pv_name in &args.pv_names {
        match client.cainfo(pv_name).await {
            Ok(info) => {
                println!("{}:", info.pv_name);
                println!("    Server:         {}", info.server_addr);
                println!("    Type:           {:?}", info.native_type);
                println!("    Element count:  {}", info.element_count);
                println!("    Access:         {}", info.access_rights);
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
