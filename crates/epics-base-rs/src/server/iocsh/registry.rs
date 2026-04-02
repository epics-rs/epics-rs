use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use crate::server::database::PvDatabase;

/// Argument type for a command parameter.
#[derive(Debug, Clone)]
pub enum ArgType {
    String,
    Int,
    Double,
}

/// Description of a single command argument.
#[derive(Debug, Clone)]
pub struct ArgDesc {
    pub name: &'static str,
    pub arg_type: ArgType,
    pub optional: bool,
}

/// A parsed argument value.
#[derive(Debug, Clone)]
pub enum ArgValue {
    String(String),
    Int(i64),
    Double(f64),
    Missing,
}

/// Result of executing a command.
pub enum CommandOutcome {
    Continue,
    Exit,
}

/// Command result type.
pub type CommandResult = Result<CommandOutcome, String>;

/// Trait for command handlers.
pub trait CommandHandler: Send + Sync {
    fn call(&self, args: &[ArgValue], ctx: &CommandContext) -> CommandResult;
}

impl<F> CommandHandler for F
where
    F: Fn(&[ArgValue], &CommandContext) -> CommandResult + Send + Sync,
{
    fn call(&self, args: &[ArgValue], ctx: &CommandContext) -> CommandResult {
        self(args, ctx)
    }
}

/// A registered command definition.
pub struct CommandDef {
    pub name: String,
    pub args: Vec<ArgDesc>,
    pub usage: String,
    pub handler: Box<dyn CommandHandler>,
}

impl CommandDef {
    pub fn new(
        name: impl Into<String>,
        args: Vec<ArgDesc>,
        usage: impl Into<String>,
        handler: impl CommandHandler + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            args,
            usage: usage.into(),
            handler: Box::new(handler),
        }
    }
}

/// Sync→async bridge for commands running on a blocking thread.
pub struct CommandContext {
    db: Arc<PvDatabase>,
    handle: tokio::runtime::Handle,
    /// Output writer — defaults to stdout, redirected to a file by `>` / `>>`.
    output: std::cell::RefCell<Box<dyn std::io::Write>>,
}

impl CommandContext {
    pub fn new(db: Arc<PvDatabase>, handle: tokio::runtime::Handle) -> Self {
        Self {
            db,
            handle,
            output: std::cell::RefCell::new(Box::new(std::io::stdout())),
        }
    }

    /// Access the PV database.
    pub fn db(&self) -> &Arc<PvDatabase> {
        &self.db
    }

    /// Access the tokio runtime handle (e.g., for spawning tasks from iocsh commands).
    pub fn runtime_handle(&self) -> &tokio::runtime::Handle {
        &self.handle
    }

    /// Print a line to the current output (stdout or redirected file).
    pub fn println(&self, msg: &str) {
        let mut out = self.output.borrow_mut();
        let _ = writeln!(out, "{msg}");
    }

    /// Print a formatted string to the current output.
    pub fn print_fmt(&self, args: std::fmt::Arguments<'_>) {
        let mut out = self.output.borrow_mut();
        let _ = out.write_fmt(args);
        let _ = writeln!(out);
    }

    /// Temporarily redirect output to a writer, run a closure, then restore.
    pub(crate) fn with_output<W: std::io::Write + 'static, R>(
        &self,
        writer: W,
        f: impl FnOnce() -> R,
    ) -> R {
        let prev = self.output.replace(Box::new(writer));
        let result = f();
        let _ = self.output.borrow_mut().flush();
        self.output.replace(prev);
        result
    }

    /// Run an async future from the blocking REPL thread.
    ///
    /// # Panics
    /// Panics if called from within a tokio runtime thread.
    pub fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "CommandContext::block_on() must not be called from a tokio runtime thread"
        );
        self.handle.block_on(future)
    }
}

/// Registry of all available commands.
pub(crate) struct CommandRegistry {
    commands: HashMap<String, CommandDef>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
        }
    }

    pub fn register(&mut self, def: CommandDef) {
        self.commands.insert(def.name.clone(), def);
    }

    pub fn get(&self, name: &str) -> Option<&CommandDef> {
        self.commands.get(name)
    }

    pub fn list(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.commands.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }
}

