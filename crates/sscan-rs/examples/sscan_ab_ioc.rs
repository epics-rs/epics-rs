//! The rs half of the sscan A/B against the C `oracleSscanIoc`: serves one
//! `.db` over CA with the sscan record registered.
//!
//! ```text
//! export EPICS_CA_AUTO_ADDR_LIST=NO EPICS_CA_ADDR_LIST=127.0.0.1 \
//!        EPICS_CA_SERVER_PORT=27364 EPICS_CAS_INTF_ADDR_LIST=127.0.0.1
//! cargo run -p sscan-rs --example sscan_ab_ioc -- sscan_ab/ab.db 27364 &
//! #  or: DB=sscan_ab/ab.db oracleSscanIoc st-sscan.cmd
//! python sscan_ab/drive.py 27364 > rs.json
//! ```
//!
//! Both IOCs get the same `.db` and are driven by the same CA client, so a
//! difference in the recorded monitor streams is the record's.
// On `exec_backend` this program's `main` refuses instead of running.
#![cfg_attr(exec_backend, allow(unused_imports))]

use std::collections::HashMap;

use epics_base_rs::error::CaResult;
#[cfg(tokio_backend)]
use epics_base_rs::server::ioc_builder::IocBuilder;
#[cfg(tokio_backend)]
use epics_ca_rs::server::CaServer;

#[cfg(tokio_backend)]
#[tokio::main]
async fn main() -> CaResult<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: sscan_ab_ioc <file.db> <port>";
    let db_file = args.next().expect(usage);
    let port: u16 = args.next().and_then(|p| p.parse().ok()).expect(usage);
    let (name, factory) = sscan_rs::sscan_record_factory();
    let ioc = IocBuilder::new()
        .register_record_type(name, factory)
        .db_file(&db_file, &HashMap::new())?
        .build_ioc()
        .await?;
    let db = ioc.database().clone();
    ioc.run().await;
    let server = CaServer::from_parts(
        db,
        port,
        Some(port),
        epics_base_rs::server::access_security::new_acf_cell(None),
        None,
        None,
    )
    .await?;
    epics_base_rs::stderr_println!("[ioc] serving {db_file} on CA port {port}");
    server.run().await
}

/// The `exec_backend` arm: the CA server front-end needs the tokio backend.
#[cfg(exec_backend)]
fn main() -> CaResult<()> {
    epics_base_rs::runtime::task::refuse_backend(
        "sscan_ab_ioc: needs the tokio backend; this build selects \
         EPICS_RS_BUILD_EXEC_BACKEND=thread.",
    )
}
