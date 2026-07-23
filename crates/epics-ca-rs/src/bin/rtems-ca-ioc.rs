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
//!    `dbCaLinkInit`, `dbCa.c:1071`), so a ` CA`-modified or `@ca://...`
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
//! The real entry point is compiled when the `runtime::task` seam is on its
//! **executor** backend — that is exactly `target_os = "rtems"` (the RTEMS
//! target has no other option) or the host-selectable `rtems-exec-model`
//! feature, the same predicate `epics-base-rs/build.rs` uses to set
//! `exec_backend`. Under the hosted default (tokio backend) this binary is
//! still built — so it stays compiled and linted in the default test set — but
//! refuses to run: with `runtime::task::spawn` routed to tokio, an
//! asynchronous record tail would need a runtime that this entry point
//! deliberately never starts, and `background_init` is not even compiled.
//! Reporting that at startup is the honest behaviour; silently starting a
//! runtime would defeat the purpose of the binary.

use std::process::ExitCode;

#[cfg(any(target_os = "rtems", feature = "rtems-exec-model"))]
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

    /// The built-in database loaded when no `.db` file is given on the command
    /// line — small enough to run on a bare target, wide enough to exercise
    /// the scalar read, write and monitor paths over CA.
    const DEMO_DB: &str = concat!(
        "record(ao, \"RTEMS:AO\") { field(VAL, \"1.5\") field(PREC, \"3\") field(EGU, \"V\") }\n",
        "record(longout, \"RTEMS:LO\") { field(VAL, \"7\") field(EGU, \"counts\") }\n",
        "record(stringout, \"RTEMS:MSG\") { field(VAL, \"rtems-ca-ioc\") }\n",
    );

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
        //      the database so ` CA`-modified / `@ca://...` INP/OUT fields
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
        // The calink resolver is mounted, so ` CA`-modified and `@ca://...`
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
        // Read here rather than at install time: `install_calink_resolver`
        // registers the link set and nothing else — C's `dbCaLinkInit`
        // likewise creates no channel — so a count taken at the mount reports
        // the resolver's own health, not the database's links. Taken at the
        // last moment before the banner, it is the registry as the first
        // client will find it.
        let link_count = resolver.link_count();
        println!(
            "rtems-ca-ioc: calink resolver installed — {link_count} ca:// record \
             link{} registered; ` CA`-modified and @ca://... INP/OUT resolve over \
             EPICS_CA_NAME_SERVERS (TCP name servers; UDP search is compiled out \
             on this target)",
            if link_count == 1 { "" } else { "s" },
        );
        for name in &names {
            println!("rtems-ca-ioc: {name}");
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

#[cfg(any(target_os = "rtems", feature = "rtems-exec-model"))]
fn main() -> ExitCode {
    ioc::main()
}

#[cfg(not(any(target_os = "rtems", feature = "rtems-exec-model")))]
fn main() -> ExitCode {
    eprintln!(
        "rtems-ca-ioc: built with the tokio task backend, which this entry point \
         does not start a runtime for.\n\
         Build it for `armv7-rtems-eabihf`, or on a host with \
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
    /// resolves no ` CA`-modified or `@ca://...` INP/OUT link at all, with no
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
             @ca://... record links would silently never resolve, and there is no \
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
}