/// Tokenize a command line supporting both C++ EPICS and space-separated syntax.
///
/// C++ syntax: `command("arg1", arg2, $(VAR))` — parens delimit args, commas separate.
/// Legacy syntax: `command "arg1" arg2` — whitespace separates.
///
/// `$(VAR)` references are resolved from environment variables in all tokens.
pub(crate) fn tokenize(line: &str) -> Vec<String> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }

    // Find the command name: everything up to first '(' or whitespace
    let mut cmd_end = line.len();
    let mut has_parens = false;
    for (i, ch) in line.char_indices() {
        if ch == '(' {
            cmd_end = i;
            has_parens = true;
            break;
        } else if ch == ' ' || ch == '\t' {
            cmd_end = i;
            break;
        }
    }

    let cmd_name = &line[..cmd_end];
    if cmd_name.is_empty() {
        return Vec::new();
    }

    let mut tokens = vec![substitute_env_vars(cmd_name)];

    if has_parens {
        // C++ syntax: command(arg1, arg2, ...)
        // Find matching closing paren
        let args_start = cmd_end + 1; // skip '('
        let rest = &line[args_start..];
        let paren_end = find_closing_paren(rest);
        let args_str = &rest[..paren_end];

        if !args_str.trim().is_empty() {
            for arg in split_comma_args(args_str) {
                tokens.push(substitute_env_vars(&arg));
            }
        }
    } else {
        // Legacy space-separated syntax
        let rest = &line[cmd_end..];
        for arg in split_space_args(rest) {
            tokens.push(substitute_env_vars(&arg));
        }
    }

    tokens
}

/// Find the closing ')' in a string, respecting quoted strings and `$(...)` sequences.
/// Returns the byte offset of ')' or the string length if not found.
fn find_closing_paren(s: &str) -> usize {
    let mut in_quotes = false;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if in_quotes {
            if ch == b'\\' {
                i += 1; // skip escaped char
            } else if ch == b'"' {
                in_quotes = false;
            }
        } else if ch == b'"' {
            in_quotes = true;
        } else if ch == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'(' {
            // Skip $(...)  — find the matching ')' for the macro ref
            if let Some(end) = bytes[i + 2..].iter().position(|&c| c == b')') {
                i += 2 + end + 1; // skip past the macro's ')'
                continue;
            }
        } else if ch == b')' {
            return i;
        }
        i += 1;
    }
    s.len()
}

/// Split comma-separated arguments, respecting quoted strings.
/// Trims whitespace around each argument and strips outer quotes.
fn split_comma_args(s: &str) -> Vec<String> {
    // First, split on commas respecting quoted strings
    let mut raw_parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '\\' {
                if let Some(&next) = chars.peek() {
                    match next {
                        '"' | '\\' => {
                            current.push(chars.next().unwrap());
                        }
                        _ => {
                            current.push(ch);
                        }
                    }
                } else {
                    current.push(ch);
                }
            } else if ch == '"' {
                in_quotes = false;
                current.push(ch);
            } else {
                current.push(ch);
            }
        } else if ch == '"' {
            in_quotes = true;
            current.push(ch);
        } else if ch == ',' {
            raw_parts.push(std::mem::take(&mut current));
        } else {
            current.push(ch);
        }
    }
    raw_parts.push(current);

    // Now process each part: trim whitespace, then strip outer quotes
    let mut args = Vec::new();
    for part in raw_parts {
        let trimmed = part.trim();
        if trimmed.is_empty() && args.is_empty() {
            continue; // skip leading empty
        }
        if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
            // Strip outer quotes and process escapes
            let inner = &trimmed[1..trimmed.len() - 1];
            let mut val = String::new();
            let mut chs = inner.chars().peekable();
            while let Some(c) = chs.next() {
                if c == '\\' {
                    if let Some(&next) = chs.peek() {
                        match next {
                            '"' | '\\' => {
                                val.push(chs.next().unwrap());
                            }
                            _ => {
                                val.push(c);
                            }
                        }
                    } else {
                        val.push(c);
                    }
                } else {
                    val.push(c);
                }
            }
            args.push(val);
        } else {
            args.push(trimmed.to_string());
        }
    }

    args
}

