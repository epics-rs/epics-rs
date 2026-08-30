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
    /// C `iocshArgStringPath` (`iocsh.h:105`): "Equivalent to
    /// iocshArgString with a hint for tab completion that the argument
    /// is a file system path". It converts exactly as
    /// [`ArgType::String`] (`iocsh.cpp:852-855`); the hint is read only
    /// by the interactive completer (`iocsh.cpp:582-584`).
    Path,
    /// C `iocshArgStringRecord` (`iocsh.h:99`): the same string, hinted
    /// as a record name, which the completer routes to
    /// `iocshCompleteRecord` (`iocsh.cpp:579-580`).
    Record,
    /// C `iocshArgArgv` (`iocsh.h:107`): not one token but every token from
    /// this position to the end of the line (`iocsh.cpp:1282-1285`, which
    /// sets `aval.ac = tokenize.size() - iarg` and `aval.av =
    /// &tokenize.argv[iarg]`). The variadic tail behind `epicsThreadShow`,
    /// `epicsThreadResume`, `help` and `on`.
    ///
    /// C's `av[0]` at `iarg == 0` is the command name, which every C callback
    /// then skips by starting at `i = 1`; the vector here carries the
    /// arguments alone, so a handler starts at 0.
    Argv,
}

/// Description of a single command argument.
#[derive(Debug, Clone)]
pub struct ArgDesc {
    pub name: &'static str,
    pub arg_type: ArgType,
}

/// A parsed argument value.
#[derive(Debug, Clone)]
pub enum ArgValue {
    String(String),
    Int(i64),
    Double(f64),
    /// Every remaining token, for an [`ArgType::Argv`] parameter. Empty when
    /// the line ended at this position — C hands the callback `ac == 0` there
    /// rather than treating the argument as absent, so there is no `Missing`
    /// case for this type.
    Argv(Vec<String>),
    Missing,
}

/// Result of executing a command.
pub enum CommandOutcome {
    Continue,
    /// The line FAILED and the command has already said everything it is
    /// going to say — C `iocshSetError(-1)` with no diagnostic of its own,
    /// the shape `dbStateSetCallFunc` and friends use
    /// (`dbIocRegister.c:534-542`, `:548-556`, `:563-571`).
    ///
    /// `Err(String)` means "failed AND print this"; those are two separate
    /// facts, and a command that must fail without printing had no way to
    /// say so. Rather than let one variant carry both meanings by context,
    /// each combination is its own named outcome: `Continue` is neither,
    /// `Failed` is the failure alone, `Err` is both. Every loop consumer
    /// therefore decides "print?" and "failed?" independently instead of
    /// inferring one from the other.
    Failed,
    Exit,
}

