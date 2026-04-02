mod commands;
pub mod registry;

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

        // Handle `< filename` include syntax
        if let Some(rest) = line.strip_prefix('<') {
            let filename = registry::substitute_env_vars(rest.trim());
            return self
                .execute_script(&filename)
                .map(|_| CommandOutcome::Continue);
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

    /// Execute a script file line by line, echoing each line like C++ iocsh.
    pub fn execute_script(&self, path: &str) -> Result<(), String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read '{}': {}", path, e))?;

        for (line_num, line) in content.lines().enumerate() {
            // Echo each line (C++ iocsh behavior)
            println!("{line}");
            match self.execute_line(line) {
                Ok(CommandOutcome::Continue) => {}
                Ok(CommandOutcome::Exit) => return Ok(()),
                Err(e) => {
                    eprintln!("{}:{}: Error: {}", path, line_num + 1, e);
                }
            }
        }
        Ok(())
    }

    /// Run the interactive REPL. Blocks until exit or EOF.
    pub fn run_repl(&self) -> Result<(), String> {
        let mut rl = rustyline::DefaultEditor::new()
            .map_err(|e| format!("failed to initialize readline: {e}"))?;

        loop {
            match rl.readline("epics> ") {
                Ok(line) => {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    let _ = rl.add_history_entry(&line);

                    match self.execute_line(&line) {
                        Ok(CommandOutcome::Continue) => {}
                        Ok(CommandOutcome::Exit) => break,
                        Err(e) => eprintln!("Error: {e}"),
                    }
                }
                Err(rustyline::error::ReadlineError::Eof) => break,
                Err(rustyline::error::ReadlineError::Interrupted) => continue,
                Err(e) => {
                    eprintln!("readline error: {e}");
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
                .await;
        });
        std::mem::forget(rt);
        IocShell::new(db, handle)
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
}
