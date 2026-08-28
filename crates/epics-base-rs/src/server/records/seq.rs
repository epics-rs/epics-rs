use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use epics_macros_rs::EpicsRecord;

/// Choice labels for the `seq` select-mechanism menu, in index order.
/// C `menu(seqSELM)` (`seqRecord.dbd.pod:23-26`): 0=All, 1=Specified,
/// 2=Mask.
const SEQ_SELM_CHOICES: &[&str] = &["All", "Specified", "Mask"];

/// `seqRecord.c:322-338` `get_graphic_double` answers the 16 `DLYn` delay
/// fields with a hard-coded `0.0 .. 10.0`, not with their DBF_DOUBLE range and
/// not from any record field — so `DLYn` can take neither the routed
/// `default:` arm nor the VAL cache.
///
/// The `10.0` is the UPPER only from `b03213ea7` on: through R7.0.10 the line
/// assigns `lower_disp_limit` twice and the upper keeps `dbAccess.c`'s seed.
/// This models the corrected form, so a reader resolving the citation against
/// the R7.0.10 tag sees the typo, not this.
///
/// The literal is graphic's alone: the same record's `get_control_double`
/// (`:342-352`) serves `0.0 .. seqDLYlimit`, and `seqDLYlimit` is 100000
/// (`:81`), not 10. Reading one slot's arm off the other would be wrong by
/// four orders of magnitude, which is why each slot is transcribed separately.
///
/// `get_units` (`:282-297`) and `get_precision` (`:299-319`) answer the same
/// 16 fields with their own literals — `"s"` and `seqDLYprecision` (`= 2`,
/// `:78`). Both switch on `(fieldIndex - indexof(DLY0)) & 3 == 0`, the DLYn
/// slot of each four-field link group, so the predicate is the same one the
/// graphic arm uses. The `DOn` slot (`& 3 == 2`) reads its units/precision
/// from the DOLn link instead, and `dbGetPrecision` on a CONSTANT link fails
/// (`S_db_noLSET`) into the same `prec->prec` fall-through every other field
/// takes — which is
/// [`route_field_metadata`](crate::server::record::RecordInstance)'s answer,
/// not this override's. A CONNECTED DOLn is that same routing's LINK arm, fed
/// by [`seq_link_metadata_field`] below.
fn seq_metadata_override(
    _rec: &SeqRecord,
    field: &str,
) -> Option<crate::server::record::FieldMetadataOverride> {
    // DLY0..DLY9, DLYA..DLYF — one per link group.
    let f = field.to_ascii_uppercase();
    let is_dly = matches!(f.as_bytes(), [b'D', b'L', b'Y', c]
        if c.is_ascii_digit() || (b'A'..=b'F').contains(c));
    is_dly.then(|| crate::server::record::FieldMetadataOverride {
        units: Some("s".into()),
        precision: Some(seq_dly_precision() as i16),
        disp_limits: Some((10.0, 0.0)),
        ctrl_limits: Some((seq_dly_limit(), 0.0)),
        ..Default::default()
    })
}

/// C `seqRecord.c:78` `int seqDLYprecision = 2;` — the precision
/// `get_precision` serves for every `DLYn`.
static SEQ_DLY_PRECISION: AtomicI32 = AtomicI32::new(2);

/// The iocsh knob `seqDLYprecision`, read and written by `var`.
pub(crate) fn seq_dly_precision() -> i32 {
    SEQ_DLY_PRECISION.load(Ordering::Relaxed)
}

/// See [`seq_dly_precision`].
pub(crate) fn set_seq_dly_precision(value: i32) {
    SEQ_DLY_PRECISION.store(value, Ordering::Relaxed);
}

/// C `seqRecord.c:81` `double seqDLYlimit = 100000;` — the control upper
/// `get_control_double` (`:342-353`) serves for every `DLYn`, over a literal
/// `0.0` lower.
///
/// Four orders of magnitude from the SAME field's graphic upper of `10.0`
/// (`:321-338`): a `DLYn` is offered a 0..100000 s settable range while its
/// display bar reads 0..10 s. Two slots, two literals, one field.
static SEQ_DLY_LIMIT: AtomicU64 = AtomicU64::new(100000f64.to_bits());

/// The iocsh knob `seqDLYlimit`, read and written by `var`.
pub(crate) fn seq_dly_limit() -> f64 {
    f64::from_bits(SEQ_DLY_LIMIT.load(Ordering::Relaxed))
}

