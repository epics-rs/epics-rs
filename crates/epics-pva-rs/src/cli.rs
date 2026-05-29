//! Helpers shared across the PVA command-line binaries (`pvget`,
//! `pvput`, `pvmonitor`, `pvinfo`, `pvcall`, `pvlist`, `pvxvct`,
//! `mshim`): timeout parsing, the `-v` effective-config diagnostic,
//! and the `-V` version-information text.

use std::time::Duration;

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
pub fn timeout_duration(secs: f64) -> Duration {
    let s = if secs.is_finite() && secs > 0.0 {
        secs
    } else {
        DEFAULT_CLI_TIMEOUT_SECS
    };
    Duration::from_secs_f64(s)
}

/// CLI `-w` timeout policy for tools whose pvxs semantics treat a
/// non-positive `-w` as **no deadline** rather than the 5 s clamp of
/// [`timeout_duration`].
///
/// pvxs `-w 0` does not mean one thing across the tools, so a single
/// duration-valued helper cannot serve all of them. `pvxlist` documents
/// `-w 0` as "disables timeout" and waits with `done.wait()` (no
/// deadline) in BOTH discovery and query mode
/// (`tools/list.cpp:55-58,154-203`). This type names that regime so a
/// tool derives one consistent policy from `-w` and applies it
/// identically in every mode, instead of one mode treating `-w 0` as
/// no-deadline while another clamps it to 5 s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeoutPolicy {
    /// A bounded operation deadline.
    Finite(Duration),
    /// No deadline — wait until the operation completes or the user
    /// interrupts.
    Forever,
}

impl TimeoutPolicy {
    /// `-w 0` ("no timeout") cannot be expressed as a truly unbounded
    /// wait on the Duration-based client builder
    /// ([`crate::client::PvaClient`]'s `timeout` takes a `Duration`, fed
    /// to `tokio::time::timeout`), so [`TimeoutPolicy::Forever`] is
    /// encoded as a ~10-year sentinel the operation timeout cannot
    /// realistically reach — operationally a no-deadline wait for an
    /// interactive CLI, and far below any `Instant` overflow. Matches
    /// pvxs `pvxlist`'s `done.wait()` (`tools/list.cpp:154-203`).
    const FOREVER_SENTINEL: Duration = Duration::from_secs(10 * 365 * 24 * 3600);

    /// `pvxlist`'s `-w` rule: a finite, strictly-positive value is a
    /// bounded deadline; `0`, a negative, or a non-finite value
    /// (NaN/±Inf) is "no timeout" (`tools/list.cpp:55-58,154-203`). This
    /// is the no-timeout-on-zero regime — the inverse of `pvxcall`'s
    /// immediate-on-zero and distinct from the 5 s clamp
    /// ([`timeout_duration`]).
    pub fn wait_or_forever(secs: f64) -> Self {
        if secs.is_finite() && secs > 0.0 {
            TimeoutPolicy::Finite(Duration::from_secs_f64(secs))
        } else {
            TimeoutPolicy::Forever
        }
    }

    /// The bounded wait duration, or `None` for [`TimeoutPolicy::Forever`].
    /// A discovery loop maps this to an `Option<Instant>` deadline so
    /// `Forever` becomes a genuinely unbounded receive wait.
    pub fn finite_duration(self) -> Option<Duration> {
        match self {
            TimeoutPolicy::Finite(d) => Some(d),
            TimeoutPolicy::Forever => None,
        }
    }

    /// The operation timeout to hand the Duration-based client builder.
    /// `Forever` maps to [`Self::FOREVER_SENTINEL`].
    pub fn op_timeout(self) -> Duration {
        match self {
            TimeoutPolicy::Finite(d) => d,
            TimeoutPolicy::Forever => Self::FOREVER_SENTINEL,
        }
    }
}

