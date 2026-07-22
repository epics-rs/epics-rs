//! `rtems-pva-ioc` — the RTEMS pvAccess IOC entry point.
//!
//! The PVA counterpart of `epics-ca-rs`'s `rtems-ca-ioc`, and the second
//! binary in the workspace built for the target. A runnable `main` that brings
//! up a complete PVA server on the **RTEMS execution model** and nothing else:
//! no tokio runtime is ever created, no async front-end is touched, and every
//! long-lived loop owns a dedicated OS thread. Three facilities, in the order
//! C `iocInit` starts its equivalents:
//!
//! 1. **Background executor** — `runtime::task::background_init()` (C
//!    `callbackInit`, `callback.c:286`): the callback pool
//!    (`cbLow`/`cbMedium`/`cbHigh`), the delayed timer and the scanOnce
//!    worker. Started *first* so no record processing can defer a tail into a
//!    facility that does not exist yet.
//! 2. **Database** — a small in-process database built through the ordinary
//!    [`IocBuilder`](epics_base_rs::server::ioc_builder::IocBuilder), driven to completion with
//!    [`block_on_sync`](epics_base_rs::runtime::task::block_on_sync), which on
//!    a plain thread with no runtime entered selects `park_on`.
//! 3. **PVA front-end** — [`BlockingPvaServer`](epics_pva_rs::server_native::blocking::BlockingPvaServer) over a
//!    [`CompositeSource`](epics_pva_rs::server_native::composite::CompositeSource) carrying two sources: the
//!    [`PvDatabaseSource`](epics_pva_rs::server::PvDatabaseSource) as `qsrvSingle` at order 0 and the QSRV
//!    bridge as `qsrvGroup` at order 1. The TCP accept loop runs on one
//!    thread, the UDP name-search responder on another.
//!
//! # Why this binary lives in `epics-bridge-rs`
//!
//! It started in `epics-pva-rs` and served single records only. Serving
//! `Q:group` PVs means mounting QSRV, QSRV lives in `epics-bridge-rs`, and
//! `epics-bridge-rs` already depends on `epics-pva-rs` — so the mount cannot
//! be made from the other side without a cyclic package dependency, which
//! cargo rejects outright (measured; doc/qsrv-rtems-design.md §9.7). The
//! binary moved down-graph to the crate that can see every source it
//! composes. That is also the C layering: QSRV sits above pvxs and base.
//!
//! It is still exactly **one** target PVA IOC, which is the point — one copy
//! of the source-text guards at the bottom of this file, one `STATUS_PREFIX`,
//! and no extra `-Zbuild-std` build in the portability gate.
//!
//! # `pva://` record links do not resolve on this target
//!
//! A C IOC linked against pvxs gets pvalink through `pvalink_enable()`
//! (`ioc/iochooks.cpp:495`), so `INP=@pva://...` resolves. This one does not,
//! and cannot yet: pvalink is a PVA *client*, there is no blocking/sans-io PVA
//! client driver, and standing one up is a measured 47 compile errors over a
//! 23,881-line `client_native` tree (design §3.4, stage 5). The startup banner
//! says so out loud rather than leaving an operator to discover that a link
//! silently never connects — the failure mode is indistinguishable from a slow
//! remote IOC from the record's side.
//!
//! # The server is reachable with the UDP responder down, deliberately
//!
//! A PVA client reaches a server it can name directly: `EPICS_PVA_NAME_SERVERS`
//! makes it open a TCP circuit and send SEARCH over that circuit, with no
//! broadcast and no UDP of any kind. That is the path a qemu SLIRP guest is
//! reachable on — SLIRP forwards TCP, and broadcast does not cross it — so a
//! UDP bind failure here is reported and stepped over rather than treated as a
//! failed startup. The IOC still serves; it is only undiscoverable by
//! broadcast.
//!
//! Two consequences worth stating, because both are silent otherwise:
//!
//! * The search port is bound with `SO_REUSEPORT` (see [`bind_udp_search`](epics_pva_rs::server_native::blocking::bind_udp_search)),
//!   so a *second* IOC on this port does not fail — it silently joins the
//!   reuse group and SEARCHes are load-balanced away. A successful UDP bind is
//!   therefore not proof of exclusive ownership; `pvxlist` is.
//! * The GUID is not a configuration field the caller fills in.
//!   [`BlockingPvaServer::bind`](epics_pva_rs::server_native::blocking::BlockingPvaServer::bind) stamps it from
//!   [`random_guid`](epics_pva_rs::server_native::search_engine::random_guid)
//!   at construction, so there is no window in which this binary could
//!   advertise the all-zero GUID a freshly-`Default`ed [`PvaServerConfig`](epics_pva_rs::server_native::config::PvaServerConfig)
//!   carries. It is printed at startup because a colliding GUID degrades
//!   *silently* on every consumer, and the console is the only place this
//!   target can say what it chose.
//!
//! # Configuration
//!
//! Ports come from the standard EPICS environment via
//! [`PvaServerConfig::with_env`](epics_pva_rs::server_native::config::PvaServerConfig::with_env) (`EPICS_PVAS_SERVER_PORT` >
//! `EPICS_PVA_SERVER_PORT` > 5075 for TCP; `EPICS_PVAS_BROADCAST_PORT` >
//! `EPICS_PVA_BROADCAST_PORT` > 5076 for the search port).
//!
//! Command-line arguments are this target's st.cmd, because it has no iocsh:
//! an argument ending in `.json` is a `dbLoadGroup` group-definition file,
//! anything else is a `.db` record file. Both are applied in order (the
//! `dbLoadRecords` / `dbLoadGroup` equivalents). With no arguments a small
//! built-in database is loaded, so the binary is runnable standalone on a bare
//! target. Records carrying `info(Q:group, ...)` need no `.json` at all — that
//! source is read straight off the database.
//!
//! QSRV2 answers to `PVXS_QSRV_ENABLE` / `EPICS_IOC_IGNORE_SERVERS=qsrv2` here
//! exactly as it does in a C IOC (pvxs `enable2()`, `ioc/iochooks.cpp:401-448`);
//! the decision is printed at startup.
//!
//! There is no shutdown command: like a C IOC on RTEMS this runs until the
//! board is reset. The interactive iocsh is host-only, so it is not wired here.
//!
//! # Build configurations
//!
//! The real entry point is compiled when the `runtime::task` seam is on its
//! **executor** backend. Under the hosted default the file is still built, so
//! it stays compiled and linted in the default test set, but refuses to run
//! rather than silently starting the runtime it exists to avoid.
//!
//! The predicate is `target_os = "rtems"` **alone**, which is where it differs
//! from `rtems-ca-ioc`'s `any(target_os = "rtems", feature =
//! "rtems-exec-model")`. `epics-bridge-rs` does not declare
//! `rtems-exec-model`: declaring it is design stage 4, and it arrives with a
//! ~250-site `rtems-exec-gate` census bill (§6.3) that must be paid, not
//! dodged by adding the feature name without the accounting. Naming a feature
//! this crate does not have would be a dangling predicate — three
//! `unexpected_cfg` warnings, and an arm no configuration can select.
//!
//! The cost, stated so stage 4 picks it up: the body below is **not** compiled
//! on a host today, so its only compile coverage is `scripts/rtems-check.sh`
//! (both configurations), and the `mod ioc` unit tests below it do not run on
//! a host. Stage 4 restores both by declaring the feature and widening this
//! predicate back. The four source-text guards at the bottom of the file are
//! outside `mod ioc` and are unaffected — they run in every host test pass.