/// Command result type.
///
/// `Err(msg)` is C's "print `msg` AND mark the line errored"; the silent
/// failure is [`CommandOutcome::Failed`] on the `Ok` side.
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
/// `handler` is `Arc`-backed because the clone is the dispatch lookup's
/// result: the shell takes the registry's read guard, clones the entry,
/// drops the guard, and only then calls — C's shape, where `registryFind`
/// returns and `(*found->def.func)(&argBuf[0])` runs with nothing held
/// (`iocsh.cpp:1258-1281`). With one process-wide table that is
/// load-bearing rather than stylistic: a handler may register more
/// commands while it runs, so holding the guard across the call
/// deadlocks the script's `iocInit` line against its own registrations.
/// A clone therefore duplicates the name, usage and arg descriptors and
/// shares the one handler, never a second command.
#[derive(Clone)]
pub struct CommandDef {
    pub name: String,
    pub args: Vec<ArgDesc>,
    /// C `iocshFuncDef.usage` (`iocsh.h:126`): the DESCRIPTION only.
    ///
    /// `help` renders the synopsis line itself from `name` and `args`
    /// (`iocsh.cpp:956-969`), so repeating it here prints it twice. Most
    /// of this port's commands were written before `help` had that
    /// shape and still open with `"<name> <args> — "`; new ones should
    /// not.
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
    ///
    /// C's `epicsSetThreadStdout` (`iocsh.cpp:417`), which `startRedirect`
    /// swaps for the duration of one command line.
    output: std::cell::RefCell<Box<dyn std::io::Write>>,
    /// Diagnostic writer — defaults to stderr, redirected by `2>` / `2>>`.
    ///
    /// C's `epicsSetThreadStderr` (`iocsh.cpp:422`). It is a SEPARATE cell
    /// from `output` for the same reason C keeps a separate FILE*: a `2>`
    /// redirect must leave stdout alone, so `dbl 2>/dev/null` still prints its
    /// listing. Every shell diagnostic goes through [`Self::eprintln`]; a bare
    /// `eprintln!` would bypass the swap and is what made `2>` inert.
    error: std::cell::RefCell<Box<dyn std::io::Write>>,
    /// Input reader — defaults to stdin, redirected by `<`.
    ///
    /// C's `epicsSetThreadStdin` (`iocsh.cpp:412`). No built-in command reads
    /// it today; it exists so the `<` redirect performs C's swap rather than
    /// being silently dropped, and so a command that needs input has the same
    /// seam its C counterpart reads.
    input: std::cell::RefCell<Box<dyn std::io::BufRead>>,
    /// The owning shell's live command table, weakly held.
    ///
    /// C needs no such field: its command table IS the registry every other
    /// kind lives in — `iocshRegister` does `registryAdd(iocshCmdID, ...)`
    /// (`iocsh.cpp:171`) and lookup is `registryFind(iocshCmdID, name)`
    /// (`:200`) — so anything holding the process can walk it. This port
    /// keeps the table on the shell instead, which leaves `registryDump`,
    /// the one command that must read the whole of it, with nothing to read.
    /// Weak so a context can never keep its shell alive, and empty for a
    /// context built standalone: no shell owns it, so it has no commands.
    ///
    /// It cannot dangle either, and not by luck: the only context that ever
    /// holds a live handle is [`IocShell::ctx`](crate::server::iocsh::IocShell),
    /// owned by value inside the very shell whose `Arc` it points at, and
    /// `CommandContext` is not `Clone`, so a handler cannot outlive the
    /// registry it is reading. `upgrade()` returning `None` therefore means
    /// "this context has no shell", never "the table was freed underneath
    /// me" — and a handler that reaches it sees an empty command list rather
    /// than a stale or resurrected one.
    commands: std::cell::RefCell<std::sync::Weak<std::sync::RwLock<CommandRegistry>>>,
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
            error: std::cell::RefCell::new(Box::new(std::io::stderr())),
            input: std::cell::RefCell::new(Box::new(std::io::BufReader::new(std::io::stdin()))),
            commands: std::cell::RefCell::new(std::sync::Weak::new()),
        }
    }

    /// Attach the shell's live command table, so `registryDump` can print
    /// C's `iocshCmd` registry. Called once by [`crate::server::iocsh::IocShell`].
    pub(crate) fn set_command_registry(&self, reg: &Arc<std::sync::RwLock<CommandRegistry>>) {
        *self.commands.borrow_mut() = Arc::downgrade(reg);
    }

    /// C's `iocshCmd` registry as `(name, entry address)`, sorted by name.
    ///
    /// Empty when no shell owns this context, which is the truth rather than
    /// a gap: C's table is a process global that exists from the first
    /// `iocshRegister`, and a port context with no shell has registered none.
    pub(crate) fn command_entries(&self) -> Vec<(String, usize)> {
        let Some(reg) = self.commands.borrow().upgrade() else {
            return Vec::new();
        };
        let guard = reg.read().unwrap_or_else(|e| e.into_inner());
        let mut entries: Vec<(String, usize)> = guard
            .list()
            .into_iter()
            .map(|name| (name.to_string(), name.as_ptr() as usize))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
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

    /// Print a line to the current DIAGNOSTIC stream (stderr, or the file a
    /// `2>` redirect installed) — C `fprintf(epicsGetThreadStderr(), ...)`.
    pub fn eprintln(&self, msg: &str) {
        let mut err = self.error.borrow_mut();
        let _ = writeln!(err, "{msg}");
    }

    /// Read one line from the current INPUT stream (stdin, or the file a `<`
    /// redirect installed) — C `fgets(..., epicsGetThreadStdin())`. `Ok(0)`
    /// is end of input.
    pub fn read_line(&self, buf: &mut String) -> std::io::Result<usize> {
        let mut input = self.input.borrow_mut();
        input.read_line(buf)
    }

    /// Temporarily redirect the DIAGNOSTIC stream, run a closure, then
    /// restore — C `startRedirect`/`stopRedirect` for fd 2.
    pub(crate) fn with_error<W: std::io::Write + 'static, R>(
        &self,
        writer: W,
        f: impl FnOnce() -> R,
    ) -> R {
        let prev = self.error.replace(Box::new(writer));
        let result = f();
        let _ = self.error.borrow_mut().flush();
        self.error.replace(prev);
        result
    }

    /// Temporarily redirect the INPUT stream, run a closure, then restore —
    /// C `startRedirect`/`stopRedirect` for fd 0.
    pub(crate) fn with_input<R: std::io::Read + 'static, T>(
        &self,
        reader: R,
        f: impl FnOnce() -> T,
    ) -> T {
        let prev = self
            .input
            .replace(Box::new(std::io::BufReader::new(reader)));
        let result = f();
        self.input.replace(prev);
        result
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
    /// Every name a later [`CommandRegistry::register`] displaced.
    ///
    /// Replacement is C's behaviour — `iocshRegister` overwrites the
    /// entry when the name is already in its list (`iocsh.cpp:684-700`),
    /// which is how a support module legitimately takes a name over —
    /// so this must not refuse. But the port's own built-in table is
    /// assembled from one file per C registrar, and two of those
    /// claiming one name is a build defect that `HashMap::insert`
    /// swallows in silence: the merged tree registers one of them and
    /// nothing fails. Recording the displacement is what lets
    /// `register_builtins` assert the table is collision-free, so two
    /// panels adding families in parallel cannot auto-merge into a tree
    /// neither of them tested.
    displaced: Vec<String>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
            displaced: Vec::new(),
        }
    }

    pub fn register(&mut self, def: CommandDef) {
        let name = def.name.clone();
        if self.commands.insert(name.clone(), def).is_some() {
            self.displaced.push(name);
        }
    }

    /// The names registered more than once, in the order they collided.
    pub fn displaced(&self) -> &[String] {
        &self.displaced
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

/// Break a command line into words the way C `split()` does
/// (`iocsh.cpp:255-376`).
///
/// C has ONE separator set — `strchr(" \t(),\r", c)` (`:271`) — and no
/// notion of two syntaxes: `cmd(a, b)`, `cmd (a, b)` and `cmd a b` are
/// the same line to it, and a `)` or a `,` anywhere outside a quote ends
/// a word wherever it appears. Deciding between a "call" and a "legacy"
/// shape first, and then giving each its own splitter, is what made
/// `dbl)` an unregistered command and `dbl , ai` a three-token line here
/// while C ran both.
///
/// Quotes are removed wherever they appear rather than stripped from the
/// ends afterwards, so `echo(a"b"c)` is the one token `abc` (measured),
/// and a backslash is special only OUTSIDE a quote — inside one it is
/// ordinary data, which is why `epicsEnvSet X "a\\b"` sets two
/// backslashes and `epicsEnvSet X "hello \"world\""` is an unbalanced
/// quote rather than an escaped one (both measured).
///
/// The state behind all of that is [`ShellScan`], the same instance
/// shape [`lint_line`] and [`super::parse_redirect`] run: C keeps one
/// `quote`/`backslash` pair and answers every question from it, so a
/// second implementation here could only disagree with them.
///
/// C parity (`iocsh.cpp:1184` `macDefExpand` → `:1215` `tokenize.split`):
/// macros are expanded across the WHOLE line *before* it is split into
/// words, and `split` itself expands nothing. So a macro whose value
/// contains a separator (`$(CMD)` with `CMD="dbpr REC 2"`) is
/// re-tokenized into multiple words, and a leading `$(MACRO)` at command
/// position resolves before the command name is taken. The caller
/// ([`super::IocShell::execute_line`]) owns that one expansion; this
/// function must not perform a second one.
pub(crate) fn tokenize(line: &str) -> Vec<String> {
    // Only ASCII bytes can be a separator, a quote or an escape, and
    // every byte of a multi-byte character is >= 0x80, so one such byte
    // stands for the whole character: classifying per character reaches
    // the same words C reaches per byte, without ever cutting a token
    // inside a UTF-8 sequence.
    const NON_ASCII: u8 = 0x80;

    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut inword = false;
    let mut scan = ShellScan::default();

    for ch in line.chars() {
        let byte = if ch.is_ascii() { ch as u8 } else { NON_ASCII };
        match scan.feed(byte) {
            // C `:275-278` `continue`s past the word builder, so the
            // backslash neither starts a word nor ends one.
            SplitRole::Escape => {}
            // C `:312-317`: a separator closes an open word and, when
            // none is open (`:331`), is simply skipped — which is why a
            // run of them yields no empty tokens.
            SplitRole::Separator => {
                if inword {
                    tokens.push(std::mem::take(&mut current));
                    inword = false;
                }
            }
            // C `:345-346` writes nothing for the quote itself, but the
            // word has begun: `epicsEnvSet X ""` passes an empty token,
            // not an absent one.
            SplitRole::Quote => inword = true,
            SplitRole::Data => {
                current.push(ch);
                inword = true;
            }
        }
    }
    // C `:353-354` terminates the word the end of the line leaves open.
    if inword {
        tokens.push(current);
    }
    tokens
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
    // state (`iocsh.cpp:362-371`).
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
/// `!quote && !backslash`: the separator test at `:271`, the whole
/// redirect block at `:274-303`, and quote termination at `:307-308`. A
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

/// What C `split()`'s word builder does with one byte, given the
/// `quote`/`backslash` state before it (`iocsh.cpp:306-349`).
pub(crate) enum SplitRole {
    /// In C's separator set `" \t(),\r"` (`:271`) and not covered by a
    /// quote or an escape: it closes an open word and starts none.
    Separator,
    /// The quote character that opened or closed a quoted run: consumed,
    /// written to no token (`:345-346`, `:307-309`), but a word has
    /// begun.
    Quote,
    /// The backslash that armed the escape (`:275-278`): consumed, and
    /// it neither starts nor ends a word.
    Escape,
    /// Everything else, including any byte inside a quote and the one
    /// byte an escape covers — appended to the current word.
    Data,
}

impl ShellScan {
    /// C's `!quote && !backslash` (`:271`, `:273`, `:311`) for the byte
    /// about to be fed — the gate every syntactic decision in `split()`
    /// sits behind. Read it BEFORE [`Self::feed`] consumes the byte.
    pub(crate) fn is_syntax(&self) -> bool {
        self.quote == 0 && !self.backslash
    }

    /// Feed the next byte and report what C's loop does with it.
    pub(crate) fn feed(&mut self, c: u8) -> SplitRole {
        if self.backslash {
            // C `:325-327`: whatever it is, the escaped byte is data,
            // and `:350` clears the flag.
            self.backslash = false;
            return SplitRole::Data;
        }
        if self.quote != 0 {
            // C `:307-309`: only the byte that opened the quote closes
            // it. Everything else inside is data — a backslash included,
            // because `:273` sits inside the `!quote` block and never
            // arms one here.
            if c == self.quote {
                self.quote = 0;
                return SplitRole::Quote;
            }
            return SplitRole::Data;
        }
        match c {
            b'\\' => {
                self.backslash = true;
                SplitRole::Escape
            }
            b'"' | b'\'' => {
                self.quote = c;
                SplitRole::Quote
            }
            // C `:271`, in full and in order.
            b' ' | b'\t' | b'(' | b')' | b',' | b'\r' => SplitRole::Separator,
            _ => SplitRole::Data,
        }
    }

    pub(crate) fn unbalanced_quote(&self) -> bool {
        self.quote != 0
    }

    pub(crate) fn trailing_backslash(&self) -> bool {
        self.backslash
    }
}

/// Why C `cvtArg` refused an `iocshArgInt` token. C has two sentences
/// here and reaches them from two different conditions, so one error
/// value cannot stand for both: [`IntArgError::OutOfRange`] is the arm
/// where `strtol` AND the `strtoul` retry under it both set `ERANGE`
/// (`iocsh.cpp:824-831`), and [`IntArgError::Invalid`] is the `*endp`
/// arm below that (`:833-837`). C returns from the range arm before it
/// ever looks at `*endp`, so trailing garbage cannot downgrade an
/// out-of-range value to an invalid one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IntArgError {
    /// C `iocsh.cpp:833-837` — `strtol` left characters unconsumed.
    Invalid,
    /// C `iocsh.cpp:824-831` — the value fits neither `long` nor
    /// `unsigned long`.
    OutOfRange,
}

/// Parse an `iocshArgInt` token the way C `cvtArg` does
/// (`iocsh.cpp:820-842`): `strtol(arg, &endp, 0)`. Base-0 means a
/// `0x`/`0X` prefix is hex and a leading `0` is octal — so `dbpr REC 010`
/// is 8, not 10, and `postEvent 0x10` is 16, not an error. On signed
/// overflow C retries with `strtoul` (`0xFFFFFFFFFFFFFFFF` → the same
/// bit pattern reinterpreted into the signed `long`), and an empty arg
/// defaults to 0. Trailing non-numeric characters are rejected, matching
/// C's `if (*endp)` "Invalid integer" check.
pub(super) fn parse_iocsh_int(token: &str) -> Result<i64, IntArgError> {
    // C `if (arg && *arg)` — an empty token defaults to 0.
    if token.is_empty() {
        return Ok(0);
    }
    // strtol skips leading whitespace, then takes an optional sign.
    let s = token.trim_start();
    if s.is_empty() {
        // Whitespace-only: strtol converts nothing and leaves *endp set
        // → C reports "Invalid integer".
        return Err(IntArgError::Invalid);
    }
    let (neg, body) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    // Base-0 prefix detection on the unsigned magnitude.
    let (radix, rest): (u32, &str) =
        if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            (16, hex)
        } else if body.len() > 1 && body.starts_with('0') {
            // Leading `0` + at least one more char → octal (the C
            // convention); `0` alone stays decimal so it parses to 0.
            (8, &body[1..])
        } else {
            (10, body)
        };
    // `strtol` converts the LONGEST valid prefix and points `endp` at the
    // first character it could not use, so the digit run and what follows
    // it are two separate facts: the run alone decides `ERANGE`, the
    // leftover alone decides `*endp`. Reading them as one — parsing the
    // whole tail and calling any failure the same thing — is what made
    // `99999999999999999999abc` "invalid" here where C calls it out of
    // range.
    let run_len = rest
        .find(|c: char| !c.is_digit(radix))
        .unwrap_or(rest.len());
    let (digits, leftover) = rest.split_at(run_len);
    if digits.is_empty() {
        // strtol converted nothing — a bare sign, `0x` with no hex digit,
        // octal `08` — so `endp == arg` and C takes the `*endp` arm.
        return Err(IntArgError::Invalid);
    }
    // Signed first; on overflow fall back to unsigned (C `strtol`
    // ERANGE → `strtoul`) and reinterpret the bit pattern into the
    // signed result exactly as C stores it in `long ival`. `digits` holds
    // only characters valid for the radix, so the unsigned parse can fail
    // for one reason: the magnitude does not fit.
    let parsed = match i64::from_str_radix(digits, radix) {
        Ok(v) => v,
        Err(_) => u64::from_str_radix(digits, radix).map_err(|_| IntArgError::OutOfRange)? as i64,
    };
    if !leftover.is_empty() {
        return Err(IntArgError::Invalid);
    }
    Ok(if neg { parsed.wrapping_neg() } else { parsed })
}

