//! `realtime-pva-ioc` — the RTEMS pvAccess IOC entry point.
//!
//! The PVA counterpart of `epics-ca-rs`'s `realtime-ca-ioc`, and the second
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
//! # `pva://` record links resolve on this target (design stage 4)
//!
//! A C IOC linked against pvxs gets pvalink through `pvalink_enable()`
//! (`ioc/iochooks.cpp:495`), so `INP=pva://...` resolves. This one does too:
//! [`install_pvalink_resolver`](epics_bridge_rs::pvalink::install_pvalink_resolver)
//! mounts the pva:// external record-link resolver on the database as a fourth
//! step at init, and the startup banner reports it installed and the link count
//! it pre-registered.
//!
//! The client that backs it is the blocking PVA client (design stages 1-3): on
//! the target it dials over TCP and reaches upstream servers through
//! `EPICS_PVA_NAME_SERVERS` alone, because the UDP SEARCH transport is compiled
//! out (`SearchTransport::NameServersOnly`, design §4.2) — there is no
//! `recvmsg`/`IP_PKTINFO` receive path and no `local_addr()` readback to stamp a
//! response port. A `pva://` link to a server reachable only by UDP broadcast
//! will therefore not resolve; one named by a TCP name server will.
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
//! exactly as it does in a C IOC (pvxs `enable2()`, `ioc/iochooks.cpp:401-448`),
//! and it covers the same three facilities C gates in one `if(enableQ)`
//! (`ioc/iochooks.cpp:485-496`): with QSRV2 off this IOC serves no single
//! record, no `Q:group` PV and resolves no `pva://` link — the server comes up
//! and answers nothing. The decision is printed at startup.
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
//! The predicate is `any(target_os = "rtems", target_os = "vxworks", feature
//! = "rtems-exec-model")`, the same one `realtime-ca-ioc` carries. It was
//! narrowed to `target_os = "rtems"` alone for one stage — `epics-bridge-rs`
//! did not yet declare `rtems-exec-model`, and naming a feature the crate
//! does not have is a dangling predicate: `unexpected_cfg` warnings and an
//! arm no configuration can select. Stage 4 declared the feature *with* the
//! `rtems-exec-gate` census it owes (§6.3) rather than dodging the bill by
//! adding the name alone, so the `any(...)` form is back and the body below
//! is host-compiled, host-linted and host-tested under `--features
//! rtems-exec-model`. `target_os = "vxworks"` is bin-side plumbing only — the
//! `exec_backend` cfg `epics-base-rs/build.rs` derives for the
//! `runtime::task` seam still gates on `target_os = "rtems"` or the feature,
//! so a VxWorks build also needs one of those until its own `exec_backend`
//! predicate lands.
//!
//! Coverage today, in full: `scripts/rtems-check.sh` compiles it for the
//! target in both configurations; the host feature-ON selection compiles it
//! and runs the `mod ioc` unit tests; the source-text guards at the bottom of
//! the file are outside `mod ioc` and run in every host test pass, featured or
//! not.

use std::process::ExitCode;

