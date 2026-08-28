//! Runtime logging — `errlog` severity surface plus the `rt_*` macros.
//!
//! C parity: `modules/libcom/src/error/errlog.{c,h}`.
//!
//! The four `rt_*` macros route through the `tracing` facade (the
//! crate's de-facto logging path) instead of bare `eprintln!`, so an
//! application's `tracing` subscriber controls level filtering,
//! formatting, and sinks uniformly.
//!
//! The `errlog`-severity API mirrors `errlogSevEnum`,
//! `errlogSevEnumString`, `errlogSetSevToLog`/`errlogGetSevToLog`, and
//! `errlogSevPrintf`. Note what C does *not* do: `errlogSevVprintf`
//! (`errlog.c:376-388`) consults no threshold, and nothing else in base
//! reads `pvt.sevToLog` either — `errlogSetSevToLog`/`errlogGetSevToLog`
//! are a stored setting and no more. A C IOC therefore prints every
//! `errlogSevPrintf` line whatever the setting says, and so does this.

use std::sync::atomic::{AtomicU8, Ordering};

/// True when no `tracing` subscriber would take an event — i.e. when
/// everything this module emits is being discarded.
///
/// Every diagnostic in this workspace funnels into `tracing`, and installing a
/// subscriber is the *application's* job. The hosted binaries do it; the RTEMS
/// IOC entry points do not, because `tracing-subscriber` sits behind an
/// optional feature that also drags in a Prometheus exporter. The result on
/// target was an IOC that emitted **nothing at all** on its console — not a
/// quiet IOC, a mute one, with every `errlog` line dropped on the floor.
///
/// C cannot reach that state: `errlogPrintf` ends at the console writer, which
/// always exists. This restores that property without duplicating output when
/// a subscriber *is* installed.
///
/// [`tracing::level_filters::LevelFilter::current`] reads the active
/// dispatcher's max-level hint and is `OFF` exactly when there is no
/// dispatcher (or one that has declared it wants nothing). It sees a
/// scoped `with_default` subscriber as well as a global one, so a test that
/// captures events does not also get console noise.
fn nothing_is_listening() -> bool {
    tracing::level_filters::LevelFilter::current() == tracing::level_filters::LevelFilter::OFF
}

/// The `tracing` target every `errlog` entry point publishes on.
///
/// The `tracing` macros take a literal, so this constant cannot be used at the
/// emit sites; `the_errlog_target_is_the_one_the_macros_publish_on` asserts the
/// two spellings agree, so the subscriber's skip cannot drift from them.
const ERRLOG_TARGET: &str = "epics_base_rs::errlog";

/// True when the dispatcher that would take this event is this crate's own
/// [`ConsoleSubscriber`] — i.e. when the errlog console is ours to write.
///
/// A fact about the process, not a guess: [`ConsoleSubscriber`] exists to give
/// an IOC C's console, so when it is the one installed, C's bytes are what the
/// console must get, and [`write_console`] is what writes them. An application
/// that installed its OWN subscriber asked for its own formatting instead, and
/// gets it — errlog stays a `tracing` event for exactly that reason.
fn console_subscriber_is_current() -> bool {
    tracing::dispatcher::get_default(|d| d.is::<ConsoleSubscriber>())
}

/// Write one already-formatted errlog line to the console, when this process's
/// errlog console is ours and the `eltc` setting still says to.
///
/// The single owner of "does this line reach the console". C's owner is the
/// errlog worker's `pvt.toConsole ? pvt.console : NULL` (`errlog.c:648`); here
/// it is this function, called on the logging thread so that a scoped
/// `tracing` subscriber and the caller's span still apply.
///
/// The console is ours in the two states an IOC runs in: nothing listening at
/// all ([`nothing_is_listening`]), and [`ConsoleSubscriber`] installed — which
/// then skips `epics_base_rs::errlog` events precisely so these bytes are
/// written once, verbatim, instead of a second time behind a `LEVEL target:`
/// prefix. Routing errlog's console through here rather than through the
/// subscriber is also what keeps `errlogPrintfNoConsole` off the console and
/// makes `eltc(0)` mean what C means by it: both are decisions about *these*
/// bytes, and a `tracing` event carries neither.
fn console_fallback(line: &str) {
    if (nothing_is_listening() || console_subscriber_is_current()) && errlog_to_console() {
        write_console(&mut std::io::stderr().lock(), line);
    }
}

/// The one place errlog bytes become console bytes, and it adds nothing.
///
/// C's console writer is `fprintf(console, "%s", base+1u)` (`errlog.c:795`,
/// and `:170` on the at-exit path): the caller's bytes and no terminator, so a
/// C caller that wants a line break puts `\n` in its own format string and one
/// that does not gets none. This does the same — `write_all`, never
/// `eprintln!`. A console that appends its own newline gives a call site's
/// bytes and the console's bytes two different meanings, and then no call site
/// can be read against its C original: the ones that correctly carry `\n`
/// print a blank line C does not print, and the ones that carry none are
/// silently rescued.
///
/// The sink is a parameter so the framing can be asserted on a buffer; the
/// process console is `stderr`, which is unbuffered, so C's `fflush` after a
/// drain pass has no analogue to skip.
fn write_console(out: &mut impl std::io::Write, line: &str) {
    // C ignores `fprintf`'s return here too: a console that cannot be written
    // is not something an errlog line can report.
    let _ = out.write_all(line.as_bytes());
}

/// The same bytes as a `tracing` record, which is one event and not a byte
/// stream.
///
/// A subscriber terminates an event itself, so handing it the caller's
/// trailing newline as well puts a blank line after every errlog line. The
/// console that shows is an *application's* formatter — `qsrv-rs`,
/// `pva-gateway-rs`, `procserv-rs` and the example IOCs all install
/// `tracing_subscriber::fmt` — and the capture tests that read the errlog sink
/// through one. This crate's own [`ConsoleSubscriber`] no longer renders these
/// events at all ([`ConsoleSubscriber::line_for`]); [`write_console`] writes
/// C's bytes for it, so the two consoles do not disagree. Only the
/// *trailing* newline is framing: the ones inside a multi-line message
/// (`dbScan`'s over-run report, `iocBuild`'s two-line `asInit` failure) are
/// content and stay.
fn as_record(line: &str) -> &str {
    line.strip_suffix('\n').unwrap_or(line)
}

/// A `tracing` subscriber that writes events to the console and nothing else.
///
/// Deliberately not `tracing_subscriber::fmt`: that crate is an optional
/// dependency here, it pulls a Prometheus exporter along with it in the
/// dependents that enable it, and none of what it adds — span storage, env
/// filters, ANSI, timestamps off a clock that is quantised to whole seconds on
/// RTEMS — is wanted on an IOC console. What is wanted is C's property: a
/// diagnostic reaches the console.
struct ConsoleSubscriber;

impl ConsoleSubscriber {
    /// The line this subscriber writes for `event`, or `None` when the event is
    /// errlog's.
    ///
    /// An errlog event's bytes are C's, and [`write_console`] has already put
    /// them on the console exactly as `fprintf(console, "%s", …)` does
    /// (`errlog.c:795`). Rendering them here as well would print each line
    /// twice, the second copy behind a `LEVEL target:` prefix C does not write
    /// — and would re-frame a caller that deliberately composes one console
    /// line out of several unterminated `errlogPrintf` calls, which is what C's
    /// own `dumpInfo` does (`epicsStackTrace.c:46-57`).
    fn line_for(event: &tracing::Event<'_>) -> Option<String> {
        (event.metadata().target() != ERRLOG_TARGET).then(|| render_event(event))
    }
}

/// Renders one event as `LEVEL target: message key=value …`.
struct ConsoleLine {
    out: String,
    wrote_message: bool,
}

impl tracing::field::Visit for ConsoleLine {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if field.name() == "message" {
            // The message field arrives as `format_args!`, whose `Debug` is its
            // `Display` — so this is the text, not a quoted rendering of it.
            let _ = write!(self.out, "{value:?}");
            self.wrote_message = true;
        } else {
            let _ = write!(
                self.out,
                "{}{}={value:?}",
                if self.wrote_message { " " } else { "" },
                field.name()
            );
            self.wrote_message = true;
        }
    }
}

/// `LEVEL target: message key=value …` — the one place an event becomes text.
fn render_event(event: &tracing::Event<'_>) -> String {
    let meta = event.metadata();
    let mut line = ConsoleLine {
        out: format!("{:<5} {}: ", meta.level(), meta.target()),
        wrote_message: false,
    };
    event.record(&mut line);
    line.out
}

impl tracing::Subscriber for ConsoleSubscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::INFO
    }

    /// Declared so [`nothing_is_listening`] is false once this is installed —
    /// without it the `errlog` console fallback would double every line.
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::INFO)
    }

    fn event(&self, event: &tracing::Event<'_>) {
        if let Some(line) = Self::line_for(event) {
            eprintln!("{line}");
        }
    }

    // Spans are not rendered: this crate's diagnostics are events, and storing
    // span data would be the one part of this that needs allocation per span.
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

/// Make this process's diagnostics reach the console, if nothing else has.
///
/// Every diagnostic in this workspace — `errlog`, the `rt_*` macros, and the
/// `tracing::{warn,error,info}!` calls in the CA and PVA servers — funnels into
/// `tracing`, and an event with no subscriber installed is *discarded*, not
/// buffered. An IOC binary that never installs one is therefore mute: measured
/// on target, a CA server refusing clients at its memory ceiling produced no
/// console output of any kind, which is indistinguishable from a network fault.
///
/// C has no such state. `errlogPrintf` and `epicsPrintf` end at a console
/// writer that always exists, so an IOC that is running always says so. This is
/// the entry point that restores that property, and it belongs in the binary
/// rather than in a library: installing a global subscriber is a whole-process
/// decision, and a hosted application that installs its own must win.
///
/// Returns `false` when a subscriber was already installed — the caller's own
/// choice takes precedence and nothing is changed.
pub fn install_console_subscriber() -> bool {
    tracing::subscriber::set_global_default(ConsoleSubscriber).is_ok()
}