/// C `epicsStrtod(s, &endp)` under the `*endp == '\0'` whole-token check
/// both of its iocsh callers apply — `cvtArg`'s double arm
/// (`iocsh.cpp:844-856`) and `varHandler`'s (`:1431-1442`). One owner
/// here because C has one function: a second port could only disagree
/// with this one about which arguments the shell accepts.
///
/// Rust's own `f64` parser takes the decimal, exponent, `inf` and `nan`
/// spellings C's does, but not the C99 hex form — which glibc's `strtod`
/// accepts, so C accepts it too. Measured on `bin/linux-x86_64/softIoc`
/// (R7.0.10-146-g8f5015b663d764ad75df): `var seqDLYlimit 0x10` leaves
/// the variable reading `double seqDLYlimit = 16`, `var seqDLYlimit
/// 0x1p3` leaves it at 8, and `epicsThreadSleep 0x10` sleeps 16.32 s.
/// The hex arm below is that form; every other spelling goes to Rust's
/// parser.
pub(super) fn epics_strtod_whole(token: &str) -> Option<f64> {
    // C's `strtod` skips leading whitespace before it converts anything,
    // so a quoted `" 1"` is 1 to C and must be here.
    let s = token.trim_start();
    if s.is_empty() {
        return None;
    }
    let (neg, body) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) else {
        return s.parse::<f64>().ok();
    };
    let (digits, exponent) = match hex.split_once(['p', 'P']) {
        Some((digits, exponent)) => (digits, Some(exponent)),
        None => (hex, None),
    };
    let (int_part, frac_part) = digits.split_once('.').unwrap_or((digits, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        // `0x` with no hex digit: C's `strtod` converts the leading `0`
        // and leaves `endp` on the `x`, which the whole-token check
        // rejects.
        return None;
    }
    let mut value = 0.0f64;
    for c in int_part.chars() {
        value = value * 16.0 + f64::from(c.to_digit(16)?);
    }
    let mut scale = 1.0f64 / 16.0;
    for c in frac_part.chars() {
        value += f64::from(c.to_digit(16)?) * scale;
        scale /= 16.0;
    }
    if let Some(exponent) = exponent {
        // `p<exp>` is a power of TWO, and C wants at least one decimal
        // digit after it — without one `strtod` stops before the `p` and
        // the whole-token check refuses the argument. The clamp only
        // saturates to the infinity and the zero C's `strtod` returns
        // for an exponent that far out.
        let exponent = exponent.parse::<i64>().ok()?.clamp(-4096, 4096) as i32;
        value *= 2f64.powi(exponent);
    }
    Some(if neg { -value } else { value })
}

