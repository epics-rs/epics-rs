//! The C simulation-mode contract — `recGbl.c:421-457`
//! (`recGblSaveSimm`, `recGblCheckSimm`, `recGblInitSimm`, `recGblGetSimm`)
//! plus the `recGblInitConstantLink(&siol, …, &sval)` every SIML/SIOL-bearing
//! record pairs with them in `init_record`.
//!
//! This module holds the *pure* half of that contract (mode resolution and the
//! link-fetch classification); the transition itself is driven by the single
//! owner [`crate::server::database::PvDatabase::rec_gbl_get_simm`] /
//! `rec_gbl_init_simm`, which are the only sites allowed to write SIMM.
//!
//! # The link rule the whole contract rests on
//!
//! C reads SIML and SIOL through `dbGetLink` / `dbTryGetLink`, which dispatch
//! on the link's `lset`. For a CONSTANT link — and an *unset* link is a
//! constant link with a NULL string (`dbLink.c::dbLinkIsConstant`,
//! `dbConstLink.c::dbConstInitLink`) — `dbConstGetValue`
//! (`dbConstLink.c:219-225`) returns status 0 and leaves the caller's buffer
//! **untouched**. A constant's value reaches the record exactly once, at
//! `init_record`, via `dbLoadLink` (`dbConstLink.c::dbConstLoadScalar`).
//!
//! So a constant SIML/SIOL is a *load-time* value, never a per-cycle read, and
//! "the read delivered nothing" is a SUCCESS (status 0) that still copies the
//! record's own buffer (`prec->val = prec->sval`) and still clears UDF — not a
//! failure. Conflating the two is what made `caput REC.SIMM 1; caput REC.SVAL
//! 42` a no-op on this port (R12-61).

use crate::types::EpicsValue;

/// The simulation mode SIMM selects, resolved from the raw menu index.
///
/// `menuSimm` (`menuSimm.dbd`) is `NO`/`YES`/`RAW`; the 13 records whose SIMM
/// is `menu(menuYesNo)` carry only `NO`/`YES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimMode {
    /// `menuSimmNO` — no simulation; the real device read/write runs.
    No,
    /// `menuSimmYES` — the SIOL round-trip substitutes the device, carrying the
    /// record's cooked value (`prec->val` / `prec->oval`).
    Yes,
    /// `menuSimmRAW` — as `Yes`, but the SIOL round-trip carries the RAW value
    /// (`prec->rval`), so the record's own conversion chain still runs
    /// (`aiRecord.c:494-497`, `aoRecord.c:575-577`).
    Raw,
}

impl SimMode {
    /// C's `switch (prec->simm)` selector.
    pub fn from_index(simm: i16) -> Self {
        match simm {
            0 => SimMode::No,
            2 => SimMode::Raw,
            _ => SimMode::Yes,
        }
    }

    /// Whether the device I/O is substituted this cycle.
    pub fn is_simulated(self) -> bool {
        !matches!(self, SimMode::No)
    }
}

/// The outcome of a C `dbGetLink` / `dbTryGetLink` on a simulation link
/// (SIML or SIOL) — the three things the C status + buffer pair can mean,
/// kept apart so no caller can read "no data" as "failure".
#[derive(Debug, Clone, PartialEq)]
pub enum SimLinkFetch {
    /// status 0, and the link wrote the buffer: a DB / CA / PVA / calc link
    /// that read successfully.
    Value(EpicsValue),
    /// status 0, and the link wrote NOTHING: a CONSTANT link (including an
    /// unset one) — `dbConstGetValue` (`dbConstLink.c:219-225`). The record's
    /// buffer (SIMM, SVAL) keeps the value `init_record` loaded into it.
    NoData,
    /// non-zero status: the link exists but the read failed (target missing,
    /// CA disconnected). C `dbGetLink` raises LINK_ALARM/INVALID through
    /// `setLinkAlarm` here; C `recGblGetSimm` (which uses `dbTryGetLink`,
    /// bypassing that) sets `nsta = LINK_ALARM` itself.
    Failed,
}

impl SimLinkFetch {
    /// C's `if (status == 0)` — true for both `Value` and `NoData`, which is
    /// the whole point of keeping them distinct from `Failed`.
    pub fn is_ok(&self) -> bool {
        !matches!(self, SimLinkFetch::Failed)
    }
}

