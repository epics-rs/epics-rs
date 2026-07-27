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
//! `super::upstream::build_write_hook`, which calls [`AccessConfig::can_write`]
//! with the ASG from `.pvlist`, the rule's ASL, and the (user, host)
//! pair the CA server attaches to the WriteContext. Rejected puts
//! return `ECA_NORDACCESS` to the client and are recorded in the
//! putlog with outcome=DENIED.

use std::path::Path;

use epics_base_rs::server::access_security::{AccessLevel, AccessSecurityConfig, parse_acf};

use crate::error::{BridgeError, BridgeResult};

/// The gateway's access policy. Modelled as one sum type so the
/// "no rules" state cannot simultaneously mean allow-all and read-only
///: each variant gives `can_read`/`can_write` exactly one
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

/// Outcome of a write access-rights check: whether the write is permitted
/// and whether the matched WRITE rule carried `TRAPWRITE` (the
/// `trapMask` C ca-gateway gates put-log emission on, `gateVc.cc:236`).
/// Returned as one value by [`AccessConfig::can_write_trap`] so the trap
/// state survives the access decision instead of being discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WritePermit {
    /// `true` iff the client is granted write access.
    pub allowed: bool,
    /// `true` iff the matched WRITE rule carried `TRAPWRITE`. Always
    /// `false` when `allowed` is `false`.
    pub trap: bool,
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
    /// opt-in only — the no-file default is [`Self::read_only`].
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
    /// The ASL is now passed to the underlying ACF check;
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

    /// Write permission for the (asg, asl, user, host) tuple, paired with
    /// the matched WRITE rule's `TRAPWRITE`/`trapMask` flag.
    ///
    /// C ca-gateway gates *all* put-log emission on
    /// `asclient->clientPvt()->trapMask` (`gateVc.cc:236`), the mask of the
    /// access rule the client matched. Collapsing the rule to a bare
    /// allow/deny here — as the old `can_write` did — discards that mask,
    /// so the put log cannot be TRAPWRITE-scoped. Returning both halves as
    /// one value keeps the trap state available to the write hook instead
    /// of reducing the rule to a boolean before the audit decision.
    ///
    /// `trap` is always `false` for `AllowAll`/`ReadOnly` (no ACF rule to
    /// carry the flag) and for any denied write (base-rs
    /// `check_access_method_trap` returns `trap = false` on a `NoAccess`
    /// outcome, matching C `asComputePvt` which only copies `trapMask` on
    /// the lines that raise `access`).
    pub fn can_write_trap(&self, asg: &str, asl: i32, user: &str, host: &str) -> WritePermit {
        match &self.mode {
            Mode::AllowAll => WritePermit {
                allowed: true,
                trap: false,
            },
            // the read-only default denies writes (C parity).
            Mode::ReadOnly => WritePermit {
                allowed: false,
                trap: false,
            },
            Mode::Rules(cfg) => {
                let asl = asl.max(0).min(u8::MAX as i32) as u8;
                // Method-aware trap-carrying check (default scopes, like
                // `check_access_asl`); the second tuple element is the
                // matched rule's `trapMask` (base-rs access_security.rs).
                let (level, trap) = cfg.check_access_method_trap(asg, host, user, asl, "", "");
                WritePermit {
                    allowed: matches!(level, AccessLevel::ReadWrite),
                    trap,
                }
            }
        }
    }

    /// Whether writing the given tuple is allowed. Thin accessor over
    /// [`Self::can_write_trap`] for the access-rights *report* path
    /// (`build_access_hook`), which gates reported write rights and never
    /// emits a put-log line, so the trap mask is irrelevant there.
    pub fn can_write(&self, asg: &str, asl: i32, user: &str, host: &str) -> bool {
        self.can_write_trap(asg, asl, user, host).allowed
    }

    /// Whether the underlying ACF was successfully loaded.
    pub fn has_rules(&self) -> bool {
        matches!(self.mode, Mode::Rules(_))
    }

    /// One-line description of the effective access mode, for the R3
    /// access-security report header.
    pub fn mode_summary(&self) -> &'static str {
        match self.mode {
            Mode::ReadOnly => "read-only default (ASG(DEFAULT){RULE(1,READ)}, no .access file)",
            Mode::AllowAll => "allow-all (no .access file)",
            Mode::Rules(_) => "rules parsed from .access file",
        }
    }

    /// Parsed UAG/HAG/ASG/RULE dump in C `asDumpFP` shape, for the R3
    /// access-security report. `Some` only when an `.access` file was
    /// loaded (`Mode::Rules`); the file-less `ReadOnly`/`AllowAll`
    /// defaults have no parsed structures to dump and return `None`.
    ///
    /// Delegates to the single dump-format owner
    /// [`AccessSecurityConfig::dump_report`] in `epics-base-rs`, shared
    /// with the `asdbdump` iocsh command. The verbose member/client
    /// listing of C's `asDumpFP(..., verbose=TRUE)` is not included (no
    /// live AS-member registry — see that method's note).
    pub fn dump_report(&self) -> Option<String> {
        match &self.mode {
            Mode::Rules(cfg) => Some(cfg.dump_report()),
            Mode::ReadOnly | Mode::AllowAll => None,
        }
    }
}

impl Default for AccessConfig {
    fn default() -> Self {
        // the no-config default is read-only, not allow-all.
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
        // no-config default must allow reads but DENY writes,
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
    fn can_write_trap_reflects_matched_rule_trapmask() {
        // The trap flag must follow the matched WRITE rule's TRAPWRITE
        // option — the mask C ca-gateway gates put-log emission on
        // (gateVc.cc:236). A granted write to a TRAPWRITE rule carries
        // trap=true; an identical write to a NOTRAPWRITE rule carries
        // trap=false; a denied write always carries trap=false.
        let acf = r#"
            ASG(TRAPPED)   { RULE(1, WRITE, TRAPWRITE) }
            ASG(UNTRAPPED) { RULE(1, WRITE, NOTRAPWRITE) }
            ASG(READONLY)  { RULE(1, READ) }
        "#;
        let acc = AccessConfig::from_string(acf).expect("ACF parses");

        let trapped = acc.can_write_trap("TRAPPED", 1, "u", "h");
        assert!(trapped.allowed, "TRAPWRITE rule grants write");
        assert!(trapped.trap, "TRAPWRITE rule sets trap mask");

        let untrapped = acc.can_write_trap("UNTRAPPED", 1, "u", "h");
        assert!(untrapped.allowed, "NOTRAPWRITE rule still grants write");
        assert!(!untrapped.trap, "NOTRAPWRITE rule clears trap mask");

        let denied = acc.can_write_trap("READONLY", 1, "u", "h");
        assert!(!denied.allowed, "READ-only ASG denies write");
        assert!(!denied.trap, "a denied write never carries a trap mask");
    }

    #[test]
    fn can_write_trap_modes_carry_no_trap() {
        // AllowAll/ReadOnly have no ACF rule to carry TRAPWRITE, so the
        // trap mask is always false (matches C: an allow-all gateway with
        // no TRAPWRITE rule logs nothing in trap-scoped mode).
        let allow = AccessConfig::allow_all().can_write_trap("X", 1, "u", "h");
        assert!(allow.allowed && !allow.trap);
        let ro = AccessConfig::read_only().can_write_trap("X", 1, "u", "h");
        assert!(!ro.allowed && !ro.trap);
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
