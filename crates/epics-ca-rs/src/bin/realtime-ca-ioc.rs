//! `realtime-ca-ioc` — the RTEMS CA IOC entry point (design doc §9.5).
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
//!    `dbCaLinkInit`, `dbCa.c:352-355`), so a ` CA`-modified or `ca://...`
//!    INP/OUT field resolves through a live `CaClient`. The client binds the
//!    UDP SEARCH socket and additionally dials `EPICS_CA_NAME_SERVERS` over
//!    TCP, as it does on a host — see `epics_base_rs::net::search_udp`.
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
//! on the same port, as they do under a C IOC (`caservertask.c:492-500`).
//!
//! Command-line arguments are this target's st.cmd, because it has no iocsh: a
//! `NAME=VALUE` argument is `epicsEnvSet` and anything else is a `.db` file
//! path, loaded in order (the `dbLoadRecords` equivalent). Every assignment is
//! applied before the first file is read, so the environment above is complete
//! before any of it is consumed — see
//! [`epics_rtems_boot::boot_args`], which owns
//! that rule for both target IOCs. With no arguments a small built-in database
//! is loaded, so the binary is runnable standalone on a bare target.
//!
//! On the RTEMS target the line comes from `EPICS_RTEMS_CMDLINE` at build time
//! or the DHCP option `rtems_cmdline` at boot, whichever the board got last;
//! `csrc/rtems_init.c` splits it into argv.
//!
//! There is no shutdown command: like a C IOC on RTEMS this runs until the
//! board is reset (on a host, until the process is signalled). The interactive
//! iocsh is host-only (`rustyline` does not build for RTEMS), so it is not
//! wired here.
//!
//! # Build configurations
//!
//! The real entry point is compiled for `target_os = "rtems"`, `target_os =
//! "vxworks"`, or the host-selectable `EPICS_RS_BUILD_EXEC_BACKEND=thread`.
//! RTEMS has no other option; on VxWorks this binary-level gate is necessary
//! but not sufficient — `epics-base-rs/build.rs` still only sets the
//! `exec_backend` cfg (the `runtime::task` seam's executor backend) for
//! `target_os = "rtems"` or `EPICS_RS_BUILD_EXEC_BACKEND=thread`, so a VxWorks
//! build also needs that variable, or the lib-side predicate this gate
//! anticipates, until the `exec_backend` derivation itself recognizes
//! `target_os = "vxworks"`. Under the hosted default (tokio backend) this
//! binary is still built — so it stays compiled and linted in the default test
//! set — but refuses to run. The reason is narrower than it once read here,
//! and worth stating exactly, because two of the three parts it used to name
//! no longer hold: record tails go to the background executor through
//! `spawn_background`, and this binary has no PVA in it at all. The CA
//! *server* half does not need a runtime either — `BlockingCaServer` suspends
//! only on runtime-agnostic `tokio::sync` primitives, and a static source
//! guard in that file holds it to that (`server/blocking.rs`,
//! `blocking_driver_has_no_async_runtime_symbols`).
//!
//! What remains is the CA *client* half that facility 3 mounts. Every task it
//! starts — the `EPICS_CA_NAME_SERVERS` circuits in `search::run_engine`, that
//! circuit's reader in `search::serve_nameserver_circuit`, the transport
//! managers, the coordinator, the beacon monitor — is spawned through the
//! `Reactor` capability, and `CaClient::new_with_config` mints the one instance
//! of it with `Reactor::current().expect(..)`. On the tokio backend
//! `Reactor::current` is `Handle::try_current().ok()`, so it is `None` on a
//! thread that has not entered a runtime — and `mod ioc` runs on the process
//! main thread, which deliberately never enters one. A hosted build that
//! reached `ioc::main` therefore fails when it builds the client, at an
//! `expect` that names the contract, rather than surviving to the first
//! `ca://` link and dying inside a spawn on tokio's "there is no reactor
//! running". Reporting that at startup is the honest behaviour; silently
//! starting a runtime would defeat the purpose of the binary.
//!
//! Symbols, not line numbers, on purpose: the three anchors this paragraph
//! used to carry (`server/blocking.rs:2294`, `client/search.rs:1016` and
//! `:1303`) were all stale within one commit of being written, because
//! `8d583a91` moved the very code they named.

use std::process::ExitCode;

/// The built-in database, kept **outside** `mod ioc` deliberately, mirroring
/// `realtime-pva-ioc`'s `demo_db` and for its reason: it is data, not RTEMS
/// code, and the part the *default* host selection can check for real — with
/// no feature flag, so a typo below cannot reach a reader as a silent "no
/// such PV" on a serial console with no shell to ask. The `test` arm is what
/// lets the guards at the bottom of this file parse it; the `rtems` and
/// `exec_backend` arms are the production use.
#[cfg(any(exec_backend, test))]
mod demo_db {
    /// The database loaded when no `.db` file is given on the command line —
    /// small enough to run on a bare target, wide enough to exercise the
    /// scalar read, write and monitor paths over CA.
    ///
    /// IOC content only, and **link-free** by construction: no record here
    /// carries an INP/OUT field, so a default build is the no-`ca://`-link
    /// image. The C6 measurement rig — every probe record and both probe
    /// threads — is behind the `bringup-probes` feature (`C6_PROBE_DB`).
    pub const DEMO_DB: &str = concat!(
        "record(ao, \"RTEMS:AO\") { field(VAL, \"1.5\") field(PREC, \"3\") field(EGU, \"V\") }\n",
        "record(longout, \"RTEMS:LO\") { field(VAL, \"7\") field(EGU, \"counts\") }\n",
        "record(stringout, \"RTEMS:MSG\") { field(VAL, \"realtime-ca-ioc\") }\n",
        // The binary-output path, and with it the only field in the demo
        // database that arms a delayed callback: `bo` `HIGH` is the
        // momentary one-shot (`boRecord.c:257-262`). A `caput RTEMS:BO.HIGH
        // 1e300` followed by `caput RTEMS:BO 1` is the repro
        // `runtime::time::deadline_after` was filed against — it is the only
        // way to make the exec backend's timer take a `Duration::MAX`
        // deadline from a target CA client, and the panic it closes was
        // never reachable from `DEMO_DB` before.
        "record(bo, \"RTEMS:BO\") { field(ZNAM, \"Off\") field(ONAM, \"On\") }\n",
    );

    /// C6 PROBE (topology A): the 14 measurement-rig records, loaded on top of
    /// [`DEMO_DB`] only under the `bringup-probes` feature (§11.7 item 3 —
    /// they are the measurement rig, not IOC content).
    ///
    /// They live in a compiled-in constant rather than in a `.db` file
    /// because a `-kernel` boot has no filesystem to name one on and
    /// `csrc/rtems_init.c:331` hands `main` a fixed one-element argv — the same
    /// forcing the pvalink probe recorded.
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

