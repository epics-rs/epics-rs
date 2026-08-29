//! iocsh commands for the CA server — currently just `casr`.
//!
//! The output is RSRV's, line for line: `casr` is a diagnostic an
//! operator reads across a mixed site, so a line that only this
//! implementation prints — or one of C's that it does not — costs them
//! the ability to compare two IOCs without first knowing which is
//! which. Every string below is transcribed from `casr`
//! (`caservertask.c:907-1051`) and `log_one_client` (`:822-902`) at
//! `R7.0.10`.
//!
//! Wiring is C's, and C's is a registrar: `rsrvRegistrar`
//! (`rsrvIocRegister.c:34-38`) runs while `dbLoadDatabase` expands the
//! `.dbd`, so `casr` is a known command from the first `st.cmd` line —
//! long before `iocInit` stands RSRV up. It answers for the absent server
//! by printing nothing, because that is what `casr` does while
//! `clientQlock` is still NULL (`caservertask.c:910-912`).
//!
//! [`register_rsrv_commands`] is that registrar for an
//! [`epics_base_rs::server::ioc_app::IocApplication`]; the running server
//! publishes what the command reads through [`publish_casr_source`].

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock, RwLock};

use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

use super::stats::ServerStats;

/// One `EPICS_CAS_INTF_ADDR_LIST` interface, as `casr` names it — C's
/// `rsrv_iface_config` (`caservertask.c:938-971`).
#[derive(Debug, Clone)]
pub struct CasrInterface {
    /// C `iface->tcpAddr`.
    pub tcp: SocketAddr,
    /// C `iface->udpAddr`.
    pub udp: SocketAddr,
    /// C `iface->udpbcastAddr`, present exactly when C's
    /// `iface->udpbcast != INVALID_SOCKET` — the second responder
    /// socket bound to the interface's broadcast address. `None` on
    /// Windows and for a wildcard interface, which is what decides
    /// between C's "name server" and "unicast/broadcast name server"
    /// wording.
    pub udp_bcast: Option<SocketAddr>,
}

/// The address lists `casr` prints from level 1 up. Built by the caller
/// so this module needs none of the host-only binder types (it compiles
/// for `epics_embedded_target` too).
#[derive(Debug, Clone, Default)]
pub struct CasrAddrs {
    /// C's `servers` list.
    pub interfaces: Vec<CasrInterface>,
    /// C's `casMCastAddrList`.
    pub mcast: Vec<SocketAddr>,
    /// C's `beaconAddrList`.
    pub beacon: Vec<SocketAddr>,
    /// C's `casIgnoreAddrs`. C prints these through `ipAddrToDottedIP`
    /// with `sin_port = 0`, so they carry a `:0` on the wire of the
    /// report; supply them that way.
    pub ignore: Vec<SocketAddr>,
}

/// C `n == 1 ? "" : "s"`.
fn plural_s(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// C `n == 1 ? "" : "es"`.
fn plural_es(n: usize) -> &'static str {
    if n == 1 { "" } else { "es" }
}