/// Set once by [`install_panic_hook`], so a second call cannot chain the hook
/// onto itself and print every panic twice.
static PANIC_HOOK_INSTALLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// One line saying what a panic on this thread costs the IOC.
///
/// A function, and a pure one, because the *consequence* is the part `std`'s
/// default hook does not print and the part nobody can infer from a serial
/// console. `std` says a thread panicked and where; it does not say whether the
/// IOC is still serving.
///
/// The two arms are genuinely different outcomes on the target. The RTEMS build
/// defaults to `panic = "unwind"`, so:
///
/// * on the entry thread the unwind leaves `main`, and the image is finished;
/// * on any other thread — a CA client thread, a PVA connection thread, the
///   status pusher — only that thread dies. The IOC keeps listening, keeps
///   answering searches, and quietly no longer does whatever that thread did.
///   That is the state this line exists to make visible, because it looks
///   exactly like a healthy IOC from outside.
fn panic_announcement(thread: Option<&str>, location: &str, payload: &str) -> String {
    let thread = thread.unwrap_or("<unnamed>");
    let consequence = if thread == "main" {
        "the IOC's entry thread is unwinding: the image is going down, and every \
         connection it serves with it"
    } else {
        "that thread is gone and nothing restarts it; the IOC keeps listening and \
         keeps answering searches, so from outside it still looks healthy"
    };
    format!("panic on thread `{thread}` at {location}: {payload} -- {consequence}")
}

/// The panic payload as text — the message a `panic!`/`assert!` carried.
fn panic_payload(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Route panics through `errlog`, in addition to whatever `std` already does.
///
/// Call it once, in an IOC's `main`, next to [`install_console_subscriber`].
///
/// # Why an IOC needs this and a program does not
///
/// `std`'s default hook writes to stderr, which on the target is the serial
/// console, so a panic is not *invisible* without this. Two things are missing
/// from it, and both matter more on an IOC than in a program:
///
/// 1. **It says nothing about what still works.** A panic on a per-connection
///    thread kills that thread and leaves the IOC listening, answering searches
///    and serving every other client — indistinguishable from health, from
///    outside, forever. The line this emits states which of the two outcomes
///    this was.
/// 2. **It is not on the errlog.** Every other diagnostic an IOC produces goes
///    through `errlog`, and a panic is the most severe thing that can happen to
///    one. Routing it there puts it in the same stream, at
///    [`ErrlogSevEnum::Fatal`], for whatever is reading that stream.
///
/// # It replaces rather than chains
///
/// This used to run `std`'s default hook after its own line, on the reasoning
/// that installing it could then only *add* output. On the target that
/// reasoning does not hold, for three measured reasons:
///
/// 1. **The output would be doubled.** The line below already carries the
///    thread, the panic site and the payload — everything the default hook
///    prints — so chaining puts the same panic on the console twice. It reaches
///    the console either way: `console_fallback` writes it with nothing
///    listening and with [`install_console_subscriber`] in place alike, and an
///    application that installed its own subscriber gets it through that.
/// 2. **The `RUST_BACKTRACE` note is advice that cannot be taken.** There is no
///    environment on the target to set that variable in, so a backtrace is off
///    by construction; printing "run with `RUST_BACKTRACE=1`" on a serial
///    console tells an operator to do something impossible.
/// 3. **The panic path must stay shallow.** The default hook's formatting and
///    backtrace machinery is stack the panic path does not otherwise need, and
///    the per-connection stack ceiling is the thing currently being measured on
///    the target. A hook must not be what makes the panic path deeper than the
///    peak that measurement is establishing.
///
/// The consequence for a hosted build is deliberate and worth stating: a
/// process that calls this gives up `std`'s backtrace-on-panic for the one line
/// below. A host application that wants the backtrace should not install this
/// hook — it is written for an image with no environment and no debugger.
///
/// Returns `false` when it was already installed, having changed nothing.
pub fn install_panic_hook() -> bool {
    use std::sync::atomic::Ordering as AtomicOrdering;
    if PANIC_HOOK_INSTALLED.swap(true, AtomicOrdering::AcqRel) {
        return false;
    }
    std::panic::set_hook(Box::new(|info| {
        let location = match info.location() {
            Some(l) => format!("{}:{}", l.file(), l.line()),
            None => "an unknown location".to_string(),
        };
        let thread = std::thread::current();
        errlog_sev_printf(
            ErrlogSevEnum::Fatal,
            // Terminated by the caller, as every C errlog format string is:
            // the console writer appends nothing (see [`write_console`]).
            &format!(
                "{}\n",
                panic_announcement(thread.name(), &location, &panic_payload(info))
            ),
        );
    }));
    true
}

/// Error-message severity — C `errlogSevEnum` (`errlog.h:49-53`).
///
/// Ordered `Info < Minor < Major < Fatal`; the discriminants match the
/// C enum values so they can be compared as the C code does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ErrlogSevEnum {
    /// `errlogInfo` = 0.
    Info = 0,
    /// `errlogMinor` = 1.
    Minor = 1,
    /// `errlogMajor` = 2.
    Major = 2,
    /// `errlogFatal` = 3.
    Fatal = 3,
}

impl ErrlogSevEnum {
    /// String form — C `errlogSevEnumString` (`errlog.h:60-65`).
    pub fn as_str(self) -> &'static str {
        match self {
            ErrlogSevEnum::Info => "info",
            ErrlogSevEnum::Minor => "minor",
            ErrlogSevEnum::Major => "major",
            ErrlogSevEnum::Fatal => "fatal",
        }
    }

    fn from_u8(v: u8) -> ErrlogSevEnum {
        match v {
            0 => ErrlogSevEnum::Info,
            1 => ErrlogSevEnum::Minor,
            2 => ErrlogSevEnum::Major,
            _ => ErrlogSevEnum::Fatal,
        }
    }
}

/// String representation of an errlog severity.
///
/// C parity: `errlogGetSevEnumString` (`errlog.c:391-397`) — an
/// out-of-range value yields `"unknown"`; the typed Rust enum cannot be
/// out of range, so this always maps to a real name.
pub fn errlog_sev_enum_string(severity: ErrlogSevEnum) -> &'static str {
    severity.as_str()
}

/// Backing store for `errlogSetSevToLog`/`errlogGetSevToLog`. C parity:
/// `pvt.sevToLog` (`errlog.c:96`), which lives in the file-scope static
/// `pvt` and is therefore zero-initialised — `errlogInfo`, not
/// `errlogMinor`. Nothing may consult it; see [`errlog_set_sev_to_log`].
static SEV_TO_LOG: AtomicU8 = AtomicU8::new(ErrlogSevEnum::Info as u8);

/// Store the severity-to-log setting — C `errlogSetSevToLog`
/// (`errlog.c:399-405`).
///
/// This sets a value and changes no behaviour, which is exactly what the
/// C call does: `rg sevToLog` over base finds the struct member
/// (`errlog.c:96`), this store (`:403`) and the read-back in
/// [`errlog_get_sev_to_log`] (`:412`) and nothing else, so no message is
/// ever filtered by it. Do not add a suppression test against this value
/// — that would drop lines a C IOC prints.
pub fn errlog_set_sev_to_log(severity: ErrlogSevEnum) {
    SEV_TO_LOG.store(severity as u8, Ordering::Relaxed);
}

/// Read the severity-to-log setting back — C `errlogGetSevToLog`
/// (`errlog.c:407-415`). Round-trips [`errlog_set_sev_to_log`]; carries
/// no filtering authority.
pub fn errlog_get_sev_to_log() -> ErrlogSevEnum {
    ErrlogSevEnum::from_u8(SEV_TO_LOG.load(Ordering::Relaxed))
}

/// C `ANSI_ESC_RED` (`errlog.h:281`) — the escape that opens a red bold span.
///
/// Present as a constant because two callers need the halves rather than the
/// whole word: this module builds [`ERL_ERROR`] from them, and the iocsh error
/// framer paints a message body the way C's own `showError` call sites do —
/// they spell the format string `ANSI_RED(...)`, so the body arrives painted
/// and the port must wrap it the same way.
pub const ANSI_ESC_RED: &str = "\x1b[31;1m";

/// C `ANSI_ESC_BOLD` (`errlog.h:287`) — opens a bold span.
///
/// Read by `iocsh`'s `format_help_entry`, which paints a command name the way
/// C's `helpCallFunc` does, and by `softIoc`'s `verbose_out`
/// (`softMain.cpp:58`), whose `CMD` colour this is.
pub const ANSI_ESC_BOLD: &str = "\x1b[1m";

/// C `ANSI_ESC_BLUE` (`errlog.h:284`) — opens a blue bold span.
///
/// `verbose_out`'s `REM` colour (`softMain.cpp:58`), and the colour `iocsh`
/// echoes a script comment in.
pub const ANSI_ESC_BLUE: &str = "\x1b[34;1m";

/// C `ANSI_ESC_UNDERLINE` (`errlog.h:288`) — opens an underlined span.
///
/// Read by `iocsh`'s `format_help_entry` for an argument name.
pub const ANSI_ESC_UNDERLINE: &str = "\x1b[4m";

/// C `ANSI_ESC_RESET` (`errlog.h:289`) — closes any span above.
pub const ANSI_ESC_RESET: &str = "\x1b[0m";

