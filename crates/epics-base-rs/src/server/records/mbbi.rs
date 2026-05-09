use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Multi-bit binary input record — manual Record impl for raw↔index conversion.
pub struct MbbiRecord {
    pub val: u16,
    pub rval: i32,
    pub oraw: i32,
    pub mask: i32,
    pub shft: i16,
    pub sdef: bool,
    pub nobt: i16,
    pub mlst: u16,
    pub lalm: u16,
    pub zrsv: i16,
    pub onsv: i16,
    pub twsv: i16,
    pub thsv: i16,
    pub frsv: i16,
    pub fvsv: i16,
    pub sxsv: i16,
    pub svsv: i16,
    pub eisv: i16,
    pub nisv: i16,
    pub tesv: i16,
    pub elsv: i16,
    pub tvsv: i16,
    pub ttsv: i16,
    pub ftsv: i16,
    pub ffsv: i16,
    pub unsv: i16,
    pub cosv: i16,
    /// Alarm filter time constant (seconds). 0 = disabled.
    /// See `BiRecord::aftc` — same low-pass filter on alarm severity.
    pub aftc: f64,
    /// Alarm filter accumulator. 0 = initial sample.
    pub afvl: f64,
    pub zrvl: i32,
    pub onvl: i32,
    pub twvl: i32,
    pub thvl: i32,
    pub frvl: i32,
    pub fvvl: i32,
    pub sxvl: i32,
    pub svvl: i32,
    pub eivl: i32,
    pub nivl: i32,
    pub tevl: i32,
    pub elvl: i32,
    pub tvvl: i32,
    pub ttvl: i32,
    pub ftvl: i32,
    pub ffvl: i32,
    pub zrst: String,
    pub onst: String,
    pub twst: String,
    pub thst: String,
    pub frst: String,
    pub fvst: String,
    pub sxst: String,
    pub svst: String,
    pub eist: String,
    pub nist: String,
    pub test: String,
    pub elst: String,
    pub tvst: String,
    pub ttst: String,
    pub ftst: String,
    pub ffst: String,
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    skip_convert: bool,
}

impl Default for MbbiRecord {
    fn default() -> Self {
        Self {
            val: 0,
            rval: 0,
            oraw: 0,
            mask: 0,
            shft: 0,
            sdef: false,
            nobt: 0,
            mlst: 0,
            lalm: 0,
            zrsv: 0,
            onsv: 0,
            twsv: 0,
            thsv: 0,
            frsv: 0,
            fvsv: 0,
            sxsv: 0,
            svsv: 0,
            eisv: 0,
            nisv: 0,
            tesv: 0,
            elsv: 0,
            tvsv: 0,
            ttsv: 0,
            ftsv: 0,
            ffsv: 0,
            unsv: 0,
            cosv: 0,
            aftc: 0.0,
            afvl: 0.0,
            zrvl: 0,
            onvl: 0,
            twvl: 0,
            thvl: 0,
            frvl: 0,
            fvvl: 0,
            sxvl: 0,
            svvl: 0,
            eivl: 0,
            nivl: 0,
            tevl: 0,
            elvl: 0,
            tvvl: 0,
            ttvl: 0,
            ftvl: 0,
            ffvl: 0,
            zrst: String::new(),
            onst: String::new(),
            twst: String::new(),
            thst: String::new(),
            frst: String::new(),
            fvst: String::new(),
            sxst: String::new(),
            svst: String::new(),
            eist: String::new(),
            nist: String::new(),
            test: String::new(),
            elst: String::new(),
            tvst: String::new(),
            ttst: String::new(),
            ftst: String::new(),
            ffst: String::new(),
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            skip_convert: false,
        }
    }
}

