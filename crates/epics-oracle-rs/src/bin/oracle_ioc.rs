//! The Rust side of the differential pair.
//!
//! Loads a `.db`, brings up a server on a kernel-assigned port, and prints
//! that port on stdout. The harness reads the number back rather than
//! predicting it: `from_parts(db, 0, ..)` binds the sockets and *then* reports
//! what it got, so nothing can take the port in between and no number is ever
//! hard-coded.
//!
//! Two protocols, selected by `--pva`:
//!
//! | mode | server | port line |
//! |---|---|---|
//! | default | [`CaServer`] | `ORACLE_IOC_PORT <n>` |
//! | `--pva` | [`PvaServer`] | `ORACLE_IOC_PVA_PORT <n>` (the UDP search port) |
//!
//! The two lines are deliberately distinct rather than one reused name: a CA
//! port and a PVA search port are not interchangeable, and a harness that
//! aimed a `pvxget` at a CA port would score every case ERROR for a reason
//! that looks like a port bug. The PVA line reports the **UDP search** port
//! because that is what a client needs; the TCP port is discovered from the
//! search reply and never named here.
//!
//! Deliberately a subprocess (not an in-process server): the harness drives
//! both sides with the same external C client tools, and giving the Rust side
//! an in-process path the C side cannot have would be a difference in the
//! instrument rather than in the thing being measured.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use epics_ca_rs::server::CaServer;
use epics_oracle_rs::{port_ioc_builder, register_port_ioc_devices};
use epics_pva_rs::server::PvaServer;

#[derive(Parser)]
#[command(about = "Rust IOC under differential test: serves a .db on a bound :0 port")]
struct Args {
    /// The `.db` to serve — the same file the C IOC is given.
    #[arg(long)]
    db: PathBuf,

    /// Serve over **PVA** instead of CA, reporting `ORACLE_IOC_PVA_PORT <n>`.
    ///
    /// The reference side is then pvxs QSRV2's `softIocPVX` rather than
    /// base's `softIoc`. The modes are exclusive: one protocol per process,
    /// so a reading is always attributable to the server that answered it.
    #[arg(long)]
    pva: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Loopback only: the oracle must never answer a real search on this host
    // while a run is in progress. Both protocols' server-side interface and
    // beacon lists are pinned, and auto-beaconing is refused outright
    // (`EPICS_PVAS_AUTO_BEACON_ADDR_LIST=NO`, pvxs `config.cpp:430`) so no
    // frame reaches a real subnet.
    unsafe {
        std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1");
        std::env::set_var("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1");
        std::env::set_var("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1");
        std::env::set_var("EPICS_PVAS_BEACON_ADDR_LIST", "127.0.0.1");
        std::env::set_var("EPICS_PVAS_AUTO_BEACON_ADDR_LIST", "NO");
    }

    let db_path = args.db.to_string_lossy().to_string();
    let macros: HashMap<String, String> = HashMap::new();

    // Register the asyn port BEFORE `build()`. The asyn record's `init_record`
    // (pass 0) runs `connectDevice`, which resolves PORT ("ORACLEASYN") out of
    // the global port registry — so the port has to be published before the db
    // is loaded. Kept alive for the process lifetime: dropping the returned
    // handle would tear the port's runtime down.
    //
    // Both halves come from the library rather than being spelled out here,
    // because `probe_supported_record_types` builds the denominator through the
    // same pair. A local copy here is how the two drift, and a denominator
    // measured against a different configuration than the one under test
    // reports record types unimplemented that the run measures perfectly well.
    let _devices = register_port_ioc_devices()?;

    let (db, _autosave) = port_ioc_builder()
        .db_file(&db_path, &macros)?
        .build()
        .await?;

    if args.pva {
        return serve_pva(db).await;
    }

    // C iocInit owns scanInit/initialProcess — the CA server does not
    // scan. The harness compares against a C softIoc whose iocInit runs
    // initialProcess and scanRun, so the Rust IOC starts the core-owned
    // scan owner (PINI pass + scan-%g threads) itself, before serving.
    let _scan_owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());

    // Port 0 => the kernel assigns; `from_parts` binds first and reports the
    // port it actually bound. This is the bind-and-read-back the harness
    // requires, not a probe-then-rebind.
    let server = CaServer::from_parts(
        db,
        0,
        None,
        epics_base_rs::server::access_security::new_acf_cell(None),
        None,
        None,
    )
    .await?;
    let port = server.udp_port();

    // Announce only after the sockets exist. The harness blocks on this line,
    // so printing it early would race the listener.
    announce("ORACLE_IOC_PORT", port)?;

    server.run().await?;
    Ok(())
}

/// Serve `db` over PVA on kernel-assigned ports, reporting the UDP search
/// port once it is bound.
///
/// `from_parts(db, 0, ..)` is the ephemeral sentinel: both TCP and UDP are
/// bound by the kernel. The port cannot be known before the bind and is not
/// guessed — `run_reporting` hands back a `ServerReportHandle` the instant
/// the listeners are up, and the number is read off that.
///
/// Read-back is not a nicety here. A PVA search socket sets `SO_REUSEPORT`,
/// so two servers on one port bind **without any error** and then answer
/// searches at random; there is no failure to detect afterwards. Allocating a
/// port, dropping it, and re-binding it later — tolerable for CA, which at
/// least prints `cas WARNING: two or more servers share the same UDP port` —
/// would here produce a silently misattributed reading, which is the one
/// outcome this harness exists to never produce.
async fn serve_pva(
    db: Arc<epics_base_rs::server::database::PvDatabase>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Same reasoning as the CA path: scanning is core-owned (C iocInit),
    // not the PVA server's — the harness's Scanned drive records need it.
    let _scan_owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());
    let server = PvaServer::from_parts(
        db,
        0,
        epics_base_rs::server::access_security::new_acf_cell(None),
        None,
        None,
    );

    // Announce from a side task rather than by racing the server future: a
    // healthy `run_reporting` never returns, and selecting on it would drop
    // the running server — the harness would then be handed the ports of a
    // server that no longer exists, and any restart would bind different
    // ones. The task observes the handle while the server keeps serving on
    // exactly the ports it reported.
    let (tx, mut rx) = tokio::sync::watch::channel::<
        Option<epics_pva_rs::server_native::ServerReportHandle>,
    >(None);
    tokio::spawn(async move {
        loop {
            if let Some(handle) = rx.borrow_and_update().clone() {
                // The UDP search port is what a client needs; the TCP port is
                // discovered from the search reply.
                if let Err(e) = announce("ORACLE_IOC_PVA_PORT", handle.report().udp_port) {
                    eprintln!("oracle-ioc: could not announce the PVA port: {e}");
                }
                return;
            }
            if rx.changed().await.is_err() {
                // Sender dropped: the server ended before it ever bound. It
                // is reporting its own error; stay silent rather than print a
                // second, less specific one.
                return;
            }
        }
    });

    // Returns only on failure — a healthy server runs until the harness kills
    // it, so reaching either arm below means no reading was obtained.
    server.run_reporting(Some(tx)).await?;
    Err("PVA server exited before the harness stopped it".into())
}

/// Print a bound port on stdout and flush, followed by the ready marker.
///
/// Flushing matters: the harness blocks on these lines, and a buffered stdout
/// on a process that then runs forever would deadlock the boot into a
/// timeout that looks like a server failure.
fn announce(key: &str, port: u16) -> std::io::Result<()> {
    use std::io::Write;
    println!("{key} {port}");
    println!("ORACLE_IOC_READY");
    std::io::stdout().flush()
}
