// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that hand-builds its own tokio runtime; runs and passes in the exec-backend suite.
mod access_commands;
/// C `asInit` (`asDbLib.c:151-154`) with no shell around it — `iocBuild_2`
/// is its other caller (`iocInit.c:187`).
#[cfg(test)]
pub(crate) use access_commands::AsInitOutcome;
pub(crate) use access_commands::{as_init, as_shutdown};
mod breakpoint_commands;
mod commands;
pub use commands::{DbLoadRecordsHook, db_load_records_hook, set_db_load_records_hook};
pub(crate) use commands::{db_get_string, db_records_once_only, record_names_type_major};
/// TAB completion for the interactive editor. Host-only, for the same
/// reason `run_repl_interactive` is: it is built on `rustyline`.
#[cfg(not(epics_embedded_target))]
mod completion;
mod core_commands;
mod dbstatic_commands;
pub(crate) mod misc_commands;
mod queue_commands;
pub mod registry;
mod registry_commands;
mod rtems_commands;
mod time_commands;
/// The iocsh *variable* table — C `iocshRegisterVariable`
/// (`iocsh.cpp:715-765`). Public because a knob registered this way can
/// live in any crate: pvxs registers `pvaLinkNWorkers` from its pvalink
/// module (`pvxs/ioc/pvalink.cpp:318-333`), not from base.
pub mod vars;

/// libCom `macParseDefns`-equivalent quote/escape-aware splitter for IOC
/// macro definition strings. Re-exported as the single owner of that
/// grammar so cross-crate macLib consumers (QSRV `dbLoadGroup`) reuse it
/// rather than duplicating a raw comma splitter.
pub use crate::runtime::mac_lib::macro_defn_pairs;

/// The RTEMS operator commands — C `iocshRegisterRTEMS`. Re-exported because
/// an RTEMS IOC's `main` is the only caller: C registers them from the RTEMS
/// boot path rather than from a registrar every IOC runs, so a hosted
/// `softioc-rs` must not reach them.
pub use rtems_commands::register_rtems_commands;

/// Declare `registrar()` lines that a sibling crate's compiled-in feature
/// set provides, so `dbDumpRegistrar` reports them beside this crate's own.
///
/// Public for the reason [`vars`] is: C resolves every `registrar(name)` in
/// the expanded `.dbd` against ONE linked image, so `softIoc.dbd`'s
/// `rsrvRegistrar` — whose body is `rsrv_register_server()` plus
/// `iocshRegister(casr)` (`rsrvIocRegister.c:34-39`) — lands in the same
/// list as the channel filters'. This port splits that image across crates
/// and resolves its `.dbd` at build time, so a registrar implemented in
/// `epics-ca-rs` has no `.dbd` line to arrive on and no way into the list.
/// This is the whole of the seam: the name, from the crate that carries the
/// behaviour, on the same footing as a name a `dbLoadDatabase` read.
pub fn add_registrars(names: &[String]) {
    dbstatic_commands::add_registrars(names);
}

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::sync::{Arc, RwLock};

use crate::runtime::log::{
    ANSI_ESC_BLUE, ANSI_ESC_BOLD, ANSI_ESC_RED, ANSI_ESC_RESET, ANSI_ESC_UNDERLINE,
};
use crate::server::database::PvDatabase;
use epics_libcom_rs::cfmt::CFloat;
use registry::*;

/// Error-handling mode set by the `on error` command — C `OnError`
/// (`iocsh.cpp:982-986`). `halt` and `wait <delay>` are ONE C state
/// (`onerr = Halt` plus `scope.timeout`), so they are one variant here:
/// a timeout that is zero, negative or infinite suspends the thread,
/// a positive finite one stalls it and lets the script run on
/// (`iocsh.cpp:1131-1142`).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
enum OnError {
    /// Default: report the error and run the next line.
    #[default]
    Continue,
    /// Stop the script at the first failing line (return its error).
    Break,
    /// Suspend the shell thread, or stall it for `timeout` seconds.
    Halt { timeout: f64 },
}

/// What C's error reaction decides for the running script — both halves
/// of it, which is why it is a type and not a `bool`.
///
/// C's loop (`iocsh.cpp:1122-1143`) makes two independent decisions at
/// once: whether to leave the loop, and whether `ret` becomes `-1`. They
/// do not coincide — the `Halt`-with-a-positive-timeout arm assigns
/// `ret = -1` and then keeps running lines. A `bool stop` could only
/// carry the first, so the script's exit status was reconstructed from
/// "did any line fail", which is C's `scope.errored`, not C's `ret`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ErrorReaction {
    /// `on error continue`: `ret` untouched, next line runs.
    Resume,
    /// `on error wait <delay>`: `ret = -1`, then the next line runs
    /// anyway (`iocsh.cpp:1132-1141`).
    ResumeFailed,
    /// `on error break` / `halt`: `ret = -1` and out of the loop.
    Stop,
}

/// What a line calls once C's lookup and argument conversion have passed.
enum Call {
    IocshLoad,
    IocshCmd,
    On,
    Help,
    Registered(CommandDef, Vec<ArgValue>),
}

/// Whether a line reached a command function.
///
/// C sets `scope.errored` ahead of the registry lookup — "error unless a
/// function is actually called" (`iocsh.cpp:1251`) — and clears it in the one
/// place a function is about to be called (`:1268`). Nothing else clears it,
/// so this is the second fact a line carries, independent of whether it
/// failed: a comment, a line macro expansion emptied, a redirect that would
/// not open and a `<` include all reach no command and leave a pending error
/// exactly as they found it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Dispatch {
    /// A command function ran (C cleared the flag just before calling it).
    Ran,
    /// Nothing was called; C `continue`s without touching the flag.
    Nothing,
}

/// C `iocshScope` (`iocsh.cpp:989-996`) — the error policy of ONE
/// `iocshBody` entry. C declares it as a fresh automatic inside
/// `iocshBody` and chains it to the thread context as the innermost of a
/// stack (`:1109-1110`, `:1310-1316`), so an `on error break` taken by an
/// included script dies with that script instead of reaching the
/// caller's next line.
/// C's `iocshBody` locals `filename` and `lineno` (`iocsh.cpp:1060-1063`,
/// `:1157`) — the pair `showError` prefixes an iocsh-raised diagnostic
/// with (`:209-210`). They live on the scope because that is what they
/// are in C: automatics of the one `iocshBody` call, so an included
/// script names itself and the caller's next diagnostic names the caller
/// again.
#[derive(Clone, Debug, PartialEq)]
struct SourceFile {
    /// C's `filename`, which is the script path past its last `/`
    /// (`iocsh.cpp:1060-1063`) — never the path the caller passed.
    base: String,
    /// C's `lineno` (`iocsh.cpp:1157`), 1-based and advanced for every
    /// line read, comments included.
    lineno: usize,
}

/// C `iocsh.cpp:1060-1063`: `strrchr(pathname, '/')`, and `'/'` only —
/// C does not special-case a backslash even on Windows builds.
fn script_basename(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

#[derive(Clone, Debug, PartialEq)]
struct IocshScope {
    on_error: OnError,
    /// C `scope.errored` (`iocsh.cpp:993`), carrying the diagnostic C keeps
    /// no room for. `Some` is C's `true`.
    ///
    /// It is STICKY, which is the whole of the difference between C's policy
    /// and "react to the failing line": C reads it at the top of every pass
    /// of the line loop (`:1122`) and only a command that was actually
    /// dispatched clears it (`:1268`), so one failing line makes `on error
    /// wait` stall once for every following line until something runs.
    /// Measured on `softIoc` R7.0.10: `on error wait 1`, a failing line, two
    /// comments and `exit` takes 3.1 s and prints three `Waiting` lines.
    ///
    /// The message is what `Self::run_script` returns when the reaction
    /// stops the script; the `iocshCmd` scope, whose reaction result nothing
    /// reads, leaves it empty.
    errored: Option<String>,
    /// C `scope.interactive` (`iocsh.cpp:1045-1050`): set whenever
    /// `iocshBody` reads stdin rather than a file or a command string —
    /// terminal or not. `on error` is refused there (`:1532-1534`) and a
    /// failing line never triggers the reaction (`:1122`).
    interactive: bool,
    /// Set for the file-script entries counted against
    /// [`MAX_SCRIPT_DEPTH`].
    file_script: bool,
    /// C's `filename`/`lineno`. `None` is C's `filename == NULL`: the
    /// interactive shell (`iocsh.cpp:1046-1051`, where `pathname` is
    /// NULL) and an `iocshCmd`/`iocshRun` command string (`:1078`),
    /// both of which `showError` reports without a location prefix.
    source: Option<SourceFile>,
}

impl IocshScope {
    /// A `<` include, an `iocshLoad` or the startup script — C's plain
    /// ctor default (`iocsh.cpp:995` `onerr(Continue)`) over the
    /// `filename` C derives from the path it opened (`:1060-1063`).
    fn script(path: &[u8]) -> Self {
        Self {
            on_error: OnError::Continue,
            errored: None,
            interactive: false,
            file_script: true,
            source: Some(SourceFile {
                // Display-only: `showError`'s `filename`.
                base: String::from_utf8_lossy(script_basename(path)).into_owned(),
                lineno: 0,
            }),
        }
    }

    /// `iocshCmd` / `iocshRun` — C `iocsh.cpp:1079-1080`, "use of
    /// iocshCmd() implies \"on error break\"".
    fn command_line() -> Self {
        Self {
            on_error: OnError::Break,
            errored: None,
            interactive: false,
            file_script: false,
            // C reaches `iocshBody` with `filename` still NULL here.
            source: None,
        }
    }

    /// The stdin REPL.
    fn interactive() -> Self {
        Self {
            on_error: OnError::Continue,
            errored: None,
            interactive: true,
            file_script: false,
            // `pathname == NULL`, so C leaves `filename` NULL too.
            source: None,
        }
    }
}

/// Interactive IOC shell with extensible command registration.
pub struct IocShell {
    registry: Arc<RwLock<CommandRegistry>>,
    ctx: CommandContext,
    /// C's `iocshScope` chain (`iocsh.cpp:989-1000`), innermost last:
    /// one entry per `iocshBody`-equivalent entry, pushed by
    /// [`IocShell::enter_scope`] and popped by [`ScopeGuard`]. `RefCell`
    /// because the shell drives one script at a time on a single thread;
    /// the `on error` command mutates the innermost entry mid-script.
    /// Empty outside every script — C's `context->scope == NULL`, where
    /// `on error` does nothing at all (`iocsh.cpp:1526-1528`).
    ///
    /// The `file_script` entries are also the include-nesting count. C
    /// has no explicit bound: a self-including script survives only
    /// because each nested include holds its `FILE*` open and `fopen`
    /// eventually fails at the process fd limit (epics-base#499). This
    /// port reads the whole script and closes the fd before recursing,
    /// so without an explicit cap the recursion is bounded only by the
    /// thread stack — a Rust stack-overflow abort at boot. See
    /// [`MAX_SCRIPT_DEPTH`].
    scopes: std::cell::RefCell<Vec<IocshScope>>,
}

thread_local! {
    /// C's `MAC_HANDLE` scope stack (`iocsh.cpp:1099` `macCreateHandle`,
    /// `:1112` `macPushScope`/`macInstallMacros`): a `<` include reached
    /// from inside an `iocshLoad` sees the load's macros, and each frame
    /// is the whole visible set, so a lookup is one map read and popping
    /// a frame restores the outer set by construction.
    ///
    /// The handle is thread-private, not per-shell: `iocshBody`
    /// reaches it through `epicsThreadPrivateGet(iocshContextId)`
    /// (`iocsh.cpp:1095`) and so does `iocshEnvClear` (`:1376-1379`),
    /// which is what lets `epicsEnvSet` — an ordinary registered command
    /// with no shell handle — clear a macro that shadows the variable it
    /// just set.
    static MACRO_SCOPE: std::cell::RefCell<Vec<HashMap<Vec<u8>, Vec<u8>>>> =
        std::cell::RefCell::new(vec![HashMap::new()]);
}

/// C `iocshEnvClear` (`iocsh.cpp:1371-1383`) — `macPutValue(handle,
/// name, NULL)`, which deletes the macro from EVERY scope, not just the
/// innermost (`macCore.c:252-268`; the comment there notes iocshEnvClear
/// is exactly why the all-scopes behaviour is kept). `epicsEnvSet` and
/// `epicsEnvUnset` both call it before touching the environment
/// (`os/default/osdEnv.c:49`, `:61`), so an `iocshLoad("inner.cmd","PORT=OLD")`
/// macro stops shadowing the environment the moment the loaded script
/// sets `PORT` itself.
pub(crate) fn iocsh_env_clear(name: &[u8]) {
    MACRO_SCOPE.with(|scope| {
        for frame in scope.borrow_mut().iter_mut() {
            frame.remove(name);
        }
    });
}

/// Deepest `<` / `iocshLoad` script nesting the shell will enter before
/// refusing the include — the explicit form of C's incidental fd-limit
/// backstop (epics-base#499). 32 matches the `db_loader`'s
/// `max_include_depth` for DB-file includes.
const MAX_SCRIPT_DEPTH: usize = 32;

/// Ticket for one `iocshBody`-equivalent entry (C `iocsh.cpp:1034`
/// `iocshScope scope;`) — every exit path of the executors pops the
/// scope via `Drop`, which is what makes the fresh `Continue` default
/// hold for the caller's next line.
///
/// It also carries `iocshBody`'s ok-to-block bracket (`iocsh.cpp:1121-1122`,
/// `:1326-1327`): a shell may wait for the errlog drain, so a line's errlog
/// output comes out before the next line runs.
struct ScopeGuard<'a> {
    shell: &'a IocShell,
    was_ok_to_block: bool,
}

impl Drop for ScopeGuard<'_> {
    fn drop(&mut self) {
        self.shell.scopes.borrow_mut().pop();
        crate::runtime::log::errlog_flush();
        crate::runtime::task::set_thread_ok_to_block(self.was_ok_to_block);
    }
}

/// C `macPushScope` (`iocsh.cpp:1112`) — every exit path of the script
/// executor pops the frame via `Drop`.
struct MacroScopeGuard;

impl Drop for MacroScopeGuard {
    fn drop(&mut self) {
        MACRO_SCOPE.with(|scope| {
            let mut scope = scope.borrow_mut();
            if scope.len() > 1 {
                scope.pop();
            }
        });
    }
}

/// softMain's one `iocsh(NULL)` (`softMain.cpp:250`) in the
/// `IocApplication::run` that holds `_main_`. A protocol runner's shell started
/// by an `iocInit` typed at the prompt is that same statement, queued behind
/// the prompt already reading, so it ends with that one instead of opening a
/// second. Outside a run a prompt is a `main`'s own `iocsh(NULL)`, which C
/// reads at every call.
///
/// Only `_main_` jobs open and close a run's span, so the queue's order puts
/// every prompt the run queued inside it, however late that prompt runs.
static MAIN_PROMPT: std::sync::Mutex<MainPrompt> = std::sync::Mutex::new(MainPrompt::NoRun);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MainPrompt {
    NoRun,
    Unopened,
    Opened,
}

fn set_main_prompt(state: MainPrompt) {
    *MAIN_PROMPT.lock().unwrap_or_else(|e| e.into_inner()) = state;
}

/// One run's [`MAIN_PROMPT`] span: a `_main_` job ahead of the run's first
/// opens it and one behind its last closes it.
pub(crate) struct SoftMainPromptSpan(());

impl SoftMainPromptSpan {
    pub(crate) fn open() -> std::io::Result<Self> {
        crate::runtime::task::run_on_main_thread(|| set_main_prompt(MainPrompt::Unopened))?;
        Ok(Self(()))
    }
}

impl Drop for SoftMainPromptSpan {
    fn drop(&mut self) {
        let _ = crate::runtime::task::run_on_main_thread(|| set_main_prompt(MainPrompt::NoRun));
    }
}

/// C's one iocsh command table — `iocshCommandHead`, the single list
/// `iocshRegister` writes and every `iocshBody` reads (`iocsh.cpp:78-86`,
/// `:684-700`, `:1290-1302`).
///
/// **Invariant: a name registered anywhere in this process is callable from
/// every shell in it.** That is what makes a `.dbd` registrar's command behave
/// the same in `st.cmd` and at the `epics>` prompt in C, with nothing for the
/// registrar to decide.
///
/// Each [`IocShell`] used to build a `CommandRegistry` of its own, and
/// `IocApplication::run` builds up to four shells: the startup script's, the
/// `afterIocRunning` queue's, the interactive tail's, and the protocol
/// runner's. A command therefore lived in whichever of them its owner
/// remembered to hand it to — `register_startup_command` reached the script
/// and nothing else, `register_shell_command` reached everything but the
/// script, and a command returned by a link-set installer could reach the
/// script shell by no route at all, because the installer runs at `iocInit`,
/// after that shell was constructed. Every "command X exists in one shell
/// only" defect is that split; one table removes the split rather than timing
/// a copy between the shells, which would be the same defect one level up.
static COMMAND_REGISTRY: std::sync::OnceLock<Arc<RwLock<CommandRegistry>>> =
    std::sync::OnceLock::new();

/// The process's command table, built with the base command set on first use —
/// C's `iocshRegisterCommon` plus the `*IocRegister.c` registrars, all of which
/// have run before softMain reads a script.
pub(crate) fn command_registry() -> &'static Arc<RwLock<CommandRegistry>> {
    COMMAND_REGISTRY.get_or_init(|| {
        let mut registry = CommandRegistry::new();
        commands::register_builtins(&mut registry);
        Arc::new(RwLock::new(registry))
    })
}

/// C `iocshFree` (`iocsh.cpp:785-804`), from an isolated `iocShutdown`: the
/// command and variable tables go back to what they are born holding — the
/// base set, which C's next `registerRecordDeviceDriver` registers again
/// through `iocshRegisterCommon` — so nothing a registrar of this IOC added
/// is left for the next one.
pub(crate) fn iocsh_free() {
    vars::free();
    let mut registry = CommandRegistry::new();
    commands::register_builtins(&mut registry);
    *command_registry()
        .write()
        .unwrap_or_else(|e| e.into_inner()) = registry;
}

/// Register `def` on the process's command table — C `iocshRegister`
/// (`iocsh.cpp:684-700`), replace-on-duplicate included.
///
/// The owner of `COMMAND_REGISTRY`'s invariant for callers that hold a
/// [`CommandDef`] and no shell: `IocApplication::run` before it starts any
/// shell, a link-set installer during the build. Every shell — already built or
/// not yet built — sees the name, so registering is one fact and not one fact
/// per shell.
pub fn register_command(def: CommandDef) {
    command_registry().write().unwrap().register(def);
}

impl IocShell {
    /// Create a new shell with built-in commands registered.
    ///
    /// `bridge` carries the runtime access the shell's blocking thread needs;
    /// capture it where the runtime is known (`BlockingBridge::capture()` on
    /// the async setup path).
    pub fn new(db: Arc<PvDatabase>, bridge: crate::runtime::task::BlockingBridge) -> Self {
        Self::new_with_acf(
            db,
            bridge,
            crate::server::access_security::new_acf_cell(None),
        )
    }

    /// Create a shell whose `as*` commands administer `acf` — the live
    /// policy cell of the IOC's servers. Server-owning roots
    /// (`IocApplication::run`, a server's `run_with_shell`) must use
    /// this so a script or interactive `asInit` reaches the gates.
    pub fn new_with_acf(
        db: Arc<PvDatabase>,
        bridge: crate::runtime::task::BlockingBridge,
        acf: crate::server::access_security::AcfCell,
    ) -> Self {
        // The process's table, not one of this shell's own — see
        // [`COMMAND_REGISTRY`].
        let registry = command_registry().clone();
        // C `iocshRegisterCommon` publishes the base version and target arch as
        // environment variables at the same point it registers the commands, so
        // the first `dbLoadRecords` can already expand `$(EPICS_VERSION_FULL)`.
        crate::runtime::env::register_iocsh_env_vars();
        let ctx = CommandContext::new_with_acf(db, bridge, acf);
        // C's command table is reachable from any `registryFind`; here the
        // context is told where it is before any command can ask.
        ctx.set_command_registry(&registry);
        Self {
            registry,
            ctx,
            scopes: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// Enter one `iocshBody`-equivalent scope.
    fn enter_scope(&self, scope: IocshScope) -> ScopeGuard<'_> {
        self.scopes.borrow_mut().push(scope);
        let was_ok_to_block = crate::runtime::task::thread_is_ok_to_block();
        crate::runtime::task::set_thread_ok_to_block(true);
        ScopeGuard {
            shell: self,
            was_ok_to_block,
        }
    }

    /// The innermost scope, `None` outside every `iocshBody` equivalent
    /// — C's `context->scope`.
    fn current_scope(&self) -> Option<IocshScope> {
        self.scopes.borrow().last().cloned()
    }

    /// C `iocsh.cpp:1157` `lineno++`, which runs for every line read —
    /// comments included, since C only echoes and `continue`s them
    /// (`:1172-1178`) after the counter has already moved.
    fn set_lineno(&self, lineno: usize) {
        if let Some(source) = self
            .scopes
            .borrow_mut()
            .last_mut()
            .and_then(|scope| scope.source.as_mut())
        {
            source.lineno = lineno;
        }
    }

    /// C `showError` (`iocsh.cpp:203-214`).
    ///
    /// This is how iocsh reports its OWN errors — an unbalanced quote, a
    /// bad redirect, an unconvertible argument, an unregistered command —
    /// as opposed to what a command function prints for itself. C emits
    /// it from the point of the error, to stderr, prefixed with
    /// `ERROR <file> line <n>: ` while `filename` is non-NULL and bare
    /// otherwise (`:209-210`). Both halves are wrapped in `ANSI_RED`
    /// (`ERL_ERROR` is `ANSI_RED("ERROR")`, `errlog.h:298`).
    ///
    /// Measured on `softIoc` R7.0.10-146: a script at
    /// `/tmp/.../sub/d.cmd` whose line 1 is `nosuchcmd` prints
    /// `ERROR d.cmd line 1: Command 'nosuchcmd' not registered.` — the
    /// basename, not the path it was given — and the same miss reached
    /// through `iocshRun "alsonosuch"` prints the bare message, because
    /// that scope's `filename` is NULL.
    ///
    /// The colour is gated on [`use_ansi_color`], which is this port's
    /// standing `NO_COLOR` deviation; C emits the escapes unconditionally
    /// (verified: `NO_COLOR=1 softIoc` still wrote them to a redirected
    /// stderr).
    /// It goes to `epicsGetStderr()`, which a `2>` around an `iocshLoad` or
    /// `iocshCmd` swaps for the inner body's diagnostics.
    fn show_error(&self, msg: &str) {
        let scopes = self.scopes.borrow();
        let source = scopes.last().and_then(|scope| scope.source.as_ref());
        self.ctx
            .eprintln(&format_show_error(source, msg, use_ansi_color()));
    }

    /// Enter the scope of one nested script, refusing past
    /// [`MAX_SCRIPT_DEPTH`] so a self-including script errors out at the
    /// include line instead of overflowing the thread stack.
    ///
    /// A DELIBERATE divergence, and the only one on this path: C sets no
    /// depth at all — `iocshBody` simply re-enters itself (`:1233`) until
    /// the process runs out of file descriptors. Measured on
    /// `bin/linux-x86_64/softIoc` (R7.0.10-146-g8f5015b663d764ad75df), a
    /// chain of 400 scripts each including the next runs to the bottom
    /// and writes nothing to stderr. Matching that would trade a named
    /// refusal for a stack overflow, so the cap stays and says so.
    fn enter_script(&self, path: &[u8]) -> Result<ScopeGuard<'_>, String> {
        let depth = self
            .scopes
            .borrow()
            .iter()
            .filter(|scope| scope.file_script)
            .count();
        if depth >= MAX_SCRIPT_DEPTH {
            return Err(format!(
                "'{}': script include depth exceeds {MAX_SCRIPT_DEPTH} — \
                 recursive '<' / iocshLoad?",
                String::from_utf8_lossy(path)
            ));
        }
        Ok(self.enter_scope(IocshScope::script(path)))
    }