impl MbbiRecord {
    pub fn new(val: u16) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }

    fn raw_values(&self) -> [i32; 16] {
        [
            self.zrvl, self.onvl, self.twvl, self.thvl, self.frvl, self.fvvl, self.sxvl, self.svvl,
            self.eivl, self.nivl, self.tevl, self.elvl, self.tvvl, self.ttvl, self.ftvl, self.ffvl,
        ]
    }

    fn compute_sdef(&mut self) {
        let rvs = self.raw_values();
        let sts: [&String; 16] = [
            &self.zrst, &self.onst, &self.twst, &self.thst, &self.frst, &self.fvst, &self.sxst,
            &self.svst, &self.eist, &self.nist, &self.test, &self.elst, &self.tvst, &self.ttst,
            &self.ftst, &self.ffst,
        ];
        self.sdef = false;
        for i in 0..16 {
            if rvs[i] != 0 || !sts[i].is_empty() {
                self.sdef = true;
                return;
            }
        }
    }

    fn raw_to_val(&self, raw: i32) -> u16 {
        if !self.sdef {
            return raw as u16;
        }
        let rvs = self.raw_values();
        for (i, &rv) in rvs.iter().enumerate() {
            if rv == raw {
                return i as u16;
            }
        }
        65535
    }
}

static MBBI_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Enum,
        read_only: false,
    },
    FieldDesc {
        name: "RVAL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "ORAW",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "MASK",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "SHFT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "MLST",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "LALM",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "NOBT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "ZRSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "ONSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TWSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "THSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "FRSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "FVSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SXSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SVSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "EISV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "NISV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TESV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "ELSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TVSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TTSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "FTSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "FFSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "UNSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "COSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "AFTC",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "AFVL",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "ZRVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "ONVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "TWVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "THVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "FRVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "FVVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "SXVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "SVVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "EIVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "NIVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "TEVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "ELVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "TVVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "TTVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "FTVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "FFVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "ZRST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "ONST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "TWST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "THST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "FRST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "FVST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "SXST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "SVST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "EIST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "NIST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "TEST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "ELST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "TVST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "TTST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "FTST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "FFST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
];

/// Helper macro: maps EPICS field name strings to struct fields.
macro_rules! mbb_get_field {
    ($self:expr, $name:expr, $( $str:literal => $field:ident : $variant:ident ),* $(,)?) => {
        match $name {
            "VAL" => Some(EpicsValue::Enum($self.val)),
            $( $str => Some(EpicsValue::$variant($self.$field.clone())), )*
            _ => None,
        }
    };
}

macro_rules! mbb_put_field {
    ($self:expr, $name:expr, $value:expr, $( $str:literal => $field:ident : $variant:ident ),* $(,)?) => {
        match $name {
            "VAL" => {
                match $value {
                    EpicsValue::Enum(v) => { $self.val = v; }
                    EpicsValue::Long(v) => { $self.val = v as u16; }
                    EpicsValue::Short(v) => { $self.val = v as u16; }
                    _ => return Err(CaError::TypeMismatch("VAL".into())),
                }
            }
            $( $str => {
                if let EpicsValue::$variant(v) = $value {
                    $self.$field = v;
                } else {
                    return Err(CaError::TypeMismatch($str.into()));
                }
            } )*
            _ => return Err(CaError::FieldNotFound($name.to_string())),
        }
    };
}