/// The built-in database, kept **outside** `mod ioc` deliberately.
///
/// It is data, not RTEMS code, and it is the part of this binary the *default*
/// host selection can check for real — with no feature flag, so a typo inside
/// the `info(Q:group, …)` bodies below cannot reach a reader as a silent "no
/// such PV" on a serial console with no shell to ask, even in a test pass that
/// never selects `rtems-exec-model`.
/// The `test` arm of the predicate is what lets the guard at the bottom of
/// this file parse it and build the group for real; the `rtems` and
/// `rtems-exec-model` arms are the production use. On a host non-test build
/// with the feature off it is compiled away, so it is never dead code.
#[cfg(any(
    target_os = "rtems",
    target_os = "vxworks",
    feature = "rtems-exec-model",
    test
))]
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
        "record(stringout, \"RTEMS:PVA:MSG\") { field(VAL, \"realtime-pva-ioc\")\n",
        "  info(Q:group, {\"RTEMS:PVA:GRP\":",
        "{\"message\":{\"+channel\":\"VAL\",\"+type\":\"plain\",\"+putorder\":2}}})\n",
        "}\n",
    );

    /// STAGE-5 PROBE (doc/pvalink-rtems-design.md §5 stage 5, topology A),
    /// loaded on top of [`DEMO_DB`] only under the `bringup-probes` feature
    /// (`doc/calink-rtems-design.md` §11.7 item 3's family: measurement rig,
    /// not IOC content — and the records below are the *only* link-bearing
    /// records in the built-in set, so a default build is also link-free).
    ///
    /// The records the stage-5 gate is about: two INP links with `CP` (the
    /// monitor path) and one OUT link (the put path), all naming PVs served
    /// by the host-side upstream IOC. They are compiled in rather than in a
    /// `.db` file because a `-kernel` boot has no filesystem to name one on
    /// and `rtems_init.c:195` hands `main` a fixed one-element argv.
    #[cfg(feature = "bringup-probes")]
    pub const STAGE5_PROBE_DB: &str = concat!(
        //
        // NOT `@pva://…`, which §5 stage 5 spells: a leading `@` is INST_IO,
        // and `dbCanSetLink` (`record/link.rs:487`, C `dbStaticLib.c:2400`)
        // rejects INST_IO on a record whose bound device support declares
        // CONSTANT — a soft `ai`. Measured on the target: iocInit fails with
        // *"ai.INP: can't initialize link type CONSTANT with
        // \"@pva://UPSTREAM:AI CP\" (type INST_IO)"* and the image exits. C
        // refuses the same `.db`. The two spellings that do load are both
        // exercised here: pvxs's documented JSON longhand
        // (`documentation/pvalink.rst:124-135`) and this tree's `pva://`
        // scheme+suffix form, which parses to `ParsedLink::Pva`.
        "record(ai, \"RTEMS:PVA:DOWN\") {\n",
        "  field(INP, \"{pva: { pv: 'UPSTREAM:AI', proc: 'CP' }}\")\n",
        "  field(PREC, \"3\") field(EGU, \"V\") field(SCAN, \"Passive\")\n",
        "}\n",
        "record(ai, \"RTEMS:PVA:DOWN2\") {\n",
        "  field(INP, \"pva://UPSTREAM:AI CP\")\n",
        "  field(PREC, \"3\") field(EGU, \"V\") field(SCAN, \"Passive\")\n",
        "}\n",
        "record(ao, \"RTEMS:PVA:UPLNK\") {\n",
        "  field(OUT, \"{pva: { pv: 'UPSTREAM:AO' }}\")\n",
        "  field(PREC, \"3\") field(EGU, \"V\") field(OMSL, \"supervisory\")\n",
        "  field(SCAN, \"Passive\")\n",
        "}\n",
    );

    /// QSRV LOAD PROBE (`doc/qsrv-rtems-design.md` §8 items 2 and 5): the
    /// 20-member group whose monitor spawns the ~40 forwarder tasks the
    /// spawn-count asymmetry is about (`group.rs:2496`, `:2532`; C runs ONE
    /// `qsrvGroup` pump thread, `ioc/groupsource.cpp:96`).
    ///
    /// Every member drives itself: `SCAN ".1 second"` + `CALC "VAL+1"`, the
    /// standard counter idiom (`VAL` is `FETCH_VAL`, calcPerform.c:73-74).
    /// Periodic scan is record *processing*, so monitors post on the same
    /// path a production IOC uses — deliberately NOT a probe thread calling
    /// `put_pv`, which posts no monitor event at all (the §11.7-item-2
    /// defect `doc/calink-rtems-design.md` records; the C6 tick had to be
    /// read by polling because of it). No load-driver code exists on the
    /// guest: the members always self-count, and the *load* is switched
    /// from the host side by opening and closing a MONITOR on the group —
    /// forwarders only work when a subscription exists, so
    /// baseline / load / recovery is a host-side choice, C4-measurement
    /// style.
    ///
    /// `RTEMS:PVA:V0` is the victim: the same self-driven shape, in no
    /// group, monitored across all three phases — its wire cadence is the
    /// jitter readout, since the guest clock is 1-second-quantized.
    #[cfg(feature = "bringup-probes")]
    pub fn qsrv_load_probe_db() -> String {
        use std::fmt::Write as _;
        let mut db = String::with_capacity(4096);
        db.push_str(
            "record(calc, \"RTEMS:PVA:V0\") { field(SCAN, \".1 second\") \
             field(CALC, \"VAL+1\") field(PREC, \"0\") }\n",
        );
        for n in 0..20 {
            // `+id`/`+atomic` ride on the first fragment only; the pvxs
            // merge pattern forbids restating them per member.
            let head = if n == 0 {
                "\"+id\":\"rtems:demo/Big:1.0\",\"+atomic\":true,"
            } else {
                ""
            };
            let _ = write!(
                db,
                "record(calc, \"RTEMS:PVA:B{n:02}\") {{ field(SCAN, \".1 second\") \
                 field(CALC, \"VAL+1\") field(PREC, \"0\")\n  \
                 info(Q:group, {{\"RTEMS:PVA:BIG\":{{{head}\"f{n:02}\":\
                 {{\"+channel\":\"VAL\",\"+type\":\"plain\",\"+putorder\":{n}}}}}}})\n}}\n"
            );
        }
        db
    }
}

#[cfg(any(
    target_os = "rtems",
    target_os = "vxworks",
    feature = "rtems-exec-model"
))]
mod ioc {
    use std::collections::HashMap;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::thread;
    use std::time::Instant;

    use epics_base_rs::error::CaResult;
    use epics_base_rs::runtime::task::{StackSizeClass, background_init, block_on_sync};
    use epics_base_rs::runtime::worker_pool::ThreadCharge;
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::server::ioc_app::GroupLoadRequest;
    use epics_base_rs::server::ioc_builder::IocBuilder;
    use epics_base_rs::server::status_pv::{StatusPv, serve_status_pvs, target_status_pvs};
    use epics_base_rs::types::EpicsValue;
    use epics_bridge_rs::qsrv::{QsrvMount, build_qsrv_mount};
    use epics_pva_rs::server_native::blocking::{BlockingPvaServer, bind_udp_search};
    use epics_pva_rs::server_native::composite::CompositeSource;
    use epics_pva_rs::server_native::config::PvaServerConfig;

    use crate::demo_db::DEMO_DB;

    /// The namespace the status PVs are published under.
    ///
    /// This plays `$(IOCNAME)`'s role in devIocStats' templates — the whole
    /// name is `<prefix>:<upstream leaf>`, one colon, upstream's spelling on
    /// the right. Deliberately the same value `realtime-ca-ioc` uses: the two
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
            // The STAGE-5 and QSRV LOAD probe records ride along whenever the
            // built-in database is the source — a bare `-kernel` boot cannot
            // choose — but only on a build that asked for the measurement rig.
            #[cfg(feature = "bringup-probes")]
            {
                builder = builder.db_string(crate::demo_db::STAGE5_PROBE_DB, &macros)?;
                builder = builder.db_string(&crate::demo_db::qsrv_load_probe_db(), &macros)?;
            }
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

