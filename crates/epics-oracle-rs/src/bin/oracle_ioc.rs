//! The Rust side of the differential pair.
//!
//! Loads a `.db`, brings up a server on the claimed `--port`, and prints
//! that port on stdout. The harness reads the number back rather than
//! predicting it: `from_parts(db, port, ..)` binds the sockets and *then* reports
//! what it got; the number is a claim the harness holds, never hard-coded.
//!
//! Two protocols, selected by `--pva`. The server types are named in code
//! spans rather than linked: `CaServer` exists on the tokio backend only, and
//! this doc block is rendered on both.
//!
//! | mode | server | port line |
//! |---|---|---|
//! | default | `CaServer` (tokio), `BlockingCaServing` (exec) | `ORACLE_IOC_PORT <n>` |
//! | `--pva` | `PvaServer` | `ORACLE_IOC_PVA_PORT <n>` (the UDP search port) |
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
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_app::InitializedServers;
#[cfg(tokio_backend)]
use epics_ca_rs::server::CaServerBuilder;
use epics_oracle_rs::{port_ioc_builder, register_port_ioc_devices};
use epics_pva_rs::server::PvaServer;

#[derive(Parser)]
#[command(about = "Rust IOC under differential test: serves a .db on a claimed port")]
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

    /// The port to serve on (CA UDP+TCP, or PVA UDP+TCP). Required: the harness
    /// claims it (`epics_oracle_rs::ioc::claim_port`) so that it is never drawn
    /// from the kernel's ephemeral pool, where any other socket could be handed
    /// the same number. The bound port is still reported back.
    #[arg(long)]
    port: u16,

    /// A `NAME=VALUE` substitution for the `.db`, repeatable — the macro
    /// string the C oracle's `st-*.cmd` hands `dbLoadRecords` (`P=`).
    #[arg(long = "macro", value_parser = parse_macro)]
    macros: Vec<(String, String)>,

    /// `PORT,MAXCHANS,TEMPLATE`, repeatable — C st.cmd's
    /// `drvScalerSoftConfigure("PORT", MAXCHANS, "TEMPLATE")`. The template
    /// takes `--macro` substitutions as the st.cmd line takes iocsh's
    /// (`$(P=)c%d`), and the channel number at `%d`.
    #[cfg(feature = "scaler")]
    #[arg(long = "scaler-soft", value_parser = parse_scaler_soft)]
    scaler_soft: Vec<(String, usize, String)>,
}

#[cfg(feature = "scaler")]
fn parse_scaler_soft(text: &str) -> Result<(String, usize, String), String> {
    let mut parts = text.splitn(3, ',');
    let (Some(port), Some(chans), Some(template)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(format!("{text:?} is not PORT,MAXCHANS,TEMPLATE"));
    };
    let chans = chans
        .trim()
        .parse()
        .map_err(|e| format!("MAXCHANS {chans:?}: {e}"))?;
    Ok((port.trim().to_string(), chans, template.trim().to_string()))
}

fn parse_macro(text: &str) -> Result<(String, String), String> {
    let (name, value) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not NAME=VALUE"))?;
    Ok((name.to_string(), value.to_string()))
}

#[cfg(tokio_backend)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run().await
}

/// The reactor-free arm: the same `run`, parked on the main thread.
#[cfg(exec_backend)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    epics_base_rs::runtime::task::block_on_sync(run())?
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
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
        // `PvaServer::from_parts(.., Some(port), ..)` names the TCP port only;
        // the search port is the environment's (default 5076, a port shared
        // with the host), so the claim is put there too.
        std::env::set_var("EPICS_PVAS_BROADCAST_PORT", args.port.to_string());
    }

    let db_path = args.db.to_string_lossy().to_string();
    let macros: HashMap<String, String> = args.macros.into_iter().collect();

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
    let boot_db = std::sync::Arc::new(epics_base_rs::server::database::PvDatabase::new());
    let _devices = register_port_ioc_devices(&boot_db)?;

    // C st-scaler.cmd configures the ports before `dbLoadRecords`, because
    // `devScalerAsyn`'s `init_record` connects to them.
    #[cfg(feature = "scaler")]
    for (port, chans, template) in &args.scaler_soft {
        let template = epics_base_rs::runtime::mac_lib::substitute_macros(template, &macros);
        epics_oracle_rs::scaler_soft_ports().configure(port, *chans, &template)?;
    }

    // The network half of C's dbCa, libca, for names this IOC does not serve,
    // and under `--pva` pvxs's `lsetPVX`, which softIocPVX links and softIoc
    // does not.
    let reactor =
        epics_base_rs::runtime::task::Reactor::current().expect("main runs on the IOC's reactor");
    let builder = port_ioc_builder();
    let builder = if args.pva {
        epics_pva_rs::pvalink::register_pva_link(builder).0
    } else {
        builder
    };
    let mut builder = builder
        .register_link_type("ca", move |db| {
            epics_ca_rs::calink::calink_link_set_install(db, reactor)
        })
        .with_database(boot_db)
        .db_file(&db_path, &macros)?;
    // C `rsrv_init`, at `dbInitServers()` inside `iocInit`: its address-list
    // warnings come between the PINI=YES and PINI=RUN passes.
    if !args.pva {
        builder = builder.register_server_init(
            epics_ca_rs::server::RSRV_SERVER,
            epics_base_rs::server::ioc_app::ServerInitArgs {
                port: args.port,
                tcp_port: None,
            },
            epics_ca_rs::server::rsrv_init,
        );
    }
    let mut ioc = match builder.build_ioc().await {
        Ok(ioc) => ioc,
        // `softIoc -d`: `dbLoadRecords` has printed its own lines, and
        // `errIf` exits 2 with an empty message (`softMain.cpp:198`).
        Err(epics_base_rs::error::CaError::DbLoadFailed(_)) => std::process::exit(2),
        Err(e) => return Err(e.into()),
    };

    // C `connectPVs`, which waits for `interruptAccept`: the inputs are
    // records of the database just loaded. A port missing an input fails on
    // its own; the IOC keeps running.
    #[cfg(feature = "scaler")]
    epics_oracle_rs::scaler_soft_ports().connect(ioc.database());

    // C iocInit owns scanInit/initialProcess — neither server scans. The
    // harness compares against a C softIoc whose iocInit runs
    // initialProcess and scanRun, so the Rust IOC runs its `iocRun` itself,
    // before serving either protocol.
    let db = Arc::clone(ioc.database());
    let mut servers = std::mem::take(ioc.servers());
    ioc.run().await;

    if args.pva {
        return serve_pva(db, args.port).await;
    }
    serve_ca(db, args.port, &mut servers).await
}