impl Record for MbbiRecord {
    fn record_type(&self) -> &'static str {
        "mbbi"
    }
    fn field_list(&self) -> &'static [FieldDesc] {
        MBBI_FIELDS
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            if self.mask == 0 && self.nobt > 0 && self.nobt <= 32 {
                self.mask = ((1i64 << self.nobt) - 1) as i32;
            }
            self.compute_sdef();
            self.mlst = self.val;
            self.lalm = self.val;
            self.oraw = self.rval;
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if !self.skip_convert {
            let mut rval = self.rval;
            if self.shft > 0 {
                rval = ((rval as u32) >> (self.shft as u32)) as i32;
            }
            self.val = self.raw_to_val(rval);
        }
        self.skip_convert = false;
        self.oraw = self.rval;
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        mbb_get_field!(self, name,
            "RVAL" => rval: Long, "ORAW" => oraw: Long, "MASK" => mask: Long,
            "SHFT" => shft: Short, "MLST" => mlst: Enum, "LALM" => lalm: Enum,
            "NOBT" => nobt: Short,
            "ZRSV" => zrsv: Short, "ONSV" => onsv: Short, "TWSV" => twsv: Short, "THSV" => thsv: Short,
            "FRSV" => frsv: Short, "FVSV" => fvsv: Short, "SXSV" => sxsv: Short, "SVSV" => svsv: Short,
            "EISV" => eisv: Short, "NISV" => nisv: Short, "TESV" => tesv: Short, "ELSV" => elsv: Short,
            "TVSV" => tvsv: Short, "TTSV" => ttsv: Short, "FTSV" => ftsv: Short, "FFSV" => ffsv: Short,
            "UNSV" => unsv: Short, "COSV" => cosv: Short,
            "AFTC" => aftc: Double, "AFVL" => afvl: Double,
            "ZRVL" => zrvl: Long, "ONVL" => onvl: Long, "TWVL" => twvl: Long, "THVL" => thvl: Long,
            "FRVL" => frvl: Long, "FVVL" => fvvl: Long, "SXVL" => sxvl: Long, "SVVL" => svvl: Long,
            "EIVL" => eivl: Long, "NIVL" => nivl: Long, "TEVL" => tevl: Long, "ELVL" => elvl: Long,
            "TVVL" => tvvl: Long, "TTVL" => ttvl: Long, "FTVL" => ftvl: Long, "FFVL" => ffvl: Long,
            "ZRST" => zrst: String, "ONST" => onst: String, "TWST" => twst: String, "THST" => thst: String,
            "FRST" => frst: String, "FVST" => fvst: String, "SXST" => sxst: String, "SVST" => svst: String,
            "EIST" => eist: String, "NIST" => nist: String, "TEST" => test: String, "ELST" => elst: String,
            "TVST" => tvst: String, "TTST" => ttst: String, "FTST" => ftst: String, "FFST" => ffst: String,
        )
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        mbb_put_field!(self, name, value,
            "RVAL" => rval: Long, "ORAW" => oraw: Long, "MASK" => mask: Long,
            "SHFT" => shft: Short, "MLST" => mlst: Enum, "LALM" => lalm: Enum,
            "NOBT" => nobt: Short,
            "ZRSV" => zrsv: Short, "ONSV" => onsv: Short, "TWSV" => twsv: Short, "THSV" => thsv: Short,
            "FRSV" => frsv: Short, "FVSV" => fvsv: Short, "SXSV" => sxsv: Short, "SVSV" => svsv: Short,
            "EISV" => eisv: Short, "NISV" => nisv: Short, "TESV" => tesv: Short, "ELSV" => elsv: Short,
            "TVSV" => tvsv: Short, "TTSV" => ttsv: Short, "FTSV" => ftsv: Short, "FFSV" => ffsv: Short,
            "UNSV" => unsv: Short, "COSV" => cosv: Short,
            "AFTC" => aftc: Double, "AFVL" => afvl: Double,
            "ZRVL" => zrvl: Long, "ONVL" => onvl: Long, "TWVL" => twvl: Long, "THVL" => thvl: Long,
            "FRVL" => frvl: Long, "FVVL" => fvvl: Long, "SXVL" => sxvl: Long, "SVVL" => svvl: Long,
            "EIVL" => eivl: Long, "NIVL" => nivl: Long, "TEVL" => tevl: Long, "ELVL" => elvl: Long,
            "TVVL" => tvvl: Long, "TTVL" => ttvl: Long, "FTVL" => ftvl: Long, "FFVL" => ffvl: Long,
            "ZRST" => zrst: String, "ONST" => onst: String, "TWST" => twst: String, "THST" => thst: String,
            "FRST" => frst: String, "FVST" => fvst: String, "SXST" => sxst: String, "SVST" => svst: String,
            "EIST" => eist: String, "NIST" => nist: String, "TEST" => test: String, "ELST" => elst: String,
            "TVST" => tvst: String, "TTST" => ttst: String, "FTST" => ftst: String, "FFST" => ffst: String,
        );
        Ok(())
    }

    fn set_device_did_compute(&mut self, did: bool) {
        self.skip_convert = did;
    }

    /// Override set_val: convert raw value from hardware → enum index.
    fn set_val(&mut self, value: EpicsValue) -> CaResult<()> {
        let raw = match value {
            EpicsValue::Long(v) => v,
            EpicsValue::Short(v) => v as i32,
            EpicsValue::Enum(v) => {
                // Already an index — store directly
                self.val = v;
                return Ok(());
            }
            _ => return Err(CaError::TypeMismatch("VAL".into())),
        };
        self.rval = raw;
        let shifted = if self.shft > 0 {
            ((raw as u32) >> (self.shft as u32)) as i32
        } else {
            raw
        };
        self.val = self.raw_to_val(shifted);
        Ok(())
    }
}