    /// E8 STACK PROBE: a record set whose CA traffic is *not* a single scalar.
    ///
    /// A 13,240 B `CAS-client` high-water was all that could be measured,
    /// because every driver in that round did `READ_NOTIFY` against one `ao`. That number cannot decide
    /// `StackSizeClass`: it is the depth of the shallowest possible CA request.
    /// These records are chosen so each one drives a different part of the
    /// server thread's stack, and the census then says which of them, if any,
    /// approaches a class boundary:
    ///
    ///   * `WF` / `WF2` — 32,768 and 8,192 `DOUBLE`, so a `READ_NOTIFY` reply
    ///     is 262,144 B and 65,536 B of payload rather than 8. Array
    ///     serialisation is what `EPICS_CA_MAX_ARRAY_BYTES` is about, and it
    ///     runs on the `CAS-client` thread for a get and on `CAS-event` for a
    ///     monitor.
    ///   * `SA` — a `subArray` window over `WF`, so the reply is built through
    ///     `ArrayKind::SubArray`'s INDX/NELM path rather than by copying a
    ///     whole field.
    ///   * `CMP` — a `compress` whose INP is `WF`, so processing consumes a
    ///     32,768-element array instead of a scalar.
    ///   * `H` → `L1..L32` — a 32-deep `FLNK` chain, four times the existing
    ///     C1..C8 one, so the chain is longer than any depth bound the engine
    ///     imposes and the bound itself becomes observable. MEASURED on
    ///     `x86_64-wrs-vxworks`: a CA put to `H` processes `H` and `L1..L15`
    ///     and stops, because `process_entry_prelude`'s `MAX_LINK_DEPTH = 16`
    ///     bails at `L16` (`L15 = 1215.0`, `L16 = L17 = L18 = 0.0`). So this
    ///     record set drives the recursion to its cap, which is what makes the
    ///     `CAS-client` high-water below depth-inclusive.
    ///   * `WFBIG` — 131,072 `DOUBLE`, a 1,048,576 B reply, 4× `WF`. `WF`
    ///     alone cannot say whether stack high-water scales with payload
    ///     size; two array sizes an octave apart can.
    ///   * `FAN` — a `dfanout` to 8 targets, breadth rather than depth, so the
    ///     two shapes can be told apart.
    ///
    /// Kept separate from [`C6_PROBE_DB`] rather than appended to it because
    /// that constant's record count is asserted by another row's test.
    #[cfg(feature = "bringup-probes")]
    pub const E8_STACK_DB: &str = concat!(
        "record(waveform, \"RTEMS:E8:WF\") { field(FTVL, \"DOUBLE\") field(NELM, \"32768\") }\n",
        "record(waveform, \"RTEMS:E8:WF2\") { field(FTVL, \"DOUBLE\") field(NELM, \"8192\") }\n",
        "record(waveform, \"RTEMS:E8:WFBIG\") { field(FTVL, \"DOUBLE\") field(NELM, \"131072\") }\n",
        "record(subArray, \"RTEMS:E8:SA\") {\n",
        "  field(FTVL, \"DOUBLE\") field(INP, \"RTEMS:E8:WF\")\n",
        "  field(MALM, \"32768\") field(NELM, \"4096\") field(INDX, \"1024\")\n",
        "}\n",
        "record(compress, \"RTEMS:E8:CMP\") {\n",
        "  field(ALG, \"Circular Buffer\") field(NSAM, \"256\") field(INP, \"RTEMS:E8:WF\")\n",
        "}\n",
        // The 32-deep chain. `H` is an `ao` so a plain `caput` starts it.
        "record(ao, \"RTEMS:E8:H\") { field(PREC, \"3\") field(FLNK, \"RTEMS:E8:L1\") }\n",
        "record(calc, \"RTEMS:E8:L1\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:H\") field(FLNK, \"RTEMS:E8:L2\") }\n",
        "record(calc, \"RTEMS:E8:L2\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L1\") field(FLNK, \"RTEMS:E8:L3\") }\n",
        "record(calc, \"RTEMS:E8:L3\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L2\") field(FLNK, \"RTEMS:E8:L4\") }\n",
        "record(calc, \"RTEMS:E8:L4\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L3\") field(FLNK, \"RTEMS:E8:L5\") }\n",
        "record(calc, \"RTEMS:E8:L5\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L4\") field(FLNK, \"RTEMS:E8:L6\") }\n",
        "record(calc, \"RTEMS:E8:L6\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L5\") field(FLNK, \"RTEMS:E8:L7\") }\n",
        "record(calc, \"RTEMS:E8:L7\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L6\") field(FLNK, \"RTEMS:E8:L8\") }\n",
        "record(calc, \"RTEMS:E8:L8\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L7\") field(FLNK, \"RTEMS:E8:L9\") }\n",
        "record(calc, \"RTEMS:E8:L9\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L8\") field(FLNK, \"RTEMS:E8:L10\") }\n",
        "record(calc, \"RTEMS:E8:L10\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L9\") field(FLNK, \"RTEMS:E8:L11\") }\n",
        "record(calc, \"RTEMS:E8:L11\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L10\") field(FLNK, \"RTEMS:E8:L12\") }\n",
        "record(calc, \"RTEMS:E8:L12\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L11\") field(FLNK, \"RTEMS:E8:L13\") }\n",
        "record(calc, \"RTEMS:E8:L13\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L12\") field(FLNK, \"RTEMS:E8:L14\") }\n",
        "record(calc, \"RTEMS:E8:L14\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L13\") field(FLNK, \"RTEMS:E8:L15\") }\n",
        "record(calc, \"RTEMS:E8:L15\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L14\") field(FLNK, \"RTEMS:E8:L16\") }\n",
        "record(calc, \"RTEMS:E8:L16\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L15\") field(FLNK, \"RTEMS:E8:L17\") }\n",
        "record(calc, \"RTEMS:E8:L17\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L16\") field(FLNK, \"RTEMS:E8:L18\") }\n",
        "record(calc, \"RTEMS:E8:L18\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L17\") field(FLNK, \"RTEMS:E8:L19\") }\n",
        "record(calc, \"RTEMS:E8:L19\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L18\") field(FLNK, \"RTEMS:E8:L20\") }\n",
        "record(calc, \"RTEMS:E8:L20\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L19\") field(FLNK, \"RTEMS:E8:L21\") }\n",
        "record(calc, \"RTEMS:E8:L21\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L20\") field(FLNK, \"RTEMS:E8:L22\") }\n",
        "record(calc, \"RTEMS:E8:L22\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L21\") field(FLNK, \"RTEMS:E8:L23\") }\n",
        "record(calc, \"RTEMS:E8:L23\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L22\") field(FLNK, \"RTEMS:E8:L24\") }\n",
        "record(calc, \"RTEMS:E8:L24\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L23\") field(FLNK, \"RTEMS:E8:L25\") }\n",
        "record(calc, \"RTEMS:E8:L25\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L24\") field(FLNK, \"RTEMS:E8:L26\") }\n",
        "record(calc, \"RTEMS:E8:L26\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L25\") field(FLNK, \"RTEMS:E8:L27\") }\n",
        "record(calc, \"RTEMS:E8:L27\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L26\") field(FLNK, \"RTEMS:E8:L28\") }\n",
        "record(calc, \"RTEMS:E8:L28\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L27\") field(FLNK, \"RTEMS:E8:L29\") }\n",
        "record(calc, \"RTEMS:E8:L29\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L28\") field(FLNK, \"RTEMS:E8:L30\") }\n",
        "record(calc, \"RTEMS:E8:L30\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L29\") field(FLNK, \"RTEMS:E8:L31\") }\n",
        "record(calc, \"RTEMS:E8:L31\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L30\") field(FLNK, \"RTEMS:E8:L32\") }\n",
        "record(calc, \"RTEMS:E8:L32\") { field(CALC, \"A+1\") field(INPA, \"RTEMS:E8:L31\") }\n",
        // Breadth, to tell a wide fan-out apart from a deep chain.
        "record(dfanout, \"RTEMS:E8:FAN\") {\n",
        "  field(OUTA, \"RTEMS:E8:F1\") field(OUTB, \"RTEMS:E8:F2\")\n",
        "  field(OUTC, \"RTEMS:E8:F3\") field(OUTD, \"RTEMS:E8:F4\")\n",
        "  field(OUTE, \"RTEMS:E8:F5\") field(OUTF, \"RTEMS:E8:F6\")\n",
        "  field(OUTG, \"RTEMS:E8:F7\") field(OUTH, \"RTEMS:E8:F8\")\n",
        "}\n",
        "record(ai, \"RTEMS:E8:F1\") { field(PREC, \"3\") }\n",
        "record(ai, \"RTEMS:E8:F2\") { field(PREC, \"3\") }\n",
        "record(ai, \"RTEMS:E8:F3\") { field(PREC, \"3\") }\n",
        "record(ai, \"RTEMS:E8:F4\") { field(PREC, \"3\") }\n",
        "record(ai, \"RTEMS:E8:F5\") { field(PREC, \"3\") }\n",
        "record(ai, \"RTEMS:E8:F6\") { field(PREC, \"3\") }\n",
        "record(ai, \"RTEMS:E8:F7\") { field(PREC, \"3\") }\n",
        "record(ai, \"RTEMS:E8:F8\") { field(PREC, \"3\") }\n",
    );
}