use std::process::ExitCode;

/// The built-in database, kept **outside** `mod ioc` deliberately.
///
/// It is data, not RTEMS code, and it is the one part of this binary a host
/// can check for real: `mod ioc` does not compile on a host until stage 4, so
/// a typo inside the `info(Q:group, …)` bodies below would otherwise reach a
/// reader as a silent "no such PV" on a serial console with no shell to ask.
/// The `test` arm of the predicate is what lets the guard at the bottom of
/// this file parse it and build the group for real; the `rtems` arm is the
/// production use. On a host non-test build it is compiled away, so it is
/// never dead code.
#[cfg(any(target_os = "rtems", test))]
mod demo_db {
    /// The database loaded when no `.db` file is given on the command line —
    /// small enough to run on a bare target, wide enough to exercise the
    /// scalar GET, PUT and MONITOR paths over pvAccess, and the QSRV2 group
    /// GET/PUT paths on top of the same three records.
    ///
    /// The group is declared with `info(Q:group, …)` rather than a `.json`
    /// file because a `-kernel` boot has no populated filesystem: there is no
    /// path a `dbLoadGroup` argument could name, so the record-info route
    /// (pvxs `loadConfigFromDb`, step 1 of `load_qsrv_groups`) is the only
    /// group source a bare target has. Each record contributes its own
    /// fragment naming the same group — the pvxs merge pattern, one field per
    /// group field name — and every `+channel` is record-relative, because
    /// info-group channels are prefixed with `"{record}."` unconditionally
    /// (`parse_info_group`, groupconfigprocessor.cpp:810-818).
    ///
    /// `+putorder` on all three members is what makes the group putable; its
    /// value is the order an atomic PUT drives the backing records in.
    pub const DEMO_DB: &str = concat!(
        "record(ao, \"RTEMS:PVA:AO\") { field(VAL, \"1.5\") field(PREC, \"3\") field(EGU, \"V\")\n",
        "  info(Q:group, {\"RTEMS:PVA:GRP\":{\"+id\":\"rtems:demo/Group:1.0\",\"+atomic\":true,",
        "\"setpoint\":{\"+channel\":\"VAL\",\"+type\":\"scalar\",\"+putorder\":0}}})\n",
        "}\n",
        "record(longout, \"RTEMS:PVA:LO\") { field(VAL, \"7\") field(EGU, \"counts\")\n",
        "  info(Q:group, {\"RTEMS:PVA:GRP\":",
        "{\"count\":{\"+channel\":\"VAL\",\"+type\":\"plain\",\"+putorder\":1}}})\n",
        "}\n",
        "record(stringout, \"RTEMS:PVA:MSG\") { field(VAL, \"rtems-pva-ioc\")\n",
        "  info(Q:group, {\"RTEMS:PVA:GRP\":",
        "{\"message\":{\"+channel\":\"VAL\",\"+type\":\"plain\",\"+putorder\":2}}})\n",
        "}\n",
    );
}

