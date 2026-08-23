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
///
/// `handler` is `Arc`-backed so a `CommandDef` can be cloned and
/// re-registered on a fresh `IocShell` (used by the
/// `afterIocRunning` post-init shell — without Clone, custom
/// site-specific commands registered via
/// `IocApplication::register_shell_command` would be unavailable
/// in the post-init queue).
#[derive(Clone)]
pub struct CommandDef {
    pub name: String,
    pub args: Vec<ArgDesc>,
    pub usage: String,
    pub handler: Arc<dyn CommandHandler>,
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
            handler: Arc::new(handler),
        }
    }
}

/// Sync→async bridge for commands running on a blocking thread.
pub struct CommandContext {
    db: Arc<PvDatabase>,
    bridge: crate::runtime::task::BlockingBridge,
    /// The IOC's live Access Security policy cell. `asInit` stores the
    /// parsed ACF here and the `as*` inspection commands read it — the
    /// same cell the IOC's protocol servers gate on when the shell was
    /// built by a server-owning root ([`CommandContext::new_with_acf`]).
    /// [`CommandContext::new`] creates a fresh unobserved cell for
    /// standalone shells that administer no server.
    acf: crate::server::access_security::AcfCell,
    /// Output writer — defaults to stdout, redirected to a file by `>` / `>>`.
    output: std::cell::RefCell<Box<dyn std::io::Write>>,
}

impl CommandContext {
    pub fn new(db: Arc<PvDatabase>, bridge: crate::runtime::task::BlockingBridge) -> Self {
        Self::new_with_acf(
            db,
            bridge,
            crate::server::access_security::new_acf_cell(None),
        )
    }

    /// Build a context that administers `acf` — the policy cell the
    /// owning IOC's servers enforce, so `asInit` from this shell is a
    /// live (re)load rather than a dead-end copy.
    pub fn new_with_acf(
        db: Arc<PvDatabase>,
        bridge: crate::runtime::task::BlockingBridge,
        acf: crate::server::access_security::AcfCell,
    ) -> Self {
        Self {
            db,
            bridge,
            acf,
            output: std::cell::RefCell::new(Box::new(std::io::stdout())),
        }
    }

    /// Access the PV database.
    pub fn db(&self) -> &Arc<PvDatabase> {
        &self.db
    }

    /// The IOC's live Access Security policy cell.
    pub fn acf(&self) -> &crate::server::access_security::AcfCell {
        &self.acf
    }

    /// The captured runtime access — for spawning tasks or blocking on async
    /// work from iocsh command handlers (which run on the blocking shell
    /// thread, where the tokio backend's runtime is otherwise unreachable).
    pub fn bridge(&self) -> &crate::runtime::task::BlockingBridge {
        &self.bridge
    }

    /// Print a line to the current output (stdout or redirected file).
    pub fn println(&self, msg: &str) {
        let mut out = self.output.borrow_mut();
        let _ = writeln!(out, "{msg}");
    }