/// E8 STACK PROBE, second half: link-time interposers that name the allocation
/// behind the wall-abort mutex `EINVAL`.
///
/// The SDK's `pthreadLib.o` settles the chain statically —
/// `pthread_mutex_lock+0x34` calls `pthreadMutexInitComplete`, which calls
/// `pthreadMutexInit`, which calls `semMCreate` and, when that returns NULL,
/// returns `0x16` = `EINVAL` rather than `ENOMEM`; `InitComplete` passes the
/// code through and `pthread_mutex_lock` returns it verbatim, so std's
/// `Mutex::lock` panics with "invalid argument (os error 22)". Note that
/// `pthread_mutex_init` only *stamps* the `0xec542a37` magic and never calls
/// `semMCreate` itself, so **every** VxWorks pthread mutex materialises its
/// semaphore on first lock and an eager `init()` does not avoid this.
///
/// This proves the same chain on target. Built only when the linker is given
/// `--wrap=semMCreate --wrap=pthread_mutex_lock`; without those flags the
/// `__wrap_*` symbols are simply unreferenced.
///
/// Everything here is allocation-free and takes no std lock, because it runs
/// *inside* the failing path: a `format!` would call the allocator that is
/// already refusing, and an `eprintln!` would lock the very kind of object
/// whose creation just failed.
#[cfg(all(feature = "bringup-probes", target_os = "vxworks"))]
mod mutex_alloc_probe {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEM_OK: AtomicUsize = AtomicUsize::new(0);
    static SEM_NULL: AtomicUsize = AtomicUsize::new(0);
    static LOCK_FAIL: AtomicUsize = AtomicUsize::new(0);

    /// How many of each event to print before going quiet. A wall event can
    /// produce these in a tight loop and the console is a 115200-baud pipe.
    const REPORT_CAP: usize = 24;

    /// Fixed-buffer line writer: no allocator, no locks, one `write(2)`.
    struct Line {
        buf: [u8; 240],
        n: usize,
    }

    impl Line {
        const fn new() -> Self {
            Self {
                buf: [0; 240],
                n: 0,
            }
        }

        fn s(&mut self, t: &str) -> &mut Self {
            for &b in t.as_bytes() {
                if self.n < self.buf.len() {
                    self.buf[self.n] = b;
                    self.n += 1;
                }
            }
            self
        }

        fn d(&mut self, mut v: usize) -> &mut Self {
            let mut tmp = [0u8; 20];
            let mut i = tmp.len();
            loop {
                i -= 1;
                tmp[i] = b'0' + (v % 10) as u8;
                v /= 10;
                if v == 0 || i == 0 {
                    break;
                }
            }
            // SAFETY-free: the slice is ASCII digits by construction.
            let digits = core::str::from_utf8(&tmp[i..]).unwrap_or("?");
            self.s(digits)
        }

        fn h(&mut self, v: usize) -> &mut Self {
            self.s("0x");
            let mut started = false;
            for shift in (0..16).rev() {
                let nib = ((v >> (shift * 4)) & 0xf) as u8;
                if nib != 0 || started || shift == 0 {
                    started = true;
                    let c = if nib < 10 {
                        b'0' + nib
                    } else {
                        b'a' + nib - 10
                    };
                    if self.n < self.buf.len() {
                        self.buf[self.n] = c;
                        self.n += 1;
                    }
                }
            }
            self
        }

        fn emit(&mut self) {
            self.s("\n");
            unsafe {
                libc::write(2, self.buf.as_ptr().cast::<c_void>(), self.n);
            }
        }
    }

    unsafe extern "C" {
        fn __real_semMCreate(options: libc::c_int) -> *mut c_void;
        fn __real_pthread_mutex_lock(m: *mut libc::pthread_mutex_t) -> libc::c_int;
    }

    /// The allocation that fails. `semMCreate` returning NULL is what turns
    /// into `EINVAL` two frames up.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn __wrap_semMCreate(options: libc::c_int) -> *mut c_void {
        let sem = unsafe { __real_semMCreate(options) };
        if sem.is_null() {
            let n = SEM_NULL.fetch_add(1, Ordering::Relaxed);
            if n < REPORT_CAP {
                let mut l = Line::new();
                l.s("MTXPROBE semMCreate=NULL nth_null=")
                    .d(n + 1)
                    .s(" succeeded_before=")
                    .d(SEM_OK.load(Ordering::Relaxed))
                    .s(" options=")
                    .h(options as usize)
                    .emit();
            }
        } else {
            let ok = SEM_OK.fetch_add(1, Ordering::Relaxed) + 1;
            // A running census on a milestone rather than from a call site: the
            // console reporter lives in the rig patch, not in this file, and the
            // trajectory up to the wall is the number the reservation budget
            // needs. The first call reports unconditionally: silence must mean
            // "no semaphores created", never "the --wrap flags did not take",
            // and on the first run of this probe those two were indistinguishable.
            if ok == 1 || ok % 512 == 0 {
                let mut l = Line::new();
                l.s("MTXPROBE semaphores_created=").d(ok).emit();
            }
        }
        sem
    }

    /// The symptom. Reports the mutex address and the running semaphore count,
    /// so a failing lock can be tied to the NULL that caused it.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn __wrap_pthread_mutex_lock(
        m: *mut libc::pthread_mutex_t,
    ) -> libc::c_int {
        let rc = unsafe { __real_pthread_mutex_lock(m) };
        if rc != 0 {
            let n = LOCK_FAIL.fetch_add(1, Ordering::Relaxed);
            if n < REPORT_CAP {
                let mut l = Line::new();
                l.s("MTXPROBE lock rc=")
                    .d(rc as usize)
                    .s(" mutex=")
                    .h(m as usize)
                    .s(" nth_fail=")
                    .d(n + 1)
                    .s(" sem_ok=")
                    .d(SEM_OK.load(Ordering::Relaxed))
                    .s(" sem_null=")
                    .d(SEM_NULL.load(Ordering::Relaxed))
                    .emit();
            }
        }
        rc
    }
}

