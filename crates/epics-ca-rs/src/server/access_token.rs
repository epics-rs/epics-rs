//! CA-side type-state ACF gate.
//!
//! The CA `tcp.rs` handles every wire op (READ / WRITE / EVENT_ADD /
//! PUT_ACKT) as an inline arm of a single `match hdr.cmmd { … }`.
//! Previously each arm had to *remember* to look up the cached
//! [`epics_base_rs::server::access_security::AccessLevel`] in
//! `state.channel_access` and compare it against the op's
//! requirement — five separate rounds of self-review (rounds 38, 39)
//! caught arms that forgot to do this for READ, EVENT_ADD, and
//! PUT_ACKT.
//!
//! This module introduces a type-state layer mirroring the PVA
//! `AccessChecked` invariant from rounds 40-43:
//!
//! * [`CaAccessChecked`] wraps the cached level. Its only public
//!   surface is `require_read` / `require_write` — direct access to
//!   the underlying [`AccessLevel`] is sealed.
//! * [`ReadGranted`] / [`WriteGranted`] are zero-sized witness types
//!   produced by those methods. Op handlers that take one as a
//!   parameter (today via the `let _ = …?` shape) cannot compile
//!   without it.
//! * [`AccessDenied`] carries the appropriate ECA error code so the
//!   caller's `?` propagation lands on the matching wire reply
//!   (`ECA_NORDACCESS` for READ-class ops, `ECA_NOWTACCESS` for
//!   WRITE-class).
//!
//! A new wire op that forgets to consult `CaAccessChecked` would
//! still compile (the match arms aren't separate functions), but the
//! convention is now "every op opens with
//! `state.lookup_access(sid).require_<read|write>()?`" — a single
//! shape that's grep-auditable and visually consistent across all
//! arms. The PVA refactor that *was* trait-driven made
//! "forgetting" a compile error; this CA shape requires the author
//! to actively bypass the helper.

use epics_base_rs::server::access_security::AccessLevel;

use crate::protocol::{ECA_NORDACCESS, ECA_NOWTACCESS};

/// Opaque proof that an access-level lookup has been performed for
/// a CA channel (SID). Construction is via
/// `crate::server::tcp::ClientState::lookup_access` only; the
/// private `_seal` field blocks external struct-literal builds.
#[derive(Debug, Clone, Copy)]
pub struct CaAccessChecked {
    level: AccessLevel,
    /// write-trap mask of the ACF rule that resolved `level`.
    /// Threaded into [`WriteGranted`] so TRAPWRITE put-logging
    /// dispatch can honour `TRAPWRITE` / `NOTRAPWRITE` instead of
    /// hard-coding every accepted write as trapped. Mirrors C
    /// `pasgclient->trapMask` (`asLibRoutines.c:1048`).
    rule_was_trap: bool,
    _seal: AccessSeal,
}

#[derive(Debug, Clone, Copy)]
struct AccessSeal;

/// Witness type returned by [`CaAccessChecked::require_read`]. Op
/// handlers may bind one (`let _read = …?`) at function entry; the
/// existence of the binding documents that the check fired.
#[derive(Debug, Clone, Copy)]
pub struct ReadGranted {
    _seal: AccessSeal,
}

/// Witness type returned by [`CaAccessChecked::require_write`].
#[derive(Debug, Clone, Copy)]
pub struct WriteGranted {
    /// write-trap mask of the ACF rule that authorised this
    /// write. The CA write handler sets
    /// `TrapWriteMessage::rule_was_trap` from this value so a
    /// `NOTRAPWRITE` rule (or a rule with no trap option) is not
    /// reported to put-logging listeners as trapped.
    rule_was_trap: bool,
    _seal: AccessSeal,
}

impl WriteGranted {
    /// True iff the ACF rule that authorised this write carried the
    /// `TRAPWRITE` option.
    pub fn rule_was_trap(&self) -> bool {
        self.rule_was_trap
    }
}

/// Denial reason carrying the matching ECA wire code. Op handlers
/// usually map this with `?` after sending the appropriate CA error
/// frame.
#[derive(Debug, Clone, Copy)]
pub enum AccessDenied {
    /// Channel lacks READ — surfaces as `ECA_NORDACCESS`.
    NoRead,
    /// Channel lacks WRITE — surfaces as `ECA_NOWTACCESS`.
    NoWrite,
}