/// C `dbConstLoadScalar` (`dbConstLink.c:152-175`): the value a CONSTANT link
/// hands its record ONCE, at `init_record`, through `dbLoadLink`. An empty
/// constant returns `S_db_badField` — i.e. nothing is stored, the field keeps
/// its dbd `initial()` — which is the `None` here.
pub fn constant_load_value(link: &crate::server::record::ParsedLink) -> Option<EpicsValue> {
    match link {
        crate::server::record::ParsedLink::Constant(_) => link.constant_value(),
        _ => None,
    }
}

/// Whether a link is a C CONSTANT link — `dbLinkIsConstant` (`dbLink.c:220`).
/// An unset link is constant (its `lset` is `dbConst_lset` with a NULL string),
/// which is why `recGblInitSimm`'s `if (dbLinkIsConstant(psiml))` covers the
/// unset case too.
pub fn is_constant(link: &crate::server::record::ParsedLink) -> bool {
    matches!(
        link,
        crate::server::record::ParsedLink::None | crate::server::record::ParsedLink::Constant(_)
    )
}

/// The record types whose C `.dbd` declares `field(SSCN)` + `field(OLDSIMM)`
/// and whose `special()` routes `SPC_MOD` on SIMM to
/// `recGblSaveSimm`/`recGblCheckSimm` — i.e. the records that own a
/// simulation-mode SCAN swap.
///
/// Enumerated from the C, not from the port:
/// `rg -l 'field\(SSCN' modules/database/src/std/rec/` lists exactly these 21.
/// `busy` (`busyRecord.dbd`) and `swait` (`swaitRecord.dbd`) carry SIMM/SIML/
/// SIOL but NO SSCN and NO OLDSIMM: their C reads SIML with a plain
/// `dbGetLink` and never calls recGblSaveSimm/recGblCheckSimm, so a SIMM
/// transition must NOT touch their SCAN. So do `mca` and `digitel`
/// (unported). Every other record type has no SIMM at all.
const RECORDS_WITH_SSCN: &[&str] = &[
    "aai",
    "aao",
    "ai",
    "ao",
    "bi",
    "bo",
    "event",
    "histogram",
    "int64in",
    "int64out",
    "longin",
    "longout",
    "lsi",
    "lso",
    "mbbi",
    "mbbiDirect",
    "mbbo",
    "mbboDirect",
    "stringin",
    "stringout",
    "waveform",
];

/// Whether a record type participates in the SIMM↔SSCN scan swap
/// (`recGblCheckSimm`). The single source of truth behind
/// [`crate::server::record::Record::has_sim_mode_scan`]; see
/// [`RECORDS_WITH_SSCN`].
pub fn record_type_has_sscn(record_type: &str) -> bool {
    RECORDS_WITH_SSCN.contains(&record_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_21_c_records_with_field_sscn_swap_scan() {
        // The C population (`rg -l 'field\(SSCN' modules/database/src/std/rec/`).
        assert!(record_type_has_sscn("longin"));
        assert!(record_type_has_sscn("mbbiDirect"));
        assert!(record_type_has_sscn("waveform"));
        // SIMM/SIML/SIOL but no SSCN, no OLDSIMM, no recGblCheckSimm call.
        assert!(!record_type_has_sscn("busy"));
        assert!(!record_type_has_sscn("swait"));
        // No simulation block at all.
        assert!(!record_type_has_sscn("calc"));
    }

    #[test]
    fn sim_mode_indices_match_menu_simm() {
        assert_eq!(SimMode::from_index(0), SimMode::No);
        assert_eq!(SimMode::from_index(1), SimMode::Yes);
        assert_eq!(SimMode::from_index(2), SimMode::Raw);
        assert!(!SimMode::No.is_simulated());
        assert!(SimMode::Yes.is_simulated());
        assert!(SimMode::Raw.is_simulated());
    }

    #[test]
    fn constant_and_unset_links_are_both_constant() {
        use crate::server::record::ParsedLink;
        assert!(is_constant(&ParsedLink::None));
        assert!(is_constant(&ParsedLink::Constant("42".into())));
        assert!(!is_constant(&crate::server::record::parse_link_v2(
            "SOME:PV"
        )));
    }

    #[test]
    fn no_data_is_a_success_not_a_failure() {
        assert!(SimLinkFetch::NoData.is_ok());
        assert!(SimLinkFetch::Value(EpicsValue::Long(1)).is_ok());
        assert!(!SimLinkFetch::Failed.is_ok());
    }
}
