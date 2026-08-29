//! The C simulation-mode contract — `recGbl.c:421-457`
//! (`recGblSaveSimm`, `recGblCheckSimm`, `recGblInitSimm`, `recGblGetSimm`)
//! plus the `recGblInitConstantLink(&siol, …, &sval)` every SIML/SIOL-bearing
//! record pairs with them in `init_record`.
//!
//! This module holds the *pure* half of that contract (mode resolution and the
//! link-fetch classification); the transition itself is driven by the single
//! owner `crate::server::database::PvDatabase::rec_gbl_get_simm` /
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
    ///
    /// Only reachable on a record whose SIMM is `menu(menuSimm)`.
    Raw,
    /// The `default:` arm of C's `switch (prec->simm)`: a SIMM value that is not
    /// a choice of THIS record's SIMM menu.
    ///
    /// ```c
    /// default:
    ///     recGblSetSevr(prec, SOFT_ALARM, INVALID_ALARM);
    ///     status = -1;
    /// ```
    ///
    /// No device I/O, no SIOL round-trip, no SIMM_ALARM, no value or UDF
    /// change — the record's `process()` ignores the `-1` for control flow, so
    /// `checkAlarms`/`monitor`/`recGblFwdLink` still run. Two ways in:
    ///
    /// * `SIMM = 2` (RAW) on one of the 13 records whose SIMM is
    ///   `menu(menuYesNo)` — the menu has no RAW choice, so 2 hits `default:`.
    /// * ANY out-of-menu index on ANY of them: `recGblGetSimm` reads SIML with
    ///   `dbTryGetLink(psiml, DBR_USHORT, psimm, 0)`, which performs NO menu
    ///   validation — a SIML pointing at a record holding 7 puts 7 in SIMM.
    Illegal,
}

impl SimMode {
    /// Whether the device I/O is substituted this cycle.
    pub fn is_simulated(self) -> bool {
        !matches!(self, SimMode::No)
    }
}

/// Resolve a record's C `switch (prec->simm)` — **the single owner of that
/// dispatch**, because the switch is not the same on every record and the
/// difference is the whole of R11-C12.
///
/// The legal arms are the choices of THIS record's own SIMM menu:
///
/// * `menu(menuSimm)` (ai, ao, bi, bo, mbbi, mbbo, mbbiDirect, mbboDirect —
///   3 choices): `case menuSimmNO / menuSimmYES / menuSimmRAW`.
/// * `menu(menuYesNo)` (the other 13, plus busy — 2 choices):
///   `case menuYesNoNO / menuYesNoYES`. **RAW is not a choice**, so `SIMM = 2`
///   falls to `default:` — `recGblSetSevr(SOFT_ALARM, INVALID_ALARM)`, with no
///   device substitution at all. The port previously ran these records' RAW
///   branch, substituting a device read C refuses to perform.
///
/// swait is the one record with no `default:` arm at all
/// (`swaitRecord.c:407-422` is a plain `if (simm == menuYesNoNO) … else …`):
/// every non-NO value simulates. It says so via
/// [`Record::rejects_illegal_sim_mode`](crate::server::record::Record::rejects_illegal_sim_mode).
pub fn resolve_sim_mode(record: &dyn crate::server::record::Record) -> SimMode {
    let simm = record
        .get_field("SIMM")
        .and_then(|v| v.to_f64())
        .unwrap_or(0.0) as i16;
    // The record's own SIMM menu decides which indices are legal. Every record
    // that declares SIMM declares its menu; a record with no SIMM never reaches
    // here (the caller's entry gate), so the fallback is inert.
    let choices = record
        .menu_field_choices("SIMM")
        .map(|c| c.len())
        .unwrap_or(0);
    match simm {
        0 => SimMode::No,
        1 => SimMode::Yes,
        2 if choices >= 3 => SimMode::Raw,
        // swait's `else` arm: anything that is not NO simulates.
        _ if !record.rejects_illegal_sim_mode() => SimMode::Yes,
        _ => SimMode::Illegal,
    }
}

