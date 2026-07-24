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

use asyn_rs::asyn_record::asyn_record_factory;
use asyn_rs::drivers::ip_port::DrvAsynIPPort;
use asyn_rs::manager::PortManager;
use clap::Parser;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_ca_rs::server::CaServer;
use epics_pva_rs::server::PvaServer;

/// The asyn port every asyn reproducer attaches to. C's `asynRecord` refuses to
/// `init_record` against an empty PORT, so both differential sides name this
/// port. Both back it with the *same* port model: a `drvAsynIPPort` on
/// `localhost:1`, `noAutoConnect` — the C side via its st.cmd
/// `drvAsynIPPortConfigure("ORACLEASYN","localhost:1",0,1,0)`, the Rust side via
/// [`DrvAsynIPPort::new_configured`]. Nothing listens on `localhost:1` and
/// `noAutoConnect` keeps the port from ever dialing, so it stays permanently
/// disconnected on both sides while still answering the IP driver's `hostinfo`
/// / `disconnectOnReadTimeout` options (HOSTINFO / DRTO) exactly as C does.
const ORACLE_ASYN_PORT: &str = "ORACLEASYN";

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
    // is loaded. Kept alive for the process lifetime: dropping the
    // manager/handle would tear the port's runtime down.
    //
    // A `drvAsynIPPort` on `localhost:1`, `noAutoConnect=1`, `noProcessEos=0`
    // — the exact C st.cmd `drvAsynIPPortConfigure("ORACLEASYN","localhost:1",
    // 0,1,0)`. `noAutoConnect` (auto_connect=false) means the port never dials
    // (port.rs arms the reconnect timer only for auto-connect ports), so it
    // stays disconnected exactly like the former null port — CNCT/AUCT=0,
    // PCNCT=1 — while now answering HOSTINFO="localhost:1" / DRTO="No" the way
    // C's real IP port does. Its interface set is the same
    // `octet_transport_capabilities()` the null port pinned (asynCommon +
    // asynOption + asynOctet), so the 100+ agreeing read fields are unchanged.
    let asyn_manager = PortManager::new();
    let _asyn_port = asyn_manager.register_port(DrvAsynIPPort::new_configured(
        ORACLE_ASYN_PORT,
        "localhost:1",
        true,  // noAutoConnect — never dial; stay permanently disconnected
        false, // noProcessEos=0 — install the default EOS interpose, as C does
    )?)?;

    // Route the `asyn` record type to asyn-rs's full `AsynRecord` (overriding
    // epics-base-rs's CNCT-only display stub), so the reproducer serves the
    // real asyn field surface and attaches to the port above. No device support
    // is registered: the asyn record inits itself via `init_record`, and its
    // only DSET use is `SCAN="I/O Intr"`, which these Passive reproducers do not
    // exercise.
    let (asyn_type, asyn_factory) = asyn_record_factory();

    // Contribute asyn's device-support DTYP menus to base's `DTYP` choice lists,
    // the way a C fat `softIoc` gains them by loading `asyn.dbd`'s `device()`
    // lines. This is menu-only on purpose: it does NOT install the universal
    // asyn factory (`register_asyn_device_support*` would), so no record's read
    // fields or processing change — only the `DTYP` value.choices a client reads
    // now lists `asynInt32` / `asynOctetWrite` / ... for the base record types,
    // matching the fat-C ground truth. Process-global (the menu is per record
    // TYPE, as in C), so one call before `build()` suffices.
    asyn_rs::adapter::register_asyn_device_menus();

    let (db, _autosave) = IocBuilder::new()
        .register_record_type(asyn_type, asyn_factory)
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
    let server = CaServer::from_parts(db, 0, None, None, None, None).await?;
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
    let server = PvaServer::from_parts(db, 0, None, None, None);

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
