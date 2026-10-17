//! CA server components — TCP handler, UDP search, beacon, monitor.

pub mod access_token;
pub mod addr_list;
// The async server front-end — the `tokio::net` accept/introspection
// stack and the `CaServer` orchestrator (which also drives `tokio::signal` and
// the `discovery` stack) — is host-only; its deps do not build for RTEMS or
// VxWorks. The embedded build serves CA through the `std::net` `blocking`
// driver plus the runtime-agnostic shared logic in
// `tcp`/`udp`/`beacon`/`monitor`/`stats`. The gate is `tokio_backend` — the accept loop
// hands each client to `runtime::task::Reactor`, so it needs a build where
// that seam is the tokio runtime, which excludes the two embedded targets and
// a host `exec_backend` build alike.
pub mod beacon;
pub mod blocking;
pub mod blocking_serving;
#[cfg(tokio_backend)]
pub mod ca_server;
#[cfg(test)]
mod delivery_alloc_tests;
pub(crate) mod frame;
#[cfg(tokio_backend)]
pub mod introspection;
pub mod ioc_app;
pub mod iocsh;
pub mod monitor;
pub mod outbox;
pub mod rate_limit;
pub(crate) mod recv;
pub(crate) mod send;
#[cfg(all(feature = "cap-tokens", tokio_backend))]
pub mod signed_beacon;
pub mod stats;
pub mod tcp;
pub mod udp;

#[cfg(tokio_backend)]
pub use ca_server::{AccessRightsNotifier, CaServer, CaServerBuilder};

/// The name RSRV's init is registered under — C's `dbServer.name` for RSRV
/// (`caservertask.c:1562`).
pub const RSRV_SERVER: &str = "rsrv";

/// RSRV's sockets as [`rsrv_init`] binds them on this backend.
#[cfg(tokio_backend)]
pub type RsrvSockets = ca_server::RsrvSockets;
#[cfg(exec_backend)]
pub type RsrvSockets = blocking_serving::BlockingRsrvSockets;

/// C `rsrv_init`, RSRV's `dbServer.init`: what
/// [`epics_base_rs::server::ioc_app::IocApplication::register_server_init`]
/// runs under [`RSRV_SERVER`], on either backend, so a CA port this IOC
/// cannot have fails `iocInit` and not a later line.
pub async fn rsrv_init(
    args: epics_base_rs::server::ioc_app::ServerInitArgs,
) -> epics_base_rs::error::CaResult<RsrvSockets> {
    #[cfg(tokio_backend)]
    return RsrvSockets::bind(args).await;
    #[cfg(exec_backend)]
    RsrvSockets::bind(args.port, args.tcp_port.unwrap_or(args.port))
        .map_err(|e| epics_base_rs::error::CaError::InvalidValue(e.to_string()))
}

/// The blocking RSRV on the sockets its init bound into `servers`, or bound
/// here for a runner given an `IocRunConfig` by hand.
#[cfg(exec_backend)]
fn serve_blocking(
    servers: &mut epics_base_rs::server::ioc_app::InitializedServers,
    port: u16,
    tcp_port: Option<u16>,
    db: std::sync::Arc<epics_base_rs::server::database::PvDatabase>,
    acf: epics_base_rs::server::access_security::AcfCell,
) -> epics_base_rs::error::CaResult<blocking_serving::BlockingCaServing> {
    let invalid = |e: blocking_serving::ServeError| {
        epics_base_rs::error::CaError::InvalidValue(e.to_string())
    };
    let sockets = match servers.take::<RsrvSockets>(RSRV_SERVER) {
        Some(sockets) => sockets,
        None => RsrvSockets::bind(port, tcp_port.unwrap_or(port)).map_err(invalid)?,
    };
    blocking_serving::BlockingCaServing::serve(
        sockets,
        db,
        acf,
        std::sync::Arc::new(ServerStats::default()),
    )
    .map_err(invalid)
}
/// Live-connection / byte / channel / subscription counters. Runtime-agnostic
/// (pure atomics) and shared by the async server and the blocking driver's
/// monitor path, so it lives outside the host-only `ca_server` module.
pub use stats::ServerStats;
pub use tcp::ServerConnectionEvent;