/// Parse tokens into argument values according to argument descriptors.
pub(crate) fn parse_args(tokens: &[String], descs: &[ArgDesc]) -> Result<Vec<ArgValue>, String> {
    let mut result = Vec::with_capacity(descs.len());

    for (i, desc) in descs.iter().enumerate() {
        // `iocshArgArgv` is the rest of the line, not a token, so it is
        // defined at every position — including one past the last token,
        // where C builds an empty `ac`/`av` rather than reporting a missing
        // argument.
        if matches!(desc.arg_type, ArgType::Argv) {
            result.push(ArgValue::Argv(tokens.get(i..).unwrap_or(&[]).to_vec()));
            continue;
        }
        // C hands `cvtArg` the token or NULL and never reports an absent
        // one: `iocsh.cpp:1294-1296` passes NULL once the tokens run out,
        // and every `cvtArg` arm defaults rather than failing — `ival = 0`,
        // `dval = 0.0`, `sval = arg` (so NULL). Its own comment
        // (`iocsh.cpp:809-812`) states the intent outright: "a double/int
        // with no value will default to 0 which may allow you to add
        // optional arguments to the end of your argument list."
        //
        // So arity is not the shell's rule to enforce. A command that needs
        // an argument checks it itself and prints its own usage — measured,
        // `dbLoadRecords` with no token answers `Usage: dbLoadRecords
        // "file", "subs"` and fails the line from inside the command, not
        // from here. Rejecting here instead made the port refuse lines C
        // runs, and under `on error break` it stopped the script a line
        // early (`libcom/test/iocshTestSuccess.cmd:8`, the argument-less
        // `epicsThreadSleep`).
        let Some(token) = tokens.get(i) else {
            result.push(ArgValue::Missing);
            continue;
        };
        let val = match desc.arg_type {
            // `iocsh.cpp:852-855` converts all three string types
            // through one `argBuf->sval = arg` arm — `Path` and
            // `Record` differ from `String` only in completion. An empty
            // token is a token: C stores `""`, not NULL, so it stays
            // distinguishable from an absent one.
            ArgType::String | ArgType::Path | ArgType::Record => ArgValue::String(token.clone()),
            // The numeric arms guard on `if (arg && *arg)`
            // (`iocsh.cpp:820`, `:843`), which collapses an EMPTY token
            // into the same default as an absent one. Only a non-empty
            // token that fails to parse is an error.
            ArgType::Int | ArgType::Double if token.is_empty() => ArgValue::Missing,
            // The sentence is C's own, and quotes the offending VALUE and
            // nothing else. C names no argument here: `cvtArg` is handed
            // the `iocshArg` but reads only its `type`
            // (`iocsh.cpp:813-819`), so the name never reaches the text.
            // The frame `showError` puts around it — `ERROR <file> line
            // <n>: ` — already says which line, and adding the parameter
            // name is the only thing on this stream that stops a site
            // script's grep from matching.
            ArgType::Int => parse_iocsh_int(token)
                .map(ArgValue::Int)
                .map_err(|why| match why {
                    IntArgError::OutOfRange => format!("Integer '{token}' out of range."),
                    IntArgError::Invalid => format!("Invalid integer '{token}'."),
                })?,
            ArgType::Double => epics_strtod_whole(token)
                .map(ArgValue::Double)
                .ok_or_else(|| format!("Invalid double '{token}'."))?,
            // Handled above: an `Argv` parameter never reaches the
            // one-token path.
            ArgType::Argv => unreachable!(),
        };
        result.push(val);
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

    /// C arms the backslash inside `if (!quote && !backslash)`
    /// (`iocsh.cpp:273-278`), so within a quoted run a backslash is
    /// ordinary data and the very next byte still closes the quote if it
    /// matches. Measured on `bin/linux-x86_64/softIoc`
    /// (R7.0.10-146-g8f5015b663d764ad75df) through `epicsEnvSet` +
    /// `epicsEnvShow`, which pass their argument on untranslated where
    /// `echo` would run it through `dbTranslateEscape`:
    /// `epicsEnvSet X "a\\b"` → `X=a\\b`, and
    /// `epicsEnvSet("X", "a\\\\b")` → `X=a\\\\b`.
    #[test]
    fn a_backslash_inside_a_quote_is_data() {
        assert_eq!(tokenize(r#"cmd "a\\b""#), vec!["cmd", r#"a\\b"#]);
        assert_eq!(tokenize(r#"cmd("a\\\\b")"#), vec!["cmd", r#"a\\\\b"#]);
    }

    /// The same rule read from the other side: `\"` inside a quoted run
    /// does not escape the quote, so the quote closes there and the one
    /// that follows `world` opens a run nothing closes. Measured —
    /// `epicsEnvSet X "hello \"world\""` answers `ERROR u1.cmd line 1:
    /// Unbalanced quote.` and sets nothing.
    #[test]
    fn an_escaped_quote_inside_a_quote_leaves_the_line_unbalanced() {
        assert_eq!(
            lint_line(r#"cmd "hello \"world\"""#),
            Some("Unbalanced quote.")
        );
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
    fn test_tokenize_blanks_before_paren_still_call_syntax() {
        // C split() separates on `strchr(" \t(),\r", c)` uniformly
        // (iocsh.cpp:271), so a blank between the name and `(` changes
        // nothing there. The port must not fall back to space-separated
        // syntax and hand the command `("L0",`-shaped tokens.
        assert_eq!(
            tokenize(r#"asynOctetSetInputEos ("L0", 0, "\r")"#),
            tokenize(r#"asynOctetSetInputEos("L0", 0, "\r")"#)
        );
        assert_eq!(tokenize("cmd\t(a, b)"), vec!["cmd", "a", "b"]);
        assert_eq!(tokenize("iocInit ()"), vec!["iocInit"]);
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

    /// C has one separator set and applies it everywhere, so a `)` or a
    /// `,` outside a quote ends a word wherever it stands — there is no
    /// "call syntax" for it to be inside of. Measured on
    /// `bin/linux-x86_64/softIoc` (R7.0.10-146-g8f5015b663d764ad75df),
    /// each line its own script against an empty database: `dbl)` and
    /// `dbl , ai` both print nothing and raise nothing, where this port
    /// answered `Command 'dbl)' not registered.` and passed `,` as a
    /// record type; `echo(a"b"c)` prints `abc`, so a quote is removed
    /// wherever it sits and not merely stripped off the ends; and
    /// `echo(a,,b)` prints `a`, so a run of separators yields no empty
    /// argument between them.
    #[test]
    fn one_separator_set_applies_off_the_call_shape_too() {
        assert_eq!(tokenize("dbl)"), vec!["dbl"]);
        assert_eq!(tokenize("dbl )"), vec!["dbl"]);
        assert_eq!(tokenize("dbl , ai"), vec!["dbl", "ai"]);
        assert_eq!(tokenize(r#"echo(a"b"c)"#), vec!["echo", "abc"]);
        assert_eq!(tokenize(r#"echo "a"b"c""#), vec!["echo", "abc"]);
        assert_eq!(tokenize("echo(a,,b)"), vec!["echo", "a", "b"]);
        // An opening quote begins a word even when it closes at once, so
        // an empty argument stays distinguishable from an absent one —
        // `cvtArg` keeps `""` and NULL apart (`iocsh.cpp:858-862`).
        assert_eq!(tokenize(r#"cmd "" x"#), vec!["cmd", "", "x"]);
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
        }];
        let result = parse_args(&[], &descs).unwrap();
        assert!(matches!(&result[0], ArgValue::Missing));
    }

    /// C's one uniform rule, by boundary rather than by story. `cvtArg`
    /// (`iocsh.cpp:813-895`) is reached for every declared parameter with
    /// either the token or NULL, so the boundaries are: token absent, token
    /// present but empty, token present and well formed, token present and
    /// malformed — crossed with the type, which is the only thing that
    /// decides the default. A descriptor carries a name and a type and
    /// nothing else, so there is nothing left for the rule to depend on.
    #[test]
    fn a_parameter_with_no_token_takes_its_types_default() {
        for (arg_type, what) in [
            (ArgType::String, "string"),
            (ArgType::Path, "path"),
            (ArgType::Record, "record"),
            (ArgType::Int, "int"),
            (ArgType::Double, "double"),
        ] {
            let descs = vec![ArgDesc {
                name: "only",
                arg_type,
            }];
            let got = parse_args(&[], &descs)
                .unwrap_or_else(|e| panic!("a missing {what} must not fail the line: {e}"));
            assert!(
                matches!(&got[0], ArgValue::Missing),
                "a missing {what} must reach the command as Missing, got {:?}",
                got[0]
            );
        }
    }

    /// `cvtArg`'s numeric arms guard on `if (arg && *arg)`, so an empty
    /// token defaults exactly like an absent one — but the string arm is
    /// `sval = arg`, which keeps `""` distinct from NULL.
    #[test]
    fn an_empty_token_defaults_for_numbers_and_stays_empty_for_strings() {
        let empty = vec![String::new()];
        for arg_type in [ArgType::Int, ArgType::Double] {
            let descs = vec![ArgDesc {
                name: "only",
                arg_type,
            }];
            let got = parse_args(&empty, &descs).expect("an empty numeric token is C's 0");
            assert!(matches!(&got[0], ArgValue::Missing), "got {:?}", got[0]);
        }
        let descs = vec![ArgDesc {
            name: "only",
            arg_type: ArgType::String,
        }];
        let got = parse_args(&empty, &descs).expect("an empty string token is C's \"\"");
        assert!(
            matches!(&got[0], ArgValue::String(s) if s.is_empty()),
            "got {:?}",
            got[0]
        );
    }

    /// Only a NON-EMPTY token that does not parse is an error — that arm of
    /// `cvtArg` is the one that returns 0 and stops the argument loop, so
    /// the command is never called.
    #[test]
    fn a_malformed_non_empty_token_is_still_an_error() {
        for (arg_type, token) in [(ArgType::Int, "xyz"), (ArgType::Double, "xyz")] {
            let descs = vec![ArgDesc {
                name: "only",
                arg_type,
            }];
            assert!(parse_args(&[token.to_string()], &descs).is_err());
        }
    }

    /// C stops converting at `nargs`; surplus tokens are simply not read
    /// (`iocsh.cpp:1270-1300` iterates the DESCRIPTORS). Measured on C:
    /// `epicsThreadSleep 0.0 extra` runs without complaint.
    #[test]
    fn a_surplus_token_is_ignored_not_rejected() {
        let descs = vec![ArgDesc {
            name: "seconds",
            arg_type: ArgType::Double,
        }];
        let tokens = vec!["0.0".to_string(), "extra".to_string()];
        let got = parse_args(&tokens, &descs).expect("a surplus token is not an error");
        assert_eq!(got.len(), 1);
        assert!(matches!(&got[0], ArgValue::Double(d) if *d == 0.0));
    }

    /// The shortfall may be several parameters deep, and each one takes its
    /// own type's default rather than the first absence ending the line.
    #[test]
    fn every_parameter_past_the_last_token_defaults_independently() {
        let descs = vec![
            ArgDesc {
                name: "file",
                arg_type: ArgType::Path,
            },
            ArgDesc {
                name: "level",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "subs",
                arg_type: ArgType::String,
            },
        ];
        let got = parse_args(&["only.db".to_string()], &descs).expect("a short line still runs");
        assert_eq!(got.len(), 3);
        assert!(matches!(&got[0], ArgValue::String(s) if s == "only.db"));
        assert!(matches!(&got[1], ArgValue::Missing));
        assert!(matches!(&got[2], ArgValue::Missing));
    }

    #[test]
    fn test_parse_args_int() {
        let descs = vec![ArgDesc {
            name: "level",
            arg_type: ArgType::Int,
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
        // Which of C's two sentences each one earns is the point — C
        // returns from the `ERANGE` arm before the `*endp` check, so a
        // magnitude that fits neither `long` nor `unsigned long` is out
        // of range even when garbage follows it.
        assert_eq!(parse_iocsh_int("10abc"), Err(IntArgError::Invalid));
        assert_eq!(parse_iocsh_int("08"), Err(IntArgError::Invalid));
        assert_eq!(parse_iocsh_int("0x"), Err(IntArgError::Invalid));
        assert_eq!(parse_iocsh_int("   "), Err(IntArgError::Invalid));
        assert_eq!(parse_iocsh_int("abc"), Err(IntArgError::Invalid));
        assert_eq!(
            parse_iocsh_int("0x1FFFFFFFFFFFFFFFF"),
            Err(IntArgError::OutOfRange)
        );
        assert_eq!(
            parse_iocsh_int("99999999999999999999"),
            Err(IntArgError::OutOfRange)
        );
        assert_eq!(
            parse_iocsh_int("99999999999999999999abc"),
            Err(IntArgError::OutOfRange)
        );
        // `LONG_MIN` itself is in range for `strtol`, and reaches the
        // same value here through the unsigned retry.
        assert_eq!(parse_iocsh_int("-9223372036854775808"), Ok(i64::MIN));
    }

    /// C `cvtArg` quotes the offending VALUE and never the parameter
    /// name, and its integer refusal has two distinct sentences
    /// (`iocsh.cpp:824-831`, `:833-837`). Measured on
    /// `bin/linux-x86_64/softIoc` (R7.0.10-146-g8f5015b663d764ad75df):
    /// `dbpr A1 notanumber` → `ERROR c.cmd line 1: Invalid integer
    /// 'notanumber'.`, `dbpr A1 99999999999999999999` → `ERROR c.cmd
    /// line 1: Integer '99999999999999999999' out of range.`,
    /// `epicsThreadSleep 0.1abc` → `ERROR c.cmd line 1: Invalid double
    /// '0.1abc'.`
    #[test]
    fn numeric_refusals_are_cs_own_sentences() {
        let int_desc = vec![ArgDesc {
            name: "level",
            arg_type: ArgType::Int,
        }];
        assert_eq!(
            parse_args(&["notanumber".to_string()], &int_desc).unwrap_err(),
            "Invalid integer 'notanumber'."
        );
        assert_eq!(
            parse_args(&["99999999999999999999".to_string()], &int_desc).unwrap_err(),
            "Integer '99999999999999999999' out of range."
        );
        let double_desc = vec![ArgDesc {
            name: "seconds",
            arg_type: ArgType::Double,
        }];
        assert_eq!(
            parse_args(&["0.1abc".to_string()], &double_desc).unwrap_err(),
            "Invalid double '0.1abc'."
        );
    }

    /// `epicsStrtod` is glibc's `strtod` here, so C takes the C99 hex
    /// form. Measured on `bin/linux-x86_64/softIoc`
    /// (R7.0.10-146-g8f5015b663d764ad75df): `var seqDLYlimit 0x10` then
    /// `var seqDLYlimit` prints `double seqDLYlimit = 16`, `0x1p3`
    /// prints `8`, `inf` prints `inf`, and `epicsThreadSleep 0x10`
    /// sleeps 16.32 s rather than raising `Invalid double '0x10'.`
    #[test]
    fn epics_strtod_takes_cs_hex_form() {
        assert_eq!(epics_strtod_whole("0x10"), Some(16.0));
        assert_eq!(epics_strtod_whole("0X10"), Some(16.0));
        assert_eq!(epics_strtod_whole("0x1p3"), Some(8.0));
        assert_eq!(epics_strtod_whole("-0x1.8p1"), Some(-3.0));
        assert_eq!(epics_strtod_whole("0x.8"), Some(0.5));
        assert_eq!(epics_strtod_whole("inf"), Some(f64::INFINITY));
        assert!(epics_strtod_whole("nan").is_some_and(f64::is_nan));
        // The decimal spellings are unchanged.
        assert_eq!(epics_strtod_whole("0.5"), Some(0.5));
        assert_eq!(epics_strtod_whole("1e3"), Some(1000.0));
        assert_eq!(epics_strtod_whole(" 1"), Some(1.0));
        // `strtod` stops where the token stops being a number, and the
        // whole-token check then refuses it.
        assert_eq!(epics_strtod_whole("0.1abc"), None);
        assert_eq!(epics_strtod_whole("0x"), None);
        assert_eq!(epics_strtod_whole("0x1p"), None);
        assert_eq!(epics_strtod_whole("0x1g"), None);
        assert_eq!(epics_strtod_whole(""), None);
        // The one port serves both callers, so `var` agrees with the
        // argument converter about what a double is.
        let descs = vec![ArgDesc {
            name: "seconds",
            arg_type: ArgType::Double,
        }];
        assert!(matches!(
            parse_args(&["0x10".to_string()], &descs).unwrap()[0],
            ArgValue::Double(v) if v == 16.0
        ));
    }

    #[test]
    fn test_parse_args_int_base0_via_parse_args() {
        let descs = vec![ArgDesc {
            name: "mask",
            arg_type: ArgType::Int,
        }];
        let result = parse_args(&["0x10".to_string()], &descs).unwrap();
        assert!(matches!(&result[0], ArgValue::Int(16)));
    }

    #[test]
    fn test_parse_args_double() {
        let descs = vec![ArgDesc {
            name: "value",
            arg_type: ArgType::Double,
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
