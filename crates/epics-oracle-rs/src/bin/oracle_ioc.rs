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

use asyn_rs::asyn_record::asyn_record_factory;
use asyn_rs::drivers::null_port::NullOctetPort;
use asyn_rs::manager::PortManager;
use clap::Parser;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_ca_rs::server::CaServer;

/// The asyn port every asyn reproducer attaches to. C's `asynRecord` refuses to
/// `init_record` against an empty PORT, so both differential sides name this
/// port; the Rust side backs it with a [`NullOctetPort`] (registered, but
/// permanently disconnected), the C side with a matching port from its st.cmd.
const ORACLE_ASYN_PORT: &str = "ORACLEASYN";

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

    // Register the null asyn port BEFORE `build()`. The asyn record's
    // `init_record` (pass 0) runs `connectDevice`, which resolves PORT
    // ("ORACLEASYN") out of the global port registry — so the port has to be
    // published before the db is loaded. Kept alive for the process lifetime:
    // dropping the manager/handle would tear the port's runtime down.
    let asyn_manager = PortManager::new();
    let _asyn_port = asyn_manager.register_port(NullOctetPort::new(ORACLE_ASYN_PORT))?;

    // Route the `asyn` record type to asyn-rs's full `AsynRecord` (overriding
    // epics-base-rs's CNCT-only display stub), so the reproducer serves the
    // real asyn field surface and attaches to the port above. No device support
    // is registered: the asyn record inits itself via `init_record`, and its
    // only DSET use is `SCAN="I/O Intr"`, which these Passive reproducers do not
    // exercise.
    let (asyn_type, asyn_factory) = asyn_record_factory();

    let (db, _autosave) = IocBuilder::new()
        .register_record_type(asyn_type, asyn_factory)
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
