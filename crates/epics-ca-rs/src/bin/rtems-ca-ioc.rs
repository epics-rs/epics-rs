//! `rtems-ca-ioc` — the RTEMS CA IOC entry point (design doc §9.5).
//!
//! A runnable `main` that brings up a complete CA IOC on the **RTEMS execution
//! model** and nothing else: no tokio runtime is ever created, no async
//! front-end is touched, and every long-lived loop owns a dedicated OS thread.
//! Four facilities, started in the order C `iocInit` starts its equivalents:
//!
//! 1. **Background executor** — `runtime::task::background_init()` (C
//!    `callbackInit`, `callback.c:286`): the callback pool
//!    (`cbLow`/`cbMedium`/`cbHigh`), the delayed timer, and the scanOnce
//!    worker. Started *first* so no record processing can defer a tail into a
//!    facility that does not exist yet. On this build the `runtime::task`
//!    spawn/sleep/interval seam routes into it, so asynchronous record
//!    completion (put-callback tails, ODLY re-processing) runs on those std
//!    threads rather than on a tokio runtime.
//! 2. **Database** — a small in-process database built through the ordinary
//!    [`epics_base_rs::server::ioc_builder::IocBuilder`], driven to completion with
//!    [`block_on_sync`](epics_base_rs::runtime::task::block_on_sync). On a
//!    plain thread with no runtime entered that selects `park_on`: the thread
//!    polls the build future and parks between polls. `build()` awaits only
//!    runtime-agnostic in-process locks, so nothing here needs a reactor.
//! 3. **`ca://` record links** —
//!    [`install_calink_resolver`](epics_ca_rs::calink::install_calink_resolver)
//!    mounts the CA external record-link resolver on the database (C
//!    `dbCaLinkInit`, `dbCa.c:1071`), so a ` CA`-modified or `ca://...`
//!    INP/OUT field resolves through a live `CaClient`. The client dials
//!    `EPICS_CA_NAME_SERVERS` over TCP; the UDP SEARCH transport is compiled
//!    out on this target.
//! 4. **CA front-end** — [`epics_ca_rs::server::blocking::BlockingCaServer`]: the TCP accept loop (C
//!    `CAS-TCP`, `caservertask.c:62`) and the UDP name-search responder (C
//!    `CAS-UDP`, `cast_server.c:113`), each on its own `std::thread`, with one
//!    blocking thread per accepted client (C `camsgtask`).
//!
//! # Configuration
//!
//! Ports come from the standard EPICS environment, resolved by the shared
//! server-side reader
//! [`cas_server_port`](epics_base_rs::runtime::net::cas_server_port)
//! (`EPICS_CAS_SERVER_PORT` > `EPICS_CA_SERVER_PORT` > 5064). UDP and TCP land
//! on the same port, as they do under a C IOC (`caservertask.c:491-499`).
//!
//! Any command-line arguments are taken as `.db` file paths and loaded in
//! order (the `dbLoadRecords` equivalent for a target with no iocsh). With no
//! arguments a small built-in database is loaded, so the binary is runnable
//! standalone on a bare target.
//!
//! There is no shutdown command: like a C IOC on RTEMS this runs until the
//! board is reset (on a host, until the process is signalled). The interactive
//! iocsh is host-only (`rustyline` does not build for RTEMS), so it is not
//! wired here.
//!
//! # Build configurations
//!
//! The real entry point is compiled for `target_os = "rtems"`, `target_os =
//! "vxworks"`, or the host-selectable `rtems-exec-model` feature. RTEMS has
//! no other option; on VxWorks this binary-level gate is necessary but not
//! sufficient — `epics-base-rs/build.rs` still only sets the `exec_backend`
//! cfg (the `runtime::task` seam's executor backend) for `target_os =
//! "rtems"` or the `rtems-exec-model` feature, so a VxWorks build also needs
//! that feature, or the lib-side predicate this gate anticipates, until the
//! `exec_backend` derivation itself recognizes `target_os = "vxworks"`.
//! Under the hosted default (tokio backend) this binary is
//! still built — so it stays compiled and linted in the default test set — but
//! refuses to run: with `runtime::task::spawn` routed to tokio, an
//! asynchronous record tail would need a runtime that this entry point
//! deliberately never starts, and `background_init` is not even compiled.
//! Reporting that at startup is the honest behaviour; silently starting a
//! runtime would defeat the purpose of the binary.

use std::process::ExitCode;

/// The built-in database, kept **outside** `mod ioc` deliberately, mirroring
/// `rtems-pva-ioc`'s `demo_db` and for its reason: it is data, not RTEMS
/// code, and the part the *default* host selection can check for real — with
/// no feature flag, so a typo below cannot reach a reader as a silent "no
/// such PV" on a serial console with no shell to ask. The `test` arm is what
/// lets the guards at the bottom of this file parse it; the `rtems` and
/// `rtems-exec-model` arms are the production use.
#[cfg(any(
    target_os = "rtems",
    target_os = "vxworks",
    feature = "rtems-exec-model",
    test
))]
mod demo_db {
    /// The database loaded when no `.db` file is given on the command line —
    /// small enough to run on a bare target, wide enough to exercise the
    /// scalar read, write and monitor paths over CA.
    ///
    /// IOC content only, and **link-free** by construction: no record here
    /// carries an INP/OUT field, so a default build is the no-`ca://`-link
    /// image `doc/calink-rtems-design.md` §11.7 item 4 asks for. The C6
    /// measurement rig — every probe record and both probe threads — is
    /// behind the `bringup-probes` feature ([`C6_PROBE_DB`]).
    pub const DEMO_DB: &str = concat!(
        "record(ao, \"RTEMS:AO\") { field(VAL, \"1.5\") field(PREC, \"3\") field(EGU, \"V\") }\n",
        "record(longout, \"RTEMS:LO\") { field(VAL, \"7\") field(EGU, \"counts\") }\n",
        "record(stringout, \"RTEMS:MSG\") { field(VAL, \"rtems-ca-ioc\") }\n",
    );