// `run_ca_ioc` (below) builds a `CaServer` — the async front-end — on tokio;
// every reactor-free build serves through `blocking_serving` instead: the two
// embedded targets, and a host `exec_backend` build with them.
use epics_base_rs::error::CaResult;
use epics_base_rs::server::ioc_app::IocRunConfig;

pub(crate) use epics_base_rs::server::database::channel_io::LongStringMode;
pub(crate) use epics_base_rs::server::database::dbr_delivery::DeliveryStage;

/// Stand `app` up on Channel Access — [`run_ca_ioc`] with C's `rsrvRegistrar`
/// already run, which is the only order that works.
///
/// C runs `rsrvRegistrar` out of `dbLoadDatabase`'s `.dbd` expansion
/// (`rsrvIocRegister.c:34-38`), so `casr` and the `dbServer` join are in
/// place before the first `st.cmd` line. Rust has no link-time registrar, so
/// the port has to make the call — and a call every application head must
/// remember is a rule that holds until one forgets. Pairing it with the
/// runner here is what removes the choice: the head that wants a CA server
/// says so once, and gets the registrar with it.
///
/// `IocApplication::run(run_ca_ioc)` still works and still registers `casr`
/// on the INTERACTIVE shell — it is the startup shell, the one `st.cmd`
/// executes on, that only this entry point can reach.
///
/// The `ca` link set comes with it for the same reason: C's dbCa is in every
/// IOC, so it is the runner's to install, not each head's.
pub async fn run_ca_ioc_app(app: epics_base_rs::server::ioc_app::IocApplication) -> CaResult<()> {
    iocsh::register_rsrv_commands(app)
        .register_link_type("ca", {
            let reactor = epics_base_rs::runtime::task::Reactor::current()
                .expect("run_ca_ioc_app is awaited on the IOC's reactor");
            move |db| crate::calink::calink_link_set_install(db, reactor)
        })
        .run(run_ca_ioc)
        .await
}

/// Run an IOC with the Channel Access protocol.
///
/// This is the standard protocol runner for [`epics_base_rs::server::ioc_app::IocApplication::run`].
/// It creates a [`CaServer`] from the provided configuration and
/// starts the CA server with an interactive iocsh shell.
///
/// # Example
///
/// ```rust,ignore
/// epics_ca_rs::server::run_ca_ioc_app(
///     IocApplication::new().startup_script("st.cmd"),
/// )
/// .await
/// ```
///
/// [`run_ca_ioc_app`] rather than `.run(run_ca_ioc)`: a head that hands this
/// runner to `IocApplication::run` directly gets `casr` on the prompt but not
/// on the script, because this function is dispatched after the script has
/// already run.
#[cfg(tokio_backend)]
pub async fn run_ca_ioc(mut config: IocRunConfig) -> CaResult<()> {
    // For a caller that reaches the CA server only through this runner: C's
    // `registrar(rsrvRegistrar)` is there from the moment the binary links
    // RSRV, so it must not wait on `run_with_shell`. It is still too late for
    // the startup script, which has already run by the time this runner is
    // dispatched; `run_ca_ioc_app` is the entry point that is early enough.
    iocsh::declare_rsrv_registrar();
    let mut builder = CaServerBuilder::serving(
        config.db,
        config.acf,
        config.autosave_config,
        config.autosave_manager,
    )
    .port(config.port)
    .bound_at_init(&mut config.servers);
    if let Some(tcp_port) = config.tcp_port {
        builder = builder.tcp_port(tcp_port);
    }
    let server = builder.build().await?;
    // `config.after_init_hooks` is always handed over EMPTY —
    // `IocApplication::run` drains the hooks itself after PINI (H3) and
    // owns scanning via the core `ScanOwner`, so the CA server neither
    // runs hooks nor scans.
    // No `casr` here: `run_with_shell` registers it for every caller, so
    // pushing a second copy would depend on which one the registry keeps.
    server
        .run_with_shell(move |shell| {
            for cmd in config.shell_commands {
                shell.register(cmd);
            }
        })
        .await
}

