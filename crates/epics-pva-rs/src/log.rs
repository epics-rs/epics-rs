//! Runtime-reconfigurable log filtering.
//!
//! Mirrors pvxs's `logger_config_env` / `logger_level_set` /
//! `logger_config_str` (log.cpp:343-388) — at startup the application
//! installs a `tracing_subscriber::EnvFilter` wrapped in a
//! `reload::Layer`, and any later call to [`set_log_filter`] swaps
//! the filter atomically without restarting the process.
//!
//! Typical usage:
//!
//! ```ignore
//! use epics_pva_rs::log;
//! use tracing_subscriber::{fmt, prelude::*};
//!
//! // Once at startup. Reads PVXS_LOG / EPICS_PVA_LOG / RUST_LOG (in that
//! // precedence), falls back to "info" when none is set.
//! let (filter, handle) = log::init_filter();
//! tracing_subscriber::registry()
//!     .with(filter)
//!     .with(fmt::layer())
//!     .init();
//! log::set_global_handle(handle);
//!
//! // Later, e.g., from an admin RPC:
//! log::set_log_filter("info,epics_pva_rs::client_native=debug").ok();
//! ```
//!
//! All knobs are crate-global. There's exactly one reload handle per
//! process — pvxs has the same constraint (logger registry is a
//! global singleton).

use std::sync::OnceLock;

use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::reload;

/// Type alias for the reload handle. The first generic is the layer
/// type; the second is the registry it sits on. We type-erase the
/// registry side via `dyn` so callers don't have to thread the full
/// subscriber stack through their signatures.
pub type FilterReloadHandle = reload::Handle<EnvFilter, tracing_subscriber::Registry>;

/// Process-wide reload handle. Set by [`set_global_handle`] once the
/// caller has installed the filter into a registry. Subsequent
/// [`set_log_filter`] / [`set_log_level`] calls go through this.
static GLOBAL_HANDLE: OnceLock<FilterReloadHandle> = OnceLock::new();

/// The base log-filter spec taken from the environment, in
/// pvxs-compatibility precedence:
///
/// 1. `PVXS_LOG` — the variable pvxs `logger_config_env()` reads
///    (`log.cpp:388-394`); each pvxs tool consults it before option
///    parsing and `-d` is documented as shorthand for
///    `PVXS_LOG="pvxs.*=DEBUG"`. Honouring it first is what makes
///    `PVXS_LOG=… pvget-rs PV` behave like `PVXS_LOG=… pvxget PV`.
/// 2. `EPICS_PVA_LOG` — retained Rust-only alias, second in precedence.
/// 3. `RUST_LOG` — the Rust-native fallback.
///
/// An empty value is treated as unset and skipped (pvxs `if(!*env)`), so an
/// exported-but-empty `PVXS_LOG` does not shadow a real `EPICS_PVA_LOG`.
/// Returns `None` when no source is set (callers default to `"info"`).
///
/// Single owner for the env-source precedence: both [`init_filter`] and
/// [`set_log_level`] read through here, so they cannot diverge on which
/// variable wins.
pub(crate) fn log_env_base() -> Option<String> {
    for key in ["PVXS_LOG", "EPICS_PVA_LOG", "RUST_LOG"] {
        if let Ok(spec) = std::env::var(key) {
            if !spec.is_empty() {
                return Some(spec);
            }
        }
    }
    None
}

/// Build an `EnvFilter` reload-layer pair seeded from the standard log env
/// vars via `log_env_base` (`PVXS_LOG`, then `EPICS_PVA_LOG`, then
/// `RUST_LOG`), defaulting to `"info"` when none is set.
///
/// Returns the wrapped layer (install into your subscriber) and a
/// handle for runtime reconfiguration.
pub fn init_filter() -> (
    reload::Layer<EnvFilter, tracing_subscriber::Registry>,
    FilterReloadHandle,
) {
    let initial_spec = log_env_base().unwrap_or_else(|| "info".to_string());
    let filter = EnvFilter::try_new(&initial_spec).unwrap_or_else(|_| EnvFilter::new("info"));
    reload::Layer::new(filter)
}

/// Register the reload handle returned by [`init_filter`] as the
/// process-wide handle. Idempotent — subsequent calls are no-ops so
/// applications with multiple wiring entry points don't conflict.
pub fn set_global_handle(handle: FilterReloadHandle) {
    let _ = GLOBAL_HANDLE.set(handle);
}

