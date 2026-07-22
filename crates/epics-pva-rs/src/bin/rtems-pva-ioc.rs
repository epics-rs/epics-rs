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
//!    [`PvDatabaseSource`](epics_pva_rs::server::PvDatabaseSource): the TCP accept loop on one thread, the UDP
//!    name-search responder on another.
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
//! Any command-line arguments are taken as `.db` file paths and loaded in
//! order (the `dbLoadRecords` equivalent for a target with no iocsh). With no
//! arguments a small built-in database is loaded, so the binary is runnable
//! standalone on a bare target.
//!
//! There is no shutdown command: like a C IOC on RTEMS this runs until the
//! board is reset. The interactive iocsh is host-only, so it is not wired here.
//!
//! # Build configurations
//!
//! Same predicate as `rtems-ca-ioc`: the real entry point is compiled when the
//! `runtime::task` seam is on its **executor** backend — `target_os = "rtems"`
//! or the host-selectable `rtems-exec-model` feature. Under the hosted default
//! it is still built, so it stays compiled and linted in the default test set,
//! but refuses to run rather than silently starting the runtime it exists to
//! avoid.

use std::process::ExitCode;

#[cfg(any(target_os = "rtems", feature = "rtems-exec-model"))]
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
    use epics_base_rs::server::ioc_builder::IocBuilder;
    use epics_base_rs::server::status_pv::{StatusPv, serve_status_pvs, target_status_pvs};
    use epics_base_rs::types::EpicsValue;
    use epics_pva_rs::server::PvDatabaseSource;
    use epics_pva_rs::server_native::blocking::{BlockingPvaServer, bind_udp_search};
    use epics_pva_rs::server_native::config::PvaServerConfig;

    /// The built-in database loaded when no `.db` file is given on the command
    /// line — small enough to run on a bare target, wide enough to exercise
    /// the scalar GET, PUT and MONITOR paths over pvAccess.
    const DEMO_DB: &str = concat!(
        "record(ao, \"RTEMS:PVA:AO\") { field(VAL, \"1.5\") field(PREC, \"3\") field(EGU, \"V\") }\n",
        "record(longout, \"RTEMS:PVA:LO\") { field(VAL, \"7\") field(EGU, \"counts\") }\n",
        "record(stringout, \"RTEMS:PVA:MSG\") { field(VAL, \"rtems-pva-ioc\") }\n",
    );

    /// The namespace the status PVs are published under.
    ///
    /// This plays `$(IOCNAME)`'s role in devIocStats' templates — the whole
    /// name is `<prefix>:<upstream leaf>`, one colon, upstream's spelling on
    /// the right. Deliberately the same value `rtems-ca-ioc` uses: the two
    /// binaries are two front-ends for the same board, never both running, so
    /// an operator's screens should not have to know which one booted.
    const STATUS_PREFIX: &str = "RTEMS";

    /// Load the database: every command-line argument is a `.db` file path
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

        // (1) C `callbackInit` (callback.c:286). Idempotent.
        background_init();

        // (2) The database.
        let db_files: Vec<String> = std::env::args().skip(1).collect();
        let db = match load_database(&db_files) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("rtems-pva-ioc: iocInit failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        let db_for_names = db.clone();
        let db_for_status = db.clone();

        // (3) The PVA front-end. `bind` consumes the config, so the two ports
        //     are read off it first.
        let config = PvaServerConfig::default().with_env();
        let (tcp_port, udp_port, bind_ip) = (config.tcp_port, config.udp_port, config.bind_ip);
        let source = Arc::new(PvDatabaseSource::new(db));
        let server =
            match BlockingPvaServer::bind(SocketAddr::new(bind_ip, tcp_port), source, config) {
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
    }
}

#[cfg(any(target_os = "rtems", feature = "rtems-exec-model"))]
fn main() -> ExitCode {
    ioc::main()
}

#[cfg(not(any(target_os = "rtems", feature = "rtems-exec-model")))]
fn main() -> ExitCode {
    eprintln!(
        "rtems-pva-ioc: built with the tokio task backend, which this entry point \
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
}