    /// C `macPushScope` + `macInstallMacros` (`iocsh.cpp:1112-1113`):
    /// enter an `iocshLoad`'s macro scope. The new frame starts as a copy
    /// of the visible set, so the loaded script still resolves everything
    /// the caller could.
    fn push_macro_scope(&self, macros: &HashMap<Vec<u8>, Vec<u8>>) -> MacroScopeGuard {
        MACRO_SCOPE.with(|scope| {
            let mut scope = scope.borrow_mut();
            let mut frame = scope.last().cloned().unwrap_or_default();
            frame.extend(macros.iter().map(|(k, v)| (k.clone(), v.clone())));
            scope.push(frame);
        });
        MacroScopeGuard
    }

    /// C `macDefExpand(raw, handle)` (`iocsh.cpp:1184`) — the ONE macro
    /// expansion an iocsh line gets, over the ONE handle that carries both
    /// the pushed `iocshLoad` scope and the environment (`:1033` `pairs[]
    /// = {"", "environ", NULL, NULL}` → `FLAG_USE_ENVIRONMENT`,
    /// `macCore.c:130-133`, `:589-594`). `Err` is C's NULL return: macLib
    /// reports the undefined macro (`macCore.c:895-900`) and
    /// `iocsh.cpp:1184-1187` skips the line instead of running it with the
    /// placeholder text. The `.db` and ACF readers deliberately keep the
    /// lenient rule — `macCreateHandle(&h, NULL)` with a warning
    /// (`dbLexRoutines.c:259,381-386`, `asLibRoutines.c:241`) — and are
    /// not routed through here.
    ///
    /// `None` is C's NULL: the line is refused and NOTHING further is
    /// printed, because macLib has already printed it. C's only message
    /// on this path is macLib's own `errlogPrintf` — measured on
    /// `softIoc R7.0.10`, an `st.cmd` whose first line is
    /// `epicsEnvSet("P", "$(UNSET)")` writes exactly one stderr line. The
    /// port used to hand the caller a message it had built itself, which
    /// `showError` then framed as `ERROR <file> line <N>: …`; once the
    /// expander raised macLib's own notice the operator saw the same
    /// sentence twice, once framed and once not. The expander owns it.
    fn expand_line(&self, raw: &[u8]) -> Option<Vec<u8>> {
        let (expanded, notices) = MACRO_SCOPE.with(|scope| {
            let scope = scope.borrow();
            let macros = scope.last().expect("macro scope stack is never empty");
            crate::server::db_loader::MacroTable::new(
                crate::runtime::mac_lib::MacroDefs::from_byte_map(macros),
                crate::server::db_loader::MacroExpandOptions {
                    env_fallback: true,
                    ..Default::default()
                },
            )
            .expand_bytes_deferred(raw)
        });
        for notice in &notices {
            crate::runtime::log::errlog_printf(notice);
        }
        // C `macDefExpand` returns `NULL` on ANY negative length, so a
        // recursive reference fails the line exactly as an undefined one
        // does (`macCore.c:216-224`, `iocsh.cpp:1184-1187`).
        (!expanded.errored()).then_some(expanded.text)
    }

    /// Register an additional command (thread-safe, takes &self).
    ///
    /// Writes the process's table, so the name is callable from every other
    /// shell too — [`register_command`] without a shell in hand does the same
    /// thing.
    pub fn register(&self, def: CommandDef) {
        self.registry.write().unwrap().register(def);
    }

    /// Execute a single line of input.
    ///
    /// C `iocsh.cpp:1162-1210`: a comment is recognised BEFORE expansion
    /// ("avoids macLib errors from comments"), the line is expanded once,
    /// and a line left empty or commented by that expansion is dropped.
    /// Supports C EPICS iocsh output redirection:
    /// - `command > file` — redirect stdout to file (overwrite)
    /// - `command >> file` — redirect stdout to file (append)
    pub fn execute_line(&self, line: impl AsRef<[u8]>) -> CommandResult {
        self.execute_line_dispatched(line.as_ref()).0
    }

    /// [`Self::execute_line`] with C's second fact about the line: whether it
    /// reached a command. Only the two `iocshBody` equivalents —
    /// [`Self::run_script`] and the `iocshCmd`/`iocshRun` entry — take it,
    /// because they are the only owners of `scope.errored`.
    fn execute_line_dispatched(&self, line: &[u8]) -> (CommandResult, Dispatch) {
        let line = line.trim_ascii();
        if line.is_empty() || line.starts_with(b"#") {
            return (Ok(CommandOutcome::Continue), Dispatch::Nothing);
        }
        match self.expand_line(line) {
            Some(expanded) => self.execute_expanded_line(&expanded),
            // C `:1184-1187`: the scope is marked errored and the line is
            // skipped, without a function ever being looked up — and with
            // no diagnostic of its own, macLib having raised the only one.
            None => (Ok(CommandOutcome::Failed), Dispatch::Nothing),
        }
    }

    /// [`Self::execute_line`] past the one macro expansion — everything
    /// here reads text C has already run through `macDefExpand`, so no
    /// fragment of it is expanded a second time.
    fn execute_expanded_line(&self, line: &[u8]) -> (CommandResult, Dispatch) {
        let line = line.trim_ascii();
        if line.is_empty() || line.starts_with(b"#") {
            return (Ok(CommandOutcome::Continue), Dispatch::Nothing);
        }

        // C `split` (`iocsh.cpp:255-371`) lifts the redirects off the line
        // before anything reads `argv[0]`, so every command — the ones the
        // port dispatches itself included — runs under them, and a malformed
        // line is refused before `openRedirect` can create a target.
        let registry::Split {
            argv,
            mut redirects,
        } = match registry::split(line) {
            Ok(split) => split,
            Err(diag) => return (Err(diag.to_string()), Dispatch::Nothing),
        };

        // C `:1225-1227`: a line that splits to nothing at all — `()`, `,` —
        // ends the body, as `exit` does.
        if argv.is_empty() && redirects.is_empty() {
            return (Ok(CommandOutcome::Exit), Dispatch::Nothing);
        }

        // `< filename` with no command: C `:1233-1243` runs it as
        // `iocshBody(commandFile, NULL, macros)` on the same handle with the
        // same macros, under the line's other redirects — here it simply
        // stays on the current frame.
        if argv.is_empty()
            && let Some(include) = redirects.remove(&0)
        {
            let Some(files) = self.open_redirects(&redirects) else {
                return (Ok(CommandOutcome::Continue), Dispatch::Nothing);
            };
            // C `:1240` keeps the include's failure as a bare flag — the inner
            // `iocshBody` has already printed whatever went wrong — and never
            // CLEARS the caller's flag, because no function of the caller's
            // own was called. Measured on `softIoc` R7.0.10: a `<` of a
            // script whose commands all succeed still leaves the caller
            // waiting on the failure before it. An unreported failure is the
            // one thing C cannot produce here, and it becomes this line's
            // diagnostic so it is still printed exactly once.
            return self.with_redirects(files, || match self.run_script(&include.path) {
                Ok(()) => (Ok(CommandOutcome::Continue), Dispatch::Nothing),
                Err(ScriptFailure::Reported(_)) => (Ok(CommandOutcome::Failed), Dispatch::Nothing),
                Err(ScriptFailure::Unreported(msg)) => (Err(msg), Dispatch::Nothing),
            });
        }

        self.execute_command(&argv, &redirects)
    }

    /// C `iocshCmd(cmd)`: one command line run as an `iocshBody` of its own.
    pub fn iocsh_cmd(&self, cmd: impl AsRef<[u8]>) -> CommandResult {
        let cmd = cmd.as_ref();
        // C `iocsh.cpp:1078-1080`: reaching `iocshBody` with a command line
        // rather than a file "implies 'on error break'", and it is a scope
        // of its own — the mode never reaches the caller's next line.
        let _scope = self.enter_scope(IocshScope::command_line());
        let (outcome, dispatch) = self.execute_line_dispatched(cmd);
        let failure = match &outcome {
            Err(e) => {
                // `showError` with this scope's NULL filename: unframed.
                self.show_error(e);
                Some(e.clone())
            }
            // The command reported for itself, as C's `iocshSetError`
            // callers do.
            Ok(CommandOutcome::Failed) => Some(String::new()),
            Ok(_) => None,
        };
        self.record_line_result(failure, dispatch);
        // C reaches the reaction on the loop pass after the failing line,
        // before `if (raw != NULL) break;` ends the single-line loop, so the
        // implied Break still reports itself.
        let _ = self.react_to_error();
        // Everything is said; C's `iocshCmd` hands back only the status. An
        // `exit` ended this one-line body, never the caller's.
        match outcome {
            Ok(CommandOutcome::Exit) => Ok(CommandOutcome::Continue),
            other => other.or(Ok(CommandOutcome::Failed)),
        }
    }

    /// [`Self::iocsh_cmd`] from inside a command `caller` is running on this
    /// thread, writing where `caller` writes: C's `iocshCmd` reaches the
    /// thread's streams, under whatever redirect the caller's line holds.
    pub(crate) fn nested_iocsh_cmd(
        &self,
        caller: &CommandContext,
        cmd: impl AsRef<[u8]>,
    ) -> CommandResult {
        caller.swap_streams(&self.ctx);
        let result = self.iocsh_cmd(cmd);
        caller.swap_streams(&self.ctx);
        result
    }

    /// Run one split line under its redirects.
    fn execute_command(
        &self,
        tokens: &[IocshString],
        redirects: &BTreeMap<u8, Redirect>,
    ) -> (CommandResult, Dispatch) {
        // C breaks out of the body on `exit` before `openRedirect`
        // (`iocsh.cpp:1248`), so `exit > f` never creates `f`.
        let exit = tokens.first().is_some_and(|t| t == "exit");
        let files = if exit {
            Vec::new()
        } else {
            match self.open_redirects(redirects) {
                Some(files) => files,
                // C `:1254` skips the whole block that sets and clears
                // `scope.errored` when `openRedirect` fails, so the line
                // neither fails nor clears a pending error.
                None => return (Ok(CommandOutcome::Continue), Dispatch::Nothing),
            }
        };

        if tokens.is_empty() {
            return (Ok(CommandOutcome::Continue), Dispatch::Nothing);
        }
        // C looks the command up and converts its arguments with the files
        // open but not yet swapped in (`startRedirect` is `:1272`, after the
        // `cvtArg` loop), so what it frames there reaches the console.
        let call = match self.resolve_call(tokens) {
            Ok(call) => call,
            Err(refused) => return refused,
        };
        self.with_redirects(files, || self.run_call(call, tokens))
    }

    /// C `openRedirect` (`iocsh.cpp:376-397`): every fd's file, in fd order,
    /// including the fds 3-9 it will not swap. A failure is `Can't open` and
    /// no command; the files already open close as C's `stopRedirect` does.
    fn open_redirects(&self, redirects: &BTreeMap<u8, Redirect>) -> Option<Vec<(u8, File)>> {
        let mut files = Vec::with_capacity(redirects.len());
        for (&fd, redir) in redirects {
            let opened = registry::bytes_path(&redir.path)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
                .and_then(|path| {
                    if fd == 0 {
                        File::open(path)
                    } else if redir.append {
                        std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(path)
                    } else {
                        File::create(path)
                    }
                });
            match opened {
                Ok(file) => files.push((fd, file)),
                Err(e) => {
                    self.show_error(&format!(
                        "Can't open '{}': {}",
                        String::from_utf8_lossy(&redir.path),
                        c_strerror(&e)
                    ));
                    return None;
                }
            }
        }
        Some(files)
    }

    /// C `startRedirect`/`stopRedirect` (`iocsh.cpp:399-451`) around `f`:
    /// `case 0`/`1`/`2` swap the thread's stdin, stdout and stderr and
    /// restore them after; fds 3-9 have no `case`, so their file stays open
    /// and swaps nothing.
    fn with_redirects<R>(&self, mut files: Vec<(u8, File)>, f: impl FnOnce() -> R) -> R {
        let Some((fd, file)) = files.pop() else {
            return f();
        };
        use crate::runtime::stdio;
        match fd {
            0 => stdio::redirect_stdin(std::io::BufReader::new(file), || {
                self.with_redirects(files, f)
            }),
            1 => stdio::redirect_stdout(file, || self.with_redirects(files, f)),
            2 => stdio::redirect_stderr(file, || self.with_redirects(files, f)),
            _ => {
                let _held = file;
                self.with_redirects(files, f)
            }
        }
    }

    /// The `registryFind` + `cvtArg` half of C `iocshBody` (`:1258-1291`):
    /// what the line will call, or the refusal that stops it short of a call.
    fn resolve_call(&self, tokens: &[IocshString]) -> Result<Call, (CommandResult, Dispatch)> {
        let cmd_name = &tokens[0];
        // `iocshLoad`, `iocshCmd`, `iocshRun`, `on` and `help` are registered
        // commands in C (`iocsh.cpp:1603-1613`); the port runs them here
        // because each needs the shell, which a registry handler cannot
        // reach. Their arguments are strings or an argv, so none can fail
        // conversion.
        match cmd_name.as_bytes() {
            b"iocshLoad" => return Ok(Call::IocshLoad),
            b"iocshCmd" | b"iocshRun" => return Ok(Call::IocshCmd),
            b"on" => return Ok(Call::On),
            b"help" => return Ok(Call::Help),
            _ => {}
        }

        // C releases the command table before it calls: `registryFind` returns
        // the entry and `(*found->def.func)(&argBuf[0])` runs with nothing held
        // (`iocsh.cpp:1258-1281`). That is what lets a command register more
        // commands while it runs — `dbLoadDatabase` and every `.dbd` registrar
        // do — and with one process-wide table (see `COMMAND_REGISTRY`) it is
        // load-bearing here too: the script's `iocInit` line performs the build,
        // and the build registers each link set's `dbcaxr`/`pvxr`/… before that
        // line returns. Holding the read guard across the handler made that a
        // deadlock. `CommandDef` is `Arc`-backed so the clone is the lookup's
        // result, not a copy of the command.
        // Every registered name is text, so a non-UTF-8 one misses as C's
        // `registryFind` would.
        let found = cmd_name
            .to_str()
            .ok()
            .and_then(|name| self.registry.read().unwrap().get(name).cloned());

        // C `iocsh.cpp:1301-1304`: the registry miss arm is
        // `showError(filename, lineno, ANSI_RED("Command '%s' not
        // registered."), tokenize.argv[0])` and NOTHING else. It does not
        // touch `ret`, and it does not need to touch `scope.errored`,
        // which `:1251` has already set to `true` ahead of the lookup
        // ("error unless a function is actually called") and which only
        // `:1268` clears, once a function is actually invoked. So the
        // line counts as failed while the script's status stays 0, and
        // under the default `on error continue` the next line runs.
        //
        // That pair — failed, already reported — is
        // `Ok(CommandOutcome::Failed)`. `Err` was the wrong half of the
        // channel: it says "failed AND print this", which hands the
        // rendering to whichever caller happens to catch it, and the
        // three callers render differently (`run_script`
        // `<path>:<n>: Error: ...`, the REPL `Error: ...`, an embedder
        // calling `execute_line` not at all). C prints one line from one
        // place no matter who is driving, so the print belongs here.
        //
        // Measured on `softIoc` R7.0.10-146, script `nosuchcmd` / `dbl`:
        // stderr `ERROR a.cmd line 1: Command 'nosuchcmd' not
        // registered.`, `dbl` runs, exit status 0.
        let Some(def) = found else {
            self.show_error(&format!("Command '{cmd_name}' not registered."));
            return Err((Ok(CommandOutcome::Failed), Dispatch::Nothing));
        };

        // C `cvtArg` failing breaks out of the argument loop WITHOUT reaching
        // the call (`:1288-1291`), so the `:1251` set stands.
        match parse_args(&tokens[1..], &def.args) {
            Ok(args) => Ok(Call::Registered(def, args)),
            Err(e) => Err((Err(e), Dispatch::Nothing)),
        }
    }

    /// The call itself (C `:1266-1286`), under whatever redirect the line
    /// carries.
    fn run_call(&self, call: Call, tokens: &[IocshString]) -> (CommandResult, Dispatch) {
        match call {
            // `iocshLoad`, `iocshCmd` and `iocshRun` are dispatched: the flag
            // is cleared before they run and set again only from the
            // `iocshSetError` each one ends with.
            Call::IocshLoad => {
                let macros = tokens
                    .get(2)
                    .map(|s| commands::parse_macro_string(s))
                    .unwrap_or_default();
                // A NULL pathname skips the `IOCSH_STARTUP_SCRIPT`
                // record and reaches `iocshBody(NULL, NULL, macros)`
                // (`iocsh.cpp:1346-1352`), which is the stdin REPL —
                // measured: a bare `iocshLoad` under `</dev/null` reads
                // EOF at once, returns 0, and the enclosing script runs
                // on. Refusing the line here broke that under
                // `on error break`.
                let Some(path) = tokens.get(1) else {
                    let _scope = self.push_macro_scope(&macros);
                    return self.command_ran(self.run_repl().map(|()| CommandOutcome::Continue));
                };
                // `iocshLoadCallFunc` is `iocshSetError(iocshLoad(...))`
                // (`:1492-1495`) — the flag, not a second message, for
                // the same reason as `<` above.
                self.command_ran(match self.run_script_with_macros(path, &macros) {
                    Ok(()) => Ok(CommandOutcome::Continue),
                    Err(ScriptFailure::Reported(_)) => Ok(CommandOutcome::Failed),
                    Err(ScriptFailure::Unreported(msg)) => Err(msg),
                })
            }
            // `iocshCmd` and `iocshRun` are one entry point in C:
            // `iocshCmd(cmd)` is literally `iocshRun(cmd, NULL)` and
            // `iocshRun(cmd, macros)` is `iocshBody(NULL, cmd, macros)`
            // (`iocsh.cpp:1335-1353` @R7.0.10). Both therefore run
            // exactly ONE command line — `iocshBody` consumes the
            // string in a single pass (`if (raw != NULL) break;`) and
            // words are separated only by `" \t(),\r"`
            // (`iocsh.cpp:271`). There is no `;` separator anywhere in
            // that file, so `iocshRun("a; b")` looks up the single
            // command `a;` and reports it unregistered.
            Call::IocshCmd => {
                // `iocshRun` returns 0 immediately for a NULL command
                // (`iocsh.cpp:1354-1360`), and `iocshCmd` is that same
                // call, so a line naming no command runs nothing and
                // does not fail. Reporting a usage line here instead
                // made `on error break` abandon the rest of the script.
                let Some(cmd) = tokens.get(1) else {
                    return (Ok(CommandOutcome::Continue), Dispatch::Ran);
                };
                self.command_ran(self.iocsh_cmd(cmd))
            }
            // `on error continue|break|halt|wait <delay>` — sets how the
            // running script reacts to a failing line. Mirrors C
            // `iocsh.cpp` `onCallFunc`.
            Call::On => self.command_ran(self.handle_on_command(tokens)),
            // Special handling for help — needs access to the registry.
            Call::Help => {
                let registry = self.registry.read().unwrap();
                self.command_ran(self.execute_help(&tokens[1..], &registry))
            }
            Call::Registered(def, args) => self.command_ran(def.handler.call(&args, &self.ctx)),
        }
    }

    /// The one rule for a line's diagnostic. C frames with `showError`
    /// (`ERROR <file> line <n>:`) only what `iocshBody` raises before it
    /// calls a function (`iocsh.cpp:1272-1304`); once a command runs, what
    /// it prints is its own text, bare and on the stream it was given. So a
    /// command's `Err` is printed here, inside any redirect, and leaves as
    /// `Failed`: an `Err` past dispatch is always the shell's own.
    fn command_ran(&self, result: CommandResult) -> (CommandResult, Dispatch) {
        let result = result.or_else(|msg| {
            self.ctx.eprintln(&msg);
            Ok(CommandOutcome::Failed)
        });
        (result, Dispatch::Ran)
    }

