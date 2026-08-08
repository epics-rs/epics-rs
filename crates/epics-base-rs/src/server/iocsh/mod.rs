// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that hand-builds its own tokio runtime; runs and passes in the feature-ON suite.
mod access_commands;
mod commands;
mod core_commands;
pub mod registry;

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

/// Error-handling mode set by the `on error` command.
/// Mirrors C `iocsh.cpp` `onCallFunc` (`continue` / `break` / `halt` /
/// `wait`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum OnError {
    /// Default: report the error and run the next line.
    #[default]
    Continue,
    /// Stop the script at the first failing line (return its error).
    Break,
    /// Like `break` — C `halt` aborts; in-process this is equivalent
    /// to stopping the current script with the failing error.
    Halt,
    /// Pause `delay` seconds after a failing line, then continue.
    Wait(u64),
}

/// Interactive IOC shell with extensible command registration.
pub struct IocShell {
    registry: Arc<RwLock<CommandRegistry>>,
    ctx: CommandContext,
    /// Error-handling mode for the running script. `Cell`
    /// because the shell drives one script at a time on a single
    /// thread; the `on error` command mutates it mid-script.
    on_error: std::cell::Cell<OnError>,
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
            on_error: std::cell::Cell::new(OnError::Continue),
        }
    }

    /// Register an additional command (thread-safe, takes &self).
    pub fn register(&self, def: CommandDef) {
        self.registry.write().unwrap().register(def);
    }

    /// Execute a single line of input.
    ///
    /// Supports C EPICS iocsh output redirection:
    /// - `command > file` — redirect stdout to file (overwrite)
    /// - `command >> file` — redirect stdout to file (append)
    pub fn execute_line(&self, line: &str) -> CommandResult {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Ok(CommandOutcome::Continue);
        }

        // Handle `< filename` include syntax (no macro substitution).
        if let Some(rest) = line.strip_prefix('<') {
            let filename = registry::substitute_env_vars(rest.trim());
            return self
                .execute_script(&filename)
                .map(|_| CommandOutcome::Continue);
        }

        // Handle `iocshLoad <path> [macros]` (Issue #847): include with
        // macro substitution applied to each line of the script. `<`
        // lacks macro support so a separate dispatch is required;
        // intercepting before registry lookup lets the loaded
        // script's own lines re-enter `execute_line` (supporting
        // `<` / `iocshLoad` / redirects / registered commands
        // recursively). `tokenize` already runs `substitute_env_vars`
        // on each token so we use the substituted path and macros
        // string directly.
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
                    return self.execute_line(cmd);
                }
                Some("iocshRun") => {
                    let Some(cmds) = toks.get(1) else {
                        return Err("iocshRun <commands>".into());
                    };
                    let mut last = Ok(CommandOutcome::Continue);
                    for one in cmds.split(';') {
                        let one = one.trim();
                        if one.is_empty() {
                            continue;
                        }
                        match self.execute_line(one) {
                            Ok(CommandOutcome::Exit) => return Ok(CommandOutcome::Exit),
                            Ok(CommandOutcome::Continue) => {}
                            Err(e) => last = Err(e),
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
        // L-5: C `iocsh.cpp` split() flags an unbalanced quote or a
        // trailing backslash and skips the line. Surface the same
        // diagnostic instead of silently tokenizing a malformed line.
        if let Some(diag) = registry::lint_line(line) {
            return Err(diag.to_string());
        }
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

    /// Execute a script with per-line macro substitution applied,
    /// mirroring C `iocshLoad("path", "K=V,...")` (Issue #847).
    /// Macros use `$(KEY)` / `${KEY}` syntax via `db_loader::substitute_macros`.
    /// Per-line errors are reported (matching `execute_script`) but
    /// the script continues to the next line.
    pub fn execute_script_with_macros(
        &self,
        path: &str,
        macros: &HashMap<String, String>,
    ) -> Result<(), String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read '{path}': {e}"))?;
        let mut last_err: Option<String> = None;
        for (line_num, line) in join_backslash_continuations(&content) {
            let expanded = if macros.is_empty() {
                line
            } else {
                crate::server::db_loader::substitute_macros(&line, macros)
            };
            // `#-` silent comments are not echoed (iocsh.cpp:1196-1204).
            if echoes_script_line(&expanded) {
                println!("{expanded}");
            }
            match self.execute_line(&expanded) {
                Ok(CommandOutcome::Continue) => {}
                Ok(CommandOutcome::Exit) => {
                    return last_err.map(Err).unwrap_or(Ok(()));
                }
                Err(e) => {
                    eprintln!("{path}:{line_num}: Error: {e}");
                    let formatted = format!("{path}:{line_num}: {e}");
                    // honour `on error break|halt` — stop the
                    // script at the first failing line.
                    if self.react_to_error() {
                        return Err(formatted);
                    }
                    last_err = Some(formatted);
                }
            }
        }
        last_err.map(Err).unwrap_or(Ok(()))
    }

    /// Execute a script file line by line, echoing each line like C++ iocsh.
    ///
    /// C parity (144f975): errors from individual commands are reported but
    /// do not abort execution. The final return value is `Err` if any command
    /// failed — the equivalent of `iocshSetError` propagating a non-zero exit
    /// status to startup-script callers (e.g., automated IOC verification).
    pub fn execute_script(&self, path: &str) -> Result<(), String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read '{}': {}", path, e))?;

        let mut last_err: Option<String> = None;
        for (line_num, line) in join_backslash_continuations(&content) {
            // Echo each logical line (C++ iocsh behavior — continuations
            // are already collapsed so the echo shows the joined line).
            // `#-` silent comments are not echoed (iocsh.cpp:1196-1204).
            if echoes_script_line(&line) {
                println!("{line}");
            }
            match self.execute_line(&line) {
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
    fn handle_on_command(&self, toks: &[String]) -> CommandResult {
        if toks.get(1).map(|s| s.as_str()) != Some("error") {
            return Err("on error continue|break|halt|wait <delay>".into());
        }
        let mode = match toks.get(2).map(|s| s.as_str()) {
            Some("continue") => OnError::Continue,
            Some("break") => OnError::Break,
            Some("halt") => OnError::Halt,
            Some("wait") => {
                let delay = toks
                    .get(3)
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or("on error wait <delay-seconds>")?;
                OnError::Wait(delay)
            }
            _ => return Err("on error continue|break|halt|wait <delay>".into()),
        };
        self.on_error.set(mode);
        Ok(CommandOutcome::Continue)
    }

    /// React to a failing script line per the current `on error`
    /// mode. Returns `true` if the script should stop (with the
    /// error) — `false` to continue to the next line.
    fn react_to_error(&self) -> bool {
        match self.on_error.get() {
            OnError::Continue => false,
            OnError::Break | OnError::Halt => true,
            OnError::Wait(delay) => {
                if delay > 0 {
                    std::thread::sleep(std::time::Duration::from_secs(delay));
                }
                false
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
    let mut in_quote = false;

    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_quote = !in_quote,
            b'>' if !in_quote => {
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
                        path: registry::substitute_env_vars(path),
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
        let _shell = make_shell();
        assert_eq!(
            registry::substitute_env_vars("$(EPICS_VERSION_FULL)"),
            crate::runtime::version::EPICS_VERSION_FULL,
        );
        assert_eq!(
            registry::substitute_env_vars("${ARCH}"),
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
        let result = shell.execute_line("dbgf");
        assert!(result.is_err());
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

    /// epics-base PR #812 — `dbCreateRecord <type> <name>` creates a
    /// new record at runtime through the same factory registry as
    /// `dbLoadRecords`. Verifies the happy path plus three rejection
    /// branches (duplicate name, bad name, unknown record type).
    #[test]
    fn test_execute_line_db_create_record_happy_path() {
        let shell = make_shell();
        let result = shell.execute_line("dbCreateRecord ai NEW:AI");
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
        // Record is now visible via dbl.
        let result = shell.execute_line("dbl ai");
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
    }

    #[test]
    fn test_execute_line_db_create_record_rejects_duplicate() {
        let shell = make_shell();
        // TEST_REC was added by make_shell() — re-creating must fail
        // gracefully (logged via println, returns Continue, not Err).
        shell.execute_line("dbCreateRecord ai TEST_REC").unwrap();
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
        let r = shell.execute_line("dbCreateRecord ai \"BAD NAME\"");
        // The command itself returns Continue (errors are printed),
        // and the record must NOT be in the registry afterward.
        assert!(matches!(r, Ok(CommandOutcome::Continue)));
    }

    #[test]
    fn test_execute_line_db_create_record_rejects_unknown_type() {
        let shell = make_shell();
        let r = shell.execute_line("dbCreateRecord nonexistent NEW_REC");
        assert!(matches!(r, Ok(CommandOutcome::Continue)));
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
        let line = format!("iocshLoad {} CMD=dbl", tmp.display());
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
        let line = format!("iocshLoad {}", tmp.display());
        let result = shell.execute_line(&line);
        std::fs::remove_file(&tmp).ok();
        assert!(matches!(result, Ok(CommandOutcome::Continue)));
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
            format!("dbLoadRecords {}\n", db_path.display()),
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
        let line = format!("iocshLoad {}", tmp.display());
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
        // Both args are required — missing recordName must Err.
        let r = shell.execute_line("dbCreateRecord ai");
        assert!(r.is_err());
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
            "on error break\nnonexistent_cmd\ndbCreateRecord ai SHOULD_NOT_EXIST\n",
        )
        .unwrap();
        let result = shell.execute_script(tmp.to_str().unwrap());
        std::fs::remove_file(&tmp).ok();
        assert!(result.is_err(), "on error break must surface Err");
        // The record from line 3 must NOT have been created.
        assert!(
            shell.execute_line("dbgf SHOULD_NOT_EXIST").is_err(),
            "on error break must stop before line 3 runs"
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