/// [`run_ca_ioc`] on the exec backend: RSRV through
/// [`blocking_serving::BlockingCaServing`], then the same `dbServer` join and
/// shell the tokio `CaServer::run_with_shell` gives.
#[cfg(exec_backend)]
pub async fn run_ca_ioc(mut config: IocRunConfig) -> CaResult<()> {
    iocsh::declare_rsrv_registrar();
    let serving = serve_blocking(
        &mut config.servers,
        config.port,
        config.tcp_port,
        config.db.clone(),
        config.acf.clone(),
    )?;
    iocsh::register_ca_db_server();
    epics_base_rs::server::db_server::db_run_servers();
    epics_base_rs::server::db_server::announce_serving();
    let result = config.run_shell(iocsh::ca_server_commands()).await;
    serving.shutdown();
    result
}

/// RSRV serving a database in the background, on either backend: the async
/// `CaServer` on a reactor task under `tokio_backend`, the
/// [`blocking_serving::BlockingCaServing`] threads under `exec_backend`.
///
/// For a head that serves CA beside something else in the foreground (a PVA
/// shell, a second server). Bound, joined to `dbServer` and announced by the
/// time [`Self::start`] returns, as C's `rsrv_run` has when `iocRun` returns.
pub struct RsrvServing {
    #[cfg(tokio_backend)]
    task: epics_base_rs::runtime::task::TaskHandle<CaResult<()>>,
    #[cfg(exec_backend)]
    serving: blocking_serving::BlockingCaServing,
}

impl RsrvServing {
    /// Serve `db` on UDP `port` and TCP `tcp_port` (`None` shares `port`),
    /// on the sockets RSRV's init bound into `servers` when it ran.
    pub async fn start(
        db: std::sync::Arc<epics_base_rs::server::database::PvDatabase>,
        port: u16,
        tcp_port: Option<u16>,
        acf: epics_base_rs::server::access_security::AcfCell,
        autosave_config: Option<(
            epics_base_rs::server::autosave::SaveSetConfig,
            epics_base_rs::server::autosave::SaveStrategy,
        )>,
        autosave_manager: Option<std::sync::Arc<epics_base_rs::server::autosave::AutosaveManager>>,
        servers: &mut epics_base_rs::server::ioc_app::InitializedServers,
    ) -> CaResult<Self> {
        #[cfg(tokio_backend)]
        {
            let mut builder =
                CaServerBuilder::serving(db, acf, autosave_config, autosave_manager).port(port);
            if let Some(tcp_port) = tcp_port {
                builder = builder.tcp_port(tcp_port);
            }
            let server = builder.bound_at_init(servers).build().await?;
            // `CaServer::run` drives `tokio::net`, so the task goes on the
            // tokio runtime this is awaited on.
            let task = tokio::runtime::Handle::try_current()
                .expect("RsrvServing::start is awaited on the IOC's reactor")
                .spawn(async move {
                    let result = server.run().await;
                    if let Err(e) = &result {
                        eprintln!("CA server error: {e}");
                    }
                    result
                });
            Ok(Self { task })
        }
        #[cfg(exec_backend)]
        {
            // The blocking server has no autosave tail; the PVA server or the
            // IOC head runs it.
            let _ = (autosave_config, autosave_manager);
            let serving = serve_blocking(servers, port, tcp_port, db, acf)?;
            iocsh::register_ca_db_server();
            epics_base_rs::server::db_server::db_run_servers();
            epics_base_rs::server::db_server::announce_serving();
            Ok(Self { serving })
        }
    }