/// `casr [<level>]` — RSRV's CA server report (`caservertask.c:907`).
///
/// # What this port cannot print, and why it prints nothing instead
///
/// C's level-1 client blocks (`log_one_client`) name each circuit's
/// peer, host, user, minor version, priority and channel count. This
/// server keeps no connection registry — `ClientState` lives in the
/// connection task and `ServerStats` is counters — so those blocks have
/// no data behind them. They are omitted rather than approximated: a
/// line an operator cannot get from C is worse here than a missing one,
/// which is the whole reason this command was rewritten.
///
/// C's level-4 block reports `freeListItemsAvail` over RSRV's client,
/// channel, event, buffer and putNotify free lists plus `bucketShow` of
/// the resource-id table. None of those structures exists here.
/// The report itself: every line `casr` prints, in order, for `clients`
/// connected clients at `level`.
///
/// Split out from the command so the wording has one owner and can be
/// asserted against a transcript of the C IOC — the only way a
/// line-for-line claim stays true after an edit. See [`casr_command`]
/// for the two blocks C prints that this port cannot.
pub fn casr_lines(clients: usize, level: i64, addrs: &CasrAddrs) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!(
        "Channel Access Server V{}.{}",
        crate::protocol::CA_MAJOR_VERSION,
        crate::protocol::CA_MINOR_VERSION
    ));

    // C tests the empty queue before the level, so a server with no
    // clients says so at every level (`caservertask.c:921-933`).
    if clients == 0 {
        out.push("No clients connected.".to_string());
    } else if level == 0 {
        out.push(format!("{clients} client{} connected.", plural_s(clients)));
    } else {
        // The colon introduces C's per-client blocks. See
        // [`casr_command`] for why none follow it here.
        out.push(format!("{clients} client{} connected:", plural_s(clients)));
    }

    if level < 1 {
        return out;
    }

    for iface in &addrs.interfaces {
        out.push(format!("CAS-TCP server on {} with", iface.tcp));
        match iface.udp_bcast {
            None => out.push(format!("    CAS-UDP name server on {}", iface.udp)),
            Some(bcast) => {
                out.push(format!("    CAS-UDP unicast name server on {}", iface.udp));
                out.push(format!("    CAS-UDP broadcast name server on {bcast}"));
            }
        }
    }

    if !addrs.mcast.is_empty() {
        let n = addrs.mcast.len();
        out.push(format!("Monitoring {n} multicast address{}:", plural_es(n)));
        out.extend(addrs.mcast.iter().map(|a| format!("    {a}")));
    }

    let beacons = addrs.beacon.len();
    out.push(format!(
        "Sending CAS-beacons to {beacons} address{}:",
        plural_es(beacons)
    ));
    out.extend(addrs.beacon.iter().map(|a| format!("    {a}")));

    if !addrs.ignore.is_empty() {
        // Transcribed exactly, quirks included: C's heading takes its
        // plural from `n`, which at this point still holds the BEACON
        // count, not the ignore count (`caservertask.c:988,1005-1007`),
        // and it ends without the colon its two neighbours carry.
        // Diverging to "fix" either would print a line no C IOC prints.
        out.push(format!(
            "Ignoring UDP messages from address{}",
            plural_es(beacons)
        ));
        out.extend(addrs.ignore.iter().map(|a| format!("    {a}")));
    }

    out
}