    /// C6 PROBE (doc/calink-rtems-design.md §6 stage C6, topology A): the 14
    /// measurement-rig records, loaded on top of [`DEMO_DB`] only under the
    /// `bringup-probes` feature (§11.7 item 3 — they are the measurement rig,
    /// not IOC content).
    ///
    /// They live in a compiled-in constant rather than in a `.db` file
    /// because a `-kernel` boot has no filesystem to name one on and
    /// `rtems_init.c:195` hands `main` a fixed one-element argv — the same
    /// forcing the pvalink probe recorded (`doc/pvalink-rtems-design.md`
    /// §12.3).
    #[cfg(feature = "bringup-probes")]
    pub const C6_PROBE_DB: &str = concat!(
        //
        // NOT `@ca://…`: a leading `@` is INST_IO and `dbCanSetLink`
        // (`record/link.rs:487`, C `dbStaticLib.c:2400`) refuses INST_IO on
        // a record whose bound device support declares CONSTANT — a soft
        // `ai`. Measured on target for the PVA twin (§12.2 there), and the
        // §6 table for this stage already spells both surviving forms:
        //
        //   * `INP=UPSTREAM:AI CP`      — the bare ` CA`-modifier spelling,
        //     C `pvlOptCA`. A `Db` link until `initialize_link_locality`
        //     turns it into a `Ca` link, which is exactly why the entry
        //     point runs that phase.
        //   * `INP=ca://UPSTREAM:AI CP` — this tree's scheme form.
        //
        // Both resolve through `strip_ca_scheme`, and §6 asks for both.
        "record(ai, \"RTEMS:CA:DOWN\") {\n",
        "  field(INP, \"UPSTREAM:AI CP\")\n",
        "  field(PREC, \"3\") field(EGU, \"V\") field(SCAN, \"Passive\")\n",
        "}\n",
        "record(ai, \"RTEMS:CA:DOWN2\") {\n",
        "  field(INP, \"ca://UPSTREAM:AI CP\")\n",
        "  field(PREC, \"3\") field(EGU, \"V\") field(SCAN, \"Passive\")\n",
        "}\n",
        // The OUT link. One record covers both `LinkPutOp` flavours
        // because the op is chosen by the *originating* write, not by the
        // link: `Database::external_put_op` (`database/links.rs:1801-1806`)
        // returns `Async` when the source record carries a put-notify
        // wait-set and `Plain` otherwise. So `caput` exercises `put_nowait`
        // and `caput -c` exercises `put`, through the same field.
        "record(ao, \"RTEMS:CA:UPLNK\") {\n",
        "  field(OUT, \"ca://UPSTREAM:AO\")\n",
        "  field(PREC, \"3\") field(EGU, \"V\") field(OMSL, \"supervisory\")\n",
        "  field(SCAN, \"Passive\")\n",
        "}\n",
        // ---- C6 PROBE: the stage-C4 band-occupancy measurement ----
        //
        // §6 stage C4's sign-off (2026-07-23) deferred the semantic change
        // and made this measurement the decision input: a deep CP→FLNK
        // chain on ONE link, and the delay it inflicts on a SECOND,
        // independent link's monitor→record latency and on timer callbacks
        // sharing the `cbMedium` band (§5.4).
        //
        // `RTEMS:CA:FAST` is the chain head — an external CP link, so
        // `run_monitor`'s inline `dispatch_external_cp_targets` is what
        // processes it, on the band worker that received the event. Its
        // FLNK runs C1..C8: nine records processed inline per upstream
        // event, which is the shape §5.4 says is unbounded.
        "record(ai, \"RTEMS:CA:FAST\") {\n",
        "  field(INP, \"ca://UPSTREAM:FAST CP\")\n",
        "  field(PREC, \"3\") field(SCAN, \"Passive\") field(FLNK, \"RTEMS:CA:C1\")\n",
        "}\n",
        "record(calc, \"RTEMS:CA:C1\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:CA:FAST\") field(FLNK, \"RTEMS:CA:C2\") }\n",
        "record(calc, \"RTEMS:CA:C2\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:CA:C1\") field(FLNK, \"RTEMS:CA:C3\") }\n",
        "record(calc, \"RTEMS:CA:C3\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:CA:C2\") field(FLNK, \"RTEMS:CA:C4\") }\n",
        "record(calc, \"RTEMS:CA:C4\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:CA:C3\") field(FLNK, \"RTEMS:CA:C5\") }\n",
        "record(calc, \"RTEMS:CA:C5\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:CA:C4\") field(FLNK, \"RTEMS:CA:C6\") }\n",
        "record(calc, \"RTEMS:CA:C6\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:CA:C5\") field(FLNK, \"RTEMS:CA:C7\") }\n",
        "record(calc, \"RTEMS:CA:C7\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:CA:C6\") field(FLNK, \"RTEMS:CA:C8\") }\n",
        "record(calc, \"RTEMS:CA:C8\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:CA:C7\") }\n",
        // The victim: a second, independent `ca://` link whose
        // monitor→record latency is what the measurement reads. No FLNK,
        // no chain — every millisecond it gains under load is a
        // millisecond the chain took from the band.
        "record(ai, \"RTEMS:CA:OTHER\") {\n",
        "  field(INP, \"ca://UPSTREAM:OTHER CP\")\n",
        "  field(PREC, \"3\") field(SCAN, \"Passive\")\n",
        "}\n",
        // The other victim: a timer callback on the same band. Driven by
        // the probe's `runtime::task::sleep` loop, which on this target is
        // a `cbMedium` task — so the interval between successive values a
        // host `camonitor` sees IS the band's timer jitter.
        "record(ai, \"RTEMS:CA:TICK\") { field(PREC, \"0\") field(SCAN, \"Passive\") }\n",
    );
}

#[cfg(any(
    target_os = "rtems",
    target_os = "vxworks",
    feature = "rtems-exec-model"
))]
mod ioc {
    use std::collections::HashMap;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::thread;
    use std::time::Instant;

    use epics_base_rs::error::CaResult;
    use epics_base_rs::runtime::net::cas_server_port;
    use epics_base_rs::runtime::task::{StackSizeClass, background_init, block_on_sync};
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::server::ioc_builder::IocBuilder;
    use epics_base_rs::server::status_pv::{StatusPv, serve_status_pvs, target_status_pvs};
    use epics_base_rs::types::EpicsValue;
    use epics_ca_rs::calink::install_calink_resolver;
    use epics_ca_rs::server::blocking::{BlockingCaServer, bind_udp_search, refused_clients};