/// See [`seq_dly_limit`].
pub(crate) fn set_seq_dly_limit(value: f64) {
    SEQ_DLY_LIMIT.store(value.to_bits(), Ordering::Relaxed);
}

/// `seq` record — sequenced multi-output writer.
///
/// C parity (`seqRecord.c:86` `#define NUM_LINKS 16`,
/// `seqRecord.dbd.pod:302-...`): 16 link groups `0..F`, each a
/// `linkGrp { DLYn, DOLn, DOn (value storage), LNKn }`. The legacy
/// 3.14 layout was 10 groups starting at index 1; modern EPICS uses
/// 16 groups starting at `DOL0`. Per-group `DLYn` staggers the
/// writes — that delayed sequencing is the record's purpose.
#[derive(EpicsRecord)]
// C `seqRecord.c:121-126`: `recGblInitConstantLink(&prec->sell, DBF_USHORT,
// &prec->seln)` and, per group, `recGblInitConstantLink(&grp->dol, DBF_DOUBLE,
// &grp->dov)`. Every one of those links is init-only: `dbGetLink` on a constant
// delivers nothing at process (`seqRecord.c:259` reads DOLn into DOn each
// cycle, and a constant DOLn leaves DOn alone), so `field(DOL0,"4")` reaches
// DO0 here or never.
#[record(
    type = "seq",
    metadata_override = seq_metadata_override,
    link_metadata_field = seq_link_metadata_field,
    constant_init = "SELL:SELN,DOL0:DO0,DOL1:DO1,DOL2:DO2,DOL3:DO3,DOL4:DO4,\
                     DOL5:DO5,DOL6:DO6,DOL7:DO7,DOL8:DO8,DOL9:DO9,DOLA:DOA,\
                     DOLB:DOB,DOLC:DOC,DOLD:DOD,DOLE:DOE,DOLF:DOF",
    init = seq_init_record,
    // seq's VAL is a `pp(TRUE)` "trigger" — C `process()` posts VAL only with
    // alarm events (`if (events) db_post_events(&prec->val, events)`,
    // seqRecord.c:227-229), never DBE_VALUE/DBE_LOG. So a run of `caput VAL`
    // posts no per-put value monitor (only the connect-time snapshot).
    no_value_monitor
)]
pub struct SeqRecord {
    // VAL is `field(VAL,DBF_LONG){ pp(TRUE) }` (seqRecord.dbd:21-25) — the "Used
    // to trigger" field. It carries no output value: C `process`
    // (seqRecord.c) never reads or writes VAL, it only sequences the DOn→LNKn
    // writes. A `caput X.VAL 1` therefore stores 1 verbatim and pp(TRUE)
    // reprocesses. Declaring it `DBF_LONG` (i32) routes a `DBR_STRING` put
    // through the numeric `c_parse::put_string` Long row; the previous
    // `Enum`/u16 declaration sent it to the enum choice-matcher, which refused a
    // bare "1" as S_db_badChoice.
    #[field(type = "Long")]
    pub val: i32,
    // OLDN is `DBF_USHORT` (seqRecord.dbd:54), change-detection state for the
    // SELN monitor: C `process` posts a SELN monitor when `seln != oldn`, then
    // copies `oldn = seln` (seqRecord.c:230-232). Its init value is `seln`
    // (`seqRecord.c:128` `prec->oldn = prec->seln`), served by
    // `seq_init_record`; the .dbd has no `initial(...)`, so a record that did
    // not store OLDN would serve type-zero (0) where C serves the copied SELN.
    #[field(type = "UShort")]
    pub oldn: u16,
    // SELM is DBF_MENU menu(seqSELM) (seqRecord.dbd.pod:265): served as
    // DBR_ENUM with the menu's choice labels (SEQ_SELM_CHOICES). The index
    // is stored as a short; the framework promotes it to Enum.
    #[field(type = "Short", menu_choices = SEQ_SELM_CHOICES)]
    pub selm: i16,
    // SELN is `DBF_USHORT` (seqRecord.dbd.pod:271): unsigned 0..65535.
    #[field(type = "UShort")]
    pub seln: u16,
    #[field(type = "String")]
    pub sell: String,
    #[field(type = "Short")]
    pub offs: i16,
    #[field(type = "Short")]
    pub shft: i16,
    #[field(type = "Double")]
    pub dly0: f64,
    #[field(type = "Double")]
    pub dly1: f64,
    #[field(type = "Double")]
    pub dly2: f64,
    #[field(type = "Double")]
    pub dly3: f64,
    #[field(type = "Double")]
    pub dly4: f64,
    #[field(type = "Double")]
    pub dly5: f64,
    #[field(type = "Double")]
    pub dly6: f64,
    #[field(type = "Double")]
    pub dly7: f64,
    #[field(type = "Double")]
    pub dly8: f64,
    #[field(type = "Double")]
    pub dly9: f64,
    #[field(type = "Double")]
    pub dlya: f64,
    #[field(type = "Double")]
    pub dlyb: f64,
    #[field(type = "Double")]
    pub dlyc: f64,
    #[field(type = "Double")]
    pub dlyd: f64,
    #[field(type = "Double")]
    pub dlye: f64,
    #[field(type = "Double")]
    pub dlyf: f64,
    #[field(type = "String")]
    pub dol0: String,
    #[field(type = "String")]
    pub dol1: String,
    #[field(type = "String")]
    pub dol2: String,
    #[field(type = "String")]
    pub dol3: String,
    #[field(type = "String")]
    pub dol4: String,
    #[field(type = "String")]
    pub dol5: String,
    #[field(type = "String")]
    pub dol6: String,
    #[field(type = "String")]
    pub dol7: String,
    #[field(type = "String")]
    pub dol8: String,
    #[field(type = "String")]
    pub dol9: String,
    #[field(type = "String")]
    pub dola: String,
    #[field(type = "String")]
    pub dolb: String,
    #[field(type = "String")]
    pub dolc: String,
    #[field(type = "String")]
    pub dold: String,
    #[field(type = "String")]
    pub dole: String,
    #[field(type = "String")]
    pub dolf: String,
    #[field(type = "Double")]
    pub do0: f64,
    #[field(type = "Double")]
    pub do1: f64,
    #[field(type = "Double")]
    pub do2: f64,
    #[field(type = "Double")]
    pub do3: f64,
    #[field(type = "Double")]
    pub do4: f64,
    #[field(type = "Double")]
    pub do5: f64,
    #[field(type = "Double")]
    pub do6: f64,
    #[field(type = "Double")]
    pub do7: f64,
    #[field(type = "Double")]
    pub do8: f64,
    #[field(type = "Double")]
    pub do9: f64,
    #[field(type = "Double")]
    pub doa: f64,
    #[field(type = "Double")]
    pub dob: f64,
    #[field(type = "Double")]
    pub doc: f64,
    #[field(type = "Double")]
    pub dod: f64,
    #[field(type = "Double")]
    pub doe: f64,
    #[field(type = "Double")]
    pub dof: f64,
    #[field(type = "String")]
    pub lnk0: String,
    #[field(type = "String")]
    pub lnk1: String,
    #[field(type = "String")]
    pub lnk2: String,
    #[field(type = "String")]
    pub lnk3: String,
    #[field(type = "String")]
    pub lnk4: String,
    #[field(type = "String")]
    pub lnk5: String,
    #[field(type = "String")]
    pub lnk6: String,
    #[field(type = "String")]
    pub lnk7: String,
    #[field(type = "String")]
    pub lnk8: String,
    #[field(type = "String")]
    pub lnk9: String,
    #[field(type = "String")]
    pub lnka: String,
    #[field(type = "String")]
    pub lnkb: String,
    #[field(type = "String")]
    pub lnkc: String,
    #[field(type = "String")]
    pub lnkd: String,
    #[field(type = "String")]
    pub lnke: String,
    #[field(type = "String")]
    pub lnkf: String,
}