#[cfg(exec_backend)]
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
    use epics_base_rs::runtime::worker_pool::ThreadCharge;
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
    /// A constant: two of these IOCs on one subnet publish the same names and
    /// a client's SEARCH would be answered by both. The fix when that day
    /// comes is to take the prefix from the environment the way
    /// `cas_server_port` takes the port — which the boot command line can now
    /// actually set, so it is a change to this line rather than to the boot
    /// contract. Left as it is because nothing has asked for it; the note is
    /// here so the next reader does not have to rediscover why.
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

    /// C6 PROBE: `host:port` of a peer that accepts a connection and then
    /// never reads from it, for the write-deadline measurement. Empty — the
    /// default — leaves that probe unstarted, so its presence costs the outage
    /// rig nothing.
    ///
    /// Build-time like [`C6_NAME_SERVER`] and for the same reason: a target
    /// image has no configuration surface, and the peer is a host address the
    /// rig picks.
    #[cfg(feature = "bringup-probes")]
    const C6_WRITE_DEADLINE_PEER: &str = match option_env!("C6_WRITE_DEADLINE_PEER") {
        Some(s) => s,
        None => "",
    };

    /// The deadline the write-deadline probe gives one frame, and how large
    /// that frame is. The frame has to be bigger than everything between the
    /// guest's send buffer and the peer's unread receive buffer, or the write
    /// completes and the run measures nothing — a completed leg prints its
    /// elapsed time and says so, so that outcome is visible rather than
    /// mistaken for a bound.
    #[cfg(feature = "bringup-probes")]
    const C6_WRITE_DEADLINE_MS: u64 = 2_000;
    #[cfg(feature = "bringup-probes")]
    const C6_WRITE_DEADLINE_FRAME: usize = 8 * 1024 * 1024;

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
    /// in `realtime-pva-ioc` — so a second OS meant editing two binaries to say
    /// the same thing twice, and the two copies had already drifted.
    #[cfg(feature = "bringup-probes")]
    fn c6_task_and_stack_report(tag: &str) {
        epics_rtems_boot::stats::dump_tasks(tag);
        epics_rtems_boot::stats::stack_report(tag);
    }

    /// PROBE: what `getifaddrs` reports on this target, and the UDP SEARCH
    /// destination list derived from it.
    ///
    /// The gate gives `net::iface_v4` a compile for both embedded triples and
    /// `nm` gives it a defined `getifaddrs` in `libbsd.a` / `libnet.a`, but
    /// neither says the call returns an interface once the BSP's stack is up:
    /// an RTEMS image whose libbsd has no configured `ifconfig` would return
    /// success and an empty list, and `EPICS_CA_AUTO_ADDR_LIST` would expand to
    /// nothing exactly as it did before the enumeration existed — a silent
    /// no-op wearing a working build's clothes. This is the line that tells the
    /// two apart, so it prints the raw walk and not only the derived list.
    ///
    /// Before the search-configuration defaults below, so it reports what the
    /// OS has rather than what this image then decides to do with it.
    #[cfg(feature = "bringup-probes")]
    fn iface_report() {
        match epics_base_rs::net::iface_v4::enumerate() {
            Ok(ifaces) => {
                for i in &ifaces {
                    println!(
                        "IFPROBE name={} ip={} flags=0x{:x} up={} lo={} bc={} p2p={} dest={:?} search={:?}",
                        i.name,
                        i.ip,
                        i.flags,
                        i.is_up() as u8,
                        i.is_loopback() as u8,
                        i.is_broadcast() as u8,
                        i.is_point_to_point() as u8,
                        i.dest,
                        i.search_destination()
                    );
                }
                println!(
                    "IFPROBE count={} broadcast_addrs={:?} local_addr={}",
                    ifaces.len(),
                    epics_base_rs::net::iface_v4::broadcast_addrs(),
                    epics_base_rs::net::iface_v4::local_addr()
                );
            }
            // Printed, not panicked: an image that cannot enumerate is still a
            // working IOC for every explicitly configured destination, and the
            // whole point of the probe is to report the difference.
            Err(e) => println!("IFPROBE error={e}"),
        }
    }

    /// UDP SEARCH PROBE: broadcast one real CA SEARCH from the client's own
    /// socket type and wait for this IOC's own responder to answer it.
    ///
    /// The four things nothing else on this target proves, in one round trip:
    /// [`SearchUdpSocket`](epics_base_rs::net::search_udp::SearchUdpSocket)
    /// **binds**, its `send_to` reaches the broadcast address
    /// [`iface_report`] derived, the responder receives it, and the receive
    /// pump thread hands the reply back to an `async fn` running with no
    /// reactor. A cross-compile gate proves none of them — `std::net::UdpSocket`
    /// on `armv7-rtems-eabihf` type-checks whatever libbsd then does.
    ///
    /// Deliberately its own socket rather than the search engine's: this must
    /// report on the transport alone, and a failure here must not be
    /// confusable with a database, resolver or name-server fault. It runs
    /// after the responder thread is up, because it needs an answer.
    #[cfg(feature = "bringup-probes")]
    fn udp_search_report(port: u16, pv: &str) {
        use epics_base_rs::net::search_udp::SearchUdpSocket;
        use epics_base_rs::runtime::ioc_role::IocRole;
        use epics_base_rs::runtime::task::{block_on_sync, timeout};
        use epics_ca_rs::protocol::{
            CA_DO_REPLY, CA_MINOR_VERSION, CA_PROTO_SEARCH, CA_PROTO_VERSION, CaHeader,
        };
        use std::net::SocketAddr;
        use std::time::Duration;

        // The pump's band comes from the role, not from a number named here:
        // this probe is a `ConsoleCensus` instrument and must not preempt
        // client service, which is the rule
        // `this_entry_point_names_no_scheduling_band` enforces.
        let sock =
            match SearchUdpSocket::bind_ephemeral(true, "PROBE-UDP", IocRole::ConsoleCensus.band())
            {
                Ok(s) => s,
                Err(e) => {
                    println!("UDPSEARCH bind error={e}");
                    return;
                }
            };
        println!("UDPSEARCH bound={:?}", sock.local_addrs());

        // VERSION then SEARCH, the two-message datagram every CA client opens
        // with (`udpiiu.cpp::searchMsg`). Built here from `CaHeader::to_bytes`
        // rather than through the search engine, for the reason above.
        let mut frame = CaHeader {
            cmmd: CA_PROTO_VERSION,
            count: CA_MINOR_VERSION,
            ..CaHeader::new(CA_PROTO_VERSION)
        }
        .to_bytes()
        .to_vec();
        let mut name = pv.as_bytes().to_vec();
        name.push(0);
        while !name.len().is_multiple_of(8) {
            name.push(0);
        }
        frame.extend_from_slice(
            &CaHeader {
                cmmd: CA_PROTO_SEARCH,
                postsize: name.len() as u16,
                data_type: CA_DO_REPLY,
                count: CA_MINOR_VERSION,
                cid: 0xC0DE,
                available: 0xC0DE,
                ..CaHeader::new(CA_PROTO_SEARCH)
            }
            .to_bytes(),
        );
        frame.extend_from_slice(&name);

        let dests = epics_base_rs::net::iface_v4::broadcast_addrs();
        if dests.is_empty() {
            println!("UDPSEARCH no broadcast destination — nothing to send to");
            return;
        }
        let outcome = block_on_sync(async {
            for ip in &dests {
                let dest = SocketAddr::from((*ip, port));
                match sock.send_to(&frame, dest).await {
                    Ok(n) => println!("UDPSEARCH sent bytes={n} pv={pv} dest={dest}"),
                    Err(e) => println!("UDPSEARCH send error={e} dest={dest}"),
                }
            }
            let mut buf = vec![0u8; 1024];
            match timeout(Duration::from_secs(5), sock.recv(&mut buf)).await {
                Ok(Ok(dg)) => format!(
                    "UDPSEARCH reply n={} src={} iface_ip={:?} drops={}",
                    dg.n, dg.src, dg.iface_ip, dg.drops
                ),
                Ok(Err(e)) => format!("UDPSEARCH recv error={e}"),
                Err(_) => "UDPSEARCH no reply within 5s".to_string(),
            }
        })
        .expect("the RTEMS entry point runs on a plain thread with no runtime entered");
        println!("{outcome}");
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
        // Each field carries its own -1: a target can measure one and not the
        // other, and blanking a reading it has because of one it has not is
        // how a probe line loses the number the run was for.
        let mem = epics_rtems_boot::stats::mem_usage();
        let mem_free = mem.free.map_or(-1, |v| v as i64);
        let mem_used = mem.used.map_or(-1, |v| v as i64);
        println!(
            "C6 seq={seq} dialpool workers={dial_workers} attempts={dial_attempts} \
             queued={dial_queued} dialing={dial_dialing} \
             MEM_FREE={mem_free} MEM_USED={mem_used}",
        );
        // The monitor queue's collapse counter, beside MEM_USED because the two
        // answer one question together: whether a wide-value monitor is being
        // held to one queued entry (C's `db_queue_event_log` early-drop for a
        // second by-reference log, `dbEvent.c:794-800`) or is accumulating whole
        // owned array copies. A rising count with a flat MEM_USED is the bound
        // working; a flat count with a climbing MEM_USED is not.
        println!(
            "MONPROBE seq={seq} COLLAPSED={}",
            epics_base_rs::server::pv::dropped_monitor_events()
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

    /// C6 PROBE: is a frame write bounded on a target with no `SO_SNDTIMEO`?
    ///
    /// Two legs against the same never-reading peer inside one boot, so the
    /// stack, the buffers and the clock are identical between them:
    ///
    /// * **control** — `set_write_timeout` then `write_all`, which is what the
    ///   writer pump did while the deadline depended on that socket option
    ///   taking. On a target that refuses the option this thread has nothing
    ///   entitled to reclaim it.
    /// * **fixed** — `write_frame_deadline`, which owns the socket's blocking
    ///   mode and polls `POLLOUT` for what the deadline has left, so it cannot
    ///   be disarmed by the option being absent — nor by a send flag the target
    ///   ignores, which is what `MSG_DONTWAIT` turned out to be on Darwin.
    ///
    /// Both legs print their elapsed time and their outcome. A leg that
    /// *completes* says so: silence is never read as a bound here, and a frame
    /// small enough to fit in the path's buffers would show up as a fast
    /// `Ok(())` rather than as a passing measurement.
    #[cfg(feature = "bringup-probes")]
    fn c6_write_deadline_probe() {
        use std::io::Write as _;
        use std::sync::atomic::{AtomicBool, Ordering};

        let peer = C6_WRITE_DEADLINE_PEER;
        let send_timeout = std::time::Duration::from_millis(C6_WRITE_DEADLINE_MS);
        println!(
            "WDPROBE begin peer={peer} frame_bytes={C6_WRITE_DEADLINE_FRAME} \
             send_timeout_ms={C6_WRITE_DEADLINE_MS}"
        );
        let started = Instant::now();

        // ---- control leg ------------------------------------------------
        let ctl_returned = Arc::new(AtomicBool::new(false));
        match std::net::TcpStream::connect(peer) {
            Ok(sock) => {
                // The option this whole item is about, read on the target
                // rather than assumed from the header.
                match sock.set_write_timeout(Some(send_timeout)) {
                    Ok(()) => println!("WDPROBE ctl so_sndtimeo=ok"),
                    Err(e) => println!(
                        "WDPROBE ctl so_sndtimeo=error kind={:?} os={:?} msg={e}",
                        e.kind(),
                        e.raw_os_error()
                    ),
                }
                let flag = ctl_returned.clone();
                let charge = ThreadCharge::fixed(StackSizeClass::Medium);
                match thread::Builder::new()
                    .name("c6-wd-ctl".to_string())
                    .stack_size(StackSizeClass::Medium.bytes())
                    .spawn(move || {
                        let _charge = charge;
                        let _ = epics_base_rs::runtime::ioc_role::enter_ioc_role(
                            epics_base_rs::runtime::ioc_role::IocRole::ConsoleCensus,
                        );
                        let mut sock = sock;
                        let frame = vec![0u8; C6_WRITE_DEADLINE_FRAME];
                        let leg = Instant::now();
                        let outcome = sock
                            .write_all(&frame)
                            .map_err(|e| (e.kind(), e.raw_os_error()));
                        flag.store(true, Ordering::SeqCst);
                        println!(
                            "WDPROBE ctl returned elapsed_ms={} outcome={outcome:?}",
                            leg.elapsed().as_millis()
                        );
                    }) {
                    Ok(_) => println!("WDPROBE ctl writing"),
                    Err(e) => println!("WDPROBE ctl thread refused: {e}"),
                }
            }
            Err(e) => println!(
                "WDPROBE ctl connect failed kind={:?} os={:?} msg={e}",
                e.kind(),
                e.raw_os_error()
            ),
        }

        // ---- fixed leg ---------------------------------------------------
        // Its own connection, so the control leg's queued bytes are not part
        // of what it measures.
        thread::sleep(std::time::Duration::from_secs(5));
        match std::net::TcpStream::connect(peer) {
            Ok(sock) => {
                let frame = vec![0u8; C6_WRITE_DEADLINE_FRAME];
                let leg = Instant::now();
                let outcome = epics_base_rs::runtime::blocking_io::write_frame_deadline(
                    &sock,
                    &frame,
                    send_timeout,
                )
                .map_err(|e| (e.kind(), e.raw_os_error()));
                println!(
                    "WDPROBE fix returned elapsed_ms={} outcome={outcome:?}",
                    leg.elapsed().as_millis()
                );
            }
            Err(e) => println!(
                "WDPROBE fix connect failed kind={:?} os={:?} msg={e}",
                e.kind(),
                e.raw_os_error()
            ),
        }

        // ---- the control leg, kept under measurement ----------------------
        for _ in 0..12 {
            thread::sleep(std::time::Duration::from_secs(10));
            println!(
                "WDPROBE mark t_ms={} ctl_returned={}",
                started.elapsed().as_millis(),
                ctl_returned.load(Ordering::SeqCst)
            );
        }
        println!(
            "WDPROBE end t_ms={} ctl_returned={}",
            started.elapsed().as_millis(),
            ctl_returned.load(Ordering::SeqCst)
        );
    }

    /// Load the database: every command-line argument is a `.db` file path
    /// (loaded in order, C `dbLoadRecords`), or the built-in demo database
    /// when there are none.
    ///
    /// Which of the two it was is printed, and that is the point of the print
    /// rather than a courtesy. A boot whose database argument never arrived —
    /// a DHCP command line the shim refused as oversized, a token past
    /// `EPICS_RTEMS_MAX_BOOT_ARGS`, a path spelled so it parsed as an
    /// assignment — serves the demo PVs instead of the site's, and from
    /// outside that is indistinguishable from a healthy IOC serving the wrong
    /// records. The console is this target's only report, so the substitution
    /// has to name itself there. A named file that fails to load is already an
    /// error and stays one: falling back to the demo database on a load
    /// failure would be the same defect with a louder cause.
    ///
    /// Driven by `block_on_sync`, which parks this thread between polls of the
    /// build future — `IocBuilder::build` awaits only in-process locks, so no
    /// reactor is involved.
    fn load_database(db_files: &[String]) -> CaResult<Arc<PvDatabase>> {
        let macros = HashMap::new();
        let mut builder = IocBuilder::new();
        if db_files.is_empty() {
            println!(
                "realtime-ca-ioc: no database named on the boot command line; \
                 loading the BUILT-IN DEMO database. A site database is named \
                 by putting its path on that line."
            );
            builder = builder.db_string(DEMO_DB, &macros)?;
            // C6 PROBE records ride along whenever the built-in database is
            // the source — a bare `-kernel` boot cannot choose — but only on
            // a build that asked for the measurement rig.
            #[cfg(feature = "bringup-probes")]
            {
                builder = builder.db_string(crate::demo_db::C6_PROBE_DB, &macros)?;
                builder = builder.db_string(crate::demo_db::E8_STACK_DB, &macros)?;
            }
        } else {
            println!(
                "realtime-ca-ioc: loading {} database file(s) from the boot \
                 command line: {}",
                db_files.len(),
                db_files.join(" ")
            );
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

        // (0-env) The boot command line — this target's st.cmd. Its
        //     `NAME=VALUE` tokens are `epicsEnvSet`, and they are applied here,
        //     first, before anything in this image reads the environment: the
        //     console subscriber, the CA server's port and interface selection
        //     and the calink resolver all read theirs once, and a variable that
        //     arrived after its reader is a variable the site did not set.
        //
        //     C reaches the same state through iocsh running the startup script
        //     the BOOTP command line names
        //     ($EPICS_BASE/modules/libcom/RTEMS/posix/rtems_init.c:103,256,1184
        //     at R7.0.10); we have no script and no filesystem to hold one, so
        //     the line itself carries the assignments. `epics-rtems-boot` owns
        //     the rule so this binary and `realtime-pva-ioc` cannot come to
        //     disagree about it.
        //
        //     SAFETY (edition 2024): the entry thread, before `background_init`
        //     or any server has started anything, so no concurrent reader or
        //     writer of the environment exists.
        let boot_args: Vec<String> =
            unsafe { epics_rtems_boot::boot_args::apply_boot_env(std::env::args().skip(1)) };

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

        // (0-cmd) The RTEMS operator commands — C `iocshRegisterRTEMS`
        //     ($EPICS_BASE/modules/libcom/RTEMS/posix/rtems_init.c:692-705 at
        //     R7.0.10). C registers `netstat`, `heapSpace`, `zoneset`, `rt` and
        //     `setlogmask` from its RTEMS boot path, so an IOC has them because
        //     it booted on RTEMS; this is that boot path, so this is where they
        //     are registered.
        //
        //     `cfg!` and not `#[cfg]`: the call compiles on every target this
        //     binary builds for, and the condition is C's own — `iocshRegisterRTEMS`
        //     exists only in the RTEMS build of libCom, so a host or VxWorks
        //     image must not grow the names.
        //
        //     KNOWN, and not a defect in the registration: this target has no
        //     iocsh (`rustyline` does not build for RTEMS, see above), so
        //     nothing reads the table here yet. The commands are held to C's
        //     names, arities and output by this workspace's host tests; a shell
        //     on the target is what turns them into something an operator can
        //     type.
        if cfg!(target_os = "rtems") {
            epics_base_rs::server::iocsh::register_rtems_commands();
        }

        // (0-iface) PROBE: what the OS's interface enumeration returns, before
        //     anything in this image decides what to do with it.
        #[cfg(feature = "bringup-probes")]
        iface_report();

        // (0-probe) C6 PROBE: the target's CA search configuration. §4.5's
        // three variables, together: the name server to dial, and the two that
        // shut the broadcast path off so a resolution can only have come over
        // TCP.
        //
        // Defaults, not overrides: a variable that is already set stays as
        // set. That now means the same thing on the target as on a host —
        // step (0-env) above has already applied the boot command line, so an
        // `EPICS_CA_NAME_SERVERS=...` boot argument wins over the value
        // compiled in here, and `tests/realtime_ca_ioc_boots.rs` points the
        // dial at its own closed port the same way.
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
        // The board's epoch is fixed for the life of the boot, so this is the
        // one moment the message can be acted on. Every stamp served past the
        // EPICS time stamp's range wraps exactly as C's `epicsTimeFromTime_t`
        // wraps it; saying so per read would be noise and refusing per read
        // would take the IOC off the air for a condition it cannot fix.
        if let Some(why) =
            epics_base_rs::types::wall_clock_range_warning(std::time::SystemTime::now())
        {
            eprintln!("epics-rs: {why}");
        }

        // (1) C `callbackInit` (callback.c:286) — the callback pool, delayed
        //     timer and scanOnce worker exist before any record can defer a
        //     tail into them. Idempotent.
        background_init();

        // (2) The database — every boot argument that was not an
        //     `epicsEnvSet` is a `.db` to load, in order (C `dbLoadRecords`).
        let db = match load_database(&boot_args) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("realtime-ca-ioc: iocInit failed: {e}");
                return ExitCode::FAILURE;
            }
        };

        // (2b) The calink resolver: install the `ca://` record-link resolver on
        //      the database so ` CA`-modified / `ca://...` INP/OUT fields
        //      resolve. C reaches the same state through `dbCaLinkInit`
        //      (`dbCa.c:352-355`) during iocInit, after the database exists.
        //      Installed here — after the database, before the CA front-end —
        //      so every record's link fields route through it before the
        //      server answers its first client.
        //
        //      The CA client reaches upstream servers the way a host client
        //      does — the UDP SEARCH socket plus any `EPICS_CA_NAME_SERVERS`
        //      circuits — and every task the resolver spawns lands on the
        //      callback pool `background_init` started above via
        //      `runtime::task::spawn`, never a tokio runtime.
        let resolver = block_on_sync(install_calink_resolver(&db))
            .expect("the RTEMS entry point runs on a plain thread with no runtime entered");

        // (2c) iocInit's link phases, in `IocApplication::run`'s order
        //      (`ioc_app.rs:913-925`) and for its reasons. Without them the
        //      resolver above is mounted and unreachable:
        //
        //      * `initialize_link_locality` commits C `dbInitLink`'s locality
        //        decision (`dbLink.c:118-129` falling through
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
        //      `realtime-pva-ioc` needs no equivalent: `install_pvalink_resolver`
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
        //     value, as under a C IOC (caservertask.c:492-500). No ACF is
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
                eprintln!("realtime-ca-ioc: cannot start the CA TCP server on port {port}: {e}");
                return ExitCode::FAILURE;
            }
        };
        let udp = match bind_udp_search(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "realtime-ca-ioc: cannot start the CA UDP search responder on port {port}: {e}"
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
        //      "no PVA clients" when it means "no PVA server". `realtime-pva-ioc`
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
            eprintln!("realtime-ca-ioc: cannot register the status PVs: {e}");
            return ExitCode::FAILURE;
        }

        // Listed after the status PVs are registered, so the console names
        // everything a client can reach rather than only what the `.db`
        // carried.
        let mut names = block_on_sync(db_for_names.all_record_names())
            .expect("the RTEMS entry point runs on a plain thread with no runtime entered");
        names.sort();

        let srv_tcp = server.clone();
        // The stack class states what this thread takes; the charge is what
        // makes the worker pool's budget know it was taken. Held inside the
        // body, so it is released by the thread ending and by nothing else,
        // and a `spawn` that fails drops the closure and gives it back.
        let charge = ThreadCharge::fixed(StackSizeClass::Medium);
        let tcp_thread = match thread::Builder::new()
            .name("CAS-TCP".to_string())
            // caservertask.c:717-719 — `epicsThreadStackMedium`. It accepts and
            // hands off; the per-client thread is where the depth is.
            .stack_size(StackSizeClass::Medium.bytes())
            .spawn(move || {
                let _charge = charge;
                srv_tcp.serve()
            }) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("realtime-ca-ioc: cannot start the CA accept thread: {e}");
                return ExitCode::FAILURE;
            }
        };
        let srv_udp = server.clone();
        let charge = ThreadCharge::fixed(StackSizeClass::Medium);
        let udp_thread = match thread::Builder::new()
            .name("CAS-UDP".to_string())
            // caservertask.c:723-725 — `epicsThreadStackMedium`, same as TCP.
            .stack_size(StackSizeClass::Medium.bytes())
            .spawn(move || {
                let _charge = charge;
                srv_udp.serve_udp_search(udp)
            }) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("realtime-ca-ioc: cannot start the CA name-search thread: {e}");
                return ExitCode::FAILURE;
            }
        };

        println!(
            "realtime-ca-ioc: serving {} records on CA port {port} (TCP + UDP search), \
             RTEMS execution model, no tokio runtime",
            names.len()
        );

        // (5-udp) PROBE: the client's SEARCH socket, end to end against this
        //     IOC's own responder. After the responder thread above, because
        //     it needs an answer.
        #[cfg(feature = "bringup-probes")]
        udp_search_report(port, "RTEMS:AO");
        // The calink resolver is mounted, so ` CA`-modified and `ca://...`
        // INP/OUT links resolve. Reported at every boot — including
        // `link_count == 0`, when the loaded database configured none —
        // because on a target with no shell the console is the only place an
        // operator can confirm the resolver came up and how many links it
        // registered. The client reaches upstream servers over both paths a
        // host client uses — the UDP SEARCH socket and any
        // `EPICS_CA_NAME_SERVERS` TCP circuits. A `bringup-probes` build
        // still resolves over TCP alone in practice, because it compiles in
        // `EPICS_CA_ADDR_LIST=""` and `EPICS_CA_AUTO_ADDR_LIST=NO` below so
        // that a C6 resolution can only have come over TCP; that is this
        // measurement rig's configuration, not the target's capability.
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
            "realtime-ca-ioc: calink resolver installed — {link_count}/{} ca:// record \
             link{} registered ({}); ` CA`-modified and ca://... INP/OUT resolve over \
             UDP search and any EPICS_CA_NAME_SERVERS TCP circuits)",
            declared.len(),
            if declared.len() == 1 { "" } else { "s" },
            if declared.is_empty() {
                "none declared".to_string()
            } else {
                declared.join(", ")
            },
        );
        for name in &names {
            println!("realtime-ca-ioc: {name}");
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
            let tick_reactor = epics_base_rs::runtime::task::Reactor::current()
                .expect("the exec backend's executor is process-global");
            tick_reactor.spawn(async move {
                let mut n = 0i64;
                loop {
                    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(
                        C6_TICK_PERIOD_MS,
                    ))
                    .await;
                    n += 1;
                    // The `dbPutField` shape (fire-and-forget), not `put_pv`:
                    // an ai's VAL is `pp(TRUE)`, so a bare `dbPut` suppresses
                    // its immediate monitor post (dbAccess.c:1411-1413) and
                    // leaves the record's UDF *alarm* standing — a host
                    // `camonitor` on the tick saw one line, forever. C driver
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
        //     used: one line group
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
            let charge = ThreadCharge::fixed(StackSizeClass::Medium);
            match thread::Builder::new()
                .name("c6-probe".to_string())
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

            // The write-deadline measurement, only when the rig named a peer
            // for it. Its own thread: both legs are meant to block, and the
            // reporter above must keep printing while they do.
            if !C6_WRITE_DEADLINE_PEER.is_empty() {
                let charge = ThreadCharge::fixed(StackSizeClass::Medium);
                match thread::Builder::new()
                    .name("c6-wd-probe".to_string())
                    .stack_size(StackSizeClass::Medium.bytes())
                    .spawn(move || {
                        let _charge = charge;
                        let _ = epics_base_rs::runtime::ioc_role::enter_ioc_role(
                            epics_base_rs::runtime::ioc_role::IocRole::ConsoleCensus,
                        );
                        c6_write_deadline_probe();
                    }) {
                    Ok(_) => {}
                    Err(e) => eprintln!("C6 probe: cannot start the write-deadline probe: {e}"),
                }
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
                eprintln!("realtime-ca-ioc: name-search responder failed: {e}");
                ExitCode::FAILURE
            }
            Err(_) => {
                eprintln!("realtime-ca-ioc: name-search thread panicked");
                ExitCode::FAILURE
            }
        }
    }
}