    /// STAGE-5 PROBE: the upstream name server the guest's `pva://` links
    /// resolve through. SLIRP puts the host at `10.0.2.2`; the port is the
    /// host-side upstream IOC's TCP port (the guest's own 5075 is taken by
    /// the inbound `hostfwd`, so the upstream cannot also use it).
    #[cfg(feature = "bringup-probes")]
    const STAGE5_NAME_SERVER: &str = "10.0.2.2:15076";

    /// STAGE-5 PROBE: `rt top` + `rt stackuse`, from inside the image.
    ///
    /// Both calls go through `epics_rtems_boot::stats`, which owns the per-OS
    /// backend — see the same function in `realtime-ca-ioc` for why the
    /// `extern "C"` block and the `#[cfg(target_os = …)]` pair that used to sit
    /// here are gone.
    #[cfg(feature = "bringup-probes")]
    fn stage5_task_and_stack_report(tag: &str) {
        epics_rtems_boot::stats::dump_tasks(tag);
        epics_rtems_boot::stats::stack_report(tag);
    }

    /// STAGE-5 PROBE: prove a PVA SEARCH broadcast round-trips on this target.
    ///
    /// Drives the **real** client search engine with no name servers
    /// configured, so what it reports on is the whole UDP path — bind, the
    /// per-interface broadcast fanout, the server's own UDP responder, and the
    /// SEARCH_RESPONSE decode — rather than just that a datagram left the box.
    /// `pv` is one this IOC serves itself, so the answer comes from this
    /// guest's own PVA server via its subnet broadcast address, which is the
    /// only reachable responder under SLIRP.
    ///
    /// Its own engine and not the pvalink resolver's: a failure here must name
    /// the transport, and a resolver that also has TCP name servers configured
    /// would resolve through those and report success for the wrong reason.
    #[cfg(feature = "bringup-probes")]
    fn udp_search_report(pv: &str) {
        use epics_base_rs::runtime::task::{block_on_sync, timeout};
        use epics_pva_rs::client_native::search_engine::{
            ClientSearchConfig, SearchEngine, SearchReason,
        };
        use std::time::Duration;

        let outcome = block_on_sync(async {
            let config = ClientSearchConfig::from_env();
            let bport = config.broadcast_port;
            // No name servers, no extra targets: auto-address broadcast is the
            // only way this engine can reach anything.
            let engine = SearchEngine::spawn_with_config(
                config,
                Vec::new(),
                Vec::new(),
                String::new(),
                String::new(),
                Duration::from_secs(1),
            )
            .await?;
            println!("UDPSEARCH broadcast_port={bport} pv={pv}");
            let hit = timeout(
                Duration::from_secs(5),
                engine.find(pv, SearchReason::Initial),
            )
            .await
            .map_err(|_| {
                epics_pva_rs::error::PvaError::Protocol("no SEARCH_RESPONSE in 5 s".into())
            })??;
            Ok::<_, epics_pva_rs::error::PvaError>(hit)
        });

        match outcome {
            Ok(Ok(hit)) => println!(
                "UDPSEARCH reply pv={pv} server={} guid={:02x?} proto={} version={}",
                hit.server, hit.guid, hit.proto, hit.peer_version
            ),
            Ok(Err(e)) => eprintln!("UDPSEARCH failed pv={pv}: {e}"),
            Err(e) => eprintln!("UDPSEARCH not runnable here: {e:?}"),
        }
    }

    /// STAGE-5 PROBE: one console report — the link registry, the ONE
    /// client's connection list, and the two link-bearing records.
    ///
    /// This is the guest half of pass criteria 1, 3, 4 and 6: with no shell
    /// on the target the console is the only place the connection count and
    /// the record's alarm state can be read from *inside* the IOC, next to
    /// what `pvxget` reads from outside.
    #[cfg(feature = "bringup-probes")]
    fn stage5_report(
        seq: u32,
        resolver: &epics_bridge_rs::pvalink::PvaLinkResolver,
        db: &Arc<PvDatabase>,
    ) {
        let rep = resolver.client_report();
        println!(
            "STAGE5 seq={seq} links={} channels_total={} active={} searching={} \
             connecting={} name_servers={} connections={}",
            resolver.link_count(),
            rep.channels_total,
            rep.channels_active,
            rep.channels_searching,
            rep.channels_connecting,
            rep.name_servers,
            rep.connections.len(),
        );
        for c in &rep.connections {
            println!(
                "STAGE5 seq={seq} conn peer={} alive={} rx={} tx={} channels={:?}",
                c.peer,
                c.alive,
                c.bytes_rx,
                c.bytes_tx,
                c.channels
                    .iter()
                    .map(|ch| ch.name.as_str())
                    .collect::<Vec<_>>(),
            );
        }
        for d in resolver.channel_diagnostics() {
            println!(
                "STAGE5 seq={seq} link pv={} dir={:?} connected={} records={:?}",
                d.pv_name, d.direction, d.connected, d.records,
            );
        }
        // The §9.11 dial-pool bound, and the heap it was said to stop eating.
        // Both on one line so a console reader can see the attempt count that
        // the worker count is bounded *against* — a worker count of 1 proves
        // nothing next to an attempt count of 1.
        let (dial_workers, dial_attempts) =
            epics_pva_rs::client_native::server_conn::dial_pool_probe();
        // Each field carries its own -1 — see the same site in `realtime-ca-ioc`.
        let mem = epics_rtems_boot::stats::mem_usage();
        let mem_free = mem.free.map_or(-1, |v| v as i64);
        let mem_used = mem.used.map_or(-1, |v| v as i64);
        println!(
            "STAGE5 seq={seq} dialpool workers={dial_workers} attempts={dial_attempts} \
             MEM_FREE={mem_free} MEM_USED={mem_used}",
        );
        for rec in ["RTEMS:PVA:DOWN", "RTEMS:PVA:DOWN2", "RTEMS:PVA:UPLNK"] {
            let val = db.get_pv(rec).map(|v| v.to_string());
            let sevr = db.get_pv(&format!("{rec}.SEVR")).map(|v| v.to_string());
            let stat = db.get_pv(&format!("{rec}.STAT")).map(|v| v.to_string());
            println!("STAGE5 seq={seq} record {rec} VAL={val:?} SEVR={sevr:?} STAT={stat:?}");
        }
    }

