use crate::types::PvString;
use epics_macros_rs::EpicsRecord;

/// Choice labels for the `fanout` select-mechanism menu, in index order.
/// C `menu(fanoutSELM)` (`fanoutRecord.dbd.pod:30-33`): 0=All,
/// 1=Specified, 2=Mask.
const FANOUT_SELM_CHOICES: &[&str] = &["All", "Specified", "Mask"];

static FANOUT_POSTED: [crate::server::record::FieldIx; 2] = crate::server::record::declared_set(
    crate::server::record::dbd_generated::FANOUT_FIELDS,
    ["VAL", "SELN"],
);

/// `fanout` record — forward-link fan-out.
///
/// C parity (`fanoutRecord.c:39` `#define NLINKS 16`,
/// `fanoutRecord.dbd.pod:139-214`): the record carries 16 forward
/// links `LNK0..LNKF`. The first slot `LNK0` is a real field — a
/// `.db` file written for C semantics frequently puts the primary
/// fan-out target on `LNK0`. Omitting it shifts every link index
/// by one and silently drops the `LNK0` target on `SELM=All`.
#[derive(EpicsRecord)]
// C `fanoutRecord.c:88`: `recGblInitConstantLink(&prec->sell, DBF_USHORT,
// &prec->seln)` — a constant SELL loads SELN ONCE, at init. `dbGetLink` on it
// at process delivers nothing, so a later `caput REC.SELN` is not stomped.
// `no_value_monitor`: fanout's VAL is a `pp(TRUE)` "trigger" — C `process()`
// posts VAL only with alarm events (`if (events) db_post_events(&prec->val,
// events)`, fanoutRecord.c:148-150), never DBE_VALUE/DBE_LOG. So a run of
// `caput VAL` posts no per-put value monitor (only the connect-time snapshot).
// `process_posted_fields`: `process()` posts VAL and SELN alone
// (`fanoutRecord.c:147-152`; fanout has no `monitor()`).
// `link_read_as`: SELL is read at `DBR_USHORT` (`fanoutRecord.c:103`), so a
// source that request cannot convert is a failed read.
#[record(
    type = "fanout",
    constant_init = "SELL:SELN",
    link_read_as(SELL = UShort),
    no_value_monitor,
    process_posted_fields = FANOUT_POSTED,
    spill()
)]
pub struct FanoutRecord {
    // VAL is `field(VAL,DBF_LONG){ pp(TRUE) }` (fanoutRecord.dbd:21-25) — the
    // "Used to trigger" field. It carries no output value: C `process`
    // (fanoutRecord.c:92-158) never reads or writes VAL, it only fans out the
    // forward links. A `caput X.VAL 1` therefore stores 1 verbatim and pp(TRUE)
    // reprocesses (a no-op leaving VAL for a link-less record). Declaring it
    // `DBF_LONG` (i32) is what routes a `DBR_STRING` put through the numeric
    // `c_parse::put_string` Long row; the previous `Enum`/u16 declaration sent it
    // to the enum choice-matcher, which refused a bare "1" as S_db_badChoice.
    #[field(type = "Long")]
    pub val: i32,
    // SELM is DBF_MENU menu(fanoutSELM) (fanoutRecord.dbd.pod:111): served
    // as DBR_ENUM with the menu's choice labels (FANOUT_SELM_CHOICES). The
    // index is stored as a short; the framework promotes it to Enum.
    #[field(type = "Short", menu_choices = FANOUT_SELM_CHOICES)]
    pub selm: i16,
    // SELN is `DBF_USHORT` (fanoutRecord.dbd.pod:117): unsigned 0..65535.
    #[field(type = "UShort")]
    pub seln: u16,
    #[field(type = "PvStr", link)]
    pub lnk0: PvString,
    #[field(type = "PvStr", link)]
    pub lnk1: PvString,
    #[field(type = "PvStr", link)]
    pub lnk2: PvString,
    #[field(type = "PvStr", link)]
    pub lnk3: PvString,
    #[field(type = "PvStr", link)]
    pub lnk4: PvString,
    #[field(type = "PvStr", link)]
    pub lnk5: PvString,
    #[field(type = "PvStr", link)]
    pub lnk6: PvString,
    #[field(type = "PvStr", link)]
    pub lnk7: PvString,
    #[field(type = "PvStr", link)]
    pub lnk8: PvString,
    #[field(type = "PvStr", link)]
    pub lnk9: PvString,
    #[field(type = "PvStr", link)]
    pub lnka: PvString,
    #[field(type = "PvStr", link)]
    pub lnkb: PvString,
    #[field(type = "PvStr", link)]
    pub lnkc: PvString,
    #[field(type = "PvStr", link)]
    pub lnkd: PvString,
    #[field(type = "PvStr", link)]
    pub lnke: PvString,
    #[field(type = "PvStr", link)]
    pub lnkf: PvString,
    #[field(type = "PvStr", link)]
    pub sell: PvString,
    #[field(type = "Short")]
    pub offs: i16,
    #[field(type = "Short")]
    pub shft: i16,
}