    /// Execute a script with the `iocshLoad` macros pushed as a scope,
    /// mirroring C `iocshLoad("path", "K=V,...")` (Issue #847). The
    /// macros go onto the shell's one macro handle (C `iocsh.cpp:1112`
    /// `macPushScope` + `macInstallMacros`), so the loaded script's
    /// `$(KEY)` and its environment references expand together in the
    /// single pass `Self::run_script` performs.
    ///
    /// As the `iocshLoad` mirror this is also the level that records
    /// `IOCSH_STARTUP_SCRIPT` — top-level loads enter here (C
    /// `iocsh(pathname)` is `iocshLoad(pathname, NULL)`), `<` includes
    /// enter [`Self::execute_script`] (C `iocshBody`) and never set it.
    pub fn execute_script_with_macros(
        &self,
        path: impl AsRef<[u8]>,
        macros: &HashMap<Vec<u8>, Vec<u8>>,
    ) -> Result<(), String> {
        self.run_script_with_macros(path.as_ref(), macros)
            .map_err(|f| self.report_once(f))
    }

    /// [`Self::execute_script_with_macros`] before the reporting boundary —
    /// what the `iocshLoad` line arm needs, because C's caller keeps the
    /// flag and prints nothing.
    fn run_script_with_macros(
        &self,
        path: &[u8],
        macros: &HashMap<Vec<u8>, Vec<u8>>,
    ) -> Result<(), ScriptFailure> {
        set_startup_script_once(path);
        let _scope = self.push_macro_scope(macros);
        self.run_script(path)
    }

    /// Execute a script file line by line, echoing each line like C++ iocsh.
    ///
    /// C parity: errors from individual commands are reported but do not
    /// abort execution, and — under the default `on error continue` — they
    /// do not change the return value either. C's `ret` starts at 0
    /// (`iocsh.cpp:1037`) and is assigned `-1` only by the Break and Halt
    /// arms of the error reaction (`:1127`, `:1132`); `iocshSetError` sets
    /// `scope.errored`, which is what feeds that reaction, and never `ret`
    /// itself. A startup script whose commands failed under `continue`
    /// therefore returns success, and `softMain.cpp:231`'s
    /// `errIf(iocsh(...))` lets the IOC come up.
    ///
    /// This is the `iocshBody`-with-a-file level (`<` includes land
    /// here) — it does not record `IOCSH_STARTUP_SCRIPT`; see
    /// [`Self::execute_script_with_macros`]. C `iocsh.cpp:1233` re-enters
    /// `iocshBody` on the same handle for a `<`, so the include keeps the
    /// enclosing scope and no macro map is threaded here.
    pub fn execute_script(&self, path: impl AsRef<[u8]>) -> Result<(), String> {
        self.run_script(path.as_ref())
            .map_err(|f| self.report_once(f))
    }

    /// One command line run outside any script, with its status checked.
    ///
    /// C's argv-driven commands (`softMain.cpp:174-222`: `asSetFilename`,
    /// `dbLoadRecords` for `-d` and `-x`) are direct calls guarded by
    /// `errIf`, not script lines: no `on error` reaction applies, there is
    /// no line number to frame a diagnostic with, and — the reason this
    /// goes to `execute_expanded_line` rather than [`Self::execute_line`] —
    /// nothing macro-expands them, so a `$(` in a filename or a
    /// substitution reaches the command as typed.
    ///
    /// A reporting boundary like [`Self::execute_script`]: an `Err`
    /// returned here has already been printed, either by the command
    /// itself (C `dbLoadRecords` writes its own two lines) or by the
    /// framing below.
    pub fn execute_line_reported(&self, line: impl AsRef<[u8]>) -> Result<(), String> {
        let line = line.as_ref();
        match self.execute_expanded_line(line).0 {
            Ok(CommandOutcome::Continue | CommandOutcome::Exit) => Ok(()),
            Ok(CommandOutcome::Failed) => {
                Err(format!("'{}' failed", String::from_utf8_lossy(line)))
            }
            Err(e) => {
                self.show_error(&e);
                Err(e)
            }
        }
    }

    /// The one place a [`ScriptFailure`] loses its reported/unreported
    /// distinction, by making it true: anything still unsaid is said here.
    /// Every public `Result<(), String>` this shell hands out therefore
    /// carries a message the operator has already seen, which is what lets
    /// `<` and `iocshLoad` keep C's bare flag.
    fn report_once(&self, failure: ScriptFailure) -> String {
        match failure {
            ScriptFailure::Reported(msg) => msg,
            ScriptFailure::Unreported(msg) => {
                self.show_error(&msg);
                msg
            }
        }
    }

    /// The shared C `iocshBody` line loop for both script entry points.
    ///
    /// Order is C's (`iocsh.cpp:1162-1210`): a comment is recognised and
    /// echoed BEFORE expansion, the line is expanded exactly once, a
    /// failed expansion marks the script errored without executing the
    /// line, and the ECHO shows the EXPANDED text.
    fn run_script(&self, path: &[u8]) -> Result<(), ScriptFailure> {
        // The cap is this port's own failure — C has none, it runs out of
        // file descriptors — so nothing has printed it yet.
        let _depth = self.enter_script(path).map_err(ScriptFailure::Unreported)?;
        // C `iocsh.cpp:1053-1058`: `iocshBody` reports the open failure
        // itself — unframed, no `ERROR <file> line <n>:` prefix, because
        // there is no line yet — and returns a bare -1. Every other
        // diagnostic this loop raises is likewise printed where it happens,
        // so the returned string is only ever a summary for the caller.
        // Bytes, as C's `fgets` reads them: a line need not be UTF-8.
        let read = registry::bytes_path(path)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
            .and_then(std::fs::read);
        let content = match read {
            Ok(content) => content,
            Err(e) => {
                let reason = c_strerror(&e);
                let path = String::from_utf8_lossy(path);
                self.ctx
                    .eprintln(&paint_error(&format!("Can't open {path}: {reason}")));
                return Err(ScriptFailure::Reported(format!(
                    "cannot read '{path}': {reason}"
                )));
            }
        };

        // C's `ret` (`iocsh.cpp:1037`): 0 until the error reaction runs a
        // Break or Halt arm, never merely because a line failed.
        let mut failed: Option<String> = None;
        for (line_num, raw) in join_backslash_continuations(&content) {
            // C runs the reaction at the TOP of the pass, BEFORE the line is
            // read (`iocsh.cpp:1122-1143`), against a `scope.errored` nothing
            // has reset. So it fires again for every line that dispatches
            // nothing, and it fires for the last line too — see after the
            // loop.
            if let Some(msg) = self.react_in_script().map_err(ScriptFailure::Reported)? {
                failed = Some(msg);
            }

            // C `iocsh.cpp:1157`: the counter moves before the comment
            // check, so every diagnostic this line raises — including one
            // from a command the line reaches through `<` or `iocshLoad`
            // — names the physical line the operator is looking at.
            self.set_lineno(line_num);
            // Comments are echoed but never expanded — C's own comment
            // says this "avoids macLib errors from comments".
            // `#-` silent comments are not echoed (iocsh.cpp:1196-1204).
            if raw.trim_ascii_start().starts_with(b"#") {
                if let Some(echo) = script_echo(&raw, ANSI_ESC_BLUE, use_ansi_color()) {
                    self.ctx.println_bytes(&echo);
                }
                continue;
            }

            // Echo the logical line (C++ iocsh behavior — continuations
            // are already collapsed so the echo shows the joined line)
            // after expansion, as C does.
            let (outcome, dispatch) = match self.expand_line(&raw) {
                Some(expanded) => {
                    if let Some(echo) = script_echo(&expanded, ANSI_ESC_BOLD, use_ansi_color()) {
                        self.ctx.println_bytes(&echo);
                    }
                    self.execute_expanded_line(&expanded)
                }
                // C `:1184-1187`: errored, and no function looked up.
                None => (Ok(CommandOutcome::Failed), Dispatch::Nothing),
            };

            // The two facts a line carries are decided separately, so no
            // arm can print by virtue of having failed or vice versa.
            let diagnostic = match &outcome {
                Ok(CommandOutcome::Exit) => {
                    // C `exit` breaks the loop where it is read (`:1240`), so
                    // the next pass — and its reaction — never happens.
                    return failed
                        .map(|m| Err(ScriptFailure::Reported(m)))
                        .unwrap_or(Ok(()));
                }
                Ok(CommandOutcome::Continue | CommandOutcome::Failed) => None,
                Err(e) => Some(e.clone()),
            };
            let line_failed = matches!(outcome, Ok(CommandOutcome::Failed) | Err(_));

            if let Some(e) = &diagnostic {
                self.show_error(e);
            }
            let failure = line_failed.then(|| {
                let path = String::from_utf8_lossy(path);
                match &diagnostic {
                    Some(e) => format!("{path}:{line_num}: {e}"),
                    None => format!("{path}:{line_num}"),
                }
            });
            self.record_line_result(failure, dispatch);
        }
        // C reaches the top of one more pass before `epicsReadline` returns
        // NULL and ends the loop, so a failure left by the LAST line gets its
        // reaction as well.
        if let Some(msg) = self.react_in_script().map_err(ScriptFailure::Reported)? {
            failed = Some(msg);
        }
        failed
            .map(|m| Err(ScriptFailure::Reported(m)))
            .unwrap_or(Ok(()))
    }

    /// softMain's `iocsh(NULL)`: [`Self::run_repl`], except a second time in
    /// one `IocApplication::run`, which C's one statement never reaches.
    pub fn run_main_prompt(&self) -> Result<(), String> {
        {
            let mut prompt = MAIN_PROMPT.lock().unwrap_or_else(|e| e.into_inner());
            match *prompt {
                MainPrompt::Opened => return Ok(()),
                MainPrompt::Unopened => *prompt = MainPrompt::Opened,
                MainPrompt::NoRun => {}
            }
        }
        self.run_repl()
    }

    /// Run the interactive REPL. Blocks until exit or EOF.
    ///
    /// When stdin is not a terminal (piped input, `<script.cmd` shell
    /// redirect, here-doc, ...) the rustyline line editor is skipped and
    /// lines come straight off stdin. That is an editor choice only: the
    /// PROMPT is not a terminal decoration in C and is not one here either,
    /// see `Self::run_repl_piped`.
    pub fn run_repl(&self) -> Result<(), String> {
        // C `iocsh.cpp:1045-1050` sets `scope.interactive` from
        // `pathname == NULL` — reading stdin, terminal or not — so the
        // piped path below is the interactive scope too.
        let _scope = self.enter_scope(IocshScope::interactive());
        // The interactive rustyline editor is host-only (rustyline pulls `nix`,
        // which does not build for RTEMS or VxWorks). On either embedded
        // target the piped-stdin REPL is the only path until the embedded
        // iocsh wiring lands (a later increment).
        #[cfg(not(epics_embedded_target))]
        {
            use std::io::IsTerminal;
            // C `epicsReadline.c:48` — `IOCSH_HISTEDIT_DISABLE` set (to anything
            // non-empty) means "do not use readline or equivalent", so the line
            // editor is never started and lines come straight off stdin.
            let histedit_disabled = crate::runtime::env_table::IOCSH_HISTEDIT_DISABLE
                .get()
                .is_some();
            if std::io::stdin().is_terminal() && !histedit_disabled {
                return self.run_repl_interactive();
            }
        }
        self.run_repl_piped()
    }

    #[cfg(not(epics_embedded_target))]
    fn run_repl_interactive(&self) -> Result<(), String> {
        // C `gnuReadline.c:49-54`:
        //     long i = 50;                                 /* the table default */
        //     envGetLongConfigParam(&IOCSH_HISTSIZE, &i);
        //     if (i < 0) i = 0;
        //     stifle_history(i);
        // `IOCSH_HISTSIZE` is the parameter an existing `st.cmd` sets; the port
        // previously read a renamed `EPICS_RS_IOCSH_HISTORY_SIZE` with a
        // different default (500), so a site's setting was silently ignored.
        let history_size =
            usize::try_from(crate::runtime::env_table::IOCSH_HISTSIZE.long_or_default())
                .unwrap_or(0);
        let config = rustyline::Config::builder()
            .max_history_size(history_size)
            .map_err(|e| format!("invalid rustyline history config: {e}"))?
            // GNU readline's `rl_complete` — the function C binds TAB to
            // (`iocsh.cpp:640`) — inserts the common prefix and lists the
            // alternatives on a second TAB. That is rustyline's `List`,
            // not its `Circular` default.
            .completion_type(rustyline::CompletionType::List)
            .build();
        let mut rl: rustyline::Editor<completion::IocshCompleter, _> =
            rustyline::Editor::with_config(config)
                .map_err(|e| format!("failed to initialize readline: {e}"))?;
        // C installs its completion hook on the same handle it reads
        // lines from (`iocsh.cpp:639` `rl_attempted_completion_function
        // = &iocsh_attempt_completion`, `:640` `rl_bind_key('\t',
        // rl_complete)`), so TAB completes arguments by their declared
        // type for the life of the interactive shell.
        rl.set_helper(Some(completion::IocshCompleter::new(
            self.registry.clone(),
            self.ctx.db().clone(),
        )));

        // epics-base 8-D `c0da3dd` ANSI color: tint the prompt
        // BRIGHT-GREEN (matching C `ANSI_GREEN` in errlog.h:282 —
        // `\033[32;1m`) and route errors through bold red so an
        // operator can scan a long terminal scrollback for command
        // outcomes. Honour the `NO_COLOR=1` env var convention
        // (<https://no-color.org>) and fall through to plain output
        // when stdout is not a TTY (already TTY-gated by `run_repl`
        // dispatch but defensive).
        let want_color = use_ansi_color();
        // C `iocsh.cpp:1047-1049`: the prompt IS `IOCSH_PS1`, whose compiled
        // default is `ANSI_GREEN("epics> ")` = `\x1b[32;1mepics> \x1b[0m`
        // (`CONFIG_SITE_ENV`, expanded by the generator). A site that sets
        // `IOCSH_PS1` in its `st.cmd` gets its own prompt, as under C; the
        // port used to hard-code the string and never read the parameter.
        //
        // rustyline 18 splits the prompt's *raw* text (what it measures for
        // visible width) from its *styled* text (what it renders), via the
        // `Prompt::raw()` / `Prompt::styled()` trait, and accepts a
        // `(raw, styled)` tuple as a `Prompt`. We pass that tuple so width is
        // always measured on the ANSI-stripped form while the tint still shows
        // wherever the terminal renders ANSI. Embedding the escapes inline in a
        // single prompt string (the pre-18 approach) made rustyline's Windows
        // console backend count the escape bytes as visible columns — its
        // `calculate_position` (tty/windows.rs) sums raw grapheme width, unlike
        // the Unix backend's ANSI-stripping path — which pushed the cursor and
        // any typed/echoed text ~9 columns to the right of `epics> `. Measuring
        // `raw()` fixes that on every platform and satisfies rustyline 18's
        // debug-assert that the measured text carries no `\x1b[`.
        let (raw_prompt, styled_prompt) = iocsh_prompt_if(want_color);
        let prompt = (raw_prompt.as_str(), styled_prompt.as_str());

        loop {
            match rl.readline(&prompt) {
                Ok(line) => {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    let _ = rl.add_history_entry(&line);

                    match self.execute_line(&line) {
                        // C guards the whole error reaction with
                        // `!scope.interactive` (`iocsh.cpp:1122`), so a
                        // failed line changes nothing at the prompt.
                        Ok(CommandOutcome::Continue | CommandOutcome::Failed) => {}
                        Ok(CommandOutcome::Exit) => break,
                        Err(e) => self.show_error(&e),
                    }
                }
                Err(rustyline::error::ReadlineError::Eof) => break,
                Err(rustyline::error::ReadlineError::Interrupted) => continue,
                Err(e) => {
                    self.show_error(&format!("readline error: {e}"));
                    break;
                }
            }
        }

        Ok(())
    }

    /// The same shell, reading a pipe instead of a line editor.
    ///
    /// C prints the prompt from the READER, not from a terminal check:
    /// `epicsReadline.c:75-76` is `if (prompt) { fputs(prompt, stdout);
    /// fflush(stdout); }` and the pin has no `isatty` anywhere outside
    /// `errlog.c:226`, which is about colour on stderr. `iocsh.cpp:1045-1050`
    /// sets a non-NULL prompt for every stdin shell and NULL only for a
    /// script path, so a shell fed from a pipe emits one prompt before every
    /// read INCLUDING the read that meets EOF — three for two lines, on
    /// stdout, unseparated. Measured against R7.0.10's own `softIoc`.
    ///
    /// Suppressing it here made every non-TTY run — which is how the tests
    /// drive this — differ from C on stdout from its first byte.
    fn run_repl_piped(&self) -> Result<(), String> {
        use std::io::{BufRead, Write};
        let (_, prompt) = iocsh_prompt();
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        let mut line = Vec::new();
        loop {
            print!("{prompt}");
            let _ = std::io::stdout().flush();
            line.clear();
            match handle.read_until(b'\n', &mut line) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let trimmed = line.trim_ascii();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match self.execute_line(trimmed) {
                        Ok(CommandOutcome::Continue | CommandOutcome::Failed) => {}
                        Ok(CommandOutcome::Exit) => break,
                        Err(e) => self.show_error(&e),
                    }
                }
                Err(e) => {
                    crate::stderr_println!("stdin read error: {e}");
                    break;
                }
            }
        }
        Ok(())
    }

    /// Handle the `on error ...` command. Tokens are the
    /// already-tokenised line (`["on", "error", "<mode>", ...]`).
    ///
    /// C `onCallFunc` (`iocsh.cpp:1519-1571`) mutates the INNERMOST
    /// scope, does nothing at all when there is none, and refuses the
    /// command outright in an interactive shell. Only an unrecognised
    /// mode word is an error: every other misuse prints the usage and
    /// leaves the line's status clean.
    fn handle_on_command(&self, toks: &[IocshString]) -> CommandResult {
        const USAGE: &str = "Usage: on error [continue | break | halt | wait <delay>]";
        let Some(scope) = self.current_scope() else {
            // Not called through an `iocshBody` equivalent.
            return Ok(CommandOutcome::Continue);
        };
        if scope.interactive {
            self.ctx.eprintln("Interactive shell, 'on error' ignored.");
            return Ok(CommandOutcome::Continue);
        }
        if toks.len() < 3 || toks[1] != "error" {
            self.ctx.eprintln(USAGE);
            return Ok(CommandOutcome::Continue);
        }
        let mode = match toks[2].as_bytes() {
            b"continue" => OnError::Continue,
            b"break" => OnError::Break,
            b"halt" => OnError::Halt { timeout: 0.0 },
            b"wait" => {
                // C parses the delay with `epicsParseDouble` — fractional
                // seconds are the documented spelling — and on a parse
                // failure prints the usage plus a named diagnostic and
                // falls back to 5.0 s. `wait` with no delay at all keeps
                // C's 0.0, which is a plain halt.
                let timeout = match toks.get(3) {
                    None => {
                        self.ctx.eprintln(USAGE);
                        0.0
                    }
                    Some(delay) => {
                        match crate::runtime::stdlib::epics_scan_double(&delay.to_str_lossy()) {
                            Some(secs) => secs,
                            None => {
                                self.ctx.eprintln(USAGE);
                                self.ctx
                                    .eprintln(&format!("Invalid 'on error wait' delay '{delay}'."));
                                5.0
                            }
                        }
                    }
                };
                OnError::Halt { timeout }
            }
            // C marks the scope errored here and nowhere else in the
            // command, which is this port's `Err` for the line.
            _ => return Err(USAGE.into()),
        };
        if let Some(scope) = self.scopes.borrow_mut().last_mut() {
            scope.on_error = mode;
        }
        Ok(CommandOutcome::Continue)
    }

    /// Record what one line did to C's `scope.errored` (`iocsh.cpp:993`) on
    /// the innermost scope.
    ///
    /// C writes that flag from inside `iocshBody`'s own line loop and nowhere
    /// else — its two apparent exceptions, `iocshSetError` (`:1010`) and
    /// `onCallFunc` (`:1538`), are both reached from a command the loop
    /// dispatched — so both `iocshBody` equivalents here funnel through this
    /// one method and nothing else touches the flag.
    fn record_line_result(&self, failure: Option<String>, dispatch: Dispatch) {
        let errored = match (failure, dispatch) {
            // `:1185`, `:1216`, `:1234`, and the `:1251` set left standing
            // when no function was reached.
            (Some(msg), _) => Some(msg),
            // `:1268`, the one clear.
            (None, Dispatch::Ran) => None,
            // C `continue`s without touching the flag.
            (None, Dispatch::Nothing) => return,
        };
        if let Some(scope) = self.scopes.borrow_mut().last_mut() {
            scope.errored = errored;
        }
    }

    /// The diagnostic of the line that left the innermost scope errored.
    fn pending_error(&self) -> Option<String> {
        self.scopes
            .borrow()
            .last()
            .and_then(|scope| scope.errored.clone())
    }

    /// [`Self::react_to_error`] as a script's line loop takes it: `Ok(Some)`
    /// is C's `ret = -1` with the loop running on (`iocsh.cpp:1132-1141`),
    /// `Ok(None)` leaves `ret` alone, and `Err` is `ret = -1` plus C's
    /// `break`.
    fn react_in_script(&self) -> Result<Option<String>, String> {
        match self.react_to_error() {
            ErrorReaction::Resume => Ok(None),
            ErrorReaction::ResumeFailed => Ok(self.pending_error()),
            ErrorReaction::Stop => Err(self.pending_error().unwrap_or_default()),
        }
    }

    /// React to a failing script line per the innermost scope's `on
    /// error` mode — C `iocsh.cpp:1122-1143`, which runs the check at
    /// the top of the next pass of the line loop.
    fn react_to_error(&self) -> ErrorReaction {
        let scope = match self.current_scope() {
            Some(scope) => scope,
            None => return ErrorReaction::Resume,
        };
        // C guards the whole reaction with `!scope.interactive && scope.errored`.
        if scope.interactive || scope.errored.is_none() {
            return ErrorReaction::Resume;
        }
        match scope.on_error {
            OnError::Continue => ErrorReaction::Resume,
            OnError::Break => {
                self.ctx.eprintln("iocsh Error: Break");
                ErrorReaction::Stop
            }
            OnError::Halt { timeout } if timeout > 0.0 && timeout.is_finite() => {
                self.ctx.eprintln(&format!(
                    "iocsh Error: Waiting {:.1} sec ...",
                    CFloat(timeout)
                ));
                // C `iocsh.cpp:1140` sleeps through `epicsThreadSleep`,
                // so an `on error wait 1e300` continues at once instead
                // of parking the script on a saturated `Duration::MAX`.
                crate::runtime::time::sleep_secs(timeout);
                ErrorReaction::ResumeFailed
            }
            OnError::Halt { .. } => {
                self.ctx.eprintln("iocsh Error: Halt");
                // The shell thread is suspended, not merely blocked: the
                // operator keeps a live IOC whose boot stopped where it
                // broke, and `epicsThreadShowAll` says so where it stopped.
                crate::runtime::task::suspend_self();
                // C `break`s out of the line loop with `ret = -1` once
                // something resumes the thread.
                ErrorReaction::Stop
            }
        }
    }

    /// C `helpCallFunc` (`iocsh.cpp:904-981`).
    ///
    /// C reaches the no-argument form by `argc == 1`, because `help` takes
    /// an `iocshArgArgv` whose `av[0]` is the command name itself; the
    /// port hands this the token list with the name already removed, so
    /// the same test is `arg_tokens.is_empty()`.
    fn execute_help(
        &self,
        arg_tokens: &[IocshString],
        registry: &CommandRegistry,
    ) -> CommandResult {
        let names = registry.list();
        if arg_tokens.is_empty() {
            self.ctx.println(&format_command_columns(&names));
        } else {
            let color = use_ansi_color();
            let mut first = true;
            // C walks the whole table once per argument and does not
            // remember what an earlier pattern already printed, so
            // `help db* dbl` prints `dbl` twice. The order is the table's,
            // which `iocshRegisterImpl` keeps sorted by `strcmp`
            // (`iocsh.cpp:159-166`) — the order `CommandRegistry::list`
            // returns.
            for pattern in arg_tokens {
                for name in &names {
                    if !commands::epics_strn_glob_match(
                        name.as_bytes(),
                        name.len(),
                        pattern.as_bytes(),
                    ) {
                        continue;
                    }
                    let Some(def) = registry.get(name) else {
                        continue;
                    };
                    self.ctx.println(&format_help_entry(def, color, first));
                    first = false;
                }
            }
            // A pattern that matches nothing prints NOTHING: C's loop body
            // is the only thing that writes, so there is no miss message
            // to render.
        }
        Ok(CommandOutcome::Continue)
    }
}

