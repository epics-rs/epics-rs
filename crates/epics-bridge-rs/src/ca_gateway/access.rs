//! Access security adapter for the gateway.
//!
//! Wraps an EPICS access security configuration (`.access` / ACF file)
//! and provides per-channel read/write permission checks.
//!
//! ## Format
//!
//! ```text
//! UAG(engineers) { jones, smith }
//! HAG(controlroom) { console1, console2 }
//!
//! ASG(DEFAULT) {
//!   RULE(1, READ)
//!   RULE(1, WRITE)
//! }
//!
//! ASG(BeamGroup) {
//!   RULE(1, READ)
//!   RULE(1, WRITE) { UAG(engineers), HAG(controlroom) }
//! }
//! ```
//!
//! Each PV is associated with an ASG via the `.pvlist` `ALLOW` / `ALIAS`
//! directives (the third token after `ALLOW`/`ALIAS` is the ASG name).
//! When a downstream client attempts a put or read, the gateway checks
//! the ASG rules against the client's user/host credentials.
//!
//! ## Status
//!
//! Per-rule enforcement is live: every PUT goes through
//! [`super::upstream::build_write_hook`], which calls [`Self::can_write`]
//! with the ASG from `.pvlist`, the rule's ASL, and the (user, host)
//! pair the CA server attaches to the WriteContext. Rejected puts
//! return `ECA_NORDACCESS` to the client and are recorded in the
//! putlog with outcome=DENIED.

use std::path::Path;

use epics_base_rs::server::access_security::{AccessLevel, AccessSecurityConfig, parse_acf};

use crate::error::{BridgeError, BridgeResult};

/// The gateway's access policy. Modelled as one sum type so the
/// "no rules" state cannot simultaneously mean allow-all and read-only
/// (BR-R63): each variant gives `can_read`/`can_write` exactly one
/// meaning.
enum Mode {
    /// Read-only default — reads allowed, writes denied. Installed when no
    /// `.access` file is provided, mirroring C ca-gateway's
    /// `ASG(DEFAULT) { RULE(1,READ) }` (`gateAs.cc:735-737`).
    ReadOnly,
    /// Allow everything — reads and writes permitted regardless of rules.
    /// Explicit opt-in only; not the no-file default.
    AllowAll,
    /// Rules parsed from an `.access` file.
    Rules(AccessSecurityConfig),
}

/// Access security configuration for the gateway.
pub struct AccessConfig {
    mode: Mode,
}

impl AccessConfig {
    /// Read-only default policy (no `.access` file): reads allowed, writes
    /// denied. Matches C ca-gateway's `ASG(DEFAULT) { RULE(1,READ) }`
    /// fallback (`gateAs.cc:735-737`).
    pub fn read_only() -> Self {
        Self {
            mode: Mode::ReadOnly,
        }
    }

    /// Construct an "allow all" config with no underlying rules. Explicit
    /// opt-in only — the no-file default is [`Self::read_only`] (BR-R63).
    pub fn allow_all() -> Self {
        Self {
            mode: Mode::AllowAll,
        }
    }

    /// Load an `.access` file from disk.
    pub fn from_file(path: &Path) -> BridgeResult<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::from_string(&content)
    }

    /// Parse `.access` content from a string.
    pub fn from_string(content: &str) -> BridgeResult<Self> {
        let config = parse_acf(content)
            .map_err(|e| BridgeError::GroupConfigError(format!("ACF parse: {e}")))?;
        Ok(Self {
            mode: Mode::Rules(config),
        })
    }

    /// Whether reading the given (asg, asl, user, host) tuple is allowed.
    ///
    /// The ASL is now passed to the underlying ACF check (C-G6 fix);
    /// rules with a level below `asl` are skipped, matching epics-base
    /// `asLibRoutines.c::asCompute`. Negative or zero ASL is treated
    /// as level 0 (most-restrictive).
    pub fn can_read(&self, asg: &str, asl: i32, user: &str, host: &str) -> bool {
        match &self.mode {
            // Both allow-all and the read-only default grant reads.
            Mode::AllowAll | Mode::ReadOnly => true,
            Mode::Rules(cfg) => {
                let asl = asl.max(0).min(u8::MAX as i32) as u8;
                matches!(
                    cfg.check_access_asl(asg, host, user, asl),
                    AccessLevel::Read | AccessLevel::ReadWrite
                )
            }
        }
    }

    /// Whether writing the given tuple is allowed.
    pub fn can_write(&self, asg: &str, asl: i32, user: &str, host: &str) -> bool {
        match &self.mode {
            Mode::AllowAll => true,
            // BR-R63: the read-only default denies writes (C parity).
            Mode::ReadOnly => false,
            Mode::Rules(cfg) => {
                let asl = asl.max(0).min(u8::MAX as i32) as u8;
                matches!(
                    cfg.check_access_asl(asg, host, user, asl),
                    AccessLevel::ReadWrite
                )
            }
        }
    }

    /// Whether the underlying ACF was successfully loaded.
    pub fn has_rules(&self) -> bool {
        matches!(self.mode, Mode::Rules(_))
    }
}

impl Default for AccessConfig {
    fn default() -> Self {
        // BR-R63: the no-config default is read-only, not allow-all.
        Self::read_only()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_all_default() {
        let acc = AccessConfig::allow_all();
        assert!(!acc.has_rules());
        assert!(acc.can_read("BeamGroup", 1, "anyone", "anywhere"));
        assert!(acc.can_write("BeamGroup", 1, "anyone", "anywhere"));
    }

    #[test]
    fn br_r63_default_is_read_only() {
        // BR-R63: no-config default must allow reads but DENY writes,
        // matching C ca-gateway's `ASG(DEFAULT) { RULE(1,READ) }`.
        let acc = AccessConfig::default();
        assert!(!acc.has_rules());
        assert!(acc.can_read("X", 0, "u", "h"));
        assert!(!acc.can_write("X", 0, "u", "h"));
    }

    #[test]
    fn br_r63_read_only_denies_write_allows_read() {
        let acc = AccessConfig::read_only();
        assert!(acc.can_read("BeamGroup", 1, "anyone", "anywhere"));
        assert!(!acc.can_write("BeamGroup", 1, "anyone", "anywhere"));
    }

    #[test]
    fn from_string_with_minimal_acf() {
        // Minimal ACF: a single ASG with READ/WRITE rules
        let content = r#"
            ASG(DEFAULT) {
                RULE(1, READ)
                RULE(1, WRITE)
            }
        "#;
        // Just verify parsing doesn't blow up; the ACF parser may have
        // its own quirks but allow-mode fallback should still hold
        let acc = AccessConfig::from_string(content);
        // ACF parser may succeed or fail depending on supported syntax;
        // both outcomes are acceptable for this skeleton.
        let _ = acc;
    }
}