    /// Print raw BYTES plus a newline to the current output.
    ///
    /// C's `echo` (`libComRegister.c:84-91`) hands `dbTranslateEscape`'s output
    /// straight to `printf("%s")` — bytes, not characters — so `echo "\xff"`
    /// emits the single byte 0xFF. Routing that through [`Self::println`] would
    /// force it through a Rust `str` and replace it.
    pub fn println_bytes(&self, bytes: &[u8]) {
        let mut out = self.output.borrow_mut();
        let _ = out.write_all(bytes);
        let _ = out.write_all(b"\n");
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
        self.bridge.block_on(future)
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
/// C parity (`iocsh.cpp:1190` `macDefExpand` → `:1215` `tokenize.split`):
/// macros are expanded across the WHOLE line *before* it is split into
/// words, and `split` itself expands nothing. So a macro whose value
/// contains a separator (`$(CMD)` with `CMD="dbpr REC 2"`) is
/// re-tokenized into multiple words, and a leading `$(MACRO)` at command
/// position resolves before the command name is taken. The caller
/// ([`super::IocShell::execute_line`]) owns that one expansion; this
/// function must not perform a second one.
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

    // The whole line was expanded once by the caller, so the tokens are
    // pushed verbatim (no per-token substitution).
    let mut tokens = vec![cmd_name.to_string()];

    if has_parens {
        // C++ syntax: command(arg1, arg2, ...)
        // Find matching closing paren
        let args_start = cmd_end + 1; // skip '('
        let rest = &line[args_start..];
        let paren_end = find_closing_paren(rest);
        let args_str = &rest[..paren_end];

        if !args_str.trim().is_empty() {
            for arg in split_comma_args(args_str) {
                tokens.push(arg);
            }
        }
    } else {
        // Legacy space-separated syntax
        let rest = &line[cmd_end..];
        for arg in split_space_args(rest) {
            tokens.push(arg);
        }
    }

    tokens
}

/// Find the closing ')' in a string, respecting quoted strings, `$(...)`
/// macro references, and `${...}` macro references (which C macLib treats
/// equivalently — see macCore.c:777). A `)` inside a `${...}` body (e.g.
/// `${foo(bar)}`) must NOT be mistaken for the outer call's closing paren.
/// Returns the byte offset of ')' or the string length if not found.
///
/// Quoting and escaping follow the same rules as `lint_line` and the
/// splitters, so the three scanners agree on where the call ends: both
/// `"` and `'` quote, and a backslash escapes the next character in and
/// out of quotes (C split(), measured: `echo(a\))` prints `a)`,
/// `echo('a)b')` prints `a)b`).
fn find_closing_paren(s: &str) -> usize {
    // `0` = not in a quote; otherwise the opening quote byte.
    let mut quote = 0u8;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch == b'\\' {
            i += 2; // skip the escaped char too
            continue;
        }
        if quote != 0 {
            if ch == quote {
                quote = 0;
            }
        } else if ch == b'"' || ch == b'\'' {
            quote = ch;
        } else if ch == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'(' {
            // Skip $(...) — find the matching ')' for the macro ref
            if let Some(end) = bytes[i + 2..].iter().position(|&c| c == b')') {
                i += 2 + end + 1; // skip past the macro's ')'
                continue;
            }
        } else if ch == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            // Skip ${...} — find the matching '}' for the macro ref so any
            // ')' inside the macro body doesn't terminate the outer call.
            if let Some(end) = bytes[i + 2..].iter().position(|&c| c == b'}') {
                i += 2 + end + 1; // skip past the macro's '}'
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
///
/// both `"` and `'` open a quoted string; the quote is closed
/// only by the *same* character it was opened with — matching C
/// `iocsh.cpp` `split()` (`if ((c == '"') || (c == '\'')) quote = c;`).
/// Outside a quote a backslash consumes itself and takes the next
/// character literally (`iocsh.cpp:275-278,326`): `\,` does not split,
/// `\"` does not open a quote. Escapes are interpreted in the first
/// pass, exactly once; the second pass only trims and strips the outer
/// quotes of a part whose first non-blank character was a *functional*
/// quote — an escape-produced quote is data and stays. (The previous
/// shape re-ran escape processing in the second pass, collapsing
/// `"a\\\\b"` twice.)
fn split_comma_args(s: &str) -> Vec<String> {
    // First, split on commas respecting quoted strings. `opens_quoted`
    // remembers whether the part's first non-blank char was a functional
    // opening quote — the only parts the second pass may strip.
    let mut raw_parts: Vec<(String, bool)> = Vec::new();
    let mut current = String::new();
    let mut opens_quoted = false;
    // `0` = not in a quote; otherwise the opening quote char.
    let mut quote: char = '\0';
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if quote != '\0' {
            if ch == '\\' {
                if let Some(&next) = chars.peek() {
                    match next {
                        '"' | '\'' | '\\' => {
                            current.push(chars.next().unwrap());
                        }
                        _ => {
                            current.push(ch);
                        }
                    }
                } else {
                    current.push(ch);
                }
            } else if ch == quote {
                quote = '\0';
                current.push(ch);
            } else {
                current.push(ch);
            }
        } else if ch == '\\' {
            // C split() outside a quote: the backslash is consumed and
            // the next character is literal — it neither splits nor
            // opens a quote. A trailing backslash is lint_line's
            // "Trailing backslash." and never gets here.
            if let Some(next) = chars.next() {
                current.push(next);
            }
        } else if ch == '"' || ch == '\'' {
            if current.chars().all(char::is_whitespace) {
                opens_quoted = true;
            }
            quote = ch;
            current.push(ch);
        } else if ch == ',' {
            raw_parts.push((std::mem::take(&mut current), opens_quoted));
            opens_quoted = false;
        } else {
            current.push(ch);
        }
    }
    raw_parts.push((current, opens_quoted));

    // Now process each part: trim whitespace, then strip outer quotes.
    let mut args = Vec::new();
    for (part, opens_quoted) in raw_parts {
        let trimmed = part.trim();
        if trimmed.is_empty() && args.is_empty() {
            continue; // skip leading empty
        }
        let outer_quote = trimmed
            .chars()
            .next()
            .filter(|c| (*c == '"' || *c == '\'') && opens_quoted);
        if let Some(q) = outer_quote {
            if trimmed.len() >= 2 && trimmed.ends_with(q) {
                args.push(trimmed[1..trimmed.len() - 1].to_string());
                continue;
            }
        }
        args.push(trimmed.to_string());
    }

    args
}

/// Split space/tab separated arguments, respecting quoted strings.
///
/// both `"` and `'` delimit a quoted string; the quote is closed
/// only by the matching character — mirrors C `iocsh.cpp` `split()`.
/// Outside a quote a backslash consumes itself and takes the next
/// character literally (`iocsh.cpp:275-278,326`): `echo \"hello\"`
/// yields the token `"hello"`, `a\ b` is one token `a b`.
fn split_space_args(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    // `0` = not in a quote; otherwise the opening quote char.
    let mut quote: char = '\0';
    let mut has_token = false;
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if quote != '\0' {
            if ch == '\\' {
                if let Some(&next) = chars.peek() {
                    match next {
                        '"' | '\'' | '\\' => {
                            current.push(chars.next().unwrap());
                        }
                        _ => {
                            current.push(ch);
                        }
                    }
                } else {
                    current.push(ch);
                }
            } else if ch == quote {
                quote = '\0';
            } else {
                current.push(ch);
            }
        } else if ch == '\\' {
            // C split() outside a quote: consume the backslash, take the
            // next character literally — a `\"` is data, a `\ ` is not a
            // separator. A trailing backslash is lint_line's
            // "Trailing backslash." and never gets here.
            if let Some(next) = chars.next() {
                current.push(next);
                has_token = true;
            }
        } else if ch == '"' || ch == '\'' {
            quote = ch;
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

/// Scan a command line for the malformed-input conditions C
/// `iocsh.cpp` `split()` (lines 362-371) flags: an unbalanced quote
/// (`"` or `'`) and a trailing backslash. Returns a human-readable
/// diagnostic for the first problem found, or `None` if the line is
/// well-formed. L-5: C marks such a line errored; the Rust tokenizer
/// previously consumed them silently.
pub(crate) fn lint_line(line: &str) -> Option<&'static str> {
    let mut scan = ShellScan::default();
    for &b in line.as_bytes() {
        scan.feed(b);
    }
    // C reports these after the same loop, from the same two pieces of
    // state (`iocsh.cpp:349-360`).
    if scan.unbalanced_quote() {
        return Some("Unbalanced quote.");
    }
    if scan.trailing_backslash() {
        return Some("Trailing backslash.");
    }
    None
}

/// The one owner of C `split()`'s quote/backslash state
/// (`iocsh.cpp:262-346`).
///
/// C keeps a single `quote` character — which remembers *which* quote
/// opened, so only the matching one closes it — and a single
/// `backslash` flag, and gates every syntactic decision it makes on
/// `!quote && !backslash`: the separator test at `:272`, the whole
/// redirect block at `:274-303`, and quote termination at `:306`. A
/// scanner that tracks a subset of that state disagrees with the
/// tokenizer about where a token ends, which is how a `>` inside a
/// single-quoted argument became a redirect and truncated a file.
///
/// Note the backslash is only ever armed outside a quote (`:273-276`
/// sits inside the `!quote` block), so inside quotes a backslash is
/// ordinary data to C.
#[derive(Default)]
pub(crate) struct ShellScan {
    /// The byte that opened the current quote, or 0 outside quotes.
    quote: u8,
    backslash: bool,
}

impl ShellScan {
    /// Feed the next byte and report whether it is SYNTAX — C's
    /// `!quote && !backslash`. Quote characters, escapes and everything
    /// they cover are data and answer `false`.
    pub(crate) fn feed(&mut self, c: u8) -> bool {
        if self.backslash {
            self.backslash = false;
            return false;
        }
        if self.quote != 0 {
            if c == self.quote {
                self.quote = 0;
            }
            return false;
        }
        match c {
            b'\\' => {
                self.backslash = true;
                false
            }
            b'"' | b'\'' => {
                self.quote = c;
                false
            }
            _ => true,
        }
    }

    pub(crate) fn unbalanced_quote(&self) -> bool {
        self.quote != 0
    }

    pub(crate) fn trailing_backslash(&self) -> bool {
        self.backslash
    }
}

/// Parse an `iocshArgInt` token the way C `cvtArg` does
/// (`iocsh.cpp:814-836`): `strtol(arg, &endp, 0)`. Base-0 means a
/// `0x`/`0X` prefix is hex and a leading `0` is octal — so `dbpr REC 010`
/// is 8, not 10, and `postEvent 0x10` is 16, not an error. On signed
/// overflow C retries with `strtoul` (`0xFFFFFFFFFFFFFFFF` → the same
/// bit pattern reinterpreted into the signed `long`), and an empty arg
/// defaults to 0. Trailing non-numeric characters are rejected, matching
/// C's `if (*endp)` "Invalid integer" check.
fn parse_iocsh_int(token: &str) -> Result<i64, ()> {
    // C `if (arg && *arg)` — an empty token defaults to 0.
    if token.is_empty() {
        return Ok(0);
    }
    // strtol skips leading whitespace, then takes an optional sign.
    let s = token.trim_start();
    if s.is_empty() {
        // Whitespace-only: strtol converts nothing and leaves *endp set
        // → C reports "Invalid integer".
        return Err(());
    }
    let (neg, body) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    // Base-0 prefix detection on the unsigned magnitude.
    let (radix, digits): (u32, &str) =
        if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            (16, hex)
        } else if body.len() > 1 && body.starts_with('0') {
            // Leading `0` + at least one more char → octal (the C
            // convention); `0` alone stays decimal so it parses to 0.
            (8, &body[1..])
        } else {
            (10, body)
        };
    if digits.is_empty() {
        // e.g. a bare sign, or `0x` with no hex digits.
        return Err(());
    }
    // Signed first; on overflow fall back to unsigned (C `strtol`
    // ERANGE → `strtoul`) and reinterpret the bit pattern into the
    // signed result exactly as C stores it in `long ival`. An invalid
    // digit fails both parses → error (matches C's `*endp` check, e.g.
    // octal `08`).
    let parsed = match i64::from_str_radix(digits, radix) {
        Ok(v) => v,
        Err(_) => u64::from_str_radix(digits, radix).map_err(|_| ())? as i64,
    };
    Ok(if neg { parsed.wrapping_neg() } else { parsed })
}

/// Parse tokens into argument values according to argument descriptors.
pub(crate) fn parse_args(tokens: &[String], descs: &[ArgDesc]) -> Result<Vec<ArgValue>, String> {
    let mut result = Vec::with_capacity(descs.len());

    for (i, desc) in descs.iter().enumerate() {
        if i < tokens.len() {
            let token = &tokens[i];
            let val = match desc.arg_type {
                ArgType::String => ArgValue::String(token.clone()),
                ArgType::Int => parse_iocsh_int(token).map(ArgValue::Int).map_err(|_| {
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

    /// C split() outside a quote (iocsh.cpp:275-278,326): the backslash
    /// consumes itself and the next character is literal — it neither
    /// separates nor opens a quote nor closes the call. Every expected
    /// token below was measured on the reference softIoc's `echo`.
    #[test]
    fn out_of_quote_backslash_escapes_like_c_split() {
        // Space syntax: `echo \"hello\"` prints `"hello"`, `echo a\ b`
        // prints `a b`.
        assert_eq!(tokenize(r#"echo \"hello\""#), vec!["echo", r#""hello""#]);
        assert_eq!(tokenize(r#"echo a\ b"#), vec!["echo", "a b"]);
        // Call syntax: `echo(a\,b)` prints `a,b` — the escaped comma
        // does not split the argument.
        assert_eq!(tokenize(r#"echo(a\,b)"#), vec!["echo", "a,b"]);
        // Escape-produced quotes are data, not outer quotes — nothing
        // strips them: `echo(\"hi\")` prints `"hi"`.
        assert_eq!(tokenize(r#"echo(\"hi\")"#), vec!["echo", r#""hi""#]);
        // The closing-paren scanner honors the same rules:
        // `echo(a\))` prints `a)`, `echo('a)b')` prints `a)b`.
        assert_eq!(tokenize(r#"echo(a\))"#), vec!["echo", "a)"]);
        assert_eq!(tokenize(r#"echo('a)b')"#), vec!["echo", "a)b"]);
        // lint_line agrees these lines are well-formed — pre-fix it
        // passed them and the splitters then mis-parsed.
        assert_eq!(lint_line(r#"echo \"hello\""#), None);
        assert_eq!(lint_line(r#"echo(a\,b)"#), None);
    }

    /// Escapes are interpreted exactly once: the call splitter's second
    /// pass no longer re-processes them, so an in-quote `\\\\` (four
    /// backslashes in the script) yields two, not one.
    #[test]
    fn call_syntax_escapes_are_not_double_processed() {
        assert_eq!(tokenize(r#"cmd("a\\\\b")"#), vec!["cmd", r#"a\\b"#]);
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

    /// `iocshArgInt` must parse like C `strtol(arg, &endp, 0)`
    /// (base-0: `0x` hex, leading `0` octal), with a `strtoul` overflow
    /// fallback and empty→0 — not Rust's decimal-only `parse::<i64>`.
    #[test]
    fn test_parse_iocsh_int_base0() {
        // Decimal.
        assert_eq!(parse_iocsh_int("10"), Ok(10));
        assert_eq!(parse_iocsh_int("0"), Ok(0));
        assert_eq!(parse_iocsh_int("-5"), Ok(-5));
        assert_eq!(parse_iocsh_int("+5"), Ok(5));
        // Octal: `dbpr REC 010` is 8 in C, was silently 10 pre-fix.
        assert_eq!(parse_iocsh_int("010"), Ok(8));
        assert_eq!(parse_iocsh_int("00"), Ok(0));
        // Hex: `postEvent 0x10` is 16 in C, errored pre-fix.
        assert_eq!(parse_iocsh_int("0x10"), Ok(16));
        assert_eq!(parse_iocsh_int("0X1F"), Ok(31));
        assert_eq!(parse_iocsh_int("-0x10"), Ok(-16));
        // Empty arg defaults to 0 (C `if (arg && *arg)` else branch).
        assert_eq!(parse_iocsh_int(""), Ok(0));
        // strtoul fallback: bit pattern reinterpreted into signed long.
        assert_eq!(parse_iocsh_int("0xFFFFFFFFFFFFFFFF"), Ok(-1));
        assert_eq!(parse_iocsh_int("18446744073709551615"), Ok(-1));
        // Errors: trailing garbage, invalid octal digit, bare/oversized.
        assert!(parse_iocsh_int("10abc").is_err());
        assert!(parse_iocsh_int("08").is_err());
        assert!(parse_iocsh_int("0x").is_err());
        assert!(parse_iocsh_int("0x1FFFFFFFFFFFFFFFF").is_err());
        assert!(parse_iocsh_int("   ").is_err());
        assert!(parse_iocsh_int("abc").is_err());
    }

    #[test]
    fn test_parse_args_int_base0_via_parse_args() {
        let descs = vec![ArgDesc {
            name: "mask",
            arg_type: ArgType::Int,
            optional: false,
        }];
        let result = parse_args(&["0x10".to_string()], &descs).unwrap();
        assert!(matches!(&result[0], ArgValue::Int(16)));
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