/// C's no-argument `help`: the command names in 16-column tab stops,
/// then a blank line and the two-line trailer (`iocsh.cpp:911-941`).
///
/// Returned as one block rather than written a character at a time as C
/// does, so the layout can be pinned byte-for-byte against a measured
/// `softIoc` run. The bytes are identical either way; the trailing
/// newline is the caller's `println`.
fn format_command_columns(names: &[&str]) -> String {
    /// A name this close to the right margin starts the next line
    /// instead (`iocsh.cpp:918`).
    const WRAP: usize = 79;
    /// Past this column C breaks the line INSTEAD of padding, so a long
    /// name never leaves a stub column behind it (`iocsh.cpp:924`).
    const BREAK: usize = 64;
    const TAB_STOP: usize = 16;

    let mut out = String::new();
    let mut col = 0usize;
    for name in names {
        // C measures with `strlen`; iocsh names are ASCII, so a byte is
        // a column.
        let width = name.len();
        if width + col >= WRAP {
            out.push('\n');
            col = 0;
        }
        out.push_str(name);
        col += width;
        if col >= BREAK {
            out.push('\n');
            col = 0;
        } else {
            // C's `do { ' ' } while (col % 16)` pads at least one space,
            // so two names never touch even when one ends on a tab stop.
            loop {
                out.push(' ');
                col += 1;
                if col % TAB_STOP == 0 {
                    break;
                }
            }
        }
    }
    if col != 0 {
        out.push('\n');
    }
    out.push_str(
        "\nType 'help <glob>' for information about commands matching\n\
         the name or pattern <glob>, e.g. 'help db*'",
    );
    out
}

/// One command's block from C's argument form (`iocsh.cpp:950-972`): the
/// rule between entries, a blank line, the name and its argument names,
/// then a blank line and the usage text.
///
/// `first` suppresses the rule, which C prints only between entries.
fn format_help_entry(def: &CommandDef, color: bool, first: bool) -> String {
    let mut out = String::new();
    if !first {
        // 60 underlined spaces.
        if color {
            out.push_str(ANSI_ESC_UNDERLINE);
        }
        out.push_str(&" ".repeat(60));
        if color {
            out.push_str(ANSI_ESC_RESET);
        }
        out.push('\n');
    }
    out.push('\n');
    if color {
        out.push_str(ANSI_ESC_BOLD);
        out.push_str(&def.name);
        out.push_str(ANSI_ESC_RESET);
    } else {
        out.push_str(&def.name);
    }
    for arg in &def.args {
        // C quotes an argument name that contains a space so the synopsis
        // still reads as one token per argument. An `iocshArgArgv` name is
        // left bare because it is already a phrase (`[command ...]`).
        if matches!(arg.arg_type, ArgType::Argv) || !arg.name.contains(' ') {
            out.push(' ');
            out.push_str(arg.name);
        } else {
            out.push_str(" '");
            out.push_str(arg.name);
            out.push('\'');
        }
    }
    // C's `if (piocshFuncDef->usage)`: a command registered without usage
    // text prints its name and arguments and nothing else.
    if !def.usage.is_empty() {
        out.push('\n');
        out.push('\n');
        // C's usage strings end in a newline and it prints them raw; the
        // port's do not, and the caller's `println` supplies the one
        // newline that ends the block either way.
        out.push_str(def.usage.trim_end_matches('\n'));
    }
    out
}

/// epics-base 8-D `c0da3dd` ANSI color: returns `true` if painted output —
/// the iocsh REPL's, and softIoc's `-v` steps — should emit ANSI color
/// sequences. Honours `NO_COLOR` env var
/// (<https://no-color.org>) and `EPICS_RS_IOCSH_NO_COLOR=1` opt-out;
/// otherwise on by default in the interactive (TTY) path.
///
/// Not host-only: C `showError` (`iocsh.cpp:203-214`) colours its output on
/// every target, so `IocShell::show_error` reads this on the embedded
/// builds too. It touches nothing but `std::env`.
pub fn use_ansi_color() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if let Ok(v) = std::env::var("EPICS_RS_IOCSH_NO_COLOR") {
        let t = v.trim().to_ascii_uppercase();
        if matches!(t.as_str(), "1" | "YES" | "TRUE" | "ON") {
            return false;
        }
    }
    true
}

/// Drop CSI escape sequences (`ESC [ ... final-byte`) from a prompt.
///
/// `IOCSH_PS1` carries them by default (`ANSI_GREEN("epics> ")`), and rustyline
/// needs the *visible* text separately to compute the cursor column.
///
/// Not host-only, for the same reason [`use_ansi_color`] is not: `IOCSH_PS1`
/// is composed by [`iocsh_prompt_if`], which `run_repl_piped` calls, and that
/// is the only REPL an embedded target has. This carried a
/// `#[cfg(not(epics_embedded_target))]` and the comment "used only by the
/// rustyline interactive editor" until `run_repl_piped` started printing the
/// prompt; nothing here touches anything but `char`, so there was never a
/// target reason for the gate.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // CSI: `ESC [` params/intermediates, terminated by 0x40..=0x7e.
        if chars.next() != Some('[') {
            continue;
        }
        for c in chars.by_ref() {
            if ('\x40'..='\x7e').contains(&c) {
                break;
            }
        }
    }
    out
}

/// The exact bytes C `showError` writes (`iocsh.cpp:203-214`), minus the
/// trailing newline: `ERL_ERROR " %s line %d: "` when `filename` is
/// non-NULL, then the message.
///
/// The word and the body are painted, the `%s line %d: ` between them is
/// not, and that asymmetry is not visible at `showError` itself — its own
/// escape closes after `ERROR` (`:209`). The body arrives already wrapped
/// because every one of the twelve `showError` call sites in `iocsh.cpp`
/// spells its format string `ANSI_RED(...)`: `:359`, `:364`, `:369`,
/// `:390`, `:822`, `:828`, `:842`, `:863`, `:876`, `:882`, `:887`,
/// `:1302`. Reading `:209` alone says the body should be plain, and it is
/// wrong — a `softIoc` R7.0.10 A/B over `nosuchcmd`, `iocshRun
/// "alsonosuch"` and `dbDumpVariable notpdbbase` writes the painted body
/// on both sides, all three forms byte-identical.
///
/// Split out from [`IocShell::show_error`] so the rendering can be pinned
/// byte-for-byte against a measured `softIoc` run without capturing the
/// process stderr.
fn format_show_error(source: Option<&SourceFile>, msg: &str, color: bool) -> String {
    let body = paint_error_if(msg, color);
    match source {
        Some(source) => format!(
            "{} {} line {}: {body}",
            if color {
                crate::runtime::log::ERL_ERROR
            } else {
                "ERROR"
            },
            source.base,
            source.lineno
        ),
        None => body,
    }
}

/// A script failure, and whether the operator has been told about it.
///
/// C's `iocshBody` prints every diagnostic where it happens — the open
/// failure at `:1053-1058`, a line's at `:1302-1303` through `showError`,
/// the error reaction's at `:1122-1143` — and returns a bare -1, so its
/// two callers (`<` at `:1233`, `iocshLoadCallFunc` at `:1494`) set a flag
/// and print nothing. This port has one failure C cannot produce, the include
/// depth cap, which nothing has printed; without this distinction a caller
/// must either say every failure twice or swallow that one.
enum ScriptFailure {
    /// Already on the diagnostic stream. The string is a summary for the
    /// caller's own bookkeeping, never something to print.
    Reported(String),
    /// Nobody has printed this yet.
    Unreported(String),
}

/// C `ANSI_RED(...)` (`errlog.h:290`, over `ANSI_ESC_RED` at `:281` and
/// `ANSI_ESC_RESET` at `:289`), under this file's standing `NO_COLOR`
/// deviation.
///
/// C's prompt for a shell reading stdin: `IOCSH_PS1` (`iocsh.cpp:1047-1049`),
/// whose compiled default is `ANSI_GREEN("epics> ")`.
///
/// Returned as `(raw, styled)` because the two readers need different halves
/// of it and neither may compute its own: `raw` is the ANSI-stripped form the
/// line editor measures visible width against, `styled` is the bytes actually
/// written. `color` is [`use_ansi_color`] at the one call that reads the
/// environment, split off here so the composition is testable without it —
/// the same split as [`paint_error`] / [`paint_error_if`].
fn iocsh_prompt() -> (String, String) {
    iocsh_prompt_if(use_ansi_color())
}

fn iocsh_prompt_if(color: bool) -> (String, String) {
    let ps1 = crate::runtime::env_table::IOCSH_PS1
        .get()
        .unwrap_or_default();
    let raw = strip_ansi(&ps1);
    let styled = if color { ps1 } else { raw.clone() };
    (raw, styled)
}

/// No `ERL_*` constant can stand in for this one, because it wraps an
/// arbitrary message rather than a severity word — but the two escapes it
/// wraps it in are the same ones `ERL_ERROR` is assembled from, and are
/// taken from there rather than respelled.
fn paint_error(msg: &str) -> String {
    paint_error_if(msg, use_ansi_color())
}

fn paint_error_if(msg: &str, color: bool) -> String {
    if color {
        format!("{ANSI_ESC_RED}{msg}{ANSI_ESC_RESET}")
    } else {
        msg.to_string()
    }
}

/// C `strerror(errno)`, which is what every open-failure diagnostic in
/// `iocsh.cpp` interpolates. `io::Error`'s Display is the same sentence with
/// ` (os error N)` appended, so the suffix comes off rather than the message
/// being rebuilt from a table this port would then have to keep in step.
pub fn c_strerror(e: &std::io::Error) -> String {
    let text = e.to_string();
    match e.raw_os_error() {
        Some(errno) => text
            .strip_suffix(&format!(" (os error {errno})"))
            .unwrap_or(&text)
            .to_string(),
        None => text,
    }
}

/// Whether a script line should be echoed before execution. Mirrors C
/// iocsh (`iocsh.cpp:1167-1204`): script lines are echoed, except a
/// *silent comment* delimited with `#-` (after leading whitespace), which
/// is suppressed. A plain `#` comment is still echoed; only `#-` is quiet.
/// Both kinds are skipped from execution (`execute_line`), so this gate
/// affects echoing only.
fn echoes_script_line(line: &[u8]) -> bool {
    !line.trim_ascii_start().starts_with(b"#-")
}

/// What a script line is echoed as, or `None` when C prints nothing for it.
///
/// One function for both of C's echo sites, which differ only in the escape:
/// a comment goes out blue (`iocsh.cpp:1175`) and every other line bold
/// (`:1202`). The two suppressions are C's and both belong to every line —
/// `#-` from [`echoes_script_line`], and the empty line from `:1200`'s
/// `*line` test, which a comment can never fail because it holds a `#`.
///
/// C paints unconditionally; the port routes every escape it emits through
/// [`use_ansi_color`], so `NO_COLOR` reaches the echo as it reaches the
/// prompt and `showError`.
fn script_echo(line: &[u8], escape: &str, color: bool) -> Option<Vec<u8>> {
    if line.is_empty() || !echoes_script_line(line) {
        return None;
    }
    Some(if color {
        [escape.as_bytes(), line, ANSI_ESC_RESET.as_bytes()].concat()
    } else {
        line.to_vec()
    })
}

/// C `iocshLoad` (`iocsh.cpp:1341-1345`) records the loaded script in
/// `IOCSH_STARTUP_SCRIPT` — since epics-base#469's fix only when the
/// variable is not already set, so it permanently names the *first*
/// script (or an inherited value from the parent environment) and a
/// nested `iocshLoad` no longer overwrites it. Set before the file is
/// even opened, like C — an unreadable path still lands here.
fn set_startup_script_once(path: &[u8]) {
    if std::env::var_os("IOCSH_STARTUP_SCRIPT").is_none()
        && let Ok(path) = registry::bytes_path(path)
    {
        // SAFETY: same single-threaded-shell rationale as `epicsEnvSet`.
        unsafe { std::env::set_var("IOCSH_STARTUP_SCRIPT", path.as_os_str()) };
    }
}

/// Collapse C iocsh backslash-newline line continuations into logical
/// lines (epics-base PR #603). A physical line ending in `\` joins to
/// the next line: the trailing backslash is stripped, the newline is
/// dropped, and the next physical line's contents (including any
/// leading whitespace) follow immediately. `\` followed by any other
/// character — including a space before the newline — keeps the
/// backslash literal and terminates the logical line normally.
///
/// Returns `(physical_line_number, logical_line)` pairs. The line
/// number is the 1-based index of the *first* physical line in the
/// group, matching where a user would look for an error.
pub(crate) fn join_backslash_continuations(input: &[u8]) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut start_line: Option<usize> = None;
    // `str::lines`' rule over bytes: split at `\n`, drop one `\r` before
    // it, and no empty last line after a final `\n`.
    let lines = (!input.is_empty())
        .then(|| {
            let input = input.strip_suffix(b"\n").unwrap_or(input);
            input.split(|&b| b == b'\n')
        })
        .into_iter()
        .flatten()
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line));
    for (idx, line) in lines.enumerate() {
        let physical_no = idx + 1;
        if start_line.is_none() {
            start_line = Some(physical_no);
        }
        if let Some(stripped) = line.strip_suffix(b"\\") {
            current.extend_from_slice(stripped);
        } else {
            current.extend_from_slice(line);
            out.push((
                start_line.take().unwrap_or(physical_no),
                std::mem::take(&mut current),
            ));
        }
    }
    if !current.is_empty() {
        // A DELIBERATE divergence: C loses this line. `epicsReadline`
        // accumulates into a buffer and, on EOF, does `free (line);
        // return NULL;` without ever looking at how much it had
        // (`epicsReadline.c:87-96`), so a script whose last line has no
        // trailing newline runs every line but that one, silently.
        // Measured on `bin/linux-x86_64/softIoc`
        // (R7.0.10-146-g8f5015b663d764ad75df): a two-line script ending
        // `epicsEnvShow A` with no `\n` echoes and runs only the first
        // line; adding the newline runs both. Matching that would mean
        // dropping an operator's command with no diagnostic anywhere, so
        // the port runs it.
        out.push((start_line.unwrap_or(1), current));
    }
    out
}

#[cfg(test)]
impl IocShell {
    /// `line > f` on this shell: what `f` got, and what reached C's raw
    /// `stdout` (the console cell) meanwhile.
    pub(crate) fn file_and_console(&self, line: &str) -> (String, String) {
        self.fd_file_and_console(1, line)
    }

    /// [`Self::file_and_console`] for `line 2> f` and C's raw `stderr`.
    pub(crate) fn err_file_and_console(&self, line: &str) -> (String, String) {
        self.fd_file_and_console(2, line)
    }

    fn fd_file_and_console(&self, fd: u8, line: &str) -> (String, String) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        let console = dir.path().join("console.txt");
        let sink = std::fs::File::create(&console).unwrap();
        let line = format!("{line} {fd}> {}", file.display());
        let _ = match fd {
            1 => self
                .ctx
                .with_console(sink, std::io::stderr(), || self.execute_line(&line)),
            _ => self
                .ctx
                .with_console(std::io::stdout(), sink, || self.execute_line(&line)),
        };
        let read = |p: &std::path::Path| std::fs::read_to_string(p).unwrap_or_default();
        (read(&file), read(&console))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::records::ai::AiRecord;