// The remaining four of `errlog.h:281-288` — GREEN, YELLOW, MAGENTA, CYAN —
// are deliberately absent: a constant nothing reads is a claim about C nobody
// is checking. Green and magenta do appear in this workspace, but only inside
// whole strings that are already correct and have no half-reader:
// `IOCSH_PS1`'s compiled default (`ANSI_GREEN("epics> ")`) and [`ERL_WARNING`]
// (`ANSI_MAGENTA("WARNING")`).

/// C `ERL_ERROR` (`errlog.h:298`) — `ANSI_RED("ERROR")`, the severity word a
/// diagnostic written straight to stderr carries.
///
/// Unconditional, and that is the whole distinction from [`erl_warning`]. C
/// puts the escapes IN the message and strips them in one place only:
/// `errlogStripANSI` sits inside errlog's own message pump
/// (`errlog.c:671-681`), so it can only reach text that was handed to
/// `errlogPrintf`. A `fprintf(stderr, ERL_ERROR ": …")` never enters that pump,
/// so its escapes reach the stream whatever the stream is — a pipe, a file, a
/// terminal alike. Every `.db`/`.dbd` loader diagnostic is such an `fprintf`.
/// C composes this from the halves (`ANSI_RED("ERROR")`, `errlog.h:290`) and
/// so would we, but `concat!` takes literals and not constants and a
/// const-concat dependency is a poor trade for three tokens. The identity
/// `ERL_ERROR == ANSI_ESC_RED ++ "ERROR" ++ ANSI_ESC_RESET` is asserted in
/// `the_severity_words_carry_c_s_escapes` instead, so the halves and the whole
/// cannot drift apart unnoticed.
pub const ERL_ERROR: &str = "\x1b[31;1mERROR\x1b[0m";

/// C `ERL_WARNING` (`errlog.h:299`) — `ANSI_MAGENTA("WARNING")`, unconditional
/// for the same reason as [`ERL_ERROR`].
///
/// Use [`erl_warning`] instead for a word that goes out through `errlogPrintf`;
/// those DO pass the strip and so must follow the console.
///
/// Written out rather than built from an `ANSI_ESC_MAGENTA` constant because
/// this is magenta's only appearance in the workspace; the constant would have
/// exactly one reader, and a whole word already spelled correctly is not the
/// shape the escapes above earn — those exist because a second crate was
/// otherwise defining `errlog.h`'s bytes for itself.
pub const ERL_WARNING: &str = "\x1b[35;1mWARNING\x1b[0m";

/// The word an *errlog* warning line carries — magenta on a terminal console
/// and plain everywhere else.
///
/// C spells it `ANSI_MAGENTA("WARNING")` ([`ERL_WARNING`]) at the call site
/// either way; the difference is that errlog strips the escapes at print time
/// when its console is not a terminal (`errlog.c:672-681`,
/// `pvt.ttyConsole = isATTY(stderr)` at `errlog.c:555`). `isATTY`
/// (`errlog.c:218-237`) also demands a non-empty `$TERM`, on the grounds that a
/// terminal that will not name itself cannot be assumed to understand escapes.
/// Both halves of that rule are here, so an `epicsEnvSet`-style capture of a
/// Rust IOC's stderr gets the same bytes as C's.
///
/// Verified head-to-head with the compiled `softIoc` (bind-conflict warning):
/// redirected to a file it writes `cas WARNING: …`; under `script(1)` it writes
/// `cas \x1b[35;1mWARNING\x1b[0m: …`.
pub fn erl_warning() -> &'static str {
    if errlog_console_paints() {
        ERL_WARNING
    } else {
        "WARNING"
    }
}

/// Whether an errlog line keeps the ANSI escapes its C literal carries.
///
/// C strips them at print time when the console is not a terminal
/// (`errlog.c:789-793`, `pvt.ttyConsole = isATTY(stderr)` at `:555`), and
/// `isATTY` (`:218-237`) also demands a non-empty `$TERM` on the grounds
/// that a terminal which will not name itself cannot be assumed to
/// understand escapes.
///
/// [`erl_warning`] answers this for one word. A call site whose C literal
/// paints more than one span — `iocBuild`'s `asInit` failure carries both
/// `ERL_ERROR` and an `ANSI_MAGENTA` sentence (`iocInit.c:188-190`) — asks
/// it directly, so the predicate stays owned here rather than being
/// re-derived per site.
pub fn errlog_console_paints() -> bool {
    use std::io::IsTerminal;
    let term_names_itself = std::env::var_os("TERM").is_some_and(|t| !t.is_empty());
    std::io::stderr().is_terminal() && term_names_itself
}

/// Emit a pre-formatted error message at the given severity.
///
/// C parity: `errlogSevVprintf`/`errlogSevPrintf` (`errlog.c:366-388`)
/// — the C code prefixes `"sevr=%s "` and routes to the message queue,
/// unconditionally. Here the prefix is preserved and the message is
/// routed through `tracing` at a level mapped from the severity.
///
/// Unconditionally is the whole point: `errlogSevVprintf` tests no
/// threshold, so an `errlogInfo` line reaches a C console whatever
/// `errlogSetSevToLog` was told. See [`errlog_set_sev_to_log`].
pub fn errlog_sev_printf(severity: ErrlogSevEnum, message: &str) {
    let line = format!("sevr={} {}", severity.as_str(), message);
    let record = as_record(&line);
    match severity {
        ErrlogSevEnum::Info => {
            tracing::info!(target: "epics_base_rs::errlog", "{record}")
        }
        ErrlogSevEnum::Minor => {
            tracing::warn!(target: "epics_base_rs::errlog", "{record}")
        }
        ErrlogSevEnum::Major | ErrlogSevEnum::Fatal => {
            tracing::error!(target: "epics_base_rs::errlog", "{record}")
        }
    }
    errlog_enqueue(&line);
    console_fallback(&line);
}

/// Emit a pre-formatted message through the errlog facility
/// unconditionally — C `errlogVprintf`/`errlogPrintf`
/// (`errlog.c:315-335`), the *no-severity* variant.
///
/// Unlike [`errlog_sev_printf`] this carries no `sevr=` prefix (C
/// `errlogVprintf` enqueues the caller's bytes verbatim). Neither call
/// is gated: see [`errlog_set_sev_to_log`]. Routed through `tracing` at info
/// level on the same `epics_base_rs::errlog` target, so an application's
/// subscriber sees it on the errlog sink. Used by `stdio` device support
/// for the `"errlog"` output stream (`devStdio.c` `logPrintf`).
pub fn errlog_printf(message: &str) {
    tracing::info!(target: "epics_base_rs::errlog", "{}", as_record(message));
    errlog_enqueue(message);
    console_fallback(message);
}

/// C `errlogPrintfNoConsole` (`errlog.c:343-364`): the same message queue,
/// but the console echo is suppressed for this line whatever `eltc` says.
/// Listeners — the IOC log client among them — still receive it.
pub fn errlog_printf_no_console(message: &str) {
    tracing::info!(target: "epics_base_rs::errlog", "{}", as_record(message));
    errlog_enqueue(message);
}

/// C `errlogMessage` (`errlog.c:337-341`) — `errlogPrintf("%s", message)`.
pub fn errlog_message(message: &str) {
    errlog_printf(message);
}

/// Debug-level runtime log line. Routes through the `tracing` facade.
#[macro_export]
macro_rules! rt_debug {
    ($($arg:tt)*) => {
        ::tracing::debug!(target: "epics_base_rs::runtime", "{}", format!($($arg)*));
    };
}

/// Info-level runtime log line. Routes through the `tracing` facade.
#[macro_export]
macro_rules! rt_info {
    ($($arg:tt)*) => {
        ::tracing::info!(target: "epics_base_rs::runtime", "{}", format!($($arg)*));
    };
}

/// Warn-level runtime log line. Routes through the `tracing` facade.
#[macro_export]
macro_rules! rt_warn {
    ($($arg:tt)*) => {
        ::tracing::warn!(target: "epics_base_rs::runtime", "{}", format!($($arg)*));
    };
}

/// Error-level runtime log line. Routes through the `tracing` facade.
#[macro_export]
macro_rules! rt_error {
    ($($arg:tt)*) => {
        ::tracing::error!(target: "epics_base_rs::runtime", "{}", format!($($arg)*));
    };
}

// ---------------------------------------------------------------------------
// The errlog message queue — C `errlog.c` @`R7.0.10`.
// ---------------------------------------------------------------------------

/// C `errlog.c:44`. `errlogInit` raises anything smaller to this.
pub const MIN_BUFFER_SIZE: usize = 1280;
/// C `errlog.c:45` — also the `maxMsgSize` `errlogInit` uses.
pub const MIN_MESSAGE_SIZE: usize = 256;
/// C `errlog.c:46`.
pub const MAX_MESSAGE_SIZE: usize = 0x00ff_ffff;

/// What `errlogAddListener` hands back.
///
/// C keys a listener on the `(function pointer, void *pPrivate)` pair, because
/// that is the only identity a C callback has, and `errlogRemoveListeners`
/// removes *every* node matching it. A Rust closure has no comparable identity
/// — two clones of one closure are indistinguishable and `fn` pointers to
/// generic shims collide — so the registration hands back a token instead.
/// Every registration is therefore removable exactly once and never removes a
/// stranger's, which is a property C's pair-matching does not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ErrlogListenerId(u64);

/// A registered listener, shared with the worker's per-drain snapshot.
type ErrlogListenerFn = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// One buffered message. C stores a flag byte and a NUL-terminated string in a
/// flat arena; the byte layout is not observable, but which messages the arena
/// ACCEPTS is, so `pos` below tracks C's arithmetic exactly.
struct ErrlogBuf {
    entries: Vec<String>,
    /// C `buffer_t::pos` — bytes consumed, `1 + nchar + 1` per message
    /// (`errlog.c:158`).
    pos: usize,
}

