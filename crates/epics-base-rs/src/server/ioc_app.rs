// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that hand-builds its own tokio runtime; runs and passes in the exec-backend suite.
//! IOC Application — st.cmd-style startup for Rust IOCs.
//!
//! Provides a 2-phase IOC lifecycle matching the C++ EPICS pattern:
//!
//! **Phase 1 (pre-init):** Execute startup script (`st.cmd`)
//!   - `epicsEnvSet`, `dbLoadRecords`, custom driver config commands
//!
//! **Phase 2 (iocInit):** Wire device support, start protocol server
//!
//! **Phase 3 (post-init):** Interactive iocsh REPL
//!   - `dbl`, `dbgf`, `dbpf`, `dbpr`, custom commands
//!
//! # Example
//!
//! ```rust,ignore
//! IocApplication::new()
//!     .port(5064)
//!     .register_device_support("myDevice", || Box::new(MyDeviceSupport::new()))
//!     .register_startup_command(my_config_command())
//!     .startup_script("st.cmd")
//!     .run(my_protocol_runner)
//!     .await
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::error::{CaError, CaResult};
use crate::runtime::net::cas_server_port;
use crate::server::record::{self, Record, SubroutineFn};

use crate::server::database::PvDatabase;
use crate::server::device_support::DeviceSupport;
use crate::server::iocsh::{self, registry::CommandDef};
use crate::server::{DeviceSupportFactory, access_security, autosave};
use autosave::startup::AutosaveStartupConfig;

/// IOC lifecycle init-hook subsystem — Rust port of epics-base
/// `libcom/src/iocsh/initHooks.{c,h}`.
///
/// C code registers a callback via `initHookRegister()` and the IOC
/// fires `initHookAnnounce(state)` at fixed points during
/// `iocBuild()` / `iocRun()`. Ported code (autosave pass-0/pass-1
/// restore, areaDetector plugins, sequencer programs, caPutLog,
/// devIocStats) all hang behaviour off these announcements.
///
/// Both Rust build paths ([`IocApplication::run`] and
/// [`crate::server::ioc_builder::IocBuilder::build`]) announce the
/// states they reach in the same order C does.
pub mod init_hooks {
    use std::sync::{Arc, Mutex};

    /// Initialization stages, mirroring C's `initHookState` enum
    /// (`initHooks.h`). Only the states this IOC can actually reach are
    /// modelled, and the order of the modelled variants matches C exactly.
    ///
    /// The pause block and the reachable half of the shutdown block are
    /// here because [`super::ioc_pause`] and [`super::ioc_shutdown`] make
    /// those transitions real. Still absent, because the port has no such
    /// transition to announce them from: `initHookAfterCloseLinks`,
    /// `initHookAfterStopCallback` and `initHookAfterStopLinks` (no
    /// `doCloseLinks`, `callbackStop` or `dbCaShutdown` analogue — the
    /// callback facility has no stop and the link sets have no lifecycle
    /// methods), `initHookBeforeFree` (C announces it only from
    /// `iocBuildIsolated`'s shutdown), the two `dbUnitTest` states, and the
    /// two states C itself marks deprecated.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum InitHookState {
        /// Start of iocBuild() / iocInit().
        AtIocBuild,
        /// Database sanity checks passed.
        AtBeginning,
        /// Callbacks, generalTime & taskwd init.
        AfterCallbackInit,
        /// CA links init.
        AfterCaLinkInit,
        /// Driver support init.
        AfterInitDrvSup,
        /// Record support init.
        AfterInitRecSup,
        /// Device support init pass 0 (also autosave pass 0).
        AfterInitDevSup,
        /// Records and locksets init (also autosave pass 1).
        AfterInitDatabase,
        /// Device support init pass 1.
        AfterFinishDevSup,
        /// Scan, AS, ProcessNotify init.
        AfterScanInit,
        /// Records with PINI = YES processed.
        AfterInitialProcess,
        /// RSRV (CA server) init.
        AfterCaServerInit,
        /// End of iocBuild().
        AfterIocBuilt,
        /// Start of iocRun().
        AtIocRun,
        /// Scan tasks and CA links running.
        AfterDatabaseRunning,
        /// RSRV (CA server) running.
        AfterCaServerRunning,
        /// End of iocRun() / iocInit().
        AfterIocRunning,

        /// Start of iocPause().
        AtIocPause,
        /// Protocol servers paused.
        AfterCaServerPaused,
        /// CA links and scan tasks paused.
        AfterDatabasePaused,
        /// End of iocPause().
        AfterIocPaused,

        /// Start of iocShutdown().
        AtShutdown,
        /// Scan tasks stopped.
        AfterStopScan,
        /// End of iocShutdown().
        AfterShutdown,
    }

    impl InitHookState {
        /// Printable representation — mirrors C `initHookName()`.
        pub fn name(&self) -> &'static str {
            match self {
                InitHookState::AtIocBuild => "initHookAtIocBuild",
                InitHookState::AtBeginning => "initHookAtBeginning",
                InitHookState::AfterCallbackInit => "initHookAfterCallbackInit",
                InitHookState::AfterCaLinkInit => "initHookAfterCaLinkInit",
                InitHookState::AfterInitDrvSup => "initHookAfterInitDrvSup",
                InitHookState::AfterInitRecSup => "initHookAfterInitRecSup",
                InitHookState::AfterInitDevSup => "initHookAfterInitDevSup",
                InitHookState::AfterInitDatabase => "initHookAfterInitDatabase",
                InitHookState::AfterFinishDevSup => "initHookAfterFinishDevSup",
                InitHookState::AfterScanInit => "initHookAfterScanInit",
                InitHookState::AfterInitialProcess => "initHookAfterInitialProcess",
                InitHookState::AfterCaServerInit => "initHookAfterCaServerInit",
                InitHookState::AfterIocBuilt => "initHookAfterIocBuilt",
                InitHookState::AtIocRun => "initHookAtIocRun",
                InitHookState::AfterDatabaseRunning => "initHookAfterDatabaseRunning",
                InitHookState::AfterCaServerRunning => "initHookAfterCaServerRunning",
                InitHookState::AfterIocRunning => "initHookAfterIocRunning",
                InitHookState::AtIocPause => "initHookAtIocPause",
                InitHookState::AfterCaServerPaused => "initHookAfterCaServerPaused",
                InitHookState::AfterDatabasePaused => "initHookAfterDatabasePaused",
                InitHookState::AfterIocPaused => "initHookAfterIocPaused",
                InitHookState::AtShutdown => "initHookAtShutdown",
                InitHookState::AfterStopScan => "initHookAfterStopScan",
                InitHookState::AfterShutdown => "initHookAfterShutdown",
            }
        }
    }

    /// Application callback type — Rust equivalent of C's
    /// `initHookFunction`. Invoked once per announced state. `Arc`
    /// so [`init_hook_announce`] can snapshot the list and drop the
    /// lock before invoking callbacks (C holds its list mutex only
    /// during traversal, never across the callback).
    pub type InitHookFunction = Arc<dyn Fn(InitHookState) + Send + Sync>;

    static HOOKS: Mutex<Vec<InitHookFunction>> = Mutex::new(Vec::new());

    /// Register a function for initHook notifications — Rust port of
    /// C `initHookRegister()`. The callback is invoked for every
    /// subsequently-announced state. Registration is process-global,
    /// matching C's single `functionList`.
    ///
    /// Unlike C (which dedups by function pointer) closures cannot be
    /// compared for identity, so every call adds a distinct callback;
    /// callers must register each hook once.
    pub fn init_hook_register(func: InitHookFunction) {
        HOOKS.lock().unwrap().push(func);
    }

    /// Announce an init-hook state to all registered callbacks —
    /// Rust port of C `initHookAnnounce()`. Called only by the IOC
    /// build paths at the fixed lifecycle points.
    ///
    /// The callback list is snapshotted (cheap `Arc` clones) and the
    /// lock released before any callback runs, so a hook that calls
    /// [`init_hook_register`] from inside the callback cannot
    /// deadlock. Hooks registered during an announce are not invoked
    /// for that same state — matching C's snapshot-of-`ellFirst`
    /// traversal semantics.
    pub fn init_hook_announce(state: InitHookState) {
        let snapshot: Vec<InitHookFunction> = HOOKS.lock().unwrap().clone();
        for cb in snapshot {
            cb(state);
        }
    }

    /// Forget all registered callbacks. Test-only — mirrors C
    /// `initHookFree()`. Lets unit tests run in isolation without
    /// leaking process-global hook state into each other.
    #[cfg(test)]
    pub fn init_hook_free() {
        HOOKS.lock().unwrap().clear();
    }
}

pub use init_hooks::{InitHookFunction, InitHookState, init_hook_announce, init_hook_register};

/// The IOC's run state — C `enum iocStateEnum` (`iocInit.h:17-19`
/// @R7.0.10) and the file-static `iocState` every transition in
/// `iocInit.c` guards on.
///
/// Modelled as a state, not as a set of booleans, for the reason C keeps
/// one cell: `iocRun` and `iocPause` are legal only from particular
/// states, and each answers a wrong one with a diagnostic rather than
/// doing half the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IocState {
    /// C `iocVoid` — nothing built, or shut down again.
    Void,
    /// C `iocBuilding` — inside the build phase.
    Building,
    /// C `iocBuilt` — quiescent: the database exists, nothing scans.
    Built,
    /// C `iocRunning`.
    Running,
    /// C `iocPaused` — built and populated, record processing frozen.
    Paused,
}

/// The IOC lifecycle's single owner.
///
/// C keeps `iocState` in one file-static and lets only `iocBuild`,
/// `iocRun`, `iocPause` and `iocShutdown` write it. This holds the same
/// cell plus the one resource whose lifetime is the IOC's rather than any
/// caller's: the scan owner. It used to be a local in
/// [`IocApplication::run_to_completion`], which is why nothing but
/// returning from `run` could stop scanning — and why there was no
/// `iocShutdown` to call.
struct Lifecycle {
    state: IocState,
    /// Alive from the `iocRun` transition until `iocShutdown`. `iocPause`
    /// does NOT drop it: C's `scanPause` leaves the periodic threads
    /// running and merely stops them scanning, which is what lets
    /// `iocRun` resume the rates in phase.
    scan: Option<crate::server::scan::ScanOwner>,
}

static LIFECYCLE: Mutex<Lifecycle> = Mutex::new(Lifecycle {
    state: IocState::Void,
    scan: None,
});

fn lifecycle() -> std::sync::MutexGuard<'static, Lifecycle> {
    LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner())
}

/// C `getIocState` (`iocInit.c:100-103`).
pub fn get_ioc_state() -> IocState {
    lifecycle().state
}

/// Record a lifecycle transition. Private, and the only writer of the
/// state cell — the whole point of having one owner.
fn set_ioc_state(state: IocState) {
    lifecycle().state = state;
}

/// Hand the scan owner to the lifecycle — called once, at the `iocRun`
/// point, by the build path that created it.
fn adopt_scan_owner(owner: crate::server::scan::ScanOwner) {
    // Replacing an existing owner drops the old one (joining its thread)
    // AFTER the lock is released, so a second `run` in one process cannot
    // deadlock against the join.
    let previous = lifecycle().scan.replace(owner);
    drop(previous);
}

/// C `iocRun` (`iocInit.c:246-276`): bring a built or paused IOC to the
/// running state. Returns C's status — 0 on success, -1 when the IOC is in
/// neither state.
///
/// The port's `iocRun` covers C's `scanRun` and `dbRunServers` halves.
/// `dbCaRun` has no analogue: the link sets ([`crate::server::database::LinkSet`])
/// carry no lifecycle methods, so external links keep running across a
/// pause — see [`ioc_pause`].
pub fn ioc_run() -> i32 {
    let from = get_ioc_state();
    if from != IocState::Paused && from != IocState::Built {
        crate::runtime::log::errlog_printf(&format!(
            "iocRun: {} IOC not paused\n",
            crate::runtime::log::erl_warning()
        ));
        return -1;
    }
    init_hook_announce(InitHookState::AtIocRun);

    crate::server::scan::scan_run();
    init_hook_announce(InitHookState::AfterDatabaseRunning);

    crate::server::db_server::db_run_servers();
    init_hook_announce(InitHookState::AfterCaServerRunning);

    crate::runtime::log::errlog_printf(if from == IocState::Built {
        "iocRun: All initialization complete\n"
    } else {
        "iocRun: IOC restarted\n"
    });
    set_ioc_state(IocState::Running);
    init_hook_announce(InitHookState::AfterIocRunning);
    0
}

/// The `iocRun` transition as reached by [`crate::server::scan::ScanOwner::start`]
/// — the one line every bring-up path in this workspace shares.
///
/// `IocApplication` is the port of C's `iocBuild`, but it is not the only
/// way an IOC starts here: `softioc-rs`, `qsrv-rs`, `dual_ioc_rs`,
/// `oracle_ioc` and the two realtime IOCs build their database through
/// `CaServerBuilder` / `PvaServerBuilder` and then start the scan owner
/// themselves. Those paths have no build phase to announce, so their
/// database is already built by the time they start scanning — which is
/// why [`IocState::Void`] resolves to [`IocState::Built`] here rather than
/// being refused. Without this the lifecycle would be correct on one path
/// and permanently `Void` on six, and `iocPause` would answer "IOC not
/// running" on the very IOC that is.
///
/// A redundant owner (the `try_claim_scan_start` case) finds the IOC
/// already running and only re-arms the facility cell.
pub(crate) fn note_scan_owner_started() {
    if get_ioc_state() == IocState::Void {
        set_ioc_state(IocState::Built);
    }
    if get_ioc_state() == IocState::Built {
        ioc_run();
    } else {
        crate::server::scan::scan_run();
    }
}

/// C `iocPause` (`iocInit.c:278-300`): freeze record processing without
/// tearing anything down. Returns 0, or -1 when the IOC is not running.
///
/// What freezes is exactly what C's `scanCtl` gate covers — periodic
/// scanning, `postEvent`, and I/O Intr callbacks. `scanOnce` is not gated
/// in C either, so a `dbpf` from the paused shell still processes its
/// record; and this port additionally leaves CA/PVA link input running,
/// because there is no `dbCaPause` to call.
pub fn ioc_pause() -> i32 {
    if get_ioc_state() != IocState::Running {
        crate::runtime::log::errlog_printf(&format!(
            "iocPause: {} IOC not running\n",
            crate::runtime::log::erl_warning()
        ));
        return -1;
    }
    init_hook_announce(InitHookState::AtIocPause);

    crate::server::db_server::db_pause_servers();
    init_hook_announce(InitHookState::AfterCaServerPaused);

    crate::server::scan::scan_pause();
    init_hook_announce(InitHookState::AfterDatabasePaused);

    set_ioc_state(IocState::Paused);
    crate::runtime::log::errlog_printf("iocPause: IOC suspended\n");
    init_hook_announce(InitHookState::AfterIocPaused);
    0
}

/// C `iocShutdown` (`iocInit.c:722-763`): stop the IOC and return it to
/// [`IocState::Void`]. Idempotent — C returns 0 immediately when already
/// void, which is what makes it safe on every exit path.
///
/// C reaches this from `epicsAtExit(exitDatabase)` (`iocInit.c:579`), and
/// so does the port: [`IocApplication::run`] registers it there, so the
/// scan threads stop when the IOC's `run` returns however it returns.
///
/// C's `doCloseLinks`, `callbackStop` and `dbCaShutdown` steps have no
/// port analogue and are not faked — the callback facility has no stop and
/// the link sets have no lifecycle methods, so their init-hook states are
/// not announced either.
pub fn ioc_shutdown() -> i32 {
    if get_ioc_state() == IocState::Void {
        return 0;
    }
    init_hook_announce(InitHookState::AtShutdown);

    // Dropping the owner is C's `scanStop`: it trips the facility to
    // `ctlExit` and joins the owner thread. Taken out from under the lock
    // first so the join happens with the lifecycle unlocked.
    let owner = lifecycle().scan.take();
    drop(owner);
    // A build that never reached `iocRun` has no owner to drop, so the
    // facility is stopped here rather than by the drop above.
    crate::server::scan::scan_stop();
    init_hook_announce(InitHookState::AfterStopScan);

    crate::server::db_server::db_stop_servers();

    set_ioc_state(IocState::Void);
    init_hook_announce(InitHookState::AfterShutdown);
    0
}

// ── QSRV `dbLoadGroup` startup queue ──────────────────────────────────
//
// `dbLoadGroup("file.json", "macros")` is the pvxs/QSRV iocsh command that
// adds DB group definitions before `iocInit`. pvxs registers it from its
// `epicsExportRegistrar` (ioc/groupsourcehooks.cpp:233-244, run before the
// startup script) and its only startup-time effect is to *queue* the file:
// the JSON is parsed and the group built only later, by `processGroups()`
// at `initHookAfterInitDatabase` (groupsourcehooks.cpp:99-188 append to
// `IOCGroupConfig::groupConfigFiles`; :192-213 process them).
//
// epics-rs has no link-time registrar, and the served `BridgeProvider`
// (the QSRV group source) is created by the PVA protocol runner *after*
// the startup script has already executed. So the iocsh `dbLoadGroup`
// command itself must live in this base layer — the owner of the startup
// shell — while the group *semantics* (JSON parse, macLib expansion,
// serving) stay entirely in the QSRV bridge: this command only records the
// request, and the QSRV runner drains [`take_group_load_requests`] and
// applies each entry to the provider it serves. A non-QSRV runner simply
// never drains the queue, so the command is a harmless no-op there.

/// One queued `dbLoadGroup(filename, macros)` invocation, recorded during
/// st.cmd for the QSRV protocol runner to apply to the served provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupLoadRequest {
    pub filename: String,
    pub macros: String,
}

static GROUP_LOAD_REQUESTS: std::sync::LazyLock<Mutex<Vec<GroupLoadRequest>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