    /// Until serving ends, with its result.
    pub async fn wait(self) -> CaResult<()> {
        #[cfg(tokio_backend)]
        let joined = self.task.await;
        #[cfg(exec_backend)]
        let joined =
            epics_base_rs::runtime::task::spawn_blocking(move || self.serving.join()).await;
        joined.unwrap_or_else(|e| {
            Err(epics_base_rs::error::CaError::InvalidValue(format!(
                "CA server task failed: {e}"
            )))
        })
    }

    /// Stop serving.
    pub fn shutdown(&self) {
        #[cfg(tokio_backend)]
        self.task.abort();
        #[cfg(exec_backend)]
        self.serving.shutdown();
    }
}

/// The address lists RSRV's `casr` prints from level 1 up
/// (`caservertask.c:938-1017`), assembled for
/// [`iocsh::casr_command`].
///
/// C builds its `servers` list at bind time out of `casIntfAddrList`,
/// and its multicast / beacon / ignore lists out of the same env parse;
/// [`addr_list::from_env`] is that parse and is memoized, so reading it
/// here re-derives the binder's own lists rather than re-resolving them.
/// The one place this can disagree with the sockets actually bound is an
/// interface whose broadcast responder failed to bind — `bind_udp_responders`
/// logs that failure and continues, and C's report, reading the socket,
/// would drop back to the "name server" wording.
#[cfg(tokio_backend)]
pub(crate) fn casr_addrs(server: &CaServer) -> CaResult<iocsh::CasrAddrs> {
    use std::net::SocketAddr;

    let cfg = addr_list::from_env()?;
    let (udp_port, tcp_port) = (server.udp_port(), server.tcp_port());
    Ok(iocsh::CasrAddrs {
        interfaces: cfg
            .intf_addrs
            .iter()
            .map(|ip| iocsh::CasrInterface {
                tcp: SocketAddr::from((*ip, tcp_port)),
                udp: SocketAddr::from((*ip, udp_port)),
                udp_bcast: bcast_responder_addr(*ip, udp_port),
            })
            .collect(),
        mcast: cfg
            .mcast_addrs
            .iter()
            .map(|ip| SocketAddr::from((*ip, udp_port)))
            .collect(),
        beacon: cfg.beacon_addrs.clone(),
        // C prints these with `sin_port = 0`.
        ignore: cfg
            .ignore_addrs
            .iter()
            .map(|ip| SocketAddr::from((*ip, 0)))
            .collect(),
    })
}

/// Where C binds a second UDP name-server socket for an interface, and this
/// port binds one too.
///
/// Loopback is the one place the answer is `None` while C still binds:
/// `osiSockDiscoverBroadcastAddresses` short-circuits an `INADDR_LOOPBACK`
/// match to the loopback address itself (`osdNetIfAddrs.c:42-54`), so
/// `rsrv_init` binds `udpbcast` to the same `127.0.0.1:<port>` under
/// `epicsSocketEnableAddressUseForDatagramFanout` and runs a second
/// `cast_server` thread, `CAS-UDP2` (`caservertask.c:677-706`, `:728-738`).
/// SO_REUSEPORT hands each datagram to exactly one of that pair, so C
/// answers a loopback search once — which is what one socket does.
/// Measured with `ss -lunp`, `softIoc R7.0.10` against this port with
/// `EPICS_CAS_INTF_ADDR_LIST` pinned: on `172.17.0.1` (`IFF_BROADCAST`)
/// both bind `172.17.0.1:5188` and `172.17.255.255:5188`; on `127.0.0.1`
/// C binds `127.0.0.1:5188` twice and this binds it once.
///
/// `casr` needs no adjustment for that: C picks its wording from whether
/// the second socket exists, not from what the interface is
/// (`caservertask.c:953-966`), so one socket prints C's own single
/// `CAS-UDP name server` line.
///
/// The broadcast responder address for one interface, under the same
/// gate `bind_udp_responders` binds the socket with — C
/// `caservertask.c:671,728` skips it on Windows and Cygwin, and there is no
/// broadcast address for a wildcard or loopback interface.
#[cfg(tokio_backend)]
fn bcast_responder_addr(ip: std::net::Ipv4Addr, port: u16) -> Option<std::net::SocketAddr> {
    #[cfg(not(ca_udp_bcast))]
    {
        let _ = (ip, port);
        None
    }
    #[cfg(ca_udp_bcast)]
    {
        addr_list::broadcast_for_ip(ip).map(|b| std::net::SocketAddr::from((b, port)))
    }
}