impl ErrlogBuf {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            pos: 0,
        }
    }
}

/// Everything C guards with `pvt.msgQueueLock`.
struct ErrlogQueue {
    buf_size: usize,
    max_msg_size: usize,
    log: ErrlogBuf,
    print: ErrlogBuf,
    n_lost: usize,
    at_exit: bool,
    to_console: bool,
    flush_seq: u64,
}

struct Errlog {
    queue: std::sync::Mutex<ErrlogQueue>,
    /// C `pvt.waitForWork`.
    work: std::sync::Condvar,
    /// C `pvt.waitForSeq`.
    seq: std::sync::Condvar,
    listeners: std::sync::Mutex<Vec<(ErrlogListenerId, ErrlogListenerFn)>>,
    next_id: std::sync::atomic::AtomicU64,
}

static ERRLOG: std::sync::OnceLock<&'static Errlog> = std::sync::OnceLock::new();

/// C `errlogInit(bufsize)` — `errlogInit2(bufsize, MIN_MESSAGE_SIZE)`
/// (`errlog.c:609-612`). Idempotent: C runs `errlogInitPvt` under
/// `epicsThreadOnce`, so the FIRST call fixes the sizes and every later one is
/// a no-op whatever it asks for.
pub fn errlog_init(bufsize: usize) {
    errlog_init2(bufsize, MIN_MESSAGE_SIZE);
}

/// C `errlogInit2` (`errlog.c:583-607`): both sizes are clamped before the
/// once-init, and the worker thread is started there.
pub fn errlog_init2(bufsize: usize, max_msg_size: usize) {
    errlog_pvt2(bufsize, max_msg_size);
}

/// [`errlog_init`]'s private half — the once-initialised state itself. Every
/// entry point below starts here, exactly as every C entry point starts with
/// `errlogInit(0)`.
fn errlog_pvt() -> &'static Errlog {
    errlog_pvt2(0, MIN_MESSAGE_SIZE)
}

fn errlog_pvt2(bufsize: usize, max_msg_size: usize) -> &'static Errlog {
    ERRLOG.get_or_init(|| {
        let (buf_size, max_msg_size) = errlog_clamp_sizes(bufsize, max_msg_size);
        let errlog: &'static Errlog = Box::leak(Box::new(Errlog {
            queue: std::sync::Mutex::new(ErrlogQueue {
                buf_size,
                max_msg_size,
                log: ErrlogBuf::new(),
                print: ErrlogBuf::new(),
                n_lost: 0,
                at_exit: false,
                // C `errlogInitPvt`: `pvt.toConsole = TRUE`.
                to_console: true,
                flush_seq: 0,
            }),
            work: std::sync::Condvar::new(),
            seq: std::sync::Condvar::new(),
            listeners: std::sync::Mutex::new(Vec::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }));
        // C `epicsThreadCreateOpt("errlog", …)` at `epicsThreadPriorityLow`
        // with `epicsThreadStackSmall` (`errlog.c:568-574`).
        let spawned = crate::runtime::task::spawn_dedicated_thread(
            "errlog".to_string(),
            crate::runtime::task::ThreadPriority::Low,
            crate::runtime::task::StackSizeClass::Small,
            move || errlog_worker(errlog),
        );
        if spawned.is_err() {
            // C exits the process when the thread cannot be created
            // (`errlog.c:604-606`). Here the queue still accepts and still
            // accounts; only delivery stops, so say so rather than kill an
            // IOC over a log sink.
            eprintln!("errlogInit failed: no errlog thread, listeners will not be called");
        }
        errlog
    })
}

/// C `errlogThread` (`errlog.c:624-720`): swap the buffers, drain the snapshot
/// with the queue UNLOCKED, then report anything the arena refused.
fn errlog_worker(errlog: &'static Errlog) {
    let mut q = errlog.queue.lock().expect("errlog queue");
    loop {
        q.flush_seq += 1;
        errlog.seq.notify_all();

        if q.log.entries.is_empty() {
            if q.at_exit {
                break;
            }
            q = errlog.work.wait(q).expect("errlog queue");
            continue;
        }

        let n_lost = std::mem::take(&mut q.n_lost);
        let to_console = q.to_console;
        // C swaps `pvt.log` and `pvt.print` so logging can continue while
        // this pass drains (`errlog.c:650-655`); the spare buffer goes back
        // as `q.print` at the bottom of the loop.
        let spare = std::mem::replace(&mut q.print, ErrlogBuf::new());
        let mut print = std::mem::replace(&mut q.log, spare);
        drop(q);

        // A snapshot, not the live list: a listener that removes itself from
        // inside its own callback is C's `active`/`removed` dance
        // (`errlog.c:684-697`), and taking a copy makes that state
        // unrepresentable — `errlog_remove_listener` can never deadlock
        // against a running callback, and the removal simply takes effect on
        // the next drain.
        let listeners: Vec<_> = errlog
            .listeners
            .lock()
            .expect("errlog listeners")
            .iter()
            .map(|(_, f)| std::sync::Arc::clone(f))
            .collect();

        for text in print.entries.drain(..) {
            // C strips before the listeners see it, always (`errlog.c:678-681`).
            let stripped = errlog_strip_ansi(&text);
            for listener in &listeners {
                listener(&stripped);
            }
        }
        print.pos = 0;

        if n_lost > 0 && to_console {
            eprintln!("errlog: lost {n_lost} messages");
        }

        q = errlog.queue.lock().expect("errlog queue");
        q.print = print;
    }
}

/// C `msgbufAlloc`/`msgbufCommit` (`errlog.c:113-180`) as one step.
///
/// The admission rule is C's and is what makes the buffer a bound rather than
/// a `Vec` that grows: a message is accepted only when the WORST CASE still
/// fits — `bufSize - pos >= 1 + maxMsgSize` — so a burst is dropped and
/// counted instead of consuming memory, and the count reaches the console as
/// `errlog: lost N messages`.
fn errlog_enqueue(message: &str) {
    let errlog = errlog_pvt();
    let mut q = errlog.queue.lock().expect("errlog queue");
    let was_empty = q.log.pos == 0;
    let accepted = q.accept(message);
    drop(q);
    if accepted && was_empty {
        errlog.work.notify_all();
    }
}

/// C `msgbufCommit`'s truncation marker (`errlog.c:151`).
const TRUNCATED: &str = "<<TRUNCATED>>\n";

impl ErrlogQueue {
    /// The whole admission decision, in one place so it can be tested at its
    /// boundaries without a worker thread. `true` when the message was taken.
    fn accept(&mut self, message: &str) -> bool {
        if self.at_exit {
            return false;
        }
        // C `msgbufAlloc` (`errlog.c:124-128`): a message is taken only when
        // the WORST CASE still fits, so the arena bounds memory instead of
        // growing, and the refusals are counted for the console.
        if self.buf_size - self.log.pos < 1 + self.max_msg_size {
            self.n_lost += 1;
            return false;
        }

        // C `msgbufCommit` (`errlog.c:145-155`): `nchar` is what `snprintf`
        // WOULD have written, so a message at or past `maxMsgSize` is cut to
        // `maxMsgSize - 1` bytes with its tail overwritten by the marker.
        let max = self.max_msg_size;
        let text = if message.len() >= max {
            let mut cut = (max - 1).saturating_sub(TRUNCATED.len());
            while cut > 0 && !message.is_char_boundary(cut) {
                cut -= 1;
            }
            let mut t = String::with_capacity(max);
            t.push_str(&message[..cut]);
            t.push_str(TRUNCATED);
            t
        } else {
            message.to_string()
        };

        self.log.pos += 1 + text.len() + 1;
        self.log.entries.push(text);
        true
    }
}

/// C `errlogInit2`'s two clamps (`errlog.c:591-599`), extracted so the
/// boundaries are testable after the once-init has already run.
fn errlog_clamp_sizes(bufsize: usize, max_msg_size: usize) -> (usize, usize) {
    (
        bufsize.max(MIN_BUFFER_SIZE),
        max_msg_size.clamp(MIN_MESSAGE_SIZE, MAX_MESSAGE_SIZE),
    )
}

/// C `errlogSequence` (`errlog.c:189-217`): block until the worker completes
/// one pass of its loop.
fn errlog_sequence() {
    let errlog = errlog_pvt();
    let mut q = errlog.queue.lock().expect("errlog queue");
    if q.at_exit {
        return;
    }
    let seq = q.flush_seq;
    while q.flush_seq == seq && !q.at_exit {
        errlog.work.notify_all();
        q = errlog.seq.wait(q).expect("errlog queue");
    }
}

/// C `errlogFlush` (`errlog.c:614-622`): TWO sequences, because it takes both
/// buffers being handled to know every message logged so far has been seen.
pub fn errlog_flush() {
    errlog_sequence();
    errlog_sequence();
}

/// C `errlogAddListener` (`errlog.c:417-431`).
///
/// The listener is called on the errlog worker thread with the message text
/// after ANSI stripping — never on the thread that logged it.
pub fn errlog_add_listener<F>(listener: F) -> ErrlogListenerId
where
    F: Fn(&str) + Send + Sync + 'static,
{
    let errlog = errlog_pvt();
    let id = ErrlogListenerId(
        errlog
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    );
    errlog
        .listeners
        .lock()
        .expect("errlog listeners")
        .push((id, std::sync::Arc::new(listener)));
    id
}

/// Remove the listener registered under `id`; `true` when one was there.
///
/// C's `errlogRemoveListeners(listener, pPrivate)` returns how many nodes
/// matched the pair (`errlog.c:434-462`); a token matches at most one, so the
/// count degenerates to a boolean. Safe to call from inside a listener: the
/// worker drains against a snapshot, so this takes the lock immediately and
/// the removal applies from the next message on.
pub fn errlog_remove_listener(id: ErrlogListenerId) -> bool {
    let errlog = errlog_pvt();
    let mut listeners = errlog.listeners.lock().expect("errlog listeners");
    let before = listeners.len();
    listeners.retain(|(other, _)| *other != id);
    listeners.len() != before
}