/// Drain every queued `dbLoadGroup` request, in invocation order. The
/// QSRV protocol runner calls this once before the PVA server accepts
/// connections — the epics-rs iocRun-handoff equivalent of pvxs running
/// `processGroups()` at `initHookAfterInitDatabase`.
pub fn take_group_load_requests() -> Vec<GroupLoadRequest> {
    std::mem::take(&mut *GROUP_LOAD_REQUESTS.lock().unwrap())
}

/// Build the `dbLoadGroup <jsonFilename> [<macros>]` startup command.
///
/// Mirrors pvxs `dbLoadGroup` (ioc/groupsourcehooks.cpp:99-188): a leading
/// `-` removes a previously queued identity (`-*` clears all, `-file`
/// removes the matching `(filename, macros)` entry); otherwise the pair is
/// appended after first erasing any prior entry of the same identity (pvxs
/// erases the matching entry before re-appending, :174-179 erase and
/// :181-183 re-append). The file is
/// opened here only to surface pvxs's early "Error opening" diagnostic at
/// the st.cmd line; the JSON parse and macLib expansion happen later in the
/// QSRV runner when the queue is drained.
///
/// [`IocApplication::run`] already registers this command into every
/// startup shell, so applications using the standard lifecycle need not
/// call it. It is public for harnesses that build their own iocsh shell
/// and want the pvxs-compatible `dbLoadGroup` command (its queue is
/// consumed via [`take_group_load_requests`]).
pub fn db_load_group_startup_command() -> CommandDef {
    use crate::server::iocsh::registry::{
        ArgDesc, ArgType, ArgValue, CommandContext, CommandOutcome,
    };
    CommandDef::new(
        "dbLoadGroup",
        vec![
            ArgDesc {
                name: "filename",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "macros",
                arg_type: ArgType::String,
            },
        ],
        "dbLoadGroup <jsonFilename> [<macros>]",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let filename = match args.first() {
                Some(ArgValue::String(s)) => s.clone(),
                _ => return Err("dbLoadGroup: missing filename".into()),
            };
            let macros = match args.get(1) {
                Some(ArgValue::String(s)) => s.clone(),
                _ => String::new(),
            };
            let mut queue = GROUP_LOAD_REQUESTS.lock().unwrap();
            // Leading `-`: removal by identity, applied to the queue (pvxs
            // groupsourcehooks.cpp:140-179 — never touches the filesystem).
            if let Some(rest) = filename.strip_prefix('-') {
                if rest == "*" {
                    let n = queue.len();
                    queue.clear();
                    ctx.println(&format!(
                        "dbLoadGroup: cleared all queued group files ({n} removed)"
                    ));
                } else {
                    let before = queue.len();
                    queue.retain(|r| !(r.filename == rest && r.macros == macros));
                    let dropped = before - queue.len();
                    ctx.println(&format!(
                        "dbLoadGroup: removed '{rest}' ({dropped} queued entr{} dropped)",
                        if dropped == 1 { "y" } else { "ies" }
                    ));
                }
                return Ok(CommandOutcome::Continue);
            }
            // pvxs opens the file at command time (early error); mirror that
            // with a readability probe. The QSRV runner re-reads and parses.
            if let Err(e) = std::fs::metadata(&filename) {
                return Err(format!("dbLoadGroup: error opening \"{filename}\": {e}"));
            }
            // Re-load of the same identity first drops the prior queue entry
            // (pvxs erases the matching `(fname, macros)` before appending).
            queue.retain(|r| !(r.filename == filename && r.macros == macros));
            queue.push(GroupLoadRequest {
                filename: filename.clone(),
                macros,
            });
            ctx.println(&format!(
                "dbLoadGroup: queued '{filename}' ({} group file(s) queued)",
                queue.len()
            ));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Context passed to dynamic device support factories during iocInit wiring.
pub struct DeviceSupportContext<'a> {
    pub dtyp: &'a str,
    pub inp: &'a str,
    pub out: &'a str,
}

/// Dynamic device support factory: given a context, returns device support if recognized.
pub type DynamicDeviceSupportFactory =
    Box<dyn Fn(&DeviceSupportContext) -> Option<Box<dyn DeviceSupport>> + Send + Sync>;

/// An async external link-set installer, registered on an
/// [`IocApplication`] via [`IocApplication::register_link_set_installer`]
/// and invoked by [`IocApplication::run`] at the C `initHookAfterCaLinkInit`
/// point — BEFORE [`PvDatabase::setup_cp_links`] warms Passive CP holders.
///
/// The installer receives the live database, registers its external
/// [`crate::server::database::LinkSet`] (e.g. the `ca` set from
/// `epics-ca-rs`'s `calink`, the `pva` set from the bridge's `pvalink`),
/// and returns any iocsh commands it owns. Those go straight onto the
/// process's one command table — C `iocshRegister` from a registrar the
/// `initHookAfterCaLinkInit` point reaches — so they are callable from the
/// script line after `iocInit` and from the prompt alike.
/// Registering the link set here — not inside the Phase-3 protocol runner
/// — is what makes a Passive holder of an external CP/CPP link warm at
/// iocInit: `setup_cp_links`'s `resolve_external_pv` open path is a no-op
/// unless the matching link set is already installed.
pub type LinkSetInstaller = Box<
    dyn FnOnce(
            Arc<PvDatabase>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Vec<CommandDef>> + Send + 'static>,
        > + Send
        + 'static,
>;

/// Configuration passed to the protocol runner after IOC initialization.
///
/// Contains all the pieces needed to start a protocol-specific server
/// (e.g., CA or PVA) with an interactive shell.
pub struct IocRunConfig {
    pub db: Arc<PvDatabase>,
    /// UDP discovery port — clients SEARCH here. Defaults to
    /// `EPICS_CA_SERVER_PORT` or 5064.
    pub port: u16,
    /// Optional TCP-listen port override. `None` means "use `port`".
    /// `Some(p)` lets multiple IOCs on one host bind unique TCP ports
    /// (epics-base PR #69, `EPICS_CAS_SERVER_PORT`) while keeping the
    /// canonical UDP discovery port.
    pub tcp_port: Option<u16>,
    /// The IOC's single live Access Security policy cell, seeded from
    /// [`IocApplication::acf`] and shared with the startup/interactive
    /// iocsh shells (whose `asInit` stores into it). A protocol runner
    /// must hand this cell to every server it builds — never re-wrap
    /// the config in a fresh cell — so a later `asInit`/ACF reload
    /// reaches all of them at once.
    pub acf: access_security::AcfCell,
    pub autosave_config: Option<autosave::SaveSetConfig>,
    pub autosave_manager: Option<Arc<autosave::AutosaveManager>>,
    pub shell_commands: Vec<CommandDef>,
    /// Retained for API compatibility. [`IocApplication::run`] now
    /// drains `register_after_init` hooks itself at the
    /// `initHookAfterIocRunning` point, so this is always handed to
    /// the protocol runner EMPTY. A runner must not execute it
    /// (doing so is a no-op on the empty vec, but the hooks have
    /// already run).
    pub after_init_hooks: Vec<Box<dyn FnOnce() + Send>>,
}

/// The two independent questions C softMain asks between the startup script
/// and the end of `main`: whether to call `iocInit()` at all
/// (`softMain.cpp:239`), and what this process does when it reaches its own
/// tail (`:247`).
///
/// C sets `loadedDb` only from `-d` and `-x`, and calls `iocInit()` only
/// when it is true. Everything a running IOC has — the scan threads, PINI,
/// and RSRV, which starts inside `iocRun` — therefore exists only on that
/// arm; a `softIoc` given no database reaches its `iocsh(NULL)` prompt
/// having built nothing and having opened no port. This port ran the whole
/// lifecycle unconditionally, so a bare `softioc-rs` bound 5064 and served
/// an empty database where C serves nothing at all.
///
/// The two booleans are orthogonal in C and so are they here. `interactive`
/// used to live inside a `Skip` variant, on the reasoning that it "means
/// nothing on the `Run` arm, where the protocol runner owns the tail" — and
/// that is false the moment `iocInit()` fails, because C then never reaches
/// `iocRun`, never starts a server, and falls through to exactly the same
/// tail (`softMain.cpp:239-245`, measured: `-S` with an unreadable ACF stays
/// alive and listens on nothing). A struct is what lets the failure arm ask
/// C's `-S` question; the constructors keep call sites from reading as a
/// bare pair of bools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IocInitDecision {
    /// C's `loadedDb` (`softMain.cpp:239`).
    run: bool,
    /// C's `-S` (`softMain.cpp:137`, `:202-203`): the `iocsh(NULL)` prompt
    /// at `:250`, or the `epicsThreadSleep(1000.0)` spin at `:264`.
    interactive: bool,
}

impl IocInitDecision {
    /// C's `loadedDb` arm (`softMain.cpp:239-245`): build the IOC, run it,
    /// and hand it to the protocol runner, which owns the tail from there.
    /// `interactive` is still C's `-S` and is what the tail falls back to
    /// when the build fails and the runner is never called.
    pub fn run(interactive: bool) -> Self {
        Self {
            run: true,
            interactive,
        }
    }

    /// C's other arm. `iocInit()` is never called, so there is no server to
    /// hand anything to and the protocol runner is NOT invoked — that is
    /// the whole content of the difference, and passing a runner a flag to
    /// obey would put it back in the hands of the caller who cannot see
    /// this decision. The process goes straight to C's tail
    /// (`softMain.cpp:247-268`).
    pub fn skip(interactive: bool) -> Self {
        Self {
            run: false,
            interactive,
        }
    }
}

/// Which phase of C softIoc's `main` a failed [`IocApplication::run_phased`]
/// was in.
///
/// C runs the whole boot inside one `try` whose `catch` exits 2, and reaches
/// its serving phase — `iocsh(NULL)`, or the non-interactive spin — only after
/// that block closes, exiting 1 when the shell fails (`softMain.cpp:247-279`).
/// A single flat error type cannot carry that difference, and a caller that
/// reconstructs it by reading the message is guessing at something already
/// known here, where the failure happens.
#[derive(Debug)]
pub enum IocRunFailure {
    /// The startup script returned non-zero. C wraps this as `Error in
    /// <path>` (`softMain.cpp:231`), so the path travels with the failure
    /// rather than being re-derived from the caller's own argv.
    StartupScript {
        /// The `st.cmd` as the caller named it.
        path: String,
        /// What the shell reported.
        reason: String,
    },
    /// A pre-script command line returned non-zero. C `softMain.cpp:192-198`
    /// runs `-d` as `errIf(dbLoadRecords(...), "")` — an EMPTY message, so
    /// the catch block prints nothing and the command's own diagnostic is
    /// the whole report. The line travels with the failure for the same
    /// reason the script path does above.
    StartupCommand {
        /// The line as this application queued it.
        line: String,
        /// What the shell reported.
        reason: String,
    },
    /// Any other failure before the protocol runner starts — inline records,
    /// autosave restore, `iocInit`. C's catch block.
    Startup(CaError),
    /// The protocol runner itself, past the point C's `try` block ends.
    Serving(CaError),
}

/// The lifecycle's default phase: every `?` inside it is a boot step, which
/// is why only the two exceptions are written out by hand.
impl From<CaError> for IocRunFailure {
    fn from(e: CaError) -> Self {
        IocRunFailure::Startup(e)
    }
}

impl From<IocRunFailure> for CaError {
    fn from(failure: IocRunFailure) -> Self {
        match failure {
            IocRunFailure::StartupScript { reason, .. }
            | IocRunFailure::StartupCommand { reason, .. } => CaError::InvalidValue(reason),
            IocRunFailure::Startup(e) | IocRunFailure::Serving(e) => e,
        }
    }
}

impl std::fmt::Display for IocRunFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IocRunFailure::StartupScript { reason, .. }
            | IocRunFailure::StartupCommand { reason, .. } => write!(f, "{reason}"),
            IocRunFailure::Startup(e) | IocRunFailure::Serving(e) => write!(f, "{e}"),
        }
    }
}

/// The exact bytes `iocBuild_2` hands `errlogPrintf` when `asInit()` fails
/// (`iocInit.c:188-190`).
///
/// C's literal is
///
/// ```c
/// ERL_ERROR " iocBuild: asInit Failed.\n"
///     ANSI_MAGENTA(" The IOC has not been started.") "\n"
/// ```
///
/// so the reset closes BEFORE the second newline and the message ends with
/// one — a terminator this had to carry itself once `console_fallback`
/// started writing the caller's bytes verbatim, as C's
/// `fprintf(console, "%s", …)` (`errlog.c:795`) does.
///
/// `paints` is [`crate::runtime::log::errlog_console_paints`]: unlike the
/// `fprintf(stderr, …)` diagnostics, this one goes through errlog's pump,
/// where `errlogStripANSI` takes BOTH spans off together when the console is
/// not a terminal.
///
/// A function so the bytes can be pinned against a measured `softIoc` run
/// without capturing the process stderr, the way `format_show_error` pins
/// `iocsh.cpp`'s.
fn as_init_failed_message(paints: bool) -> String {
    let (error, magenta, reset) = if paints {
        (crate::runtime::log::ERL_ERROR, "\x1b[35;1m", "\x1b[0m")
    } else {
        ("ERROR", "", "")
    };
    format!("{error} iocBuild: asInit Failed.\n{magenta} The IOC has not been started.{reset}\n")
}

/// C softMain's whole pre-`iocInit` sequence, on one shell: the command
/// lines argv built, in argv order, then the startup script.
///
/// Free rather than a method because it runs on the startup thread, where
/// the application struct has already been taken apart.
fn run_startup_phase(
    shell: &iocsh::IocShell,
    lines: &[String],
    script: Option<&str>,
) -> Result<(), IocRunFailure> {
    for line in lines {
        shell
            .execute_line_reported(line)
            .map_err(|reason| IocRunFailure::StartupCommand {
                line: line.clone(),
                reason,
            })?;
    }
    if let Some(script) = script {
        // The iocshLoad mirror: C `iocsh(pathname)` is
        // `iocshLoad(pathname, NULL)`, which also records
        // IOCSH_STARTUP_SCRIPT (epics-base#469).
        shell
            .execute_script_with_macros(script, &Default::default())
            .map_err(|reason| IocRunFailure::StartupScript {
                path: script.to_string(),
                reason,
            })?;
    }
    Ok(())
}

/// C softMain's tail for a process whose `iocInit()` never ran
/// (`softMain.cpp:247-268`): the interactive shell, or the forever spin.
///
/// Nothing here starts a server, and that is the point — RSRV is started by
/// `iocRun` (`rsrv_run`, `caservertask.c`), so a C `softIoc` on this arm has
/// no listener and answers no search. The protocol runner is therefore not
/// called at all rather than being handed a flag it might not obey.
///
/// Free rather than a method for the same reason [`run_startup_phase`] is:
/// the application struct has already been taken apart by here.
async fn run_uninitialized_tail(
    db: Arc<PvDatabase>,
    bridge: crate::runtime::task::BlockingBridge,
    acf: access_security::AcfCell,
    interactive: bool,
) -> Result<(), IocRunFailure> {
    if !interactive {
        // C `softMain.cpp:264-265`: `while (true) epicsThreadSleep(1000.0);`
        // — the process exists to be killed. A future that never completes
        // is the same forever and parks instead of holding a thread.
        std::future::pending::<()>().await;
        unreachable!("a pending future never completes");
    }

    let (tx, rx) = crate::runtime::sync::oneshot::channel();
    // Mandatory for the same reason the two shells above are: this IS the
    // process on this arm, so a thread that will not start is a boot
    // failure and not something to carry on without.
    crate::runtime::task::MandatoryThread::new(
        "iocsh",
        crate::runtime::task::ThreadPriority::Iocsh,
        crate::runtime::task::StackSizeClass::Big,
    )
    .try_spawn(move || {
        // C runs `iocsh(NULL)` on the thread `epicsThreadInit` lists as
        // `_main_` (`osdThread.c:406-412`), as `CaServer::run_with_shell`
        // does for the initialised arm.
        crate::runtime::task::register_main_thread();
        let shell = iocsh::IocShell::new_with_acf(db, bridge, acf);
        let _ = tx.send(shell.run_repl());
    })
    .map_err(|e| CaError::InvalidValue(format!("could not start the iocsh thread: {e}")))?;

    match rx.await {
        Ok(Ok(())) => Ok(()),
        // C `softMain.cpp:253-256`: a non-zero `iocsh(NULL)` is
        // `epicsExit(1)`, which is this port's serving-phase status.
        Ok(Err(e)) => Err(IocRunFailure::Serving(CaError::InvalidValue(e))),
        Err(_) => Err(IocRunFailure::Serving(CaError::InvalidValue(
            "shell thread dropped".into(),
        ))),
    }
}