#[cfg(target_os = "rtems")]
mod ioc {
    use std::collections::HashMap;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::thread;
    use std::time::Instant;

    use epics_base_rs::error::CaResult;
    use epics_base_rs::runtime::task::{StackSizeClass, background_init, block_on_sync};
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::server::ioc_app::GroupLoadRequest;
    use epics_base_rs::server::ioc_builder::IocBuilder;
    use epics_base_rs::server::status_pv::{StatusPv, serve_status_pvs, target_status_pvs};
    use epics_base_rs::types::EpicsValue;
    use epics_bridge_rs::qsrv::{QsrvMount, build_qsrv_mount};
    use epics_pva_rs::server::PvDatabaseSource;
    use epics_pva_rs::server_native::blocking::{BlockingPvaServer, bind_udp_search};
    use epics_pva_rs::server_native::composite::CompositeSource;
    use epics_pva_rs::server_native::config::PvaServerConfig;

    use crate::demo_db::DEMO_DB;

    /// The namespace the status PVs are published under.
    ///
    /// This plays `$(IOCNAME)`'s role in devIocStats' templates — the whole
    /// name is `<prefix>:<upstream leaf>`, one colon, upstream's spelling on
    /// the right. Deliberately the same value `rtems-ca-ioc` uses: the two
    /// binaries are two front-ends for the same board, never both running, so
    /// an operator's screens should not have to know which one booted.
    const STATUS_PREFIX: &str = "RTEMS";

    /// Split the command line into record files and group-definition files.
    ///
    /// This target has no iocsh, so argv *is* st.cmd and this is the whole
    /// command language: `.json` means `dbLoadGroup`, anything else means
    /// `dbLoadRecords`. Split by suffix rather than by a flag because the C
    /// commands are distinguished by which file you hand them too, and a flag
    /// would be a spelling this IOC invented.
    ///
    /// A function so it is testable: getting it wrong routes a group file into
    /// the record parser, and the resulting error names a `.json` file as bad
    /// `.db` syntax — a diagnostic that sends the reader to the wrong file.
    fn split_load_args(args: &[String]) -> (Vec<String>, Vec<GroupLoadRequest>) {
        let mut db_files = Vec::new();
        let mut group_files = Vec::new();
        for arg in args {
            if arg.ends_with(".json") {
                group_files.push(GroupLoadRequest {
                    filename: arg.clone(),
                    // No macro syntax on this command line. `dbLoadGroup`'s
                    // second argument is macro text, and the target has no
                    // shell to supply it; an empty string is what the host
                    // command records when it is omitted.
                    macros: String::new(),
                });
            } else {
                db_files.push(arg.clone());
            }
        }
        (db_files, group_files)
    }

    /// Load the database: every record-file argument is a `.db` file path
    /// (loaded in order, C `dbLoadRecords`), or the built-in demo database
    /// when there are none.
    fn load_database(db_files: &[String]) -> CaResult<Arc<PvDatabase>> {
        let macros = HashMap::new();
        let mut builder = IocBuilder::new();
        if db_files.is_empty() {
            builder = builder.db_string(DEMO_DB, &macros)?;
        } else {
            for path in db_files {
                builder = builder.db_file(path, &macros)?;
            }
        }
        let (db, _autosave) = block_on_sync(builder.build())
            .expect("the RTEMS entry point runs on a plain thread with no runtime entered")?;
        Ok(db)
    }