/// How many listeners are registered — C has no such call; this exists for
/// the tests and for `iocLogShow`.
#[must_use]
pub fn errlog_listener_count() -> usize {
    errlog_pvt()
        .listeners
        .lock()
        .expect("errlog listeners")
        .len()
}

/// C `errlogShow(level)` (`errlog.c:694-720`). Returns the lines rather than
/// printing them, so the iocsh command can send them through its own
/// redirected output — the same split `ioc_log_show` uses.
///
/// Level 0 is the two sizes, level 1 adds the listener count, level 2 adds
/// both buffers. The first three quantities are C's own, read from the port's
/// `buf_size`, `max_msg_size` and listener list.
///
/// The buffer dump is NOT C's bytes, and stating what it is beats inventing a
/// field to match C's line shape. C packs the queued messages into a flat
/// arena and prints `pvt.log->base`, which `printf("%s")` cuts at the first
/// NUL — so C shows the FIRST queued message — then `printf("%*s^\n", pos,
/// "")`, a caret standing at the byte offset the next message would go to.
/// The port's queue is a `Vec<String>` carrying C's `pos` arithmetic
/// alongside it (`pos` is what decides acceptance, so it is kept exactly),
/// and there is no arena for a caret to point into. So every queued message
/// is printed, one per line, and the position is stated as a number instead
/// of drawn.
#[must_use]
pub fn errlog_show(level: u32) -> Vec<String> {
    let errlog = errlog_pvt();
    // Snapshot under the queue lock, format outside it: the queue is the one
    // lock a formatting path must not be holding if it ever logs.
    let (buf_size, max_msg_size, log, print) = {
        let q = errlog.queue.lock().expect("errlog queue");
        (
            q.buf_size,
            q.max_msg_size,
            (q.log.entries.clone(), q.log.pos),
            (q.print.entries.clone(), q.print.pos),
        )
    };

    let mut out = vec![
        "Error log:".to_string(),
        format!("  buffer size: {buf_size}"),
        format!("  max message size: {max_msg_size}"),
    ];
    if level > 0 {
        // Taken after the queue lock is released, never under it.
        out.push(format!(
            "  number of listeners: {}",
            errlog_listener_count()
        ));
    }
    if level > 1 {
        for (which, (entries, pos)) in [("log", log), ("print", print)] {
            out.push(format!("  buffer({which}) contents:"));
            out.extend(
                entries
                    .iter()
                    .map(|msg| format!("    {}", msg.trim_end_matches('\n'))),
            );
            out.push(format!(
                "  buffer({which}) position: {pos} of {buf_size} bytes"
            ));
        }
    }
    out
}

/// C `eltc(yesno)` (`errlog.c:465-473`) — "error log to console". Returns the
/// previous setting.
///
/// C's console lives in the errlog worker, so `eltc` gates the worker's
/// `fprintf`. The port's console is the `tracing` facade, emitted on the
/// thread that logged the message so a scoped subscriber and the caller's span
/// still see it; `eltc` therefore gates that call site instead. The setting
/// and its observable — a quiet console — are the same either way.
pub fn eltc(yesno: bool) -> bool {
    let errlog = errlog_pvt();
    let previous = {
        let mut q = errlog.queue.lock().expect("errlog queue");
        std::mem::replace(&mut q.to_console, yesno)
    };
    errlog_flush();
    previous
}

/// Whether errlog messages currently reach the console — the `eltc` setting.
#[must_use]
pub fn errlog_to_console() -> bool {
    errlog_pvt().queue.lock().expect("errlog queue").to_console
}

/// How many messages the buffer has refused since the last drain reported.
#[must_use]
pub fn errlog_messages_lost() -> usize {
    errlog_pvt().queue.lock().expect("errlog queue").n_lost
}