impl AccessDenied {
    /// Wire-level ECA status code for this denial.
    pub fn eca_code(&self) -> u32 {
        match self {
            AccessDenied::NoRead => ECA_NORDACCESS,
            AccessDenied::NoWrite => ECA_NOWTACCESS,
        }
    }
}

impl CaAccessChecked {
    /// Construct from a cached level. Crate-internal — the wire
    /// dispatcher in `tcp.rs` is the only place that mints these
    /// (via `ClientState::lookup_access`). External callers cannot
    /// produce a `CaAccessChecked` and therefore cannot fabricate
    /// `ReadGranted` / `WriteGranted` witnesses.
    pub(crate) fn from_level(level: AccessLevel, rule_was_trap: bool) -> Self {
        Self {
            level,
            rule_was_trap,
            _seal: AccessSeal,
        }
    }

    /// "Denied" token used when no channel_access cache entry
    /// exists. Surfaces NoRead / NoWrite on require_*. A denied
    /// channel never reaches the write path, so the trap mask is
    /// `false`.
    pub(crate) fn denied() -> Self {
        Self::from_level(AccessLevel::NoAccess, false)
    }

    /// Returns `Ok(ReadGranted)` iff the level grants at least
    /// READ; else `Err(NoRead)` carrying `ECA_NORDACCESS`.
    pub fn require_read(&self) -> Result<ReadGranted, AccessDenied> {
        if matches!(self.level, AccessLevel::NoAccess) {
            Err(AccessDenied::NoRead)
        } else {
            Ok(ReadGranted { _seal: AccessSeal })
        }
    }

    /// Returns `Ok(WriteGranted)` iff the level is `ReadWrite`;
    /// else `Err(NoWrite)` carrying `ECA_NOWTACCESS`.
    pub fn require_write(&self) -> Result<WriteGranted, AccessDenied> {
        if matches!(self.level, AccessLevel::ReadWrite) {
            Ok(WriteGranted {
                rule_was_trap: self.rule_was_trap,
                _seal: AccessSeal,
            })
        } else {
            Err(AccessDenied::NoWrite)
        }
    }

    /// Raw level — exposed for diagnostics (compute_access wire
    /// frame builder needs the integer mask 0/1/3). Should NOT be
    /// used as a substitute for `require_*` in op handlers.
    pub fn level(&self) -> AccessLevel {
        self.level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_access_denies_read_and_write() {
        let c = CaAccessChecked::from_level(AccessLevel::NoAccess, false);
        assert!(matches!(c.require_read(), Err(AccessDenied::NoRead)));
        assert!(matches!(c.require_write(), Err(AccessDenied::NoWrite)));
    }

    #[test]
    fn read_grants_read_denies_write() {
        let c = CaAccessChecked::from_level(AccessLevel::Read, false);
        assert!(c.require_read().is_ok());
        assert!(matches!(c.require_write(), Err(AccessDenied::NoWrite)));
    }

    #[test]
    fn read_write_grants_both() {
        let c = CaAccessChecked::from_level(AccessLevel::ReadWrite, false);
        assert!(c.require_read().is_ok());
        assert!(c.require_write().is_ok());
    }

    #[test]
    fn mr_r20_write_granted_carries_trap_mask() {
        // A `TRAPWRITE` rule resolves a ReadWrite token whose
        // `WriteGranted` witness reports `rule_was_trap == true`.
        let trapped = CaAccessChecked::from_level(AccessLevel::ReadWrite, true);
        assert!(trapped.require_write().unwrap().rule_was_trap());
        // A `NOTRAPWRITE` / no-option rule resolves the same access
        // level but the witness must report `false`.
        let untrapped = CaAccessChecked::from_level(AccessLevel::ReadWrite, false);
        assert!(!untrapped.require_write().unwrap().rule_was_trap());
    }

    #[test]
    fn denied_helper_returns_no_access_token() {
        let c = CaAccessChecked::denied();
        assert!(matches!(c.require_read(), Err(AccessDenied::NoRead)));
        assert!(matches!(c.require_write(), Err(AccessDenied::NoWrite)));
    }

    #[test]
    fn eca_codes_match_wire_constants() {
        assert_eq!(AccessDenied::NoRead.eca_code(), ECA_NORDACCESS);
        assert_eq!(AccessDenied::NoWrite.eca_code(), ECA_NOWTACCESS);
    }
}
