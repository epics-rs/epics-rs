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