/// C `errlogStripANSI` (`errlog.c:269-313`) — remove CSI escape sequences.
///
/// Transcribed rather than written afresh, edges included: a lone `ESC` not
/// followed by `[` loses only the `ESC`, and a CSI run ends at the first byte
/// outside `?;0-9` — consuming one final letter if there is one, and nothing
/// if the sequence is truncated. Only ASCII bytes are ever dropped, so UTF-8
/// text survives.
#[must_use]
pub fn errlog_strip_ansi(message: &str) -> String {
    let b = message.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != 0x1b {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1; // the ESC itself is dropped
        if i < b.len() && b[i] == b'[' {
            i += 1;
            while i < b.len() && (b[i] == b'?' || b[i] == b';' || b[i].is_ascii_digit()) {
                i += 1;
            }
            if i < b.len() && b[i].is_ascii_alphabetic() {
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn test_log_macros_compile() {
        rt_debug!("debug message {}", 42);
        rt_info!("info message");
        rt_warn!("warn: {}", "something");
        rt_error!("error: {} {}", "bad", "thing");
    }

    /// The condition the console fallback keys on. With no subscriber the
    /// `tracing` dispatcher reports `OFF`, and every errlog line in the
    /// process is being discarded — the state each RTEMS IOC binary runs in,
    /// because installing a subscriber is the application's job and those
    /// entry points do not do it (`tracing-subscriber` sits behind an
    /// optional feature that also pulls a Prometheus exporter).
    #[test]
    #[serial]
    fn with_no_subscriber_nothing_is_listening() {
        assert!(
            nothing_is_listening(),
            "the test process has no global subscriber, so errlog output is \
             being discarded and the console fallback must engage"
        );
    }

    /// …and with one installed the fallback must stand down, or every hosted
    /// IOC gets each errlog line twice: once through its own sink and once on
    /// stderr.
    #[test]
    #[serial]
    fn with_a_subscriber_the_fallback_stands_down() {
        use tracing::subscriber::with_default;
        let captured = with_default(tracing_subscriber::registry(), nothing_is_listening);
        assert!(
            !captured,
            "a scoped subscriber is listening, so the console fallback must not fire"
        );
    }

    /// The console subscriber must be one of the subscribers that stands the
    /// fallback down. It is not automatic: a `Subscriber` whose
    /// `max_level_hint` is left at the default reports no hint, and this file's
    /// own fallback would then print every errlog line a second time on the
    /// very target the subscriber exists for.
    #[test]
    #[serial]
    fn the_console_subscriber_declares_itself_to_the_dispatcher() {
        use tracing::level_filters::LevelFilter;
        use tracing::subscriber::with_default;

        let (still_mute, level) = with_default(ConsoleSubscriber, || {
            (nothing_is_listening(), LevelFilter::current())
        });
        assert!(
            !still_mute,
            "the console subscriber is listening, so the errlog fallback must \
             not also print — that is every line twice"
        );
        assert_eq!(
            level,
            LevelFilter::INFO,
            "the console takes INFO and above, matching a C IOC's errlog console"
        );
    }

    /// A capturing stand-in that shares the console's rendering. What it
    /// asserts is [`render_event`], which is the whole of what the console
    /// subscriber does with an event.
    struct CapturingSubscriber(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl tracing::Subscriber for CapturingSubscriber {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() <= tracing::Level::INFO
        }
        fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
            Some(tracing::level_filters::LevelFilter::INFO)
        }
        fn event(&self, event: &tracing::Event<'_>) {
            self.0.lock().expect("sink").push(render_event(event));
        }
        fn new_span(&self, _s: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _s: &tracing::span::Id, _v: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _s: &tracing::span::Id, _f: &tracing::span::Id) {}
        fn enter(&self, _s: &tracing::span::Id) {}
        fn exit(&self, _s: &tracing::span::Id) {}
    }

    /// The rendered line carries the message *unquoted* and the structured
    /// fields as `key=value`. The message field arrives as `format_args!`, so
    /// rendering it through `Debug` is what keeps it readable; switching to a
    /// `record_str` arm would wrap every diagnostic in quotes.
    #[test]
    #[serial]
    fn the_console_line_carries_message_and_fields() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        tracing::subscriber::with_default(CapturingSubscriber(seen.clone()), || {
            tracing::warn!(target: "epics_base_rs::test", nth = 7, "refused a client");
            tracing::debug!(target: "epics_base_rs::test", "not at console level");
        });

        let lines = seen.lock().expect("sink").clone();
        assert_eq!(
            lines,
            vec!["WARN  epics_base_rs::test: refused a client nth=7".to_string()],
            "one INFO-or-above event, message unquoted, fields appended"
        );
    }

    /// Below-INFO events must not reach the console — asserted above by the
    /// `debug!` that produced no line, and here at the filter itself so the
    /// reason is not mistaken for a rendering accident.
    #[test]
    #[serial]
    fn the_console_subscriber_declines_below_info() {
        use tracing::level_filters::LevelFilter;
        let taken = tracing::subscriber::with_default(ConsoleSubscriber, || {
            LevelFilter::current() >= LevelFilter::DEBUG
        });
        assert!(!taken, "DEBUG must be below the console's level");
    }

    #[test]
    fn sev_enum_strings_match_c() {
        // C `errlogSevEnumString` (errlog.h:60-65).
        assert_eq!(errlog_sev_enum_string(ErrlogSevEnum::Info), "info");
        assert_eq!(errlog_sev_enum_string(ErrlogSevEnum::Minor), "minor");
        assert_eq!(errlog_sev_enum_string(ErrlogSevEnum::Major), "major");
        assert_eq!(errlog_sev_enum_string(ErrlogSevEnum::Fatal), "fatal");
    }

    #[test]
    fn sev_enum_ordering() {
        assert!(ErrlogSevEnum::Info < ErrlogSevEnum::Minor);
        assert!(ErrlogSevEnum::Minor < ErrlogSevEnum::Major);
        assert!(ErrlogSevEnum::Major < ErrlogSevEnum::Fatal);
    }

    #[test]
    #[serial(errlog_sev)]
    fn sev_to_log_threshold_roundtrips() {
        errlog_set_sev_to_log(ErrlogSevEnum::Major);
        assert_eq!(errlog_get_sev_to_log(), ErrlogSevEnum::Major);
        // Restore the C default.
        errlog_set_sev_to_log(ErrlogSevEnum::Info);
        assert_eq!(errlog_get_sev_to_log(), ErrlogSevEnum::Info);
    }

    /// C's `pvt` is a file-scope static, so `pvt.sevToLog` starts at 0 —
    /// `errlogInfo`. Every test in the `errlog_sev` group restores that
    /// value, so this holds whichever order they run in.
    #[test]
    #[serial(errlog_sev)]
    fn sev_to_log_defaults_to_info_like_c_zero_init() {
        assert_eq!(
            errlog_get_sev_to_log(),
            ErrlogSevEnum::Info,
            "C zero-initialises pvt.sevToLog to errlogInfo, not errlogMinor"
        );
    }

    /// The setting is inert. `errlogSevVprintf` (`errlog.c:376-388`) tests
    /// no threshold, and no other C function reads `pvt.sevToLog`, so a
    /// C IOC prints `sevr=info` lines even after `errlogSetSevToLog(major)`
    /// — e.g. `devBiDbState`'s "Creating new db state" notice at iocInit.
    #[test]
    #[serial(errlog_sev)]
    fn sev_printf_emits_every_severity_whatever_sev_to_log_says() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        errlog_set_sev_to_log(ErrlogSevEnum::Fatal);
        tracing::subscriber::with_default(CapturingSubscriber(seen.clone()), || {
            errlog_sev_printf(ErrlogSevEnum::Info, "creating new db state 'mystate'");
            errlog_sev_printf(ErrlogSevEnum::Minor, "a minor complaint");
            errlog_sev_printf(ErrlogSevEnum::Major, "a major complaint");
        });
        errlog_set_sev_to_log(ErrlogSevEnum::Info);

        let lines = seen.lock().expect("sink").clone();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("sevr=info creating new db state 'mystate'")),
            "sevToLog=fatal must not suppress an errlogInfo line: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("sevr=minor a minor complaint")),
            "sevToLog=fatal must not suppress an errlogMinor line: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("sevr=major a major complaint")),
            "sevToLog=fatal must not suppress an errlogMajor line: {lines:?}"
        );
    }
    /// The distinction the whole hook exists for. A panic on a worker thread
    /// leaves an IOC that still listens and still answers searches, which is
    /// indistinguishable from health from outside; the announcement has to say
    /// so, because nothing else will.
    #[test]
    fn a_worker_panic_says_the_ioc_is_still_up_and_no_longer_whole() {
        let line = panic_announcement(
            Some("CAS-client-3"),
            "blocking.rs:412",
            "index out of bounds",
        );
        assert!(line.contains("CAS-client-3"), "{line}");
        assert!(line.contains("blocking.rs:412"), "{line}");
        assert!(line.contains("index out of bounds"), "{line}");
        assert!(
            line.contains("keeps listening"),
            "a worker panic must say the IOC survives it, or the console reads \
             like the IOC died when it did not: {line}"
        );
        assert!(
            !line.contains("going down"),
            "a worker panic must not claim the image is finished: {line}"
        );
    }

    /// The other outcome, which is the opposite claim and must not be confused
    /// with it: the RTEMS build unwinds, so a panic that leaves `main` ends the
    /// image.
    #[test]
    fn an_entry_thread_panic_says_the_image_is_finished() {
        let line = panic_announcement(Some("main"), "realtime-ca-ioc.rs:118", "iocInit failed");
        assert!(
            line.contains("going down"),
            "a panic out of the entry thread ends the image, and the console is \
             the only place that can say so: {line}"
        );
        assert!(!line.contains("keeps listening"), "{line}");
    }

    /// RTEMS threads that were not named through `thread::Builder` have no
    /// name, and the line must still identify itself rather than render an
    /// empty pair of backticks.
    #[test]
    fn an_unnamed_thread_is_still_named_something() {
        let line = panic_announcement(None, "x.rs:1", "boom");
        assert!(line.contains("<unnamed>"), "{line}");
        assert!(
            line.contains("keeps listening"),
            "an unnamed thread is not the entry thread — std names that one \
             `main` — so it takes the worker consequence: {line}"
        );
    }

    /// Installing twice must not chain the hook onto itself: that prints every
    /// panic once per install, and the second copy looks like a second panic.
    ///
    /// Restores the default hook afterwards so a `cargo test` run — which,
    /// unlike `cargo nextest`, shares one process across tests — is not left
    /// with this one.
    #[test]
    #[serial]
    fn the_panic_hook_installs_once() {
        assert!(install_panic_hook(), "the first install takes effect");
        assert!(
            !install_panic_hook(),
            "a second install must be refused, not chained onto the first"
        );
        let _ = std::panic::take_hook();
    }

    /// The hook replaces the previous one; it does not run it afterwards.
    ///
    /// Chaining would print the panic twice — this hook's line already carries
    /// the thread, site and payload — and would append `std`'s
    /// "run with `RUST_BACKTRACE=1`" note, which on the target is advice for an
    /// environment that does not exist. A sentinel hook proves the absence
    /// directly: if the previous hook still ran, it would flip the flag.
    #[test]
    #[serial]
    fn the_panic_hook_does_not_run_the_hook_it_replaced() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

        let previous_ran = Arc::new(AtomicBool::new(false));
        let flag = previous_ran.clone();
        std::panic::set_hook(Box::new(move |_| {
            flag.store(true, AtomicOrdering::SeqCst);
        }));

        assert!(install_panic_hook(), "the install takes effect");
        let caught = std::panic::catch_unwind(|| panic!("a panic the hook must report once"));
        assert!(caught.is_err(), "the panic was raised");

        assert!(
            !previous_ran.load(AtomicOrdering::SeqCst),
            "the replaced hook must not run: chaining it doubles the console \
             output and appends a RUST_BACKTRACE note that cannot be acted on"
        );
        let _ = std::panic::take_hook();
    }

    fn test_queue() -> ErrlogQueue {
        let (buf_size, max_msg_size) = errlog_clamp_sizes(0, 0);
        ErrlogQueue {
            buf_size,
            max_msg_size,
            log: ErrlogBuf::new(),
            print: ErrlogBuf::new(),
            n_lost: 0,
            at_exit: false,
            to_console: true,
            flush_seq: 0,
        }
    }

    /// Boundary: both clamps. C raises a small `bufsize` to `MIN_BUFFER_SIZE`
    /// and holds `maxMsgSize` inside `[MIN_MESSAGE_SIZE, MAX_MESSAGE_SIZE]`
    /// (`errlog.c:591-599`), so `errlogInit(0)` — the call every entry point
    /// makes — produces 1280/256 and not 0/0.
    #[test]
    fn errlog_init_clamps_both_sizes_the_way_c_does() {
        assert_eq!(
            errlog_clamp_sizes(0, 0),
            (MIN_BUFFER_SIZE, MIN_MESSAGE_SIZE)
        );
        assert_eq!(
            errlog_clamp_sizes(MIN_BUFFER_SIZE - 1, MIN_MESSAGE_SIZE - 1),
            (MIN_BUFFER_SIZE, MIN_MESSAGE_SIZE)
        );
        assert_eq!(
            errlog_clamp_sizes(MIN_BUFFER_SIZE + 1, MIN_MESSAGE_SIZE + 1),
            (MIN_BUFFER_SIZE + 1, MIN_MESSAGE_SIZE + 1),
            "a size above the floor is kept"
        );
        assert_eq!(
            errlog_clamp_sizes(0, MAX_MESSAGE_SIZE + 1).1,
            MAX_MESSAGE_SIZE,
            "and the ceiling holds too"
        );
    }

    /// Boundary: the admission rule. C takes a message only when the WORST
    /// case still fits — `bufSize - pos >= 1 + maxMsgSize` — so with the
    /// default 1280/256 the arena stops accepting once `pos` passes 1023,
    /// whatever the actual message lengths were. This is what makes the
    /// buffer a bound; a `Vec` that simply grew would never refuse.
    #[test]
    fn the_buffer_refuses_and_counts_once_the_worst_case_no_longer_fits() {
        let mut q = test_queue();
        let msg = "0123456789"; // 10 bytes, so 12 bytes of `pos` each
        let mut taken = 0;
        while q.accept(msg) {
            taken += 1;
            assert!(taken < 1000, "the arena must stop accepting");
        }
        // `pos` may reach 1023 and still admit, so the last accepted message
        // is the one that starts at 1020 — 86 of them, ending with `pos` past
        // the limit at 1032.
        assert_eq!(taken, 86, "the last message that starts at or below 1023");
        assert_eq!(q.log.pos, 86 * 12);
        assert_eq!(q.n_lost, 1, "the first refusal is counted");
        assert!(!q.accept(msg));
        assert_eq!(q.n_lost, 2, "and so is the next");
        assert_eq!(q.log.entries.len(), 86, "nothing lost was stored");
    }

    /// Boundary: exactly at the limit. `pos == buf_size - (1 + max_msg_size)`
    /// still admits; one byte past does not.
    #[test]
    fn the_admission_boundary_is_inclusive() {
        let mut q = test_queue();
        q.log.pos = q.buf_size - (1 + q.max_msg_size);
        assert!(q.accept("x"), "the last worst case that fits is taken");
        let mut q = test_queue();
        q.log.pos = q.buf_size - q.max_msg_size;
        assert!(!q.accept("x"), "one byte past it is not");
        assert_eq!(q.n_lost, 1);
    }

    /// Boundary: a message at `maxMsgSize`. C cuts it to `maxMsgSize - 1`
    /// bytes and overwrites the tail with `<<TRUNCATED>>\n`
    /// (`errlog.c:148-154`) — the marker replaces text, it is not appended.
    #[test]
    fn an_oversized_message_is_cut_to_max_minus_one_with_the_marker_as_its_tail() {
        let mut q = test_queue();
        let max = q.max_msg_size;
        assert!(q.accept(&"a".repeat(max)));
        let stored = &q.log.entries[0];
        assert_eq!(stored.len(), max - 1, "C's `nchar = maxMsgSize - 1`");
        assert!(stored.ends_with(TRUNCATED), "{stored}");

        // One byte under the limit is stored whole.
        let mut q = test_queue();
        assert!(q.accept(&"a".repeat(max - 1)));
        assert_eq!(q.log.entries[0].len(), max - 1);
        assert!(!q.log.entries[0].contains("TRUNCATED"));
    }

    /// The two severity words expand exactly as `errlog.h:290-299` does:
    /// `ANSI_ESC_RED` / `ANSI_ESC_MAGENTA`, the word, `ANSI_ESC_RESET`.
    /// Measured against `softIoc` @`R7.0.10` with stderr redirected to a FILE,
    /// where a terminal-conditional word would have come out bare and does not
    /// — `dbLoadRecords("nosuch.db")` writes these bytes either way.
    #[test]
    fn the_severity_words_carry_c_s_escapes() {
        assert_eq!(ERL_ERROR, "\u{1b}[31;1mERROR\u{1b}[0m");
        // The halves and the whole are one definition, the way C's
        // `ANSI_RED("ERROR")` makes them one (`errlog.h:290`).
        assert_eq!(ERL_ERROR, format!("{ANSI_ESC_RED}ERROR{ANSI_ESC_RESET}"));
        assert_eq!(
            errlog_strip_ansi(&format!("{ANSI_ESC_RED}x{ANSI_ESC_RESET}")),
            "x"
        );
        assert_eq!(ERL_WARNING, "\u{1b}[35;1mWARNING\u{1b}[0m");
        // The pair errlog itself hands to `errlogStripANSI` — a wrapped word
        // and the word alone must be the same message to a log listener, or
        // the console and the listener disagree about what was logged.
        assert_eq!(errlog_strip_ansi(ERL_ERROR), "ERROR");
        assert_eq!(errlog_strip_ansi(ERL_WARNING), "WARNING");
        // `erl_warning` is the errlogPrintf-side twin: same bytes when the
        // console is a terminal, stripped by errlog when it is not. It may
        // never be substituted for the constant at a direct-`fprintf` site,
        // and this is the assertion that would fail if it were — under
        // `cargo test` stderr is not a terminal.
        assert_eq!(erl_warning(), "WARNING");
    }

    /// The escapes `iocsh` and `softIoc` paint with, pinned to the macros they
    /// port. They used to be a second copy in `epics-base-rs`, where nothing
    /// compared them with `errlog.h`.
    #[test]
    fn the_span_escapes_are_errlog_h_s() {
        assert_eq!(ANSI_ESC_RED, "\u{1b}[31;1m"); // errlog.h:281
        assert_eq!(ANSI_ESC_BLUE, "\u{1b}[34;1m"); // errlog.h:284
        assert_eq!(ANSI_ESC_BOLD, "\u{1b}[1m"); // errlog.h:287
        assert_eq!(ANSI_ESC_UNDERLINE, "\u{1b}[4m"); // errlog.h:288
        assert_eq!(ANSI_ESC_RESET, "\u{1b}[0m"); // errlog.h:289
        // Every one of them is a CSI sequence `errlogStripANSI` removes, which
        // is what lets a log listener see the same message as the console.
        for esc in [
            ANSI_ESC_RED,
            ANSI_ESC_BLUE,
            ANSI_ESC_BOLD,
            ANSI_ESC_UNDERLINE,
        ] {
            assert_eq!(errlog_strip_ansi(&format!("{esc}x{ANSI_ESC_RESET}")), "x");
        }
    }

    /// Boundary: `errlogStripANSI` (`errlog.c:269-313`). A listener always
    /// sees stripped text, so a colourised warning does not reach a site's
    /// log server as escape bytes.
    #[test]
    fn ansi_stripping_matches_the_c_state_machine() {
        assert_eq!(errlog_strip_ansi("plain"), "plain");
        assert_eq!(
            errlog_strip_ansi("cas \x1b[35;1mWARNING\x1b[0m: bind"),
            "cas WARNING: bind"
        );
        assert_eq!(
            errlog_strip_ansi("\x1b[?25lhidden\x1b[?25h"),
            "hidden",
            "`?` is part of a CSI parameter run"
        );
        assert_eq!(
            errlog_strip_ansi("a\x1bZb"),
            "aZb",
            "an ESC not followed by `[` loses only the ESC"
        );
        assert_eq!(
            errlog_strip_ansi("a\x1b"),
            "a",
            "a trailing lone ESC is dropped"
        );
        assert_eq!(
            errlog_strip_ansi("a\x1b[31"),
            "a",
            "a truncated CSI consumes the parameter run and stops"
        );
        assert_eq!(
            errlog_strip_ansi("\u{c624}\u{b958}\x1b[0m"),
            "\u{c624}\u{b958}",
            "only ASCII bytes are dropped, so UTF-8 survives"
        );
    }

    /// The console owner appends nothing, which is the whole invariant: C
    /// writes an already-formatted line with `fprintf(console, "%s", base+1u)`
    /// (`errlog.c:795`, and `:170` on the at-exit path), so the caller's format
    /// string owns the framing. A message that ends in `\n` must not gain a
    /// second one and a message that does not must not gain one.
    #[test]
    fn the_console_writes_the_callers_bytes_and_appends_nothing() {
        let mut terminated = Vec::new();
        write_console(&mut terminated, "iocPause: IOC suspended\n");
        assert_eq!(terminated, b"iocPause: IOC suspended\n");

        let mut bare = Vec::new();
        write_console(&mut bare, "dbConvertJSON: ");
        assert_eq!(bare, b"dbConvertJSON: ");
    }

    /// The subscriber's skip is keyed on a target the `tracing` macros spell as
    /// a literal, so this is what stops the two spellings drifting apart.
    #[test]
    #[serial]
    fn the_errlog_target_is_the_one_the_macros_publish_on() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        tracing::subscriber::with_default(CapturingSubscriber(seen.clone()), || {
            errlog_printf("x\n");
        });
        let lines = seen.lock().expect("sink").clone();
        assert!(
            lines
                .iter()
                .any(|l| l.contains(&format!("{ERRLOG_TARGET}:"))),
            "{lines:?}"
        );
    }

    /// `ConsoleSubscriber` is not the errlog console. Rendering an errlog event
    /// would print C's bytes a second time behind a `LEVEL target:` prefix C
    /// never writes, so it declines them and takes every other target.
    #[test]
    #[serial]
    fn the_console_subscriber_declines_errlog_events_and_takes_the_rest() {
        let seen: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&seen);
        struct Probe(std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>);
        impl tracing::Subscriber for Probe {
            fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
                *m.level() <= tracing::Level::INFO
            }
            fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
                Some(tracing::level_filters::LevelFilter::INFO)
            }
            fn event(&self, event: &tracing::Event<'_>) {
                self.0
                    .lock()
                    .expect("sink")
                    .push(ConsoleSubscriber::line_for(event));
            }
            fn new_span(&self, _s: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _s: &tracing::span::Id, _v: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _s: &tracing::span::Id, _f: &tracing::span::Id) {}
            fn enter(&self, _s: &tracing::span::Id) {}
            fn exit(&self, _s: &tracing::span::Id) {}
        }
        tracing::subscriber::with_default(Probe(sink), || {
            errlog_printf("iocPause: IOC suspended\n");
            tracing::warn!(target: "epics_base_rs::runtime", "a runtime line");
        });

        let lines = seen.lock().expect("sink").clone();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0], None, "an errlog event is write_console's");
        assert_eq!(
            lines[1].as_deref(),
            Some("WARN  epics_base_rs::runtime: a runtime line"),
            "every other target still renders"
        );
    }

    /// The console is ours in the two states an IOC runs in and nobody else's.
    /// An application that installed its own subscriber asked for its own
    /// formatting, so `write_console` must stay silent under one.
    #[test]
    #[serial]
    fn the_errlog_console_is_ours_only_with_nothing_listening_or_our_subscriber() {
        assert!(nothing_is_listening(), "no subscriber in a unit test");
        assert!(!console_subscriber_is_current());

        tracing::subscriber::with_default(ConsoleSubscriber, || {
            assert!(!nothing_is_listening());
            assert!(
                console_subscriber_is_current(),
                "our own subscriber leaves the console to write_console"
            );
        });

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        tracing::subscriber::with_default(CapturingSubscriber(seen), || {
            assert!(!nothing_is_listening());
            assert!(
                !console_subscriber_is_current(),
                "a foreign subscriber owns its own console"
            );
        });
    }

    /// The `tracing` half of the same bytes. An event is a record and its
    /// subscriber frames it, so exactly one trailing newline comes off and the
    /// newlines *inside* a multi-line message stay — `dbScan`'s over-run report
    /// (`dbScan.c:832-835`) is four lines and one errlog message.
    #[test]
    fn a_tracing_record_drops_one_trailing_newline_and_no_more() {
        assert_eq!(as_record("Starting iocInit\n"), "Starting iocInit");
        assert_eq!(as_record("dbConvertJSON: "), "dbConvertJSON: ");
        assert_eq!(as_record("a\n\n"), "a\n");
        assert_eq!(
            as_record("\ndbScan WARNING from 'x':\n\tOver-runs.\n"),
            "\ndbScan WARNING from 'x':\n\tOver-runs."
        );
    }

    /// The invariant end to end: one `errlog_printf` reaches its listeners with
    /// the caller's bytes exactly (C hands `base+1u` to every listener,
    /// `errlog.c:687`) while the `tracing` record carries the same line once,
    /// unframed. Not asserted here: the process console itself, which cannot be
    /// read back in-process without redirecting `stderr` — the byte-exactness
    /// of its writer is pinned by
    /// [`the_console_writes_the_callers_bytes_and_appends_nothing`].
    #[test]
    #[serial(errlog_listeners)]
    fn the_callers_bytes_frame_every_errlog_sink() {
        let heard = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&heard);
        let id = errlog_add_listener(move |m| sink.lock().expect("sink").push(m.to_string()));

        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        tracing::subscriber::with_default(CapturingSubscriber(events.clone()), || {
            errlog_printf("iocPause: IOC suspended\n");
            errlog_printf("dbConvertJSON: ");
        });
        errlog_flush();
        assert!(errlog_remove_listener(id));

        let lines = heard.lock().expect("sink").clone();
        assert!(
            lines.iter().any(|l| l == "iocPause: IOC suspended\n"),
            "a listener sees the caller's terminator: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "dbConvertJSON: "),
            "and gains none when the caller supplied none: {lines:?}"
        );

        let records = events.lock().expect("events").clone();
        assert!(
            records
                .iter()
                .any(|r| r == "INFO  epics_base_rs::errlog: iocPause: IOC suspended"),
            "the tracing record is framed by its subscriber, not by the caller: {records:?}"
        );
        assert!(
            records
                .iter()
                .any(|r| r == "INFO  epics_base_rs::errlog: dbConvertJSON: "),
            "{records:?}"
        );
    }

    /// The row's own observable, half one: a registered listener sees the
    /// message. Delivery is on the errlog worker thread, so the test flushes
    /// the way C's `errlogFlush` does before looking.
    #[test]
    #[serial(errlog_listeners)]
    fn a_registered_listener_receives_every_message_after_a_flush() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&seen);
        let id = errlog_add_listener(move |m| sink.lock().expect("sink").push(m.to_string()));

        errlog_printf("one");
        errlog_sev_printf(ErrlogSevEnum::Minor, "two");
        errlog_flush();

        let lines = seen.lock().expect("sink").clone();
        assert!(lines.iter().any(|l| l == "one"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "sevr=minor two"), "{lines:?}");
        assert!(errlog_remove_listener(id));
    }

    /// Boundary: removal. C's `errlogRemoveListeners` returns how many nodes
    /// matched; a token matches at most one, so a second removal answers
    /// false and nothing further is delivered.
    #[test]
    #[serial(errlog_listeners)]
    fn a_removed_listener_stops_receiving_and_cannot_be_removed_twice() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&seen);
        let id = errlog_add_listener(move |m| sink.lock().expect("sink").push(m.to_string()));
        errlog_printf("before");
        errlog_flush();
        assert!(errlog_remove_listener(id));
        assert!(!errlog_remove_listener(id), "the token is spent");
        errlog_printf("after");
        errlog_flush();

        let lines = seen.lock().expect("sink").clone();
        assert!(lines.iter().any(|l| l == "before"), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l == "after"),
            "a removed listener must see nothing more: {lines:?}"
        );
    }

    /// Boundary: the listener sees the message with its ANSI stripped, which
    /// is what C does before the listener loop (`errlog.c:678-681`).
    #[test]
    #[serial(errlog_listeners)]
    fn a_listener_sees_the_message_with_its_escapes_removed() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&seen);
        let id = errlog_add_listener(move |m| sink.lock().expect("sink").push(m.to_string()));
        errlog_printf("cas \x1b[35;1mWARNING\x1b[0m: bind");
        errlog_flush();
        assert!(errlog_remove_listener(id));

        let lines = seen.lock().expect("sink").clone();
        assert!(lines.iter().any(|l| l == "cas WARNING: bind"), "{lines:?}");
    }

    /// Boundary: a listener that removes itself from inside its own callback.
    /// C guards this with `active`/`removed` flags; the port drains against a
    /// snapshot, so the call cannot deadlock and the removal takes effect on
    /// the next message.
    #[test]
    #[serial(errlog_listeners)]
    fn a_listener_can_remove_itself_from_inside_its_own_callback() {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let slot: std::sync::Arc<std::sync::Mutex<Option<ErrlogListenerId>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let n = std::sync::Arc::clone(&count);
        let me = std::sync::Arc::clone(&slot);
        let id = errlog_add_listener(move |_m| {
            n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Some(id) = *me.lock().expect("slot") {
                errlog_remove_listener(id);
            }
        });
        *slot.lock().expect("slot") = Some(id);

        errlog_printf("first");
        errlog_flush();
        errlog_printf("second");
        errlog_flush();

        assert_eq!(
            count.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the self-removal must take effect, and must not have deadlocked"
        );
    }

    /// Boundary: `eltc`. C returns 0 always; the port returns the previous
    /// setting so a caller can restore it, and the setting itself is what
    /// gates the console.
    #[test]
    #[serial(errlog_console)]
    fn eltc_reports_the_previous_setting_and_gates_the_console() {
        assert!(errlog_to_console(), "C initialises pvt.toConsole to TRUE");
        assert!(eltc(false), "the previous setting comes back");
        assert!(!errlog_to_console());
        assert!(!eltc(true));
        assert!(errlog_to_console());
    }
}