/// Install the process-global tracing subscriber for a PVA CLI binary
/// and register its reload handle, optionally raising the library log
/// level to `DEBUG`.
///
/// This is the single wiring entry point every `pv*-rs` tool calls from
/// `main`, mirroring pvxs where each tool opens with
/// `logger_config_env()` (read `$PVXS_LOG`) and maps `-d` to
/// `logger_level_set("pvxs.*", Level::Debug)` (`tools/get.cpp:47-70`,
/// `monitor.cpp:48-70`, `put.cpp:41-64`, `list.cpp:66-99`,
/// `info.cpp:47-66`, `call.cpp:47-64`).
///
/// The filter is seeded by [`init_filter`] (`PVXS_LOG`, then
/// `EPICS_PVA_LOG`, then `RUST_LOG`, then `"info"`), so
/// `PVXS_LOG=... pvget-rs PV` works for these tools the way
/// `PVXS_LOG=... pvxget PV` does for pvxs. Log records go to **stderr**,
/// never stdout, so they cannot corrupt the value output a script parses.
///
/// `debug` (the `-d` flag) raises the `epics_pva_rs` library namespace
/// to `DEBUG` on top of the env base — the analogue of pvxs's
/// `pvxs.* = Debug`. `EnvFilter` matches targets by module-path prefix,
/// so this also enables every `epics_pva_rs::*` submodule.
///
/// Call once per process from `main`. If a subscriber is already
/// installed (e.g. by a test harness), installation is skipped and the
/// existing subscriber is left intact.
pub fn install_cli_logging(debug: bool) {
    use tracing_subscriber::prelude::*;

    let (filter, handle) = init_filter();
    let installed = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init()
        .is_ok();
    if !installed {
        return;
    }
    set_global_handle(handle);
    if debug {
        let _ = set_log_level("epics_pva_rs", LevelFilter::DEBUG);
    }
}

/// Replace the active log filter spec. `spec` follows the standard
/// `tracing_subscriber::EnvFilter` syntax — same as pvxs's
/// `logger_config_str` (log.cpp:343), e.g.,
/// `"info,epics_pva_rs::client_native=debug"`.
///
/// Returns Err when no global handle is installed (caller forgot to
/// call [`set_global_handle`]) or when `spec` fails to parse.
pub fn set_log_filter(spec: &str) -> Result<(), LogFilterError> {
    let handle = GLOBAL_HANDLE.get().ok_or(LogFilterError::NoHandle)?;
    let new_filter = EnvFilter::try_new(spec).map_err(|e| LogFilterError::Parse(e.to_string()))?;
    handle
        .reload(new_filter)
        .map_err(|e| LogFilterError::Reload(e.to_string()))
}

/// Set a single target's level. Mirrors pvxs `logger_level_set(name,
/// Level)`. Internally builds an `EnvFilter` of the form
/// `"<base>,<target>=<level>"` where `<base>` is the same env-derived
/// spec [`init_filter`] seeds from (`log_env_base`: `PVXS_LOG`, then
/// `EPICS_PVA_LOG`, then `RUST_LOG`), or `"info"` when none is set.
pub fn set_log_level(target: &str, level: LevelFilter) -> Result<(), LogFilterError> {
    let base = log_env_base().unwrap_or_else(|| "info".to_string());
    let level_str = match level {
        LevelFilter::OFF => "off",
        LevelFilter::ERROR => "error",
        LevelFilter::WARN => "warn",
        LevelFilter::INFO => "info",
        LevelFilter::DEBUG => "debug",
        LevelFilter::TRACE => "trace",
    };
    let spec = if base.is_empty() {
        format!("{target}={level_str}")
    } else {
        format!("{base},{target}={level_str}")
    };
    set_log_filter(&spec)
}

/// Errors from [`set_log_filter`] / [`set_log_level`].
#[derive(Debug, thiserror::Error)]
pub enum LogFilterError {
    #[error("no reload handle registered; call log::set_global_handle() first")]
    NoHandle,
    #[error("invalid filter spec: {0}")]
    Parse(String),
    #[error("reload failed: {0}")]
    Reload(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log env base must read
    /// `PVXS_LOG` for pvxs `logger_config_env()` parity, with the documented
    /// precedence `PVXS_LOG` > `EPICS_PVA_LOG` > `RUST_LOG`, an empty value
    /// treated as unset (pvxs `if(!*env)`), and `None` when nothing is set.
    /// One case per boundary. Serialised on `epics_env` because the three
    /// variables are process-global and shared with other env-driven tests.
    #[test]
    #[serial_test::serial(epics_env)]
    fn log_env_base_pvxs_precedence_and_empty_skip() {
        // SAFETY: std::env::{set,remove}_var are unsafe in edition 2024;
        // the `epics_env` serial guard makes the mutation race-free.
        unsafe {
            for k in ["PVXS_LOG", "EPICS_PVA_LOG", "RUST_LOG"] {
                std::env::remove_var(k);
            }
        }

        // Nothing set → None (caller defaults to "info").
        assert_eq!(log_env_base(), None);

        // RUST_LOG alone is the fallback.
        unsafe { std::env::set_var("RUST_LOG", "rust_log_spec") };
        assert_eq!(log_env_base().as_deref(), Some("rust_log_spec"));

        // EPICS_PVA_LOG wins over RUST_LOG.
        unsafe { std::env::set_var("EPICS_PVA_LOG", "epics_pva_spec") };
        assert_eq!(log_env_base().as_deref(), Some("epics_pva_spec"));

        // PVXS_LOG wins over both (pvxs compatibility variable).
        unsafe { std::env::set_var("PVXS_LOG", "epics_pva_rs=debug") };
        assert_eq!(log_env_base().as_deref(), Some("epics_pva_rs=debug"));

        // An empty PVXS_LOG is treated as unset, so EPICS_PVA_LOG wins —
        // an exported-but-empty PVXS_LOG must not shadow a real setting.
        unsafe { std::env::set_var("PVXS_LOG", "") };
        assert_eq!(log_env_base().as_deref(), Some("epics_pva_spec"));

        unsafe {
            for k in ["PVXS_LOG", "EPICS_PVA_LOG", "RUST_LOG"] {
                std::env::remove_var(k);
            }
        }
    }
}
