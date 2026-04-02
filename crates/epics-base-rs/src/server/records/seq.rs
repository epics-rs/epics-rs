use epics_macros_rs::EpicsRecord;

#[derive(EpicsRecord)]
#[record(type = "seq")]
pub struct SeqRecord {
    #[field(type = "Enum")]
    pub val: u16,
    #[field(type = "Short")]
    pub selm: i16,
    #[field(type = "Short")]
    pub seln: i16,
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
}

impl Default for SeqRecord {
    fn default() -> Self {
        Self {
            val: 0,
            selm: 0,
            seln: 0,
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
        }
    }
}

impl SeqRecord {
    pub fn new() -> Self {
        Self::default()
    }
}