/// Everything C's `iocBuild()` needs, and the only implementation of that
/// transition in this crate.
///
/// **Invariant: the build and the run each happen exactly once per
/// [`IocApplication::run`], and both are complete before the line after
/// `iocInit` in the startup script executes.** [`IocLifecycle`] is the owner
/// that enforces it, by consuming the value that stands for the state each
/// transition starts from. This one is armed before the script runs and
/// consumed by whichever entry point reaches it first — the script's own
/// `iocInit` or `iocBuild` line, or [`IocApplication::run_to_completion`] when
/// the script spelled neither. Because there is one implementation and one
/// consumption, `iocInit` has a single meaning on both paths.
///
/// It used to have two. The shell's `iocInit` closed the record-load phase and
/// nothing else, while the real build ran after the whole script — so every
/// line a real `st.cmd` puts after `iocInit` (`dbpf`, `dbl`, `dbtr`,
/// `asSetFilename`, `seq`) ran against an IOC with no device support, no scan
/// threads and no PINI, and a script-loaded database never reached the protocol
/// runner at all: `softioc-rs -S st.cmd` listed its records from `dbl` and
/// served none of them to a CA client.
/// The protocol runner, type-erased so the lifecycle can carry it.
///
/// [`IocApplication::run`] takes it as a generic closure; it has to reach
/// [`BuiltIoc::run`], which is inside the lifecycle owner and cannot be
/// generic over it.
type ProtocolRunner = Box<
    dyn FnOnce(
            IocRunConfig,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = CaResult<()>> + Send + 'static>,
        > + Send
        + 'static,
>;

/// What [`BuiltIoc::run`] needs to start the servers, carried from
/// [`IocApplication::run_to_completion`] through the build untouched.
struct ProtocolStart {
    /// The runtime the runner is spawned onto. `BuiltIoc::run` is reached from
    /// the `iocsh-startup` thread through `BlockingBridge::block_on`, which is
    /// not a runtime thread, so the reactor has to be carried rather than
    /// looked up.
    bridge: crate::runtime::task::BlockingBridge,
    port: u16,
    tcp_port: Option<u16>,
    acf: access_security::AcfCell,
    autosave_config: Option<autosave::SaveSetConfig>,
    runner: ProtocolRunner,
}

/// The started protocol runner, and the only handle to it.
///
/// **Once the runner has been spawned, no path may leave
/// [`IocApplication::run_to_completion`] without this value having joined or
/// stopped the task.** C needs no such rule: `rsrv_run` starts threads the
/// process owns until `epicsExit`, and softMain's exits all run through it.
/// Here the runner is one task with one outcome, and the outcome is this
/// `run`'s return value, so it needs exactly one owner.
///
/// [`Self::wait`] and [`Self::shut_down`] are that owner's two answers; `Drop`
/// is the backstop for the paths that reach neither — a `?` in the
/// `afterIocRunning` drain below, a panic unwinding through `run_to_completion`,
/// a startup thread that failed after the script had already run `iocInit`.
struct ProtocolServer {
    handle: crate::runtime::task::TaskHandle<CaResult<()>>,
    /// The outcome [`Self::await_serving`] collected because the runner
    /// finished before any layer announced. Held rather than reported on the
    /// spot so the outcome still leaves through the one owner.
    finished: Option<Result<(), IocRunFailure>>,
    /// False once the outcome has been taken, so `Drop` does not abort a task
    /// that has already been accounted for.
    live: bool,
}

impl ProtocolServer {
    /// The runner's own outcome. Cancel-safe: dropping this future (the signal
    /// arms of the `select!` below) leaves `self.live` set, so the guard or
    /// [`Self::shut_down`] still owns the task.
    async fn wait(&mut self) -> Result<(), IocRunFailure> {
        if let Some(collected) = self.finished.take() {
            return collected;
        }
        let joined = (&mut self.handle).await;
        self.live = false;
        Self::outcome(joined)
    }

    /// C `iocRun`'s ordering, which the port has to wait for where C gets it
    /// for free: return once a protocol layer has announced it is serving
    /// (`db_server::announce_serving`, the port's `rsrv_run` return), or once
    /// the runner has finished without ever announcing.
    ///
    /// Both arms are terminal — a runner is either serving or done — so this
    /// is not a check that can be left waiting on a runner that forgot to
    /// signal. A runner that serves nothing resolves through the second arm,
    /// which is what every `async { Ok(()) }` in the tests does.
    async fn await_serving(&mut self, generation: u64) {
        tokio::select! {
            biased;
            joined = &mut self.handle => {
                self.live = false;
                self.finished = Some(Self::outcome(joined));
            }
            () = crate::server::db_server::serving_after(generation) => {}
        }
    }

    /// The join result as the failure `run_to_completion` reports.
    fn outcome(
        joined: Result<CaResult<()>, crate::runtime::task::TaskJoinError>,
    ) -> Result<(), IocRunFailure> {
        match joined {
            Ok(res) => res.map_err(IocRunFailure::Serving),
            // A panic in the runner reached `run_to_completion` as an unwind
            // before it was spawned, which skipped `run_phased`'s
            // `call_at_exits`. As a value it takes the same exit every other
            // serving failure does.
            Err(e) => Err(IocRunFailure::Serving(CaError::InvalidValue(format!(
                "protocol runner did not finish: {e}"
            )))),
        }
    }

    /// Stop the runner and wait until it is gone — what dropping the awaited
    /// future used to do, made explicit now that the task outlives the await.
    async fn shut_down(mut self) {
        self.live = false;
        self.handle.abort();
        let _ = (&mut self.handle).await;
    }
}

impl Drop for ProtocolServer {
    fn drop(&mut self) {
        if self.live {
            self.handle.abort();
        }
    }
}

/// Drops whatever this `run` armed and did not consume.
///
/// The window is [`arm_build`] to the `take_lifecycle` that finishes the
/// script's work: the startup script performs the build AND the run inside
/// itself, so a failure of the startup thread — or a panic anywhere in that
/// window — can return with an [`IocLifecycle::Running`], and its live
/// [`ProtocolServer`], still parked in the static. C has no such window
/// because its `iocInit()` and its `iocsh()` are statements in one `main`.
struct ArmedLifecycle;

impl Drop for ArmedLifecycle {
    fn drop(&mut self) {
        drop(take_lifecycle());
    }
}

struct IocBuild {
    db: Arc<PvDatabase>,
    acf: access_security::AcfCell,
    autosave_config: Option<autosave::SaveSetConfig>,
    autosave_startup: Option<Arc<Mutex<AutosaveStartupConfig>>>,
    device_factories: HashMap<String, DeviceSupportFactory>,
    dynamic_device_factory: Option<DynamicDeviceSupportFactory>,
    subroutine_registry: HashMap<String, Arc<SubroutineFn>>,
    link_set_installers: Vec<LinkSetInstaller>,
    after_init_hooks: Vec<Box<dyn FnOnce() + Send>>,
    protocol: ProtocolStart,
}

/// The IOC C `iocBuild()` leaves behind: [`IocState::Built`], quiescent, with
/// everything [`BuiltIoc::run`] needs to make it run.
struct BuiltIoc {
    db: Arc<PvDatabase>,
    autosave_manager: Option<Arc<autosave::AutosaveManager>>,
    /// Drained by the run half, at C's `initHookAfterIocRunning` point.
    after_init_hooks: Vec<Box<dyn FnOnce() + Send>>,
    /// Carried, not used: C's servers are initialised by `dbInitServers()` in
    /// `iocBuild` (`iocInit.c:222`) and only *started* by `dbRunServers()` in
    /// `iocRun` (`:265-267`), and this port's runner does both at once, so the
    /// whole of it belongs on the run half.
    protocol: ProtocolStart,
}

/// What the run transition leaves for the phases after it: the running
/// servers, and nothing else. The autosave manager and the port numbers used
/// to be carried across it so Phase 3 could assemble an [`IocRunConfig`]; that
/// assembly is now [`BuiltIoc::run`]'s, at C's `dbRunServers` point.
struct RunningIoc {
    server: ProtocolServer,
}

/// Where [`IocBuild::perform_build`] stops.
enum BuildOutcome {
    /// Boxed for the reason [`IocLifecycle::Armed`] is: `BuiltIoc` carries the
    /// database, the after-init hooks and the whole [`ProtocolStart`], and the
    /// other variant carries nothing.
    Built(Box<BuiltIoc>),
    /// `asInit` returned non-zero, so the build stopped there. Nothing here
    /// re-words the reason, which `asInitFile` has already written.
    AsInitFailed,
}

/// One value, one lifecycle — and the whole of this `run`'s claim on it.
///
/// Each variant is a state C names, and each transition **consumes** the value
/// standing for the state it starts from, so "exactly once" is carried by
/// ownership rather than by a runtime `if already_built`: after
/// [`IocBuild::perform_build`] there is no `IocBuild` left to build again, and
/// after [`BuiltIoc::run`] there is no `BuiltIoc` left to run again.
enum IocLifecycle {
    /// Armed before the startup script; C's `iocVoid`.
    /// Boxed: `IocBuild` carries the whole database and every registry, and
    /// the other variants are a fraction of its size.
    Armed(Box<IocBuild>),
    /// C's `iocBuilt`. Boxed for the same reason [`Self::Armed`] is.
    Built(Box<BuiltIoc>),
    /// C's `iocRunning`.
    Running(RunningIoc),
    /// The build stopped at a failed `asInit`.
    AsInitFailed,
    /// A transition returned an error, held for `run_to_completion` to report.
    Failed(CaError),
}

/// The single cell. The IOC lifecycle it belongs to is already process-global
/// (see [`set_ioc_state`]), so this is scoped the same way.
static LIFECYCLE_OWNER: Mutex<Option<IocLifecycle>> = Mutex::new(None);

fn arm_build(build: IocBuild) {
    *LIFECYCLE_OWNER.lock().unwrap() = Some(IocLifecycle::Armed(Box::new(build)));
}

fn take_lifecycle() -> Option<IocLifecycle> {
    LIFECYCLE_OWNER.lock().unwrap().take()
}

fn put_lifecycle(state: IocLifecycle) {
    *LIFECYCLE_OWNER.lock().unwrap() = Some(state);
}

/// What an iocsh lifecycle line did.
pub(crate) enum ShellTransition {
    /// This call performed the transition.
    Done,
    /// It performed it and the build failed; the line fails with it.
    Failed,
    /// This `run` owns a lifecycle, but not in the state this transition
    /// starts from — a second `iocBuild`, or `iocRun` with nothing built.
    Refused,
    /// This IOC has no [`IocApplication`] lifecycle to drive: every
    /// `CaServerBuilder` binary and every bare [`PvDatabase`] shell.
    NotOurs,
}

/// The iocsh `iocBuild` line, and the build half of `iocInit`.
pub(crate) fn build_from_shell(bridge: &crate::runtime::task::BlockingBridge) -> ShellTransition {
    match take_lifecycle() {
        None => ShellTransition::NotOurs,
        Some(IocLifecycle::Armed(build)) => match bridge.block_on(build.perform_build()) {
            Ok(BuildOutcome::Built(built)) => {
                put_lifecycle(IocLifecycle::Built(built));
                ShellTransition::Done
            }
            Ok(BuildOutcome::AsInitFailed) => {
                put_lifecycle(IocLifecycle::AsInitFailed);
                ShellTransition::Failed
            }
            Err(e) => {
                put_lifecycle(IocLifecycle::Failed(e));
                ShellTransition::Failed
            }
        },
        Some(other) => {
            put_lifecycle(other);
            ShellTransition::Refused
        }
    }
}

/// The iocsh `iocRun` line, and the run half of `iocInit`.
///
/// `Refused` is not an error the caller should print: it only means this line
/// has no freshly built IOC to consume, and the plain [`ioc_run`] transition —
/// which is what resumes a paused IOC — is the right next thing to try.
pub(crate) fn run_from_shell(bridge: &crate::runtime::task::BlockingBridge) -> ShellTransition {
    match take_lifecycle() {
        None => ShellTransition::NotOurs,
        Some(IocLifecycle::Built(built)) => {
            put_lifecycle(IocLifecycle::Running(bridge.block_on(built.run())));
            ShellTransition::Done
        }
        Some(other) => {
            put_lifecycle(other);
            ShellTransition::Refused
        }
    }
}

impl IocBuild {
    /// C `iocBuild()` (`iocInit.c:210-231`), ending where `iocBuild_3` does:
    /// [`IocState::Built`], the quiescent state [`BuiltIoc::run`] is legal
    /// from. `initHookAfterIocBuilt` is announced on this half, as
    /// `iocBuild_3` announces it (`iocInit.c:201-207`).
    async fn perform_build(self) -> CaResult<BuildOutcome> {
        let Self {
            db,
            acf,
            autosave_config,
            autosave_startup,
            device_factories,
            dynamic_device_factory,
            subroutine_registry,
            link_set_installers,
            after_init_hooks,
            protocol,
        } = self;

        // Collect restore paths and builder from startup config (scoped mutex lock)
        let (pass0_files, pass1_files, builder_opt) = if let Some(ref config) = autosave_startup {
            let cfg = config.lock().unwrap();
            let pass0: Vec<std::path::PathBuf> = cfg
                .pass0_restores
                .iter()
                .map(|r| cfg.resolve_save_file(&r.filename))
                .collect();
            let pass1: Vec<std::path::PathBuf> = cfg
                .pass1_restores
                .iter()
                .map(|r| cfg.resolve_save_file(&r.filename))
                .collect();
            let builder = if !cfg.monitor_sets.is_empty() || !cfg.triggered_sets.is_empty() {
                Some(cfg.into_builder())
            } else {
                None
            };
            (pass0, pass1, builder)
        } else {
            (Vec::new(), Vec::new(), None)
        };

        // initHooks subsystem (C `iocInit.c` / `initHooks.c` parity).
        //
        // Autosave pass-0 / pass-1 restore are no longer hard-coded
        // into the build flow: they are registered here as ordinary
        // init hooks (C autosave registers `initHookAfterInitDevSup`
        // for pass 0 and `initHookAfterInitDatabase` for pass 1).
        // Any third-party `init_hook_register` callback also fires at
        // the matching `init_hook_announce` point below. Because the
        // restore work is async and the C-parity `InitHookFunction`
        // is sync, autosave restores live in this local async-hook
        // table; `announce` below fires *both* tables.
        type AsyncHook = Box<
            dyn FnOnce()
                    -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>
                + Send
                + 'static,
        >;
        let mut lifecycle_hooks: Vec<(InitHookState, AsyncHook)> = Vec::new();

        // Register pass-0 restore as an `AfterInitDevSup` hook.
        {
            let db_p0 = db.clone();
            let files = pass0_files.clone();
            lifecycle_hooks.push((
                InitHookState::AfterInitDevSup,
                Box::new(move || {
                    Box::pin(async move {
                        for sav_path in &files {
                            match autosave::restore_from_file(&db_p0, sav_path).await {
                                Ok(count) if count > 0 => {
                                    eprintln!(
                                        "pass0 restore: {count} PVs from {}",
                                        sav_path.display()
                                    );
                                }
                                Err(e) => {
                                    eprintln!(
                                        "pass0 restore warning: {} - {e}",
                                        sav_path.display()
                                    );
                                }
                                _ => {}
                            }
                        }
                    })
                }),
            ));
        }
        // Register pass-1 restore + SaveSetConfig restore as an
        // `AfterInitDatabase` hook.
        {
            let db_p1 = db.clone();
            let files = pass1_files.clone();
            let cfg_path = autosave_config.as_ref().map(|c| c.save_path.clone());
            lifecycle_hooks.push((
                InitHookState::AfterInitDatabase,
                Box::new(move || {
                    Box::pin(async move {
                        for sav_path in &files {
                            match autosave::restore_from_file(&db_p1, sav_path).await {
                                Ok(count) if count > 0 => {
                                    eprintln!(
                                        "pass1 restore: {count} PVs from {}",
                                        sav_path.display()
                                    );
                                }
                                Err(e) => {
                                    eprintln!(
                                        "pass1 restore warning: {} - {e}",
                                        sav_path.display()
                                    );
                                }
                                _ => {}
                            }
                        }
                        if let Some(path) = cfg_path {
                            match autosave::restore_from_file(&db_p1, &path).await {
                                Ok(count) if count > 0 => {
                                    eprintln!("autosave: restored {count} PVs");
                                }
                                Err(e) => {
                                    eprintln!("autosave restore warning: {} - {e}", path.display());
                                }
                                _ => {}
                            }
                        }
                    })
                }),
            ));
        }

        // Fire an init-hook state: the C-parity sync `init_hook_*`
        // callbacks first, then the local async lifecycle hooks
        // (autosave restore). Drains every lifecycle hook matching
        // `state` out of the table so each fires exactly once.
        macro_rules! announce {
            ($state:expr) => {{
                let state = $state;
                init_hook_announce(state);
                let mut i = 0;
                while i < lifecycle_hooks.len() {
                    if lifecycle_hooks[i].0 == state {
                        let (_, hook) = lifecycle_hooks.remove(i);
                        hook().await;
                    } else {
                        i += 1;
                    }
                }
            }};
        }

        // C `iocBuild_1` announces both of these while `iocState` is still
        // `iocVoid` (`iocInit.c:122` and `:145`) and assigns `iocBuilding`
        // only after them (`:148`), so an `initHookRegister` observer at
        // `AtBeginning` reads the PRE-build state. Announcing them below the
        // assignment showed it `Building` instead — the one thing this order
        // decides that no caller can see for itself.
        announce!(InitHookState::AtIocBuild);
        // C `iocBuild_1` (`iocInit.c:129`), between the two announces above and
        // through the errlog rather than stdout, so a subscriber that captures
        // the errlog sink sees the boot start exactly as it sees every later
        // `iocInit:` line. It is what `epics_oracle_rs::ioc`'s boot classifier
        // reads to tell "the IOC began initialising" from "the process printed
        // nothing", and this port emitted it nowhere.
        crate::runtime::log::errlog_printf("Starting iocInit\n");
        announce!(InitHookState::AtBeginning);
        // C `coreRelease()` stands between the announce and the assignment
        // (`iocInit.c:147`) and writes stdout, as its `printf` does
        // (`misc/epicsRelease.c:23-27`). The wording is the iocsh command's,
        // which is C's arrangement too: one function, two callers.
        for line in crate::server::iocsh::misc_commands::core_release_block() {
            println!("{line}");
        }

        // iocBuild begins.
        set_ioc_state(IocState::Building);
        // C `scanInit` leaves the facility at `ctlPause` (`dbScan.c:199`)
        // and `iocRun`'s `scanRun` is what starts it, so nothing an
        // in-flight build wires up — an I/O Intr callback, a posted event
        // — can process a record before `initHookAfterInterruptAccept`.
        // The port's facility starts running (a bare `PvDatabase` has no
        // lifecycle to have paused it), so the build closes the gate here
        // rather than the facility opening it later.
        crate::server::scan::scan_pause();
        // C `initDatabase` registers the shutdown on the process exit list
        // (`epicsAtExit(exitDatabase, NULL)`, iocInit.c:579), and
        // `exitDatabase` is a one-line call to `iocShutdown`. Registered
        // here so `run`'s `call_at_exits()` stops the scan threads however
        // this IOC ends.
        crate::runtime::exit::at_exit("iocShutdown", || {
            ioc_shutdown();
        });
        // C `iocBuild_1` builds the callback facilities here and nowhere else
        // — `iocState = iocBuilding` then `taskwdInit(); callbackInit();`
        // immediately before this announce (`iocInit.c:148-153`). That
        // position is the whole of `callbackSetQueueSize`'s and
        // `callbackParallelThreads`'s contract: both refuse once the pool
        // exists (`callback.c:106-109`, `:162-165`) because the pool reads
        // their knobs when it is constructed, so a startup script's
        // `callbackSetQueueSize 5000` on the line before `iocInit` is only
        // honoured while nothing has constructed it earlier.
        crate::runtime::task::background_init();
        // Both of this IOC's access-security watchers spawn on the pool that
        // line just built (C `asCaStart`, reached from `asInitCommon`,
        // `asDbLib.c:147`). They are the reason moving `background_init`
        // alone changed nothing: the cell used to be created watching, and
        // that spawn built the pool before the script ran.
        access_security::start_acf_watchers(&db, &acf);
        announce!(InitHookState::AfterCallbackInit);
        announce!(InitHookState::AfterCaLinkInit);

        // External link-set installers fire at `AfterCaLinkInit` — the C
        // `initHookAfterCaLinkInit` point — so every external link set is
        // registered on the database BEFORE `setup_cp_links` (Phase 2b,
        // below) warms Passive CP/CPP holders. A link set registered later
        // (e.g. inside the Phase-3 protocol runner) is too late: the warm's
        // `resolve_external_pv` open path no-ops when no matching link set
        // is installed, so a Passive holder of an external CP link never
        // opens its monitor and never processes on a remote change. Each
        // installer also yields its iocsh commands (`caxr`/`dbcaxr`, …),
        // registered on the process's command table the moment they exist —
        // C `iocshRegister` from a registrar reached by `initHookAfterCaLinkInit`.
        // This is the point that made the table have to be one: the startup
        // shell was constructed before this line, so a per-shell table could
        // never carry these names into the script, however they were handed
        // around afterwards.
        for installer in link_set_installers {
            for cmd in installer(db.clone()).await {
                iocsh::register_command(cmd);
            }
        }

        announce!(InitHookState::AfterInitDrvSup);
        announce!(InitHookState::AfterInitRecSup);

        // Phase 2b: iocInit. C order is initDevSup() → initHookAfterInitDevSup
        // (autosave pass 0) → initDatabase() (per-record init_record, where
        // devMotorAsyn's init_controller runs). Rust's per-record device init
        // lives in `wire_device_support` — the initDatabase-era half of C's
        // init, not initDevSup (whose analogue, device-factory registration,
        // already happened) — so the pass-0 hook fires BEFORE it. A pass0-
        // restored field must land as a plain pre-init field write (C dbPut
        // before init_record); `DeviceSupport::init` then anchors/clears any
        // command state the write armed (motor `clear_last_write`). Firing the
        // hook after wiring let a restored motor VAL survive as a pending move
        // command, dispatched as a real move on the first driver-status pass —
        // and on a fast axis (VELO high) the Startup readback then caught the
        // move mid-flight and synced the instantaneous position into VAL/DVAL.
        announce!(InitHookState::AfterInitDevSup);
        let record_count =
            wire_device_support(&db, &device_factories, &dynamic_device_factory).await?;
        // Retain the registry in the database for runtime re-resolution
        // (aSub LFLG=READ / SUBL); `wire_subroutines` then performs the
        // static init-time SNAM resolution (C `init_record`).
        db.install_subroutine_registry(subroutine_registry.clone())
            .await;
        wire_subroutines(&db, &subroutine_registry).await;
        let io_intr_count = setup_io_intr(db.clone()).await;
        setup_property_posts(db.clone()).await;
        // C `dbInitLink`'s locality decision, committed once for the whole
        // database now that every record has loaded: a `Db` link naming a
        // record this IOC does not have becomes a `Ca` link
        // (`dbLink.c:118-130` falling through `dbDbInitLink`'s
        // `S_db_notFound`, `dbDbLink.c:94-96`). Runs BEFORE `setup_cp_links`
        // for C's reason — `initPVLinks` initialises links before anything
        // consumes them — and it is a one-shot: C guards re-entry with
        // `DBLINK_FLAG_INITIALIZED` (`dbLink.c:96-100`), so a record added
        // later at runtime does not un-convert a link already made external.
        db.initialize_link_locality().await;
        db.setup_cp_links().await;
        // Open the rest of the external links at init, as C does. Every
        // non-local `PV_LINK` reaches `dbCaAddLink` from `dbInitLink`
        // (`dbLink.c:118-130`) regardless of direction or CP/CPP policy, so a
        // C IOC's first scan finds an already-connecting channel.
        // `setup_cp_links` above covers only the CP/CPP subset; without this
        // pass every other external `INP`/`OUT`/`DOL`/`TSEL`/`SDIS`/`INPA..`
        // link pays one cold scan cycle to stage its own open. Runs here — the
        // same init phase, after link parsing and before scan start — and
        // after `setup_cp_links` so the `Db`→`Ca` rewrite it applies to
        // non-local CP holders is already visible to the enumeration.
        db.setup_external_link_opens().await;

        // Phase 2b.5: wait for the CA links to local records to connect
        // before PINI runs (epics-base PR #768/#856 — `dbCa: iocInit
        // wait`). This is a CA-facility wait only: `pva://` links and
        // non-local CA links open in the background and never block
        // iocInit (pvxs parity — pvalink `linkGlobal_t::init` just opens
        // channels). Default 10s timeout, override via
        // `EPICS_RS_INIT_LINK_TIMEOUT` (seconds, fractional accepted).
        // Pass-through when no CA link set is registered.
        let link_wait_secs = crate::runtime::env::get("EPICS_RS_INIT_LINK_TIMEOUT")
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(10.0)
            .max(0.0);
        if link_wait_secs > 0.0 {
            let (connected, total) = db
                .wait_for_external_links(crate::runtime::time::duration_from_secs(link_wait_secs))
                .await;
            if total > 0 {
                if connected == total {
                    eprintln!("iocInit: {connected}/{total} external links connected");
                } else {
                    let unconnected = db.unconnected_external_links().await;
                    eprintln!(
                        "iocInit: {connected}/{total} external links connected after \
                         {link_wait_secs}s — proceeding without: {}",
                        unconnected.join(", ")
                    );
                }
            }
        }

        // C: initDatabase() then initHookAfterInitDatabase (autosave
        // pass 1). The registered hook performs pass-1 + SaveSetConfig
        // restore.
        //
        // The `iocInit` barrier runs as part of initDatabase: every record the
        // startup script loaded now exists, so the link-status classifications
        // queued during the load run here, against the complete database. A
        // no-op when the script already spelled `iocInit` out.
        db.ioc_init().await;
        announce!(InitHookState::AfterInitDatabase);
        announce!(InitHookState::AfterFinishDevSup);

        // C `iocBuild_2` (`iocInit.c:186-191`): `scanInit()`, then `asInit()`,
        // then `initHookAfterScanInit`. THIS is the second caller of
        // `asInitCommon`, and the port had only the iocsh command — so
        // `softioc-rs` queued a literal `asInit` line to stand in for it,
        // which ran before the startup script rather than after it and left
        // an `st.cmd` that names its own ACF with access security off.
        //
        // A non-zero `asInit` fails the build in C, and the two lines it then
        // writes are [`as_init_failed_message`]. Nothing here re-words the
        // REASON: C `asInitFile` reports an unreadable or unparseable ACF
        // itself, on stderr, and hands `asInitCommon` only a status
        // (`asLibRoutines.c:174-190`), which is why `as_init` returns an
        // outcome and not a message.
        let as_init = crate::server::iocsh::as_init(&acf);
        if let Some(message) = as_init.message() {
            println!("{message}");
        }
        if as_init.failed() {
            crate::runtime::log::errlog_printf(&as_init_failed_message(
                crate::runtime::log::errlog_console_paints(),
            ));
            // `iocBuild_2` returns -1, so `iocBuild` does, so `iocInit()`
            // does — and a non-zero `iocInit()` is REPORTED, not fatal:
            // C `softMain.cpp:239-243` prints one line and falls through to
            // the same tail the never-built arm takes. Measured against
            // R7.0.10.1-DEV with an unreadable ACF: interactive reaches the
            // prompt and exits 0 on EOF, `-S` stays alive and listens on
            // nothing, because `iocRun` — which starts RSRV — never ran.
            //
            // The line is softMain's, and softMain's tail already lives here
            // ([`run_uninitialized_tail`]); a return channel out of
            // `run_phased` would only move the same bytes further from the
            // moment C writes them, which is before the shell starts.
            // Straight to stderr as C's `std::cerr` is, so `ERL_ERROR`'s
            // escapes survive a non-terminal stream exactly as C's do.
            return Ok(BuildOutcome::AsInitFailed);
        }

        announce!(InitHookState::AfterScanInit);

        // Phase 2b.6: process PINI=YES records BEFORE the protocol
        // runner can accept client connections (H2 — match C's
        // iocBuild ordering: initialProcess() runs inside iocBuild,
        // before iocRun starts the CA server). Without this, a CA
        // client connecting in the first moments after IOC start
        // could `caget` a PINI record's UDF/default value instead of
        // its processed value. C guarantees this cannot happen.
        {
            // C `initialProcess()` (iocInit.c:653-657) — `piniProcess(menuPiniYES)`.
            db.pini_process(crate::server::record::PiniMode::Yes).await;
            // Publish completion: a later-started scan owner (or a
            // non-owner scheduler) sees PINI as already done — the
            // owner branch then skips its own PINI pass (exactly-once,
            // as C's `initialProcess`) and non-owners run their hooks
            // without blocking.
            db.mark_pini_done();
        }
        announce!(InitHookState::AfterInitialProcess);

        // Phase 2d: Build AutosaveManager from startup config
        let autosave_manager = if let Some(builder) = builder_opt {
            // `build` cannot fail: a set it could not construct is reported
            // on the error log and carried as that set's error status, so
            // one bad `.req` file no longer costs the IOC every other set.
            let mgr = builder.build().await;
            eprintln!("autosave: {} save set(s) configured", mgr.set_names().len());
            Some(Arc::new(mgr))
        } else {
            None
        };

        let total_records = db.all_record_names().await.len();
        eprintln!(
            "iocInit: {total_records} records, {record_count} with device support, {io_intr_count} I/O Intr"
        );

        // C: rsrv init / iocBuild end. The Rust CA/PVA listener is
        // owned by the protocol runner, but PINI is already complete
        // (Phase 2b.6) so announcing here keeps hook ordering correct
        // for consumers that key off these states.
        announce!(InitHookState::AfterCaServerInit);
        announce!(InitHookState::AfterIocBuilt);
        // C `iocBuild` ends here, and the IOC is quiescent — `iocBuilt` is
        // the state `iocRun` below is legal from.
        set_ioc_state(IocState::Built);

        Ok(BuildOutcome::Built(Box::new(BuiltIoc {
            db,
            autosave_manager,
            after_init_hooks,
            protocol,
        })))
    }
}

impl BuiltIoc {
    /// C `iocRun()` (`iocInit.c:250-277`) over the IOC this value owns.
    ///
    /// Consuming `self` is what keeps the transition exactly once: a
    /// `BuiltIoc` is produced only by [`IocBuild::perform_build`] and
    /// destroyed only here, so a second `iocRun` has no built IOC to run and
    /// the question never becomes a runtime check.
    async fn run(self) -> RunningIoc {
        let Self {
            db,
            autosave_manager,
            after_init_hooks,
            protocol,
        } = self;

        // C `piniProcessHook` (iocInit.c:629-646): the hook registered by
        // `initialProcess()` runs `piniProcess(menuPiniRUN)` when
        // `initHookAtIocRun` is announced. It is a hook CONSUMER in C, not
        // part of `iocRun`'s body; the port's hook table is synchronous
        // and `pini_process` is not, so the pass runs here, immediately
        // before the transition that announces the state it keys on.
        db.pini_process(crate::server::record::PiniMode::Run).await;
        // Periodic scanning is owned by the IOC core, not by any protocol
        // server — a PVA-only or server-less IOC scans all the same. The
        // owner's PINI=YES pass is skipped (Phase 2b.6 already ran it and
        // published completion); the `try_claim_scan_start` claim it takes
        // keeps a protocol runner or embedded harness that starts another
        // owner parked and harmless.
        //
        // Handed to the lifecycle rather than held in a local: `iocShutdown`
        // is what stops the scan threads now, and `run` reaches it through
        // the `epicsAtExit` registration below on every exit path — which
        // the local could only do by returning from this function.
        // C `iocInit` is `iocBuild() || iocRun()` (iocInit.c:107-110), and
        // starting the owner is that `iocRun`: `ScanOwner::start` calls
        // [`note_scan_owner_started`], which runs the same transition the
        // shell's `iocRun` runs. One caller, so the two cannot announce
        // different things.
        adopt_scan_owner(crate::server::scan::ScanOwner::start(db.clone()));
        // C `piniProcessHook` at `initHookAfterIocRunning` (iocInit.c:637-639)
        // — `piniProcess(menuPiniRUNNING)`; a hook consumer for the same
        // reason as the PINI=RUN pass above, run immediately after the
        // transition that announces its state.
        db.pini_process(crate::server::record::PiniMode::Running)
            .await;

        // H3: drain `after_init_hooks` HERE — a guaranteed drain
        // point inside `run`. These were previously moved into
        // `IocRunConfig.after_init_hooks` and silently dropped
        // unless the external protocol runner remembered to execute
        // the vector. `register_after_init` promises "run after
        // iocInit completes"; PINI is done and the database is
        // built, so this is the correct C `initHookAfterIocRunning`
        // equivalent point. The `IocRunConfig.after_init_hooks`
        // field is now always handed over EMPTY (kept for API
        // compatibility) so a runner cannot double-run them.
        for hook in after_init_hooks {
            hook();
        }

        // C `iocRun` starts the servers HERE — `if (iocBuildMode ==
        // buildServers) { dbRunServers(); initHookAnnounce(
        // initHookAfterCaServerRunning); }` (`iocInit.c:265-268`), which
        // reaches `rsrv_run` (`caservertask.c:766-771`) and RETURNS, leaving
        // the CA server threads listening while softMain goes on to its
        // interactive `iocsh`. The port had the runner awaited by
        // `run_to_completion` instead, so the servers could not exist until
        // the whole startup script had finished: measured, a `st.cmd` of
        // `iocInit` then `casr` printed nothing and `ss` showed no listening
        // socket where C shows one.
        //
        // Spawned rather than awaited, for the same reason `rsrv_run` returns:
        // this transition is a statement in the middle of a script, not the
        // tail of the process. `ProtocolServer` is what keeps that from being
        // a detached task.
        let ProtocolStart {
            bridge,
            port,
            tcp_port,
            acf,
            autosave_config,
            runner,
        } = protocol;
        let config = IocRunConfig {
            db,
            port,
            tcp_port,
            acf,
            autosave_config,
            autosave_manager,
            // Retained for the runners that build an `IocRunConfig` by hand
            // (`run_ca_ioc`, `run_pva_ioc`, `softioc-rs`): it is how THEY get
            // their own `casr`/`caxr` onto the table. `IocApplication::run`
            // has already registered everything it owns on the process's one
            // command table, so it hands the field over EMPTY rather than
            // asking the runner to register the same names again.
            shell_commands: Vec::new(),
            // Drained above. Handed over empty so a protocol runner that still
            // inspects the field cannot double-run the hooks.
            after_init_hooks: Vec::new(),
        };
        // C's `iocRun` does not return until every layer is up, because
        // `dbRunServers()` is a plain call: `rsrv_run` flips the control words
        // and returns (`caservertask.c:766-771`) over sockets `dbInitServers()`
        // bound one phase earlier. Spawning the runner alone does not
        // reproduce that — the next script line would race the runner's own
        // bind and its `casr` registration, and a measured `ss` of 1 was a race
        // that happened to win. So the generation is sampled BEFORE the spawn
        // and awaited after it: the serve entry of each protocol server
        // announces (see [`crate::server::db_server::announce_serving`]), and
        // this line is where C's ordering is restored.
        let generation = crate::server::db_server::serving_generation();
        let mut server = ProtocolServer {
            handle: bridge.spawn(runner(config)),
            finished: None,
            live: true,
        };
        server.await_serving(generation).await;

        RunningIoc { server }
    }
}

/// IOC Application with st.cmd-style startup support.
pub struct IocApplication {
    port: u16,
    /// Optional TCP listen port override. `None` means "share with UDP
    /// discovery port". Set via [`Self::tcp_port`] or the
    /// `EPICS_CAS_SERVER_PORT` env var (resolved at run time).
    tcp_port: Option<u16>,
    device_factories: HashMap<String, DeviceSupportFactory>,
    dynamic_device_factory: Option<DynamicDeviceSupportFactory>,
    record_factories: HashMap<String, super::RecordFactory>,
    subroutine_registry: HashMap<String, Arc<SubroutineFn>>,
    acf: Option<access_security::AccessSecurityConfig>,
    autosave_config: Option<autosave::SaveSetConfig>,
    autosave_startup: Option<Arc<Mutex<AutosaveStartupConfig>>>,
    /// Every iocsh command this application contributes, in registration
    /// order. One list because there is one command table: the two public
    /// `register_*` methods differ only in the name a caller reads, and a
    /// command that existed in one shell and not the other was the defect
    /// they encoded.
    commands: Vec<CommandDef>,
    startup_script: Option<String>,
    /// Command lines run on the startup shell BEFORE the startup script,
    /// in the order queued. C `softMain.cpp:192-198,216-222` builds the
    /// same list out of `-d` and `-x` while it is still reading argv, so
    /// the records they load are already in the database when the script's
    /// first line runs.
    startup_lines: Vec<String>,
    /// Simple PVs added via the declarative builder.
    inline_pvs: Vec<(String, crate::types::EpicsValue)>,
    /// Records added via the declarative builder (Phase 7).
    inline_records: Vec<(String, Box<dyn Record>)>,
    /// Callbacks invoked after iocInit completes (e.g., start pollers).
    after_init_hooks: Vec<Box<dyn FnOnce() + Send>>,
    /// Async external link-set installers (CA links via `epics-ca-rs`'s
    /// `calink`, PVA links via the bridge's `pvalink`). Fired at the
    /// `AfterCaLinkInit` hook in [`Self::run`] — before `setup_cp_links`
    /// — so a Passive holder of an external CP link warms at iocInit.
    link_set_installers: Vec<LinkSetInstaller>,
    /// C softMain's own turn between the startup script and `iocInit`
    /// ([`Self::before_ioc_init`]). `None` is [`IocInitDecision::run`].
    ioc_init_gate: Option<Box<dyn FnOnce() -> IocInitDecision + Send>>,
}

impl IocApplication {
    pub fn new() -> Self {
        // No context-free built-in device support: every base builtin
        // (`Soft Timestamp`, `stdio`, `Db State`, `getenv`) needs the record's
        // INST_IO `INP`/`OUT`, which only the dynamic factory's
        // `DeviceSupportContext` carries — so all base builtins are dispatched
        // below, and this static map starts empty (users register their own
        // context-free device support into it via `register_device_support`).
        let device_factories: HashMap<String, DeviceSupportFactory> = HashMap::new();
        Self {
            // SERVER-side port: caservertask.c:492-499 honours
            // EPICS_CAS_SERVER_PORT > EPICS_CA_SERVER_PORT > 5064.
            port: cas_server_port(),
            tcp_port: None,
            device_factories,
            // The base built-in device support — all needing the runtime
            // context (INP/OUT). Pre-registered as the base of the
            // dynamic-factory chain so a user's
            // `register_dynamic_device_support` factory takes priority and
            // falls through to here.
            dynamic_device_factory: Some(Box::new(
                crate::server::builtin_devices::builtin_dynamic_factory,
            )),
            record_factories: HashMap::new(),
            subroutine_registry: HashMap::new(),
            acf: None,
            autosave_config: None,
            autosave_startup: None,
            commands: Vec::new(),
            startup_script: None,
            startup_lines: Vec::new(),
            inline_pvs: Vec::new(),
            inline_records: Vec::new(),
            after_init_hooks: Vec::new(),
            link_set_installers: Vec::new(),
            ioc_init_gate: None,
        }
    }

    /// Set the UDP discovery port (default: 5064).
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Set the TCP listen port independently from the UDP discovery
    /// port (epics-base PR #69, `EPICS_CAS_SERVER_PORT`). Multiple IOCs
    /// on one host can each bind a unique TCP port while sharing the
    /// canonical 5064 UDP search port. When unset, the IOC resolves it
    /// at run time from `EPICS_CAS_SERVER_PORT`; if that's also unset,
    /// the TCP listener inherits [`Self::port`].
    pub fn tcp_port(mut self, port: u16) -> Self {
        self.tcp_port = Some(port);
        self
    }

    /// Register a device support factory by DTYP name.
    /// Called during iocInit to wire device support to records.
    pub fn register_device_support<F>(mut self, dtyp: &str, factory: F) -> Self
    where
        F: Fn() -> Box<dyn DeviceSupport> + Send + Sync + 'static,
    {
        self.device_factories
            .insert(dtyp.to_string(), Box::new(factory));
        self
    }

    /// Register a dynamic device support factory.
    ///
    /// Called as a fallback when a record's DTYP doesn't match any
    /// statically registered factory. The closure receives the DTYP name
    /// and returns `Some(device_support)` if it can handle that DTYP.
    ///
    /// Multiple calls are chained: new factory is tried first, then existing.
    pub fn register_dynamic_device_support<F>(mut self, factory: F) -> Self
    where
        F: Fn(&DeviceSupportContext) -> Option<Box<dyn DeviceSupport>> + Send + Sync + 'static,
    {
        if let Some(existing) = self.dynamic_device_factory.take() {
            self.dynamic_device_factory = Some(Box::new(move |ctx: &DeviceSupportContext| {
                factory(ctx).or_else(|| existing(ctx))
            }));
        } else {
            self.dynamic_device_factory = Some(Box::new(factory));
        }
        self
    }

    /// Register an iocsh command on this application.
    ///
    /// C has one command table and one `iocshRegister`, so a registered name
    /// is callable from `st.cmd` and from the `epics>` prompt alike; this
    /// method and [`Self::register_shell_command`] are the same registration
    /// and are kept apart only because callers spell both.
    pub fn register_startup_command(mut self, cmd: CommandDef) -> Self {
        self.commands.push(cmd);
        self
    }

    /// [`Self::register_startup_command`] under its other name. Registering a
    /// command twice, once through each, is the workaround the two shells used
    /// to need and now displaces the name with itself.
    pub fn register_shell_command(mut self, cmd: CommandDef) -> Self {
        self.commands.push(cmd);
        self
    }

    /// The commands this application registers, in registration order.
    ///
    /// This is the surface a startup script is executed against — a command
    /// missing here is a fatal unknown command in `st.cmd`, before `iocInit`.
    /// Exposed so a pre-configured IOC (e.g. `AdIoc`) can be checked against
    /// the script it promises to run without booting a server. `CommandDef` is
    /// `Clone`, so a caller may also install these on an [`iocsh::IocShell`] of
    /// its own to exercise a script.
    pub fn startup_commands(&self) -> &[CommandDef] {
        &self.commands
    }

    /// Register a callback to run after iocInit completes.
    ///
    /// Use this to start pollers and other periodic tasks that should
    /// not run during st.cmd execution or autosave restore.
    ///
    /// [`Self::run`] guarantees these fire — they are drained inside
    /// `run` at the `initHookAfterIocRunning` point (after PINI
    /// processing, before handoff to the protocol runner). They are
    /// NOT delegated to the protocol runner, so a custom runner does
    /// not need to remember to drain them.
    pub fn register_after_init(mut self, hook: impl FnOnce() + Send + 'static) -> Self {
        self.after_init_hooks.push(Box::new(hook));
        self
    }

    /// Register an async external link-set installer.
    ///
    /// The installer is invoked by [`Self::run`] at the C
    /// `initHookAfterCaLinkInit` point — BEFORE
    /// [`PvDatabase::setup_cp_links`] warms Passive CP holders. It
    /// registers its external [`crate::server::database::LinkSet`] on the
    /// live database and returns any iocsh commands it owns, which [`Self::run`]
    /// registers on the process's command table as it receives them.
    ///
    /// This is the seam that makes external record links resolve by
    /// construction: registering the link set inside the Phase-3 protocol
    /// runner is too late, because `setup_cp_links`'s `resolve_external_pv`
    /// warm has already run and found no matching link set, so a Passive
    /// holder of an external CP/CPP link never opens its monitor. A
    /// CA-serving IOC wires `epics_ca_rs::calink::calink_link_set_install`
    /// here so CA links resolve with no further setup.
    pub fn register_link_set_installer<F, Fut>(mut self, installer: F) -> Self
    where
        F: FnOnce(Arc<PvDatabase>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Vec<CommandDef>> + Send + 'static,
    {
        self.link_set_installers
            .push(Box::new(move |db| Box::pin(installer(db))));
        self
    }

    /// C softMain's own turn between the startup script and `iocInit`
    /// (`softMain.cpp:236-245`).
    ///
    /// The one instant at which a caller can both observe that the script
    /// has finished and decide whether the IOC initialises at all — C has
    /// it because `iocsh(st.cmd)` and `iocInit()` are two statements in its
    /// `main`, and this port had collapsed them into one call. Unset means
    /// [`IocInitDecision::run`], so an application that never had C's
    /// `loadedDb` question boots exactly as it did before.
    ///
    /// The gate runs on the lifecycle's own task, after the startup shell
    /// has been joined, so anything it prints lands between the script's
    /// last line and the build's first.
    pub fn before_ioc_init(
        mut self,
        gate: impl FnOnce() -> IocInitDecision + Send + 'static,
    ) -> Self {
        self.ioc_init_gate = Some(Box::new(gate));
        self
    }

    /// Set the startup script path (executed before iocInit).
    pub fn startup_script(mut self, path: &str) -> Self {
        self.startup_script = Some(path.to_string());
        self
    }

    /// Queue one iocsh command line to run before the startup script.
    ///
    /// The pre-`iocInit` half of C's argv handling: `-d file.db` IS
    /// `dbLoadRecords("file.db", "macros")` called before `iocsh(st.cmd)`,
    /// and `-a`/`-x` are the same shape. Expressing those flags as lines on
    /// the startup shell keeps ONE loader — the command the script would
    /// have called itself — instead of a second implementation reachable
    /// only from the command line. A line that fails ends the boot with
    /// [`IocRunFailure::StartupCommand`], C's `errIf(..., "")`.
    pub fn startup_line(mut self, line: &str) -> Self {
        self.startup_lines.push(line.to_string());
        self
    }

    /// Register a record type factory (e.g., "motor", "asyn").
    /// Avoids the global registry — factories are passed to IocBuilder.
    pub fn register_record_type<F>(mut self, type_name: &str, factory: F) -> Self
    where
        F: Fn() -> Box<dyn Record> + Send + Sync + 'static,
    {
        let factory: super::RecordFactory = Box::new(factory);
        super::db_loader::snapshot_declared_fields(type_name, &factory);
        self.record_factories.insert(type_name.to_string(), factory);
        self
    }

    /// Register a subroutine function by name (for sub/aSub records).
    /// The closure returns the C `long` status (`Ok(0)` normal, `Ok(n<0)`
    /// raises `SOFT_ALARM`/`BRSV`; `aSub` publishes it as `VAL`).
    pub fn register_subroutine<F>(mut self, name: &str, func: F) -> Self
    where
        F: Fn(&mut dyn Record) -> CaResult<i64> + Send + Sync + 'static,
    {
        self.subroutine_registry
            .insert(name.to_string(), Arc::new(Box::new(func)));
        self
    }

    /// Configure autosave with a save set configuration.
    pub fn autosave(mut self, config: autosave::SaveSetConfig) -> Self {
        self.autosave_config = Some(config);
        self
    }

    /// Configure autosave startup (C-compatible iocsh commands).
    ///
    /// When set, autosave iocsh commands (`set_requestfile_path`, `create_monitor_set`,
    /// `set_pass0_restoreFile`, etc.) are registered as startup commands and populate
    /// the config during st.cmd execution. After iocInit, the config is consumed to
    /// build an `AutosaveManager`.
    pub fn autosave_startup(mut self, config: Arc<Mutex<AutosaveStartupConfig>>) -> Self {
        self.autosave_startup = Some(config);
        self
    }

    /// Configure access security.
    pub fn acf(mut self, config: access_security::AccessSecurityConfig) -> Self {
        self.acf = Some(config);
        self
    }

    // --- Declarative IOC Builder (Phase 7) ---

    /// Add a typed record to the IOC (no .db file needed).
    ///
    /// ```rust,ignore
    /// IocApplication::new()
    ///     .record("sensor:temp", AiRecord::new(0.0))
    ///     .record("heater:sp", AoRecord::new(0.0))
    ///     .run(my_runner).await
    /// ```
    pub fn record(mut self, name: &str, record: impl Record) -> Self {
        self.inline_records
            .push((name.to_string(), Box::new(record)));
        self
    }

    /// Add a pre-boxed record.
    pub fn record_boxed(mut self, name: &str, record: Box<dyn Record>) -> Self {
        self.inline_records.push((name.to_string(), record));
        self
    }

    /// Add a simple PV, created before the startup script runs.
    ///
    /// Same instant as [`Self::record`], and before it, which is the order
    /// `IocBuilder::build` uses for the same two sources.
    pub fn pv(mut self, name: &str, initial: crate::types::EpicsValue) -> Self {
        self.inline_pvs.push((name.to_string(), initial));
        self
    }

    /// Run the full IOC lifecycle: startup script -> iocInit -> the tail.
    ///
    /// The `protocol_runner` closure receives an [`IocRunConfig`] containing the
    /// fully initialized database, port, and configuration. It is responsible for
    /// starting the protocol-specific server (e.g., CA, PVA) and the interactive
    /// shell. It is SPAWNED by `BuiltIoc::run`, at C's `iocRun` ->
    /// `dbRunServers()` point (`iocInit.c:265-267`), so the servers are up
    /// while the rest of the startup script runs; this function then waits for
    /// it the way softMain waits on its `iocsh(NULL)`.
    /// Every way out of the IOC — the runner finishing, a signal, a failure
    /// during load or `iocInit` — runs the process's exit callbacks before
    /// returning. That is C's arrangement: `softIoc`'s `main` reaches all six
    /// of its exits through `epicsExit(status)` (`softMain.cpp:167`, `:172`,
    /// `:251`, `:265`, `:270`, `:277`), and `epicsExit` runs the list first
    /// (`epicsExit.c:172-177`).
    ///
    /// This wrapper is where that becomes structural rather than remembered:
    /// the whole lifecycle sits in `Self::run_to_completion`, whose every
    /// `?` returns *here*, so no exit path can be added later that skips the
    /// teardown. Ports registered themselves at creation
    /// (`asyn`'s `create_port_runtime`, C's `registerPort` at
    /// `asynManager.c:2097`), so this is where a driver's `Drop` finally runs
    /// and its device gets the goodbye it is owed.
    ///
    /// [`crate::runtime::exit::call_at_exits`] runs the list once per process,
    /// so a second `run` — a test that boots two IOCs — tears down what the
    /// first left, not what the first already tore down.
    pub async fn run<F, Fut>(self, protocol_runner: F) -> CaResult<()>
    where
        F: FnOnce(IocRunConfig) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = CaResult<()>> + Send + 'static,
    {
        self.run_phased(protocol_runner)
            .await
            .map_err(CaError::from)
    }

    /// [`Self::run`], reporting which phase of the lifecycle failed.
    ///
    /// For a caller that has to reproduce C softIoc's exit statuses, where
    /// the same `CaError` means 2 before the protocol runner starts and 1
    /// after it (`softMain.cpp:247-279`). Everything else wants [`Self::run`],
    /// which throws the phase away.
    pub async fn run_phased<F, Fut>(self, protocol_runner: F) -> Result<(), IocRunFailure>
    where
        F: FnOnce(IocRunConfig) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = CaResult<()>> + Send + 'static,
    {
        let result = self.run_to_completion(protocol_runner).await;
        crate::runtime::exit::call_at_exits();
        result
    }

    /// The IOC lifecycle itself. Private, and reached only through
    /// [`Self::run`], because leaving it by any route has to run the exit
    /// callbacks and only that wrapper does.
    async fn run_to_completion<F, Fut>(self, protocol_runner: F) -> Result<(), IocRunFailure>
    where
        F: FnOnce(IocRunConfig) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = CaResult<()>> + Send + 'static,
    {
        let db = Arc::new(PvDatabase::new());
        // Everything from here to the `db.ioc_init()` barrier below is C's
        // pre-`iocInit` load: inline records, then the `st.cmd`'s
        // `dbLoadRecords` calls. Records created in it queue their link-status
        // classification instead of running it against a database that is still
        // being built (R18-92).
        db.begin_load()
            .expect("a database created a line ago has not run iocInit");

        let bridge = crate::runtime::task::BlockingBridge::capture();

        let Self {
            port,
            tcp_port,
            device_factories,
            dynamic_device_factory,
            record_factories,
            subroutine_registry,
            acf,
            autosave_config,
            autosave_startup,
            mut commands,
            startup_script,
            startup_lines,
            inline_pvs,
            inline_records,
            after_init_hooks,
            link_set_installers,
            ioc_init_gate,
        } = self;

        // The IOC's single live policy cell, created BEFORE the startup
        // script runs so the script's `asInit` and the servers built
        // afterwards observe the same store (upstream issue #667
        // adjacent: a config that only lands in a shell-local copy is
        // access security silently OFF).
        // The cell only. Its two watcher tasks — the ASG `INP*` monitor and
        // the HAG DNS refresher — run on the callback pool, so starting them
        // here would build that pool before the script's first line and take
        // `callbackSetQueueSize` away from it; `perform_build` starts them at
        // C's `callbackInit` point instead. Nothing serves this database
        // until Phase 3, so the unwatched window is not one in which a
        // client can hold a stale grant.
        let acf = access_security::new_acf_cell(acf);

        // Register record type factories with global registry so dbLoadRecords
        // (called from st.cmd) can find them. This bridges the injected factories
        // to the global registry that the iocsh dbLoadRecords command uses.
        for (name, factory) in record_factories {
            super::db_loader::register_record_type(&name, factory);
        }

        // Register autosave startup commands if configured
        if let Some(ref config) = autosave_startup {
            let cmds = AutosaveStartupConfig::register_startup_commands(config.clone());
            commands.extend(cmds);
        }

        // Register the QSRV `dbLoadGroup` startup command so a
        // pvxs-compatible st.cmd can queue group definition files before
        // iocInit (pvxs registers it from its registrar before the
        // startup script). The QSRV protocol runner drains the queue and
        // applies it to the served provider; a non-QSRV runner never
        // drains it, leaving the command a harmless no-op.
        commands.push(db_load_group_startup_command());

        // C has every command registered before it reads a script: the
        // registrars run from `registerRecordDeviceDriver` and `dbLoadDatabase`,
        // which softMain calls before `iocsh(st.cmd)` (`softMain.cpp:181-232`).
        // One registration onto the one table, so each of the shells below —
        // the script's, the `afterIocRunning` queue's, the interactive tail's,
        // the protocol runner's — has the name without being handed it.
        for cmd in commands {
            iocsh::register_command(cmd);
        }

        // Add inline PVs then inline records — `IocBuilder::build`'s order
        // for the same two sources, and before the script for C's reason:
        // everything argv named is in the database when the script starts.
        for (name, value) in inline_pvs {
            db.add_pv(&name, value).await?;
        }
        for (name, record) in inline_records {
            db.add_record(&name, record).await?;
        }

        // Arm the build BEFORE the script runs. That is what gives `iocInit`
        // one meaning: the script's own `iocInit` line performs this build, so
        // the line after it — a `dbpf`, a `dbl`, an `asSetFilename` — runs
        // against an IOC that has device support, scan threads and PINI behind
        // it. When the script never spells `iocInit`, the turn after Phase 1
        // performs the same build instead.
        arm_build(IocBuild {
            db: db.clone(),
            acf: acf.clone(),
            autosave_config: autosave_config.clone(),
            autosave_startup,
            device_factories,
            dynamic_device_factory,
            subroutine_registry,
            link_set_installers,
            after_init_hooks,
            // C parity (`caservertask.c:492-500`): the server-side env var
            // EPICS_CAS_SERVER_PORT sets `ca_server_port`, and `ca_udp_port =
            // ca_server_port` — so UDP and TCP bind the same value unless the
            // Rust-extension `.tcp_port(...)` explicitly splits them. The
            // `port` field already incorporates the CAS / CA / default
            // precedence via `cas_server_port()` (see `IocApplication::new`);
            // `tcp_port` stays `Some(...)` only when the caller explicitly
            // invoked `.tcp_port(...)`.
            protocol: ProtocolStart {
                bridge: bridge.clone(),
                port,
                tcp_port,
                acf: acf.clone(),
                autosave_config,
                runner: Box::new(move |config| Box::pin(protocol_runner(config))),
            },
        });
        // From here the static can hold a running IOC, and a running IOC owns
        // a spawned protocol runner. Nothing may return past this line without
        // that value being dropped.
        let _armed = ArmedLifecycle;

        // Phase 1: Execute the queued command lines and then the startup
        // script, on ONE shell, in a separate std::thread. std::thread (not
        // spawn_blocking) is required because iocsh commands use
        // Handle::block_on() which panics inside the tokio runtime context.
        //
        // One shell for both because C uses one iocsh context too: a
        // `dbLoadRecords` from `-d` and one from the script are the same
        // call, so a `dbPutAttribute` or `dbLoadTemplate` queued here must
        // be visible to the script exactly as it is to a later script line.
        if startup_script.is_some() || !startup_lines.is_empty() {
            // C's `iocsh(pathname)` returns before `iocsh(NULL)` is reached
            // (`softMain.cpp:231`, `:250`). The script's `iocInit` line now
            // starts the protocol runner, whose interactive shell is a thread
            // of its own, so that ordering has to be held rather than implied
            // — see `iocsh::STARTUP_SCRIPT_PHASE`. A guard, so a failed load
            // ends the phase too.
            let _script_phase = iocsh::startup_script_phase();
            let script = startup_script;
            let db1 = db.clone();
            let b1 = bridge.clone();
            let acf1 = acf.clone();

            let (tx, rx) = crate::runtime::sync::oneshot::channel();
            // Mandatory: the startup script is what loads this IOC's database.
            // Booting on without it would serve an empty or half-loaded IOC.
            // `try_spawn` rather than `spawn` because this *is* a fallible boot
            // step — the error reaches `run`'s caller, which then never starts
            // serving, so there is no need to abort the process.
            crate::runtime::task::MandatoryThread::new(
                "iocsh-startup",
                // C bands the thread that runs iocsh, and for the reason
                // this thread has too — see `iocsh_threads_take_the_iocsh_band`.
                crate::runtime::task::ThreadPriority::Iocsh,
                // The shell runs arbitrary registered commands, which reach
                // record processing and device support — the same depth the
                // callback bands get, so the same class they use.
                crate::runtime::task::StackSizeClass::Big,
            )
            .try_spawn(move || {
                let shell = iocsh::IocShell::new_with_acf(db1, b1, acf1);
                let _ = tx.send(run_startup_phase(&shell, &startup_lines, script.as_deref()));
            })
            .map_err(|e| {
                CaError::InvalidValue(format!("could not start the iocsh-startup thread: {e}"))
            })?;

            rx.await
                .map_err(|_| CaError::InvalidValue("startup thread dropped".into()))??;
        }

        // C softMain's turn between `iocsh(st.cmd)` and `iocInit()`
        // (`softMain.cpp:236-245`). Outside the `if` above because C asks
        // its question whatever the flags were — a `softIoc` with neither a
        // script nor a `-d` still reaches this line, and is exactly the case
        // that must NOT build.
        // `None` is an application that never installed the gate, and that is
        // not the same as answering `Run`: such a caller is not C softMain,
        // has not answered C's `-S`, and owns no tail of its own — the
        // protocol runner is the whole of its tail. It therefore gets a
        // failed build as an `Err` rather than a shell it never asked for.
        let decision = ioc_init_gate.map(|gate| gate());
        // Finish whatever the startup script left undone. Each arm is a state
        // the script could have stopped in, and the value it carries is this
        // `run`'s only claim on the transition out of it.
        let outcome = match take_lifecycle() {
            // The script spelled neither `iocInit` nor `iocBuild`, so this is
            // the turn that decides whether the IOC is built at all.
            Some(IocLifecycle::Armed(build)) => {
                if let Some(decision) = decision
                    && !decision.run
                {
                    return run_uninitialized_tail(db, bridge, acf, decision.interactive).await;
                }
                match build.perform_build().await? {
                    BuildOutcome::Built(built) => Ok(built.run().await),
                    BuildOutcome::AsInitFailed => Err(()),
                }
            }
            // The script spelled `iocBuild` and never `iocRun`. C's softMain
            // would call `iocInit()`, watch `iocBuild_1` refuse from a state
            // that is not `iocVoid`, and leave the IOC quiescent — bound to no
            // port and serving nothing, which is the failure this whole owner
            // exists to remove. Finish the transition the script started, and
            // say so rather than doing it silently.
            Some(IocLifecycle::Built(built)) => {
                crate::runtime::log::errlog_printf(
                    "iocInit: startup script built the IOC without running it; running it now\n",
                );
                Ok(built.run().await)
            }
            // The script's `iocInit`, or its `iocBuild` and `iocRun`, already
            // did it. There is no second build to gate and no second decision
            // to take: the script asked for the IOC to be built and run, which
            // is what makes every line after those — and the protocol runner
            // below — see a running IOC.
            Some(IocLifecycle::Running(running)) => Ok(running),
            Some(IocLifecycle::AsInitFailed) => Err(()),
            Some(IocLifecycle::Failed(e)) => return Err(e.into()),
            None => unreachable!(
                "the lifecycle owner is armed before the startup script runs and \
                 every transition puts a state back"
            ),
        };
        let running = match outcome {
            Ok(running) => running,
            // `iocBuild_2` returns -1, so `iocBuild` does, so `iocInit()`
            // does — and a non-zero `iocInit()` is REPORTED, not fatal:
            // C `softMain.cpp:239-243` prints one line and falls through to
            // the same tail the never-built arm takes. Measured against
            // R7.0.10.1-DEV with an unreadable ACF: interactive reaches the
            // prompt and exits 0 on EOF, `-S` stays alive and listens on
            // nothing, because `iocRun` — which starts RSRV — never ran.
            Err(()) => {
                return match decision {
                    Some(decision) => {
                        eprintln!("{} during iocInit()", crate::runtime::log::ERL_ERROR);
                        run_uninitialized_tail(db, bridge, acf, decision.interactive).await
                    }
                    None => Err(IocRunFailure::Startup(CaError::InvalidValue(
                        "iocBuild: asInit Failed.".into(),
                    ))),
                };
            }
        };
        // Held as a live local across everything below: the runner is already
        // serving, and a `?` from here on must stop it. `ProtocolServer`'s
        // `Drop` is what makes that true of a return this function does not
        // yet have.
        let mut server = running.server;

        // Phase 2e: drain `afterIocRunning` queue (epics-base PR #558).
        // Each line is an iocsh command queued by the startup script;
        // execute through a fresh shell so post-init state (including
        // PINI side effects) is visible. It reads the same command table
        // every other shell does, so a site-specific name like `motorReport`
        // is addressable from the post-init queue with no re-registration.
        let pending = db.take_after_ioc_running();
        if !pending.is_empty() {
            let db1 = db.clone();
            let b1 = bridge.clone();
            let acf1 = acf.clone();
            let (tx, rx) = crate::runtime::sync::oneshot::channel();
            // Mandatory for the same reason as "iocsh-startup": the queue holds
            // commands the startup script deferred to post-init, so skipping it
            // hands the operator an IOC that is missing part of its own boot.
            // Still inside `run`, so the failure propagates rather than aborts.
            crate::runtime::task::MandatoryThread::new(
                "iocsh-after-ioc-running",
                // Same reasoning as "iocsh-startup" above.
                crate::runtime::task::ThreadPriority::Iocsh,
                // Same reasoning as "iocsh-startup" above.
                crate::runtime::task::StackSizeClass::Big,
            )
            .try_spawn(move || {
                let shell = iocsh::IocShell::new_with_acf(db1, b1, acf1);
                let mut errs: Vec<String> = Vec::new();
                for line in pending {
                    // A queued line fails in either of the two ways C's
                    // `scope.errored` covers: with a diagnostic for the
                    // caller to print (`Err`), or having already printed
                    // its own (`CommandOutcome::Failed` — an unregistered
                    // command reports itself at `iocsh.cpp:1302`, and the
                    // `db*` commands that answer a bare non-zero do the
                    // same). Reading only `Err` dropped the second kind
                    // from this summary.
                    match shell.execute_line(&line) {
                        Err(e) => errs.push(format!("{line}: {e}")),
                        Ok(iocsh::registry::CommandOutcome::Failed) => {
                            errs.push(format!("{line}: failed"));
                        }
                        Ok(
                            iocsh::registry::CommandOutcome::Continue
                            | iocsh::registry::CommandOutcome::Exit,
                        ) => {}
                    }
                }
                let _ = tx.send(errs);
            })
            .map_err(|e| {
                CaError::InvalidValue(format!(
                    "could not start the iocsh-after-ioc-running thread: {e}"
                ))
            })?;
            if let Ok(errs) = rx.await {
                for e in errs {
                    eprintln!("afterIocRunning: {e}");
                }
            }
        }

        // Phase 3: what softMain does once `iocInit()` has returned — wait for
        // the process to be told to stop. The servers have been running since
        // the `iocInit` line (see `BuiltIoc::run`); what is left here is the
        // runner's own tail, which for `run_ca_ioc` and `run_pva_ioc` is the
        // interactive `iocsh(NULL)` of `softMain.cpp:250`.
        //
        // epics-base PR #671 parity: race it against SIGTERM/SIGINT so a `kill`
        // (or Ctrl+C on the controlling terminal) cleanly returns Ok(()) instead
        // of leaving the future suspended forever. The CA/PVA runners already
        // wire their own signal handlers when used standalone; this one covers
        // the `IocApplication::run` entry point where the runner closure may
        // not (e.g., a custom user runner that only sleeps on `pending()`).
        // SIGINT/SIGTERM racing is host-only: `tokio::signal` needs the tokio
        // `signal` feature (signal-hook-registry + mio), which is dropped for
        // both embedded targets (RTEMS, VxWorks). On either, both arms are
        // `pending()`, so `run` simply awaits the runner; process-signal
        // shutdown is the embedded driver's concern (a later increment).
        // Both embedded targets are `cfg(unix)` too, so the guard is
        // `all(unix, not(epics_embedded_target))`, not `unix` alone.
        #[cfg(not(epics_embedded_target))]
        let ctrl_c = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        #[cfg(epics_embedded_target)]
        let ctrl_c = std::future::pending::<()>();
        #[cfg(all(unix, not(epics_embedded_target)))]
        let sigterm = async {
            if let Ok(mut sig) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                let _ = sig.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        #[cfg(not(all(unix, not(epics_embedded_target))))]
        let sigterm = std::future::pending::<()>();

        let outcome = tokio::select! {
            biased;
            // Runner takes priority: if it completes naturally
            // before any signal arrives, propagate its result. This is the
            // one arm that is past C's `try` block, so it is the one arm
            // whose failure is not the catch block's.
            res = server.wait() => Some(res),
            _ = ctrl_c => {
                tracing::info!(target: "epics_base_rs::ioc_app", "SIGINT received, shutting down IOC");
                None
            }
            _ = sigterm => {
                tracing::info!(target: "epics_base_rs::ioc_app", "SIGTERM received, shutting down IOC");
                None
            }
        };
        match outcome {
            Some(res) => res,
            // The signal arms dropped `ProtocolServer::wait` without taking the
            // outcome, so the task is still this value's to stop. Awaiting the
            // abort — rather than letting `Drop` fire it — is what keeps the
            // process's exit callbacks, which `run_phased` runs the moment this
            // returns, from racing a server that is still on a socket.
            None => {
                server.shut_down().await;
                Ok(())
            }
        }
    }
}

/// Wire device support to all records that have DTYP set.
pub(crate) async fn wire_device_support(
    db: &PvDatabase,
    factories: &HashMap<String, DeviceSupportFactory>,
    dynamic_factory: &Option<DynamicDeviceSupportFactory>,
) -> CaResult<usize> {
    let names = db.all_record_names().await;
    let mut count = 0;
    for name in names {
        if let Some(rec_arc) = db.get_record(&name) {
            let mut instance = rec_arc.write();
            let dtyp = instance.common.dtyp.clone();
            if !crate::server::device_support::is_soft_dtyp(&dtyp) {
                let ctx = DeviceSupportContext {
                    dtyp: &dtyp,
                    inp: &instance.common.inp,
                    out: &instance.common.out,
                };
                let dev_opt = if let Some(factory) = factories.get(&dtyp) {
                    Some(factory())
                } else if let Some(dyn_factory) = dynamic_factory {
                    dyn_factory(&ctx)
                } else {
                    None
                };
                if let Some(dev) = dev_opt {
                    // Canonical device-support init order (M1/M2):
                    // set_record_info → apply_record_info → init,
                    // with init-failure logged and the record flagged
                    // INVALID. Single owner of the contract, shared
                    // with the IocBuilder build path.
                    crate::server::device_support::wire_device_to_record(&mut instance, dev);
                    count += 1;
                } else {
                    eprintln!(
                        "warning: no device support registered for DTYP '{dtyp}' (record: {name})"
                    );
                }
            }
        }
    }
    Ok(count)
}

/// Wire subroutine functions to sub records.
async fn wire_subroutines(db: &PvDatabase, registry: &HashMap<String, Arc<SubroutineFn>>) {
    if registry.is_empty() {
        return;
    }
    let names = db.all_record_names().await;
    for name in names {
        if let Some(rec_arc) = db.get_record(&name) {
            let mut instance = rec_arc.write();
            // Both `sub` and `aSub` resolve their subroutine from SNAM via the
            // function registry at init (C `subRecord.c` / `aSubRecord.c`
            // `init_record` -> `registryFunctionFind`).
            let rt = instance.record.record_type();
            if rt == "sub" || rt == "aSub" {
                // INAM: invoke the init routine exactly once at init, before
                // SNAM resolution (C `subRecord.c` / `aSubRecord.c`
                // `init_record`: `registryFunctionFind(inam)` then
                // `(*psubroutine)(prec)`, return value discarded; a missing
                // function is an init error -> stderr).
                if let Some(crate::types::EpicsValue::String(inam)) =
                    instance.record.get_field("INAM")
                {
                    let inam = inam.as_str_lossy();
                    if !inam.is_empty() {
                        match registry.get(inam.as_ref()) {
                            Some(init_fn) => {
                                let init_fn = init_fn.clone();
                                if let Err(e) = init_fn(&mut *instance.record) {
                                    eprintln!(
                                        "iocInit: {name}.INAM '{inam}' init routine failed: {e}"
                                    );
                                }
                            }
                            None => eprintln!("iocInit: {name}.INAM function '{inam}' not found"),
                        }
                    }
                }
                // C `init_record`'s `prec->sadr = registryFunctionFind(...)`,
                // assigned whatever the lookup returned. Unconditional so the
                // invariant "`subroutine` is the resolution of the current
                // SNAM" holds by construction rather than by this field
                // happening to start out `None`.
                if let Some(crate::types::EpicsValue::String(snam)) =
                    instance.record.get_field("SNAM")
                {
                    instance.subroutine = registry.get(snam.as_str_lossy().as_ref()).cloned();
                }
            }
        }
    }
}

/// C `scanAdd`'s `menuScanI_O_Intr` failure exit (`dbScan.c:272-293`): a record
/// whose device support cannot supply an interrupt source is reported with
/// `recGblRecordError` and **demoted to `menuScanPassive`** — it never joins the
/// I/O Intr scan list, and `caget REC.SCAN` reads back `Passive`.
///
/// The demotion is a SCAN transition like any other, so it goes through
/// `RecordInstance::set_scan` — the single owner, which drives the C
/// `scanDelete` → `get_ioint_info(1)` hook and hands back the delta for
/// `update_scan_index`, so the record also leaves the `IoIntr` scan bucket that
/// `scanpiol` and `dbla` report from.
async fn demote_io_intr_to_passive(db: &PvDatabase, name: &str, reason: &str) {
    let Some(rec_arc) = db.get_record(name) else {
        return;
    };
    let result = {
        let mut inst = rec_arc.write();
        if inst.common.scan != record::ScanType::IoIntr {
            return;
        }
        inst.set_scan(record::ScanType::Passive)
    };
    if let record::CommonFieldPutResult::ScanChanged {
        old_scan,
        new_scan,
        phas,
    } = result
    {
        db.update_scan_index(name, old_scan, new_scan, phas, phas);
    }
    eprintln!("scanAdd: I/O Intr not valid ({reason}), {name} set to Passive");
}

/// Set up I/O Intr scanning for records with SCAN="I/O Intr".
///
/// The single owner of that wiring — `IocBuilder` calls this too, so the C
/// `scanAdd` failure paths cannot be present on one startup route and absent on
/// the other.
pub(crate) async fn setup_io_intr(db: Arc<PvDatabase>) -> usize {
    let all_names = db.all_record_names().await;
    let io_intr_recs: Vec<(String, Arc<parking_lot::RwLock<record::RecordInstance>>)> = {
        let mut recs = Vec::new();
        for name in &all_names {
            if let Some(arc) = db.get_record(name) {
                recs.push((name.clone(), arc));
            }
        }
        recs
    };

    let mut count = 0;
    // Records that reached one of C `scanAdd`'s I/O Intr failure exits. The
    // demotion runs after the loop: it takes the registration mutex and the
    // records map (`update_scan_index`), which must not be entered while this
    // loop holds a record's write guard.
    let mut demote: Vec<(String, &'static str)> = Vec::new();
    for (name, rec_arc) in io_intr_recs {
        let mut inst = rec_arc.write();
        // Wire poll feedback when the record is on I/O Intr scan, OR when the
        // device drives processing from its own callback independently of the
        // SCAN menu (motorRecord statusCallback; asyn readback records, PRs
        // #60/#208). The device's decision is authoritative — the SCAN check
        // alone would suppress callbacks for a SCAN="Passive" motor, breaking
        // the pp(TRUE) dbPutField re-process gate.
        // NOTE: property-post wiring (setup_property_posts) is a separate
        // pass — an enum re-propagation callback is independent of SCAN.
        let independent = inst
            .device
            .as_ref()
            .is_some_and(|d| d.io_intr_scan_independent());
        let on_io_intr = inst.common.scan == record::ScanType::IoIntr;
        if !on_io_intr && !independent {
            continue;
        }
        let Some(mut dev) = inst.device.take() else {
            // C `dbScan.c:272-276` — `precord->dset == NULL`.
            if on_io_intr {
                demote.push((name, "no DSET"));
            }
            continue;
        };
        if let Some(mut intr_rx) = dev.io_intr_receiver() {
            let db_clone = db.clone();
            let rec_name = name.clone();
            let rec_arc_clone = rec_arc.clone();
            // C keeps one I/O Intr scan list per band and fires
            // `callbackRequest` on the band the record joined —
            // `callbackSetPriority(prio, &piosl->callback)` (`dbScan.c:597`),
            // with `scanAdd` filing the record under `precord->prio`. This
            // pump is per record, so the record's own band is the list it
            // would be on.
            let prio = inst.common.callback_priority();
            crate::runtime::task::spawn_background(prio, async move {
                while intr_rx.recv().await.is_some() {
                    // C `scanIoRequest` (`dbScan.c:616-618`): an I/O Intr
                    // callback queues nothing while the scan facility is
                    // not running, which is what `interruptAccept` buys a
                    // building or paused IOC.
                    if !crate::server::scan::scan_is_running() {
                        continue;
                    }
                    // Process if the device drives SCAN-independently,
                    // or the record is still on I/O Intr scan.
                    let process = independent || {
                        let inst = rec_arc_clone.read();
                        inst.common.scan == record::ScanType::IoIntr
                    };
                    if !process {
                        continue;
                    }
                    let mut visited = std::collections::HashSet::new();
                    // Driver-callback cycle: an output (`asyn:READBACK`)
                    // record reads the value back into VAL and skips the
                    // device write; input records are unaffected.
                    let _ = db_clone
                        .process_record_readback(&rec_name, &mut visited, 0)
                        .await;
                }
            });
            count += 1;
        } else if on_io_intr {
            // C `dbScan.c:278-293` — device support with no `get_ioint_info`,
            // or one whose `get_ioint_info` yields no scan list. The port
            // collapses all three into "the device offers no interrupt
            // receiver"; the observable is the same demotion.
            demote.push((name, "no interrupt source from device support"));
        }
        inst.device = Some(dev);
    }
    for (name, reason) in demote {
        demote_io_intr_to_passive(&db, &name, reason).await;
    }
    count
}

/// Spawn the out-of-band PROPERTY-post drains for every device exposing a
/// [`DeviceSupport::property_post_receiver`] (asyn enum-string runtime
/// re-propagation). C `registerInterruptUser(callbackEnum)` registers the
/// callback at init; the per-record callback re-applies the enum table and
/// `db_post_events(DBE_PROPERTY)` independently of `SCAN`. This mirrors
/// [`setup_io_intr`]: the device owns the source subscription (the asyn
/// interrupt) and yields a channel of [`PropertyPost`]s; the framework owns
/// the post (`post_property`). Returns the number of drains wired.
///
/// [`PropertyPost`]: crate::server::device_support::PropertyPost
pub(crate) async fn setup_property_posts(db: Arc<PvDatabase>) -> usize {
    let names = db.all_record_names().await;
    let mut count = 0;
    for name in names {
        if let Some(rec_arc) = db.get_record(&name) {
            let mut inst = rec_arc.write();
            if let Some(mut dev) = inst.device.take() {
                if let Some(mut rx) = dev.property_post_receiver() {
                    let db_clone = db.clone();
                    let rec_name = name.clone();
                    let prio = inst.common.callback_priority();
                    crate::runtime::task::spawn_background(prio, async move {
                        // Each message is the setEnums field block plus the
                        // one field C posts on; DBE_PROPERTY on that field is
                        // what makes clients re-read the choices.
                        while let Some(post) = rx.recv().await {
                            let _ = db_clone.post_property(&rec_name, post);
                        }
                    });
                    count += 1;
                }
                inst.device = Some(dev);
            }
        }
    }
    count
}

#[cfg(test)]
mod io_intr_scan_add_tests {
    use super::setup_io_intr;
    use crate::server::database::PvDatabase;
    use crate::server::record::ScanType;
    use crate::server::records::ai::AiRecord;
    use std::sync::Arc;

    /// R6-8 — C `scanAdd` (`dbScan.c:272-276`): a `SCAN="I/O Intr"` record with
    /// no device support (`precord->dset == NULL`) is reported with
    /// `recGblRecordError` and **demoted to `menuScanPassive`**. It must not be
    /// left claiming I/O Intr, and must not stay in the I/O Intr scan bucket
    /// that `scanpiol` reports from.
    #[epics_macros_rs::epics_test]
    async fn io_intr_without_device_support_is_demoted_to_passive() {
        let db = Arc::new(PvDatabase::new());
        db.add_record("NODEV", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        {
            let rec = db.get_record("NODEV").unwrap();
            let mut inst = rec.write();
            inst.common.scan = ScanType::IoIntr;
        }
        db.update_scan_index("NODEV", ScanType::Passive, ScanType::IoIntr, 0, 0);
        assert_eq!(
            db.records_for_scan(ScanType::IoIntr).await,
            vec!["NODEV".to_string()],
            "precondition: the record starts in the I/O Intr bucket"
        );

        let wired = setup_io_intr(db.clone()).await;
        assert_eq!(wired, 0, "no device support ⇒ nothing to wire");

        // C: `precord->scan = menuScanPassive` — `caget NODEV.SCAN` reads Passive.
        let rec = db.get_record("NODEV").unwrap();
        assert_eq!(
            rec.read().common.scan,
            ScanType::Passive,
            "an unusable I/O Intr record must be demoted to Passive"
        );
        assert!(
            db.records_for_scan(ScanType::IoIntr).await.is_empty(),
            "and must leave the I/O Intr scan list"
        );
    }

    /// R6-8 — C `scanAdd` (`dbScan.c:278-293`): device support that supplies no
    /// interrupt source (`get_ioint_info == NULL`, or it returns non-zero, or it
    /// yields a NULL scan list) is the same failure exit — log and demote. The
    /// port collapses those three C cases into "the device offers no
    /// `io_intr_receiver`", so this covers all of them.
    #[epics_macros_rs::epics_test]
    async fn io_intr_with_device_but_no_interrupt_source_is_demoted_to_passive() {
        use crate::error::CaResult;
        use crate::server::device_support::DeviceSupport;
        use crate::server::record::Record;

        /// Device support with the default `io_intr_receiver` (→ `None`).
        struct NoIntrDevice;
        impl DeviceSupport for NoIntrDevice {
            fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
                Ok(())
            }
            fn dtyp(&self) -> &str {
                "NoIntr"
            }
        }

        let db = Arc::new(PvDatabase::new());
        db.add_record("NOINTR", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        {
            let rec = db.get_record("NOINTR").unwrap();
            let mut inst = rec.write();
            inst.common.scan = ScanType::IoIntr;
            inst.device = Some(Box::new(NoIntrDevice));
        }
        db.update_scan_index("NOINTR", ScanType::Passive, ScanType::IoIntr, 0, 0);

        let wired = setup_io_intr(db.clone()).await;
        assert_eq!(wired, 0, "no interrupt source ⇒ nothing to wire");

        let rec = db.get_record("NOINTR").unwrap();
        {
            let inst = rec.read();
            assert_eq!(
                inst.common.scan,
                ScanType::Passive,
                "device support with no interrupt source must demote SCAN to Passive"
            );
            assert!(
                inst.device.is_some(),
                "the demotion must not drop the record's device support"
            );
        }
        assert!(db.records_for_scan(ScanType::IoIntr).await.is_empty());
    }

    /// The demotion is scoped to the failure exits: a record that was never on
    /// I/O Intr is untouched by the pass.
    #[epics_macros_rs::epics_test]
    async fn a_passive_record_is_not_touched_by_the_io_intr_pass() {
        let db = Arc::new(PvDatabase::new());
        db.add_record("PASV", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let wired = setup_io_intr(db.clone()).await;
        assert_eq!(wired, 0);
        let rec = db.get_record("PASV").unwrap();
        assert_eq!(rec.read().common.scan, ScanType::Passive);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use source_guard::{Comments, production};
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Serialises the initHooks tests — `HOOKS` is process-global, so
    /// two tests announcing at once would observe each other's
    /// callbacks. The state machine here is small; a mutex is enough.
    static INIT_HOOK_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    /// `iocInit.c:188-190`, byte for byte, terminator included.
    ///
    /// Measured on stderr from `softIoc` R7.0.10.1-DEV with an unreadable
    /// ACF, redirected to a FILE — so this is the stripped form, which is
    /// what `errlogStripANSI` leaves when the console is not a terminal:
    ///
    /// ```text
    /// ERROR iocBuild: asInit Failed.$
    ///  The IOC has not been started.$
    /// ```
    ///
    /// (`cat -A`, so `$` is the newline.) Both lines are terminated; the
    /// second's `\n` sits OUTSIDE `ANSI_MAGENTA(...)`, after the reset.
    #[test]
    fn the_as_init_failure_lines_are_c_s() {
        assert_eq!(
            as_init_failed_message(false),
            "ERROR iocBuild: asInit Failed.\n The IOC has not been started.\n"
        );
        assert_eq!(
            as_init_failed_message(true),
            format!(
                "{} iocBuild: asInit Failed.\n\u{1b}[35;1m The IOC has not been \
                 started.\u{1b}[0m\n",
                crate::runtime::log::ERL_ERROR
            )
        );
    }

    /// # Invariant
    ///
    /// MUST: every thread this module creates take its band **and** its OS
    /// name through `enter_ioc_thread`. MUST NOT: an iocsh thread run at the
    /// priority it inherited from `POSIX_Init`.
    ///
    /// All three threads here run iocsh command bodies — the startup script,
    /// the `afterIocRunning` queue (epics-base PR #558), and the `iocsh(NULL)`
    /// prompt of an IOC whose `iocInit()` never ran (C `softMain.cpp:250`,
    /// [`run_uninitialized_tail`]). In C that is one thread, the shell, and
    /// base-on-RTEMS bands it explicitly:
    /// `epicsThreadSetPriority(epicsThreadGetIdSelf(), epicsThreadPriorityIocsh)`
    /// (`libcom/RTEMS/posix/rtems_init.c:1002`), under the comment *"Override
    /// RTEMS Posix configuration, it gets started with posix prio 2"*. That is
    /// the same inheritance defect the port has: RTEMS pthreads inherit their
    /// creator's parameters (`cpukit/posix/src/pthreadattrdefault.c:49-58` (both `rtems` pins))
    /// and the boot shim runs `POSIX_Init` at `RTEMS_MAXIMUM_PRIORITY - 1`, so
    /// a thread that skips the prologue runs one level above idle.
    ///
    /// `Iocsh` = 91 is the top of the EPICS range — above `High`(90) and every
    /// scan and callback band. That is C's choice and it is the right one for
    /// both callers: `run` **awaits** each of these threads, so with an
    /// inherited near-idle band the whole of iocInit sits behind every scan
    /// thread and callback worker already running. The startup script is
    /// bounded (it runs once and exits); the post-init queue is as bounded as
    /// the command an operator would have typed at the C console, which C runs
    /// at this same 91.
    ///
    /// Source inspection, because the defect is a call that is *absent*.
    ///
    /// The prologue itself moved into `runtime::task::MandatoryThread`, which
    /// takes the band as a constructor argument and runs `enter_ioc_thread`
    /// before the body — so what this module can still get wrong is *which*
    /// band it declares, and whether it declares one at all. Both are checked
    /// below; `thread_census.rs` is what forbids creating a thread here by any
    /// other route.
    #[test]
    fn iocsh_threads_take_the_iocsh_band() {
        let prod = production(include_str!("ioc_app.rs"), Comments::Strip);

        assert_eq!(
            prod.matches("MandatoryThread::new(").count(),
            3,
            "the startup-script, afterIocRunning and uninitialised-tail threads"
        );
        assert_eq!(
            prod.matches("name_current_thread(").count(),
            0,
            "naming without banding leaves the thread one level above idle on \
             the target; the `MandatoryThread` prologue is the whole of it"
        );
        assert_eq!(
            prod.matches("apply_to_current_thread(").count(),
            0,
            "banding without naming leaves an RTEMS-anonymous thread"
        );
        for name in ["iocsh-startup", "iocsh-after-ioc-running", "iocsh"] {
            let at = prod
                .find(&format!("\"{name}\","))
                .unwrap_or_else(|| panic!("the {name} thread moved; update this guard"));
            let head = &prod[at..(at + 700).min(prod.len())];
            assert!(
                head.contains("ThreadPriority::Iocsh"),
                "{name} must be declared at `ThreadPriority::Iocsh` \
                 (posix/rtems_init.c:1002)"
            );
        }
    }

    /// `epicsThread.h:86` — the band the guard above pins is C's constant.
    #[test]
    fn the_iocsh_band_is_epics_thread_priority_iocsh() {
        assert_eq!(crate::runtime::task::ThreadPriority::Iocsh.value(), 91);
    }

    /// The `dbLoadGroup` startup command queues `(filename, macros)`
    /// pairs (NOT bound to any provider), with pvxs removal
    /// semantics applied to the queue (`-file` by identity, `-*` clears),
    /// and a re-load of the same identity nets to a single entry. The QSRV
    /// runner later drains [`take_group_load_requests`] to build the
    /// served provider.
    #[test]
    fn dbloadgroup_startup_command_queues_and_removes() {
        // Process-global queue: drain any leftover so this test is isolated.
        let _ = take_group_load_requests();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(PvDatabase::new());
        let bridge = {
            let _guard = rt.enter();
            crate::runtime::task::BlockingBridge::capture()
        };
        let shell = iocsh::IocShell::new(db, bridge);
        shell.register(db_load_group_startup_command());

        // Two distinct group files (the command probes existence early).
        let tmpdir = tempfile::tempdir().expect("fixture root");
        let a = tmpdir.path().join("qsrv_q_a.json");
        let b = tmpdir.path().join("qsrv_q_b.json");
        std::fs::write(&a, "{}").unwrap();
        std::fs::write(&b, "{}").unwrap();

        shell
            .execute_line(&format!("dbLoadGroup(\"{}\")", a.display()))
            .unwrap();
        shell
            .execute_line(&format!("dbLoadGroup(\"{}\",\"M=1\")", b.display()))
            .unwrap();
        // Re-load of the same identity nets to one entry (pvxs erases first).
        shell
            .execute_line(&format!("dbLoadGroup(\"{}\")", a.display()))
            .unwrap();

        // Missing file → early error at the st.cmd line (pvxs parity).
        assert!(
            shell
                .execute_line("dbLoadGroup(\"/no/such/group.json\")")
                .is_err(),
            "a missing group file must error at command time"
        );

        // `-file` removes only the matching identity (a, macros="").
        shell
            .execute_line(&format!("dbLoadGroup(\"-{}\")", a.display()))
            .unwrap();

        let reqs = take_group_load_requests();
        assert_eq!(reqs.len(), 1, "only the (b, M=1) entry must remain");
        assert_eq!(reqs[0].filename, b.to_string_lossy());
        assert_eq!(reqs[0].macros, "M=1");

        // `-*` clears the whole queue.
        shell
            .execute_line(&format!("dbLoadGroup(\"{}\")", b.display()))
            .unwrap();
        shell.execute_line("dbLoadGroup(\"-*\")").unwrap();
        assert!(
            take_group_load_requests().is_empty(),
            "dbLoadGroup(\"-*\") must clear the queue"
        );

        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    /// H1 regression: a callback registered via `init_hook_register`
    /// fires for every announced state, and `init_hook_announce`
    /// delivers states in the order they were announced.
    #[test]
    fn init_hook_register_and_announce_in_order() {
        let _guard = INIT_HOOK_TEST_LOCK.lock().unwrap();
        init_hooks::init_hook_free();

        let seen: Arc<StdMutex<Vec<InitHookState>>> = Arc::new(StdMutex::new(Vec::new()));
        let seen_cb = seen.clone();
        init_hook_register(Arc::new(move |state| {
            seen_cb.lock().unwrap().push(state);
        }));

        // Announce a subset in C order.
        let order = [
            InitHookState::AtIocBuild,
            InitHookState::AfterInitDevSup,
            InitHookState::AfterInitDatabase,
            InitHookState::AfterInitialProcess,
            InitHookState::AfterIocRunning,
        ];
        for &s in &order {
            init_hook_announce(s);
        }

        let got = seen.lock().unwrap().clone();
        assert_eq!(got, order, "hooks must fire in announce order");

        init_hooks::init_hook_free();
    }

    /// H1 regression: a hook that registers ANOTHER hook from inside
    /// its callback must not deadlock, and the newly-registered hook
    /// is not invoked for the in-progress state (C snapshot
    /// semantics).
    #[test]
    fn init_hook_reentrant_register_does_not_deadlock() {
        let _guard = INIT_HOOK_TEST_LOCK.lock().unwrap();
        init_hooks::init_hook_free();

        let inner_calls = Arc::new(AtomicUsize::new(0));
        let inner_for_outer = inner_calls.clone();
        init_hook_register(Arc::new(move |_state| {
            // Register a second hook from inside the callback.
            let inner = inner_for_outer.clone();
            init_hook_register(Arc::new(move |_s| {
                inner.fetch_add(1, Ordering::SeqCst);
            }));
        }));

        // First announce: outer hook runs, registers inner. Inner is
        // NOT called for this state.
        init_hook_announce(InitHookState::AtIocBuild);
        assert_eq!(inner_calls.load(Ordering::SeqCst), 0);

        // Second announce: both outer and the inner(s) run.
        init_hook_announce(InitHookState::AfterIocRunning);
        assert!(inner_calls.load(Ordering::SeqCst) >= 1);

        init_hooks::init_hook_free();
    }

    /// H1: state name strings match C `initHookName()`.
    #[test]
    fn init_hook_state_names_match_c() {
        assert_eq!(InitHookState::AtIocBuild.name(), "initHookAtIocBuild");
        assert_eq!(
            InitHookState::AfterInitDevSup.name(),
            "initHookAfterInitDevSup"
        );
        assert_eq!(
            InitHookState::AfterInitDatabase.name(),
            "initHookAfterInitDatabase"
        );
        assert_eq!(
            InitHookState::AfterIocRunning.name(),
            "initHookAfterIocRunning"
        );
    }

    #[epics_macros_rs::epics_test]
    async fn test_ioc_application_empty() {
        // An empty IocApplication with no script or records should start and stop cleanly
        // We can't easily test run() because it blocks on REPL, so test the wiring functions
        let db = Arc::new(PvDatabase::new());
        let factories = HashMap::new();
        let count = wire_device_support(&db, &factories, &None).await.unwrap();
        assert_eq!(count, 0);
    }

    #[epics_macros_rs::epics_test]
    async fn test_wire_device_support_no_dtyp() {
        use crate::server::records::ai::AiRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("TEST", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();

        let factories = HashMap::new();
        let count = wire_device_support(&db, &factories, &None).await.unwrap();
        assert_eq!(count, 0); // No DTYP set, so no wiring
    }

    /// Regression: `wire_device_support` (the IocApplication
    /// startup-script device-support attach path) MUST forward
    /// info(...) tags to the driver via `apply_record_info`. An earlier fix
    /// only patched the IocBuilder path; without this fix, IOCs
    /// loaded entirely through iocsh `dbLoadRecords` lose every
    /// `info()` tag the driver depends on (e.g. asyn `asyn:READBACK`).
    #[epics_macros_rs::epics_test]
    async fn wire_device_support_forwards_info_tags_to_driver() {
        use crate::server::device_support::{DeviceReadOutcome, DeviceSupport};
        use crate::server::record::ScanType;
        use crate::server::records::ai::AiRecord;
        use std::sync::{Arc as StdArc, Mutex as StdMutex};

        // Recording device support — captures the info map it received
        // via apply_record_info so the test can assert on its contents.
        struct RecordingDev {
            seen: StdArc<StdMutex<HashMap<String, String>>>,
        }
        impl DeviceSupport for RecordingDev {
            fn write(&mut self, _record: &mut dyn crate::server::record::Record) -> CaResult<()> {
                Ok(())
            }
            fn dtyp(&self) -> &str {
                "TestRecording"
            }
            fn read(
                &mut self,
                _record: &mut dyn crate::server::record::Record,
            ) -> CaResult<DeviceReadOutcome> {
                Ok(DeviceReadOutcome::ok())
            }
            fn apply_record_info(&mut self, info: &HashMap<String, String>) {
                let mut g = self.seen.lock().unwrap();
                *g = info.clone();
            }
            fn set_record_info(&mut self, _name: &str, _scan: ScanType) {}
        }

        let seen = StdArc::new(StdMutex::new(HashMap::<String, String>::new()));
        let seen_factory = seen.clone();
        let mut factories: HashMap<String, DeviceSupportFactory> = HashMap::new();
        factories.insert(
            "TestRecording".to_string(),
            Box::new(move || {
                Box::new(RecordingDev {
                    seen: seen_factory.clone(),
                })
            }),
        );

        let db = Arc::new(PvDatabase::new());
        db.add_record("AI:WITH:INFO", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        // Populate the record's info map — exactly what
        // IocBuilder/iocsh now do after loading info(...) directives.
        let rec = db.get_record("AI:WITH:INFO").unwrap();
        {
            let mut inst = rec.write();
            inst.common.dtyp = "TestRecording".to_string();
            inst.set_info("asyn:READBACK", "1");
            inst.set_info("Q:group", "demo");
        }

        let count = wire_device_support(&db, &factories, &None).await.unwrap();
        assert_eq!(count, 1, "device support must have attached");

        // The recording driver should have observed both tags via
        // apply_record_info — proves the hook fires from the
        // IocApplication batch-wiring path too.
        let observed = seen.lock().unwrap().clone();
        assert_eq!(observed.get("asyn:READBACK").map(String::as_str), Some("1"));
        assert_eq!(observed.get("Q:group").map(String::as_str), Some("demo"));
    }

    /// Regression: `wire_device_support` must bind records in **database load
    /// order**, the order C's `initDevSup` walks
    /// (`dbFirstRecord`/`dbNextRecord`). Real device support depends on it:
    /// epics-modules/opcua's element records refuse to init unless their
    /// `opcuaItem` record bound first (`linkParser.cpp:226-234`), and the
    /// shipped databases guarantee that only by declaring the item record
    /// first. This walked `all_record_names()` when that returned `HashMap`
    /// keys, so binding ran in hash order — not load order, and not even
    /// stable across runs of the same binary.
    #[epics_macros_rs::epics_test]
    async fn wire_device_support_binds_in_database_load_order() {
        use crate::server::device_support::{DeviceReadOutcome, DeviceSupport};
        use crate::server::records::ai::AiRecord;
        use std::sync::{Arc as StdArc, Mutex as StdMutex};

        struct NoopDev;
        impl DeviceSupport for NoopDev {
            fn write(&mut self, _record: &mut dyn crate::server::record::Record) -> CaResult<()> {
                Ok(())
            }
            fn dtyp(&self) -> &str {
                "SeqDev"
            }
            fn read(
                &mut self,
                _record: &mut dyn crate::server::record::Record,
            ) -> CaResult<DeviceReadOutcome> {
                Ok(DeviceReadOutcome::ok())
            }
        }

        // A fixed permutation of 0..24 — load order is deliberately neither
        // lexical nor hash order, so a pass cannot be a coincidence.
        let names: Vec<String> = (0..24)
            .map(|i: usize| format!("LOAD:{:02}", (i * 7 + 3) % 24))
            .collect();

        let db = Arc::new(PvDatabase::new());
        for name in &names {
            db.add_record(name, Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
            let rec = db.get_record(name).unwrap();
            let mut inst = rec.write();
            inst.common.dtyp = "SeqDev".to_string();
            // `DeviceSupportContext` carries the links, not the record name;
            // echoing the name through INP is how the test observes which
            // record is being wired.
            inst.common.inp = format!("@{name}");
        }

        let wired: StdArc<StdMutex<Vec<String>>> = StdArc::new(StdMutex::new(Vec::new()));
        let captured = wired.clone();
        let dynamic: Option<DynamicDeviceSupportFactory> =
            Some(Box::new(move |ctx: &DeviceSupportContext| {
                captured
                    .lock()
                    .unwrap()
                    .push(ctx.inp.trim_start_matches('@').to_string());
                Some(Box::new(NoopDev) as Box<dyn DeviceSupport>)
            }));

        let factories: HashMap<String, DeviceSupportFactory> = HashMap::new();
        let count = wire_device_support(&db, &factories, &dynamic)
            .await
            .unwrap();
        assert_eq!(count, names.len());

        let wired = std::mem::take(&mut *wired.lock().unwrap());
        assert_eq!(
            wired, names,
            "device support must bind in database load order (C initDevSup), \
             not HashMap hash order"
        );
    }

    /// Regression: an `asyn:READBACK` OUTPUT record processed because of a
    /// driver interrupt callback must READ the callback value back into VAL
    /// and MUST NOT write it to the driver. Writing it re-asserts the
    /// setpoint and re-triggers the driver — the AD `Acquire` loop where a
    /// single `Acquire 1` produced ~6 acquisitions (`ArrayCounter` ≈ 6,
    /// `Acquire` stuck at 1). C `devAsynInt32.c::processBo` takes the
    /// `newOutputCallbackValue` readback branch and never calls
    /// `processCallbackOutput`'s `write()` on a callback cycle; a
    /// put/FLNK/scan cycle still writes the setpoint.
    #[epics_macros_rs::epics_test]
    async fn readback_output_cycle_reads_back_and_skips_device_write() {
        use crate::server::device_support::{DeviceReadOutcome, DeviceSupport};
        use crate::server::record::ScanType;
        use crate::server::records::bo::BoRecord;
        use crate::types::EpicsValue;
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Mock asyn-style readback device: read() pushes the driver's
        // callback value into VAL; write() counts how often the framework
        // asked it to push VAL back out to the driver.
        struct ReadbackDev {
            writes: StdArc<AtomicUsize>,
            readback_val: u16,
        }
        impl DeviceSupport for ReadbackDev {
            fn dtyp(&self) -> &str {
                "TestReadback"
            }
            // asyn:READBACK records follow driver-side changes regardless of
            // SCAN (PRs #60/#208) — the trait flag the I/O Intr wiring keys on.
            fn io_intr_scan_independent(&self) -> bool {
                true
            }
            // ...and take the C `newOutputCallbackValue` readback branch on
            // callback cycles (never re-write the setpoint). Devices that do
            // not declare this — e.g. devMotorAsyn — still write on callback
            // passes; that default-false path has its own regression tests.
            fn output_callback_readback(&self) -> bool {
                true
            }
            fn read(
                &mut self,
                record: &mut dyn crate::server::record::Record,
            ) -> CaResult<DeviceReadOutcome> {
                record.set_val(EpicsValue::Enum(self.readback_val))?;
                Ok(DeviceReadOutcome::computed(
                    crate::server::device_support::DeviceUdf::Defined,
                ))
            }
            fn write(&mut self, _record: &mut dyn crate::server::record::Record) -> CaResult<()> {
                self.writes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn set_record_info(&mut self, _name: &str, _scan: ScanType) {}
        }

        let writes = StdArc::new(AtomicUsize::new(0));
        let db = Arc::new(PvDatabase::new());
        // bo VAL starts at 1 — the setpoint (e.g. Acquire=1).
        db.add_record("BO:RBK", Box::new(BoRecord::new(1)))
            .await
            .unwrap();
        {
            let rec = db.get_record("BO:RBK").unwrap();
            let mut inst = rec.write();
            // Non-soft DTYP so the read stage is eligible to run.
            inst.common.dtyp = "TestReadback".to_string();
            inst.device = Some(Box::new(ReadbackDev {
                writes: writes.clone(),
                readback_val: 0,
            }));
        }

        // Driver-callback cycle: the driver reported Acquire=0 (acquisition
        // done). The record must read 0 back into VAL and must NOT write.
        {
            let mut visited = std::collections::HashSet::new();
            db.process_record_readback("BO:RBK", &mut visited, 0)
                .await
                .unwrap();
        }
        {
            let rec = db.get_record("BO:RBK").unwrap();
            let inst = rec.read();
            assert_eq!(
                inst.record.get_field("VAL"),
                Some(EpicsValue::Enum(0)),
                "readback cycle must pull the driver callback value (0) into VAL"
            );
        }
        assert_eq!(
            writes.load(Ordering::SeqCst),
            0,
            "readback cycle must NOT write VAL back to the driver (no re-trigger)"
        );

        // Put/scan cycle: a normal process still writes the setpoint to the
        // driver exactly once (device_callback == false).
        {
            let rec = db.get_record("BO:RBK").unwrap();
            let mut inst = rec.write();
            inst.record.put_field("VAL", EpicsValue::Enum(1)).unwrap();
        }
        {
            let mut visited = std::collections::HashSet::new();
            db.process_record_with_links("BO:RBK", &mut visited, 0)
                .await
                .unwrap();
        }
        assert_eq!(
            writes.load(Ordering::SeqCst),
            1,
            "a put/scan cycle must write the setpoint to the driver exactly once"
        );
    }
}

#[cfg(test)]
mod lifecycle_tests {
    //! The `iocState` boundaries, one case per legal and illegal edge —
    //! C `iocInit.c`'s guards (`iocRun` :247, `iocPause` :279,
    //! `iocShutdown` :723), not one case per narrative.
    //!
    //! Every case drives the process-global lifecycle, which is what C's
    //! file-static `iocState` is; nextest gives each test its own process,
    //! so they do not share it.

    use super::*;
    use crate::server::scan::{ScanCtl, scan_ctl};

    /// BOUNDARY: `iocVoid`. C refuses both transitions from it — `iocRun`
    /// because the state is neither `iocBuilt` nor `iocPaused`
    /// (`iocInit.c:247-250`), `iocPause` because it is not `iocRunning`
    /// (`:279-282`) — and leaves the state alone.
    #[test]
    fn a_void_ioc_refuses_run_and_pause() {
        assert_eq!(get_ioc_state(), IocState::Void);
        assert_eq!(ioc_run(), -1, "iocRun from iocVoid is C's -1");
        assert_eq!(ioc_pause(), -1, "iocPause from iocVoid is C's -1");
        assert_eq!(get_ioc_state(), IocState::Void, "a refusal changes nothing");
    }

    /// BOUNDARY: `iocShutdown` from `iocVoid` returns 0 without announcing
    /// anything (`iocInit.c:723`). That early return is what makes it safe
    /// on every exit path, including one that never built an IOC.
    #[test]
    fn shutting_down_a_void_ioc_is_a_success() {
        assert_eq!(ioc_shutdown(), 0);
        assert_eq!(get_ioc_state(), IocState::Void);
    }

    /// The full walk, and the scan gate at each stop. `note_scan_owner_started`
    /// stands in for the bring-up path here so the test needs no threads:
    /// what it drives is the same `ioc_run` the shell's command calls.
    #[test]
    fn the_lifecycle_walks_void_running_paused_running_void() {
        note_scan_owner_started();
        assert_eq!(get_ioc_state(), IocState::Running);
        assert_eq!(scan_ctl(), ScanCtl::Run);

        assert_eq!(ioc_pause(), 0);
        assert_eq!(get_ioc_state(), IocState::Paused);
        assert_eq!(
            scan_ctl(),
            ScanCtl::Pause,
            "iocPause must close the gate every asynchronous scan source reads"
        );

        assert_eq!(ioc_run(), 0);
        assert_eq!(get_ioc_state(), IocState::Running);
        assert_eq!(scan_ctl(), ScanCtl::Run);

        assert_eq!(ioc_shutdown(), 0);
        assert_eq!(get_ioc_state(), IocState::Void);
        assert_eq!(scan_ctl(), ScanCtl::Exit);
    }

    /// BOUNDARY: the illegal repeat of each transition. C answers a second
    /// `iocPause` with "IOC not running" and a second `iocRun` with "IOC
    /// not paused", both -1, and neither moves the state.
    #[test]
    fn a_repeated_transition_is_refused_from_its_own_end_state() {
        note_scan_owner_started();
        assert_eq!(ioc_run(), -1, "already running");
        assert_eq!(get_ioc_state(), IocState::Running);

        assert_eq!(ioc_pause(), 0);
        assert_eq!(ioc_pause(), -1, "already paused");
        assert_eq!(get_ioc_state(), IocState::Paused);
    }

    /// BOUNDARY: `iocShutdown` from `iocPaused`. C shuts down from any
    /// non-void state, not only from running.
    #[test]
    fn a_paused_ioc_can_be_shut_down() {
        note_scan_owner_started();
        assert_eq!(ioc_pause(), 0);
        assert_eq!(ioc_shutdown(), 0);
        assert_eq!(get_ioc_state(), IocState::Void);
    }

    /// The gate is what `iocPause` buys: C `postEvent` queues nothing while
    /// the facility is not running (`dbScan.c:536-539`), so a `SCAN="Event"`
    /// record does not process on a paused IOC and does again once it runs.
    #[epics_macros_rs::epics_test]
    async fn a_paused_ioc_does_not_process_its_event_records() {
        use crate::error::CaResult;
        use crate::server::record::{FieldDesc, ProcessOutcome, Record, ScanType};
        use crate::types::EpicsValue;
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Counts its own `process()` calls.
        struct CountProbe(Arc<AtomicUsize>);

        impl Record for CountProbe {
            fn record_type(&self) -> &'static str {
                "ioc_pause_probe"
            }
            fn process(&mut self) -> CaResult<ProcessOutcome> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(ProcessOutcome::complete())
            }
            fn get_field(&self, _name: &str) -> Option<EpicsValue> {
                None
            }
            fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
                Ok(())
            }
            fn declared_fields(&self) -> &'static [FieldDesc] {
                &[]
            }
        }

        let runs = Arc::new(AtomicUsize::new(0));
        let db = Arc::new(PvDatabase::new());
        db.add_record("EV", Box::new(CountProbe(Arc::clone(&runs))))
            .await
            .unwrap();
        {
            let rec = db.get_record("EV").unwrap();
            rec.write().common.scan = ScanType::Event;
        }
        db.update_scan_index("EV", ScanType::Passive, ScanType::Event, 0, 0);

        note_scan_owner_started();
        db.post_event().await;
        let while_running = runs.load(Ordering::SeqCst);
        assert_eq!(while_running, 1, "a running IOC processes its Event list");

        assert_eq!(ioc_pause(), 0);
        db.post_event().await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            while_running,
            "a posted event must not process anything while the IOC is paused"
        );

        assert_eq!(ioc_run(), 0);
        db.post_event().await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            while_running + 1,
            "iocRun must reopen the gate"
        );
    }
}