#[cfg(exec_backend)]
fn main() -> ExitCode {
    ioc::main()
}

#[cfg(tokio_backend)]
fn main() -> ExitCode {
    eprintln!(
        "realtime-ca-ioc: built with the tokio task backend, which this entry point \
         does not start a runtime for.\n\
         Build it for `armv7-rtems-eabihf` or a VxWorks target, or on a host \
         rebuild with EPICS_RS_BUILD_EXEC_BACKEND=thread."
    );
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use source_guard::{Comments, production};

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
        let src = include_str!("realtime-ca-ioc.rs");
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

    /// Every thread this entry point starts is charged to the process account.
    ///
    /// A stack class alone is not enough, and
    /// `every_ca_server_thread_states_a_stack_size` in `server/blocking.rs` is
    /// the guard that already covers the class half. The class says how much
    /// this thread takes; the charge is what makes the worker pool's budget
    /// know it was taken. Without it the pool admits clients against a number
    /// that has never heard of the acceptor, the name-search responder or the
    /// probe — roughly 15 MiB of fixed IOC threads measured on the VxWorks
    /// target, so the refusal lands later than the budget says it will.
    ///
    /// The charge must be held *inside* the spawned body, not by the spawning
    /// function: a guard left on the calling thread is released the moment
    /// `main` walks past the `spawn`, while the thread it paid for runs for the
    /// life of the IOC. That is the shape this guard pins.
    ///
    /// Fails today, on Linux, with no cross toolchain.
    #[test]
    fn every_thread_here_is_charged_to_the_process_account() {
        let src = include_str!("realtime-ca-ioc.rs");
        let prod = production(src, Comments::Keep);
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
            sites, 5,
            "expected the accept thread, the UDP responder, the bring-up \
             probe and the write-deadline probe's two legs, found {sites} — \
             a thread was added or moved, and the account has to be told \
             about it"
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
    /// The bring-up probe named `ThreadPriority::Low` here until the ramp
    /// measurement showed it emitting 4 ticks in 444 s instead of ~44 while
    /// the CA server ran at `CaServerLow-4..CaServerLow`. The serving threads
    /// take their bands from `epics_ca_rs::server::blocking`'s named
    /// constants, which is why they were never the ones that stopped.
    #[test]
    fn this_entry_point_names_no_scheduling_band() {
        let src = include_str!("realtime-ca-ioc.rs");
        let prod = production(src, Comments::Keep);
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
        let src = include_str!("realtime-ca-ioc.rs");
        let prod = production(src, Comments::Keep);
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
    /// The CA counterpart of `realtime-pva-ioc`'s
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
        let src = include_str!("realtime-ca-ioc.rs");
        let prod = production(src, Comments::Keep);
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
        let src = include_str!("realtime-ca-ioc.rs");
        let prod = production(src, Comments::Keep);
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
        let src = include_str!("realtime-ca-ioc.rs");
        let prod = production(src, Comments::Keep);
        assert!(
            prod.contains(concat!("install_panic_", "hook()")),
            "this entry point installs no panic hook, so a panicking worker \
             thread leaves an IOC that still looks healthy from the network"
        );
    }

    /// The default binary is IOC content only: no C6 measurement-rig records,
    /// and no link fields at all, so a default image holds zero client-side
    /// descriptors
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
            vec!["RTEMS:AO", "RTEMS:LO", "RTEMS:MSG", "RTEMS:BO"],
            "the default database is the four demo records and nothing else"
        );
        for rec in &recs {
            assert!(
                !rec.name.starts_with("RTEMS:CA:"),
                "{} is a C6 probe record; the measurement rig belongs behind \
                 `bringup-probes`, not in the default image",
                rec.name
            );
            for field in rec.fields.iter().map(|f| &f.name) {
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
        let src = include_str!("realtime-ca-ioc.rs");
        let prod = production(src, Comments::Keep);
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

    /// Same guard for the E8 stack-probe set. It is a second constant with a
    /// second load site, so the C6 gate above does not cover it.
    #[test]
    fn the_e8_stack_db_loads_only_behind_the_feature() {
        let src = include_str!("realtime-ca-ioc.rs");
        let prod = production(src, Comments::Keep);
        let load_at = prod
            .find(concat!("crate::demo_db::E8_STACK_", "DB, &macros"))
            .expect("the E8 stack database is loaded somewhere in load_database");
        let before = &prod[load_at.saturating_sub(400)..load_at];
        assert!(
            before.contains(concat!("#[cfg(feature = \"bringup-", "probes\")]")),
            "the E8_STACK_DB load site is not feature-gated: the measurement \
             rig would ship in the default image"
        );
    }

    /// The E8 set exists to make a CA request that is not a single scalar, so
    /// pin the three properties the stack measurement depends on: the big
    /// array is actually big, the `FLNK` chain is actually 32 deep, and the
    /// whole set parses on top of the other two constants.
    #[cfg(feature = "bringup-probes")]
    #[test]
    fn the_e8_stack_db_defines_a_deep_chain_and_a_large_array() {
        use std::collections::HashMap;

        let full = format!(
            "{}{}{}",
            crate::demo_db::DEMO_DB,
            crate::demo_db::C6_PROBE_DB,
            crate::demo_db::E8_STACK_DB
        );
        let recs = epics_base_rs::server::db_loader::parse_db(&full, &HashMap::new())
            .expect("demo + C6 + E8 must parse together");
        let names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();

        for n in [
            "RTEMS:E8:WF",
            "RTEMS:E8:WF2",
            "RTEMS:E8:SA",
            "RTEMS:E8:CMP",
            "RTEMS:E8:H",
            "RTEMS:E8:FAN",
        ] {
            assert!(names.contains(&n), "{n} missing from the E8 stack set");
        }
        for i in 1..=32 {
            let n = format!("RTEMS:E8:L{i}");
            assert!(
                names.contains(&n.as_str()),
                "{n} missing: the chain is not 32 deep, so a depth comparison \
                 against C1..C8 would not be a comparison"
            );
        }
        assert!(
            !names.contains(&"RTEMS:E8:L33"),
            "the chain is longer than the 32 the measurement reports"
        );

        // Two array sizes an octave apart, because one size cannot say whether
        // the CAS-client stack high-water scales with payload size.
        for (pv, want, why) in [
            (
                "RTEMS:E8:WF",
                "32768",
                "the array must stay large enough that a reply is 262,144 B, not 8",
            ),
            (
                "RTEMS:E8:WFBIG",
                "131072",
                "the second array must stay 4x the first, or the payload-size \
                 sensitivity arm compares nothing",
            ),
        ] {
            let rec = recs.iter().find(|r| r.name == pv).unwrap_or_else(|| {
                panic!("{pv} missing from the E8 stack set");
            });
            let nelm = rec
                .fields
                .iter()
                .find(|f| f.name == "NELM")
                .map(|f| f.value.to_string());
            assert_eq!(nelm.as_deref(), Some(want), "{why}");
        }
    }

    /// With the feature on, the rig is exactly the 14 records the C6
    /// measurement was built from, on top of the same clean demo set — so the
    /// bring-up image is one
    /// cargo flag away, not a code edit away.
    #[cfg(feature = "bringup-probes")]
    #[test]
    fn the_probe_rig_defines_the_c6_records() {
        use std::collections::HashMap;

        let full = format!("{}{}", crate::demo_db::DEMO_DB, crate::demo_db::C6_PROBE_DB);
        let recs = epics_base_rs::server::db_loader::parse_db(&full, &HashMap::new())
            .expect("the demo + probe database must parse");
        // The demo half of this total has an owner already
        // (`the_default_database_is_clean_and_link_free`), so read it rather
        // than restate it: this test is about the rig's own 14, and hard-coding
        // the sum is what made adding `RTEMS:BO` to `DEMO_DB` fail here.
        let demo =
            epics_base_rs::server::db_loader::parse_db(crate::demo_db::DEMO_DB, &HashMap::new())
                .expect("the demo database must parse");
        assert_eq!(
            recs.len(),
            demo.len() + 14,
            "the demo records plus the 14-record rig"
        );
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
                .and_then(|r| r.fields.iter().find(|f| f.name == "INP"))
                .map(|f| f.value.to_string())
        };
        assert_eq!(inp("RTEMS:CA:DOWN").as_deref(), Some("UPSTREAM:AI CP"));
        assert_eq!(
            inp("RTEMS:CA:DOWN2").as_deref(),
            Some("ca://UPSTREAM:AI CP")
        );
    }
}
