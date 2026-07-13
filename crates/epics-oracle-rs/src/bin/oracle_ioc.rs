//! The Rust side of the differential pair.
//!
//! Loads a `.db`, brings up the CA server on a kernel-assigned port, and
//! prints that port on stdout as `ORACLE_IOC_PORT <n>`. The harness reads the
//! number back rather than predicting it: `CaServer::from_parts(db, 0, ..)`
//! binds the sockets and *then* reports what it got, so nothing can take the
//! port in between and no number is ever hard-coded.
//!
//! Deliberately a subprocess (not an in-process server): the harness drives
//! both sides with the same external C CA tools, and giving the Rust side an
//! in-process path the C side cannot have would be a difference in the
//! instrument rather than in the thing being measured.

use std::collections::HashMap;
use std::path::PathBuf;

use clap::Parser;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_ca_rs::server::CaServer;

#[derive(Parser)]
#[command(about = "Rust IOC under differential test: serves a .db over CA on a bound :0 port")]
struct Args {
    /// The `.db` to serve — the same file the C softIoc is given.
    #[arg(long)]
    db: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Loopback only: the oracle must never answer a real CA search on this
    // host while a run is in progress.
    unsafe {
        std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1");
        std::env::set_var("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1");
    }

    let db_path = args.db.to_string_lossy().to_string();
    let macros: HashMap<String, String> = HashMap::new();

    let (db, _autosave) = IocBuilder::new()
        .db_file(&db_path, &macros)?
        .build()
        .await?;

    // Port 0 => the kernel assigns; `from_parts` binds first and reports the
    // port it actually bound. This is the bind-and-read-back the harness
    // requires, not a probe-then-rebind.
    let server = CaServer::from_parts(db, 0, None, None, None, None).await?;
    let port = server.udp_port();

    // Announce only after the sockets exist. The harness blocks on this line,
    // so printing it early would race the listener.
    println!("ORACLE_IOC_PORT {port}");
    println!("ORACLE_IOC_READY");
    use std::io::Write;
    std::io::stdout().flush()?;

    server.run().await?;
    Ok(())
}
