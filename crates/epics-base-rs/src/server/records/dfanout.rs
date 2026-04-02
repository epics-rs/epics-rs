use epics_macros_rs::EpicsRecord;

#[derive(EpicsRecord)]
#[record(type = "dfanout")]
pub struct DfanoutRecord {
    #[field(type = "Double")]
    pub val: f64,
    #[field(type = "Short")]
    pub selm: i16,
    #[field(type = "Short")]
    pub seln: i16,
    #[field(type = "String")]
    pub outa: String,
    #[field(type = "String")]
    pub outb: String,
    #[field(type = "String")]
    pub outc: String,
    #[field(type = "String")]
    pub outd: String,
    #[field(type = "String")]
    pub oute: String,
    #[field(type = "String")]
    pub outf: String,
    #[field(type = "String")]
    pub outg: String,
    #[field(type = "String")]
    pub outh: String,
}

impl Default for DfanoutRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            selm: 0,
            seln: 0,
            outa: String::new(),
            outb: String::new(),
            outc: String::new(),
            outd: String::new(),
            oute: String::new(),
            outf: String::new(),
            outg: String::new(),
            outh: String::new(),
        }
    }
}

impl DfanoutRecord {
    pub fn new(val: f64) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }

    /// Get all non-empty output link targets.
    pub fn output_links(&self) -> Vec<&str> {
        [
            &self.outa, &self.outb, &self.outc, &self.outd, &self.oute, &self.outf, &self.outg,
            &self.outh,
        ]
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.as_str())
        .collect()
    }
}