    use crate::demo_db::DEMO_DB;

    /// The namespace the status PVs are published under, matching [`DEMO_DB`]'s.
    ///
    /// This plays `$(IOCNAME)`'s role in devIocStats' templates — the whole
    /// name is `<prefix>:<upstream leaf>`, one colon, upstream's spelling on
    /// the right.
    ///
    /// A constant because this binary has no configuration surface — there is
    /// no iocsh and no `.cmd` on the target. It follows that two of these IOCs
    /// on one subnet publish the same names and a client's SEARCH would be
    /// answered by both; the fix when that day comes is to take the prefix
    /// from the environment the way `cas_server_port` takes the port, and this
    /// note is here so the next reader does not have to rediscover why.
    const STATUS_PREFIX: &str = "RTEMS";

    /// C6 PROBE: the upstream CA name server the guest's `ca://` links
    /// resolve through. SLIRP puts the host at `10.0.2.2`; the port is the
    /// host-side upstream IOC's CA port, which cannot be the guest's own
    /// 5064 because that belongs to the inbound `hostfwd`.
    ///
    /// Overridable at *build* time through `C6_NAME_SERVERS`, because the
    /// measurement rigs differ only in this string and a target image has no
    /// configuration surface to differ in at runtime: topology B points it at
    /// the peer guest (`10.0.2.15:5064`), and the `MAX_DIAL_WORKERS` rig needs
    /// several blackholed addresses at once (`EPICS_CA_NAME_SERVERS` is a
    /// space-separated list, so `"192.0.2.1:5064 192.0.2.2:5064 …"` is one
    /// value here). A build that sets nothing keeps the C6 address.
    #[cfg(feature = "bringup-probes")]
    const C6_NAME_SERVER: &str = match option_env!("C6_NAME_SERVERS") {
        Some(s) => s,
        None => "10.0.2.2:15076",
    };

    /// C6 PROBE: the record the band-occupancy tick writes, and the
    /// interval it aims for. 200 ms is well inside the upstream's 10 Hz
    /// burst rate, so a burst overlaps several ticks.
    #[cfg(feature = "bringup-probes")]
    const C6_TICK_RECORD: &str = "RTEMS:CA:TICK";
    #[cfg(feature = "bringup-probes")]
    const C6_TICK_PERIOD_MS: u64 = 200;

    /// How long the banner waits for iocInit's staged link opens to reach
    /// the declared external-PV set before reporting the count. Bounded on
    /// purpose — an unreachable upstream must still boot the IOC and still
    /// print a banner, with the shortfall visible. (Introduced by the C6
    /// bring-up, kept unconditionally: it is what makes the banner a report
    /// rather than a race, whatever database was loaded.)
    const LINK_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    const LINK_SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(50);

    /// C6 PROBE: `rt top` + `rt stackuse`, from inside the image — this
    /// image configures the shell's commands but starts no shell.
    ///
    /// Both calls go through `epics_rtems_boot::stats`, which owns the per-OS
    /// backend. This used to be an `extern "C"` block plus a
    /// `#[cfg(target_os = …)]` / `#[cfg(not(…))]` pair right here, duplicated
    /// in `rtems-pva-ioc` — so a second OS meant editing two binaries to say
    /// the same thing twice, and the two copies had already drifted.
    #[cfg(feature = "bringup-probes")]
    fn c6_task_and_stack_report(tag: &str) {
        epics_rtems_boot::stats::dump_tasks(tag);
        epics_rtems_boot::stats::stack_report(tag);
    }

    /// C6 PROBE: one console report — the link registry, the shared
    /// client's circuit count, and every record the gate reads.
    ///
    /// The guest half of criteria 1, 3, 4 and 6: with no iocsh on the
    /// target the console is the only place the circuit count and a
    /// record's alarm state can be read from *inside* the IOC, next to
    /// what `caget` reads from outside.
    #[cfg(feature = "bringup-probes")]
    fn c6_report(
        seq: u32,
        resolver: &epics_ca_rs::calink::CaLinkResolver,
        db: &Arc<PvDatabase>,
        server: &Arc<BlockingCaServer>,
    ) {
        let conns = block_on_sync(resolver.client_connection_count())
            .ok()
            .flatten();
        // The descriptor reading, in the shape the topology-B rig already logs
        // it: the count against the ceiling, beside the *server* connection
        // count, so a descriptor that belongs to an inbound client is not
        // mistaken for one the client half is holding.
        let (fd_cnt, fd_max) = match epics_rtems_boot::stats::fd_usage() {
            Some(f) => (f.used as i64, f.max as i64),
            None => (-1, -1),
        };
        println!(
            "FDPROBE seq={seq} FD_CNT={fd_cnt} FD_MAX={fd_max} CA_CONN_CNT={}",
            server.active_connections()
        );
        println!(
            "C6 seq={seq} links={} circuits={conns:?}",
            resolver.link_count(),
        );
        // The §13 dial-pool bound, and the heap it was said to stop eating.
        // Both on one line so a console reader can see the attempt count that
        // the worker count is bounded *against* — a worker count of 1 proves
        // nothing next to an attempt count of 1.
        let (dial_workers, dial_attempts, dial_queued, dial_dialing) =
            epics_ca_rs::client::dial_pool_probe();
        let (mem_free, mem_used) = match epics_rtems_boot::stats::mem_usage() {
            Some(m) => (m.free as i64, m.used as i64),
            None => (-1, -1),
        };
        println!(
            "C6 seq={seq} dialpool workers={dial_workers} attempts={dial_attempts} \
             queued={dial_queued} dialing={dial_dialing} \
             MEM_FREE={mem_free} MEM_USED={mem_used}",
        );
        for (pv, connected) in resolver.link_report() {
            println!("C6 seq={seq} link pv={pv} connected={connected}");
        }
        for rec in [
            "RTEMS:CA:DOWN",
            "RTEMS:CA:DOWN2",
            "RTEMS:CA:UPLNK",
            "RTEMS:CA:FAST",
            "RTEMS:CA:C8",
            "RTEMS:CA:OTHER",
        ] {
            let val = db.get_pv(rec).map(|v| v.to_string());
            let sevr = db.get_pv(&format!("{rec}.SEVR")).map(|v| v.to_string());
            let stat = db.get_pv(&format!("{rec}.STAT")).map(|v| v.to_string());
            println!("C6 seq={seq} record {rec} VAL={val:?} SEVR={sevr:?} STAT={stat:?}");
        }
    }

