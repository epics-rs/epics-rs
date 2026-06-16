use epics_macros_rs::EpicsRecord;

/// Choice labels for the `seq` select-mechanism menu, in index order.
/// C `menu(seqSELM)` (`seqRecord.dbd.pod:23-26`): 0=All, 1=Specified,
/// 2=Mask.
const SEQ_SELM_CHOICES: &[&str] = &["All", "Specified", "Mask"];

/// `seq` record — sequenced multi-output writer.
///
/// C parity (`seqRecord.c:86` `#define NUM_LINKS 16`,
/// `seqRecord.dbd.pod:302-...`): 16 link groups `0..F`, each a
/// `linkGrp { DLYn, DOLn, DOn (value storage), LNKn }`. The legacy
/// 3.14 layout was 10 groups starting at index 1; modern EPICS uses
/// 16 groups starting at `DOL0`. Per-group `DLYn` staggers the
/// writes — that delayed sequencing is the record's purpose.
#[derive(EpicsRecord)]
#[record(type = "seq")]
pub struct SeqRecord {
    #[field(type = "Enum")]
    pub val: u16,
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

impl Default for SeqRecord {
    fn default() -> Self {
        Self {
            val: 0,
            selm: 0,
            seln: 0,
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
