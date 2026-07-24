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
//! `errlogSevPrintf` — a record's error messages can be suppressed
//! below a configurable severity threshold, exactly as a C IOC does.

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

/// Write one already-formatted errlog line to the console, but only when
/// [`nothing_is_listening`].
fn console_fallback(line: &std::fmt::Arguments<'_>) {
    if nothing_is_listening() {
        eprintln!("{line}");
    }
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
        eprintln!("{}", render_event(event));
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
///    the console either way: with [`install_console_subscriber`] in place the
///    subscriber writes it, and with nothing listening the `errlog` console
///    fallback does.
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
            &panic_announcement(thread.name(), &location, &panic_payload(info)),
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

/// Severity threshold below which `errlog_sev_printf` messages are
/// suppressed from being logged. C parity: `pvt.sevToLog`, default
/// `errlogMinor` (`errlog.c` static init).
static SEV_TO_LOG: AtomicU8 = AtomicU8::new(ErrlogSevEnum::Minor as u8);

/// Set the severity-to-log threshold — C `errlogSetSevToLog`
/// (`errlog.c:399-405`). Messages with a severity below this value are
/// suppressed.
pub fn errlog_set_sev_to_log(severity: ErrlogSevEnum) {
    SEV_TO_LOG.store(severity as u8, Ordering::Relaxed);
}

/// Get the current severity-to-log threshold — C `errlogGetSevToLog`
/// (`errlog.c:407-415`).
pub fn errlog_get_sev_to_log() -> ErrlogSevEnum {
    ErrlogSevEnum::from_u8(SEV_TO_LOG.load(Ordering::Relaxed))
}

/// C `ERL_WARNING` (`errlog.h:299`) — the word an errlog warning line carries,
/// magenta on a terminal console and plain everywhere else.
///
/// C spells it `ANSI_MAGENTA("WARNING")`, i.e. the escapes are always IN the
/// message, and errlog strips them at print time when its console is not a
/// terminal (`errlog.c:672-681`, `pvt.ttyConsole = isATTY(stderr)` at
/// `errlog.c:555`). `isATTY` (`errlog.c:218-237`) also demands a non-empty
/// `$TERM`, on the grounds that a terminal that will not name itself cannot be
/// assumed to understand escapes. Both halves of that rule are here, so an
/// `epicsEnvSet`-style capture of a Rust IOC's stderr gets the same bytes as C's.
///
/// Verified head-to-head with the compiled `softIoc` (bind-conflict warning):
/// redirected to a file it writes `cas WARNING: …`; under `script(1)` it writes
/// `cas \x1b[35;1mWARNING\x1b[0m: …`.
pub fn erl_warning() -> &'static str {
    use std::io::IsTerminal;
    let term_names_itself = std::env::var_os("TERM").is_some_and(|t| !t.is_empty());
    if std::io::stderr().is_terminal() && term_names_itself {
        "\x1b[35;1mWARNING\x1b[0m"
    } else {
        "WARNING"
    }
}

/// Emit a pre-formatted error message at the given severity, suppressed
/// when `severity` is below the [`errlog_get_sev_to_log`] threshold.
///
/// C parity: `errlogSevVprintf`/`errlogSevPrintf` (`errlog.c:366-389`)
/// — the C code prefixes `"sevr=%s "` and routes to the message queue.
/// Here the prefix is preserved and the message is routed through
/// `tracing` at a level mapped from the severity. Returns `true` when
/// the message was emitted, `false` when suppressed by the threshold.
pub fn errlog_sev_printf(severity: ErrlogSevEnum, message: &str) -> bool {
    if severity < errlog_get_sev_to_log() {
        return false;
    }
    let line = format!("sevr={} {}", severity.as_str(), message);
    match severity {
        ErrlogSevEnum::Info => {
            tracing::info!(target: "epics_base_rs::errlog", "{line}")
        }
        ErrlogSevEnum::Minor => {
            tracing::warn!(target: "epics_base_rs::errlog", "{line}")
        }
        ErrlogSevEnum::Major | ErrlogSevEnum::Fatal => {
            tracing::error!(target: "epics_base_rs::errlog", "{line}")
        }
    }
    console_fallback(&format_args!("{line}"));
    true
}

/// Emit a pre-formatted message through the errlog facility
/// unconditionally — C `errlogVprintf`/`errlogPrintf`
/// (`errlog.c:333-364`), the *no-severity* variant.
///
/// Unlike [`errlog_sev_printf`] this carries no `sevr=` prefix and is
/// never gated by the [`errlog_get_sev_to_log`] threshold (C
/// `errlogVprintf` always enqueues). Routed through `tracing` at info
/// level on the same `epics_base_rs::errlog` target, so an application's
/// subscriber sees it on the errlog sink. Used by `stdio` device support
/// for the `"errlog"` output stream (`devStdio.c` `logPrintf`).
pub fn errlog_printf(message: &str) {
    tracing::info!(target: "epics_base_rs::errlog", "{message}");
    console_fallback(&format_args!("{message}"));
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
        errlog_set_sev_to_log(ErrlogSevEnum::Minor);
        assert_eq!(errlog_get_sev_to_log(), ErrlogSevEnum::Minor);
    }

    #[test]
    #[serial(errlog_sev)]
    fn sev_printf_suppresses_below_threshold() {
        errlog_set_sev_to_log(ErrlogSevEnum::Major);
        // Below threshold -> suppressed.
        assert!(!errlog_sev_printf(ErrlogSevEnum::Info, "quiet"));
        assert!(!errlog_sev_printf(ErrlogSevEnum::Minor, "quiet"));
        // At or above threshold -> emitted.
        assert!(errlog_sev_printf(ErrlogSevEnum::Major, "loud"));
        assert!(errlog_sev_printf(ErrlogSevEnum::Fatal, "loud"));
        errlog_set_sev_to_log(ErrlogSevEnum::Minor);
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
        let line = panic_announcement(Some("main"), "rtems-ca-ioc.rs:118", "iocInit failed");
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