/// Render `record` so it cannot end or split a line in a line-oriented log.
///
/// Every ASCII control character — `0x00..=0x1F` (which includes `\n`, `\r`
/// and NUL) and `0x7F` — becomes a printable `\xNN` escape. Everything else,
/// including all non-ASCII UTF-8, is passed through untouched, so the common
/// case allocates nothing.
///
/// # What this guarantees, and what it does not
///
/// It guarantees **line framing**: one record in, one line out, whatever the
/// record contains. That is the property an audit log needs — a reader must
/// not be able to mistake attacker-supplied text for a separate record.
///
/// It is deliberately **not** a reversible encoding: a backslash is left
/// alone, so a user string containing the four literal characters `\x0a` and
/// a real newline escape to the same bytes. Escaping backslashes would make
/// it reversible but would also corrupt any record that is already escaped —
/// a JSON record whose own encoder emitted `\n` would come back out as
/// `\\n`. Leaving backslash alone is what makes this safe to apply uniformly
/// at the writer, to every record, without the writer having to know which
/// renderer produced it.
///
/// Applying it to already-escaped output is a no-op, because a correct
/// encoder has already removed every raw control byte.
pub fn single_line(record: &str) -> std::borrow::Cow<'_, str> {
    fn must_escape(c: char) -> bool {
        (c as u32) < 0x20 || c as u32 == 0x7F
    }
    if !record.contains(must_escape) {
        return std::borrow::Cow::Borrowed(record);
    }
    let mut out = String::with_capacity(record.len() + 8);
    for c in record.chars() {
        if must_escape(c) {
            use std::fmt::Write;
            let _ = write!(out, "\\x{:02x}", c as u32);
        } else {
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod single_line_tests {
    use super::single_line;

    /// The framing guarantee, stated as a boundary sweep over every byte a
    /// record could carry rather than as a story about one attack.
    #[test]
    fn no_input_can_produce_more_than_one_line() {
        for b in 0u8..=0x7F {
            let c = b as char;
            let record = format!("a{c}b");
            let out = single_line(&record);
            assert_eq!(
                out.lines().count().max(1),
                1,
                "byte {b:#04x} split the record: {out:?}"
            );
            assert!(!out.contains('\n'), "byte {b:#04x} left a newline");
            assert!(!out.contains('\r'), "byte {b:#04x} left a carriage return");
            assert!(!out.contains('\0'), "byte {b:#04x} left a NUL");
        }
    }

    #[test]
    fn exactly_the_ascii_control_range_is_escaped() {
        for b in 0u8..=0xFF {
            if b >= 0x80 {
                continue; // non-ASCII is tested as UTF-8 below
            }
            let c = b as char;
            let raw = c.to_string();
            let out = single_line(&raw);
            let escaped = out != raw;
            assert_eq!(
                escaped,
                b < 0x20 || b == 0x7F,
                "byte {b:#04x}: escaped={escaped}, expected={}",
                b < 0x20 || b == 0x7F
            );
        }
        assert_eq!(single_line("\n"), "\\x0a");
        assert_eq!(single_line("\r"), "\\x0d");
        assert_eq!(single_line("\0"), "\\x00");
        assert_eq!(single_line("\u{7f}"), "\\x7f");
    }

    /// A clean record is returned borrowed — no allocation on the hot path.
    #[test]
    fn a_clean_record_is_passed_through_without_allocating() {
        let clean = "Apr 09 14:35:21 alice@opi-1 TEMP:setpoint 3.14 old=? OK";
        assert!(matches!(single_line(clean), std::borrow::Cow::Borrowed(_)));
        assert_eq!(single_line(clean), clean);
        // Non-ASCII survives intact: this escapes line framing, not Unicode.
        assert_eq!(single_line("설정값 μm"), "설정값 μm");
    }

    /// Applying it to output that is already escaped must not corrupt it —
    /// this is what lets the writer apply ONE rule to every renderer instead
    /// of asking which renderer produced the record.
    #[test]
    fn it_is_a_no_op_on_already_escaped_output() {
        let json = r#"{"user":"a\nb","pv":"X"}"#;
        assert_eq!(single_line(json), json);
        assert_eq!(single_line(&single_line("a\nb")), single_line("a\nb"));
    }
}
