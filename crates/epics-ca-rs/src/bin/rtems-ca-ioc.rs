//! `rtems-ca-ioc` — the RTEMS CA IOC entry point (design doc §9.5).
//!
//! A runnable `main` that brings up a complete CA IOC on the **RTEMS execution
//! model** and nothing else: no tokio runtime is ever created, no async
//! front-end is touched, and every long-lived loop owns a dedicated OS thread.
//! Three facilities, started in the order C `iocInit` starts its equivalents:
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
//!    [`IocBuilder`], driven to completion with
//!    [`block_on_sync`](epics_base_rs::runtime::task::block_on_sync). On a
//!    plain thread with no runtime entered that selects `park_on`: the thread
//!    polls the build future and parks between polls. `build()` awaits only
//!    runtime-agnostic in-process locks, so nothing here needs a reactor.
//! 3. **CA front-end** — [`BlockingCaServer`]: the TCP accept loop (C
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

    use epics_base_rs::error::CaResult;
    use epics_base_rs::runtime::net::cas_server_port;
    use epics_base_rs::runtime::task::{StackSizeClass, background_init, block_on_sync};
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::server::ioc_builder::IocBuilder;
    use epics_ca_rs::server::blocking::{BlockingCaServer, bind_udp_search};

    /// The built-in database loaded when no `.db` file is given on the command
    /// line — small enough to run on a bare target, wide enough to exercise
    /// the scalar read, write and monitor paths over CA.
    const DEMO_DB: &str = concat!(
        "record(ao, \"RTEMS:AO\") { field(VAL, \"1.5\") field(PREC, \"3\") field(EGU, \"V\") }\n",
        "record(longout, \"RTEMS:LO\") { field(VAL, \"7\") field(EGU, \"counts\") }\n",
        "record(stringout, \"RTEMS:MSG\") { field(VAL, \"rtems-ca-ioc\") }\n",
    );

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
        let mut names = block_on_sync(db.all_record_names())
            .expect("the RTEMS entry point runs on a plain thread with no runtime entered");
        names.sort();

        // (3) The CA front-end. UDP search port and TCP port are the same
        //     value, as under a C IOC (caservertask.c:491-499). No ACF is
        //     configured, so access control is the permissive default.
        let port = cas_server_port();
        let acf = Arc::new(tokio::sync::RwLock::new(None));
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
}
