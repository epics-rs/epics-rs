use epics_macros::EpicsRecord;

#[derive(EpicsRecord)]
#[record(type = "mbbo")]
pub struct MbboRecord {
    #[field(type = "Enum")]
    pub val: u16,
    #[field(type = "Short")]
    pub nobt: i16,
    #[field(type = "Short")]
    pub zrsv: i16,
    #[field(type = "Short")]
    pub onsv: i16,
    #[field(type = "Short")]
    pub twsv: i16,
    #[field(type = "Short")]
    pub thsv: i16,
    #[field(type = "Short")]
    pub frsv: i16,
    #[field(type = "Short")]
    pub fvsv: i16,
    #[field(type = "Short")]
    pub sxsv: i16,
    #[field(type = "Short")]
    pub svsv: i16,
    #[field(type = "Short")]
    pub eisv: i16,
    #[field(type = "Short")]
    pub nisv: i16,
    #[field(type = "Short")]
    pub tesv: i16,
    #[field(type = "Short")]
    pub elsv: i16,
    #[field(type = "Short")]
    pub tvsv: i16,
    #[field(type = "Short")]
    pub ttsv: i16,
    #[field(type = "Short")]
    pub ftsv: i16,
    #[field(type = "Short")]
    pub ffsv: i16,
    #[field(type = "Short")]
    pub unsv: i16,
    #[field(type = "Short")]
    pub cosv: i16,
    #[field(type = "Short")]
    pub omsl: i16,
    #[field(type = "String")]
    pub dol: String,
    #[field(type = "Long")]
    pub zrvl: i32,
    #[field(type = "Long")]
    pub onvl: i32,
    #[field(type = "Long")]
    pub twvl: i32,
    #[field(type = "Long")]
    pub thvl: i32,
    #[field(type = "Long")]
    pub frvl: i32,
    #[field(type = "Long")]
    pub fvvl: i32,
    #[field(type = "Long")]
    pub sxvl: i32,
    #[field(type = "Long")]
    pub svvl: i32,
    #[field(type = "Long")]
    pub eivl: i32,
    #[field(type = "Long")]
    pub nivl: i32,
    #[field(type = "Long")]
    pub tevl: i32,
    #[field(type = "Long")]
    pub elvl: i32,
    #[field(type = "Long")]
    pub tvvl: i32,
    #[field(type = "Long")]
    pub ttvl: i32,
    #[field(type = "Long")]
    pub ftvl: i32,
    #[field(type = "Long")]
    pub ffvl: i32,
    #[field(type = "String")]
    pub zrst: String,
    #[field(type = "String")]
    pub onst: String,
    #[field(type = "String")]
    pub twst: String,
    #[field(type = "String")]
    pub thst: String,
    #[field(type = "String")]
    pub frst: String,
    #[field(type = "String")]
    pub fvst: String,
    #[field(type = "String")]
    pub sxst: String,
    #[field(type = "String")]
    pub svst: String,
    #[field(type = "String")]
    pub eist: String,
    #[field(type = "String")]
    pub nist: String,
    #[field(type = "String")]
    pub test: String,
    #[field(type = "String")]
    pub elst: String,
    #[field(type = "String")]
    pub tvst: String,
    #[field(type = "String")]
    pub ttst: String,
    #[field(type = "String")]
    pub ftst: String,
    #[field(type = "String")]
    pub ffst: String,
}

impl Default for MbboRecord {
    fn default() -> Self {
        Self {
            val: 0,
            nobt: 0,
            zrsv: 0, onsv: 0, twsv: 0, thsv: 0,
            frsv: 0, fvsv: 0, sxsv: 0, svsv: 0,
            eisv: 0, nisv: 0, tesv: 0, elsv: 0,
            tvsv: 0, ttsv: 0, ftsv: 0, ffsv: 0,
            unsv: 0, cosv: 0,
            omsl: 0, dol: String::new(),
            zrvl: 0, onvl: 1, twvl: 2, thvl: 3, frvl: 4, fvvl: 5,
            sxvl: 6, svvl: 7, eivl: 8, nivl: 9, tevl: 10, elvl: 11,
            tvvl: 12, ttvl: 13, ftvl: 14, ffvl: 15,
            zrst: String::new(), onst: String::new(), twst: String::new(),
            thst: String::new(), frst: String::new(), fvst: String::new(),
            sxst: String::new(), svst: String::new(), eist: String::new(),
            nist: String::new(), test: String::new(), elst: String::new(),
            tvst: String::new(), ttst: String::new(), ftst: String::new(),
            ffst: String::new(),
        }
    }
}

impl MbboRecord {
    pub fn new(val: u16) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }
}
