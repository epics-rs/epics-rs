use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Multi-bit binary output record — manual Record impl for raw↔index conversion.
pub struct MbboRecord {
    pub val: u16,
    pub rval: i32,
    pub oraw: i32,
    pub rbv: i32,
    pub orbv: i32,
    pub mask: i32,
    pub shft: i16,
    pub sdef: bool,
    pub nobt: i16,
    pub mlst: u16,
    pub lalm: u16,
    pub ivoa: i16,
    pub ivov: u16,
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
    pub omsl: i16,
    pub dol: String,
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
    /// Set by `convert()` when VAL is an illegal state (`> 15` with a
    /// defined state table). C `mbboRecord.c::convert` raises
    /// `SOFT_ALARM/INVALID` for this case; the alarm is actually
    /// raised in `check_alarms()` since `convert()` has no access to
    /// `CommonFields`.
    soft_alarm: bool,
}

impl Default for MbboRecord {
    fn default() -> Self {
        Self {
            val: 0,
            rval: 0,
            oraw: 0,
            rbv: 0,
            orbv: 0,
            mask: 0,
            shft: 0,
            sdef: false,
            nobt: 0,
            mlst: 0,
            lalm: 0,
            ivoa: 0,
            ivov: 0,
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
            omsl: 0,
            dol: String::new(),
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
            soft_alarm: false,
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

    /// C convert(): VAL -> RVAL with SDEF check and SHFT.
    ///
    /// When the state table is defined and `VAL > 15` the conversion
    /// is illegal — C `mbboRecord.c::convert` raises
    /// `SOFT_ALARM/INVALID` and leaves RVAL stale. We flag it via
    /// `soft_alarm` so `check_alarms()` raises the alarm.
    fn convert(&mut self) {
        self.soft_alarm = false;
        if self.sdef {
            if self.val > 15 {
                self.soft_alarm = true;
                return;
            }
            let rvs = self.raw_values();
            self.rval = rvs[self.val as usize];
        } else {
            self.rval = self.val as i32;
        }
        if self.shft > 0 {
            // C `mbboRecord.c:433-434` does `prec->rval <<= prec->shft`
            // on a 32-bit `epicsUInt32`. A CA-written SHFT >= 32 makes
            // that shift UB in C (it does not crash — the value just
            // becomes 0 / implementation-defined). The Rust `<<`
            // panics in debug builds for the same shift count, so use
            // `checked_shl` and treat an out-of-range shift as a
            // fully-shifted-out 0 — defined, non-panicking, and the
            // value any sane 32-bit shift-out yields.
            self.rval = (self.rval as u32)
                .checked_shl(self.shft as u32)
                .unwrap_or(0) as i32;
        }
    }

    /// Per-state severity fields ZRSV..FFSV indexed by state 0..15.
    fn state_severities(&self) -> [i16; 16] {
        [
            self.zrsv, self.onsv, self.twsv, self.thsv, self.frsv, self.fvsv, self.sxsv, self.svsv,
            self.eisv, self.nisv, self.tesv, self.elsv, self.tvsv, self.ttsv, self.ftsv, self.ffsv,
        ]
    }
}

static MBBO_FIELDS: &[FieldDesc] = &[
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
        name: "RBV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "ORBV",
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
        name: "IVOA",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IVOV",
        dbf_type: DbFieldType::Enum,
        read_only: false,
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
        name: "OMSL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DOL",
        dbf_type: DbFieldType::String,
        read_only: false,
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
    // `String`-variant fields store a Rust `String`; `EpicsValue::String`
    // now wraps `PvString`, so convert with `.into()` (lossy text record).
    (@get $self:expr, $field:ident, String) => {
        Some(EpicsValue::String($self.$field.clone().into()))
    };
    (@get $self:expr, $field:ident, $variant:ident) => {
        Some(EpicsValue::$variant($self.$field.clone()))
    };
    ($self:expr, $name:expr, $( $str:literal => $field:ident : $variant:ident ),* $(,)?) => {
        match $name {
            "VAL" => Some(EpicsValue::Enum($self.val)),
            $( $str => mbb_get_field!(@get $self, $field, $variant), )*
            _ => None,
        }
    };
}

macro_rules! mbb_put_field {
    // `String`-variant fields store a Rust `String`; the extracted payload
    // is a `PvString`, so convert with `.as_str_lossy().into_owned()`.
    (@put $self:expr, $field:ident, String, $v:expr) => {
        $self.$field = $v.as_str_lossy().into_owned();
    };
    (@put $self:expr, $field:ident, $variant:ident, $v:expr) => {
        $self.$field = $v;
    };
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
                    mbb_put_field!(@put $self, $field, $variant, v);
                } else {
                    return Err(CaError::TypeMismatch($str.into()));
                }
            } )*
            _ => return Err(CaError::FieldNotFound($name.to_string())),
        }
    };
}