/// `seqRecord.c:279-280` `get_dol` — within each of the 16 `linkGrp`s the
/// field at offset 2 is `DOn`, whose units/precision/graphic/alarm C reads
/// from that group's `DOLn` (`:293`, `:311`, `:333`, `:361`). `DOLn` itself is
/// four bytes and never matches, so the mapping is exactly `DOn` -> `DOLn`
/// over the group suffixes `0`..`9`, `A`..`F`.
fn seq_link_metadata_field(_rec: &SeqRecord, field: &str) -> Option<String> {
    let &[b'D', b'O', c] = field.as_bytes() else {
        return None;
    };
    (c.is_ascii_digit() || (b'A'..=b'F').contains(&c)).then(|| format!("DOL{}", c as char))
}

impl Default for SeqRecord {
    fn default() -> Self {
        Self {
            val: 0,
            // Init state; `seq_init_record` copies SELN into it at pass 1,
            // matching C `seqRecord.c:128`.
            oldn: 0,
            selm: 0,
            // C `seqRecord.dbd.pod` `field(SELN,DBF_USHORT){ initial("1") }`:
            // an unset SELN defaults to 1, not 0. Only observable when the
            // .db omits SELN and SELM is Specified/Mask (All ignores SELN).
            seln: 1,
            sell: String::new(),
            offs: 0,
            // C `seqRecord.dbd.pod:287` `field(SHFT,DBF_SHORT){ initial("-1") }`:
            // unset SHFT defaults to -1, so SELM=Mask shifts SELN bits LEFT by
            // 1 (`seln << 1`). The POD note: "If not set, the SHFT field is -1
            // so bits from SELN are shifted left by 1." A 0 default fires the
            // wrong links (e.g. SELN=1 would drive DOL0/LNK0 instead of
            // DOL1/LNK1).
            shft: -1,
            dly0: 0.0,
            dly1: 0.0,
            dly2: 0.0,
            dly3: 0.0,
            dly4: 0.0,
            dly5: 0.0,
            dly6: 0.0,
            dly7: 0.0,
            dly8: 0.0,
            dly9: 0.0,
            dlya: 0.0,
            dlyb: 0.0,
            dlyc: 0.0,
            dlyd: 0.0,
            dlye: 0.0,
            dlyf: 0.0,
            dol0: String::new(),
            dol1: String::new(),
            dol2: String::new(),
            dol3: String::new(),
            dol4: String::new(),
            dol5: String::new(),
            dol6: String::new(),
            dol7: String::new(),
            dol8: String::new(),
            dol9: String::new(),
            dola: String::new(),
            dolb: String::new(),
            dolc: String::new(),
            dold: String::new(),
            dole: String::new(),
            dolf: String::new(),
            do0: 0.0,
            do1: 0.0,
            do2: 0.0,
            do3: 0.0,
            do4: 0.0,
            do5: 0.0,
            do6: 0.0,
            do7: 0.0,
            do8: 0.0,
            do9: 0.0,
            doa: 0.0,
            dob: 0.0,
            doc: 0.0,
            dod: 0.0,
            doe: 0.0,
            dof: 0.0,
            lnk0: String::new(),
            lnk1: String::new(),
            lnk2: String::new(),
            lnk3: String::new(),
            lnk4: String::new(),
            lnk5: String::new(),
            lnk6: String::new(),
            lnk7: String::new(),
            lnk8: String::new(),
            lnk9: String::new(),
            lnka: String::new(),
            lnkb: String::new(),
            lnkc: String::new(),
            lnkd: String::new(),
            lnke: String::new(),
            lnkf: String::new(),
        }
    }
}

