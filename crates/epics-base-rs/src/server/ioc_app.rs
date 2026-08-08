// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that hand-builds its own tokio runtime; runs and passes in the feature-ON suite.
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
    /// (`initHooks.h`). Only the states an embedded-style Rust IOC
    /// can actually reach are modelled; the C `iocPause` /
    /// `iocShutdown` / unit-test states are omitted because neither
    /// Rust build path has a pause/shutdown transition that announces
    /// them. The order of the modelled variants matches C exactly.
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
/// erases the matching entry before re-appending, :160-184). The file is
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
                optional: false,
            },
            ArgDesc {
                name: "macros",
                arg_type: ArgType::String,
                optional: true,
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
/// and returns any iocsh commands to expose in the interactive shell.
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
    startup_commands: Vec<CommandDef>,
    shell_commands: Vec<CommandDef>,
    startup_script: Option<String>,
    /// Records added via the declarative builder (Phase 7).
    inline_records: Vec<(String, Box<dyn Record>)>,
    /// Callbacks invoked after iocInit completes (e.g., start pollers).
    after_init_hooks: Vec<Box<dyn FnOnce() + Send>>,
    /// Async external link-set installers (CA links via `epics-ca-rs`'s
    /// `calink`, PVA links via the bridge's `pvalink`). Fired at the
    /// `AfterCaLinkInit` hook in [`Self::run`] — before `setup_cp_links`
    /// — so a Passive holder of an external CP link warms at iocInit.
    link_set_installers: Vec<LinkSetInstaller>,
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
            // SERVER-side port: caservertask.c:491-498 honours
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
            startup_commands: Vec::new(),
            shell_commands: Vec::new(),
            startup_script: None,
            inline_records: Vec::new(),
            after_init_hooks: Vec::new(),
            link_set_installers: Vec::new(),
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

    /// Register a command available during startup script execution (Phase 1).
    /// Use this for driver configuration commands like `simDetectorConfig`.
    pub fn register_startup_command(mut self, cmd: CommandDef) -> Self {
        self.startup_commands.push(cmd);
        self
    }

    /// Register a command available in the interactive shell (Phase 3).
    /// Use this for runtime commands like `simDetectorReport`.
    pub fn register_shell_command(mut self, cmd: CommandDef) -> Self {
        self.shell_commands.push(cmd);
        self
    }

    /// The commands the startup script (`st.cmd`) may call, in registration
    /// order.
    ///
    /// This is the surface a startup script is executed against — a command
    /// missing here is a fatal unknown command in `st.cmd`, before `iocInit`.
    /// Exposed so a pre-configured IOC (e.g. `AdIoc`) can be checked against
    /// the script it promises to run without booting a server. `CommandDef` is
    /// `Clone`, so a caller may also install these on an [`iocsh::IocShell`] of
    /// its own to exercise a script.
    pub fn startup_commands(&self) -> &[CommandDef] {
        &self.startup_commands
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
    /// live database and returns any iocsh commands to add to the
    /// interactive shell (merged into the protocol runner's command set).
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

    /// Set the startup script path (executed before iocInit).
    pub fn startup_script(mut self, path: &str) -> Self {
        self.startup_script = Some(path.to_string());
        self
    }

    /// Register a record type factory (e.g., "motor", "asyn").
    /// Avoids the global registry — factories are passed to IocBuilder.
    pub fn register_record_type<F>(mut self, type_name: &str, factory: F) -> Self
    where
        F: Fn() -> Box<dyn Record> + Send + Sync + 'static,
    {
        self.record_factories
            .insert(type_name.to_string(), Box::new(factory));
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

    /// Run the full IOC lifecycle: startup script -> iocInit -> protocol runner.
    ///
    /// The `protocol_runner` closure receives an [`IocRunConfig`] containing the
    /// fully initialized database, port, and configuration. It is responsible for
    /// starting the protocol-specific server (e.g., CA, PVA) and the interactive shell.
    pub async fn run<F, Fut>(self, protocol_runner: F) -> CaResult<()>
    where
        F: FnOnce(IocRunConfig) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = CaResult<()>> + Send,
    {
        let db = Arc::new(PvDatabase::new());
        // Everything from here to the `db.ioc_init()` barrier below is C's
        // pre-`iocInit` load: inline records, then the `st.cmd`'s
        // `dbLoadRecords` calls. Records created in it queue their link-status
        // classification instead of running it against a database that is still
        // being built (R18-92).
        db.begin_load()
            .expect("a database created a line ago has not run iocInit");

        // On an embedded target (RTEMS or VxWorks), bring up the background
        // executor (callback pool, delayed timer, scanOnce worker) before any
        // record processing can defer a tail — C parity: `callbackInit` runs
        // early in `iocInit` (callback.c:286). Hosted builds drive tails on
        // the tokio runtime and skip this; a spawn/sleep/interval on a path
        // that never reaches here still lazy-inits the same executor on
        // first use.
        #[cfg(epics_embedded_target)]
        crate::runtime::task::background_init();

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
            mut startup_commands,
            mut shell_commands,
            startup_script,
            inline_records,
            after_init_hooks,
            link_set_installers,
        } = self;

        // The IOC's single live policy cell, created BEFORE the startup
        // script runs so the script's `asInit` and the servers built
        // afterwards observe the same store (upstream issue #667
        // adjacent: a config that only lands in a shell-local copy is
        // access security silently OFF).
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
            startup_commands.extend(cmds);
        }

        // Register the QSRV `dbLoadGroup` startup command so a
        // pvxs-compatible st.cmd can queue group definition files before
        // iocInit (pvxs registers it from its registrar before the
        // startup script). The QSRV protocol runner drains the queue and
        // applies it to the served provider; a non-QSRV runner never
        // drains it, leaving the command a harmless no-op.
        startup_commands.push(db_load_group_startup_command());

        // Add inline records (Phase 7 declarative builder)
        for (name, record) in inline_records {
            db.add_record(&name, record).await?;
        }

        // Phase 1: Execute startup script in a separate std::thread.
        // std::thread (not spawn_blocking) is required because iocsh commands
        // use Handle::block_on() which panics inside the tokio runtime context.
        if let Some(script) = startup_script {
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
                for cmd in startup_commands {
                    shell.register(cmd);
                }
                let result = shell.execute_script(&script);
                let _ = tx.send(result);
            })
            .map_err(|e| {
                CaError::InvalidValue(format!("could not start the iocsh-startup thread: {e}"))
            })?;

            let result = rx
                .await
                .map_err(|_| CaError::InvalidValue("startup thread dropped".into()))?;
            result.map_err(|e| CaError::InvalidValue(e))?;
        }

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

        // iocBuild begins.
        announce!(InitHookState::AtIocBuild);
        announce!(InitHookState::AtBeginning);
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
        // merged into the shell command set the protocol runner registers.
        for installer in link_set_installers {
            shell_commands.extend(installer(db.clone()).await);
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
        // (`dbLink.c:117-129` falling through `dbDbInitLink`'s
        // `S_db_notFound`, `dbDbLink.c:94-96`). Runs BEFORE `setup_cp_links`
        // for C's reason — `initPVLinks` initialises links before anything
        // consumes them — and it is a one-shot: C guards re-entry with
        // `DBLINK_FLAG_INITIALIZED` (`dbLink.c:95-101`), so a record added
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
                .wait_for_external_links(std::time::Duration::from_secs_f64(link_wait_secs))
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
            match builder.build().await {
                Ok(mgr) => {
                    eprintln!("autosave: {} save set(s) configured", mgr.set_names().len());
                    Some(Arc::new(mgr))
                }
                Err(e) => {
                    eprintln!("autosave: failed to build manager: {e}");
                    None
                }
            }
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
        // iocRun begins. Scan tasks / CA links are started by the
        // protocol runner immediately after handoff.
        announce!(InitHookState::AtIocRun);
        // C `piniProcessHook` (iocInit.c:629-646): the hook registered by
        // `initialProcess()` runs `piniProcess(menuPiniRUN)` when
        // `initHookAtIocRun` is announced. PINI=RUN records are processed
        // here and NOT in the PINI=YES pass above.
        db.pini_process(crate::server::record::PiniMode::Run).await;
        // C `scanRun` (iocInit.c, iocRun: after the PINI=RUN hook, before
        // `initHookAfterDatabaseRunning`): periodic scanning is owned by
        // the IOC core, not by any protocol server — a PVA-only or
        // server-less IOC scans all the same. The owner's PINI=YES pass
        // is skipped (Phase 2b.6 already ran it and published
        // completion); the `try_claim_scan_start` claim it takes keeps a
        // protocol runner or embedded harness that starts another owner
        // parked and harmless. Held across the runner handoff: when
        // `run` returns (or its future is dropped), the drop stops every
        // scan-%g thread.
        let _scan_owner = crate::server::scan::ScanOwner::start(db.clone());
        announce!(InitHookState::AfterDatabaseRunning);
        announce!(InitHookState::AfterCaServerRunning);

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
        announce!(InitHookState::AfterIocRunning);
        // C `piniProcessHook` at `initHookAfterIocRunning` (iocInit.c:637-639)
        // — `piniProcess(menuPiniRUNNING)`.
        db.pini_process(crate::server::record::PiniMode::Running)
            .await;

        // Phase 2e: drain `afterIocRunning` queue (epics-base PR #558).
        // Each line is an iocsh command queued by the startup script;
        // execute through a fresh shell so post-init state (including
        // PINI side effects) is visible. Both built-in iocsh commands
        // AND every user-registered `shell_commands` entry are
        // re-registered on this shell (CommandDef now `Clone`-able)
        // so site-specific names like `motorReport` are addressable
        // from the post-init queue.
        let pending = db.take_after_ioc_running();
        if !pending.is_empty() {
            let db1 = db.clone();
            let b1 = bridge.clone();
            let acf1 = acf.clone();
            let shell_cmds_clone = shell_commands.clone();
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
                for cmd in shell_cmds_clone {
                    shell.register(cmd);
                }
                let mut errs: Vec<String> = Vec::new();
                for line in pending {
                    if let Err(e) = shell.execute_line(&line) {
                        errs.push(format!("{line}: {e}"));
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

        // Phase 3: Hand off to protocol runner.
        // C parity (caservertask.c:491-499): the server-side env var
        // EPICS_CAS_SERVER_PORT sets `ca_server_port`, and
        // `ca_udp_port = ca_server_port` — so UDP and TCP bind the
        // same value unless the Rust-extension `.tcp_port(...)`
        // explicitly splits them. The `port` field already
        // incorporates the CAS / CA / default precedence via
        // `cas_server_port()` (see `IocApplication::new`).
        // `tcp_port` here remains `Some(...)` only when the caller
        // explicitly invoked `.tcp_port(...)`; otherwise the runner
        // will inherit `port`.
        let config = IocRunConfig {
            db,
            port,
            tcp_port,
            acf,
            autosave_config,
            autosave_manager,
            shell_commands,
            // Already drained above (H3). Handed over empty so a
            // protocol runner that still inspects the field cannot
            // double-run the hooks.
            after_init_hooks: Vec::new(),
        };

        // epics-base PR #671 parity: race the protocol runner against
        // SIGTERM/SIGINT so a `kill` (or Ctrl+C on the controlling
        // terminal) cleanly returns Ok(()) instead of leaving the
        // future suspended forever. The CA/PVA runners already wire
        // their own signal handlers when used standalone; this one
        // covers the `IocApplication::run` entry point where the
        // runner closure may not (e.g., a custom user runner that
        // only sleeps on `pending()`).
        let runner_fut = protocol_runner(config);
        tokio::pin!(runner_fut);
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

        tokio::select! {
            biased;
            // Runner takes priority: if it completes naturally
            // before any signal arrives, propagate its result.
            res = &mut runner_fut => res,
            _ = ctrl_c => {
                tracing::info!(target: "epics_base_rs::ioc_app", "SIGINT received, shutting down IOC");
                Ok(())
            }
            _ = sigterm => {
                tracing::info!(target: "epics_base_rs::ioc_app", "SIGTERM received, shutting down IOC");
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
                if let Some(crate::types::EpicsValue::String(snam)) =
                    instance.record.get_field("SNAM")
                {
                    if let Some(sub_fn) = registry.get(snam.as_str_lossy().as_ref()) {
                        instance.subroutine = Some(sub_fn.clone());
                    }
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
            crate::runtime::task::spawn(async move {
                while intr_rx.recv().await.is_some() {
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
/// interrupt) and yields a channel of field-deltas; the framework owns the
/// post (`post_property_fields`). Returns the number of drains wired.
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
                    crate::runtime::task::spawn(async move {
                        // Each message is the full setEnums field block; post
                        // it DBE_PROPERTY so clients re-read the choices.
                        while let Some(fields) = rx.recv().await {
                            let _ = db_clone.post_property_fields(&rec_name, fields);
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
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Serialises the initHooks tests — `HOOKS` is process-global, so
    /// two tests announcing at once would observe each other's
    /// callbacks. The state machine here is small; a mutex is enough.
    static INIT_HOOK_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    fn production_scope(src: &str) -> &str {
        match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        }
    }

    /// # Invariant
    ///
    /// MUST: every thread this module creates take its band **and** its OS
    /// name through `enter_ioc_thread`. MUST NOT: an iocsh thread run at the
    /// priority it inherited from `POSIX_Init`.
    ///
    /// Both threads here run iocsh command bodies — the startup script, and
    /// the `afterIocRunning` queue (epics-base PR #558). In C that is one
    /// thread, the shell, and base-on-RTEMS bands it explicitly:
    /// `epicsThreadSetPriority(epicsThreadGetIdSelf(), epicsThreadPriorityIocsh)`
    /// (`libcom/RTEMS/posix/rtems_init.c:1002`), under the comment *"Override
    /// RTEMS Posix configuration, it gets started with posix prio 2"*. That is
    /// the same inheritance defect the port has: RTEMS pthreads inherit their
    /// creator's parameters (`cpukit/posix/src/pthreadattrdefault.c:49-58`)
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
        let prod = production_scope(include_str!("ioc_app.rs"));

        assert_eq!(
            prod.matches("MandatoryThread::new(").count(),
            2,
            "the startup-script thread and the afterIocRunning thread"
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
        for name in ["iocsh-startup", "iocsh-after-ioc-running"] {
            let at = prod
                .find(&format!("\"{name}\","))
                .unwrap_or_else(|| panic!("the {name} thread moved; update this guard"));
            let head = &prod[at..(at + 700).min(prod.len())];
            assert!(
                head.contains("ThreadPriority::Iocsh"),
                "{name} must be declared at `ThreadPriority::Iocsh` \
                 (rtems_init.c:1002)"
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
        let a = std::env::temp_dir().join("qsrv_q_a.json");
        let b = std::env::temp_dir().join("qsrv_q_b.json");
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
                Ok(DeviceReadOutcome::computed())
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