    /// One `iocsh(NULL)` per run: once it has opened, a later prompt — a
    /// runner's, queued behind it on `_main_` — returns without reading. Run
    /// over a stdin that never ends, so a read would hang instead of passing.
    #[cfg(unix)]
    #[test]
    fn a_prompt_after_the_main_one_returns_without_reading() {
        let mut fds = [0; 2];
        // SAFETY: `fds` is a two-int out-param; dup2 onto 0 replaces this
        // test process's stdin with the pipe's read end, kept open by `fds[1]`.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { libc::dup2(fds[0], 0) }, 0);

        *MAIN_PROMPT.lock().unwrap() = MainPrompt::Opened;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(make_shell().run_main_prompt()).unwrap());
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(10)),
            Ok(Ok(())),
            "the second prompt read stdin"
        );
    }

    /// `#-` silent comments are not echoed; plain `#` comments and
    /// commands are. Boundary cases: leading whitespace before `#-`, a
    /// space between `#` and `-` (a plain comment, echoed), and the bare
    /// `#-` token. Mirrors C iocsh `iocsh.cpp:1167-1204`.
    #[test]
    fn echoes_script_line_suppresses_only_hash_dash() {
        // Silent comments — not echoed.
        assert!(!echoes_script_line(b"#-"));
        assert!(!echoes_script_line(b"#- a quiet note"));
        assert!(!echoes_script_line(b"#-nospace"));
        assert!(!echoes_script_line(b"   #- leading whitespace"));
        // Plain comments — echoed.
        assert!(echoes_script_line(b"#"));
        assert!(echoes_script_line(b"# normal comment"));
        assert!(echoes_script_line(b"# -space after hash"));
        // Commands and blanks — echoed (unchanged behaviour).
        assert!(echoes_script_line(b"dbLoadRecords(\"x.db\")"));
        assert!(echoes_script_line(b""));
    }

    /// The echo bytes, measured against softIoc R7.0.10 running a script
    /// holding each of these lines: a comment goes out
    /// `ESC[34;1m…ESC[0m`, a command `ESC[1m…ESC[0m`, an empty line
    /// nothing at all, and a whitespace-only line the bold treatment.
    #[test]
    fn script_echo_paints_comments_blue_and_commands_bold() {
        assert_eq!(
            script_echo(b"# plain comment", ANSI_ESC_BLUE, true).as_deref(),
            Some("\x1b[34;1m# plain comment\x1b[0m".as_bytes())
        );
        assert_eq!(
            script_echo(b"   # indented comment", ANSI_ESC_BLUE, true).as_deref(),
            Some("\x1b[34;1m   # indented comment\x1b[0m".as_bytes())
        );
        assert_eq!(
            script_echo(b"dbLoadRecords(\"x.db\")", ANSI_ESC_BOLD, true).as_deref(),
            Some("\x1b[1mdbLoadRecords(\"x.db\")\x1b[0m".as_bytes())
        );
        // Leading whitespace is part of the painted text, not stripped.
        assert_eq!(
            script_echo(b"   dbLoadRecords(\"x.db\")", ANSI_ESC_BOLD, true).as_deref(),
            Some("\x1b[1m   dbLoadRecords(\"x.db\")\x1b[0m".as_bytes())
        );
        // C `:1200`'s `*line`: nothing for an empty line, bold for one that
        // holds only whitespace.
        assert_eq!(script_echo(b"", ANSI_ESC_BOLD, true), None);
        assert_eq!(
            script_echo(b"\t", ANSI_ESC_BOLD, true).as_deref(),
            Some("\x1b[1m\t\x1b[0m".as_bytes())
        );
        // `#-` is silent whichever escape it is offered.
        assert_eq!(script_echo(b"#- quiet", ANSI_ESC_BLUE, true), None);
        assert_eq!(script_echo(b"   #- quiet", ANSI_ESC_BLUE, true), None);
        // NO_COLOR leaves the same text unpainted, and suppresses nothing.
        assert_eq!(
            script_echo(b"# plain comment", ANSI_ESC_BLUE, false).as_deref(),
            Some("# plain comment".as_bytes())
        );
        assert_eq!(script_echo(b"", ANSI_ESC_BOLD, false), None);
    }

    /// Both readers take the same prompt, and it is a prompt whether or not
    /// stdin is a terminal: `epicsReadline.c:75-76` writes it with no
    /// `isatty` in sight, and R7.0.10's own `softIoc` fed two lines from a
    /// pipe answers with these bytes three times on stdout.
    #[test]
    fn the_prompt_is_iocsh_ps1_painted_or_stripped() {
        assert_eq!(
            iocsh_prompt_if(true),
            (
                "epics> ".to_string(),
                "\x1b[32;1mepics> \x1b[0m".to_string()
            )
        );
        assert_eq!(
            iocsh_prompt_if(false),
            ("epics> ".to_string(), "epics> ".to_string())
        );
    }

    /// A path as an unquoted iocsh token. The arg scanner honors C's
    /// out-of-quote backslash escape, which would eat the separators of
    /// a native Windows path — forward slashes survive the scanner and
    /// every Windows API accepts them (what a real Windows st.cmd
    /// writes too).
    fn script_token(p: &std::path::Path) -> String {
        p.display().to_string().replace('\\', "/")
    }

    /// C gates redirect detection on `!quote && !backslash`
    /// (`iocsh.cpp:273`), so a `>` inside EITHER quote — or escaped —
    /// is data. The port saw only the double quote, so a `'`-quoted
    /// argument containing `>` was split into a redirect and the text
    /// after it was passed to `File::create`, which truncates.
    #[test]
    #[serial_test::serial(epics_env)]
    fn a_quoted_or_escaped_gt_is_never_a_redirect() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("st.cmd");
        std::fs::write(&victim, "iocInit\n").unwrap();

        // Single-quoted: C's own help text quotes substitutions this
        // way (`dbIocRegister.c:63-64`).
        shell
            .execute_line("epicsEnvSet(\"EPICS_RS_REDIR\", 'A>B')")
            .expect("a single-quoted `>` is data, not a redirect");
        assert_eq!(std::env::var("EPICS_RS_REDIR").unwrap(), "A>B");
        assert!(
            !std::path::Path::new("B')").exists(),
            "the redirect target was invented from quoted text"
        );

        // The destructive case: the path after the bogus `>` is
        // created with truncation.
        let line = format!("epicsEnvSet(\"EPICS_RS_REDIR2\", 'a>{}')", victim.display());
        shell.execute_line(&line).expect("quoted `>` is data");
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "iocInit\n",
            "a quoted `>` truncated the running startup script"
        );

        // Backslash half: C makes `echo a\>b` print `a>b`.
        let escaped = dir.path().join("escaped_target");
        let line = format!("echo a\\>{}", escaped.display());
        shell.execute_line(&line).expect("an escaped `>` is data");
        assert!(
            !escaped.exists(),
            "an escaped `>` was treated as a redirect"
        );

        // And the malformed line must not create its target either: C
        // reports the unbalanced quote from `split()` before
        // `openRedirect` ever runs.
        let unlinted = dir.path().join("unlinted_target");
        let line = format!("dbl > \"{}", unlinted.display());
        let err = shell
            .execute_line(&line)
            .err()
            .expect("a malformed line must be refused");
        assert_eq!(err, "Unbalanced quote.");
        assert!(
            !std::path::Path::new(&format!("\"{}", unlinted.display())).exists(),
            "a line that fails the lint created its redirect target"
        );

        // SAFETY: single-threaded under the `epics_env` serial group.
        unsafe {
            std::env::remove_var("EPICS_RS_REDIR");
            std::env::remove_var("EPICS_RS_REDIR2");
        }
    }

    /// 09 L-3 — `2>file` actually captures the command's diagnostics, and
    /// leaves stdout alone.
    ///
    /// C `iocsh.cpp:401-428` (`startRedirect`) swaps the thread's stderr for
    /// `case 2` and restores it in `stopRedirect` (`:429-451`). The port used
    /// to print "fd 2 redirect not plumbed" and run the command unredirected,
    /// so `2>/dev/null` suppressed nothing and `2>file` did not even create
    /// the file.
    #[test]
    fn fd2_redirect_captures_diagnostics_and_spares_stdout() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();

        // `epicsEnvSet` with no name diagnoses on the thread's stderr
        // (`libComRegister.c`), so `2>` must capture it. Measured on
        // `softIoc` R7.0.10: `epicsEnvSet 2> f` leaves the sentence in `f`.
        let err = dir.path().join("err.txt");
        shell
            .execute_line(format!("epicsEnvSet 2>{}", script_token(&err)))
            .expect("the redirect itself must not fail the line");
        assert_eq!(
            std::fs::read_to_string(&err).unwrap().trim(),
            "Missing environment variable name argument.",
            "2> must capture the command's stderr"
        );

        // The other half of the same rule: a `2>` must NOT reroute stdout.
        // `dbl` prints its listing on stdout, so the fd-2 file stays empty —
        // routing stdout there would lose the listing entirely.
        let err2 = dir.path().join("err2.txt");
        shell
            .execute_line(format!("dbl 2>{}", script_token(&err2)))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&err2).unwrap(),
            "",
            "2> must leave stdout alone"
        );

        // `>` still captures stdout, unchanged.
        let out = dir.path().join("out.txt");
        shell
            .execute_line(format!("dbl > {}", script_token(&out)))
            .unwrap();
        assert!(
            std::fs::read_to_string(&out).unwrap().contains("TEST_REC"),
            "1> must still capture stdout"
        );
    }

    /// fds 3-9 have no arm in C's `startRedirect` switch, so the file is
    /// opened by `openRedirect` (`iocsh.cpp:378`) and no stream is swapped —
    /// the file is created and stays empty. The port used to skip the open
    /// entirely, so the file never appeared.
    #[test]
    fn high_fd_redirect_creates_the_file_and_captures_nothing() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("fd5.txt");
        shell
            .execute_line(format!("dbl 5>{}", script_token(&f)))
            .unwrap();
        assert!(f.exists(), "C opens the file for every redirected fd");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "");
    }

    pub(super) fn make_shell() -> IocShell {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(PvDatabase::new());
        let bridge = {
            let _guard = rt.enter();
            crate::runtime::task::BlockingBridge::capture()
        };
        rt.block_on(async {
            db.add_record("TEST_REC", Box::new(AiRecord::new(42.0)))
                .await
                .unwrap();
        });
        std::mem::forget(rt);
        IocShell::new(db, bridge)
    }

    fn dump_registrar(shell: &IocShell) -> String {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        shell
            .execute_line(format!(
                "dbDumpRegistrar pdbbase > {}",
                script_token(tmp.path())
            ))
            .unwrap();
        std::fs::read_to_string(tmp.path()).unwrap()
    }

    /// The cross-crate seam, end to end: a name declared through
    /// [`add_registrars`] reaches `dbDumpRegistrar`'s list, which is the only
    /// route by which a registrar implemented outside this crate — C
    /// `softIoc.dbd`'s `rsrvRegistrar`, whose body lives in `epics-ca-rs` —
    /// can be reported at all. The repeat is asserted because the
    /// contributing crate announces from two entry points, either of which a
    /// caller may reach first.
    #[test]
    fn a_registrar_declared_through_the_seam_is_reported_once() {
        let shell = make_shell();
        add_registrars(&["zzSeamProbe".to_string()]);
        let printed = dump_registrar(&shell);
        assert!(
            printed.contains("registrar(zzSeamProbe)"),
            "seam name missing from:\n{printed}"
        );

        add_registrars(&["zzSeamProbe".to_string()]);
        let again = dump_registrar(&shell);
        assert_eq!(
            again.matches("registrar(zzSeamProbe)").count(),
            1,
            "{again}"
        );
    }

    /// C `iocshRegisterCommon` publishes the base version and the target arch
    /// as environment variables, which is what makes `$(EPICS_VERSION_FULL)` in
    /// a `.db`, a `.substitutions` or an `st.cmd` expand. It expanded under C
    /// and passed through verbatim here, because nothing ever set the variable.
    ///
    /// The boundary is the shell's existence: before `IocShell::new` there is no
    /// registration to have run, after it every one of C's names resolves.
    #[test]
    fn a_shell_publishes_the_version_macros_a_db_can_expand() {
        let shell = make_shell();
        assert_eq!(
            shell.expand_line(b"$(EPICS_VERSION_FULL)").unwrap(),
            crate::runtime::version::EPICS_VERSION_FULL.as_bytes(),
        );
        assert_eq!(
            shell.expand_line(b"${ARCH}").unwrap(),
            crate::runtime::build_info::TARGET_ARCH.as_bytes(),
        );
    }

    /// Regression: a CommandDef must be cloneable so the
    /// post-init `afterIocRunning` shell can re-register
    /// site-specific user commands. Pre-fix the handler was
    /// `Box<dyn CommandHandler>` and CommandDef itself was not
    /// Clone — `IocApplication::run` skipped user commands when
    /// spawning the post-init shell, leaving them dead.
    #[test]
    fn command_def_is_clone_and_handler_shared() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let cmd = CommandDef::new(
            "myCmd",
            vec![],
            "myCmd — count invocations",
            move |_args: &[ArgValue], _ctx: &CommandContext| {
                calls_clone.fetch_add(1, Ordering::Relaxed);
                Ok(CommandOutcome::Continue)
            },
        );

        // Cloning the CommandDef shares the same handler counter —
        // the Arc<dyn CommandHandler> is what enables the
        // afterIocRunning re-registration.
        let cmd_dup = cmd.clone();

        let shell = make_shell();
        shell.register(cmd);
        shell.execute_line("myCmd").unwrap();

        let shell2 = make_shell();
        shell2.register(cmd_dup);
        shell2.execute_line("myCmd").unwrap();

        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_execute_line_dbl() {
        let shell = make_shell();
        let result = shell.execute_line("dbl");
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    /// C `iocsh.cpp:1301-1304`: the miss reports itself through
    /// `showError` and touches nothing else. `scope.errored` is already
    /// `true` from `:1251` — set BEFORE the lookup, "error unless a
    /// function is actually called", cleared at `:1268` only once one is
    /// — so the line is failed; `ret` is untouched, so the script's own
    /// status is unaffected. `Ok(CommandOutcome::Failed)` is exactly that
    /// pair, and `Err` was not: it would make the caller print, and each
    /// caller prints differently.
    #[test]
    fn test_execute_line_unknown() {
        let shell = make_shell();
        assert!(matches!(
            shell.execute_line("nonexistent_cmd"),
            Ok(CommandOutcome::Failed)
        ));
    }

    /// The bytes C `showError` writes for a registry miss, both spellings
    /// of `filename`. Measured on `softIoc` R7.0.10-146 with a script at
    /// `<tmp>/sub/d.cmd` whose line 1 is `nosuchcmd` and whose line 2 is
    /// `iocshRun "alsonosuch"`:
    ///
    /// ```text
    /// \x1b[31;1mERROR\x1b[0m d.cmd line 1: \x1b[31;1mCommand 'nosuchcmd' not registered.\x1b[0m
    /// \x1b[31;1mCommand 'alsonosuch' not registered.\x1b[0m
    /// ```
    ///
    /// The first names the BASENAME of an absolute path (C
    /// `iocsh.cpp:1060-1063`); the second has no location at all,
    /// because an `iocshRun` command string reaches `iocshBody` with
    /// `filename` still NULL (`:1078`).
    #[test]
    fn show_error_renders_c_s_two_forms() {
        let source = SourceFile {
            base: "d.cmd".into(),
            lineno: 1,
        };
        let miss = "Command 'nosuchcmd' not registered.";
        assert_eq!(
            format_show_error(Some(&source), miss, true),
            "\x1b[31;1mERROR\x1b[0m d.cmd line 1: \x1b[31;1mCommand 'nosuchcmd' not registered.\x1b[0m"
        );
        assert_eq!(
            format_show_error(None, "Command 'alsonosuch' not registered.", true),
            "\x1b[31;1mCommand 'alsonosuch' not registered.\x1b[0m"
        );
        // `NO_COLOR` is this port's deviation; C has no such gate.
        assert_eq!(
            format_show_error(Some(&source), miss, false),
            "ERROR d.cmd line 1: Command 'nosuchcmd' not registered."
        );
        assert_eq!(format_show_error(None, miss, false), miss);

        // A REJECTED ARGUMENT reaches the same renderer as an unknown
        // command — measured on softIoc R7.0.10, script line 10
        // `dbDumpVariable notpdbbase`. It used to be written by a second,
        // ad-hoc renderer as `s.cmd:10: Error: ...`, which is neither of
        // C's two forms.
        let rejected = SourceFile {
            base: "s.cmd".into(),
            lineno: 10,
        };
        assert_eq!(
            format_show_error(
                Some(&rejected),
                "Expecting 'pdbbase' got 'notpdbbase'.",
                true
            ),
            "\x1b[31;1mERROR\x1b[0m s.cmd line 10: \x1b[31;1mExpecting 'pdbbase' got 'notpdbbase'.\x1b[0m"
        );
    }

    /// C `iocsh.cpp:1060-1063` takes `filename` past the last `/` of the
    /// path it opened, so the diagnostic never carries the caller's
    /// directory. Only `/` — C does not special-case a backslash.
    #[test]
    fn script_basename_is_c_s_strrchr_slash() {
        assert_eq!(script_basename(b"/tmp/x/sub/d.cmd"), b"d.cmd");
        assert_eq!(script_basename(b"d.cmd"), b"d.cmd");
        assert_eq!(script_basename(b"./d.cmd"), b"d.cmd");
        assert_eq!(script_basename(b"a\\b.cmd"), b"a\\b.cmd");
        assert_eq!(script_basename(b"dir/"), b"");
    }

    #[test]
    fn test_execute_line_empty() {
        let shell = make_shell();
        let result = shell.execute_line("");
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    #[test]
    fn test_execute_line_comment() {
        let shell = make_shell();
        let result = shell.execute_line("# this is a comment");
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    #[test]
    fn test_execute_line_missing_required_arg() {
        let shell = make_shell();
        // No command has "required" arguments as far as the shell is
        // concerned: `cvtArg` defaults every absent one and returns
        // success (`iocsh.cpp:809-812`), so a short line always reaches
        // the body. `epicsEnvSet` is one whose body refuses, and it does
        // so C's way — its own sentence on stderr plus
        // `iocshSetError(-1)` (`libComRegister.c:142-146`), which is a
        // failed line carrying no diagnostic for the shell to frame.
        let result = shell.execute_line("epicsEnvSet");
        assert!(
            matches!(result, Ok(CommandOutcome::Failed)),
            "the shell must neither refuse the line nor frame the body's \
             sentence"
        );
    }

    /// C `epicsEnvSet` clears the shell macro of the same name before
    /// setting the variable (`osdEnv.c:49` -> `iocshEnvClear` ->
    /// `macPutValue(handle, name, NULL)`), so a script loaded with
    /// `iocshLoad("inner.cmd","PORT=OLD")` that sets `PORT` itself sees
    /// its own value from then on. Without the clear the installed
    /// macro keeps winning and `$(PORT)` still expands to `OLD`.
    #[test]
    #[serial_test::serial(epics_env)]
    fn epics_env_set_clears_the_shadowing_load_macro() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("inner.cmd");
        std::fs::write(
            &inner,
            "epicsEnvSet(\"EPICS_RS_PORT\",\"NEW\")\n\
             epicsEnvSet(\"EPICS_RS_CHOSEN\",\"$(EPICS_RS_PORT)\")\n",
        )
        .unwrap();

        // SAFETY: single-threaded under the `epics_env` serial group.
        unsafe {
            std::env::remove_var("EPICS_RS_PORT");
            std::env::remove_var("EPICS_RS_CHOSEN");
        }
        shell
            .execute_line(format!(
                "iocshLoad(\"{}\",\"EPICS_RS_PORT=OLD\")",
                inner.display()
            ))
            .expect("iocshLoad must run");

        let chosen = std::env::var("EPICS_RS_CHOSEN").unwrap_or_default();
        // SAFETY: same serial group.
        unsafe {
            std::env::remove_var("EPICS_RS_PORT");
            std::env::remove_var("EPICS_RS_CHOSEN");
        }
        assert_eq!(chosen, "NEW");
    }

    /// `epicsEnvUnset` runs the same clear (`osdEnv.c:60`), so an unset
    /// variable is not still readable through the macro that shadowed
    /// it.
    #[test]
    #[serial_test::serial(epics_env)]
    fn epics_env_unset_clears_the_shadowing_load_macro() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("inner.cmd");
        std::fs::write(
            &inner,
            "epicsEnvUnset(\"EPICS_RS_GONE\")\n\
             epicsEnvSet(\"EPICS_RS_SEEN\",\"$(EPICS_RS_GONE=fallback)\")\n",
        )
        .unwrap();

        // SAFETY: single-threaded under the `epics_env` serial group.
        unsafe {
            std::env::remove_var("EPICS_RS_GONE");
            std::env::remove_var("EPICS_RS_SEEN");
        }
        shell
            .execute_line(format!(
                "iocshLoad(\"{}\",\"EPICS_RS_GONE=OLD\")",
                inner.display()
            ))
            .expect("iocshLoad must run");

        let seen = std::env::var("EPICS_RS_SEEN").unwrap_or_default();
        // SAFETY: same serial group.
        unsafe {
            std::env::remove_var("EPICS_RS_GONE");
            std::env::remove_var("EPICS_RS_SEEN");
        }
        assert_eq!(seen, "fallback");
    }

    #[test]
    fn test_execute_line_help() {
        let shell = make_shell();
        let result = shell.execute_line("help");
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    #[test]
    fn test_execute_line_help_specific() {
        let shell = make_shell();
        let result = shell.execute_line("help dbl");
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    /// The 130 command names a stock C `softIoc` lists, in the order
    /// `iocshRegisterImpl`'s sorted insertion (`iocsh.cpp:159-166`) puts
    /// them in — the input half of a measured `help` oracle.
    const C_HELP_NAMES: &[&str] = &[
        "#",
        "ClockTime_Report",
        "afterIocRunning",
        "asDumpHash",
        "asInit",
        "asSetFilename",
        "asSetSubstitutions",
        "ascar",
        "asdbdump",
        "asphag",
        "aspmem",
        "asprules",
        "aspuag",
        "astac",
        "callbackParallelThreads",
        "callbackQueueShow",
        "callbackSetQueueSize",
        "casr",
        "cd",
        "coreRelease",
        "date",
        "dbCreateAlias",
        "dbCreateRecord",
        "dbDumpBreaktable",
        "dbDumpDevice",
        "dbDumpDriver",
        "dbDumpField",
        "dbDumpFunction",
        "dbDumpLink",
        "dbDumpMenu",
        "dbDumpPath",
        "dbDumpRecord",
        "dbDumpRecordType",
        "dbDumpRegistrar",
        "dbDumpVariable",
        "dbLoadDatabase",
        "dbLoadRecords",
        "dbLoadTemplate",
        "dbLockShowLocked",
        "dbNotifyDump",
        "dbPutAttribute",
        "dbPvdDump",
        "dbPvdTableSize",
        "dbReportDeviceConfig",
        "dbStateClear",
        "dbStateCreate",
        "dbStateSet",
        "dbStateShow",
        "dbStateShowAll",
        "dba",
        "dbap",
        "dbb",
        "dbc",
        "dbcar",
        "dbd",
        "dbel",
        "dbgf",
        "dbglob",
        "dbgrep",
        "dbhcr",
        "dbior",
        "dbjlr",
        "dbl",
        "dbla",
        "dbli",
        "dblsr",
        "dbnr",
        "dbp",
        "dbpf",
        "dbpr",
        "dbs",
        "dbsr",
        "dbstat",
        "dbtgf",
        "dbtpf",
        "dbtpn",
        "dbtr",
        "dlload",
        "echo",
        "eltc",
        "epicsEnvSet",
        "epicsEnvShow",
        "epicsEnvUnset",
        "epicsMutexShowAll",
        "epicsParamShow",
        "epicsPrtEnvParams",
        "epicsThreadResume",
        "epicsThreadShow",
        "epicsThreadShowAll",
        "epicsThreadSleep",
        "errlog",
        "errlogInit",
        "errlogInit2",
        "errlogShow",
        "exit",
        "generalTimeReport",
        "gft",
        "help",
        "installLastResortEventProvider",
        "iocBuild",
        "iocInit",
        "iocLogInit",
        "iocLogPrefix",
        "iocLogShow",
        "iocPause",
        "iocRun",
        "iocshCmd",
        "iocshLoad",
        "iocshRun",
        "on",
        "pft",
        "postEvent",
        "pwd",
        "registerAllRecordDeviceDrivers",
        "registryDeviceSupportFind",
        "registryDriverSupportFind",
        "registryDump",
        "registryFunctionFind",
        "registryRecordTypeFind",
        "scanOnceQueueShow",
        "scanOnceSetQueueSize",
        "scanpel",
        "scanpiol",
        "scanppl",
        "setIocLogDisable",
        "softIoc_registerRecordDeviceDriver",
        "system",
        "taskwdShow",
        "tpn",
        "var",
    ];

    /// What that `softIoc` printed for those names, line for line.
    ///
    /// Four of these lines end in column padding, so every line carries a
    /// closing `|` that the test strips: a whitespace-trimming editor
    /// would otherwise delete the padding this exists to pin.
    const C_HELP_BLOCK: &[&str] = &[
        "#               ClockTime_Report                afterIocRunning asDumpHash|",
        "asInit          asSetFilename   asSetSubstitutions              ascar|",
        "asdbdump        asphag          aspmem          asprules        aspuag|",
        "astac           callbackParallelThreads         callbackQueueShow|",
        "callbackSetQueueSize            casr            cd              coreRelease|",
        "date            dbCreateAlias   dbCreateRecord  dbDumpBreaktable|",
        "dbDumpDevice    dbDumpDriver    dbDumpField     dbDumpFunction  dbDumpLink|",
        "dbDumpMenu      dbDumpPath      dbDumpRecord    dbDumpRecordType|",
        "dbDumpRegistrar dbDumpVariable  dbLoadDatabase  dbLoadRecords   dbLoadTemplate|",
        "dbLockShowLocked                dbNotifyDump    dbPutAttribute  dbPvdDump|",
        "dbPvdTableSize  dbReportDeviceConfig            dbStateClear    dbStateCreate|",
        "dbStateSet      dbStateShow     dbStateShowAll  dba             dbap|",
        "dbb             dbc             dbcar           dbd             dbel|",
        "dbgf            dbglob          dbgrep          dbhcr           dbior|",
        "dbjlr           dbl             dbla            dbli            dblsr|",
        "dbnr            dbp             dbpf            dbpr            dbs|",
        "dbsr            dbstat          dbtgf           dbtpf           dbtpn|",
        "dbtr            dlload          echo            eltc            epicsEnvSet|",
        "epicsEnvShow    epicsEnvUnset   epicsMutexShowAll               epicsParamShow|",
        "epicsPrtEnvParams               epicsThreadResume               |",
        "epicsThreadShow epicsThreadShowAll              epicsThreadSleep|",
        "errlog          errlogInit      errlogInit2     errlogShow      exit|",
        "generalTimeReport               gft             help            |",
        "installLastResortEventProvider  iocBuild        iocInit         iocLogInit|",
        "iocLogPrefix    iocLogShow      iocPause        iocRun          iocshCmd|",
        "iocshLoad       iocshRun        on              pft             postEvent|",
        "pwd             registerAllRecordDeviceDrivers  registryDeviceSupportFind|",
        "registryDriverSupportFind       registryDump    registryFunctionFind|",
        "registryRecordTypeFind          scanOnceQueueShow               |",
        "scanOnceSetQueueSize            scanpel         scanpiol        scanppl|",
        "setIocLogDisable                softIoc_registerRecordDeviceDriver|",
        "system          taskwdShow      tpn             var             |",
    ];

    fn stub_def(name: &str, args: Vec<ArgDesc>, usage: &str) -> CommandDef {
        CommandDef::new(
            name.to_string(),
            args,
            usage.to_string(),
            |_args: &[ArgValue], _ctx: &CommandContext| Ok(CommandOutcome::Continue),
        )
    }

    /// The whole no-argument layout, against a `softIoc` run captured at
    /// R7.0.10. Feeding the C name list must reproduce the C block byte
    /// for byte, which pins the tab stop, both break rules and the
    /// trailer in one assertion.
    #[test]
    fn the_column_layout_reproduces_a_measured_c_help_block() {
        let want: String = C_HELP_BLOCK
            .iter()
            .map(|l| l.trim_end_matches('|'))
            .collect::<Vec<_>>()
            .join("\n");
        let got = format_command_columns(C_HELP_NAMES);
        let (list, trailer) = got.split_at(got.find("\n\nType 'help <glob>'").unwrap());
        assert_eq!(list, want);
        assert_eq!(
            trailer,
            "\n\nType 'help <glob>' for information about commands matching\n\
             the name or pattern <glob>, e.g. 'help db*'"
        );
    }

    /// C pads with a `do`/`while`, so a name that lands exactly on a tab
    /// stop is followed by a WHOLE further stop of spaces rather than
    /// butting against the next name (`iocsh.cpp:929-932`).
    #[test]
    fn a_name_ending_on_a_tab_stop_is_padded_to_the_next_one() {
        let got = format_command_columns(&["0123456789abcdef", "x"]);
        assert_eq!(
            got.lines().next().unwrap(),
            "0123456789abcdef                x               "
        );
    }

    /// Reaching column 64 ends the line with no padding at all
    /// (`iocsh.cpp:924-927`) — 60 characters then a 4-character name.
    #[test]
    fn a_name_reaching_column_64_ends_the_line_unpadded() {
        // 45 characters pads to 48; a 16-character name lands on 64.
        let long = "a".repeat(45);
        let got = format_command_columns(&[&long, "0123456789abcdef", "next"]);
        let first = got.lines().next().unwrap();
        assert_eq!(first.len(), 64);
        assert!(!first.ends_with(' '));
        assert_eq!(got.lines().nth(1).unwrap().trim_end(), "next");
    }

    /// The other break: a name whose END would reach column 79 starts the
    /// next line instead, leaving the padding it was placed after
    /// (`iocsh.cpp:918-921`).
    #[test]
    fn a_name_crossing_column_79_starts_the_next_line() {
        // 5 characters pad to 16; a 64-character name would end at 80.
        let long = "b".repeat(64);
        let got = format_command_columns(&["short", &long]);
        assert_eq!(got.lines().next().unwrap(), "short           ");
        assert_eq!(got.lines().nth(1).unwrap().trim_end(), long);
    }

    /// With nothing registered C's loop writes nothing and `col` is 0, so
    /// the block is the trailer alone — no stray blank line from a final
    /// newline that never got written.
    #[test]
    fn an_empty_command_table_prints_only_the_trailer() {
        assert_eq!(
            format_command_columns(&[]),
            "\nType 'help <glob>' for information about commands matching\n\
             the name or pattern <glob>, e.g. 'help db*'"
        );
    }

    /// C quotes an argument name containing a space so each argument still
    /// reads as one token — except the variadic tail, whose name is
    /// already a phrase (`iocsh.cpp:959-968`).
    #[test]
    fn an_argument_name_with_a_space_is_quoted_unless_it_is_the_variadic_tail() {
        let def = stub_def(
            "dbl",
            vec![
                ArgDesc {
                    name: "record type",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "fields",
                    arg_type: ArgType::String,
                },
            ],
            "Database list.",
        );
        assert_eq!(
            format_help_entry(&def, false, true),
            "\ndbl 'record type' fields\n\nDatabase list."
        );

        let variadic = stub_def(
            "help",
            vec![ArgDesc {
                name: "[command ...]",
                arg_type: ArgType::Argv,
            }],
            "With no arguments, list available command names.",
        );
        assert_eq!(
            format_help_entry(&variadic, false, true),
            "\nhelp [command ...]\n\nWith no arguments, list available command names."
        );
    }

    /// The 60-space rule separates entries, so it precedes every entry but
    /// the first (`iocsh.cpp:950-954`). Colour is C's unconditional
    /// `ANSI_ESC_BOLD`/`ANSI_ESC_UNDERLINE`, which this port routes through the
    /// same `NO_COLOR` opt-out as the rest of the shell.
    #[test]
    fn the_rule_between_entries_precedes_every_entry_but_the_first() {
        let def = stub_def("cd", vec![], "Change directory.");
        assert_eq!(
            format_help_entry(&def, false, true),
            "\ncd\n\nChange directory."
        );
        assert_eq!(
            format_help_entry(&def, false, false),
            format!("{}\n\ncd\n\nChange directory.", " ".repeat(60))
        );
        assert_eq!(
            format_help_entry(&def, true, false),
            format!(
                "\x1b[4m{}\x1b[0m\n\n\x1b[1mcd\x1b[0m\n\nChange directory.",
                " ".repeat(60)
            )
        );
    }

    /// C guards the usage on `piocshFuncDef->usage` being non-NULL, so a
    /// command registered without one prints its synopsis and stops — no
    /// dangling blank line (`iocsh.cpp:970-972`).
    #[test]
    fn a_command_without_usage_text_prints_only_its_synopsis() {
        let def = stub_def("quiet", vec![], "");
        assert_eq!(format_help_entry(&def, false, true), "\nquiet");
    }

    /// End to end: every argument is an `epicsStrGlobMatch` pattern over
    /// the whole table, taken in turn, and a pattern matching nothing
    /// prints nothing at all — C's loop body is the only thing that
    /// writes, so there is no "unknown command" line to emit.
    #[test]
    fn help_globs_every_argument_and_stays_silent_on_a_miss() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();

        let miss = dir.path().join("miss.txt");
        shell
            .execute_line(format!("help nosuchthing > {}", script_token(&miss)))
            .unwrap();
        assert_eq!(std::fs::read_to_string(&miss).unwrap(), "");

        let hit = dir.path().join("hit.txt");
        shell
            .execute_line(format!("help dbPvd* > {}", script_token(&hit)))
            .unwrap();
        let hit = std::fs::read_to_string(&hit).unwrap();
        assert!(hit.contains("dbPvdDump"), "glob must match: {hit}");
        assert!(hit.contains("dbPvdTableSize"), "glob must match all: {hit}");

        // Arguments are taken in order and nothing remembers what an
        // earlier pattern printed, so an overlapping pair repeats an
        // entry — C dedupes nothing.
        let twice = dir.path().join("twice.txt");
        shell
            .execute_line(format!("help dbPvdDump dbPvd* > {}", script_token(&twice)))
            .unwrap();
        let twice = std::fs::read_to_string(&twice).unwrap();
        // Count the synopsis line, which is the only one ending in the
        // argument names — and match on the tail so the test does not
        // depend on whether the name came out bold.
        assert_eq!(
            twice
                .lines()
                .filter(|l| l.ends_with("pdbbase verbose"))
                .count(),
            2,
            "got: {twice}"
        );
    }

    /// C `iocsh.cpp:1233` — `if (iocshBody(...)) scope.errored = true;`. An
    /// include that could not be opened leaves the line errored and hands
    /// back no message, because the inner body has already printed one
    /// (`:1053-1058`). Returning the summary as this line's diagnostic is
    /// what made the port say it twice.
    #[test]
    fn a_failed_include_errors_the_line_without_a_second_message() {
        let shell = make_shell();
        let (result, dispatch) = shell.execute_line_dispatched(b"< nonexistent_file.cmd");
        assert!(
            matches!(result, Ok(CommandOutcome::Failed)),
            "a failed include is the errored flag, not a diagnostic"
        );
        assert!(
            matches!(dispatch, Dispatch::Nothing),
            "`<` reaches no registered function"
        );
    }

    /// C `iocsh.cpp:1053-1058`: `iocshBody` prints the open failure itself,
    /// unframed and in red, then returns -1. Byte-compared against
    /// `softIoc -S /no/such/st.cmd` at R7.0.10, which writes exactly this
    /// line before `softMain`'s `ERROR: Error in <path>`.
    #[test]
    fn an_unopenable_script_prints_cs_open_failure_once() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().join("no-such.cmd");
        let missing = script_token(&missing_path);
        let captured = dir.path().join("captured.err");

        let result = shell
            .ctx
            .with_error(std::fs::File::create(&captured).unwrap(), || {
                shell.execute_script(&missing)
            });
        assert!(result.is_err(), "an unopenable script is a failure");

        // The reason is whatever this platform says about the open the shell
        // just tried, taken from the same failure rather than spelled out:
        // glibc's "No such file or directory" is not what Windows prints for
        // a file that is not there. C's half of the line — the wording, the
        // colour, the single unframed line — is what this compares.
        let reason = c_strerror(&std::fs::File::open(&missing_path).expect_err("no such file"));
        let printed = std::fs::read_to_string(&captured).unwrap();
        assert_eq!(
            printed,
            format!(
                "{}\n",
                paint_error(&format!("Can't open {missing}: {reason}"))
            ),
            "one line, C's wording, C's `strerror` with no Rust suffix"
        );
    }

    /// The same `strerror` text C prints, which `io::Error` renders with a
    /// ` (os error N)` tail C never has.
    #[test]
    fn c_strerror_drops_rusts_errno_suffix() {
        let enoent = std::io::Error::from_raw_os_error(2);
        assert!(enoent.to_string().ends_with(" (os error 2)"));
        assert_eq!(
            c_strerror(&enoent),
            enoent.to_string().trim_end_matches(" (os error 2)")
        );
        // The sentence is the platform's, and only glibc's is the one C
        // prints here: Windows spells this code "The system cannot find the
        // file specified."
        #[cfg(all(unix, target_env = "gnu"))]
        assert_eq!(c_strerror(&enoent), "No such file or directory");

        // Not an OS error, so there is no suffix to drop and the message
        // must survive whole.
        let other = std::io::Error::other("stream did not contain valid UTF-8");
        assert_eq!(c_strerror(&other), "stream did not contain valid UTF-8");
    }

    /// epics-base#499: a self-including script must error out at the
    /// depth cap, not abort the IOC with a stack overflow. C survives
    /// only because each nested include holds its `FILE*` open until
    /// `fopen` fails at the fd limit; this port closes the file before
    /// recursing, so the cap is explicit. Covers both recursion entries
    /// (`<` and `iocshLoad`) and checks the depth ticket unwinds to 0.
    #[test]
    fn self_including_script_errors_at_the_depth_cap() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();

        // Under the default `on error continue` the cap's diagnostic is a
        // per-line error, and C returns 0 from such a script
        // (`iocsh.cpp:1037` + `:1233-1234`, where a failing `<` only sets
        // `scope.errored`). What must hold is that the recursion
        // TERMINATES and the scope stack unwinds.
        let path = dir.path().join("self.cmd");
        std::fs::write(&path, format!("< {}\n", path.display())).unwrap();
        shell
            .execute_script(path.display().to_string())
            .expect("the cap terminates the recursion; continue keeps ret 0");
        assert!(
            shell.scopes.borrow().is_empty(),
            "scope ticket fully released"
        );

        let path2 = dir.path().join("self2.cmd");
        std::fs::write(&path2, format!("iocshLoad {}\n", script_token(&path2))).unwrap();
        shell
            .execute_script(path2.display().to_string())
            .expect("same for the iocshLoad spelling");
        assert!(
            shell.scopes.borrow().is_empty(),
            "scope ticket fully released"
        );

        // And under `on error break`, where C DOES return -1, the failure
        // reaches the caller as the INCLUDE LINE and not as the cap's own
        // sentence: C prints a nested body's diagnostic once, at the level
        // that hit it, and every level above keeps only `scope.errored`
        // (`iocsh.cpp:1233`). Carrying the message up is what used to print
        // it once per frame.
        let path3 = dir.path().join("self3.cmd");
        std::fs::write(&path3, format!("on error break\n< {}\n", path3.display())).unwrap();
        let err = shell
            .execute_script(path3.display().to_string())
            .expect_err("break makes the cap's failure the script's result");
        assert_eq!(err, format!("{}:2", path3.display()), "got: {err}");
        assert!(
            shell.scopes.borrow().is_empty(),
            "scope ticket fully released"
        );
    }

    /// Legitimate nesting under the cap keeps working, and the depth
    /// resets between runs (the guard is a ticket, not a ratchet).
    #[test]
    fn nested_include_under_the_cap_still_runs() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("inner.cmd");
        std::fs::write(&inner, "#- inner\n").unwrap();
        let outer = dir.path().join("outer.cmd");
        std::fs::write(&outer, format!("< {}\n", inner.display())).unwrap();
        let outer_path = outer.display().to_string();
        shell
            .execute_script(&outer_path)
            .expect("two-level include must succeed");
        assert!(shell.scopes.borrow().is_empty());
        // A second run starts from depth 0 again.
        shell
            .execute_script(&outer_path)
            .expect("re-running the same include must succeed");
        assert!(shell.scopes.borrow().is_empty());
    }

    /// epics-base#469 (as fixed upstream in 48eed22f3): the first
    /// loaded script — and only the first — lands in
    /// `IOCSH_STARTUP_SCRIPT`; a nested `iocshLoad` does not overwrite
    /// it, a `<` include (the `iocshBody` path) never sets it, and an
    /// inherited value from the parent environment is kept. One test fn
    /// so the process-global variable has a single owner.
    #[test]
    fn startup_script_env_is_first_load_only() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("inner.cmd");
        std::fs::write(&inner, "#- inner\n").unwrap();
        let outer = dir.path().join("outer.cmd");
        std::fs::write(&outer, format!("iocshLoad {}\n", script_token(&inner))).unwrap();
        let outer_path = outer.display().to_string();

        // SAFETY: single-threaded test process (nextest), sole owner of
        // this variable.
        unsafe { std::env::remove_var("IOCSH_STARTUP_SCRIPT") };
        shell
            .execute_script_with_macros(&outer_path, &HashMap::new())
            .unwrap();
        assert_eq!(
            std::env::var("IOCSH_STARTUP_SCRIPT").as_deref(),
            Ok(outer_path.as_str()),
            "the outer script wins; the nested iocshLoad must not overwrite"
        );

        unsafe { std::env::remove_var("IOCSH_STARTUP_SCRIPT") };
        shell.execute_script(inner.display().to_string()).unwrap();
        assert!(
            std::env::var_os("IOCSH_STARTUP_SCRIPT").is_none(),
            "the iocshBody path ('<' includes) must not set the variable"
        );

        unsafe { std::env::set_var("IOCSH_STARTUP_SCRIPT", "inherited.cmd") };
        shell
            .execute_script_with_macros(&outer_path, &HashMap::new())
            .unwrap();
        assert_eq!(
            std::env::var("IOCSH_STARTUP_SCRIPT").as_deref(),
            Ok("inherited.cmd"),
            "a value inherited from the environment is kept (C getenv guard)"
        );
        unsafe { std::env::remove_var("IOCSH_STARTUP_SCRIPT") };
    }

    #[test]
    fn test_register_custom_command() {
        let shell = make_shell();
        shell.register(CommandDef::new(
            "myCmd",
            vec![],
            "myCmd - custom command",
            |_args: &[ArgValue], _ctx: &CommandContext| Ok(CommandOutcome::Continue),
        ));
        let result = shell.execute_line("myCmd");
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    #[test]
    fn test_redirect_dbl_to_file() {
        let shell = make_shell();
        let tmpdir = tempfile::tempdir().expect("fixture root");
        let tmp = tmpdir.path().join("iocsh_test_dbl_redirect.txt");
        let line = format!("dbl > {}", tmp.display());
        let result = shell.execute_line(&line);
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
        let content = std::fs::read_to_string(&tmp).unwrap();
        assert!(
            content.contains("TEST_REC"),
            "dbl output should contain TEST_REC, got: {content}"
        );
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_redirect_append() {
        let shell = make_shell();
        let tmpdir = tempfile::tempdir().expect("fixture root");
        let tmp = tmpdir.path().join("iocsh_test_append.txt");
        std::fs::write(&tmp, "existing\n").unwrap();
        let line = format!("dbl >> {}", tmp.display());
        let result = shell.execute_line(&line);
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
        let content = std::fs::read_to_string(&tmp).unwrap();
        assert!(content.starts_with("existing\n"));
        assert!(content.contains("TEST_REC"));
        std::fs::remove_file(&tmp).ok();
    }

    /// C `epicsStdio.h` makes each `printf` the thread's stdout, so `>`
    /// reaches a print that has no `CommandContext`, in order with the
    /// command's own lines; another thread's print stays on the console.
    #[test]
    fn a_redirect_reaches_a_print_with_no_context() {
        let shell = make_shell();
        shell.register(CommandDef::new(
            "deepPrint",
            vec![],
            "deepPrint",
            |_args: &[ArgValue], ctx: &CommandContext| {
                ctx.println("one");
                crate::epics_println!("two");
                std::thread::spawn(|| crate::epics_println!("elsewhere"))
                    .join()
                    .unwrap();
                ctx.println("three");
                Ok(CommandOutcome::Continue)
            },
        ));
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.txt");
        shell
            .execute_line(format!("deepPrint > {}", script_token(&out)))
            .unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "one\ntwo\nthree\n");
    }

    /// A `2>` (fd≠1) redirect must NOT capture the command's stdout. C
    /// `iocsh.cpp:401-428` (startRedirect) reroutes only the named
    /// stream — `case 1` stdout, `case 2` stderr — so `dbl 2>FILE` keeps
    /// its listing (stdout) on the terminal and only stderr (unplumbed
    /// here) would go to FILE. The previous code replaced the stdout sink
    /// for any fd, diverting the listing into FILE and losing it.
    #[test]
    fn test_redirect_fd2_leaves_stdout_intact() {
        let shell = make_shell();
        let tmpdir = tempfile::tempdir().expect("fixture root");
        let tmp = tmpdir.path().join("iocsh_test_fd2_redirect.txt");
        let line = format!("dbl 2> {}", tmp.display());
        let result = shell.execute_line(&line);
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
        // The dbl listing is stdout; a 2> redirect must not divert it
        // into the file (the fd-2 stream is not plumbed, so the file is
        // not even written).
        let captured = std::fs::read_to_string(&tmp).unwrap_or_default();
        assert!(
            !captured.contains("TEST_REC"),
            "fd-2 redirect must not capture stdout, got: {captured}"
        );
        std::fs::remove_file(&tmp).ok();
    }

    type SplitParts = (Vec<String>, Vec<(u8, String, bool)>);

    /// `line` through [`registry::split`]: the words, and each fd's
    /// `(fd, path, append)`.
    fn split_parts(line: &str) -> Result<SplitParts, &'static str> {
        let split = registry::split(line.as_bytes())?;
        Ok((
            split.argv.iter().map(|w| w.to_string()).collect(),
            split
                .redirects
                .iter()
                .map(|(&fd, r)| (fd, String::from_utf8_lossy(&r.path).into_owned(), r.append))
                .collect(),
        ))
    }

    /// C `split` (`iocsh.cpp:255-371`). The cases are softIoc R7.0.10's
    /// files for the same lines.
    #[test]
    fn split_lifts_every_redirect_wherever_it_sits() {
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let r = |fd, p: &str, a| (fd, p.to_string(), a);
        for (line, argv, redirects) in [
            (
                "dbl > /tmp/out.txt",
                words(&["dbl"]),
                vec![r(1, "/tmp/out.txt", false)],
            ),
            (
                "dbl >> /tmp/out.txt",
                words(&["dbl"]),
                vec![r(1, "/tmp/out.txt", true)],
            ),
            ("dbl", words(&["dbl"]), vec![]),
            (
                "dbl 2>>/tmp/err.txt",
                words(&["dbl"]),
                vec![r(2, "/tmp/err.txt", true)],
            ),
            (
                "myCmd < /tmp/in.txt",
                words(&["myCmd"]),
                vec![r(0, "/tmp/in.txt", false)],
            ),
            (
                "echo hi > a 2> b",
                words(&["echo", "hi"]),
                vec![r(1, "a", false), r(2, "b", false)],
            ),
            (
                "> f echo hi",
                words(&["echo", "hi"]),
                vec![r(1, "f", false)],
            ),
            (
                "echo > f hi",
                words(&["echo", "hi"]),
                vec![r(1, "f", false)],
            ),
            (
                "echo hi > \"p 5.txt\"",
                words(&["echo", "hi"]),
                vec![r(1, "p 5.txt", false)],
            ),
            ("> f exit", words(&["exit"]), vec![r(1, "f", false)]),
            (
                "< inc.cmd > out",
                words(&[]),
                vec![r(0, "inc.cmd", false), r(1, "out", false)],
            ),
            // Any 1-9 directly before `>` names the fd, inside a word too.
            ("echo a1>x", words(&["echo", "a"]), vec![r(1, "x", false)]),
            ("cmd5>file", words(&["cmd"]), vec![r(5, "file", false)]),
            (
                "echo hi 12>x",
                words(&["echo", "hi", "1"]),
                vec![r(2, "x", false)],
            ),
            ("foo 0>x", words(&["foo", "0"]), vec![r(1, "x", false)]),
            // A second name for one fd ends the scan and keeps the first.
            (
                "echo hi > a > b",
                words(&["echo", "hi"]),
                vec![r(1, "a", false)],
            ),
            // Quoted or escaped, an operator is data.
            (
                "epicsEnvSet(\"X\", 'a<b')",
                words(&["epicsEnvSet", "X", "a<b"]),
                vec![],
            ),
            ("echo a\\>b", words(&["echo", "a>b"]), vec![]),
        ] {
            assert_eq!(split_parts(line), Ok((argv, redirects)), "{line}");
        }
        for (line, diag) in [
            ("echo hi >", "Invalid redirection."),
            ("echo b ><x", "Invalid redirection."),
            ("echo > \"abc", "Unbalanced quote."),
            ("echo hi \\", "Trailing backslash."),
        ] {
            assert_eq!(split_parts(line), Err(diag), "{line}");
        }
    }

    /// epics-base PR #812 — `dbCreateRecord pdbbase <type> <name>` creates a
    /// new record at runtime through the same factory registry as
    /// `dbLoadRecords`. Verifies the happy path plus three rejection
    /// branches (duplicate name, bad name, unknown record type), which
    /// return Err so `on error` sees them — current C base wraps these
    /// in `iocshSetError` (epics-base#498 / UI-105).
    #[test]
    fn test_execute_line_db_create_record_happy_path() {
        let shell = make_shell();
        let result = shell.execute_line("dbCreateRecord pdbbase ai NEW:AI");
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
        // Record is now visible via dbl.
        let result = shell.execute_line("dbl ai");
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    #[test]
    fn test_execute_line_db_create_record_rejects_duplicate() {
        let shell = make_shell();
        // TEST_REC was added by make_shell() — re-creating must fail.
        let r = shell.execute_line("dbCreateRecord pdbbase ai TEST_REC");
        assert!(
            matches!(r, Ok(CommandOutcome::Failed)),
            "duplicate name must fail"
        );
        // After the rejected call, the original record (val=42.0) is
        // still there, not overwritten. Verify with dbpr, which reads
        // the live record; `dbgf` cannot be the probe here because C
        // refuses it before `iocInit` (`dbTest.c:366-368`) and this
        // shell has not run one, while `dbpr` is deliberately ungated.
        let r = shell.execute_line("dbpr TEST_REC");
        assert!(matches!(r, Ok(CommandOutcome::Continue)));
    }

    #[test]
    fn test_execute_line_db_create_record_rejects_bad_name() {
        let shell = make_shell();
        // Space inside the name → validate_record_name returns Err.
        // Quote so the parser keeps the space as one argument.
        let r = shell.execute_line("dbCreateRecord pdbbase ai \"BAD NAME\"");
        assert!(
            matches!(r, Ok(CommandOutcome::Failed)),
            "bad name must fail"
        );
    }

    #[test]
    fn test_execute_line_db_create_record_rejects_unknown_type() {
        let shell = make_shell();
        let r = shell.execute_line("dbCreateRecord pdbbase nonexistent NEW_REC");
        assert!(
            matches!(r, Ok(CommandOutcome::Failed)),
            "unknown record type must fail"
        );
    }

    /// UI-105 / epics-base#498 — a failing db* command must trip
    /// `on error break`, not just print. The failure here is a real
    /// command failure (unknown record type), not an unknown command.
    #[test]
    fn on_error_break_stops_at_a_failed_db_command() {
        let shell = make_shell();
        let dir = std::env::temp_dir().join(format!("iocsh_ui105_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("st.cmd");
        std::fs::write(
            &path,
            "on error break\ndbCreateRecord pdbbase nonexistent X\ndbCreateRecord pdbbase ai SHOULD_NOT_EXIST\n",
        )
        .unwrap();
        let result = shell.execute_script(path.to_str().unwrap());
        assert!(result.is_err(), "script must surface the db failure");
        assert!(
            shell.ctx.db().get_record("SHOULD_NOT_EXIST").is_none(),
            "on error break must stop before the next line creates the record"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PR #603 — line ending in `\` joins to the next physical line.
    /// Mirrors the 8 scenarios in epics-base
    /// `modules/libcom/test/multiline-input.txt`. Uses `concat!` (no
    /// Rust source-line continuation) so trailing whitespace before
    /// `\\\n` and leading whitespace on continuation chunks is
    /// preserved verbatim — `String::lines()` then sees the exact
    /// physical line layout from the upstream test file.
    #[test]
    fn test_backslash_continuation_scenarios() {
        let input = concat!(
            "1 not a multiline string\n",
            "2 first multiline \\\n",
            "string\n",
            "3 second multiline \\\n",
            "string \\\n",
            "with more lines\n",
            "4 several lines .. \\\n",
            "next line is empty: \\\n",
            "\\\n",
            "next has only a space:\\\n",
            " \\\n",
            "next line has 3 spaces:\\\n",
            "   \\\n",
            "END\n",
            "5 it is fine to sp\\\n",
            "it words, or really \\\n",
            "c\\\n",
            "h\\\n",
            "o\\\n",
            "p\\\n",
            " them up!\n",
            "\\\n",
            "6 start with backslash , fine with me but why?\n",
            "7 have a trailing space after backslash \\ \n",
            "8 not part of the string no. 7\n",
        );
        let lines: Vec<String> = join_backslash_continuations(input.as_bytes())
            .into_iter()
            .map(|(_, l)| String::from_utf8(l).unwrap())
            .collect();
        assert_eq!(lines[0], "1 not a multiline string");
        assert_eq!(lines[1], "2 first multiline string");
        assert_eq!(lines[2], "3 second multiline string with more lines");
        assert_eq!(
            lines[3],
            "4 several lines .. next line is empty: next has only a space: next line has 3 spaces:   END"
        );
        assert_eq!(
            lines[4],
            "5 it is fine to spit words, or really chop them up!"
        );
        assert_eq!(lines[5], "6 start with backslash , fine with me but why?");
        assert_eq!(lines[6], "7 have a trailing space after backslash \\ ");
        assert_eq!(lines[7], "8 not part of the string no. 7");
        assert_eq!(lines.len(), 8);
    }

    /// Logical line numbers reported by `join_backslash_continuations`
    /// point at the *first* physical line of the joined group — this
    /// is what the user reads in the script when debugging.
    #[test]
    fn test_backslash_continuation_line_numbers() {
        let input = "a\nb \\\nc\nd\n";
        let out = join_backslash_continuations(input.as_bytes());
        assert_eq!(
            out,
            vec![(1, "a".into()), (2, "b c".into()), (4, "d".into())]
        );
    }

    /// EOF without trailing newline: emit the partial as a final line
    /// (matches `String::lines()` semantics).
    #[test]
    fn test_backslash_continuation_no_trailing_newline() {
        let out = join_backslash_continuations(b"partial");
        assert_eq!(out, vec![(1, "partial".into())]);
    }

    /// CRLF input must yield the same logical lines as LF (Rust's
    /// `str::lines()` strips the CR for us).
    #[test]
    fn test_backslash_continuation_crlf() {
        let out = join_backslash_continuations(b"a \\\r\nb\r\n");
        assert_eq!(out, vec![(1, "a b".into())]);
    }

    /// End-to-end: a backslash-continued script line tokenizes and
    /// runs as one logical command.
    #[test]
    fn test_iocsh_script_backslash_continuation_end_to_end() {
        let shell = make_shell();
        let tmpdir = tempfile::tempdir().expect("fixture root");
        let tmp = tmpdir.path().join("iocsh_multiline.cmd");
        // dbgf TEST_REC.VAL — but split across two physical lines.
        std::fs::write(&tmp, "dbgf \\\nTEST_REC.VAL\n").unwrap();
        let result = shell.execute_script(tmp.to_str().unwrap());
        std::fs::remove_file(&tmp).ok();
        assert!(result.is_ok(), "joined `dbgf TEST_REC.VAL` must succeed");
    }

    /// Issue #847 — `iocshLoad <path> [macros]` reads a script and
    /// substitutes `$(KEY)` / `${KEY}` per-call macros before
    /// dispatching each line through the standard `execute_line`
    /// pipeline. Verifies the happy path: a macro-parameterised
    /// command is recognised after substitution.
    #[test]
    fn test_iocsh_load_macro_substitutes_command_name() {
        let shell = make_shell();
        let tmpdir = tempfile::tempdir().expect("fixture root");
        let tmp = tmpdir.path().join("iocsh_load_macro_cmd.cmd");
        std::fs::write(&tmp, "$(CMD)\n").unwrap();
        let line = format!("iocshLoad {} CMD=dbl", script_token(&tmp));
        let result = shell.execute_line(&line);
        std::fs::remove_file(&tmp).ok();
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    /// Without macros, `iocshLoad` behaves like `<` (no substitution).
    #[test]
    fn test_iocsh_load_no_macros() {
        let shell = make_shell();
        let tmpdir = tempfile::tempdir().expect("fixture root");
        let tmp = tmpdir.path().join("iocsh_load_no_macros.cmd");
        std::fs::write(&tmp, "dbl\n").unwrap();
        let line = format!("iocshLoad {}", script_token(&tmp));
        let result = shell.execute_line(&line);
        std::fs::remove_file(&tmp).ok();
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    /// I-R3-1: C runs ONE `macDefExpand` per line over ONE handle that
    /// carries both the environment (`iocsh.cpp:1033` `pairs[]` →
    /// `macCore.c:131-133` `FLAG_USE_ENVIRONMENT`) and the `iocshLoad`
    /// macros pushed onto it (`iocsh.cpp:1112-1113`). The port used two
    /// engines: an env-less `substitute_macros` that ran only when the
    /// macro map was non-empty, then an env-only second pass — so
    /// passing any macro to `iocshLoad` rewrote `$(TOP)` to
    /// `$(TOP,undefined)` and the env pass could no longer see it.
    ///
    /// Boundaries: {macros empty, non-empty} x {env ref present,
    /// absent} x {`iocshLoad`, `<` include}.
    #[test]
    #[serial_test::serial(epics_env)]
    fn one_pass_resolves_env_and_iocsh_load_macros_together() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: serial(epics_env) serialises env-mutating tests.
        unsafe { std::env::set_var("R3_TOP", "/opt/myioc") };

        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();

        // `<` include reached from INSIDE the load: both its filename
        // and its body must see the pushed scope (C `iocsh.cpp:1233`
        // re-enters `iocshBody` on the same handle).
        std::fs::write(
            sub.join("inner.cmd"),
            "epicsEnvSet(\"R3_INNER\", \"$(R3_TOP)/db/$(PORT).db\")\n",
        )
        .unwrap();

        let loaded = dir.path().join("loaded.cmd");
        std::fs::write(
            &loaded,
            format!(
                "epicsEnvSet(\"R3_BOTH\", \"$(R3_TOP)/db/$(PORT).db\")\n\
                 epicsEnvSet(\"R3_MACRO_ONLY\", \"$(PORT)\")\n\
                 < {}/$(SUBDIR)/inner.cmd\n",
                script_token(dir.path())
            ),
        )
        .unwrap();

        // macros non-empty x env ref present x iocshLoad (and, through
        // the third line, x `<` include).
        // The macro list has to be quoted: C's `split()` separates on
        // `,` as readily as on a blank (`iocsh.cpp:271`), so the bare
        // form hands `iocshLoad` only `PORT=L0` and the rest becomes a
        // further argument it never reads. Measured — `iocshLoad
        // inner2.cmd PORT=L0,SUBDIR=sub` leaves `$(SUBDIR)` undefined
        // inside the loaded script, while the quoted form defines it.
        shell
            .execute_line(format!(
                "iocshLoad {} \"PORT=L0,SUBDIR=sub\"",
                script_token(&loaded)
            ))
            .expect("iocshLoad with macros must not break env references");
        assert_eq!(std::env::var("R3_BOTH").unwrap(), "/opt/myioc/db/L0.db");
        assert_eq!(std::env::var("R3_MACRO_ONLY").unwrap(), "L0");
        assert_eq!(std::env::var("R3_INNER").unwrap(), "/opt/myioc/db/L0.db");

        // macros empty x env ref present x iocshLoad.
        let env_only = dir.path().join("env_only.cmd");
        std::fs::write(
            &env_only,
            "epicsEnvSet(\"R3_ENV_ONLY\", \"$(R3_TOP)/db\")\n",
        )
        .unwrap();
        shell
            .execute_line(format!("iocshLoad {}", script_token(&env_only)))
            .unwrap();
        assert_eq!(std::env::var("R3_ENV_ONLY").unwrap(), "/opt/myioc/db");

        // macros empty x env ref present x `<` include at top level.
        shell
            .execute_line(format!("< {}", script_token(&env_only)))
            .unwrap();
        assert_eq!(std::env::var("R3_ENV_ONLY").unwrap(), "/opt/myioc/db");

        // The pushed scope is popped when the load returns, so `$(PORT)`
        // is undefined again and the line is refused — `Failed`, with no
        // message of its own, macLib having raised the only one C raises
        // (see [`IocShell::expand_line`]).
        let after = dir.path().join("after.cmd");
        std::fs::write(&after, "epicsEnvSet(\"R3_AFTER\", \"$(PORT)\")\n").unwrap();
        assert!(
            matches!(
                shell.execute_line("epicsEnvSet(\"R3_AFTER\", \"$(PORT)\")"),
                Ok(CommandOutcome::Failed)
            ),
            "PORT must not survive the iocshLoad scope"
        );
        assert!(
            std::env::var("R3_AFTER").is_err(),
            "the refused line must install nothing"
        );
        // The same line inside a script is skipped, not fatal: C's
        // `iocsh.cpp:1184-1187` marks the scope errored and moves on, and
        // `ret` stays 0 under the default `continue`.
        shell
            .execute_script(after.display().to_string())
            .expect("a skipped line leaves C's ret at 0");
        assert!(std::env::var("R3_AFTER").is_err());

        unsafe {
            std::env::remove_var("R3_TOP");
            std::env::remove_var("R3_BOTH");
            std::env::remove_var("R3_MACRO_ONLY");
            std::env::remove_var("R3_INNER");
            std::env::remove_var("R3_ENV_ONLY");
        }
    }

    /// I-R3-2: an undefined macro makes `macDefExpand` return NULL
    /// (`macCore.c:895-896` + `:220`, `macEnv.c:59-61`) and
    /// `iocsh.cpp:1184-1187` skips the line entirely. The port passed
    /// `$(P)` through as literal text and ran the command, installing a
    /// four-character literal as the IOC prefix.
    ///
    /// The refusal carries no message of its own: macLib's `errlogPrintf`
    /// is C's only line here, and the port raises it inside the expander
    /// — see [`IocShell::expand_line`]. So the outcome is `Failed`, which
    /// is what `scope.errored` and `on error` read, and the console keeps
    /// exactly one sentence.
    ///
    /// The `.db` reader keeps C's opposite choice (`dbLexRoutines.c:381-386`
    /// downgrades the same condition to a warning) and is unaffected.
    #[test]
    #[serial_test::serial(epics_env)]
    fn an_undefined_macro_refuses_the_iocsh_line() {
        let shell = make_shell();
        // SAFETY: serial(epics_env) serialises env-mutating tests.
        unsafe { std::env::remove_var("R3_UNSET") };
        unsafe { std::env::remove_var("R3_PREFIX") };

        assert!(
            matches!(
                shell.execute_line("epicsEnvSet(\"R3_PREFIX\", \"$(R3_UNSET)\")"),
                Ok(CommandOutcome::Failed)
            ),
            "an undefined macro must refuse the line, and say so without a \
             second copy of macLib's sentence"
        );
        assert!(
            std::env::var("R3_PREFIX").is_err(),
            "the refused line must install nothing"
        );

        // A default keeps the line runnable, as C macLib does.
        shell
            .execute_line("epicsEnvSet(\"R3_PREFIX\", \"$(R3_UNSET=fallback)\")")
            .unwrap();
        assert_eq!(std::env::var("R3_PREFIX").unwrap(), "fallback");

        unsafe { std::env::remove_var("R3_PREFIX") };
    }

    /// An escaped argument survives the shell's one expansion and its split
    /// byte for byte, whichever structural byte it holds.
    #[test]
    #[serial_test::serial(epics_env)]
    fn an_escaped_argument_reaches_the_command_unchanged() {
        let shell = make_shell();
        // SAFETY: serial(epics_env) serialises env-mutating tests.
        unsafe { std::env::set_var("R3_ESCAPED", "expanded") };
        for arg in [
            r#"C:\Users\RUNNER~1\j.db"#,
            r#"a b\t"c" 'd' $(R3_ESCAPED) ${R3_ESCAPED},(e)<f>2>g"#,
            "P=A:,R=B é\t\\",
            "",
            "\\",
        ] {
            let line = format!("dbLoadRecords({}, x)", registry::escape_arg(arg));
            let expanded = shell.expand_line(line.as_bytes()).unwrap();
            assert_eq!(
                registry::tokenize(&expanded),
                vec!["dbLoadRecords", arg, "x"],
                "{line}"
            );
            assert_eq!(registry::lint_line(&expanded), None, "{line}");
        }
        unsafe { std::env::remove_var("R3_ESCAPED") };
    }

    /// A macro value carrying token separators is re-tokenized because
    /// the single expansion runs over the WHOLE line before `split`
    /// (C `iocsh.cpp:1184` then `:1215`). Guards against re-introducing
    /// a per-token expander.
    #[test]
    #[serial_test::serial(epics_env)]
    fn a_macro_value_with_separators_is_split_into_words() {
        let shell = make_shell();
        // SAFETY: serial(epics_env) serialises env-mutating tests.
        unsafe { std::env::set_var("R3_MULTIWORD", "dbpr TEST_REC 2") };
        let expanded = shell.expand_line(b"$(R3_MULTIWORD) EXTRA").unwrap();
        assert_eq!(
            registry::tokenize(&expanded),
            vec!["dbpr", "TEST_REC", "2", "EXTRA"]
        );
        unsafe { std::env::remove_var("R3_MULTIWORD") };
    }

    /// A bare `iocshLoad` is not an arity error. C's `iocshLoad` skips the
    /// `IOCSH_STARTUP_SCRIPT` record for a NULL pathname and calls
    /// `iocshBody(NULL, NULL, macros)` (`iocsh.cpp:1346-1352`) — the stdin
    /// REPL — so the line succeeds and the enclosing script runs on.
    /// Measured against `softIoc` R7.0.10 with `</dev/null`: the nested shell
    /// reads EOF at once, returns 0, and the next line still executes. The
    /// test harness gives this process the same EOF-on-read stdin.
    #[test]
    fn a_bare_iocsh_load_runs_the_nested_shell_rather_than_refusing_the_line() {
        let shell = make_shell();
        assert!(matches!(
            shell.execute_line("iocshLoad"),
            Ok(CommandOutcome::Continue)
        ));
    }

    /// epics-base 144f975 — `dbLoadRecords` rejection (e.g., duplicate
    /// name) must propagate an `Err` back to the iocsh script chain
    /// (the Rust equivalent of `iocshSetError`). Pre-fix the command
    /// printed the error and returned `Ok(Continue)`, so a startup
    /// script silently succeeded. Verifies `execute_script` surfaces
    /// C `dbLexRoutines.c:1173-1180` parity: dbLoadRecords with a
    /// duplicate record name of a DIFFERENT record_type must propagate
    /// Err. Same-name + same-type merges (covered by
    /// `commands::tests::test_db_load_records_same_type_duplicate_merges_fields`).
    #[test]
    fn test_db_load_records_different_type_duplicate_propagates() {
        let shell = make_shell();
        // make_shell already added TEST_REC as an `ai`. Loading a .db
        // that redefines it as `mbbo` must hit the type-mismatch
        // branch and surface Err to the script chain.
        let tmpdir = tempfile::tempdir().expect("fixture root");
        let db_path = tmpdir.path().join("iocsh_dup_load.db");
        std::fs::write(&db_path, "record(mbbo, \"TEST_REC\") {}\n").unwrap();
        let script_path = tmpdir.path().join("iocsh_dup_load.cmd");
        std::fs::write(
            &script_path,
            format!("dbLoadRecords {}\n", script_token(&db_path)),
        )
        .unwrap();
        // The COMMAND must fail — that is the branch under test. The
        // script's own return value is C's `ret`, which stays 0 under the
        // default `on error continue` (`iocsh.cpp:1037`), so the two are
        // asserted separately.
        let line_result =
            shell.execute_line_reported(format!("dbLoadRecords {}", script_token(&db_path)));
        let script_result = shell.execute_script(script_path.to_str().unwrap());
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&script_path);
        assert!(
            line_result.is_err(),
            "dbLoadRecords with type-mismatched duplicate must fail the line"
        );
        assert!(
            script_result.is_ok(),
            "a failed line under `on error continue` leaves C's ret at 0"
        );
    }

    /// C++-style call `iocshLoad("path", "K=V,...")` must tokenize to
    /// the same args as the space form — quotes around the macro
    /// string protect the comma so it stays one token.
    #[test]
    fn test_iocsh_load_cpp_paren_syntax() {
        let shell = make_shell();
        let tmpdir = tempfile::tempdir().expect("fixture root");
        let tmp = tmpdir.path().join("iocsh_load_paren.cmd");
        std::fs::write(&tmp, "$(CMD)\n").unwrap();
        let line = format!("iocshLoad(\"{}\", \"CMD=dbl\")", tmp.display());
        let result = shell.execute_line(&line);
        std::fs::remove_file(&tmp).ok();
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    /// Per-line errors during an `iocshLoad` do not abort the rest of the
    /// loaded script, and under the default `on error continue` they do
    /// not fail the load either: C's `iocshLoadCallFunc` is
    /// `iocshSetError(iocshLoad(...))` (`iocsh.cpp:1494`) over an
    /// `iocshBody` whose `ret` is still 0 (`:1037`). With `on error break`
    /// inside the loaded script the same load DOES fail.
    #[test]
    fn test_iocsh_load_per_line_errors_continue_and_only_break_propagates() {
        let shell = make_shell();
        let tmpdir = tempfile::tempdir().expect("fixture root");
        let tmp = tmpdir.path().join("iocsh_load_err.cmd");
        std::fs::write(&tmp, "nonexistent_cmd\ndbl\n").unwrap();
        let result = shell.execute_line(format!("iocshLoad {}", script_token(&tmp)));
        std::fs::remove_file(&tmp).ok();
        assert!(
            result.is_ok(),
            "a failing line under `continue` leaves iocshLoad's status 0"
        );

        let brk = tmpdir.path().join("iocsh_load_err_break.cmd");
        std::fs::write(&brk, "on error break\nnonexistent_cmd\ndbl\n").unwrap();
        let result = shell.execute_line(format!("iocshLoad {}", script_token(&brk)));
        std::fs::remove_file(&brk).ok();
        // `iocshSetError(iocshLoad(...))` sets the flag and prints nothing —
        // the loaded body already showed the failing line and the `Break`
        // reaction — so the failure surfaces as the line's outcome, not as
        // a second message.
        assert!(
            matches!(result, Ok(CommandOutcome::Failed)),
            "`on error break` is what makes iocshLoad fail the line"
        );
    }

    /// The discriminator for the double-report: an unopenable include has
    /// already printed its own `Can't open` line, so the outer script's
    /// failure names the include LINE and does not repeat the inner
    /// summary. C `iocsh.cpp:1233` — `if (iocshBody(...)) scope.errored =
    /// true;`, no print.
    #[test]
    fn a_failed_include_does_not_repeat_the_inner_summary() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let missing = script_token(&dir.path().join("no-such-include.cmd"));
        let outer = dir.path().join("outer.cmd");
        std::fs::write(&outer, format!("on error break\n< {missing}\n")).unwrap();

        let err = shell
            .execute_script(script_token(&outer))
            .expect_err("break turns the failed include into the script's result");
        assert_eq!(err, format!("{}:2", script_token(&outer)));
    }

    #[test]
    fn test_execute_line_db_create_record_missing_args() {
        let shell = make_shell();
        // C accepts the call and lets the body diagnose it:
        // `dbStaticIocRegister.c:294-295` at `f4ccf7bc8` sets
        // `S_dbLib_recordNameMissing` and `:307-308` prints it on stderr
        // and sets the shell error. The command is in no release tag, so
        // it is absent at the `R7.0.10` pin.
        let dir = tempfile::tempdir().unwrap();
        let captured = dir.path().join("captured.err");
        let result = shell
            .ctx
            .with_error(std::fs::File::create(&captured).unwrap(), || {
                shell.execute_line("dbCreateRecord pdbbase ai")
            });
        assert!(
            matches!(result, Ok(CommandOutcome::Failed)),
            "a missing record name must fail the command"
        );
        assert_eq!(
            std::fs::read_to_string(&captured).unwrap(),
            "33554465 Record name is required\n"
        );
    }

    /// `iocshCmd("dbl")` runs a single command line by
    /// re-entering the shell.
    #[test]
    fn test_iocsh_cmd_runs_single_command() {
        let shell = make_shell();
        let result = shell.execute_line(r#"iocshCmd("dbl")"#);
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    /// `iocshRun` runs ONE command line, exactly like `iocshCmd` —
    /// `iocshCmd(cmd)` is `iocshRun(cmd, NULL)` and `iocshRun` is
    /// `iocshBody(NULL, cmd, macros)` (`iocsh.cpp:1335-1353` @R7.0.10).
    /// `;` is not a word separator in C's tokenizer (`iocsh.cpp:271`
    /// separates on `" \t(),\r"` only) and appears nowhere else in that
    /// file, so `"dbl; pwd"` is the single unregistered command `dbl;`.
    #[test]
    fn test_iocsh_run_runs_one_command_line_and_does_not_split_on_semicolon() {
        let shell = make_shell();
        assert!(matches!(
            shell.execute_line(r#"iocshRun("dbl")"#),
            Ok(CommandOutcome::Continue)
        ));
        // `Failed`, not `Err`: C reports the miss itself at
        // `iocsh.cpp:1302` and leaves the caller nothing to print.
        assert!(
            matches!(
                shell.execute_line(r#"iocshRun("dbl; pwd")"#),
                Ok(CommandOutcome::Failed)
            ),
            "`;` is not a command separator in C, so `dbl;` must be unregistered"
        );
    }

    /// core commands `echo`, `pwd`, `date` are registered so a
    /// stock `st.cmd` no longer errors on them.
    #[test]
    fn test_core_commands_registered() {
        let shell = make_shell();
        for line in ["echo hello", "pwd", "date", "epicsPrtEnvParams"] {
            assert!(
                matches!(shell.execute_line(line), Ok(CommandOutcome::Continue)),
                "core command line `{line}` must run"
            );
        }
    }

    /// the `as*` family is registered — `asInit` without a
    /// filename is a success no-op (C `asInitCommon`, asDbLib.c:127-128:
    /// returns 0 with no ACF file, leaving access security disabled), so
    /// a startup script under `on error break` is not aborted.
    ///
    /// `asInit` reads the process-global `as_state()` in
    /// `access_commands.rs`, which `access_commands::tests` also mutates
    /// (tempfile paths that get deleted, deliberately malformed ACFs). Take
    /// the same test lock those tests use, and reset the state itself
    /// rather than assume it starts fresh — the lock alone only stops a
    /// *concurrent* sibling from racing in; it does nothing about a
    /// filename a sibling left behind from running earlier in the same
    /// process, which is what turned this test's expected `Ok(Continue)`
    /// into a stale file-read `Err` under `cargo test`'s default
    /// concurrency.
    #[test]
    fn test_as_commands_registered() {
        let _guard = super::access_commands::as_state_test_guard();
        super::access_commands::reset_as_state_for_test();
        let shell = make_shell();
        // asInit without asSetFilename leaves AS disabled and continues.
        assert!(matches!(
            shell.execute_line("asInit"),
            Ok(CommandOutcome::Continue)
        ));
        // asprules with no config loaded prints a notice, returns Ok.
        assert!(matches!(
            shell.execute_line("asprules"),
            Ok(CommandOutcome::Continue)
        ));
    }

    /// `dbsr` walks the `dbServer` layer list — it is not the name search,
    /// and it is not a database-population report either.
    ///
    /// This shell administers no protocol server, so the list is empty and C
    /// prints its one line and returns BEFORE the state line
    /// (`dbServer.c:99-102`). Measured on `softIoc` at
    /// `R7.0.10-146-g8f5015b66`, a shell that DOES own a CA server prints
    /// `Server state: running` / `Server 'rsrv'` / RSRV's own report; the
    /// port emits the first two identically once
    /// `epics_ca_rs::server::iocsh::register_ca_db_server` has joined the
    /// list.
    #[test]
    fn test_dbsr_is_server_report() {
        let shell = make_shell();
        let tmpdir = tempfile::tempdir().expect("fixture root");
        let tmp = tmpdir.path().join("iocsh_dbsr_report.txt");
        let line = format!("dbsr > {}", tmp.display());
        assert!(matches!(
            shell.execute_line(&line),
            Ok(CommandOutcome::Continue)
        ));
        let content = std::fs::read_to_string(&tmp).unwrap();
        assert_eq!(
            content.trim_end(),
            "No server layers registered with IOC",
            "dbsr with no layer registered is C's one line and nothing else"
        );
        // Specifically NOT the database-population report it used to print,
        // and not the dbgrep name-search output.
        assert!(!content.contains("Records served"));
        assert!(!content.contains("Total"));
        std::fs::remove_file(&tmp).ok();
    }

    /// A registry miss under the default `on error continue` is not a
    /// script failure. Measured on `softIoc` R7.0.10-146 with
    /// `nosuchcmd` / `dbl` / `exit`: stderr carries
    /// `ERROR a.cmd line 1: Command 'nosuchcmd' not registered.`, `dbl`
    /// runs, and the process exits 0 — C leaves `ret` at its
    /// `iocsh.cpp:1037` zero because only the Break and Halt arms
    /// (`:1127`, `:1132`) assign it.
    #[test]
    fn an_unregistered_command_does_not_fail_the_script() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("a.cmd");
        std::fs::write(
            &script,
            "nonexistent_cmd\ndbCreateRecord pdbbase ai AFTER_THE_MISS\n",
        )
        .unwrap();

        let result = shell.execute_script(script.to_str().unwrap());
        assert!(
            result.is_ok(),
            "C's `ret` stays 0 through a registry miss: {result:?}"
        );
        assert!(
            shell.ctx.db().get_record("AFTER_THE_MISS").is_some(),
            "C runs the line after the miss"
        );
    }

    /// `on error break` stops the script at the first failing
    /// line. Without it (default `continue`) the whole script runs.
    #[test]
    fn test_on_error_break_stops_script() {
        let shell = make_shell();
        let tmpdir = tempfile::tempdir().expect("fixture root");
        let tmp = tmpdir.path().join("iocsh_on_error_break.cmd");
        // Line 2 fails; line 3 would create a record if reached.
        std::fs::write(
            &tmp,
            "on error break\nnonexistent_cmd\ndbCreateRecord pdbbase ai SHOULD_NOT_EXIST\n",
        )
        .unwrap();
        let result = shell.execute_script(tmp.to_str().unwrap());
        std::fs::remove_file(&tmp).ok();
        assert!(result.is_err(), "on error break must surface Err");
        // The record from line 3 must NOT have been created.
        assert!(
            shell.ctx.db().get_record("SHOULD_NOT_EXIST").is_none(),
            "on error break must stop before line 3 runs"
        );
    }

    /// C `iocsh.cpp:1034` declares `iocshScope scope;` as a fresh
    /// automatic inside `iocshBody`, so the `on error break` an included
    /// script sets dies with that script. With one shell-global mode the
    /// caller's very next failing line stopped the boot instead.
    #[test]
    fn on_error_break_in_an_included_script_dies_with_that_script() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("inner.cmd");
        std::fs::write(&inner, "on error break\n").unwrap();
        let out = dir.path().join("after.txt");
        let outer = dir.path().join("outer.cmd");
        std::fs::write(
            &outer,
            format!(
                "< {}\nnonexistent_cmd\ndbl > {}\n",
                inner.display(),
                out.display()
            ),
        )
        .unwrap();

        let result = shell.execute_script(outer.display().to_string());
        assert!(
            result.is_ok(),
            "the caller's own scope is `continue`, so C's ret stays 0"
        );
        assert!(
            out.exists(),
            "the include's 'on error break' must not stop the caller's script"
        );
        assert!(shell.scopes.borrow().is_empty(), "scope ticket released");
    }

    /// C `iocsh.cpp:1078-1080`: "use of iocshCmd() implies 'on error
    /// break'" — and it is a scope of its own, so the forced mode does
    /// not reach the calling script's next line.
    ///
    /// The failing line inside `iocshRun` used to be the second half of
    /// a `;`-separated pair; C has no such separator, so the break is
    /// pinned on the one command line C actually runs and the leak half
    /// is pinned by the caller's own next line still executing.
    #[test]
    fn iocsh_run_breaks_without_leaking_the_mode() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let after = dir.path().join("after.txt");
        let script = dir.path().join("st.cmd");
        std::fs::write(
            &script,
            format!(
                "iocshRun \"nonexistent_cmd\"\nnonexistent_cmd\ndbl > {}\n",
                after.display()
            ),
        )
        .unwrap();

        let result = shell.execute_script(script.display().to_string());
        assert!(
            result.is_ok(),
            "the enclosing script's scope is `continue`, so C's ret stays 0"
        );
        assert!(
            after.exists(),
            "the implied break must not outlive the iocshRun scope"
        );
    }

    /// C parses the `on error wait` delay with `epicsParseDouble`
    /// (`iocsh.cpp:1554-1555`), so a fractional delay is legal, and
    /// `Halt` with a positive timeout stalls and CONTINUES the script
    /// (`:1139-1141`) rather than unwinding it.
    #[test]
    fn on_error_wait_takes_a_fractional_delay_and_continues() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("after.txt");
        let script = dir.path().join("st.cmd");
        std::fs::write(
            &script,
            format!(
                "on error wait 0.25\nnonexistent_cmd\ndbl > {}\n",
                out.display()
            ),
        )
        .unwrap();

        let start = std::time::Instant::now();
        let result = shell.execute_script(script.display().to_string());
        let elapsed = start.elapsed();
        assert!(result.is_err(), "the failing line must still be reported");
        assert!(
            elapsed >= std::time::Duration::from_millis(200),
            "a 0.25 s wait must actually stall: {elapsed:?}"
        );
        assert!(out.exists(), "'wait' continues the script after the stall");
    }

    /// One unit of `on error wait`, long enough that the boundary between
    /// "stalled once" and "stalled once per line" survives a loaded box.
    const WAIT_UNIT: f64 = 0.4;

    /// Write `body` to a script in `dir` and time running it.
    fn timed_script(shell: &IocShell, dir: &std::path::Path, body: &str) -> std::time::Duration {
        let script = dir.join("st.cmd");
        std::fs::write(&script, body).unwrap();
        let start = std::time::Instant::now();
        let _ = shell.execute_script(script.display().to_string());
        start.elapsed()
    }

    /// C reads `scope.errored` at the TOP of every pass of the line loop
    /// (`iocsh.cpp:1122`) and nothing resets it there, so a line that
    /// dispatches no command — a comment here — leaves the failure standing
    /// and stalls again. The pass that ends the loop counts too.
    ///
    /// Measured on `softIoc` R7.0.10, `on error wait 1` + a failing line + two
    /// comments + `exit`: three `iocsh Error: Waiting 1.0 sec ...` lines and
    /// 3.1 s wall clock.
    #[test]
    fn on_error_wait_stalls_once_per_line_that_dispatches_nothing() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let elapsed = timed_script(
            &shell,
            dir.path(),
            &format!("on error wait {WAIT_UNIT}\nnonexistent_cmd\n# one\n# two\n"),
        );
        assert!(
            elapsed.as_secs_f64() >= WAIT_UNIT * 2.0,
            "two comments and the loop's last pass must each stall again: {elapsed:?}"
        );
    }

    /// ...and the one thing that clears it is a command that actually ran —
    /// C sets `scope.errored` ahead of the lookup and clears it immediately
    /// before the call (`iocsh.cpp:1251`, `:1268`).
    #[test]
    fn a_command_that_runs_clears_the_pending_error() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("after.txt");
        let elapsed = timed_script(
            &shell,
            dir.path(),
            &format!(
                "on error wait {WAIT_UNIT}\nnonexistent_cmd\ndbl > {}\n# one\n",
                out.display()
            ),
        );
        assert!(out.exists(), "'wait' continues the script after the stall");
        assert!(
            elapsed.as_secs_f64() < WAIT_UNIT * 2.0,
            "the `dbl` clears the failure, so nothing after it stalls: {elapsed:?}"
        );
    }

    /// A `<` include is NOT such a command: C runs it through its own
    /// `iocshBody` and only ever SETS the caller's flag from the result
    /// (`iocsh.cpp:1233-1234`). Measured on `softIoc` R7.0.10 — a `<` whose
    /// script succeeds still leaves the caller waiting on the failure before
    /// it — and it is the case that separates "some command ran" from "a
    /// command of THIS scope ran".
    #[test]
    fn an_include_that_succeeds_does_not_clear_the_callers_error() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("inner.txt");
        let inner = dir.path().join("inner.cmd");
        std::fs::write(&inner, format!("dbl > {}\n", out.display())).unwrap();
        let elapsed = timed_script(
            &shell,
            dir.path(),
            &format!(
                "on error wait {WAIT_UNIT}\nnonexistent_cmd\n< {}\n",
                inner.display()
            ),
        );
        assert!(out.exists(), "the include itself must have run");
        assert!(
            elapsed.as_secs_f64() >= WAIT_UNIT * 2.0,
            "the include line dispatched nothing of the caller's own: {elapsed:?}"
        );
    }

    /// `on error ...` clears the flag itself (`iocsh.cpp:1538`, "don't fault
    /// on previous, ignored, errors") — so re-stating the policy after a
    /// failure does not stall on it a second time.
    #[test]
    fn the_on_command_clears_a_pending_error() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let elapsed = timed_script(
            &shell,
            dir.path(),
            &format!(
                "on error wait {WAIT_UNIT}\nnonexistent_cmd\non error wait {WAIT_UNIT}\n# one\n"
            ),
        );
        assert!(
            elapsed.as_secs_f64() < WAIT_UNIT * 2.0,
            "the second `on error` cleared the failure: {elapsed:?}"
        );
    }

    /// The reaction runs on the pass that finds EOF, so a failure on the LAST
    /// line is reacted to like any other — C only leaves the loop through
    /// `epicsReadline` returning NULL, one pass after the failing line.
    #[test]
    fn a_failure_on_the_last_line_still_reaches_the_reaction() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("st.cmd");
        std::fs::write(&script, "on error break\nnonexistent_cmd\n").unwrap();
        assert!(
            shell.execute_script(script.display().to_string()).is_err(),
            "`break` on the file's last line must still set C's ret = -1"
        );
    }

    /// C `iocsh.cpp:1132-1135`: `Halt` with a non-positive timeout calls
    /// `epicsThreadSuspendSelf()`. Blocking the thread is not the same
    /// thing, and neither is unwinding the script: the operator's next
    /// move is `epicsThreadShowAll` to see where the boot stopped and
    /// `epicsThreadResume` to let it go on, so both are asserted here.
    #[test]
    fn on_error_halt_suspends_the_shell_thread() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("st.cmd");
        std::fs::write(&script, "on error halt\nnonexistent_cmd\n").unwrap();
        let path = script.display().to_string();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let shell = make_shell();
            let _ = shell.execute_script(&path);
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(750))
                .is_err(),
            "'on error halt' must leave the shell thread suspended"
        );

        let halted = crate::runtime::task::thread_report()
            .into_iter()
            .find(|t| t.is_suspended())
            .expect("the halted shell must be a SUSPEND row, not an OK one");
        assert!(
            halted.show_line().ends_with(" SUSPEND"),
            "C's STATE column reads SUSPEND \
             (os/Linux/osdThreadExtra.c:49-54), got {:?}",
            halted.show_line()
        );
        assert!(halted.resume(), "epicsThreadResume must find it suspended");
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("epicsThreadResume must release the halted shell");
    }

    /// Boundary table for `onCallFunc` (`iocsh.cpp:1526-1568`): no scope
    /// is a no-op, an interactive scope refuses the command, a bad delay
    /// falls back to 5.0 s, `wait` with no delay is a plain halt, and
    /// only an unrecognised mode word fails the line.
    #[test]
    fn on_error_command_boundaries_match_c() {
        let shell = make_shell();

        shell
            .execute_line("on error break")
            .expect("outside iocshBody the command does nothing");
        assert!(shell.scopes.borrow().is_empty());

        {
            let _scope = shell.enter_scope(IocshScope::interactive());
            shell
                .execute_line("on error break")
                .expect("an interactive shell ignores it without failing");
            assert_eq!(
                shell.current_scope().unwrap().on_error,
                OnError::Continue,
                "'on error' must not take effect in an interactive shell"
            );
        }

        let _scope = shell.enter_scope(IocshScope::script(b"st.cmd"));
        assert_eq!(shell.current_scope().unwrap().on_error, OnError::Continue);

        shell.execute_line("on error halt").unwrap();
        assert_eq!(
            shell.current_scope().unwrap().on_error,
            OnError::Halt { timeout: 0.0 }
        );

        shell.execute_line("on error wait 1.5").unwrap();
        assert_eq!(
            shell.current_scope().unwrap().on_error,
            OnError::Halt { timeout: 1.5 }
        );

        shell
            .execute_line("on error wait bogus")
            .expect("C prints the usage and keeps the line's status clean");
        assert_eq!(
            shell.current_scope().unwrap().on_error,
            OnError::Halt { timeout: 5.0 },
            "an unparseable delay falls back to C's 5.0 s"
        );

        shell
            .execute_line("on error wait")
            .expect("usage only, not an error");
        assert_eq!(
            shell.current_scope().unwrap().on_error,
            OnError::Halt { timeout: 0.0 },
            "'wait' with no delay is C's plain halt"
        );

        assert!(
            matches!(
                shell.execute_line("on error bogus"),
                Ok(CommandOutcome::Failed)
            ),
            "an unrecognised mode is the one case C flags as an error"
        );
    }

    /// single-quoted arguments tokenize as one token.
    #[test]
    fn test_single_quote_tokenization() {
        assert_eq!(
            tokenize(b"dbpf REC:VAL 'hello world'"),
            vec!["dbpf", "REC:VAL", "hello world"]
        );
        assert_eq!(tokenize(b"cmd('a, b', c)"), vec!["cmd", "a, b", "c"]);
    }

    /// L-5: an unbalanced quote / trailing backslash is flagged.
    #[test]
    fn test_malformed_line_is_rejected() {
        let shell = make_shell();
        assert!(
            shell.execute_line(r#"echo "unterminated"#).is_err(),
            "unbalanced quote must be rejected"
        );
        assert!(
            shell.execute_line("echo trailing\\").is_err(),
            "trailing backslash must be rejected"
        );
    }

    /// `NO_COLOR` and `EPICS_RS_IOCSH_NO_COLOR` env vars opt out of
    /// the ANSI prompt. Defensive — uses `serial_test` group key so
    /// concurrent env-mutating tests don't race.
    #[test]
    #[serial_test::serial(epics_env)]
    fn test_use_ansi_color_respects_no_color() {
        // Snapshot + restore so the test doesn't leak state to siblings.
        let no_color = std::env::var_os("NO_COLOR");
        let epics_no = std::env::var_os("EPICS_RS_IOCSH_NO_COLOR");
        // Clear both so the default path returns true.
        unsafe {
            std::env::remove_var("NO_COLOR");
            std::env::remove_var("EPICS_RS_IOCSH_NO_COLOR");
        }
        assert!(use_ansi_color());

        unsafe { std::env::set_var("NO_COLOR", "1") };
        assert!(!use_ansi_color(), "NO_COLOR=1 must disable color");
        unsafe { std::env::remove_var("NO_COLOR") };

        unsafe { std::env::set_var("EPICS_RS_IOCSH_NO_COLOR", "yes") };
        assert!(
            !use_ansi_color(),
            "EPICS_RS_IOCSH_NO_COLOR=yes must disable color"
        );
        unsafe { std::env::remove_var("EPICS_RS_IOCSH_NO_COLOR") };

        // Restore.
        if let Some(v) = no_color {
            unsafe { std::env::set_var("NO_COLOR", v) };
        }
        if let Some(v) = epics_no {
            unsafe { std::env::set_var("EPICS_RS_IOCSH_NO_COLOR", v) };
        }
    }
}
