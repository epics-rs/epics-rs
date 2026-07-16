use epics_macros_rs::EpicsRecord;

/// Choice labels for the `fanout` select-mechanism menu, in index order.
/// C `menu(fanoutSELM)` (`fanoutRecord.dbd.pod:30-33`): 0=All,
/// 1=Specified, 2=Mask.
const FANOUT_SELM_CHOICES: &[&str] = &["All", "Specified", "Mask"];

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
#[record(type = "fanout", constant_init = "SELL:SELN", no_value_monitor)]
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
    #[field(type = "String")]
    pub sell: String,
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
            sell: String::new(),
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
    pub fn links(&self) -> Vec<&str> {
        [
            &self.lnk0, &self.lnk1, &self.lnk2, &self.lnk3, &self.lnk4, &self.lnk5, &self.lnk6,
            &self.lnk7, &self.lnk8, &self.lnk9, &self.lnka, &self.lnkb, &self.lnkc, &self.lnkd,
            &self.lnke, &self.lnkf,
        ]
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.as_str())
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
        let sel = select_link_indices_ex(SelmKind::FanoutSeq, 2, 1, rec.offs, rec.shft, 16);
        assert_eq!(sel.indices, vec![1]);
    }
}
