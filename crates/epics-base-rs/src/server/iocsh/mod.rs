// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that hand-builds its own tokio runtime; runs and passes in the feature-ON suite.
mod access_commands;
mod commands;
mod core_commands;
pub mod registry;
mod vars;

/// libCom `macParseDefns`-equivalent quote/escape-aware splitter for IOC
/// macro definition strings. Re-exported as the single owner of that
/// grammar so cross-crate macLib consumers (QSRV `dbLoadGroup`) reuse it
/// rather than duplicating a raw comma splitter.
pub use commands::macro_defn_pairs;

use std::collections::HashMap;
use std::fs::File;
use std::sync::{Arc, RwLock};

use crate::server::database::PvDatabase;
use registry::*;

/// Error-handling mode set by the `on error` command — C `OnError`
/// (`iocsh.cpp:988-992`). `halt` and `wait <delay>` are ONE C state
/// (`onerr = Halt` plus `scope.timeout`), so they are one variant here:
/// a timeout that is zero, negative or infinite suspends the thread,
/// a positive finite one stalls it and lets the script run on
/// (`iocsh.cpp:1136-1150`).
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

/// C `iocshScope` (`iocsh.cpp:995-1002`) — the error policy of ONE
/// `iocshBody` entry. C declares it as a fresh automatic inside
/// `iocshBody` and chains it to the thread context as the innermost of a
/// stack (`:1105-1106`, `:1315-1321`), so an `on error break` taken by an
/// included script dies with that script instead of reaching the
/// caller's next line.
#[derive(Clone, Copy, Debug, PartialEq)]
struct IocshScope {
    on_error: OnError,
    /// C `scope.interactive` (`iocsh.cpp:1051-1056`): set whenever
    /// `iocshBody` reads stdin rather than a file or a command string —
    /// terminal or not. `on error` is refused there (`:1538-1540`) and a
    /// failing line never triggers the reaction (`:1128`).
    interactive: bool,
    /// Set for the file-script entries counted against
    /// [`MAX_SCRIPT_DEPTH`].
    file_script: bool,
}

impl IocshScope {
    /// A `<` include, an `iocshLoad` or the startup script — C's plain
    /// ctor default (`iocsh.cpp:1001` `onerr(Continue)`).
    fn script() -> Self {
        Self {
            on_error: OnError::Continue,
            interactive: false,
            file_script: true,
        }
    }

    /// `iocshCmd` / `iocshRun` — C `iocsh.cpp:1085-1086`, "use of
    /// iocshCmd() implies \"on error break\"".
    fn command_line() -> Self {
        Self {
            on_error: OnError::Break,
            interactive: false,
            file_script: false,
        }
    }

    /// The stdin REPL.
    fn interactive() -> Self {
        Self {
            on_error: OnError::Continue,
            interactive: true,
            file_script: false,
        }
    }
}

/// Interactive IOC shell with extensible command registration.
pub struct IocShell {
    registry: Arc<RwLock<CommandRegistry>>,
    ctx: CommandContext,
    /// C's `iocshScope` chain (`iocsh.cpp:995-1006`), innermost last:
    /// one entry per `iocshBody`-equivalent entry, pushed by
    /// [`IocShell::enter_scope`] and popped by [`ScopeGuard`]. `RefCell`
    /// because the shell drives one script at a time on a single thread;
    /// the `on error` command mutates the innermost entry mid-script.
    /// Empty outside every script — C's `context->scope == NULL`, where
    /// `on error` does nothing at all (`iocsh.cpp:1532-1534`).
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
    /// C's `MAC_HANDLE` scope stack (`iocsh.cpp:1105` `macCreateHandle`,
    /// `:1118` `macPushScope`/`macInstallMacros`): a `<` include reached
    /// from inside an `iocshLoad` sees the load's macros, and each frame
    /// is the whole visible set, so a lookup is one map read and popping
    /// a frame restores the outer set by construction.
    ///
    /// The handle is thread-private, not per-shell: `iocshBody`
    /// reaches it through `epicsThreadPrivateGet(iocshContextId)`
    /// (`iocsh.cpp:1382-1385`), which is what lets `epicsEnvSet` —
    /// an ordinary registered command with no shell handle — clear a
    /// macro that shadows the variable it just set.
    static MACRO_SCOPE: std::cell::RefCell<Vec<HashMap<String, String>>> =
        std::cell::RefCell::new(vec![HashMap::new()]);
}

/// C `iocshEnvClear` (`iocsh.cpp:1377-1389`) — `macPutValue(handle,
/// name, NULL)`, which deletes the macro from EVERY scope, not just the
/// innermost (`macCore.c:252-268`; the comment there notes iocshEnvClear
/// is exactly why the all-scopes behaviour is kept). `epicsEnvSet` and
/// `epicsEnvUnset` both call it before touching the environment
/// (`osdEnv.c:49`, `:60`), so an `iocshLoad("inner.cmd","PORT=OLD")`
/// macro stops shadowing the environment the moment the loaded script
/// sets `PORT` itself.
pub(crate) fn iocsh_env_clear(name: &str) {
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

/// Ticket for one `iocshBody`-equivalent entry (C `iocsh.cpp:1040`
/// `iocshScope scope;`) — every exit path of the executors pops the
/// scope via `Drop`, which is what makes the fresh `Continue` default
/// hold for the caller's next line.
struct ScopeGuard<'a>(&'a IocShell);

impl Drop for ScopeGuard<'_> {
    fn drop(&mut self) {
        self.0.scopes.borrow_mut().pop();
    }
}