impl SeqRecord {
    pub fn new() -> Self {
        Self::default()
    }
}

/// C `seqRecord.c:init_record` — `Record::init_record` for `seq`, wired through
/// `#[record(init = seq_init_record)]`.
///
/// C runs the body at pass 1 (`seqRecord.c:112-113` returns early on pass 0):
/// after `recGblInitConstantLink(&prec->sell, …, &prec->seln)`, it copies
/// `prec->oldn = prec->seln` (`seqRecord.c:121,128`) so the first `process`
/// posts a SELN monitor only on a real change. The SELL constant seed itself
/// is this framework's constant-link owner's job (`SELL:SELN` in
/// `constant_init`), which runs after the init passes; OLDN therefore tracks
/// the default/`.db` SELN here. A non-empty CONSTANT `field(SELL,…)` — the one
/// case where C seeds SELN inside `init_record` before this copy — is the sole
/// configuration where OLDN would differ, and is not exercised by the soft
/// database surface.
fn seq_init_record(rec: &mut SeqRecord, pass: u8) -> crate::error::CaResult<()> {
    if pass == 1 {
        rec.oldn = rec.seln;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::database::{SelmKind, select_link_indices_ex};

    /// C dbd `initial("-1")` parity (seqRecord.dbd.pod:287): an unset SHFT
    /// defaults to -1, so SELM=Mask shifts SELN bits LEFT by 1. With the
    /// previous 0 default, SELN=1 drove DOL0/LNK0; the dbd default drives
    /// DOL1/LNK1.
    #[test]
    fn default_shft_is_minus_one_and_mask_shifts_left() {
        let rec = SeqRecord::default();
        assert_eq!(rec.shft, -1);
        // SELM=Mask(2), SELN=1, default SHFT=-1 → mask = 1<<1 = 0b10 → slot 1.
        let sel = select_link_indices_ex(SelmKind::FanoutSeq, 2, 1, rec.offs, rec.shft, 16);
        assert_eq!(sel.indices, vec![1]);
    }
}
