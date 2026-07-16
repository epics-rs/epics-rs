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
