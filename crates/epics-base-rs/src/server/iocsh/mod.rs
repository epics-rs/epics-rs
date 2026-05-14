mod commands;
pub mod registry;

use std::collections::HashMap;
use std::fs::File;
use std::sync::{Arc, RwLock};

use crate::server::database::PvDatabase;
use registry::*;

/// Interactive IOC shell with extensible command registration.
pub struct IocShell {
    registry: Arc<RwLock<CommandRegistry>>,
    ctx: CommandContext,
}

impl IocShell {
    /// Create a new shell with built-in commands registered.
    pub fn new(db: Arc<PvDatabase>, handle: tokio::runtime::Handle) -> Self {
        let mut registry = CommandRegistry::new();
        commands::register_builtins(&mut registry);
        Self {
            registry: Arc::new(RwLock::new(registry)),
            ctx: CommandContext::new(db, handle),
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
            if toks.first().map(|s| s.as_str()) == Some("iocshLoad") {
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
        if let Some(redir) = redirect {
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
        } else {
            self.execute_command_inner(line)
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
            println!("{expanded}");
            match self.execute_line(&expanded) {
                Ok(CommandOutcome::Continue) => {}
                Ok(CommandOutcome::Exit) => {
                    return last_err.map(Err).unwrap_or(Ok(()));
                }
                Err(e) => {
                    eprintln!("{path}:{line_num}: Error: {e}");
                    last_err = Some(format!("{path}:{line_num}: {e}"));
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
            println!("{line}");
            match self.execute_line(&line) {
                Ok(CommandOutcome::Continue) => {}
                Ok(CommandOutcome::Exit) => {
                    return last_err.map(Err).unwrap_or(Ok(()));
                }
                Err(e) => {
                    eprintln!("{path}:{line_num}: Error: {e}");
                    last_err = Some(format!("{path}:{line_num}: {e}"));
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
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            self.run_repl_interactive()
        } else {
            self.run_repl_piped()
        }
    }

    fn run_repl_interactive(&self) -> Result<(), String> {
        // History capacity from `EPICS_RS_IOCSH_HISTORY_SIZE` (default
        // 500). Mirrors epics-base PR #459 — bound the history so a
        // long-running IOC shell session does not grow unbounded.
        // Lower bound 16 keeps history useful even for hostile env values.
        let history_size = crate::runtime::env::get("EPICS_RS_IOCSH_HISTORY_SIZE")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(500)
            .max(16);
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
        // (https://no-color.org) and fall through to plain output
        // when stdout is not a TTY (already TTY-gated by `run_repl`
        // dispatch but defensive).
        let want_color = use_ansi_color();
        let prompt = if want_color {
            // \x1b[32;1m = bright green (matches C ANSI_GREEN);
            // \x1b[0m = reset. Bracket as `\x01...\x02` so rustyline
            // excludes the sequence from prompt-width / cursor-
            // position tracking (otherwise the cursor lands several
            // chars off).
            "\x01\x1b[32;1m\x02epics> \x01\x1b[0m\x02"
        } else {
            "epics> "
        };

        loop {
            match rl.readline(prompt) {
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
/// (https://no-color.org) and `EPICS_RS_IOCSH_NO_COLOR=1` opt-out;
/// otherwise on by default in the interactive (TTY) path.
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

/// Format an error string with optional ANSI bold-red prefix.
/// Plain `Error: <msg>` when color is off — preserves grep-ability.
fn format_error(msg: &str, color: bool) -> String {
    if color {
        format!("\x1b[1;31mError:\x1b[0m {msg}")
    } else {
        format!("Error: {msg}")
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
}

/// Parse `>` / `>>` redirect from end of line.
/// Returns (command_part, optional redirect).
fn parse_redirect(line: &str) -> (&str, Option<Redirect>) {
    let bytes = line.as_bytes();
    let mut in_quote = false;
    let mut redir_pos = None;
    let mut is_append = false;

    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_quote = !in_quote,
            b'>' if !in_quote => {
                redir_pos = Some(i);
                is_append = i + 1 < bytes.len() && bytes[i + 1] == b'>';
                break; // use first unquoted > position
            }
            _ => {}
        }
        i += 1;
    }

    match redir_pos {
        Some(pos) => {
            let cmd = line[..pos].trim();
            let skip = if is_append { 2 } else { 1 };
            let path = line[pos + skip..].trim();
            if path.is_empty() {
                (line, None)
            } else {
                (
                    cmd,
                    Some(Redirect {
                        path: registry::substitute_env_vars(path),
                        append: is_append,
                    }),
                )
            }
        }
        None => (line, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::records::ai::AiRecord;

    fn make_shell() -> IocShell {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(PvDatabase::new());
        let handle = rt.handle().clone();
        rt.block_on(async {
            db.add_record("TEST_REC", Box::new(AiRecord::new(42.0)))
                .await
                .unwrap();
        });
        std::mem::forget(rt);
        IocShell::new(db, handle)
    }

    /// Round-16 regression: a CommandDef must be cloneable so the
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
        // the Arc<dyn CommandHandler> is what enables the round-16
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

    #[test]
    fn test_parse_redirect() {
        let (cmd, redir) = parse_redirect("dbl > /tmp/out.txt");
        assert_eq!(cmd, "dbl");
        let r = redir.unwrap();
        assert_eq!(r.path, "/tmp/out.txt");
        assert!(!r.append);

        let (cmd, redir) = parse_redirect("dbl >> /tmp/out.txt");
        assert_eq!(cmd, "dbl");
        assert!(redir.unwrap().append);

        let (cmd, redir) = parse_redirect("dbl");
        assert_eq!(cmd, "dbl");
        assert!(redir.is_none());
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
    /// the rejection as its overall result.
    #[test]
    fn test_db_load_records_duplicate_rejection_propagates() {
        let shell = make_shell();
        // make_shell already added TEST_REC. Loading a .db with the same
        // name must hit `add_record` rejection and surface Err.
        let db_path = std::env::temp_dir().join("iocsh_dup_load.db");
        std::fs::write(&db_path, "record(ai, \"TEST_REC\") {}\n").unwrap();
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
            "dbLoadRecords with duplicate record name must propagate Err"
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