    pub fn main() -> ExitCode {
        // (0) Pull the RTEMS boot shim into the link. Measured: rustc forwards
        //     a dependency's `rustc-link-lib` entries only when the binary
        //     actually references that dependency, so without this call the
        //     shim archive, `-lbsd -lm -lz` and `POSIX_Init` itself are all
        //     absent from the image. Compiles to nothing on a host build.
        epics_rtems_boot::link_anchor();

        // (0-reg) Announce this thread to the statistics funnel's thread
        //     census. Every other IOC thread does this from
        //     `runtime::task::enter_ioc_thread`, which `main` deliberately does
        //     not call — it is the one thread that keeps the band the OS
        //     started it with — so it is also the one thread that has to
        //     register itself. Without this the census would be missing the
        //     thread that owns the database.
        //
        //     No `#[cfg]`: the funnel is portable and every backend but
        //     VxWorks' takes this as a no-op.
        epics_rtems_boot::stats::register_task();

        // (0-probe) STAGE-5 PROBE: the target's `EPICS_PVA_NAME_SERVERS`.
        // `rtems_init.c:195` hands `main` a fixed one-element argv and
        // `POSIX_Init` calls `setenv` zero times, so on the target nothing
        // outside the image can configure it — the value compiled in here is
        // the one that takes effect. Set before `install_pvalink_resolver`
        // builds the ONE client, because `PvaClientBuilder`'s default reads
        // the variable once at construction (`client_native/context.rs:171`).
        //
        // A default, not an override: an already-set variable stays as set,
        // so a host harness (`tests/realtime_pva_ioc_boots.rs`) can point the
        // dial somewhere it controls instead of the image's SLIRP address.
        //
        // SAFETY (edition 2024): this runs on the single init thread before
        // `background_init` starts any other thread, so no concurrent
        // reader/writer of the environment exists.
        #[cfg(feature = "bringup-probes")]
        if std::env::var_os("EPICS_PVA_NAME_SERVERS").is_none() {
            unsafe { std::env::set_var("EPICS_PVA_NAME_SERVERS", STAGE5_NAME_SERVER) };
        }

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
                eprintln!("realtime-pva-ioc: iocInit failed: {e}");
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
        //      A fresh cell holding no ACF: this entry point loads no
        //      access-security file, and an empty cell grants like
        //      `AllowAllAccess` — which is what an IOC with no ACF means
        //      on the CA side too.
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let mount: QsrvMount = match block_on_sync(build_qsrv_mount(&db, acf, &group_files)) {
            Ok(m) => m,
            Err(_) => {
                eprintln!(
                    "realtime-pva-ioc: the QSRV mount needs a plain thread with no runtime entered"
                );
                return ExitCode::FAILURE;
            }
        };

        // (2c) Everything the QSRV2 enable decision turns on, through the one
        //      owner that turns all of it on together: the `qsrvSingle`
        //      database source, the `qsrvGroup` QSRV store, and the pva://
        //      external record-link resolver so `INP=pva://...` /
        //      `OUT=pva://...` fields resolve. C runs its three equivalents as
        //      three statements inside one `if(enableQ)` during iocInit
        //      (`ioc/iochooks.cpp:485-496`), after the database exists. Run
        //      here — after the database and the QSRV mount, before the PVA
        //      front-end and before scanning starts — so every record's link
        //      fields are pre-registered before the first client and before the
        //      first scan pass.
        //
        //      On this target the PVA client reaches upstream servers over TCP
        //      name servers alone (`EPICS_PVA_NAME_SERVERS`): the UDP SEARCH
        //      transport is compiled out (design §4.2, `SearchTransport::
        //      NameServersOnly`), and every task the resolver spawns lands on the
        //      callback pool `background_init` started above via
        //      `runtime::task::spawn`, never a tokio runtime. `link_count` is the
        //      number of pva:// links pre-registered from the loaded database.
        let composite = CompositeSource::new();
        let sources = match block_on_sync(mount.install_sources(&db, &composite))
            .expect("the RTEMS entry point runs on a plain thread with no runtime entered")
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("realtime-pva-ioc: cannot register the QSRV2 sources: {e}");
                return ExitCode::FAILURE;
            }
        };
        let resolver = sources.pvalink;
        let link_count = resolver.as_ref().map_or(0, |r| r.link_count());

        // (2b) Periodic scan + PINI. C `iocInit` owns `scanInit` (`dbScan.c`)
        //      independent of any network server; without this step every
        //      periodic `SCAN` field in the database is silently dead on
        //      the PVA-only target (found by the §8 load probe, whose
        //      self-counting members never counted — the scheduler used to
        //      be driven only by the CA server's run loop, a server this
        //      binary does not have). `ScanOwner` is the core-owned form of
        //      the dedicated scan-owner thread this binary first proved on
        //      target: a thread (not `runtime::task::spawn`) because on the
        //      exec backend a task parked on a forever-pending future with
        //      its waker registered nowhere is dropped by the executor,
        //      taking the `ScanStopGuard` — and every `scan-%g` thread —
        //      with it. Redundant starts stay parked non-owners
        //      (`try_claim_scan_start`), so exactly one set of `scan-%g`
        //      threads exists no matter how many entry points start one.
        //      Held to the end of `main`: this IOC scans for as long as it
        //      serves.
        let _scan_owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());

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

        //     The sources went onto `composite` at step (2c), under pvxs's
        //     own names and orders: single records resolve on the database
        //     source at 0, and a group PV — not in the database under its own
        //     name — falls through to the QSRV store at 1. The status PVs
        //     registered below are ordinary records, so they answer from
        //     order 0, and with QSRV2 disabled they do not answer at all.

        let server =
            match BlockingPvaServer::bind(SocketAddr::new(bind_ip, tcp_port), composite, config) {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    // Not "cannot bind": `BlockingPvaServer::bind` also calls
                    // `local_addr`, and on RTEMS that is the call that fails
                    // (the libc `sockaddr` length byte). The inner error says which.
                    eprintln!(
                        "realtime-pva-ioc: cannot start the PVA TCP server on port {tcp_port}: {e}"
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
        let (udp, search_status) = match bind_udp_search(SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            udp_port,
        )) {
            Ok(s) => {
                let status = search_status(Some(s.local_addr()));
                (Some(s), status)
            }
            Err(e) => {
                eprintln!(
                    "realtime-pva-ioc: no UDP name-search responder on port {udp_port}: {e}\n\
                         realtime-pva-ioc: the server is still serving on TCP {bound_tcp}; reach it \
                         with EPICS_PVA_NAME_SERVERS=<host>:{bound_tcp}"
                );
                (None, search_status(None))
            }
        };

        // (3c) The status PVs. Same facility, same names and the same reason as
        //      `realtime-ca-ioc`: on a target with no iocsh and no shell these are
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
            eprintln!("realtime-pva-ioc: cannot register the status PVs: {e}");
            return ExitCode::FAILURE;
        }

        // Listed after the status PVs are registered, so the console names
        // everything a client can reach rather than only what the `.db` carried.
        let mut names = block_on_sync(db_for_names.all_record_names())
            .expect("the RTEMS entry point runs on a plain thread with no runtime entered");
        names.sort();

        // Raw `Builder` rather than `spawn_dedicated_thread`, matching
        // `realtime-ca-ioc`: `serve` and `serve_udp_search` each take their own
        // thread to their own priority through `enter_ioc_thread`, so a
        // priority passed here would be set twice and the second one would win
        // anyway. The stack class is stated, which is what one guard wants, and
        // so is the charge, which is what the other wants: a thread the process
        // account never hears about is memory the worker pool spends twice.
        let srv_tcp = server.clone();
        let charge = ThreadCharge::fixed(StackSizeClass::Medium);
        let tcp_thread = match thread::Builder::new()
            .name("PVAS-TCP".to_string())
            // Accepts and hands off; the per-connection threads are where the
            // depth is (`blocking.rs` spawns those at Big/Medium).
            .stack_size(StackSizeClass::Medium.bytes())
            .spawn(move || {
                let _charge = charge;
                srv_tcp.serve()
            }) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("realtime-pva-ioc: cannot start the PVA accept thread: {e}");
                return ExitCode::FAILURE;
            }
        };
        let udp_thread = match udp {
            Some(socket) => {
                let srv_udp = server.clone();
                let charge = ThreadCharge::fixed(StackSizeClass::Medium);
                match thread::Builder::new()
                    .name("PVAS-UDP".to_string())
                    .stack_size(StackSizeClass::Medium.bytes())
                    .spawn(move || {
                        let _charge = charge;
                        srv_udp.serve_udp_search(socket)
                    }) {
                    Ok(h) => Some(h),
                    Err(e) => {
                        eprintln!("realtime-pva-ioc: cannot start the PVA name-search thread: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            None => None,
        };

        println!(
            "realtime-pva-ioc: serving {} records on PVA TCP port {bound_tcp} ({search_status}), \
             GUID {}, RTEMS execution model, no tokio runtime",
            names.len(),
            guid_hex(server.guid()),
        );
        println!(
            "realtime-pva-ioc: QSRV2 {}",
            if mount.enabled {
                "ENABLED — sources: qsrvSingle(0), qsrvGroup(1)"
            } else {
                "disabled — nothing mounted: this IOC answers no record and no \
                 group and resolves no pva:// link (set PVXS_QSRV_ENABLE=YES)"
            },
        );
        // The pvalink resolver, when the QSRV2 gate mounted one. Reported at
        // every boot — including `link_count == 0`, when the loaded database
        // configured none — because on a target with no shell the console is
        // the only place an operator can confirm the resolver came up and how
        // many links it pre-registered. The client reaches upstream servers
        // both ways now: UDP SEARCH broadcast to the interfaces' own
        // destinations, and any `EPICS_PVA_NAME_SERVERS` over TCP.
        if resolver.is_some() {
            println!(
                "realtime-pva-ioc: pvalink resolver installed — {link_count} pva:// record \
                 link{} pre-registered; pva://... INP/OUT resolve over UDP SEARCH \
                 broadcast and EPICS_PVA_NAME_SERVERS (TCP)",
                if link_count == 1 { "" } else { "s" },
            );
        }
        for name in &names {
            println!("realtime-pva-ioc: {name}");
        }

        // STAGE-5 PROBE: the console reporter. One line group every 10 s, and
        // the task census + stack-usage report every 6th pass (~1 min), which
        // is the `rt top` / `rt stackuse` reading criterion 6 asks for on a
        // target that starts no shell. Its own thread with a stated stack
        // class, like every other thread this entry point starts.
        // The report reads the pvalink resolver, which exists only when
        // QSRV2 is enabled; with it off there is nothing here to report.
        #[cfg(feature = "bringup-probes")]
        if let Some(probe_resolver) = resolver.clone() {
            let probe_db = db.clone();
            println!(
                "STAGE5 probe: EPICS_PVA_NAME_SERVERS={} (compiled in), reporting every 10 s",
                STAGE5_NAME_SERVER,
            );
            let charge = ThreadCharge::fixed(StackSizeClass::Medium);
            match thread::Builder::new()
                .name("stage5-probe".to_string())
                .stack_size(StackSizeClass::Medium.bytes())
                .spawn(move || {
                    let _charge = charge;
                    // The band is the role's, and for this role the table
                    // says below client service: a census this long, above
                    // the serving threads, rewrites the measurement it is
                    // here to take — see `runtime::ioc_role`.
                    let _ = epics_base_rs::runtime::ioc_role::enter_ioc_role(
                        epics_base_rs::runtime::ioc_role::IocRole::ConsoleCensus,
                    );
                    // Once, before the periodic census: the transport either
                    // round-trips or it does not, and repeating it every 10 s
                    // would bury the answer in the report stream.
                    udp_search_report("RTEMS:PVA:AO");
                    let mut seq = 0u32;
                    loop {
                        thread::sleep(std::time::Duration::from_secs(10));
                        seq += 1;
                        stage5_report(seq, &probe_resolver, &probe_db);
                        if seq.is_multiple_of(6) {
                            stage5_task_and_stack_report(&format!("s5-{seq}"));
                        }
                    }
                }) {
                Ok(_) => {}
                Err(e) => eprintln!("STAGE5 probe: cannot start the reporter thread: {e}"),
            }
        } else {
            println!(
                "STAGE5 probe: not started \u{2014} QSRV2 is disabled, so this IOC has no pvalink resolver"
            );
        }

        // Runs until the board is reset: an IOC has no self-shutdown path on
        // this target. Joining the accept thread is how the main thread waits;
        // the accept loop only returns if `shutdown()` is requested, which
        // nothing here does.
        let _ = tcp_thread.join();
        match udp_thread.map(|h| h.join()) {
            None | Some(Ok(Ok(()))) => ExitCode::SUCCESS,
            Some(Ok(Err(e))) => {
                eprintln!("realtime-pva-ioc: name-search responder failed: {e}");
                ExitCode::FAILURE
            }
            Some(Err(_)) => {
                eprintln!("realtime-pva-ioc: name-search thread panicked");
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

#[cfg(any(
    target_os = "rtems",
    target_os = "vxworks",
    feature = "rtems-exec-model"
))]
fn main() -> ExitCode {
    ioc::main()
}

#[cfg(not(any(
    target_os = "rtems",
    target_os = "vxworks",
    feature = "rtems-exec-model"
)))]
fn main() -> ExitCode {
    eprintln!(
        "realtime-pva-ioc: built with the tokio task backend, which this entry point \
         does not start a runtime for.\n\
         Build it for `armv7-rtems-eabihf` or a VxWorks target, or on a host select \
         `--features rtems-exec-model`, which routes the `runtime::task` seam to \
         the same std-thread executor the target uses."
    );
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    /// The RTEMS constraint for the entry point: it must never construct or
    /// enter a tokio runtime, and must never reach for tokio's async
    /// net/timer/spawn machinery — none of which the RTEMS build can drive.
    /// `tokio::sync` (runtime-agnostic locks) IS allowed. Static guard over
    /// this file's own source, in the same shape as `realtime-ca-ioc`'s: the
    /// RTEMS `cargo check` cannot catch this on its own, because tokio's
    /// `rt`/`rt-multi-thread` features are retained on that target, so a
    /// runtime constructor still *compiles* there — only this guard rejects
    /// it. (Comments in this file deliberately avoid the forbidden literals so
    /// they cannot self-match.)
    #[test]
    fn entry_point_never_starts_a_runtime() {
        let src = include_str!("realtime-pva-ioc.rs");
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
        let src = include_str!("realtime-pva-ioc.rs");
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

    /// Every thread this entry point starts is also charged to the process
    /// account, which is the half the guard above does not cover.
    ///
    /// A stack class says how much this thread takes. The charge is what makes
    /// the worker pool's budget know it was taken — without it the pool admits
    /// clients against a number that has never heard of the acceptor, the UDP
    /// responder or the probe. Measured at roughly 15 MiB of fixed IOC threads
    /// on the VxWorks target: inside the headroom, which is not the same as
    /// accounted for, and the moment a target runs more fixed threads than
    /// this one the error is no longer small and nothing reports that it grew.
    ///
    /// The charge must be held *inside* the spawned body, not by the spawning
    /// function: a guard left on the calling thread is released the moment
    /// `main` walks past the `spawn`, while the thread it paid for runs for the
    /// life of the IOC. That is the shape this guard pins.
    ///
    /// Fails today, on Linux, with no cross toolchain.
    #[test]
    fn every_thread_here_is_charged_to_the_process_account() {
        let src = include_str!("realtime-pva-ioc.rs");
        let prod = match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        // Needle assembled with `concat!` so this guard does not match itself
        // in the file it is written in.
        let mut sites = 0usize;
        let mut unpaid = Vec::new();
        for (n, after) in prod
            .split(concat!("thread", "::Builder::new()"))
            .skip(1)
            .enumerate()
        {
            sites += 1;
            let body = after.split(".spawn(").nth(1).unwrap_or("");
            // The charge is the first statement of the body, so a short window
            // is the whole check: a `_charge` further down would be a `Drop`
            // that runs at a different time than the thread's life.
            let head = &body[..body.len().min(120)];
            if !head.contains("let _charge = charge;") {
                unpaid.push(format!("Builder #{}", n + 1));
            }
        }
        assert_eq!(
            sites, 3,
            "expected the accept thread, the UDP responder and the bring-up \
             probe, found {sites} — a thread was added or moved, and the \
             account has to be told about it"
        );
        assert!(
            unpaid.is_empty(),
            "these threads reserve stack the pool's budget never hears about, \
             so it admits clients against memory that is already gone: \
             {unpaid:?}. Take a `ThreadCharge::fixed` before the `spawn` and \
             move it into the body."
        );
    }

    /// This entry point takes bands by role and names none of its own.
    ///
    /// The bring-up probe named `ThreadPriority::Low` here, the same band the
    /// CA entry point's probe measured as starving under load while the
    /// servers ran at 16..20. The serving threads take their bands from
    /// `epics_pva_rs::server_native::blocking`'s named constants.
    #[test]
    fn this_entry_point_names_no_scheduling_band() {
        let src = include_str!("realtime-pva-ioc.rs");
        let prod = match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        assert!(
            !prod.contains(concat!("Thread", "Priority")),
            "this file must not name a scheduling band — ask \
             `runtime::ioc_role` for one by role"
        );
        assert!(
            prod.contains("IocRole::ConsoleCensus"),
            "the bring-up probe must enter its thread as \
             `IocRole::ConsoleCensus`"
        );
    }

    /// The UDP responder is optional by construction. A regression that made a
    /// UDP bind failure fatal would take away the only configuration a qemu
    /// SLIRP guest is reachable on, and it would do so on the target, where
    /// nobody is reading a backtrace.
    #[test]
    fn a_udp_bind_failure_does_not_stop_the_server() {
        let src = include_str!("realtime-pva-ioc.rs");
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
        let src = include_str!("realtime-pva-ioc.rs");
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
             `realtime-ca-ioc` cannot publish, because it starts no PVA server"
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
        let src = include_str!("realtime-pva-ioc.rs");
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

    /// Every QSRV2 facility is mounted through the one gated owner, and the
    /// banner reports what came up.
    ///
    /// This is the whole reason the binary moved crates. The three facilities
    /// C turns on together in one `if(enableQ)` — the single-record source,
    /// the group source and the pvalink resolver — are mounted here by one
    /// call to `QsrvMount::install_sources`. A regression that opened any of
    /// them back up at this call site would leave an IOC that still boots,
    /// still answers searches and still passes every other guard here, while
    /// serving a facility a C IOC with the same environment does not. There
    /// is no shell on the target to notice, so the call-site shape is
    /// asserted directly: no `add_source` of its own, and the banner lines an
    /// operator reads instead of a shell.
    #[test]
    fn the_qsrv2_facilities_are_mounted_through_the_one_gate() {
        let src = include_str!("realtime-pva-ioc.rs");
        let prod = match src.find("\n    #[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        // `concat!` so these assertions cannot match their own source text.
        assert!(
            prod.contains(concat!("install_", "sources(&db, &composite)")),
            "the QSRV2 sources are no longer mounted through the one owner that \
             gates all three together; single records, groups and pva:// links \
             can drift apart again"
        );
        assert!(
            !prod.contains(concat!("add_", "source(")),
            "this entry point registers a source itself again — that is the split \
             the shared owner exists to close, and the ungated half is invisible \
             until a client asks"
        );
        assert!(
            prod.contains(concat!("build_qsrv_", "mount(")),
            "the group set must be built through the shared mount owner, which is \
             what finalizes it before the store exposing it exists"
        );
        assert!(
            prod.contains("pvalink resolver installed"),
            "the boot banner no longer reports the pvalink resolver; on a target with \
             no shell the console is the only place an operator can confirm it came up"
        );
        assert!(
            prod.contains("link_count"),
            "the banner no longer reports the pre-registered link count, the one number \
             that tells an operator whether the database's pva:// links were seen"
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
        let src = include_str!("realtime-pva-ioc.rs");
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
        // The three group members and nothing else: the stage-5 link-bearing
        // records moved to `STAGE5_PROBE_DB`, behind `bringup-probes`.
        assert_eq!(recs.len(), 3, "the three group members, nothing else");

        let mut groups: HashMap<String, epics_bridge_rs::qsrv::GroupPvDef> = HashMap::new();
        for rec in &recs {
            let Some(json) = rec
                .info_tags
                .iter()
                .find(|(k, _)| k == "Q:group")
                .map(|(_, v)| v.clone())
            else {
                continue;
            };
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

    /// The default binary is IOC content only: no stage-5 link records, no
    /// QSRV load-probe group — and no link fields at all, the same link-free
    /// default `realtime-ca-ioc` keeps (`doc/calink-rtems-design.md` §11.7
    /// items 3 and 4).
    ///
    /// Runs in every host test pass (the probe rig moved to
    /// `STAGE5_PROBE_DB` / `qsrv_load_probe_db`, which only exist under
    /// `bringup-probes`), so a probe record leaking back into `DEMO_DB`
    /// fails the default `--workspace` suite, not just a feature slice.
    #[test]
    fn the_default_database_is_clean_and_link_free() {
        use std::collections::HashMap;

        let recs =
            epics_base_rs::server::db_loader::parse_db(crate::demo_db::DEMO_DB, &HashMap::new())
                .expect("the built-in database must parse");
        let names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["RTEMS:PVA:AO", "RTEMS:PVA:LO", "RTEMS:PVA:MSG"],
            "the default database is the three group members and nothing else"
        );
        for rec in &recs {
            for (field, _) in &rec.fields {
                assert!(
                    !matches!(field.as_str(), "INP" | "OUT" | "FLNK"),
                    "{}.{field} is a link field: the default image must be \
                     link-free — put link-bearing records behind `bringup-probes` \
                     or in a loaded .db",
                    rec.name
                );
            }
        }
    }

    /// The probe ride-along in `load_database` is behind the feature gate.
    ///
    /// The parse tests pin what each constant *contains*; this pins what the
    /// binary *loads*: an edit that loads the probe databases unconditionally
    /// would pass every parse test and ship the rig in the default image.
    #[test]
    fn the_probe_dbs_load_only_behind_the_feature() {
        let src = include_str!("realtime-pva-ioc.rs");
        let prod = match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        for needle in [
            concat!("crate::demo_db::STAGE5_PROBE_", "DB, &macros"),
            concat!("crate::demo_db::qsrv_load_probe_", "db(), &macros"),
        ] {
            let load_at = prod
                .find(needle)
                .expect("the probe database is loaded somewhere in load_database");
            let before = &prod[load_at.saturating_sub(500)..load_at];
            assert!(
                before.contains(concat!("#[cfg(feature = \"bringup-", "probes\")]")),
                "the `{needle}` load site is not feature-gated: the measurement \
                 rig would ship in the default image"
            );
        }
    }

    /// With the feature on, the stage-5 rig is exactly the three link
    /// records the pvalink gate measured with, in both surviving spellings
    /// (`doc/pvalink-rtems-design.md` §12.2).
    #[cfg(feature = "bringup-probes")]
    #[test]
    fn the_stage5_rig_defines_the_three_link_records() {
        use std::collections::HashMap;

        let recs = epics_base_rs::server::db_loader::parse_db(
            crate::demo_db::STAGE5_PROBE_DB,
            &HashMap::new(),
        )
        .expect("the stage-5 probe database must parse");
        let names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["RTEMS:PVA:DOWN", "RTEMS:PVA:DOWN2", "RTEMS:PVA:UPLNK"],
            "the stage-5 rig is the two INP links and the OUT link"
        );
        let field = |name: &str, field: &str| {
            recs.iter()
                .find(|r| r.name == name)
                .and_then(|r| r.fields.iter().find(|(f, _)| f == field))
                .map(|(_, v)| v.to_string())
        };
        assert_eq!(
            field("RTEMS:PVA:DOWN", "INP").as_deref(),
            Some("{pva: { pv: 'UPSTREAM:AI', proc: 'CP' }}"),
            "the pvxs JSON longhand spelling"
        );
        assert_eq!(
            field("RTEMS:PVA:DOWN2", "INP").as_deref(),
            Some("pva://UPSTREAM:AI CP"),
            "this tree's scheme+suffix spelling"
        );
        assert_eq!(
            field("RTEMS:PVA:UPLNK", "OUT").as_deref(),
            Some("{pva: { pv: 'UPSTREAM:AO' }}"),
            "the put-path OUT link"
        );
    }

    /// The QSRV load probe (`doc/qsrv-rtems-design.md` §8 items 2/5) must
    /// survive the same offline reading as the demo group: a typo in a
    /// generated fragment would otherwise surface only as a missing PV on a
    /// serial console with no shell to ask.
    #[cfg(feature = "bringup-probes")]
    #[test]
    fn the_load_probe_defines_the_twenty_member_group() {
        use epics_bridge_rs::qsrv::group_config::{merge_group_defs, parse_info_group};
        use std::collections::HashMap;

        let src = crate::demo_db::qsrv_load_probe_db();
        let recs = epics_base_rs::server::db_loader::parse_db(&src, &HashMap::new())
            .expect("the load-probe database must parse");
        assert_eq!(recs.len(), 21, "the victim plus twenty group members");

        let mut groups: HashMap<String, epics_bridge_rs::qsrv::GroupPvDef> = HashMap::new();
        for rec in &recs {
            let Some(json) = rec
                .info_tags
                .iter()
                .find(|(k, _)| k == "Q:group")
                .map(|(_, v)| v.clone())
            else {
                continue;
            };
            let defs = parse_info_group(&rec.name, &json)
                .unwrap_or_else(|e| panic!("record {}: {e}", rec.name));
            merge_group_defs(&mut groups, defs);
        }

        assert_eq!(groups.len(), 1, "twenty fragments name one group");
        let grp = groups.get("RTEMS:PVA:BIG").expect("the load-probe group");
        assert_eq!(grp.struct_id.as_deref(), Some("rtems:demo/Big:1.0"));
        assert!(grp.atomic, "the group is declared +atomic");
        assert_eq!(grp.members.len(), 20, "every member fragment merged");
        for (n, m) in grp.members.iter().enumerate() {
            assert_eq!(m.field_name, format!("f{n:02}"));
            assert_eq!(m.channel, format!("RTEMS:PVA:B{n:02}.VAL"));
            assert_eq!(m.put_order, Some(n as i64));
        }

        // The victim must stay OUT of the group: it is the control the
        // load phases are read against.
        assert!(
            recs.iter()
                .find(|r| r.name == "RTEMS:PVA:V0")
                .expect("the victim record")
                .info_tags
                .iter()
                .all(|(k, _)| k != "Q:group"),
            "RTEMS:PVA:V0 must carry no Q:group tag"
        );
    }
}