    /// Load the database: every command-line argument is a `.db` file path
    /// (loaded in order, C `dbLoadRecords`), or the built-in demo database
    /// when there are none.
    ///
    /// Driven by `block_on_sync`, which parks this thread between polls of the
    /// build future — `IocBuilder::build` awaits only in-process locks, so no
    /// reactor is involved.
    fn load_database(db_files: &[String]) -> CaResult<Arc<PvDatabase>> {
        let macros = HashMap::new();
        let mut builder = IocBuilder::new();
        if db_files.is_empty() {
            builder = builder.db_string(DEMO_DB, &macros)?;
            // C6 PROBE records ride along whenever the built-in database is
            // the source — a bare `-kernel` boot cannot choose — but only on
            // a build that asked for the measurement rig.
            #[cfg(feature = "bringup-probes")]
            {
                builder = builder.db_string(crate::demo_db::C6_PROBE_DB, &macros)?;
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

    pub fn main() -> ExitCode {
        // (0) Pull the RTEMS boot shim into the link. Measured: rustc forwards
        //     a dependency's `rustc-link-lib` entries only when the binary
        //     actually references that dependency, so without this call the
        //     shim archive, `-lbsd -lm -lz` and `POSIX_Init` itself are all
        //     absent from the image. Compiles to nothing on a host build.
        //
        //     By the time this runs, `POSIX_Init` has already brought up the
        //     console, the clock, libbsd and DHCP, and called us.
        epics_rtems_boot::link_anchor();

        // (0-probe) C6 PROBE: the target's CA search configuration.
        // `rtems_init.c:195` hands `main` a fixed one-element argv and
        // `POSIX_Init` calls `setenv` zero times, so on the target nothing
        // outside the image can configure it — the values compiled in here
        // are the ones that take effect. §4.5's three variables, together:
        // the name server to dial, and the two that shut the broadcast path
        // off so a resolution can only have come over TCP.
        //
        // Defaults, not overrides: a variable that is already set stays as
        // set. On the target that is the same thing (nothing can set one);
        // on a host it is what lets `tests/rtems_ca_ioc_boots.rs` point the
        // dial at its own closed port instead of the image's SLIRP address.
        //
        // Set before `install_calink_resolver`, because the client the
        // resolver lazily builds reads them once at construction.
        //
        // SAFETY (edition 2024): this runs on the single init thread
        // before `background_init` starts any other thread, so no
        // concurrent reader or writer of the environment exists.
        #[cfg(feature = "bringup-probes")]
        for (var, compiled_in) in [
            ("EPICS_CA_NAME_SERVERS", C6_NAME_SERVER),
            ("EPICS_CA_ADDR_LIST", ""),
            ("EPICS_CA_AUTO_ADDR_LIST", "NO"),
        ] {
            if std::env::var_os(var).is_none() {
                unsafe { std::env::set_var(var, compiled_in) };
            }
        }

        // (0b) Make the IOC audible. Every diagnostic below — and every one in
        //      the CA server, the database and the runtime — is a `tracing`
        //      event, and an event with no subscriber is discarded. Without
        //      this line the IOC boots, serves, refuses clients at its memory
        //      ceiling and dies with an identical, empty console. C's
        //      `errlogPrintf` cannot be silenced this way; neither may ours.
        epics_base_rs::runtime::log::install_console_subscriber();

        // (0c) …and make a panic say what it costs. `std`'s default hook
        //      already writes to this console, so a panic is not invisible
        //      without this; what is missing from it is the consequence. A
        //      panic on a per-client thread kills that thread and leaves the
        //      IOC listening, answering searches and serving every other
        //      client — indistinguishable from health from outside, forever.
        //      The hook chains rather than replaces, so the payload, the
        //      location and the backtrace note all survive.
        epics_base_rs::runtime::log::install_panic_hook();

        // (0d) …and say which lock protocol the process got. C prints the
        //      same fact from `epicsMutexShowAll`; we have no iocsh to ask,
        //      so it goes on the console at boot. Before any thread that can
        //      take a record gate exists, so the line cannot be read as a
        //      report about a process that already ran without it.
        epics_base_rs::runtime::sync::report_lock_protocol();

        // (1) C `callbackInit` (callback.c:286) — the callback pool, delayed
        //     timer and scanOnce worker exist before any record can defer a
        //     tail into them. Idempotent.
        background_init();

        // (2) The database.
        let db_files: Vec<String> = std::env::args().skip(1).collect();
        let db = match load_database(&db_files) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("rtems-ca-ioc: iocInit failed: {e}");
                return ExitCode::FAILURE;
            }
        };

        // (2b) The calink resolver: install the `ca://` record-link resolver on
        //      the database so ` CA`-modified / `ca://...` INP/OUT fields
        //      resolve. C reaches the same state through `dbCaLinkInit`
        //      (`dbCa.c:1071`) during iocInit, after the database exists.
        //      Installed here — after the database, before the CA front-end —
        //      so every record's link fields route through it before the
        //      server answers its first client.
        //
        //      On this target the CA client reaches upstream servers over TCP
        //      name servers alone (`EPICS_CA_NAME_SERVERS`): the UDP SEARCH
        //      transport is compiled out (design §6 stage C5,
        //      `search::SearchTransport::NameServersOnly`), and every task the
        //      resolver spawns lands on the callback pool `background_init`
        //      started above via `runtime::task::spawn`, never a tokio runtime.
        let resolver = block_on_sync(install_calink_resolver(&db))
            .expect("the RTEMS entry point runs on a plain thread with no runtime entered");

        // (2c) iocInit's link phases, in `IocApplication::run`'s order
        //      (`ioc_app.rs:913-925`) and for its reasons. Without them the
        //      resolver above is mounted and unreachable:
        //
        //      * `initialize_link_locality` commits C `dbInitLink`'s locality
        //        decision (`dbLink.c:117-129` falling through
        //        `dbDbInitLink`'s `S_db_notFound`, `dbDbLink.c:94-96`) — a
        //        `Db` link naming a record this IOC does not have becomes a
        //        `Ca` link. `INP=UPSTREAM:AI CP`, the bare ` CA`-modifier
        //        spelling C calls `pvlOptCA`, is a `Db` link until this runs,
        //        so without it that whole spelling reads nothing forever.
        //      * `setup_cp_links` warms external CP/CPP links. A Passive
        //        holder of one is never scanned, so its link never opens
        //        lazily and its monitor is never created — a chicken-and-egg
        //        a lazy open cannot break.
        //      * `setup_external_link_opens` stages the rest, as C's
        //        `dbInitLink` hands every non-local `PV_LINK` to
        //        `dbCaAddLink` regardless of direction. Without it every
        //        other external link pays one cold scan cycle to stage its
        //        own open.
        //
        //      AFTER the mount above, not before: the warm path
        //      (`resolve_external_pv`) routes through the registered link
        //      set's lazy open and is a documented no-op when no lset is
        //      installed, so running these first would silently warm nothing.
        //
        //      `rtems-pva-ioc` needs no equivalent: `install_pvalink_resolver`
        //      walks the whole database itself and pre-registers every
        //      `pva://` link. `install_calink_resolver` registers the link set
        //      and scans nothing — C's `dbCaLinkInit` does not scan either —
        //      because for CA the scan IS these three iocInit passes.
        block_on_sync(async {
            db.initialize_link_locality().await;
            db.setup_cp_links().await;
            db.setup_external_link_opens().await;
        })
        .expect("the RTEMS entry point runs on a plain thread with no runtime entered");

        // (2d) Periodic scan + PINI. C `iocInit` owns `scanInit`/
        //      `initialProcess` (`dbScan.c`, `iocInit.c:653`) independent
        //      of RSRV — and until the scan-ownership hoist this binary
        //      had NO scan start at all (the scheduler lived only in the
        //      hosted async CA server's run loop, which this blocking
        //      target never runs), so every periodic `SCAN` field and
        //      every `PINI=YES` record was silently dead here. The core
        //      `ScanOwner` runs the PINI pass and the `scan-%g` threads on
        //      a dedicated owner thread (exec-backend safe: a parked
        //      detached task would be dropped by the executor). Held to
        //      the end of `main`: this IOC scans for as long as it serves.
        let _scan_owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());

        // (3) The CA front-end. UDP search port and TCP port are the same
        //     value, as under a C IOC (caservertask.c:491-499). No ACF is
        //     configured, so access control is the permissive default.
        let port = cas_server_port();
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let db_for_status = db.clone();
        let db_for_names = db.clone();
        let server = match BlockingCaServer::bind((Ipv4Addr::UNSPECIFIED, port), db, acf) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                // Not "cannot bind": `BlockingCaServer::bind` also calls
                // `local_addr`, and on RTEMS that is the call that fails.
                // The inner error says which.
                eprintln!("rtems-ca-ioc: cannot start the CA TCP server on port {port}: {e}");
                return ExitCode::FAILURE;
            }
        };
        let udp = match bind_udp_search(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "rtems-ca-ioc: cannot start the CA UDP search responder on port {port}: {e}"
                );
                return ExitCode::FAILURE;
            }
        };

        // (3b) The status PVs. Nothing on this target has a shell, so these
        //      numbers are the only way to ask the IOC how it is doing from
        //      anywhere but the serial console — and the console is
        //      write-only. See `status_pv`'s module docs for why this is a
        //      pusher and not a read hook, and why the names are devIocStats'.
        //
        //      `PVA_CONN_CNT` is NOT here, and not because it is expensive:
        //      `BlockingPvaServer::active_connections` is already public and
        //      would be one more `StatusPv`. This binary starts no PVA server
        //      (step (3) is the CA front-end and nothing else), so publishing
        //      it would publish a constant zero — a number that reads like
        //      "no PVA clients" when it means "no PVA server". `rtems-pva-ioc`
        //      is the entry point that owns a `BlockingPvaServer`, and it
        //      publishes it there.
        //
        //      Registered after `bind` so `CA_CONN_CNT` reads the server that
        //      is already listening, and before the accept thread starts so
        //      the first client cannot find them missing.
        let started = Instant::now();
        let conns_server = server.clone();
        let mut status = target_status_pvs(STATUS_PREFIX, started);
        status.extend([
            // devIocStats' `@ca_connections` (ioc.template:82-94). The ceiling
            // number: measured on target at 142 concurrent, the 143rd refused
            // by the libbsd socket zone with ENFILE.
            StatusPv::new(format!("{STATUS_PREFIX}:CA_CONN_CNT"), move || {
                EpicsValue::Double(conns_server.active_connections() as f64)
            }),
            // No upstream counterpart: devIocStats counts CA clients and CA
            // connections but never refusals. Named in the shape of the ones
            // that do exist rather than a different one. Climbs only when a
            // client was turned away; process-wide and monotonic, so a client
            // can tell "never happened" from "happened and stopped".
            StatusPv::new(format!("{STATUS_PREFIX}:CA_REFUSED_CNT"), || {
                EpicsValue::Double(refused_clients() as f64)
            }),
        ]);
        if let Err(e) = serve_status_pvs(db_for_status, status) {
            eprintln!("rtems-ca-ioc: cannot register the status PVs: {e}");
            return ExitCode::FAILURE;
        }

        // Listed after the status PVs are registered, so the console names
        // everything a client can reach rather than only what the `.db`
        // carried.
        let mut names = block_on_sync(db_for_names.all_record_names())
            .expect("the RTEMS entry point runs on a plain thread with no runtime entered");
        names.sort();

        let srv_tcp = server.clone();
        let tcp_thread = match thread::Builder::new()
            .name("CAS-TCP".to_string())
            // caservertask.c:716-718 — `epicsThreadStackMedium`. It accepts and
            // hands off; the per-client thread is where the depth is.
            .stack_size(StackSizeClass::Medium.bytes())
            .spawn(move || srv_tcp.serve())
        {
            Ok(h) => h,
            Err(e) => {
                eprintln!("rtems-ca-ioc: cannot start the CA accept thread: {e}");
                return ExitCode::FAILURE;
            }
        };
        let srv_udp = server.clone();
        let udp_thread = match thread::Builder::new()
            .name("CAS-UDP".to_string())
            // caservertask.c:722-724 — `epicsThreadStackMedium`, same as TCP.
            .stack_size(StackSizeClass::Medium.bytes())
            .spawn(move || srv_udp.serve_udp_search(udp))
        {
            Ok(h) => h,
            Err(e) => {
                eprintln!("rtems-ca-ioc: cannot start the CA name-search thread: {e}");
                return ExitCode::FAILURE;
            }
        };

        println!(
            "rtems-ca-ioc: serving {} records on CA port {port} (TCP + UDP search), \
             RTEMS execution model, no tokio runtime",
            names.len()
        );
        // The calink resolver is mounted, so ` CA`-modified and `ca://...`
        // INP/OUT links resolve. Reported at every boot — including
        // `link_count == 0`, when the loaded database configured none —
        // because on a target with no shell the console is the only place an
        // operator can confirm the resolver came up and how many links it
        // registered. The client reaches upstream servers over
        // `EPICS_CA_NAME_SERVERS` (TCP); the UDP SEARCH transport is compiled
        // out on this target, so a link to a server reachable only by
        // broadcast will not resolve, and `EPICS_CA_ADDR_LIST` is not even
        // parsed here.
        //
        // Counted against the database's DECLARED external PV set, after
        // waiting for the registry to reach it.
        //
        // iocInit's two link phases STAGE opens (`setup_cp_links` ->
        // `resolve_external_pv`, `setup_external_link_opens` ->
        // `stage_external_link_open_by_name`); each open then runs on the
        // link work owner and only registers its `CaLink` after a
        // `subscribe()` round-trip to the upstream IOC completes. A count
        // sampled the instant `run()` returns is therefore a race against the
        // network, and stage C6 boot 3 read it as the useless `0` while the
        // registry was on its way to 4. Waiting until the registry reaches
        // the declared set turns the banner from a sample into a report; the
        // deadline bounds it so an unreachable upstream still boots and still
        // prints, with the shortfall named.
        //
        // `external_link_pv_names` is per-PV, matching what a link set holds;
        // iocInit's own lines above count link FIELDS, which is why they read
        // 4 and 1 while this reads 4.
        let declared = block_on_sync(db_for_names.external_link_pv_names())
            .expect("the RTEMS entry point runs on a plain thread with no runtime entered");
        let deadline = std::time::Instant::now() + LINK_SETTLE_TIMEOUT;
        while resolver.link_count() < declared.len() && std::time::Instant::now() < deadline {
            std::thread::sleep(LINK_SETTLE_POLL);
        }
        let link_count = resolver.link_count();
        println!(
            "rtems-ca-ioc: calink resolver installed — {link_count}/{} ca:// record \
             link{} registered ({}); ` CA`-modified and ca://... INP/OUT resolve over \
             EPICS_CA_NAME_SERVERS (TCP name servers; UDP search is compiled out \
             on this target)",
            declared.len(),
            if declared.len() == 1 { "" } else { "s" },
            if declared.is_empty() {
                "none declared".to_string()
            } else {
                declared.join(", ")
            },
        );
        for name in &names {
            println!("rtems-ca-ioc: {name}");
        }

        // ---- C6 PROBE: the two measurement threads ----
        //
        // (a) The band-occupancy tick. A `runtime::task::spawn` +
        //     `runtime::task::sleep` loop, which on this target IS a
        //     `cbMedium` band task — the same band `run_monitor` and its
        //     inline `dispatch_external_cp_targets` run on (§5.4). Each
        //     wake writes an incrementing count to `RTEMS:CA:TICK`, so a
        //     host `camonitor` on that record measures the band's timer
        //     jitter directly: `Instant` is 1-second-quantized on target
        //     (§5.5), so the sub-second numbers have to be read off the
        //     wire, not off the guest's clock.
        #[cfg(feature = "bringup-probes")]
        {
            let tick_db = db_for_names.clone();
            println!(
                "C6 probe: tick task on the callback band writing {C6_TICK_RECORD} \
                 every {C6_TICK_PERIOD_MS} ms",
            );
            epics_base_rs::runtime::task::spawn(async move {
                let mut n = 0i64;
                loop {
                    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(
                        C6_TICK_PERIOD_MS,
                    ))
                    .await;
                    n += 1;
                    // The `dbPutField` shape (fire-and-forget), not `put_pv`:
                    // an ai's VAL is `pp(TRUE)`, so a bare `dbPut` suppresses
                    // its immediate monitor post (dbAccess.c:1414-1418) and
                    // leaves the record's UDF *alarm* standing — a host
                    // `camonitor` on the tick saw one line, forever
                    // (doc/calink-rtems-design.md §11.7 item 2). C driver
                    // code writing its own record does `dbPutField`, whose pp
                    // gate processes the passive record: the cycle posts the
                    // monitor with a fresh timestamp and clears the UDF
                    // alarm, which is what makes the tick measurable off the
                    // wire.
                    let _ = tick_db
                        .put_record_field_from_ca_no_notify(
                            C6_TICK_RECORD,
                            "VAL",
                            EpicsValue::Double(n as f64),
                        )
                        .await;
                }
            });
        }

        // (b) The console reporter, in the shape the pvalink stage-5 probe
        //     used (`doc/pvalink-rtems-design.md` §12.4): one line group
        //     every 10 s, and the task census + stack-usage report every
        //     6th pass (~1 min), which is the `rt top` / `rt stackuse`
        //     reading criterion 6 asks for on a target that starts no
        //     shell. Its own thread with a stated stack class, like every
        //     other thread this entry point starts.
        #[cfg(feature = "bringup-probes")]
        {
            let probe_db = db_for_names.clone();
            let probe_resolver = resolver.clone();
            let probe_server = server.clone();
            println!(
                "C6 probe: EPICS_CA_NAME_SERVERS={C6_NAME_SERVER} (compiled in), \
                 EPICS_CA_ADDR_LIST empty, EPICS_CA_AUTO_ADDR_LIST=NO; reporting every 10 s",
            );
            match thread::Builder::new()
                .name("c6-probe".to_string())
                .stack_size(StackSizeClass::Medium.bytes())
                .spawn(move || {
                    let _ = epics_base_rs::runtime::task::enter_ioc_thread(
                        epics_base_rs::runtime::task::ThreadPriority::Low,
                    );
                    let mut seq = 0u32;
                    loop {
                        thread::sleep(std::time::Duration::from_secs(10));
                        seq += 1;
                        c6_report(seq, &probe_resolver, &probe_db, &probe_server);
                        // The census is one line per open descriptor, so it
                        // runs on the same 1-minute pass as the task census
                        // rather than every 10 s: an outage measurement wants
                        // the identity to be re-read as the phases change, not
                        // to bury the per-tick lines it is read against.
                        if seq.is_multiple_of(6) {
                            epics_rtems_boot::stats::fd_census(&format!("c6-{seq}"));
                            c6_task_and_stack_report(&format!("c6-{seq}"));
                        }
                    }
                }) {
                Ok(_) => {}
                Err(e) => eprintln!("C6 probe: cannot start the reporter thread: {e}"),
            }
        }

        // Runs until the process is killed: an IOC has no self-shutdown path
        // on this target. Joining the accept thread is how the main thread
        // waits; the accept loop only returns if `shutdown()` is requested,
        // which nothing here does.
        let _ = tcp_thread.join();
        match udp_thread.join() {
            Ok(Ok(())) => ExitCode::SUCCESS,
            Ok(Err(e)) => {
                eprintln!("rtems-ca-ioc: name-search responder failed: {e}");
                ExitCode::FAILURE
            }
            Err(_) => {
                eprintln!("rtems-ca-ioc: name-search thread panicked");
                ExitCode::FAILURE
            }
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
        "rtems-ca-ioc: built with the tokio task backend, which this entry point \
         does not start a runtime for.\n\
         Build it for `armv7-rtems-eabihf` or a VxWorks target, or on a host with \
         `--features rtems-exec-model`."
    );
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    /// The RTEMS constraint for the entry point: it must never construct or
    /// enter a tokio runtime, and must never reach for tokio's async
    /// net/timer/spawn machinery — none of which the RTEMS build can drive.
    /// `tokio::sync` (the runtime-agnostic locks the ACF handle is built from)
    /// IS allowed. Static guard over this file's own source, in the same shape
    /// as `server::blocking`'s: the RTEMS `cargo check` cannot catch this on
    /// its own, because tokio's `rt`/`rt-multi-thread` features are retained
    /// on that target, so a runtime constructor still *compiles* there — only
    /// this guard rejects it. (Comments in this file deliberately avoid the
    /// forbidden literals so they cannot self-match.)
    #[test]
    fn entry_point_never_starts_a_runtime() {
        let src = include_str!("rtems-ca-ioc.rs");
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
                "the RTEMS CA entry point must not reference `{token}`: it starts no \
                 tokio runtime and drives every future via park_on"
            );
        }
    }

    /// The target has no shell, so an IOC that publishes no status PVs can only
    /// be asked how it is doing by reading a write-only serial console. Each of
    /// these answers a question nothing else on the box answers: how close the
    /// connection ceiling is, and whether anyone has been turned away.
    ///
    /// The descriptor, heap and uptime values are NOT checked here — they come
    /// from `target_status_pvs`, which owns their names and has its own test
    /// pinning them. Restating them here would be a second copy of the naming
    /// rule, which is the thing that function exists to prevent.
    #[test]
    fn the_entry_point_publishes_its_status() {
        let src = include_str!("rtems-ca-ioc.rs");
        let prod = match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        assert!(
            prod.contains(concat!("serve_status_", "pvs(")),
            "the entry point registers no status PVs; on a target with no iocsh \
             that leaves `caget` with nothing to ask"
        );
        assert!(
            prod.contains(concat!("target_status_", "pvs(STATUS_PREFIX")),
            "the entry point stopped publishing the common descriptor/heap/uptime \
             set; FD_CNT against FD_MAX is the value that predicts the ceiling"
        );
        for value in [":CA_CONN_CNT", ":CA_REFUSED_CNT"] {
            assert!(
                prod.contains(value),
                "status PV `{value}` is gone from the entry point"
            );
        }
        // The handle is deliberately not held: `StatusPvs` has no `Drop`, so
        // the pusher outlives it. If that ever changes, this entry point —
        // which discards the handle — starts publishing three PVs frozen at 0,
        // and `dropping_the_handle_does_not_stop_the_pusher` in `status_pv` is
        // the test that fails first.
        assert!(
            !prod.contains(concat!("let _stat", "us = ")),
            "this entry point does not hold the status-PV handle, and must not \
             start pretending it needs to"
        );
    }

    /// The calink resolver is mounted, and the banner reports it.
    ///
    /// The CA counterpart of `rtems-pva-ioc`'s
    /// `the_pvalink_resolver_is_mounted_and_the_banner_reports_it`, and it
    /// exists for the same reason: a regression that dropped the
    /// `install_calink_resolver` call would leave an IOC that still boots,
    /// still serves every record and still answers searches — and silently
    /// resolves no ` CA`-modified or `ca://...` INP/OUT link at all, with no
    /// shell on the target to notice. The banner line is the only place an
    /// operator can confirm from the console that the resolver came up and how
    /// many links it registered, so it is checked too.
    #[test]
    fn the_calink_resolver_is_mounted_and_the_banner_reports_it() {
        let src = include_str!("rtems-ca-ioc.rs");
        let prod = match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        // `concat!` so these assertions cannot match their own source text.
        assert!(
            prod.contains(concat!("install_calink_", "resolver(&db)")),
            "the calink resolver is no longer installed; ` CA`-modified and \
             ca://... record links would silently never resolve, and there is no \
             shell on the target to say so"
        );
        assert!(
            prod.contains("calink resolver installed"),
            "the boot banner no longer reports the calink resolver; on a target with \
             no shell the console is the only place an operator can confirm it came up"
        );
        assert!(
            prod.contains("link_count"),
            "the banner no longer reports the registered link count, the one number \
             that tells an operator whether the database's ca:// links were seen"
        );
    }

    /// The mount is not enough on its own: iocInit's three link phases are
    /// what route the database's links through it.
    ///
    /// This binary does not go through `IocApplication::run`, which is the
    /// only other place these are called, so dropping them here drops them
    /// entirely — and the failure is silent in exactly the way the target
    /// cannot survive. Without `initialize_link_locality` the bare
    /// ` CA`-modifier spelling (`INP=UPSTREAM:AI CP`) stays a local `Db` link
    /// to a record that does not exist; without `setup_cp_links` a Passive
    /// holder of an external CP link never opens it; without
    /// `setup_external_link_opens` every other external link waits for a cold
    /// scan. The IOC boots and serves in all three cases.
    #[test]
    fn iocinit_link_phases_run_after_the_mount() {
        let src = include_str!("rtems-ca-ioc.rs");
        let prod = match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        // `concat!` so these assertions cannot match their own source text.
        let mount = concat!("install_calink_", "resolver(&db)");
        let phases = [
            concat!("initialize_link_", "locality()"),
            concat!("setup_cp_", "links()"),
            concat!("setup_external_link_", "opens()"),
        ];
        let mount_at = prod
            .find(mount)
            .expect("the mount guard above covers this; if it passed, this cannot fail");
        for phase in phases {
            let at = prod.find(phase).unwrap_or_else(|| {
                panic!(
                    "this entry point no longer calls `{phase}`; it is not reached by \
                     `IocApplication::run` either, so the database's ca:// links would \
                     never be routed through the mounted resolver and the IOC would boot \
                     and serve with every external link dead"
                )
            });
            assert!(
                at > mount_at,
                "`{phase}` must run AFTER the resolver mount: the warm path routes \
                 through the registered link set and is a no-op with none installed"
            );
        }
    }

    /// A panic on a per-connection thread kills that thread and leaves the IOC
    /// listening, answering searches and serving every other client — from
    /// outside, indistinguishable from health, forever. `std`'s default hook
    /// says a thread panicked; it does not say that. Dropping this call would
    /// restore exactly the silent-degradation shape the console subscriber
    /// above exists to remove.
    #[test]
    fn a_panic_reaches_the_errlog_and_says_what_it_costs() {
        let src = include_str!("rtems-ca-ioc.rs");
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

    /// The default binary is IOC content only: no C6 measurement-rig records,
    /// and — `doc/calink-rtems-design.md` §11.7 item 4's enabler — no link
    /// fields at all, so a default image holds zero client-side descriptors
    /// and the per-circuit fd cost can be isolated against it.
    ///
    /// Runs in every host test pass (the probe rig moved to `C6_PROBE_DB`,
    /// which only exists under `bringup-probes`), so a probe record leaking
    /// back into `DEMO_DB` fails the default `--workspace` suite, not just a
    /// feature slice.
    #[test]
    fn the_default_database_is_clean_and_link_free() {
        use std::collections::HashMap;

        let recs =
            epics_base_rs::server::db_loader::parse_db(crate::demo_db::DEMO_DB, &HashMap::new())
                .expect("the built-in database must parse");
        let names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["RTEMS:AO", "RTEMS:LO", "RTEMS:MSG"],
            "the default database is the three demo records and nothing else"
        );
        for rec in &recs {
            assert!(
                !rec.name.starts_with("RTEMS:CA:"),
                "{} is a C6 probe record; the measurement rig belongs behind \
                 `bringup-probes`, not in the default image",
                rec.name
            );
            for (field, _) in &rec.fields {
                assert!(
                    !matches!(field.as_str(), "INP" | "OUT" | "INPA" | "FLNK"),
                    "{}.{field} is a link field: the default image must be \
                     link-free (§11.7 item 4) — put link-bearing records behind \
                     `bringup-probes` or in a loaded .db",
                    rec.name
                );
            }
        }
    }

    /// The probe ride-along in `load_database` is behind the feature gate.
    ///
    /// The parse tests pin what each constant *contains*; this pins what the
    /// binary *loads*: an edit that loads `C6_PROBE_DB` unconditionally would
    /// pass both parse tests and ship the rig in the default image.
    #[test]
    fn the_probe_db_loads_only_behind_the_feature() {
        let src = include_str!("rtems-ca-ioc.rs");
        let prod = match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        let load_at = prod
            .find(concat!("crate::demo_db::C6_PROBE_", "DB, &macros"))
            .expect("the probe database is loaded somewhere in load_database");
        let before = &prod[load_at.saturating_sub(400)..load_at];
        assert!(
            before.contains(concat!("#[cfg(feature = \"bringup-", "probes\")]")),
            "the C6_PROBE_DB load site is not feature-gated: the measurement \
             rig would ship in the default image"
        );
    }

    /// With the feature on, the rig is exactly the 14 records the C6
    /// measurement was built from (`doc/calink-rtems-design.md` §11.7 item
    /// 3), on top of the same clean demo set — so the bring-up image is one
    /// cargo flag away, not a code edit away.
    #[cfg(feature = "bringup-probes")]
    #[test]
    fn the_probe_rig_defines_the_c6_records() {
        use std::collections::HashMap;

        let full = format!("{}{}", crate::demo_db::DEMO_DB, crate::demo_db::C6_PROBE_DB);
        let recs = epics_base_rs::server::db_loader::parse_db(&full, &HashMap::new())
            .expect("the demo + probe database must parse");
        assert_eq!(recs.len(), 17, "3 demo records plus the 14-record rig");
        for name in [
            "RTEMS:CA:DOWN",
            "RTEMS:CA:DOWN2",
            "RTEMS:CA:UPLNK",
            "RTEMS:CA:FAST",
            "RTEMS:CA:C1",
            "RTEMS:CA:C2",
            "RTEMS:CA:C3",
            "RTEMS:CA:C4",
            "RTEMS:CA:C5",
            "RTEMS:CA:C6",
            "RTEMS:CA:C7",
            "RTEMS:CA:C8",
            "RTEMS:CA:OTHER",
            "RTEMS:CA:TICK",
        ] {
            assert!(
                recs.iter().any(|r| r.name == name),
                "probe record {name} is missing from the rig"
            );
        }
        // Both link spellings the C6 stage table asks for survive the split:
        // the bare ` CA`-modifier and the `ca://` scheme.
        let inp = |name: &str| {
            recs.iter()
                .find(|r| r.name == name)
                .and_then(|r| r.fields.iter().find(|(f, _)| f == "INP"))
                .map(|(_, v)| v.to_string())
        };
        assert_eq!(inp("RTEMS:CA:DOWN").as_deref(), Some("UPSTREAM:AI CP"));
        assert_eq!(
            inp("RTEMS:CA:DOWN2").as_deref(),
            Some("ca://UPSTREAM:AI CP")
        );
    }
}