/// C `macPushScope` (`iocsh.cpp:1118`) — every exit path of the script
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
        let mut registry = CommandRegistry::new();
        commands::register_builtins(&mut registry);
        // C `iocshRegisterCommon` publishes the base version and target arch as
        // environment variables at the same point it registers the commands, so
        // the first `dbLoadRecords` can already expand `$(EPICS_VERSION_FULL)`.
        crate::runtime::env::register_iocsh_env_vars();
        Self {
            registry: Arc::new(RwLock::new(registry)),
            ctx: CommandContext::new_with_acf(db, bridge, acf),
            scopes: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// Enter one `iocshBody`-equivalent scope.
    fn enter_scope(&self, scope: IocshScope) -> ScopeGuard<'_> {
        self.scopes.borrow_mut().push(scope);
        ScopeGuard(self)
    }

    /// The innermost scope, `None` outside every `iocshBody` equivalent
    /// — C's `context->scope`.
    fn current_scope(&self) -> Option<IocshScope> {
        self.scopes.borrow().last().copied()
    }

    /// Enter the scope of one nested script, refusing past
    /// [`MAX_SCRIPT_DEPTH`] so a self-including script errors out at the
    /// include line instead of overflowing the thread stack.
    fn enter_script(&self, path: &str) -> Result<ScopeGuard<'_>, String> {
        let depth = self
            .scopes
            .borrow()
            .iter()
            .filter(|scope| scope.file_script)
            .count();
        if depth >= MAX_SCRIPT_DEPTH {
            return Err(format!(
                "'{path}': script include depth exceeds {MAX_SCRIPT_DEPTH} — \
                 recursive '<' / iocshLoad?"
            ));
        }
        Ok(self.enter_scope(IocshScope::script()))
    }

    /// C `macPushScope` + `macInstallMacros` (`iocsh.cpp:1118-1119`):
    /// enter an `iocshLoad`'s macro scope. The new frame starts as a copy
    /// of the visible set, so the loaded script still resolves everything
    /// the caller could.
    fn push_macro_scope(&self, macros: &HashMap<String, String>) -> MacroScopeGuard {
        MACRO_SCOPE.with(|scope| {
            let mut scope = scope.borrow_mut();
            let mut frame = scope.last().cloned().unwrap_or_default();
            frame.extend(macros.iter().map(|(k, v)| (k.clone(), v.clone())));
            scope.push(frame);
        });
        MacroScopeGuard
    }

    /// C `macDefExpand(raw, handle)` (`iocsh.cpp:1190`) — the ONE macro
    /// expansion an iocsh line gets, over the ONE handle that carries both
    /// the pushed `iocshLoad` scope and the environment (`:1039` `pairs[]
    /// = {"", "environ", NULL, NULL}` → `FLAG_USE_ENVIRONMENT`,
    /// `macCore.c:130-133`, `:589-594`). `Err` is C's NULL return: macLib
    /// reports the undefined macro (`macCore.c:911-916`) and
    /// `iocsh.cpp:1190-1193` skips the line instead of running it with the
    /// placeholder text. The `.db` and ACF readers deliberately keep the
    /// lenient rule — `macCreateHandle(&h, NULL)` with a warning
    /// (`dbLexRoutines.c:259,381-386`, `asLibRoutines.c:241`) — and are
    /// not routed through here.
    fn expand_line(&self, raw: &str) -> Result<String, String> {
        let expanded = MACRO_SCOPE.with(|scope| {
            let scope = scope.borrow();
            let macros = scope.last().expect("macro scope stack is never empty");
            crate::server::db_loader::expand_macros(
                raw,
                macros,
                crate::server::db_loader::MacroExpandOptions {
                    env_fallback: true,
                    ..Default::default()
                },
            )
        });
        if expanded.undefined.is_empty() {
            return Ok(expanded.text);
        }
        let mut names: Vec<&str> = Vec::new();
        for name in &expanded.undefined {
            if !names.contains(&name.as_str()) {
                names.push(name);
            }
        }
        Err(format!(
            "macLib: macro {} is undefined (expanding string {raw})",
            names.join(", ")
        ))
    }

    /// Register an additional command (thread-safe, takes &self).
    pub fn register(&self, def: CommandDef) {
        self.registry.write().unwrap().register(def);
    }

    /// Execute a single line of input.
    ///
    /// C `iocsh.cpp:1166-1213`: a comment is recognised BEFORE expansion
    /// ("avoids macLib errors from comments"), the line is expanded once,
    /// and a line left empty or commented by that expansion is dropped.
    /// Supports C EPICS iocsh output redirection:
    /// - `command > file` — redirect stdout to file (overwrite)
    /// - `command >> file` — redirect stdout to file (append)
    pub fn execute_line(&self, line: &str) -> CommandResult {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Ok(CommandOutcome::Continue);
        }
        let expanded = self.expand_line(line)?;
        self.execute_expanded_line(&expanded)
    }

    /// [`Self::execute_line`] past the one macro expansion — everything
    /// here reads text C has already run through `macDefExpand`, so no
    /// fragment of it is expanded a second time.
    fn execute_expanded_line(&self, line: &str) -> CommandResult {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Ok(CommandOutcome::Continue);
        }

        // L-5: C `split()` (`iocsh.cpp:349-360`) flags an unbalanced
        // quote or a trailing backslash and returns BEFORE
        // `openRedirect` runs, so a malformed line never creates —
        // and never truncates — a redirect target. Linting here, ahead
        // of `parse_redirect`, is what gives the port that ordering;
        // it also covers the `<` / `iocshLoad` / `on` lines below,
        // which C lints in the same pass.
        if let Some(diag) = registry::lint_line(line) {
            return Err(diag.to_string());
        }

        // `< filename` include. C `iocsh.cpp:1239`
        // `iocshBody(commandFile, NULL, macros)` re-enters on the same
        // handle with the same macros, so the included script keeps the
        // enclosing scope — here it simply stays on the current frame.
        if let Some(rest) = line.strip_prefix('<') {
            return self
                .execute_script(rest.trim())
                .map(|_| CommandOutcome::Continue);
        }

        // Handle `iocshLoad <path> [macros]` (Issue #847): include with
        // the macros pushed as a scope for the loaded script.
        // Intercepting before registry lookup lets the loaded script's
        // own lines re-enter `execute_line` (supporting `<` /
        // `iocshLoad` / redirects / registered commands recursively).
        // The path and macro string arrive already expanded.
        {
            let toks = tokenize(line);
            match toks.first().map(|s| s.as_str()) {
                Some("iocshLoad") => {
                    if toks.len() < 2 {
                        return Err("iocshLoad <path> [macros]".into());
                    }
                    let macros = toks
                        .get(2)
                        .map(|s| commands::parse_macro_string(s))
                        .unwrap_or_default();
                    return self
                        .execute_script_with_macros(&toks[1], &macros)
                        .map(|_| CommandOutcome::Continue);
                }
                // `iocshCmd("cmd")` runs a single command line;
                // `iocshRun("c1; c2")` runs `;`-separated commands.
                // Both re-enter `execute_line`, so they must be
                // dispatched here (the registry handler signature has
                // no access to the shell). Mirrors C `iocsh.cpp`
                // `iocshCmd` / `iocshRun`.
                Some("iocshCmd") => {
                    let Some(cmd) = toks.get(1) else {
                        return Err("iocshCmd <command>".into());
                    };
                    // C `iocsh.cpp:1085-1086`: reaching `iocshBody` with a
                    // command line rather than a file "implies 'on error
                    // break'", and it is a scope of its own — the mode
                    // never reaches the caller's next line.
                    let _scope = self.enter_scope(IocshScope::command_line());
                    return self.execute_line(cmd);
                }
                Some("iocshRun") => {
                    let Some(cmds) = toks.get(1) else {
                        return Err("iocshRun <commands>".into());
                    };
                    let _scope = self.enter_scope(IocshScope::command_line());
                    let mut last = Ok(CommandOutcome::Continue);
                    for one in cmds.split(';') {
                        let one = one.trim();
                        if one.is_empty() {
                            continue;
                        }
                        match self.execute_line(one) {
                            Ok(CommandOutcome::Exit) => return Ok(CommandOutcome::Exit),
                            Ok(CommandOutcome::Continue) => {}
                            Err(e) => {
                                let stop = self.react_to_error();
                                last = Err(e);
                                if stop {
                                    return last;
                                }
                            }
                        }
                    }
                    return last;
                }
                // `on error continue|break|halt|wait <delay>` —
                // sets how the running script reacts to a failing
                // line. Mirrors C `iocsh.cpp` `onCallFunc`.
                Some("on") => {
                    return self.handle_on_command(&toks);
                }
                _ => {}
            }
        }

        // Handle `> filename` / `>> filename` output redirection
        let (cmd_line, redirect) = parse_redirect(line);

        if let Some(redir) = redirect {
            let result = self.execute_command(cmd_line, Some(&redir));
            return result;
        }

        self.execute_command(cmd_line, None)
    }

    /// Execute a command, optionally redirecting output to a file.
    fn execute_command(&self, line: &str, redirect: Option<&Redirect>) -> CommandResult {
        let Some(redir) = redirect else {
            return self.execute_command_inner(line);
        };

        // C parity (iocsh.cpp:401-428 startRedirect): each fd-numbered
        // redirect reroutes ONLY its own stream — `case 1` → stdout,
        // `case 2` → stderr, fd 3-9 open the file but redirect nothing.
        // CommandContext models only the stdout sink, so a `>`/`1>`
        // redirect reroutes it via `with_output`; a `2>` (or higher) must
        // leave stdout ALONE. Routing stdout into the fd-N file would send
        // e.g. a `dbl 2>/dev/null` listing (which is stdout) to the file
        // and lose it entirely. fd≠1 capture (stderr) is not plumbed
        // through CommandContext, so the command runs with stdout intact
        // and a diagnostic notes the fd-N capture is unsupported.
        if redir.fd != 1 {
            eprintln!(
                "iocsh: fd {} redirect (stderr/other) not plumbed — \
                 stdout left intact, '{}' not captured",
                redir.fd, redir.path
            );
            return self.execute_command_inner(line);
        }

        let file_result = if redir.append {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&redir.path)
        } else {
            File::create(&redir.path)
        };
        match file_result {
            Ok(file) => self
                .ctx
                .with_output(file, || self.execute_command_inner(line)),
            Err(e) => {
                eprintln!("cannot open '{}': {}", redir.path, e);
                Ok(CommandOutcome::Continue)
            }
        }
    }

    fn execute_command_inner(&self, line: &str) -> CommandResult {
        let tokens = tokenize(line);
        if tokens.is_empty() {
            return Ok(CommandOutcome::Continue);
        }

        let cmd_name = &tokens[0];
        let arg_tokens = &tokens[1..];

        let registry = self.registry.read().unwrap();

        // Special handling for help — needs access to the registry
        if cmd_name == "help" {
            return self.execute_help(arg_tokens, &registry);
        }

        let def = registry
            .get(cmd_name)
            .ok_or_else(|| format!("unknown command: '{cmd_name}'"))?;

        let args = parse_args(arg_tokens, &def.args)?;
        def.handler.call(&args, &self.ctx)
    }

    /// Execute a script with the `iocshLoad` macros pushed as a scope,
    /// mirroring C `iocshLoad("path", "K=V,...")` (Issue #847). The
    /// macros go onto the shell's one macro handle (C `iocsh.cpp:1118`
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
        path: &str,
        macros: &HashMap<String, String>,
    ) -> Result<(), String> {
        set_startup_script_once(path);
        let _scope = self.push_macro_scope(macros);
        self.run_script(path)
    }

    /// Execute a script file line by line, echoing each line like C++ iocsh.
    ///
    /// C parity (144f975): errors from individual commands are reported but
    /// do not abort execution. The final return value is `Err` if any command
    /// failed — the equivalent of `iocshSetError` propagating a non-zero exit
    /// status to startup-script callers (e.g., automated IOC verification).
    ///
    /// This is the `iocshBody`-with-a-file level (`<` includes land
    /// here) — it does not record `IOCSH_STARTUP_SCRIPT`; see
    /// [`Self::execute_script_with_macros`]. C `iocsh.cpp:1239` re-enters
    /// `iocshBody` on the same handle for a `<`, so the include keeps the
    /// enclosing scope and no macro map is threaded here.
    pub fn execute_script(&self, path: &str) -> Result<(), String> {
        self.run_script(path)
    }

    /// The shared C `iocshBody` line loop for both script entry points.
    ///
    /// Order is C's (`iocsh.cpp:1166-1213`): a comment is recognised and
    /// echoed BEFORE expansion, the line is expanded exactly once, a
    /// failed expansion marks the script errored without executing the
    /// line, and the ECHO shows the EXPANDED text.
    fn run_script(&self, path: &str) -> Result<(), String> {
        let _depth = self.enter_script(path)?;
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read '{path}': {e}"))?;

        let mut last_err: Option<String> = None;
        for (line_num, raw) in join_backslash_continuations(&content) {
            // Comments are echoed but never expanded — C's own comment
            // says this "avoids macLib errors from comments".
            // `#-` silent comments are not echoed (iocsh.cpp:1196-1204).
            if raw.trim_start().starts_with('#') {
                if echoes_script_line(&raw) {
                    println!("{raw}");
                }
                continue;
            }

            // Echo the logical line (C++ iocsh behavior — continuations
            // are already collapsed so the echo shows the joined line)
            // after expansion, as C does.
            let outcome = match self.expand_line(&raw) {
                Ok(expanded) => {
                    if echoes_script_line(&expanded) {
                        println!("{expanded}");
                    }
                    self.execute_expanded_line(&expanded)
                }
                Err(e) => Err(e),
            };

            match outcome {
                Ok(CommandOutcome::Continue) => {}
                Ok(CommandOutcome::Exit) => {
                    return last_err.map(Err).unwrap_or(Ok(()));
                }
                Err(e) => {
                    eprintln!("{path}:{line_num}: Error: {e}");
                    let formatted = format!("{path}:{line_num}: {e}");
                    // honour `on error break|halt` — stop the
                    // script at the first failing line instead of
                    // the hardcoded "continue, report at end".
                    if self.react_to_error() {
                        return Err(formatted);
                    }
                    last_err = Some(formatted);
                }
            }
        }
        last_err.map(Err).unwrap_or(Ok(()))
    }

    /// Run the interactive REPL. Blocks until exit or EOF.
    ///
    /// When stdin is not a terminal (piped input, `<script.cmd` shell
    /// redirect, here-doc, ...) the rustyline interactive editor is
    /// skipped and lines come straight from `BufRead::lines()` with no
    /// prompt — mirrors epics-base PR #848 ("Skip readline interactive
    /// setup when not interactive"). Avoids `epics> ` prompt noise in
    /// the captured stderr stream when an operator pipes a script in.
    pub fn run_repl(&self) -> Result<(), String> {
        // C `iocsh.cpp:1051-1056` sets `scope.interactive` from
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
            .build();
        let mut rl = rustyline::DefaultEditor::with_config(config)
            .map_err(|e| format!("failed to initialize readline: {e}"))?;

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
        let ps1 = crate::runtime::env_table::IOCSH_PS1
            .get()
            .unwrap_or_default();
        let raw_prompt = strip_ansi(&ps1);
        let styled_prompt = if want_color { ps1 } else { raw_prompt.clone() };
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
                        Ok(CommandOutcome::Continue) => {}
                        Ok(CommandOutcome::Exit) => break,
                        Err(e) => eprintln!("{}", format_error(&e, want_color)),
                    }
                }
                Err(rustyline::error::ReadlineError::Eof) => break,
                Err(rustyline::error::ReadlineError::Interrupted) => continue,
                Err(e) => {
                    eprintln!(
                        "{}",
                        format_error(&format!("readline error: {e}"), want_color)
                    );
                    break;
                }
            }
        }

        Ok(())
    }

    fn run_repl_piped(&self) -> Result<(), String> {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        let mut line = String::new();
        loop {
            line.clear();
            match handle.read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match self.execute_line(trimmed) {
                        Ok(CommandOutcome::Continue) => {}
                        Ok(CommandOutcome::Exit) => break,
                        Err(e) => eprintln!("Error: {e}"),
                    }
                }
                Err(e) => {
                    eprintln!("stdin read error: {e}");
                    break;
                }
            }
        }
        Ok(())
    }

    /// Handle the `on error ...` command. Tokens are the
    /// already-tokenised line (`["on", "error", "<mode>", ...]`).
    ///
    /// C `onCallFunc` (`iocsh.cpp:1525-1577`) mutates the INNERMOST
    /// scope, does nothing at all when there is none, and refuses the
    /// command outright in an interactive shell. Only an unrecognised
    /// mode word is an error: every other misuse prints the usage and
    /// leaves the line's status clean.
    fn handle_on_command(&self, toks: &[String]) -> CommandResult {
        const USAGE: &str = "Usage: on error [continue | break | halt | wait <delay>]";
        let Some(scope) = self.current_scope() else {
            // Not called through an `iocshBody` equivalent.
            return Ok(CommandOutcome::Continue);
        };
        if scope.interactive {
            eprintln!("Interactive shell, 'on error' ignored.");
            return Ok(CommandOutcome::Continue);
        }
        if toks.len() < 3 || toks[1] != "error" {
            eprintln!("{USAGE}");
            return Ok(CommandOutcome::Continue);
        }
        let mode = match toks[2].as_str() {
            "continue" => OnError::Continue,
            "break" => OnError::Break,
            "halt" => OnError::Halt { timeout: 0.0 },
            "wait" => {
                // C parses the delay with `epicsParseDouble` — fractional
                // seconds are the documented spelling — and on a parse
                // failure prints the usage plus a named diagnostic and
                // falls back to 5.0 s. `wait` with no delay at all keeps
                // C's 0.0, which is a plain halt.
                let timeout = match toks.get(3) {
                    None => {
                        eprintln!("{USAGE}");
                        0.0
                    }
                    Some(delay) => match crate::runtime::stdlib::epics_scan_double(delay) {
                        Some(secs) => secs,
                        None => {
                            eprintln!("{USAGE}");
                            eprintln!("Invalid 'on error wait' delay '{delay}'.");
                            5.0
                        }
                    },
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

    /// React to a failing script line per the innermost scope's `on
    /// error` mode — C `iocsh.cpp:1128-1152`, which runs the check at
    /// the top of the next pass of the line loop. Returns `true` if the
    /// script must stop (with the error), `false` to run the next line.
    fn react_to_error(&self) -> bool {
        let scope = match self.current_scope() {
            Some(scope) => scope,
            None => return false,
        };
        // C guards the whole reaction with `!scope.interactive`.
        if scope.interactive {
            return false;
        }
        match scope.on_error {
            OnError::Continue => false,
            OnError::Break => {
                eprintln!("iocsh Error: Break");
                true
            }
            OnError::Halt { timeout } if timeout > 0.0 && timeout.is_finite() => {
                eprintln!("iocsh Error: Waiting {timeout:.1} sec ...");
                std::thread::sleep(crate::runtime::time::duration_from_secs(timeout));
                false
            }
            OnError::Halt { .. } => {
                eprintln!("iocsh Error: Halt");
                suspend_self();
                // C `break`s out of the line loop with `ret = -1` once
                // something resumes the thread.
                true
            }
        }
    }

    fn execute_help(&self, arg_tokens: &[String], registry: &CommandRegistry) -> CommandResult {
        if let Some(name) = arg_tokens.first() {
            if let Some(def) = registry.get(name) {
                self.ctx.println(&def.usage);
            } else {
                self.ctx.println(&format!("unknown command: '{name}'"));
            }
        } else {
            self.ctx.println("Available commands:");
            for name in registry.list() {
                self.ctx.println(&format!("  {name}"));
            }
        }
        Ok(CommandOutcome::Continue)
    }
}

/// C `epicsThreadSuspendSelf` (`osdThread.c`, `epicsEventWait` on a
/// never-signalled per-thread event) — park the shell thread with the
/// process still running, which is the point of `on error halt`: the
/// operator keeps a live IOC whose boot stopped where it broke. Parking
/// in a loop because nothing in this port issues `epicsThreadResume`, so
/// only a spurious wake-up could reach the loop head.
fn suspend_self() {
    loop {
        std::thread::park();
    }
}

/// Collapse C iocsh backslash-newline line continuations into logical
/// lines (epics-base PR #603). A physical line ending in `\` joins to
/// the next line: the trailing backslash is stripped, the newline is
/// dropped, and the next physical line's contents (including any
/// leading whitespace) follow immediately. `\` followed by any other
/// character — including a space before the newline — keeps the
/// backslash literal and terminates the logical line normally.
/// epics-base 8-D `c0da3dd` ANSI color: returns `true` if the iocsh
/// REPL should emit ANSI color sequences. Honours `NO_COLOR` env var
/// (<https://no-color.org>) and `EPICS_RS_IOCSH_NO_COLOR=1` opt-out;
/// otherwise on by default in the interactive (TTY) path.
///
/// Host-only: used only by the rustyline interactive editor, which is gated
/// out on `epics_embedded_target`.
#[cfg(not(epics_embedded_target))]
fn use_ansi_color() -> bool {
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
/// Host-only: used only by the rustyline interactive editor, which is gated
/// out on `epics_embedded_target`.
#[cfg(not(epics_embedded_target))]
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

/// Format an error string with optional ANSI bold-red prefix.
/// Plain `Error: <msg>` when color is off — preserves grep-ability.
///
/// Host-only: used only by the rustyline interactive editor, which is gated
/// out on `epics_embedded_target`.
#[cfg(not(epics_embedded_target))]
fn format_error(msg: &str, color: bool) -> String {
    if color {
        format!("\x1b[1;31mError:\x1b[0m {msg}")
    } else {
        format!("Error: {msg}")
    }
}

/// Whether a script line should be echoed before execution. Mirrors C
/// iocsh (`iocsh.cpp:1167-1204`): script lines are echoed, except a
/// *silent comment* delimited with `#-` (after leading whitespace), which
/// is suppressed. A plain `#` comment is still echoed; only `#-` is quiet.
/// Both kinds are skipped from execution (`execute_line`), so this gate
/// affects echoing only.
fn echoes_script_line(line: &str) -> bool {
    !line.trim_start().starts_with("#-")
}

/// C `iocshLoad` (`iocsh.cpp:1347-1351`) records the loaded script in
/// `IOCSH_STARTUP_SCRIPT` — since epics-base#469's fix only when the
/// variable is not already set, so it permanently names the *first*
/// script (or an inherited value from the parent environment) and a
/// nested `iocshLoad` no longer overwrites it. Set before the file is
/// even opened, like C — an unreadable path still lands here.
fn set_startup_script_once(path: &str) {
    if std::env::var_os("IOCSH_STARTUP_SCRIPT").is_none() {
        // SAFETY: same single-threaded-shell rationale as `epicsEnvSet`.
        unsafe { std::env::set_var("IOCSH_STARTUP_SCRIPT", path) };
    }
}

///
/// Returns `(physical_line_number, logical_line)` pairs. The line
/// number is the 1-based index of the *first* physical line in the
/// group, matching where a user would look for an error.
pub(crate) fn join_backslash_continuations(input: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut start_line: Option<usize> = None;
    for (idx, line) in input.lines().enumerate() {
        let physical_no = idx + 1;
        if start_line.is_none() {
            start_line = Some(physical_no);
        }
        if let Some(stripped) = line.strip_suffix('\\') {
            current.push_str(stripped);
        } else {
            current.push_str(line);
            out.push((
                start_line.take().unwrap_or(physical_no),
                std::mem::take(&mut current),
            ));
        }
    }
    if !current.is_empty() {
        out.push((start_line.unwrap_or(1), current));
    }
    out
}

struct Redirect {
    path: String,
    append: bool,
    /// File descriptor target. C iocsh (iocsh.cpp:287-303) accepts
    /// `1>` through `9>` (and `1>>` ... `9>>`). The Rust port only plumbs
    /// the stdout sink (fd 1) — `with_output` captures `ctx.println` /
    /// `print_fmt` writes. Tracking the fd here lets us recognize the C
    /// syntax without erroring; fd≠1 (e.g. `2>`) leaves stdout untouched
    /// (the fd-N stream is not plumbed) so `dbl 2>/dev/null` keeps its
    /// stdout listing rather than misrouting it into the file.
    fd: u8,
}

/// Parse `>` / `>>` / `N>` / `N>>` redirect from a line.
/// Returns (command_part, optional redirect).
///
/// C parity (iocsh.cpp:287-303): `1>file` through `9>file` and the
/// double-`>>` (append) variants. Default fd when bare `>` is used is
/// stdout (fd 1).
fn parse_redirect(line: &str) -> (&str, Option<Redirect>) {
    let bytes = line.as_bytes();
    // C gates the whole redirect block on `!quote && !backslash`
    // (`iocsh.cpp:272`), the same state that decides separators and
    // quote termination — so a `>` inside either quote, or escaped, is
    // ordinary data. [`registry::ShellScan`] is that state.
    let mut scan = registry::ShellScan::default();

    let mut i = 0;
    while i < bytes.len() {
        let syntax = scan.feed(bytes[i]);
        match bytes[i] {
            b'>' if syntax => {
                // C parity: if the char before `>` is a single ASCII
                // digit 1..9 AND that digit follows a separator (start
                // of line, space, tab) — interpret as N>.
                let (op_start, fd) = if i > 0 && bytes[i - 1].is_ascii_digit() {
                    let d = bytes[i - 1];
                    // Confirm the digit is at a token boundary: either
                    // i==1 (digit is first non-empty char in line) or
                    // the char before the digit is whitespace.
                    let at_boundary =
                        i == 1 || matches!(bytes[i - 2], b' ' | b'\t' | b'\r' | b'\n');
                    if at_boundary && (b'1'..=b'9').contains(&d) {
                        (i - 1, d - b'0')
                    } else {
                        (i, 1u8)
                    }
                } else {
                    (i, 1u8)
                };
                let is_append = i + 1 < bytes.len() && bytes[i + 1] == b'>';
                let cmd = line[..op_start].trim_end();
                let skip = if is_append { 2 } else { 1 };
                let path = line[i + skip..].trim();
                if path.is_empty() {
                    return (line, None);
                }
                return (
                    cmd,
                    Some(Redirect {
                        // Already expanded once by `execute_line`, as
                        // C's `split` sees a `macDefExpand`ed line.
                        path: path.to_string(),
                        append: is_append,
                        fd,
                    }),
                );
            }
            _ => {}
        }
        i += 1;
    }

    (line, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::records::ai::AiRecord;

    /// `#-` silent comments are not echoed; plain `#` comments and
    /// commands are. Boundary cases: leading whitespace before `#-`, a
    /// space between `#` and `-` (a plain comment, echoed), and the bare
    /// `#-` token. Mirrors C iocsh `iocsh.cpp:1167-1204`.
    #[test]
    fn echoes_script_line_suppresses_only_hash_dash() {
        // Silent comments — not echoed.
        assert!(!echoes_script_line("#-"));
        assert!(!echoes_script_line("#- a quiet note"));
        assert!(!echoes_script_line("#-nospace"));
        assert!(!echoes_script_line("   #- leading whitespace"));
        // Plain comments — echoed.
        assert!(echoes_script_line("#"));
        assert!(echoes_script_line("# normal comment"));
        assert!(echoes_script_line("# -space after hash"));
        // Commands and blanks — echoed (unchanged behaviour).
        assert!(echoes_script_line("dbLoadRecords(\"x.db\")"));
        assert!(echoes_script_line(""));
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
    /// (`iocsh.cpp:272`), so a `>` inside EITHER quote — or escaped —
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
        // way (`dbIocRegister.c:68`).
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

    fn make_shell() -> IocShell {
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
            shell.expand_line("$(EPICS_VERSION_FULL)").unwrap(),
            crate::runtime::version::EPICS_VERSION_FULL,
        );
        assert_eq!(
            shell.expand_line("${ARCH}").unwrap(),
            crate::runtime::build_info::TARGET_ARCH,
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

    #[test]
    fn test_execute_line_unknown() {
        let shell = make_shell();
        let result = shell.execute_line("nonexistent_cmd");
        assert!(result.is_err());
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
        // Neither `dbgf` nor `dbpf` belongs here any more: C answers a
        // missing name with its own usage line (`dbTest.c:358-361`,
        // `:400-403`), so both registrations mark their arguments
        // optional. `epicsEnvSet` still declares its two required,
        // which is what this exercises.
        let result = shell.execute_line("epicsEnvSet");
        assert!(result.is_err());
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
            .execute_line(&format!(
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
            .execute_line(&format!(
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

    #[test]
    fn test_execute_line_include_syntax() {
        let shell = make_shell();
        // A non-existent file should return an error
        let result = shell.execute_line("< nonexistent_file.cmd");
        assert!(result.is_err());
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

        let path = dir.path().join("self.cmd");
        std::fs::write(&path, format!("< {}\n", path.display())).unwrap();
        let err = shell
            .execute_script(&path.display().to_string())
            .expect_err("self-include via '<' must fail at the cap");
        assert!(err.contains("depth exceeds"), "got: {err}");
        assert!(
            shell.scopes.borrow().is_empty(),
            "scope ticket fully released"
        );

        let path2 = dir.path().join("self2.cmd");
        std::fs::write(&path2, format!("iocshLoad {}\n", script_token(&path2))).unwrap();
        let err = shell
            .execute_script(&path2.display().to_string())
            .expect_err("self-include via iocshLoad must fail at the cap");
        assert!(err.contains("depth exceeds"), "got: {err}");
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
        shell.execute_script(&inner.display().to_string()).unwrap();
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
        let tmp = std::env::temp_dir().join("iocsh_test_dbl_redirect.txt");
        let _ = std::fs::remove_file(&tmp);
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
        let tmp = std::env::temp_dir().join("iocsh_test_append.txt");
        std::fs::write(&tmp, "existing\n").unwrap();
        let line = format!("dbl >> {}", tmp.display());
        let result = shell.execute_line(&line);
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
        let content = std::fs::read_to_string(&tmp).unwrap();
        assert!(content.starts_with("existing\n"));
        assert!(content.contains("TEST_REC"));
        std::fs::remove_file(&tmp).ok();
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
        let tmp = std::env::temp_dir().join("iocsh_test_fd2_redirect.txt");
        let _ = std::fs::remove_file(&tmp);
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

    #[test]
    fn test_parse_redirect() {
        let (cmd, redir) = parse_redirect("dbl > /tmp/out.txt");
        assert_eq!(cmd, "dbl");
        let r = redir.unwrap();
        assert_eq!(r.path, "/tmp/out.txt");
        assert!(!r.append);
        assert_eq!(r.fd, 1, "bare > defaults to fd 1");

        let (cmd, redir) = parse_redirect("dbl >> /tmp/out.txt");
        assert_eq!(cmd, "dbl");
        let r = redir.unwrap();
        assert!(r.append);
        assert_eq!(r.fd, 1);

        let (cmd, redir) = parse_redirect("dbl");
        assert_eq!(cmd, "dbl");
        assert!(redir.is_none());
    }

    /// C parity (iocsh.cpp:287-303): `1>file` and `1>>file` are
    /// fd-numbered variants of `>` and `>>`, and `2>file` requests
    /// stderr capture. The parser MUST accept these forms; bare `dbl
    /// 2>err.log` should not error out the line.
    #[test]
    fn test_parse_redirect_fd_numbered() {
        // 1> equivalent to >
        let (cmd, redir) = parse_redirect("dbl 1>/tmp/out.txt");
        assert_eq!(cmd, "dbl");
        let r = redir.unwrap();
        assert_eq!(r.path, "/tmp/out.txt");
        assert!(!r.append);
        assert_eq!(r.fd, 1);

        // 2> stderr
        let (cmd, redir) = parse_redirect("dbl 2>/tmp/err.txt");
        assert_eq!(cmd, "dbl");
        let r = redir.unwrap();
        assert_eq!(r.path, "/tmp/err.txt");
        assert!(!r.append);
        assert_eq!(r.fd, 2);

        // 2>> stderr append
        let (cmd, redir) = parse_redirect("dbl 2>>/tmp/err.txt");
        assert_eq!(cmd, "dbl");
        let r = redir.unwrap();
        assert_eq!(r.path, "/tmp/err.txt");
        assert!(r.append);
        assert_eq!(r.fd, 2);

        // Digit not at boundary — `cmd5>file` is NOT a fd-redirect;
        // `5` is part of the previous token. Should parse as bare `>`
        // with fd=1, path=file. (The cmd portion includes the trailing
        // `5`; this is a syntax oddity but matches C behavior.)
        let (cmd, redir) = parse_redirect("cmd5>file");
        let r = redir.unwrap();
        assert_eq!(r.fd, 1, "digit not at boundary is part of command");
        assert_eq!(cmd, "cmd5");

        // 9> high fd parses to fd=9
        let (_cmd, redir) = parse_redirect("foo 9>x");
        assert_eq!(redir.unwrap().fd, 9);

        // `0>` does NOT parse as fd-numbered (C only accepts 1..9);
        // it parses as bare `>` with fd=1 leaving `0` in cmd.
        let (cmd, redir) = parse_redirect("foo 0>x");
        let r = redir.unwrap();
        assert_eq!(r.fd, 1);
        assert_eq!(cmd, "foo 0");
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
        assert!(r.is_err(), "duplicate name must return Err");
        // After the rejected call, the original record (val=42.0) is
        // still there, not overwritten. Verify with dbgf which reads
        // the live VAL.
        let r = shell.execute_line("dbgf TEST_REC");
        assert!(matches!(r, Ok(CommandOutcome::Continue)));
    }

    #[test]
    fn test_execute_line_db_create_record_rejects_bad_name() {
        let shell = make_shell();
        // Space inside the name → validate_record_name returns Err.
        // Quote so the parser keeps the space as one argument.
        let r = shell.execute_line("dbCreateRecord pdbbase ai \"BAD NAME\"");
        assert!(r.is_err(), "bad name must return Err");
    }

    #[test]
    fn test_execute_line_db_create_record_rejects_unknown_type() {
        let shell = make_shell();
        let r = shell.execute_line("dbCreateRecord pdbbase nonexistent NEW_REC");
        assert!(r.is_err(), "unknown record type must return Err");
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
        let lines: Vec<String> = join_backslash_continuations(input)
            .into_iter()
            .map(|(_, l)| l)
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
        let out = join_backslash_continuations(input);
        assert_eq!(
            out,
            vec![(1, "a".into()), (2, "b c".into()), (4, "d".into())]
        );
    }

    /// EOF without trailing newline: emit the partial as a final line
    /// (matches `String::lines()` semantics).
    #[test]
    fn test_backslash_continuation_no_trailing_newline() {
        let out = join_backslash_continuations("partial");
        assert_eq!(out, vec![(1, "partial".into())]);
    }

    /// CRLF input must yield the same logical lines as LF (Rust's
    /// `str::lines()` strips the CR for us).
    #[test]
    fn test_backslash_continuation_crlf() {
        let out = join_backslash_continuations("a \\\r\nb\r\n");
        assert_eq!(out, vec![(1, "a b".into())]);
    }

    /// End-to-end: a backslash-continued script line tokenizes and
    /// runs as one logical command.
    #[test]
    fn test_iocsh_script_backslash_continuation_end_to_end() {
        let shell = make_shell();
        let tmp = std::env::temp_dir().join("iocsh_multiline.cmd");
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
        let tmp = std::env::temp_dir().join("iocsh_load_macro_cmd.cmd");
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
        let tmp = std::env::temp_dir().join("iocsh_load_no_macros.cmd");
        std::fs::write(&tmp, "dbl\n").unwrap();
        let line = format!("iocshLoad {}", script_token(&tmp));
        let result = shell.execute_line(&line);
        std::fs::remove_file(&tmp).ok();
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    /// I-R3-1: C runs ONE `macDefExpand` per line over ONE handle that
    /// carries both the environment (`iocsh.cpp:1039` `pairs[]` →
    /// `macCore.c:131-133` `FLAG_USE_ENVIRONMENT`) and the `iocshLoad`
    /// macros pushed onto it (`iocsh.cpp:1118-1119`). The port used two
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
        // and its body must see the pushed scope (C `iocsh.cpp:1239`
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
        shell
            .execute_line(&format!(
                "iocshLoad {} PORT=L0,SUBDIR=sub",
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
            .execute_line(&format!("iocshLoad {}", script_token(&env_only)))
            .unwrap();
        assert_eq!(std::env::var("R3_ENV_ONLY").unwrap(), "/opt/myioc/db");

        // macros empty x env ref present x `<` include at top level.
        shell
            .execute_line(&format!("< {}", script_token(&env_only)))
            .unwrap();
        assert_eq!(std::env::var("R3_ENV_ONLY").unwrap(), "/opt/myioc/db");

        // The pushed scope is popped when the load returns, so `$(PORT)`
        // is undefined again and the line is refused.
        let after = dir.path().join("after.cmd");
        std::fs::write(&after, "epicsEnvSet(\"R3_AFTER\", \"$(PORT)\")\n").unwrap();
        let err = shell
            .execute_script(&after.display().to_string())
            .expect_err("PORT must not survive the iocshLoad scope");
        assert!(err.contains("PORT"), "got: {err}");
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
    /// (`macCore.c:911-913` + `:220`, `macEnv.c:59-61`) and
    /// `iocsh.cpp:1190-1193` skips the line entirely. The port passed
    /// `$(P)` through as literal text and ran the command, installing a
    /// four-character literal as the IOC prefix.
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

        let Err(err) = shell.execute_line("epicsEnvSet(\"R3_PREFIX\", \"$(R3_UNSET)\")") else {
            panic!("an undefined macro must refuse the line");
        };
        assert!(
            err.contains("macLib: macro R3_UNSET is undefined"),
            "got: {err}"
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

    /// A macro value carrying token separators is re-tokenized because
    /// the single expansion runs over the WHOLE line before `split`
    /// (C `iocsh.cpp:1190` then `:1215`). Guards against re-introducing
    /// a per-token expander.
    #[test]
    #[serial_test::serial(epics_env)]
    fn a_macro_value_with_separators_is_split_into_words() {
        let shell = make_shell();
        // SAFETY: serial(epics_env) serialises env-mutating tests.
        unsafe { std::env::set_var("R3_MULTIWORD", "dbpr TEST_REC 2") };
        let expanded = shell.expand_line("$(R3_MULTIWORD) EXTRA").unwrap();
        assert_eq!(
            registry::tokenize(&expanded),
            vec!["dbpr", "TEST_REC", "2", "EXTRA"]
        );
        unsafe { std::env::remove_var("R3_MULTIWORD") };
    }

    /// Missing required `<path>` arg surfaces an error to the caller.
    #[test]
    fn test_iocsh_load_missing_path_errors() {
        let shell = make_shell();
        let result = shell.execute_line("iocshLoad");
        assert!(result.is_err());
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
        let db_path = std::env::temp_dir().join("iocsh_dup_load.db");
        std::fs::write(&db_path, "record(mbbo, \"TEST_REC\") {}\n").unwrap();
        let script_path = std::env::temp_dir().join("iocsh_dup_load.cmd");
        std::fs::write(
            &script_path,
            format!("dbLoadRecords {}\n", script_token(&db_path)),
        )
        .unwrap();
        let result = shell.execute_script(script_path.to_str().unwrap());
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&script_path);
        assert!(
            result.is_err(),
            "dbLoadRecords with type-mismatched duplicate must propagate Err"
        );
    }

    /// C++-style call `iocshLoad("path", "K=V,...")` must tokenize to
    /// the same args as the space form — quotes around the macro
    /// string protect the comma so it stays one token.
    #[test]
    fn test_iocsh_load_cpp_paren_syntax() {
        let shell = make_shell();
        let tmp = std::env::temp_dir().join("iocsh_load_paren.cmd");
        std::fs::write(&tmp, "$(CMD)\n").unwrap();
        let line = format!("iocshLoad(\"{}\", \"CMD=dbl\")", tmp.display());
        let result = shell.execute_line(&line);
        std::fs::remove_file(&tmp).ok();
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    /// Per-line errors during an iocshLoad must not abort the rest of
    /// the script (matches the `execute_script` semantics) but the
    /// final result is `Err` so callers detect a non-zero exit.
    #[test]
    fn test_iocsh_load_per_line_errors_continue_and_propagate() {
        let shell = make_shell();
        let tmp = std::env::temp_dir().join("iocsh_load_err.cmd");
        std::fs::write(&tmp, "nonexistent_cmd\ndbl\n").unwrap();
        let line = format!("iocshLoad {}", script_token(&tmp));
        let result = shell.execute_line(&line);
        std::fs::remove_file(&tmp).ok();
        assert!(
            result.is_err(),
            "iocshLoad with bad command must surface Err"
        );
    }

    #[test]
    fn test_execute_line_db_create_record_missing_args() {
        let shell = make_shell();
        // C accepts the call and lets the body diagnose it:
        // `dbStaticIocRegister.c:292-296` reports
        // `S_dbLib_recordNameMissing` on stderr and sets the shell
        // error, which is what Err carries here.
        match shell.execute_line("dbCreateRecord pdbbase ai") {
            Err(msg) => assert_eq!(msg, "Record name is required"),
            Ok(_) => panic!("a missing record name must fail the command"),
        }
    }

    /// epics-base 8-D `c0da3dd`: `format_error` emits a bold-red
    /// `Error:` prefix when color is on, plain `Error:` otherwise.
    #[test]
    fn test_format_error_with_and_without_color() {
        let plain = format_error("oops", false);
        assert_eq!(plain, "Error: oops");
        let colored = format_error("oops", true);
        assert!(colored.starts_with("\x1b[1;31mError:\x1b[0m "));
        assert!(colored.contains("oops"));
    }

    /// `iocshCmd("dbl")` runs a single command line by
    /// re-entering the shell.
    #[test]
    fn test_iocsh_cmd_runs_single_command() {
        let shell = make_shell();
        let result = shell.execute_line(r#"iocshCmd("dbl")"#);
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    /// `iocshRun` runs `;`-separated commands.
    #[test]
    fn test_iocsh_run_runs_multiple_commands() {
        let shell = make_shell();
        let result = shell.execute_line(r#"iocshRun("dbl; pwd")"#);
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
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

    /// `dbsr` is the Database Server Report, not the name search.
    #[test]
    fn test_dbsr_is_server_report() {
        let shell = make_shell();
        let tmp = std::env::temp_dir().join("iocsh_dbsr_report.txt");
        let _ = std::fs::remove_file(&tmp);
        let line = format!("dbsr > {}", tmp.display());
        assert!(matches!(
            shell.execute_line(&line),
            Ok(CommandOutcome::Continue)
        ));
        let content = std::fs::read_to_string(&tmp).unwrap();
        assert!(
            content.contains("Database Server Report"),
            "dbsr must print the server report, got: {content}"
        );
        // The server report must NOT be a record listing.
        assert!(
            !content.contains("Total:") || content.contains("Total channels"),
            "dbsr must not be the dbgrep name-search output"
        );
        std::fs::remove_file(&tmp).ok();
    }

    /// `on error break` stops the script at the first failing
    /// line. Without it (default `continue`) the whole script runs.
    #[test]
    fn test_on_error_break_stops_script() {
        let shell = make_shell();
        let tmp = std::env::temp_dir().join("iocsh_on_error_break.cmd");
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

    /// C `iocsh.cpp:1040` declares `iocshScope scope;` as a fresh
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

        let result = shell.execute_script(&outer.display().to_string());
        assert!(result.is_err(), "the failing line must still be reported");
        assert!(
            out.exists(),
            "the include's 'on error break' must not stop the caller's script"
        );
        assert!(shell.scopes.borrow().is_empty(), "scope ticket released");
    }

    /// C `iocsh.cpp:1085-1086`: "use of iocshCmd() implies 'on error
    /// break'" — and it is a scope of its own, so the forced mode does
    /// not reach the calling script's next line.
    #[test]
    fn iocsh_run_breaks_at_the_first_failure_without_leaking_the_mode() {
        let shell = make_shell();
        let dir = tempfile::tempdir().unwrap();
        let inside = dir.path().join("inside.txt");
        let after = dir.path().join("after.txt");
        let script = dir.path().join("st.cmd");
        std::fs::write(
            &script,
            format!(
                "iocshRun \"nonexistent_cmd; dbl > {}\"\nnonexistent_cmd\ndbl > {}\n",
                inside.display(),
                after.display()
            ),
        )
        .unwrap();

        let result = shell.execute_script(&script.display().to_string());
        assert!(result.is_err(), "both failures must be reported");
        assert!(
            !inside.exists(),
            "iocshRun's implied 'on error break' must stop at the first failure"
        );
        assert!(
            after.exists(),
            "the implied break must not outlive the iocshRun scope"
        );
    }

    /// C parses the `on error wait` delay with `epicsParseDouble`
    /// (`iocsh.cpp:1560-1561`), so a fractional delay is legal, and
    /// `Halt` with a positive timeout stalls and CONTINUES the script
    /// (`:1146-1148`) rather than unwinding it.
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
        let result = shell.execute_script(&script.display().to_string());
        let elapsed = start.elapsed();
        assert!(result.is_err(), "the failing line must still be reported");
        assert!(
            elapsed >= std::time::Duration::from_millis(200),
            "a 0.25 s wait must actually stall: {elapsed:?}"
        );
        assert!(out.exists(), "'wait' continues the script after the stall");
    }

    /// C `iocsh.cpp:1138-1141`: `Halt` with a non-positive timeout calls
    /// `epicsThreadSuspendSelf()`. Suspending the thread with the process
    /// alive is the point of `halt`; unwinding the script is not the
    /// same thing.
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
    }

    /// Boundary table for `onCallFunc` (`iocsh.cpp:1532-1575`): no scope
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

        let _scope = shell.enter_scope(IocshScope::script());
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
            shell.execute_line("on error bogus").is_err(),
            "an unrecognised mode is the one case C flags as an error"
        );
    }

    /// single-quoted arguments tokenize as one token.
    #[test]
    fn test_single_quote_tokenization() {
        assert_eq!(
            tokenize("dbpf REC:VAL 'hello world'"),
            vec!["dbpf", "REC:VAL", "hello world"]
        );
        assert_eq!(tokenize("cmd('a, b', c)"), vec!["cmd", "a, b", "c"]);
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