/// The outcome of a C `dbGetLink` / `dbTryGetLink` on ANY link at process
/// time — the three things the C status + buffer pair can mean, kept apart so
/// no caller can read "no data" as "failure".
///
/// This is the process-time link-read classification for the whole database,
/// not just the simulation links: SIML/SIOL, SDIS, TSEL, SELL, SUBL, INPA..L,
/// INAA..LL and DOL all cross it. The rule it encodes is one rule —
/// `dbConstGetValue` (`dbConstLink.c:219-225`) sets `*pnRequest = 0` and
/// returns SUCCESS, so a CONSTANT link delivers NOTHING on every process
/// cycle, whatever field holds it. The constant's value reaches the record
/// exactly once, at init, through the init-seed owner
/// (`PvDatabase::rec_gbl_init_constant_links`).
#[derive(Debug, Clone, PartialEq)]
pub enum LinkFetch {
    /// status 0, and the link wrote the buffer: a DB / CA / PVA / calc link
    /// that read successfully.
    Value(EpicsValue),
    /// status 0, and the link wrote NOTHING: a CONSTANT link (including an
    /// unset one) — `dbConstGetValue` (`dbConstLink.c:219-225`). The record's
    /// buffer (SIMM, SVAL, A..L, SELN, ...) keeps the value `init_record`
    /// loaded into it.
    NoData,
    /// non-zero status: the link exists but the read failed (target missing,
    /// CA disconnected). C `dbGetLink` raises LINK_ALARM/INVALID through
    /// `setLinkAlarm` here; C `recGblGetSimm` (which uses `dbTryGetLink`,
    /// bypassing that) sets `nsta = LINK_ALARM` itself.
    Failed,
}

impl LinkFetch {
    /// C's `if (status == 0)` — true for both `Value` and `NoData`, which is
    /// the whole point of keeping them distinct from `Failed`.
    pub fn is_ok(&self) -> bool {
        !matches!(self, LinkFetch::Failed)
    }

    /// The delivered value, if the link delivered one. `None` for BOTH
    /// `NoData` and `Failed` — only use where the two are already known to be
    /// equivalent (a caller that just wants the value and has no gate).
    pub fn value(self) -> Option<EpicsValue> {
        match self {
            LinkFetch::Value(v) => Some(v),
            LinkFetch::NoData | LinkFetch::Failed => None,
        }
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
/// [`crate::server::record::Record::uses_recgbl_simm_helpers`]; see
/// `RECORDS_WITH_SSCN`.
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
    fn simm_2_is_raw_on_a_menu_simm_record_and_illegal_on_a_menu_yesno_one() {
        use crate::server::record::Record;
        use crate::server::records::ai::AiRecord; // SIMM is menu(menuSimm)
        use crate::server::records::longin::LonginRecord; // SIMM is menu(menuYesNo)

        let mut ai = AiRecord::new(0.0);
        let mut li = LonginRecord::new(0);
        for (simm, ai_mode, li_mode) in [
            (0i16, SimMode::No, SimMode::No),
            (1, SimMode::Yes, SimMode::Yes),
            (2, SimMode::Raw, SimMode::Illegal),
            (7, SimMode::Illegal, SimMode::Illegal),
        ] {
            ai.put_field("SIMM", EpicsValue::Short(simm)).unwrap();
            li.put_field("SIMM", EpicsValue::Short(simm)).unwrap();
            assert_eq!(resolve_sim_mode(&ai), ai_mode, "ai SIMM={simm}");
            assert_eq!(resolve_sim_mode(&li), li_mode, "longin SIMM={simm}");
        }
        assert!(!SimMode::No.is_simulated());
        assert!(SimMode::Yes.is_simulated());
        assert!(SimMode::Raw.is_simulated());
        assert!(SimMode::Illegal.is_simulated());
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
        assert!(LinkFetch::NoData.is_ok());
        assert!(LinkFetch::Value(EpicsValue::Long(1)).is_ok());
        assert!(!LinkFetch::Failed.is_ok());
    }
}