    /// `01020304…` — the GUID as the wire carries it, so a console line can be
    /// compared against what `pvxlist`/`pvxinfo` report from the other side.
    fn guid_hex(guid: [u8; 12]) -> String {
        guid.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// How the console reports the name-search responder, from the socket's
    /// *own* address rather than from the port that was requested.
    ///
    /// A function so it is testable: on a target with no shell this line is the
    /// only statement of where the IOC can be reached, and an earlier version
    /// echoed the configured port — which prints `UDP search on 0` for an
    /// ephemeral request and, worse, would keep claiming a port the bind had
    /// silently joined someone else's reuse group on.
    fn search_status(bound: Option<std::io::Result<SocketAddr>>) -> String {
        match bound {
            Some(Ok(addr)) => format!("UDP search on {}", addr.port()),
            // A bound socket whose port cannot be read back is a known *target*
            // state, not a failure to serve: RTEMS's libc omits the BSD
            // `sockaddr` length byte, so `bind` succeeds and `local_addr`
            // returns InvalidInput. Say that rather than print an unverified port.
            Some(Err(e)) => format!("UDP search bound, port unreadable ({e})"),
            None => "no UDP search — reach it by EPICS_PVA_NAME_SERVERS".to_string(),
        }
    }

    pub fn main() -> ExitCode {
        // (0) Pull the RTEMS boot shim into the link. Measured: rustc forwards
        //     a dependency's `rustc-link-lib` entries only when the binary
        //     actually references that dependency, so without this call the
        //     shim archive, `-lbsd -lm -lz` and `POSIX_Init` itself are all
        //     absent from the image. Compiles to nothing on a host build.
        epics_rtems_boot::link_anchor();

        // (0b) Make the IOC audible. Every diagnostic below is a `tracing`
        //      event, and an event with no subscriber is discarded, not
        //      buffered — without this line the IOC boots, serves and dies
        //      with an identical, empty console.
        epics_base_rs::runtime::log::install_console_subscriber();

        // (0c) …and make a panic say what it costs. `std`'s default hook
        //      already writes to this console; what is missing from it is the
        //      consequence. A panic on a per-connection thread kills that
        //      thread and leaves the IOC listening and answering searches —
        //      indistinguishable from health from outside, forever. The hook
        //      chains rather than replaces, so the payload, the location and
        //      the backtrace note all survive.
        epics_base_rs::runtime::log::install_panic_hook();

        // (0d) …and say which lock protocol the process got. C prints the
        //      same fact from `epicsMutexShowAll`; we have no iocsh to ask,
        //      so it goes on the console at boot. Before any thread that can
        //      take a record gate exists, so the line cannot be read as a
        //      report about a process that already ran without it.
        epics_base_rs::runtime::sync::report_lock_protocol();

        // (1) C `callbackInit` (callback.c:286). Idempotent.
        background_init();

        // (2) The database.
        let args: Vec<String> = std::env::args().skip(1).collect();
        let (db_files, group_files) = split_load_args(&args);
        let db = match load_database(&db_files) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("rtems-pva-ioc: iocInit failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        let db_for_names = db.clone();
        let db_for_status = db.clone();

        // (2b) The QSRV mount: the enable decision, the provider, and the
        //      group set, finalized. C runs the equivalent at
        //      `initHookAfterInitDatabase` — after the database exists, before
        //      the source is registered (`ioc/iochooks.cpp:343-366`). The
        //      ordering is structural rather than remembered: `build_qsrv_mount`
        //      finalizes the groups before it returns the store that exposes
        //      them, so the `add_source` below cannot run early.
        //
        //      `None` for the ACF: this entry point loads no access-security
        //      file. Passing `None` leaves the provider on `AllowAllAccess`,
        //      which is what an IOC with no ACF means on the CA side too.
        let mount: QsrvMount = match block_on_sync(build_qsrv_mount(&db, None, &group_files)) {
            Ok(m) => m,
            Err(_) => {
                eprintln!(
                    "rtems-pva-ioc: the QSRV mount needs a plain thread with no runtime entered"
                );
                return ExitCode::FAILURE;
            }
        };

        // (3) The PVA front-end. `bind` consumes the config, so the two ports
        //     are read off it first.
        //
        //     `PvaServerConfig::default().with_env()` and NOT a hand-built
        //     struct: `bind` stamps the GUID from `random_guid` at
        //     construction, and a config assembled field-by-field would ship
        //     the all-zero GUID a freshly-`Default`ed value carries — which
        //     degrades silently on every consumer.
        let config = PvaServerConfig::default().with_env();
        let (tcp_port, udp_port, bind_ip) = (config.tcp_port, config.udp_port, config.bind_ip);

        //     Two sources under one server, with pvxs's own names and orders:
        //     `qsrvSingle` at 0 (`ioc/singlesourcehooks.cpp:159`) and
        //     `qsrvGroup` at 1 (`ioc/groupsourcehooks.cpp:219`), "lower order
        //     first". Single records resolve on the database source; a group
        //     PV is not in the database under its own name, so it falls
        //     through to the QSRV store. The status PVs registered below are
        //     ordinary records, so they answer from order 0.
        let composite = CompositeSource::new();
        if let Err(e) =
            composite.add_source("qsrvSingle", Arc::new(PvDatabaseSource::new(db.clone())), 0)
        {
            eprintln!("rtems-pva-ioc: cannot register the single-record source: {e}");
            return ExitCode::FAILURE;
        }
        // Only when QSRV2 is enabled, matching pvxs calling `group_enable()`
        // solely inside `if(enableQ)` (`ioc/iochooks.cpp:492-496`). Disabled,
        // the IOC still serves every single record — it just answers no group.
        if mount.enabled {
            if let Err(e) = composite.add_source("qsrvGroup", mount.store.clone(), 1) {
                eprintln!("rtems-pva-ioc: cannot register the QSRV group source: {e}");
                return ExitCode::FAILURE;
            }
        }

        let server =
            match BlockingPvaServer::bind(SocketAddr::new(bind_ip, tcp_port), composite, config) {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    // Not "cannot bind": `BlockingPvaServer::bind` also calls
                    // `local_addr`, and on RTEMS that is the call that fails
                    // (the libc `sockaddr` length byte). The inner error says which.
                    eprintln!(
                        "rtems-pva-ioc: cannot start the PVA TCP server on port {tcp_port}: {e}"
                    );
                    return ExitCode::FAILURE;
                }
            };
        let bound_tcp = server.tcp_port();

        // (3b) The UDP search responder, bound wildcard rather than to
        //      `bind_ip`: it has to receive broadcast and multicast SEARCHes,
        //      which a socket bound to one interface address does not, and
        //      that is why pvxs binds wildcard too (`udp_collector.cpp:140-151`,
        //      quoted at `bind_udp_search`).
        //
        //      A failure here is NOT fatal — see the module docs. The IOC is
        //      still reachable over TCP alone via `EPICS_PVA_NAME_SERVERS`,
        //      which is exactly how it is reached under qemu SLIRP, so
        //      refusing to start would take away the configuration the bring-up
        //      box uses.
        //
        //      The reported port is read back off the bound socket, never
        //      echoed from the config: with an ephemeral request the two differ,
        //      and a console line that says "UDP search on 0" is worse than none.
        let (udp, search_status) =
            match bind_udp_search(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, udp_port)) {
                Ok(s) => {
                    let status = search_status(Some(s.local_addr()));
                    (Some(s), status)
                }
                Err(e) => {
                    eprintln!(
                        "rtems-pva-ioc: no UDP name-search responder on port {udp_port}: {e}\n\
                         rtems-pva-ioc: the server is still serving on TCP {bound_tcp}; reach it \
                         with EPICS_PVA_NAME_SERVERS=<host>:{bound_tcp}"
                    );
                    (None, search_status(None))
                }
            };

        // (3c) The status PVs. Same facility, same names and the same reason as
        //      `rtems-ca-ioc`: on a target with no iocsh and no shell these are
        //      the only way to ask the IOC how it is doing from anywhere but a
        //      write-only serial console. The descriptor, heap and uptime set
        //      is `target_status_pvs`, so both IOCs answer to one vocabulary.
        //
        //      Registered after `bind` so the connection count reads the server
        //      that is already listening, and before the accept thread starts
        //      so the first client cannot find them missing.
        let conns_server = server.clone();
        let mut status = target_status_pvs(STATUS_PREFIX, Instant::now());
        status.push(
            // No upstream counterpart: devIocStats predates PVA entirely, so
            // there is no `@pva_connections` to match. Named in the shape of
            // devIocStats' `CA_CONN_CNT` rather than a different one, so an
            // operator who knows that name can guess this one.
            StatusPv::new(format!("{STATUS_PREFIX}:PVA_CONN_CNT"), move || {
                EpicsValue::Double(conns_server.active_connections() as f64)
            }),
        );
        if let Err(e) = serve_status_pvs(db_for_status, status) {
            eprintln!("rtems-pva-ioc: cannot register the status PVs: {e}");
            return ExitCode::FAILURE;
        }

        // Listed after the status PVs are registered, so the console names
        // everything a client can reach rather than only what the `.db` carried.
        let mut names = block_on_sync(db_for_names.all_record_names())
            .expect("the RTEMS entry point runs on a plain thread with no runtime entered");
        names.sort();

        // Raw `Builder` rather than `spawn_dedicated_thread`, matching
        // `rtems-ca-ioc`: `serve` and `serve_udp_search` each take their own
        // thread to their own priority through `enter_ioc_thread`, so a
        // priority passed here would be set twice and the second one would win
        // anyway. The stack class is stated, which is what the guard wants.
        let srv_tcp = server.clone();
        let tcp_thread = match thread::Builder::new()
            .name("PVAS-TCP".to_string())
            // Accepts and hands off; the per-connection threads are where the
            // depth is (`blocking.rs` spawns those at Big/Medium).
            .stack_size(StackSizeClass::Medium.bytes())
            .spawn(move || srv_tcp.serve())
        {
            Ok(h) => h,
            Err(e) => {
                eprintln!("rtems-pva-ioc: cannot start the PVA accept thread: {e}");
                return ExitCode::FAILURE;
            }
        };
        let udp_thread = match udp {
            Some(socket) => {
                let srv_udp = server.clone();
                match thread::Builder::new()
                    .name("PVAS-UDP".to_string())
                    .stack_size(StackSizeClass::Medium.bytes())
                    .spawn(move || srv_udp.serve_udp_search(socket))
                {
                    Ok(h) => Some(h),
                    Err(e) => {
                        eprintln!("rtems-pva-ioc: cannot start the PVA name-search thread: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            None => None,
        };

        println!(
            "rtems-pva-ioc: serving {} records on PVA TCP port {bound_tcp} ({search_status}), \
             GUID {}, RTEMS execution model, no tokio runtime",
            names.len(),
            guid_hex(server.guid()),
        );
        println!(
            "rtems-pva-ioc: QSRV2 {} — sources: qsrvSingle(0){}",
            if mount.enabled { "ENABLED" } else { "disabled" },
            if mount.enabled {
                ", qsrvGroup(1)"
            } else {
                " only (set PVXS_QSRV_ENABLE=YES for groups)"
            },
        );
        // Said at every boot, not only when a link is configured: this IOC
        // cannot detect that a `pva://` link exists — the resolver that would
        // see one is the thing that is missing. An operator whose `INP=@pva://`
        // never connects has no other way to learn it is unimplemented rather
        // than slow, so the gap is stated unconditionally. Remove this line in
        // design stage 5, together with the pvalink mount it describes.
        println!(
            "rtems-pva-ioc: NOTE pva:// record links do NOT resolve on this target — pvalink \
             needs a blocking PVA client, which does not exist yet (design stage 5). \
             An INP/OUT of the form @pva://... will never connect. ca:// links are unaffected."
        );
        for name in &names {
            println!("rtems-pva-ioc: {name}");
        }

        // Runs until the board is reset: an IOC has no self-shutdown path on
        // this target. Joining the accept thread is how the main thread waits;
        // the accept loop only returns if `shutdown()` is requested, which
        // nothing here does.
        let _ = tcp_thread.join();
        match udp_thread.map(|h| h.join()) {
            None | Some(Ok(Ok(()))) => ExitCode::SUCCESS,
            Some(Ok(Err(e))) => {
                eprintln!("rtems-pva-ioc: name-search responder failed: {e}");
                ExitCode::FAILURE
            }
            Some(Err(_)) => {
                eprintln!("rtems-pva-ioc: name-search thread panicked");
                ExitCode::FAILURE
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::net::{IpAddr, SocketAddr};

        /// The reported port must be the one the socket says it has. An
        /// ephemeral request is the case that separates "read it back" from
        /// "echo the config": the requested value is 0 and the bound value
        /// never is.
        #[test]
        fn the_reported_search_port_comes_from_the_socket() {
            let bound = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 51314);
            assert_eq!(
                search_status(Some(Ok(bound))),
                "UDP search on 51314",
                "the console must name the bound port, not the requested one"
            );
        }

        /// RTEMS `local_addr` failure: bound and serving, port unverifiable.
        /// It must not be reported as an absent responder — the responder is up.
        #[test]
        fn an_unreadable_port_is_not_reported_as_no_responder() {
            let status = search_status(Some(Err(std::io::Error::from(
                std::io::ErrorKind::InvalidInput,
            ))));
            assert!(
                status.contains("bound") && status.contains("unreadable"),
                "expected a bound-but-unreadable report, got {status:?}"
            );
            assert!(
                !status.contains("no UDP search"),
                "a bound responder must not read as an absent one: {status:?}"
            );
        }

        /// No responder at all: the line must carry the configuration that
        /// still reaches the IOC, because on this target it is the only place
        /// the operator will learn it.
        #[test]
        fn an_absent_responder_names_the_configuration_that_still_works() {
            assert!(
                search_status(None).contains("EPICS_PVA_NAME_SERVERS"),
                "the no-UDP line must name the TCP-only path"
            );
        }

        /// `.json` arguments are group files, everything else is a record
        /// file. Routing a group file into the record parser produces an error
        /// naming a `.json` file as bad `.db` syntax, which sends the reader to
        /// the wrong file on a target with no shell to re-run anything.
        #[test]
        fn json_arguments_load_as_groups_and_the_rest_as_records() {
            let args = vec![
                "/pv/ioc.db".to_string(),
                "/pv/groups.json".to_string(),
                "/pv/more.db".to_string(),
            ];
            let (db_files, group_files) = split_load_args(&args);
            assert_eq!(db_files, vec!["/pv/ioc.db", "/pv/more.db"]);
            assert_eq!(group_files.len(), 1, "the .json argument is the group file");
            assert_eq!(group_files[0].filename, "/pv/groups.json");
            assert!(
                group_files[0].macros.is_empty(),
                "this command line has no macro syntax, matching dbLoadGroup with \
                 its second argument omitted"
            );
        }

        /// No arguments at all must still be a runnable IOC: neither kind of
        /// file, and the built-in demo database downstream.
        #[test]
        fn an_empty_command_line_loads_neither_kind_of_file() {
            let (db_files, group_files) = split_load_args(&[]);
            assert!(db_files.is_empty() && group_files.is_empty());
        }
    }
}

#[cfg(target_os = "rtems")]
fn main() -> ExitCode {
    ioc::main()
}

#[cfg(not(target_os = "rtems"))]
fn main() -> ExitCode {
    eprintln!(
        "rtems-pva-ioc: built with the tokio task backend, which this entry point \
         does not start a runtime for.\n\
         Build it for `armv7-rtems-eabihf`. The host-selectable \
         `rtems-exec-model` build of this binary arrives with design stage 4, \
         which is what declares that feature on `epics-bridge-rs`."
    );
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    /// The RTEMS constraint for the entry point: it must never construct or
    /// enter a tokio runtime, and must never reach for tokio's async
    /// net/timer/spawn machinery — none of which the RTEMS build can drive.
    /// `tokio::sync` (runtime-agnostic locks) IS allowed. Static guard over
    /// this file's own source, in the same shape as `rtems-ca-ioc`'s: the
    /// RTEMS `cargo check` cannot catch this on its own, because tokio's
    /// `rt`/`rt-multi-thread` features are retained on that target, so a
    /// runtime constructor still *compiles* there — only this guard rejects
    /// it. (Comments in this file deliberately avoid the forbidden literals so
    /// they cannot self-match.)
    #[test]
    fn entry_point_never_starts_a_runtime() {
        let src = include_str!("rtems-pva-ioc.rs");
        // Assembled with `concat!` so the forbidden literals never appear
        // contiguously here — otherwise this test body would match itself.
        let forbidden = [
            concat!("tokio", "::main"),
            concat!("tokio", "::net"),
            concat!("tokio", "::time"),
            concat!("tokio", "::", "spawn"),
            concat!("Runtime", "::new"),
            concat!("Builder", "::new_multi_thread"),
            concat!("block", "_in_place"),
            concat!("block", "_on("),
        ];
        for token in forbidden {
            assert!(
                !src.contains(token),
                "the RTEMS PVA entry point must not reference `{token}`: it starts no \
                 tokio runtime and drives every future via park_on"
            );
        }
    }

    /// Every thread this entry point starts states a stack size, and none is
    /// started through the bare `spawn` that carries the platform default —
    /// on the target that default is 2 MiB and two of them per connection is
    /// what the measured ceiling is made of.
    #[test]
    fn every_thread_here_states_a_stack_size() {
        let src = include_str!("rtems-pva-ioc.rs");
        assert!(
            !src.contains(concat!("thread", "::", "spawn(")),
            "use `thread::Builder` with an explicit `stack_size`, not the bare spawn"
        );
        // Both needles are assembled with `concat!` for the same reason the
        // guard above does it: a literal here would be counted as a site.
        let builders = src.matches(concat!("thread", "::Builder::new()")).count();
        let stacks = src.matches(concat!(".stack", "_size(")).count();
        assert!(builders > 0, "the guard must be counting something");
        assert_eq!(
            builders, stacks,
            "every `Builder` in this entry point must state a stack size"
        );
    }

    /// The UDP responder is optional by construction. A regression that made a
    /// UDP bind failure fatal would take away the only configuration a qemu
    /// SLIRP guest is reachable on, and it would do so on the target, where
    /// nobody is reading a backtrace.
    #[test]
    fn a_udp_bind_failure_does_not_stop_the_server() {
        let src = include_str!("rtems-pva-ioc.rs");
        // Delimited by brace balance from the call, not by the line that
        // happens to follow it: an earlier version of this guard split on the
        // exact `let udp = match …` spelling and stopped checking anything the
        // moment that binding was reshaped, while still reporting green.
        let start = src
            .find(concat!("bind_udp_search(", "SocketAddrV4"))
            .expect("the UDP bind call site");
        let tail = &src[start..];
        let open = tail.find('{').expect("the match body opens");
        let mut depth = 0usize;
        let mut close = None;
        for (i, c) in tail[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let arm = &tail[open..=close.expect("the match body closes")];
        assert!(
            arm.contains(concat!("bind_udp", "_search")) || arm.contains("Err(e)"),
            "the scanned region must be the UDP bind match, not an unrelated block"
        );
        assert!(
            !arm.contains("ExitCode::FAILURE"),
            "a failed UDP search bind must be reported and stepped over, not fatal: \
             the server is still reachable over TCP via EPICS_PVA_NAME_SERVERS"
        );
        assert!(
            arm.contains("EPICS_PVA_NAME_SERVERS"),
            "the failure message must name the configuration that still works"
        );
    }

    /// The target has no shell, so an IOC that publishes no status PVs can only
    /// be asked how it is doing by reading a write-only serial console.
    ///
    /// Only `PVA_CONN_CNT` is named here. The descriptor, heap and uptime
    /// values come from `target_status_pvs`, which owns their names and has its
    /// own test pinning them to devIocStats' spelling; restating them here
    /// would be the second copy of the naming rule that function exists to
    /// prevent.
    #[test]
    fn the_entry_point_publishes_its_status() {
        let src = include_str!("rtems-pva-ioc.rs");
        let prod = match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        assert!(
            prod.contains(concat!("serve_status_", "pvs(")),
            "the entry point registers no status PVs; on a target with no iocsh \
             that leaves `pvget` with nothing to ask"
        );
        assert!(
            prod.contains(concat!("target_status_", "pvs(STATUS_PREFIX")),
            "the entry point stopped publishing the common descriptor/heap/uptime \
             set; FD_CNT against FD_MAX is the value that predicts the ceiling"
        );
        assert!(
            prod.contains(":PVA_CONN_CNT"),
            "the PVA connection count is gone — this is the one status value \
             `rtems-ca-ioc` cannot publish, because it starts no PVA server"
        );
    }

    /// A panic on a per-connection thread kills that thread and leaves the IOC
    /// listening, answering searches and serving every other client — from
    /// outside, indistinguishable from health, forever. `std`'s default hook
    /// says a thread panicked; it does not say that. Dropping this call would
    /// restore exactly the silent-degradation shape the console subscriber
    /// above exists to remove.
    #[test]
    fn a_panic_reaches_the_errlog_and_says_what_it_costs() {
        let src = include_str!("rtems-pva-ioc.rs");
        let prod = match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        assert!(
            prod.contains(concat!("install_panic_", "hook()")),
            "this entry point installs no panic hook, so a panicking worker \
             thread leaves an IOC that still looks healthy from the network"
        );
    }

    /// The QSRV group source is mounted, under pvxs's names and orders.
    ///
    /// This is the whole reason the binary moved crates. A regression that
    /// dropped the second `add_source` would leave an IOC that still boots,
    /// still serves every single record, still answers searches and still
    /// passes every other guard here — and silently serves no `Q:group` PV at
    /// all. There is no shell on the target to notice.
    #[test]
    fn the_group_source_is_mounted_at_the_pvxs_order() {
        let src = include_str!("rtems-pva-ioc.rs");
        let prod = match src.find("\n    #[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        assert!(
            prod.contains(concat!("add_", "source(\"qsrvSingle\"")),
            "the single-record source is gone; single-record PVs would stop resolving"
        );
        assert!(
            prod.contains(concat!("add_", "source(\"qsrvGroup\"")),
            "the QSRV group source is not mounted — this binary would boot, serve \
             single records, and answer no Q:group PV, with no shell to say so"
        );
        // pvxs `singlesourcehooks.cpp:159` / `groupsourcehooks.cpp:219`: the
        // orders are the resolution order, and swapping them changes which
        // source answers a name both could claim.
        let single = prod
            .find(concat!("add_", "source(\"qsrvSingle\""))
            .expect("the single source call site");
        let group = prod
            .find(concat!("add_", "source(\"qsrvGroup\""))
            .expect("the group source call site");
        assert!(
            single < group,
            "qsrvSingle must be registered at the lower order, as in pvxs"
        );
        assert!(
            prod.contains(concat!("build_qsrv_", "mount(")),
            "the group set must be built through the shared mount owner, which is \
             what finalizes it before the store exposing it exists"
        );
    }

    /// The startup banner states the pvalink gap.
    ///
    /// A `pva://` link on this target never connects, and the IOC cannot
    /// detect that one was configured — the resolver that would see it is the
    /// missing piece. So the only place an operator can learn this is the
    /// boot console, unconditionally. Design stage 5 removes both the gap and
    /// this guard.
    #[test]
    fn the_banner_states_that_pva_links_do_not_resolve() {
        let src = include_str!("rtems-pva-ioc.rs");
        let prod = match src.find("\n    #[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        assert!(
            prod.contains("pva:// record links do NOT resolve"),
            "the boot banner no longer warns that pva:// links are unimplemented on \
             this target; an operator's INP=@pva://... would silently never connect"
        );
    }

    /// The server config comes from the constructor that fills the GUID.
    ///
    /// `BlockingPvaServer::bind` stamps the GUID from `random_guid`; a config
    /// assembled field-by-field from `PvaServerConfig::default()` without
    /// `with_env` — or a struct literal — ships the all-zero GUID, which
    /// degrades silently on every consumer rather than failing.
    #[test]
    fn the_config_is_built_through_with_env() {
        let src = include_str!("rtems-pva-ioc.rs");
        let prod = match src.find("\n    #[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        assert!(
            prod.contains(concat!("PvaServerConfig::default().with_", "env()")),
            "the PVA server config must come from `PvaServerConfig::default().with_env()`; \
             a hand-built config ships GUID 0"
        );
    }

    /// The built-in database really does define the group it advertises.
    ///
    /// Every other guard here reads source text; this one runs the same two
    /// parsers the target runs — `parse_db`, then `parse_info_group` on each
    /// record's `Q:group` tag, merged the way `load_qsrv_groups` merges them.
    /// A misplaced brace or a `+channel` that names a field the record does
    /// not have costs a full cross-build, image copy and qemu boot to notice,
    /// and the symptom on the console is nothing at all: a group that was
    /// never defined is simply a name no client can find.
    #[test]
    fn the_demo_database_defines_a_putable_group() {
        use epics_bridge_rs::qsrv::group_config::{merge_group_defs, parse_info_group};
        use std::collections::HashMap;

        let recs =
            epics_base_rs::server::db_loader::parse_db(crate::demo_db::DEMO_DB, &HashMap::new())
                .expect("the built-in database must parse");
        assert_eq!(recs.len(), 3, "the demo database is three records");

        let mut groups: HashMap<String, epics_bridge_rs::qsrv::GroupPvDef> = HashMap::new();
        for rec in &recs {
            let json = rec
                .info_tags
                .iter()
                .find(|(k, _)| k == "Q:group")
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("record {} carries no Q:group tag", rec.name));
            let defs = parse_info_group(&rec.name, &json)
                .unwrap_or_else(|e| panic!("record {}: {e}", rec.name));
            merge_group_defs(&mut groups, defs);
        }

        assert_eq!(groups.len(), 1, "all three fragments name one group");
        let grp = groups.get("RTEMS:PVA:GRP").expect("the demo group");
        assert_eq!(grp.struct_id.as_deref(), Some("rtems:demo/Group:1.0"));
        assert!(grp.atomic, "the group is declared +atomic");
        assert!(grp.atomic_is_set, "and declares it explicitly");

        // Members in `put_order`, which is the order an atomic PUT drives the
        // backing records in, with the record-relative channels resolved.
        let members: Vec<(&str, &str, Option<i64>)> = grp
            .members
            .iter()
            .map(|m| (m.field_name.as_str(), m.channel.as_str(), m.put_order))
            .collect();
        assert_eq!(
            members,
            vec![
                ("setpoint", "RTEMS:PVA:AO.VAL", Some(0)),
                ("count", "RTEMS:PVA:LO.VAL", Some(1)),
                ("message", "RTEMS:PVA:MSG.VAL", Some(2)),
            ],
            "every member must be putable and point at a record the demo database defines"
        );

        // Each channel must name a record/field the same database declares —
        // an unresolvable channel is accepted by the parser and only fails at
        // group creation, on the target.
        for m in &grp.members {
            let (rec_name, _field) = m.channel.split_once('.').expect("record.FIELD");
            assert!(
                recs.iter().any(|r| r.name == rec_name),
                "group member {} names record {rec_name}, which the demo database does not define",
                m.field_name
            );
        }
    }
}