/// `casr [<level>]` — RSRV's CA server report (`caservertask.c:907`).
///
/// # What this port cannot print, and why it prints nothing instead
///
/// C's client blocks (`log_one_client`, reached from level 1 for TCP
/// circuits and level 2 for the name-server sockets) name each
/// circuit's peer, host, user, minor version, priority and channel
/// count. This server keeps no connection registry — `ClientState`
/// lives in the connection task and `ServerStats` is counters — so
/// those blocks have no data behind them. They are omitted rather than
/// approximated: a line an operator cannot also get from C is worse
/// here than a missing one, which is the whole reason this command was
/// rewritten.
///
/// C's level-4 block reports `freeListItemsAvail` over RSRV's client,
/// channel, event, buffer and putNotify free lists plus `bucketShow` of
/// the resource-id table. None of those structures exists here.
pub fn casr_command() -> CommandDef {
    CommandDef::new(
        "casr",
        vec![ArgDesc {
            name: "level",
            arg_type: ArgType::Int,
        }],
        "casr [<level>]",
        |args: &[ArgValue], ctx: &CommandContext| {
            let level = match args.first() {
                Some(ArgValue::Int(n)) => *n,
                _ => 0,
            };
            for line in casr_report_lines(level) {
                ctx.println(&line);
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// What `casr` reads: RSRV's own `clientQ` and `servers` statics
/// (`caservertask.c:44-52`), which the C command reaches directly because
/// there is one RSRV per process.
struct CasrSource {
    stats: Arc<ServerStats>,
    addrs: CasrAddrs,
}

/// C's `clientQlock`, in the only form this port can spell it: the cell is
/// `None` until a CA server has bound, and `casr` returns silently while it
/// is (`caservertask.c:910-912`). Process-global for C's reason — the
/// command is registered by a registrar, before any server exists, so it
/// cannot capture one — and alongside the two process-global sinks the same
/// registration already writes, `add_registrars` and `db_register_server`.
fn casr_cell() -> &'static RwLock<Option<CasrSource>> {
    static CELL: OnceLock<RwLock<Option<CasrSource>>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(None))
}

/// The single writer — C `rsrv_init` filling the statics `casr` reads.
///
/// Called from the one place that knows both halves and knows the listeners
/// are up (`CaServer::run_with_shell`, which also joins the `dbServer` list
/// there). Last write wins: a process that stands up a second CA server has
/// replaced the first, which is the only state C can be in at all.
pub fn publish_casr_source(stats: Arc<ServerStats>, addrs: CasrAddrs) {
    *casr_cell().write().unwrap() = Some(CasrSource { stats, addrs });
}

/// The single reader, shared by the `casr` command and the `dbServer`
/// report — C's two entry points are literally one function
/// (`rsrv_server.report = casr`, `caservertask.c:1563`), so a second
/// derivation here could drift from the first.
fn casr_report_lines(level: i64) -> Vec<String> {
    match casr_cell().read().unwrap().as_ref() {
        Some(src) => casr_lines(src.stats.active_clients() as usize, level, &src.addrs),
        None => Vec::new(),
    }
}

/// Every iocsh command the CA SERVER owns — the port's counterpart of the
/// `iocshRegister` calls RSRV makes for itself (`caservertask.c:907`
/// registers `casr`).
///
/// The single owner of that list, and it is a list rather than one `register`
/// call at the one caller because the ownership is the point: C registers
/// `casr` from the server, so no application can stand a CA server and not
/// have it. The port had made it an opt-in the caller pushed into
/// `IocRunConfig::shell_commands`; `run_ca_ioc` did push it and `softioc-rs`,
/// which reaches `CaServer::run_with_shell` without `run_ca_ioc`, did not —
/// so `casr` answered "Command 'casr' not registered." on an IOC whose
/// statistics it exists to print. `run_with_shell` now registers this list for
/// every caller, and [`register_rsrv_commands`] puts it on the startup shell
/// as well, which is the half `run_with_shell` cannot reach.
///
/// `CaServer::run_with_shell` is named in a code span rather than linked: this
/// module is compiled on every target and `server::ca_server` is
/// `tokio_backend`-only, so an intra-doc link from here resolves in no embedded
/// configuration (`rustdoc-embedded`, `.github/workflows/rust.yml`).
pub fn ca_server_commands() -> Vec<CommandDef> {
    declare_rsrv_registrar();
    vec![casr_command()]
}

/// C `rsrvRegistrar` (`rsrvIocRegister.c:34-38`) applied to an
/// [`IocApplication`] — all three of the things C's registrar does, at the
/// moment C does them, which is while `dbLoadDatabase` expands the `.dbd`
/// and therefore before the startup script: the `registrar(rsrvRegistrar)`
/// line, `rsrv_register_server()`, and
/// `iocshRegister(&casrFuncDef, casrCallFunc)`.
///
/// The port had all three riding on `CaServer::run_with_shell` instead,
/// which is the Phase-3 protocol runner — after the script AND after
/// `ioc_run`. Both later halves were dead there, measured on `softioc-rs`:
///
/// * `casr` on any `st.cmd` line — before `iocInit` and, after a two-second
///   wait, after it — was `ERROR st.cmd line N: Command 'casr' not
///   registered.`, which aborts the script. C `softIoc R7.0.10` runs the
///   same script with the first `casr` silent (`clientQlock` still NULL,
///   `caservertask.c:910-912`) and the second printing the full report.
/// * `dbsr` printed "No server layers registered with IOC" on a running
///   IOC, because `ioc_run` has already moved the `dbServer` phase to
///   `running` (`ioc_app.rs`, C `iocInit.c:266`) and
///   `dbRegisterServer` accepts a layer only while it is `registering`
///   (`dbServer.c:30-72`). Measured: `dbRegisterServer refused 'rsrv':
///   state=Running`.
///
/// Only the command's data is late-bound, and only because it must be:
/// [`publish_casr_source`] fills it when the server binds, which is C's
/// `rsrv_init` filling the statics its already-registered `casr` reads.
///
/// Both shells, the way `asyn-rs` registers its own set and for the same
/// reason: in C there is one command table and `st.cmd` and the prompt read
/// it alike. `CaServer::run_with_shell` also registers this list, but only
/// the paths that reach it — the dual-protocol runner stands its CA server
/// up with a bare `run()` and puts the iocsh on the PVA side, so `casr` was
/// missing from `scope_ioc`'s prompt as well as its script. The two copies
/// cannot disagree: neither captures anything, both read
/// [`publish_casr_source`].
pub fn register_rsrv_commands(mut app: IocApplication) -> IocApplication {
    register_ca_db_server();
    for cmd in ca_server_commands() {
        app = app.register_startup_command(cmd.clone());
        app = app.register_shell_command(cmd);
    }
    app
}

/// C's `registrar(rsrvRegistrar)` line — `softIoc.dbd`'s seventh — whose
/// body is the two entry points either side of this function.
///
/// C reads the line while `dbLoadDatabase` expands the `.dbd`, and
/// `dbDumpRegistrar` reports it whether or not RSRV ever starts: the list is
/// declaration residue, not a call record. The port resolves its `.dbd` at
/// build time and has no read to carry the line, so the crate holding the
/// body holds the declaration too. Both halves announce it because either
/// one may be the first a caller reaches, and the sink drops a repeat.
///
/// MUST be called by every entry point that stands up a CA server, at its
/// head, BEFORE any startup script is dispatched — measured against C
/// `softIoc` R7.0.10, whose list is identical in stdin and startup-script
/// mode because it is a property of what the binary linked, not of when the
/// server starts. Announcing it from [`ca_server_commands`] and
/// [`register_ca_db_server`] alone is announcing it from inside
/// `CaServer::run_with_shell`, which `IocApplication` reaches only after the
/// script has already run — so a script's own `dbDumpRegistrar` saw eight
/// lines where C shows ten.
pub fn declare_rsrv_registrar() {
    epics_base_rs::server::iocsh::add_registrars(&["rsrvRegistrar".to_string()]);
}

/// Register the CA server as a `dbServer` layer, so `dbsr` finds it — C
/// `rsrv_register_server` (`caservertask.c:1572-1575`), which hands
/// `dbRegisterServer` a `dbServer` whose `name` is `"rsrv"` and whose `report`
/// is `casr`. Without this the port's `dbsr` prints C's "No server layers
/// registered with IOC" forever, because nothing ever joined the list.
///
/// The report reads the same published source the `casr` command does,
/// because C's two entry points are literally one function
/// (`rsrv_server.report = casr`, `caservertask.c:1563`). Capturing a second
/// copy of `stats`/`addrs` here would be a second writer of RSRV's wording,
/// free to drift from the command's — and `EPICS_IOC_IGNORE_SERVERS=rsrv`,
/// which suppresses only this join, must not also silence `casr`, which C
/// reads straight out of the statics.
pub fn register_ca_db_server() {
    use epics_base_rs::server::db_server::{DbServer, db_register_server};
    declare_rsrv_registrar();
    db_register_server(DbServer {
        name: "rsrv",
        report: Some(Box::new(|level, out: &dyn Fn(&str)| {
            for line in casr_report_lines(level as i64) {
                out(&line);
            }
        })),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// The loopback-interface shape a C `softIoc` actually printed, and
    /// the transcript the assertions below are taken from. Captured at
    /// `R7.0.10` with `EPICS_CAS_INTF_ADDR_LIST=127.0.0.1`,
    /// `EPICS_CAS_BEACON_ADDR_LIST=127.0.0.1`,
    /// `EPICS_CAS_AUTO_BEACON_ADDR_LIST=NO` on port 42553.
    fn loopback_addrs() -> CasrAddrs {
        CasrAddrs {
            interfaces: vec![CasrInterface {
                tcp: addr("127.0.0.1:42553"),
                udp: addr("127.0.0.1:42553"),
                // C reports the loopback interface's own address as its
                // broadcast destination, so it takes the two-line
                // unicast/broadcast wording here.
                udp_bcast: Some(addr("127.0.0.1:42553")),
            }],
            mcast: Vec::new(),
            beacon: vec![addr("127.0.0.1:5065")],
            ignore: Vec::new(),
        }
    }

    #[test]
    fn casr_command_returns_named_command() {
        assert_eq!(casr_command().name, "casr");
    }

    /// Declaring C's `registrar(rsrvRegistrar)` is a side effect of
    /// `ca_server_commands`, so it may not change what the list holds nor
    /// refuse a second call — a process that stands up two CaServers reaches
    /// it again, and the declaration sink drops the repeat. The sibling half
    /// of the declaration rides in `register_ca_db_server`, exercised by
    /// `the_ca_server_registers_as_the_rsrv_layer`; it is left out here
    /// because joining the dbServer list is process-global state that test
    /// asserts the "before" of.
    #[test]
    fn declaring_the_registrar_leaves_the_command_list_intact() {
        let names: Vec<String> = ca_server_commands()
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(names, vec!["casr"]);
        let again: Vec<String> = ca_server_commands()
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(again, vec!["casr"]);
    }

    #[test]
    fn plurals_match_c() {
        // C `n == 1 ? "" : "s"` / `"es"` — zero is plural in both.
        assert_eq!((plural_s(0), plural_s(1), plural_s(2)), ("s", "", "s"));
        assert_eq!((plural_es(0), plural_es(1), plural_es(2)), ("es", "", "es"));
    }

    /// `casr` on an idle server, verbatim from the C transcript.
    #[test]
    fn level_zero_idle_matches_c() {
        assert_eq!(
            casr_lines(0, 0, &loopback_addrs()),
            ["Channel Access Server V4.13", "No clients connected."]
        );
    }

    /// `casr 1` on the same idle server, verbatim from the C
    /// transcript. Note the empty-queue line still leads: C tests
    /// `n == 0` before it tests the level.
    #[test]
    fn level_one_idle_matches_c() {
        assert_eq!(
            casr_lines(1, 0, &loopback_addrs())[1],
            "1 client connected.",
            "the singular has no trailing `s` (C `n == 1 ? \"\" : \"s\"`)"
        );
        assert_eq!(
            casr_lines(0, 1, &loopback_addrs()),
            [
                "Channel Access Server V4.13",
                "No clients connected.",
                "CAS-TCP server on 127.0.0.1:42553 with",
                "    CAS-UDP unicast name server on 127.0.0.1:42553",
                "    CAS-UDP broadcast name server on 127.0.0.1:42553",
                "Sending CAS-beacons to 1 address:",
                "    127.0.0.1:5065",
            ]
        );
    }

    /// An interface with no second broadcast responder takes C's
    /// single-line "name server" wording (`caservertask.c:955-957`).
    #[test]
    fn interface_without_broadcast_socket_uses_the_plain_wording() {
        let addrs = CasrAddrs {
            interfaces: vec![CasrInterface {
                tcp: addr("0.0.0.0:5064"),
                udp: addr("0.0.0.0:5064"),
                udp_bcast: None,
            }],
            beacon: vec![addr("255.255.255.255:5065")],
            ..Default::default()
        };
        assert_eq!(
            casr_lines(2, 1, &addrs),
            [
                "Channel Access Server V4.13",
                "2 clients connected:",
                "CAS-TCP server on 0.0.0.0:5064 with",
                "    CAS-UDP name server on 0.0.0.0:5064",
                "Sending CAS-beacons to 1 address:",
                "    255.255.255.255:5065",
            ]
        );
    }

    /// The multicast and ignore blocks, including the two things C gets
    /// wrong and this port therefore reproduces: the ignore heading's
    /// plural comes from the BEACON count, and it carries no colon.
    #[test]
    fn mcast_and_ignore_blocks_match_c_including_its_quirks() {
        let addrs = CasrAddrs {
            interfaces: Vec::new(),
            mcast: vec![addr("224.0.2.3:5064"), addr("224.0.2.4:5064")],
            beacon: vec![addr("10.0.0.255:5065")],
            ignore: vec![addr("10.0.0.7:0"), addr("10.0.0.8:0")],
        };
        assert_eq!(
            casr_lines(0, 1, &addrs),
            [
                "Channel Access Server V4.13",
                "No clients connected.",
                "Monitoring 2 multicast addresses:",
                "    224.0.2.3:5064",
                "    224.0.2.4:5064",
                "Sending CAS-beacons to 1 address:",
                "    10.0.0.255:5065",
                // singular, from the ONE beacon address, though two
                // addresses follow; and no colon.
                "Ignoring UDP messages from address",
                "    10.0.0.7:0",
                "    10.0.0.8:0",
            ]
        );
    }

    /// Level 4 adds C's free-list report, which has no analogue here —
    /// the report must not grow lines of its own instead.
    #[test]
    fn high_levels_add_no_invented_lines() {
        let a = loopback_addrs();
        assert_eq!(casr_lines(0, 4, &a), casr_lines(0, 1, &a));
    }
    /// The STARTUP shell — the one `st.cmd` runs on, built before any server
    /// exists — answers `casr`, and answers it with no server published.
    ///
    /// Measured on the `softioc-rs` binary before this fix, with a script
    /// holding `casr`, `iocInit`, `epicsThreadSleep 2`, `casr`: every one of
    /// those `casr` lines was `ERROR st.cmd line N: Command 'casr' not
    /// registered.`. C `softIoc R7.0.10` runs the same script with the first
    /// `casr` silent and the second printing the full report, because
    /// `rsrvRegistrar` registered the command out of the `.dbd` expansion
    /// (`rsrvIocRegister.c:34-38`) and `casr` returns quietly while
    /// `clientQlock` is NULL (`caservertask.c:910-912`).
    #[test]
    fn the_startup_shell_answers_casr_before_any_server_exists() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::iocsh::IocShell;
        use epics_base_rs::server::iocsh::registry::CommandOutcome;

        // RTEMS-EXEC-MODEL-ALLOW(1): the runtime is built only to capture a
        // `BlockingBridge` for the shell; the command under test then runs
        // synchronously, so this site does not need the ambient reactor the
        // feature withholds and the test passes in the exec-backend suite.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let bridge = {
            let _guard = rt.enter();
            epics_base_rs::runtime::task::BlockingBridge::capture()
        };
        let shell = IocShell::new(Arc::new(PvDatabase::new()), bridge);
        std::mem::forget(rt);

        // The pre-fix state, in the same process: the builtins alone do not
        // answer `casr`, so the assertion below is about the server's list and
        // not about `execute_line` returning `Continue` for everything.
        assert!(
            matches!(shell.execute_line("casr"), Ok(CommandOutcome::Failed)),
            "`casr` is not an iocsh builtin — it is the CA server's own command"
        );

        // Exactly what `softioc-rs` hands `IocApplication`, and nothing else.
        for cmd in register_rsrv_commands(IocApplication::new()).startup_commands() {
            shell.register(cmd.clone());
        }

        assert!(
            matches!(
                shell.execute_line("casr"),
                Ok(CommandOutcome::Continue) | Ok(CommandOutcome::Exit)
            ),
            "an unregistered command reports `Failed` from `execute_line`"
        );
        assert!(matches!(
            shell.execute_line("casr 1"),
            Ok(CommandOutcome::Continue) | Ok(CommandOutcome::Exit)
        ));
    }

    /// C `casr` reads RSRV's statics and returns without printing while
    /// `clientQlock` is NULL (`caservertask.c:910-912`) — the state this
    /// port is in for the whole startup script, since the CA server is stood
    /// up by the Phase-3 protocol runner. The command must therefore have an
    /// empty report, not an invented "no server" line.
    ///
    /// nextest runs each test in its own process, so the process-global cell
    /// this publishes into is private to this case.
    #[test]
    fn casr_reports_nothing_until_a_server_publishes() {
        assert!(
            casr_report_lines(0).is_empty(),
            "no CA server has published; C prints nothing here"
        );
        assert!(casr_report_lines(1).is_empty(), "and nothing at level 1");

        let addrs = loopback_addrs();
        publish_casr_source(Arc::new(ServerStats::default()), addrs.clone());
        assert_eq!(casr_report_lines(1), casr_lines(0, 1, &addrs));
    }

    /// C fills the statics `casr` reads in `rsrv_init`, which runs when RSRV
    /// binds its sockets (`caservertask.c:1519-1560`) and has nothing to do
    /// with a shell. The port published from `CaServer::run_with_shell`
    /// instead, so a runner that stands its CA server up with a bare `run()`
    /// — the dual-protocol one every `scope_ioc`-shaped IOC uses — left
    /// `casr` and the `dbsr` layer report empty on a fully running IOC.
    /// Measured there before this: `casr` at the prompt printed nothing.
    #[cfg(tokio_backend)]
    #[tokio::test]
    async fn building_a_server_publishes_the_casr_source() {
        use epics_base_rs::server::database::PvDatabase;

        let server = crate::server::ca_server::CaServer::from_parts(
            Arc::new(PvDatabase::new()),
            0,
            None,
            epics_base_rs::server::access_security::new_acf_cell(None),
            None,
            None,
        )
        .await
        .expect("bind an ephemeral server");

        assert!(
            !casr_report_lines(0).is_empty(),
            "`build()` bound the sockets, so `casr` has a server to report"
        );
        let detailed = casr_report_lines(1);
        assert!(
            detailed
                .iter()
                .any(|l| l.contains(&server.tcp_port().to_string())),
            "the published addresses are this server's, not a stale copy: {detailed:?}"
        );
    }

    /// The CA server joins the `dbServer` list under C's name, and `dbsr`
    /// then prints C's frame around this layer's own report — the
    /// `rsrv_register_server` half of the fix (`caservertask.c:1572-1575`).
    /// Without it `dbsr` prints "No server layers registered with IOC" no
    /// matter how many clients are connected.
    ///
    /// Through the shipped path and in C's order: the registrar joins the
    /// list, and only then does `ioc_run` move the phase
    /// (`dbRunServers`, `iocInit.c:266`). Registering after that move is
    /// refused (`dbServer.c:30-72`), which is what the port did while the
    /// join lived in the Phase-3 protocol runner — measured on `softioc-rs`
    /// as `dbRegisterServer refused 'rsrv': state=Running`, and read by an
    /// operator as "No server layers registered with IOC" on a running IOC.
    ///
    /// nextest runs each test in its own process, so the process-global
    /// registry this touches is private to this case.
    #[test]
    fn the_ca_server_registers_as_the_rsrv_layer() {
        use epics_base_rs::server::db_server::{db_run_servers, dbsr};

        let out = std::cell::RefCell::new(Vec::<String>::new());
        let sink = |s: &str| out.borrow_mut().push(s.to_string());

        dbsr(0, &sink);
        assert_eq!(
            *out.borrow(),
            vec!["No server layers registered with IOC"],
            "nothing is registered until the server says so"
        );

        out.borrow_mut().clear();
        publish_casr_source(Arc::new(ServerStats::default()), CasrAddrs::default());
        let _app = register_rsrv_commands(IocApplication::new());
        db_run_servers();
        dbsr(0, &sink);

        let lines = out.borrow().clone();
        assert_eq!(lines[0], "Server state: running");
        assert_eq!(lines[1], "Server 'rsrv'");
        assert_eq!(
            &lines[2..],
            casr_lines(0, 0, &CasrAddrs::default()),
            "the layer's own report follows its name line"
        );
        assert_eq!(lines.len(), 4, "level 0 prints no address block: {lines:?}");

        out.borrow_mut().clear();
        dbsr(1, &sink);
        assert!(
            out.borrow().len() > 4,
            "the interest level reaches the layer: {:?}",
            out.borrow()
        );
    }
}