/// Split space/tab separated arguments, respecting quoted strings.
fn split_space_args(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_token = false;
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '\\' {
                if let Some(&next) = chars.peek() {
                    match next {
                        '"' | '\\' => {
                            current.push(chars.next().unwrap());
                        }
                        _ => {
                            current.push(ch);
                        }
                    }
                } else {
                    current.push(ch);
                }
            } else if ch == '"' {
                in_quotes = false;
            } else {
                current.push(ch);
            }
        } else if ch == '"' {
            in_quotes = true;
            has_token = true;
        } else if ch == ' ' || ch == '\t' {
            if has_token {
                args.push(std::mem::take(&mut current));
                has_token = false;
            }
        } else {
            current.push(ch);
            has_token = true;
        }
    }

    if has_token {
        args.push(current);
    }

    args
}

/// Substitute `$(NAME)` references with environment variable values.
pub(crate) fn substitute_env_vars(s: &str) -> String {
    if !s.contains("$(") {
        return s.to_string();
    }
    let mut result = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if i + 1 < chars.len() && chars[i] == '$' && chars[i + 1] == '(' {
            if let Some(end) = chars[i + 2..].iter().position(|&c| c == ')') {
                let var_expr: String = chars[i + 2..i + 2 + end].iter().collect();
                // Support $(VAR=DEFAULT) syntax
                let (var_name, default_val) = if let Some(eq_pos) = var_expr.find('=') {
                    (&var_expr[..eq_pos], Some(&var_expr[eq_pos + 1..]))
                } else {
                    (var_expr.as_str(), None)
                };
                if let Some(val) = crate::runtime::env::get(var_name) {
                    result.push_str(&val);
                } else if let Some(def) = default_val {
                    result.push_str(def);
                } else {
                    result.push_str(&format!("$({})", var_expr));
                }
                i += 2 + end + 1;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

/// Parse tokens into argument values according to argument descriptors.
pub(crate) fn parse_args(tokens: &[String], descs: &[ArgDesc]) -> Result<Vec<ArgValue>, String> {
    let mut result = Vec::with_capacity(descs.len());

    for (i, desc) in descs.iter().enumerate() {
        if i < tokens.len() {
            let token = &tokens[i];
            let val = match desc.arg_type {
                ArgType::String => ArgValue::String(token.clone()),
                ArgType::Int => token.parse::<i64>().map(ArgValue::Int).map_err(|_| {
                    format!(
                        "argument '{}': expected integer, got '{}'",
                        desc.name, token
                    )
                })?,
                ArgType::Double => token.parse::<f64>().map(ArgValue::Double).map_err(|_| {
                    format!("argument '{}': expected number, got '{}'", desc.name, token)
                })?,
            };
            result.push(val);
        } else if desc.optional {
            result.push(ArgValue::Missing);
        } else {
            return Err(format!("missing required argument '{}'", desc.name));
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Legacy space-separated syntax ---

    #[test]
    fn test_tokenize_simple() {
        assert_eq!(tokenize("dbl"), vec!["dbl"]);
        assert_eq!(tokenize("dbgf TEMP.VAL"), vec!["dbgf", "TEMP.VAL"]);
    }

    #[test]
    fn test_tokenize_quoted() {
        assert_eq!(
            tokenize(r#"dbpf TEMP "42.0""#),
            vec!["dbpf", "TEMP", "42.0"]
        );
    }

    #[test]
    fn test_tokenize_escaped_quotes() {
        assert_eq!(
            tokenize(r#"cmd "hello \"world\"""#),
            vec!["cmd", r#"hello "world""#]
        );
    }

    #[test]
    fn test_tokenize_escaped_backslash() {
        assert_eq!(tokenize(r#"cmd "a\\b""#), vec!["cmd", r#"a\b"#]);
    }

    #[test]
    fn test_tokenize_empty() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("   ").is_empty());
    }

    #[test]
    fn test_tokenize_trailing_whitespace() {
        assert_eq!(tokenize("dbl   "), vec!["dbl"]);
    }

    // --- C++ EPICS function-call syntax ---

    #[test]
    fn test_tokenize_cpp_basic() {
        assert_eq!(
            tokenize(r#"epicsEnvSet("PREFIX", "SIM1:")"#),
            vec!["epicsEnvSet", "PREFIX", "SIM1:"]
        );
    }

    #[test]
    fn test_tokenize_cpp_mixed_types() {
        assert_eq!(
            tokenize(r#"simDetectorConfig("SIM1", 256, 256, 50000000)"#),
            vec!["simDetectorConfig", "SIM1", "256", "256", "50000000"]
        );
    }

    #[test]
    fn test_tokenize_cpp_no_args() {
        assert_eq!(tokenize("iocInit()"), vec!["iocInit"]);
    }

    #[test]
    fn test_tokenize_cpp_spaces_around_commas() {
        assert_eq!(
            tokenize(r#"cmd( "a" , "b" , 3 )"#),
            vec!["cmd", "a", "b", "3"]
        );
    }

    #[test]
    fn test_tokenize_cpp_env_var() {
        unsafe { std::env::set_var("_TEST_TOK_VAR", "HELLO") };
        assert_eq!(
            tokenize(r#"cmd("$(_TEST_TOK_VAR)", $(_TEST_TOK_VAR))"#),
            vec!["cmd", "HELLO", "HELLO"]
        );
        unsafe { std::env::remove_var("_TEST_TOK_VAR") };
    }

    #[test]
    fn test_tokenize_cpp_env_var_unset() {
        // Unset env vars kept as $(NAME)
        assert_eq!(
            tokenize(r#"cmd($(UNLIKELY_VAR_XYZ))"#),
            vec!["cmd", "$(UNLIKELY_VAR_XYZ)"]
        );
    }

    #[test]
    fn test_tokenize_cpp_dbloadrecords() {
        // Matches real C++ EPICS syntax
        assert_eq!(
            tokenize(r#"dbLoadRecords("path/to/file.db","P=SIM1:,R=cam1:")"#),
            vec!["dbLoadRecords", "path/to/file.db", "P=SIM1:,R=cam1:"]
        );
    }

    #[test]
    fn test_tokenize_cpp_quoted_with_parens_inside() {
        // Parens inside quotes should not confuse the parser
        assert_eq!(
            tokenize(r#"cmd("hello(world)")"#),
            vec!["cmd", "hello(world)"]
        );
    }

    #[test]
    fn test_parse_args_required() {
        let descs = vec![ArgDesc {
            name: "name",
            arg_type: ArgType::String,
            optional: false,
        }];
        let tokens = vec!["TEMP".to_string()];
        let result = parse_args(&tokens, &descs).unwrap();
        assert!(matches!(&result[0], ArgValue::String(s) if s == "TEMP"));
    }

    #[test]
    fn test_parse_args_optional_missing() {
        let descs = vec![ArgDesc {
            name: "type",
            arg_type: ArgType::String,
            optional: true,
        }];
        let result = parse_args(&[], &descs).unwrap();
        assert!(matches!(&result[0], ArgValue::Missing));
    }

    #[test]
    fn test_parse_args_missing_required() {
        let descs = vec![ArgDesc {
            name: "name",
            arg_type: ArgType::String,
            optional: false,
        }];
        let result = parse_args(&[], &descs);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_args_int() {
        let descs = vec![ArgDesc {
            name: "level",
            arg_type: ArgType::Int,
            optional: false,
        }];
        let tokens = vec!["42".to_string()];
        let result = parse_args(&tokens, &descs).unwrap();
        assert!(matches!(&result[0], ArgValue::Int(42)));
    }

    #[test]
    fn test_parse_args_int_invalid() {
        let descs = vec![ArgDesc {
            name: "level",
            arg_type: ArgType::Int,
            optional: false,
        }];
        let tokens = vec!["abc".to_string()];
        assert!(parse_args(&tokens, &descs).is_err());
    }

    #[test]
    fn test_parse_args_double() {
        let descs = vec![ArgDesc {
            name: "value",
            arg_type: ArgType::Double,
            optional: false,
        }];
        let tokens = vec!["3.14".to_string()];
        let result = parse_args(&tokens, &descs).unwrap();
        match &result[0] {
            ArgValue::Double(v) => assert!((*v - 3.14).abs() < 1e-10),
            other => panic!("expected Double, got {:?}", other),
        }
    }

    #[test]
    fn test_registry_basic() {
        let mut reg = CommandRegistry::new();
        reg.register(CommandDef::new(
            "test",
            vec![],
            "test command",
            |_args: &[ArgValue], _ctx: &CommandContext| Ok(CommandOutcome::Continue),
        ));
        assert!(reg.get("test").is_some());
        assert!(reg.get("nonexistent").is_none());
        assert_eq!(reg.list(), vec!["test"]);
    }
}
