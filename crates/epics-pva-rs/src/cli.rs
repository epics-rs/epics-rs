//! Helpers shared across the `pvget` / `pvput` / `pvmonitor` /
//! `pvinfo` / `pvcall` / `pvlist` command-line binaries.

/// Default PVA CLI timeout in seconds when a user-supplied `-w`
/// is missing or non-finite. Matches `pvget-rs` default of 5.0 s
/// (epics-base `pvget` likewise defaults to 5 s, vs CA tools' 1 s).
pub const DEFAULT_CLI_TIMEOUT_SECS: f64 = 5.0;

/// Convert a user-supplied timeout (CLI `-w`) into a
/// `std::time::Duration`. `Duration::from_secs_f64` panics on NaN /
/// infinity / negative values; clap parses those literally as f64
/// so this guard clamps to [`DEFAULT_CLI_TIMEOUT_SECS`] for any
/// non-finite-or-non-positive input. Mirrors the
/// `epics_ca_rs::cli::timeout_duration` analog (epics-base 1655d68e
/// — defensive handling of pathological floats in tool timeouts).
pub fn timeout_duration(secs: f64) -> std::time::Duration {
    let s = if secs.is_finite() && secs > 0.0 {
        secs
    } else {
        DEFAULT_CLI_TIMEOUT_SECS
    };
    std::time::Duration::from_secs_f64(s)
}

/// Print the effective PVA client configuration, the shared `-v`
/// ("make more noise") verbose path for the pvxs-compatible tools.
///
/// pvxs routes `-v` through `verbose=true` and, before issuing any
/// operation, prints `Effective config\n` followed by the client
/// context configuration — `tools/get.cpp:99-100`,
/// `tools/monitor.cpp:97-98`, `tools/put.cpp:109-110`,
/// `tools/call.cpp:122-123`, and `tools/info.cpp:76-79`. Every one of
/// those tools emits the same block, so it lives here as a single owner
/// rather than being re-implemented (or, worse, overloaded onto the
/// output formatter) per binary. The values are read through the
/// resolved [`crate::config`] getters so the output reflects
/// environment + defaults exactly as the client will use them.
pub fn print_effective_config() {
    print!("{}", effective_config_string());
}

/// Render the `Effective config` block as a string (see
/// [`print_effective_config`]). Split out so the verbose output is
/// unit-testable without capturing process stdout.
pub fn effective_config_string() -> String {
    use crate::config;
    use std::fmt::Write;
    let addr_list = std::env::var("EPICS_PVA_ADDR_LIST").unwrap_or_default();
    let name_servers = config::name_servers()
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let interfaces = config::list_intf_addresses()
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let mut s = String::new();
    let _ = writeln!(s, "Effective config");
    let _ = writeln!(s, "  EPICS_PVA_ADDR_LIST={addr_list}");
    let _ = writeln!(
        s,
        "  EPICS_PVA_AUTO_ADDR_LIST={}",
        if config::auto_addr_list_enabled() {
            "YES"
        } else {
            "NO"
        }
    );
    let _ = writeln!(s, "  EPICS_PVA_SERVER_PORT={}", config::server_port());
    let _ = writeln!(s, "  EPICS_PVA_BROADCAST_PORT={}", config::broadcast_port());
    let _ = writeln!(s, "  EPICS_PVA_NAME_SERVERS={name_servers}");
    let _ = writeln!(s, "  interfaces={interfaces}");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_duration_clamps_pathological_floats() {
        assert_eq!(
            timeout_duration(f64::NAN).as_secs_f64(),
            DEFAULT_CLI_TIMEOUT_SECS
        );
        assert_eq!(
            timeout_duration(f64::INFINITY).as_secs_f64(),
            DEFAULT_CLI_TIMEOUT_SECS
        );
        assert_eq!(
            timeout_duration(f64::NEG_INFINITY).as_secs_f64(),
            DEFAULT_CLI_TIMEOUT_SECS
        );
        assert_eq!(
            timeout_duration(-1.0).as_secs_f64(),
            DEFAULT_CLI_TIMEOUT_SECS
        );
        assert_eq!(
            timeout_duration(0.0).as_secs_f64(),
            DEFAULT_CLI_TIMEOUT_SECS
        );
    }

    #[test]
    fn timeout_duration_preserves_positive_finite() {
        let d = timeout_duration(3.5);
        assert!((d.as_secs_f64() - 3.5).abs() < 1e-9);
    }

    /// The shared `-v` verbose path emits the pvxs `Effective config`
    /// block with the resolved client configuration keys. This is the
    /// text every pvxs-compatible tool (`pvget`/`pvmonitor`/`pvput`/
    /// `pvcall`/`pvinfo`) prints on `-v`; asserting on the fixed keys
    /// keeps the check environment-independent.
    #[test]
    fn effective_config_emits_config_keys() {
        let s = effective_config_string();
        assert!(s.starts_with("Effective config\n"), "got: {s:?}");
        for key in [
            "EPICS_PVA_ADDR_LIST=",
            "EPICS_PVA_AUTO_ADDR_LIST=",
            "EPICS_PVA_SERVER_PORT=",
            "EPICS_PVA_BROADCAST_PORT=",
            "EPICS_PVA_NAME_SERVERS=",
            "interfaces=",
        ] {
            assert!(s.contains(key), "missing {key:?} in:\n{s}");
        }
    }
}