impl Record for MbboRecord {
    fn record_type(&self) -> &'static str {
        "mbbo"
    }
    fn field_list(&self) -> &'static [FieldDesc] {
        MBBO_FIELDS
    }

    // C recMbbo.c IVOA=set_to_IVOV: val = ivov; rval = ivov.
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        let rval = match &ivov {
            EpicsValue::Enum(e) => EpicsValue::Long(*e as i32),
            EpicsValue::Short(s) => EpicsValue::Long(*s as i32),
            other => other.clone(),
        };
        self.put_field("RVAL", rval)?;
        self.put_field("VAL", ivov)
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
            self.convert();
            self.mlst = self.val;
            self.lalm = self.val;
            self.oraw = self.rval;
            self.orbv = self.rbv;
        }
        Ok(())
    }

    /// C `mbboRecord.c::process` (line 217) calls `convert(prec)`
    /// UNCONDITIONALLY on every non-pact process — the VAL→RVAL output
    /// translation. It is never gated on whether VAL was just written
    /// (CA put, DOL fetch, or device support). A CA put to `mbbo.VAL`
    /// must therefore recompute `RVAL`/`ORAW`/`ORBV`. `mbbo` is an
    /// output record, so it does NOT override `set_device_did_compute`
    /// or `soft_channel_skips_convert` — the soft-channel convert-skip
    /// applies only to INPUT records (ai/bi/mbbi), where VAL is the
    /// engineering value.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.convert();
        self.oraw = self.rval;
        self.orbv = self.rbv;
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        mbb_get_field!(self, name,
            "RVAL" => rval: Long, "ORAW" => oraw: Long,
            "RBV" => rbv: Long, "ORBV" => orbv: Long,
            "MASK" => mask: Long, "SHFT" => shft: Short,
            "MLST" => mlst: Enum, "LALM" => lalm: Enum,
            "IVOA" => ivoa: Short, "IVOV" => ivov: Enum,
            "NOBT" => nobt: Short,
            "ZRSV" => zrsv: Short, "ONSV" => onsv: Short, "TWSV" => twsv: Short, "THSV" => thsv: Short,
            "FRSV" => frsv: Short, "FVSV" => fvsv: Short, "SXSV" => sxsv: Short, "SVSV" => svsv: Short,
            "EISV" => eisv: Short, "NISV" => nisv: Short, "TESV" => tesv: Short, "ELSV" => elsv: Short,
            "TVSV" => tvsv: Short, "TTSV" => ttsv: Short, "FTSV" => ftsv: Short, "FFSV" => ffsv: Short,
            "UNSV" => unsv: Short, "COSV" => cosv: Short,
            "OMSL" => omsl: Short, "DOL" => dol: String,
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
            "RVAL" => rval: Long, "ORAW" => oraw: Long,
            "RBV" => rbv: Long, "ORBV" => orbv: Long,
            "MASK" => mask: Long, "SHFT" => shft: Short,
            "MLST" => mlst: Enum, "LALM" => lalm: Enum,
            "IVOA" => ivoa: Short, "IVOV" => ivov: Enum,
            "NOBT" => nobt: Short,
            "ZRSV" => zrsv: Short, "ONSV" => onsv: Short, "TWSV" => twsv: Short, "THSV" => thsv: Short,
            "FRSV" => frsv: Short, "FVSV" => fvsv: Short, "SXSV" => sxsv: Short, "SVSV" => svsv: Short,
            "EISV" => eisv: Short, "NISV" => nisv: Short, "TESV" => tesv: Short, "ELSV" => elsv: Short,
            "TVSV" => tvsv: Short, "TTSV" => ttsv: Short, "FTSV" => ftsv: Short, "FFSV" => ffsv: Short,
            "UNSV" => unsv: Short, "COSV" => cosv: Short,
            "OMSL" => omsl: Short, "DOL" => dol: String,
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

    fn can_device_write(&self) -> bool {
        true
    }

    /// C `mbboRecord.c::checkAlarms` + the `SOFT_ALARM` raised by
    /// `convert()` — STATE alarm from the per-state severity
    /// (ZRSV..FFSV, or UNSV for an unknown state), COS alarm (COSV),
    /// and `SOFT_ALARM/INVALID` when `VAL > 15` with a defined state
    /// table. The framework's `rec_gbl_check_udf` raises UDF.
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        use crate::server::recgbl::{self, alarm_status};
        use crate::server::record::AlarmSeverity;

        // SOFT_ALARM for an illegal VAL — C convert() path.
        if self.soft_alarm {
            recgbl::rec_gbl_set_sevr(common, alarm_status::SOFT_ALARM, AlarmSeverity::Invalid);
        }

        let val = self.val;
        let raw_sev = if val > 15 {
            self.unsv
        } else {
            self.state_severities()[val as usize]
        };
        let sev = AlarmSeverity::from_u16(raw_sev as u16);
        if sev != AlarmSeverity::NoAlarm {
            recgbl::rec_gbl_set_sevr(common, alarm_status::STATE_ALARM, sev);
        }
        if val != self.lalm {
            let cos_sev = AlarmSeverity::from_u16(self.cosv as u16);
            if cos_sev != AlarmSeverity::NoAlarm {
                recgbl::rec_gbl_set_sevr(common, alarm_status::COS_ALARM, cos_sev);
            }
            self.lalm = val;
        }
    }

    /// C `mbboRecord.c::special` — recompute `sdef` after any runtime
    /// write to a state value (ZRVL..FFVL) or state string
    /// (ZRST..FFST) field. See `mbbi::is_state_table_field`.
    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if after && super::mbbi::is_state_table_field(field) {
            self.compute_sdef();
        }
        Ok(())
    }
}