impl Default for FanoutRecord {
    fn default() -> Self {
        Self {
            val: 0,
            selm: 0,
            // C `fanoutRecord.dbd.pod` `field(SELN,DBF_USHORT){ initial("1") }`:
            // an unset SELN defaults to 1, not 0. Only observable when the
            // .db omits SELN and SELM is Specified/Mask (All ignores SELN).
            seln: 1,
            lnk0: PvString::new(),
            lnk1: PvString::new(),
            lnk2: PvString::new(),
            lnk3: PvString::new(),
            lnk4: PvString::new(),
            lnk5: PvString::new(),
            lnk6: PvString::new(),
            lnk7: PvString::new(),
            lnk8: PvString::new(),
            lnk9: PvString::new(),
            lnka: PvString::new(),
            lnkb: PvString::new(),
            lnkc: PvString::new(),
            lnkd: PvString::new(),
            lnke: PvString::new(),
            lnkf: PvString::new(),
            sell: PvString::new(),
            offs: 0,
            // C `fanoutRecord.dbd.pod:133` `field(SHFT,DBF_SHORT){ initial("-1") }`:
            // when SHFT is not set in the .db file it defaults to -1, so in
            // SELM=Mask the SELN bits shift LEFT by 1 (`seln << 1`). A 0
            // default would instead leave SELN unshifted and fire the wrong
            // forward links.
            shft: -1,
        }
    }
}

impl FanoutRecord {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get all non-empty link targets, in `LNK0..LNKF` order.
    pub fn links(&self) -> Vec<&[u8]> {
        [
            &self.lnk0, &self.lnk1, &self.lnk2, &self.lnk3, &self.lnk4, &self.lnk5, &self.lnk6,
            &self.lnk7, &self.lnk8, &self.lnk9, &self.lnka, &self.lnkb, &self.lnkc, &self.lnkd,
            &self.lnke, &self.lnkf,
        ]
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.as_bytes())
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::database::{SelmKind, select_link_indices_ex};

    /// C dbd `initial("-1")` parity: an unset SHFT defaults to -1, so a
    /// SELM=Mask selection shifts SELN bits LEFT by 1. With the previous 0
    /// default, SELN=1 fired LNK0; the dbd default fires LNK1.
    #[test]
    fn default_shft_is_minus_one_and_mask_shifts_left() {
        let rec = FanoutRecord::default();
        assert_eq!(rec.shft, -1);
        // SELM=Mask(2), SELN=1, default SHFT=-1 → mask = 1<<1 = 0b10 → LNK1.
        let sel = select_link_indices_ex(SelmKind::FanoutSeq, 2, 1, rec.offs, rec.shft);
        assert_eq!(sel.indices().collect::<Vec<_>>(), vec![1]);
    }
}