/// Serve `db` over CA on the sockets `iocInit` bound, reporting the port the
/// server holds, which the harness checks against the one it claimed.
///
/// Announce only after the sockets exist. The harness blocks on this line, so
/// printing it early would race the listener.
#[cfg(tokio_backend)]
async fn serve_ca(
    db: Arc<PvDatabase>,
    port: u16,
    servers: &mut InitializedServers,
) -> Result<(), Box<dyn std::error::Error>> {
    let server = CaServerBuilder::serving(
        db,
        epics_base_rs::server::access_security::new_acf_cell(None),
        None,
        None,
    )
    .port(port)
    .bound_at_init(servers)
    .build()
    .await?;
    announce("ORACLE_IOC_PORT", server.udp_port())?;
    server.run().await?;
    Ok(())
}

/// The exec arm of [`serve_ca`]: the blocking RSRV on the same sockets,
/// announced as `CaServer::run` announces. `RsrvServing` would serve them too,
/// but does not expose the port it serves on.
#[cfg(exec_backend)]
async fn serve_ca(
    db: Arc<PvDatabase>,
    port: u16,
    servers: &mut InitializedServers,
) -> Result<(), Box<dyn std::error::Error>> {
    use epics_ca_rs::server::blocking_serving::BlockingCaServing;
    use epics_ca_rs::server::{RSRV_SERVER, RsrvSockets, ServerStats};

    let sockets = servers
        .take::<RsrvSockets>(RSRV_SERVER)
        .ok_or_else(|| format!("rsrv_init bound no CA sockets on port {port}"))?;
    let serving = BlockingCaServing::serve(
        sockets,
        db,
        epics_base_rs::server::access_security::new_acf_cell(None),
        Arc::new(ServerStats::default()),
    )?;
    epics_base_rs::server::db_server::announce_serving(RSRV_SERVER);
    announce("ORACLE_IOC_PORT", serving.udp_port())?;
    epics_base_rs::runtime::task::spawn_blocking(move || serving.join()).await??;
    Ok(())
}

/// Serve `db` over PVA on the claimed port, reporting the UDP search
/// port once it is bound.
///
/// TCP is `Some(port)`, UDP search is `EPICS_PVAS_BROADCAST_PORT` (set in
/// `main`). `run_reporting` hands back a `ServerReportHandle` the instant the
/// listeners are up, and the number is read off that.
///
/// Read-back is not a nicety here. A PVA search socket sets `SO_REUSEPORT`,
/// so two servers on one port bind **without any error** and then answer
/// searches at random; there is no failure to detect afterwards. Allocating a
/// port, dropping it, and re-binding it later — tolerable for CA, which at
/// least prints `cas WARNING: two or more servers share the same UDP port` —
/// would here produce a silently misattributed reading, which is the one
/// outcome this harness exists to never produce.
async fn serve_pva(db: Arc<PvDatabase>, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let server = PvaServer::from_parts(
        db,
        Some(port),
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
    let reactor =
        epics_base_rs::runtime::task::Reactor::current().expect("serve_pva runs on the reactor");
    reactor.spawn(async move {
        loop {
            if let Some(handle) = rx.borrow_and_update().clone() {
                // The UDP search port is what a client needs; the TCP port is
                // discovered from the search reply.
                if let Err(e) = announce("ORACLE_IOC_PVA_PORT", handle.report().udp_port) {
                    epics_base_rs::stderr_println!(
                        "oracle-ioc: could not announce the PVA port: {e}"
                    );
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