#[cfg(test)]
mod spawn_capability_guard {
    //! The CA server's production code may not read an executor out of the
    //! calling thread.
    //!
    //! Two entry points state which executor the server is on, and they are
    //! the only places allowed to: `run_with_shell` starts `run` on the
    //! ambient tokio `Handle` (`ca_server.rs:902`) because `run` drives
    //! `tokio::net` listeners, and `run` mints from its own runtime the two
    //! capabilities the sites below it need (`:997` tokio, `:999` seam).
    //! `bridge.reactor()` used to place `run`, and on the exec backend that
    //! is a callback band with no reactor, so the server bound its sockets
    //! and then panicked inside `tokio::net` on the first accepted client.
    //!
    //! Below those two, a task site must take the capability as an argument.
    //! A bare `tokio::spawn` reads the thread-local of whichever worker
    //! happens to poll the caller, which is not necessarily the runtime that
    //! owns the sockets — and being right on the host proves nothing about
    //! the placement, which is the whole failure mode.
    //!
    //! `spawn_blocking` is deliberately not on this list — it names a
    //! different pool with different band semantics on the exec backend, and
    //! folding it in here would hide that difference behind one needle.
    //!
    //! `introspection.rs` is not on the file list either, and for the opposite
    //! reason: its accept loop hands each request to a handler that does
    //! `tokio::net` I/O, so that task belongs on the tokio runtime and nowhere
    //! else — routing it through the seam `Reactor` puts it on a callback
    //! worker under `exec_backend`, where it panics on the first poll
    //! (`end_to_end_healthz` measures exactly that). The ambient read is sound
    //! there because a successful `TcpListener::bind` earlier on the same task
    //! already proves the runtime it reads.
    //!
    //! Needles are assembled with `concat!` so this module's own text cannot
    //! satisfy the check it performs.

    use source_guard::{Comments, production};

    #[test]
    fn server_production_spawns_go_through_a_held_reactor() {
        let files: [(&str, &str); 4] = [
            ("ca_server.rs", include_str!("ca_server.rs")),
            ("tcp.rs", include_str!("tcp.rs")),
            ("blocking.rs", include_str!("blocking.rs")),
            ("monitor.rs", include_str!("monitor.rs")),
        ];

        // Fail closed: if the slicer ever stops covering the code this guard
        // is about, say so instead of reporting a vacuous pass.
        let anchors = [
            ("ca_server.rs", "pub async fn run("),
            ("tcp.rs", "fn write_notify_queue("),
            ("blocking.rs", "fn command_drives_without_spawn("),
            ("monitor.rs", "fn deliver_monitor_event"),
        ];

        let bare = concat!("tokio", "::spawn(");
        let aliased = concat!("tokio::task", "::spawn(");

        for (name, src) in files {
            let prod = production(src, Comments::Strip);
            let anchor = anchors.iter().find(|(n, _)| *n == name).unwrap().1;
            assert!(
                prod.contains(anchor),
                "{name}: production slice no longer contains `{anchor}` — the \
                 slicer stopped covering the guarded code"
            );
            assert_eq!(
                prod.matches(bare).count(),
                0,
                "{name}: production must start tasks on a capability handed \
                 down from `run`; found bare `{bare}`, which reads the \
                 executor out of whichever worker polls the caller"
            );
            assert_eq!(
                prod.matches(aliased).count(),
                0,
                "{name}: found `{aliased}` — the same ambient read under \
                 another name"
            );
        }
    }
}
