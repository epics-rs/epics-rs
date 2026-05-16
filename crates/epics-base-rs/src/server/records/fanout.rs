use epics_macros_rs::EpicsRecord;

/// `fanout` record — forward-link fan-out.
///
/// C parity (`fanoutRecord.c:39` `#define NLINKS 16`,
/// `fanoutRecord.dbd.pod:139-214`): the record carries 16 forward
/// links `LNK0..LNKF`. The first slot `LNK0` is a real field — a
/// `.db` file written for C semantics frequently puts the primary
/// fan-out target on `LNK0`. Omitting it shifts every link index
/// by one and silently drops the `LNK0` target on `SELM=All`.
#[derive(EpicsRecord)]
#[record(type = "fanout")]
pub struct FanoutRecord {
    #[field(type = "Enum")]
    pub val: u16,
    #[field(type = "Short")]
    pub selm: i16,
    #[field(type = "Short")]
    pub seln: i16,
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
            seln: 0,
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
            shft: 0,
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