/// The shared `-V` / `--version` text for the PVA CLI tools, the
/// analogue of pvxs `version_information` (`src/describe.cpp:135-140`).
///
/// pvxs routes every tool's `-V` through
/// `std::cout << pvxs::version_information; return 0;`
/// (`tools/get.cpp:62-64`, `put.cpp:55`, `list.cpp:75-82`,
/// `monitor.cpp:63`, `call.cpp:55`, …), which prints the PVXS library
/// version, the EPICS Base version string (`EPICS_VERSION_STRING`,
/// `src/describe.cpp:138`), and the libevent version — the
/// protocol/toolchain stack an operator pastes into a bug report.
/// Clap's generated `--version` reports only `<binary> <crate-version>`,
/// losing that dependency context.
///
/// The Rust analogue reports the three facts the linked libraries know
/// at compile time without a build script:
///   - the `epics-pva-rs` crate version (the PVA implementation, the
///     PVXS analogue),
///   - the EPICS Base release the port targets
///     ([`epics_base_rs::EPICS_BASE_VERSION`], from
///     `configure/CONFIG_BASE_VERSION`) — the `EPICS_VERSION_STRING`
///     analogue, and
///   - the PVA wire-protocol version it speaks
///     ([`crate::proto::PVA_VERSION`]) — the interop-critical fact a
///     "talks to pvxs/Java but not X" report needs, the analogue of
///     pvxs's libevent/runtime line.
///
/// The EPICS Base version names the upstream C release being ported,
/// which tracks EPICS Base's release cadence rather than the
/// `epics-base-rs` crate version, so it is a genuinely separate line
/// and not a duplicate of [`crate::VERSION`].
pub fn version_information() -> &'static str {
    use std::sync::OnceLock;
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        format!(
            "epics-pva-rs {}\nEPICS Base {}\nPVA protocol version {}\n",
            crate::VERSION,
            epics_base_rs::EPICS_BASE_VERSION,
            crate::proto::PVA_VERSION
        )
    })
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

    /// `pvxlist` `-w 0` means "no timeout": the policy is `Forever`, and
    /// non-positive / non-finite inputs collapse the same way
    /// (`tools/list.cpp:55-58,154-203`). This is the inverse of the
    /// `timeout_duration` 5 s clamp above.
    #[test]
    fn wait_or_forever_treats_nonpositive_as_forever() {
        for secs in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                TimeoutPolicy::wait_or_forever(secs),
                TimeoutPolicy::Forever,
                "secs={secs}"
            );
        }
        // Forever has no finite deadline (discovery → unbounded wait)
        // and a far-future operation timeout (query → effectively no
        // deadline).
        assert_eq!(TimeoutPolicy::Forever.finite_duration(), None);
        assert!(TimeoutPolicy::Forever.op_timeout() >= Duration::from_secs(365 * 24 * 3600));
    }

    /// A finite, strictly-positive `-w` is a bounded deadline, applied
    /// identically by discovery (`finite_duration`) and query
    /// (`op_timeout`).
    #[test]
    fn wait_or_forever_preserves_positive_finite() {
        let p = TimeoutPolicy::wait_or_forever(3.0);
        assert_eq!(p, TimeoutPolicy::Finite(Duration::from_secs(3)));
        assert_eq!(p.finite_duration(), Some(Duration::from_secs(3)));
        assert_eq!(p.op_timeout(), Duration::from_secs(3));
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

    /// The shared `-V` text reports more than `<binary> <crate-version>`:
    /// it names the `epics-pva-rs` library version, the EPICS Base
    /// release the port targets, and the PVA wire-protocol version (the
    /// pvxs `version_information` analogue). This is the
    /// dependency/protocol context clap's crate-only `--version` dropped.
    #[test]
    fn version_information_includes_protocol_stack() {
        let v = super::version_information();
        assert!(
            v.contains(&format!("epics-pva-rs {}", crate::VERSION)),
            "got: {v:?}"
        );
        assert!(
            v.contains(&format!("EPICS Base {}", epics_base_rs::EPICS_BASE_VERSION)),
            "got: {v:?}"
        );
        assert!(
            v.contains(&format!(
                "PVA protocol version {}",
                crate::proto::PVA_VERSION
            )),
            "got: {v:?}"
        );
        // Strictly more than the bare crate version: at least three lines
        // (crate version, EPICS Base version, PVA protocol version).
        assert!(
            v.lines().count() >= 3,
            "version output must carry dependency context, got: {v:?}"
        );
    }
}
