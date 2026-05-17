use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Number of input signals A..L (C `selRecord.c::SEL_MAX`).
const SEL_MAX: usize = 12;

/// Sel (select) record — selects one of A-L based on SELM algorithm.
pub struct SelRecord {
    pub val: f64,
    pub selm: i16, // 0=Specified, 1=High, 2=Low, 3=Median
    /// Selection number. C `selRecord.h` declares SELN as `DBF_USHORT`
    /// so it is always ≥ 0; the Rust field is `i16` but is treated as
    /// an unsigned 0..65535 index — `do_sel` rejects values ≥ SEL_MAX.
    pub seln: i16,
    pub nvl: String, // NVL link (for SELN)
    // Input links
    pub inpa: String,
    pub inpb: String,
    pub inpc: String,
    pub inpd: String,
    pub inpe: String,
    pub inpf: String,
    pub inpg: String,
    pub inph: String,
    pub inpi: String,
    pub inpj: String,
    pub inpk: String,
    pub inpl: String,
    // Input values
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
    pub g: f64,
    pub h: f64,
    pub i: f64,
    pub j: f64,
    pub k: f64,
    pub l: f64,
    // Limit-alarm configuration (C `selRecord.c::checkAlarms`).
    pub hihi: f64,
    pub high: f64,
    pub low: f64,
    pub lolo: f64,
    pub hhsv: i16, // HIHI severity
    pub hsv: i16,  // HIGH severity
    pub lsv: i16,  // LOW severity
    pub llsv: i16, // LOLO severity
    pub hyst: f64, // alarm deadband
    pub lalm: f64, // last alarmed value (C `prec->lalm`)
    /// True when `do_sel` detected an out-of-range SELN in Specified
    /// mode — raises `SOFT_ALARM/INVALID` in `check_alarms`.
    soft_alarm: bool,
}

impl Default for SelRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            selm: 0,
            seln: 0,
            nvl: String::new(),
            inpa: String::new(),
            inpb: String::new(),
            inpc: String::new(),
            inpd: String::new(),
            inpe: String::new(),
            inpf: String::new(),
            inpg: String::new(),
            inph: String::new(),
            inpi: String::new(),
            inpj: String::new(),
            inpk: String::new(),
            inpl: String::new(),
            // C initializes inputs to epicsNAN; NaN values are skipped by algorithms
            a: f64::NAN,
            b: f64::NAN,
            c: f64::NAN,
            d: f64::NAN,
            e: f64::NAN,
            f: f64::NAN,
            g: f64::NAN,
            h: f64::NAN,
            i: f64::NAN,
            j: f64::NAN,
            k: f64::NAN,
            l: f64::NAN,
            hihi: 0.0,
            high: 0.0,
            low: 0.0,
            lolo: 0.0,
            hhsv: 0,
            hsv: 0,
            lsv: 0,
            llsv: 0,
            hyst: 0.0,
            lalm: 0.0,
            soft_alarm: false,
        }
    }
}

impl SelRecord {
    fn get_values(&self) -> [f64; SEL_MAX] {
        [
            self.a, self.b, self.c, self.d, self.e, self.f, self.g, self.h, self.i, self.j, self.k,
            self.l,
        ]
    }
}

static SEL_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "SELM",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SELN",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "NVL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPA",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPB",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPC",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPD",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPE",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPF",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPG",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPH",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPI",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPJ",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPK",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "A",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "B",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "C",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "D",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "E",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "F",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "G",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "H",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "I",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "J",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "K",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "L",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "HIHI",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "HIGH",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOW",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOLO",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "HHSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "HSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "LSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "LLSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "HYST",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LALM",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
];

impl Record for SelRecord {
    fn record_type(&self) -> &'static str {
        "sel"
    }

    /// C `selRecord.c::do_sel` — pick VAL per SELM, updating SELN for
    /// the High/Low/Median algorithms and setting VAL=NaN (→ UDF) when
    /// no valid input exists or the selected input is undefined.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.soft_alarm = false;
        let vals = self.get_values();
        match self.selm {
            0 => {
                // Specified: index by SELN. C `do_sel` raises
                // SOFT_ALARM/INVALID and returns (VAL unchanged) when
                // SELN >= SEL_MAX. SELN is unsigned in C; treat a
                // negative i16 as a huge unsigned value → out of range.
                let idx = self.seln as u16 as usize;
                if idx >= SEL_MAX {
                    self.soft_alarm = true;
                } else {
                    // C: `prec->val = val;` then `udf = isnan(val)`.
                    self.val = vals[idx];
                }
            }
            1 => {
                // High Signal: max of non-NaN values; SELN = winning index.
                let mut val = f64::NEG_INFINITY;
                for (i, &v) in vals.iter().enumerate() {
                    if !v.is_nan() && val < v {
                        val = v;
                        self.seln = i as i16;
                    }
                }
                self.val = val;
            }
            2 => {
                // Low Signal: min of non-NaN values; SELN = winning index.
                let mut val = f64::INFINITY;
                for (i, &v) in vals.iter().enumerate() {
                    if !v.is_nan() && val > v {
                        val = v;
                        self.seln = i as i16;
                    }
                }
                self.val = val;
            }
            3 => {
                // Median: C inserts non-NaN values into `order`, sets
                // SELN = count, and picks `order[count/2]`. With zero
                // valid inputs `order[0]` is epicsNAN → VAL = NaN.
                let mut order: Vec<f64> = Vec::with_capacity(SEL_MAX);
                for &v in vals.iter() {
                    if !v.is_nan() {
                        order.push(v);
                    }
                }
                let count = order.len();
                self.seln = count as i16;
                order.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                self.val = if count == 0 {
                    f64::NAN
                } else {
                    order[count / 2]
                };
            }
            _ => {
                // C `do_sel` default: CALC_ALARM/INVALID, VAL unchanged.
                self.soft_alarm = true;
            }
        }
        Ok(ProcessOutcome::complete())
    }

    /// C `selRecord.c::do_sel` sets `prec->udf = isnan(prec->val)` — a
    /// selected NaN propagates into VAL and must keep UDF set so the
    /// framework raises UDF_ALARM.
    fn value_is_undefined(&self) -> bool {
        self.val.is_nan()
    }

    /// C `selRecord.c::checkAlarms` — UDF alarm plus the analog limit
    /// alarms (HIHI/HIGH/LOW/LOLO) with per-level hysteresis, and the
    /// SOFT/CALC alarm raised by `do_sel` for a bad SELM/SELN.
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        use crate::server::recgbl::{self, alarm_status};
        use crate::server::record::AlarmSeverity;

        // SOFT_ALARM / CALC_ALARM raised by do_sel — C raises this and
        // returns from do_sel; checkAlarms still runs but with UDF
        // possibly set. Raise it unconditionally here.
        if self.soft_alarm {
            recgbl::rec_gbl_set_sevr(common, alarm_status::SOFT_ALARM, AlarmSeverity::Invalid);
        }

        // C checkAlarms: UDF first; if undefined, raise UDF and return.
        if common.udf {
            recgbl::rec_gbl_set_sevr(common, alarm_status::UDF_ALARM, common.udfs);
            return;
        }

        let val = self.val;
        let hyst = self.hyst;
        let lalm = self.lalm;

        // C `recGblSetSevr` returns true when it actually raised the
        // severity (new severity strictly greater than the pending
        // one). The Rust helper returns `()`, so mirror its gate here.
        let raised = |common: &crate::server::record::CommonFields, sev: AlarmSeverity| {
            (sev as u16) > (common.nsev as u16)
        };

        // hihi
        let hhsv = AlarmSeverity::from_u16(self.hhsv as u16);
        if hhsv != AlarmSeverity::NoAlarm
            && (val >= self.hihi || (lalm == self.hihi && val >= self.hihi - hyst))
        {
            if raised(common, hhsv) {
                self.lalm = self.hihi;
            }
            recgbl::rec_gbl_set_sevr(common, alarm_status::HIHI_ALARM, hhsv);
            return;
        }
        // lolo
        let llsv = AlarmSeverity::from_u16(self.llsv as u16);
        if llsv != AlarmSeverity::NoAlarm
            && (val <= self.lolo || (lalm == self.lolo && val <= self.lolo + hyst))
        {
            if raised(common, llsv) {
                self.lalm = self.lolo;
            }
            recgbl::rec_gbl_set_sevr(common, alarm_status::LOLO_ALARM, llsv);
            return;
        }
        // high
        let hsv = AlarmSeverity::from_u16(self.hsv as u16);
        if hsv != AlarmSeverity::NoAlarm
            && (val >= self.high || (lalm == self.high && val >= self.high - hyst))
        {
            if raised(common, hsv) {
                self.lalm = self.high;
            }
            recgbl::rec_gbl_set_sevr(common, alarm_status::HIGH_ALARM, hsv);
            return;
        }
        // low
        let lsv = AlarmSeverity::from_u16(self.lsv as u16);
        if lsv != AlarmSeverity::NoAlarm
            && (val <= self.low || (lalm == self.low && val <= self.low + hyst))
        {
            if raised(common, lsv) {
                self.lalm = self.low;
            }
            recgbl::rec_gbl_set_sevr(common, alarm_status::LOW_ALARM, lsv);
            return;
        }
        // out of alarm by at least hyst
        self.lalm = val;
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "SELM" => Some(EpicsValue::Short(self.selm)),
            "SELN" => Some(EpicsValue::Short(self.seln)),
            "NVL" => Some(EpicsValue::String(self.nvl.clone())),
            "INPA" => Some(EpicsValue::String(self.inpa.clone())),
            "INPB" => Some(EpicsValue::String(self.inpb.clone())),
            "INPC" => Some(EpicsValue::String(self.inpc.clone())),
            "INPD" => Some(EpicsValue::String(self.inpd.clone())),
            "INPE" => Some(EpicsValue::String(self.inpe.clone())),
            "INPF" => Some(EpicsValue::String(self.inpf.clone())),
            "INPG" => Some(EpicsValue::String(self.inpg.clone())),
            "INPH" => Some(EpicsValue::String(self.inph.clone())),
            "INPI" => Some(EpicsValue::String(self.inpi.clone())),
            "INPJ" => Some(EpicsValue::String(self.inpj.clone())),
            "INPK" => Some(EpicsValue::String(self.inpk.clone())),
            "INPL" => Some(EpicsValue::String(self.inpl.clone())),
            "A" => Some(EpicsValue::Double(self.a)),
            "B" => Some(EpicsValue::Double(self.b)),
            "C" => Some(EpicsValue::Double(self.c)),
            "D" => Some(EpicsValue::Double(self.d)),
            "E" => Some(EpicsValue::Double(self.e)),
            "F" => Some(EpicsValue::Double(self.f)),
            "G" => Some(EpicsValue::Double(self.g)),
            "H" => Some(EpicsValue::Double(self.h)),
            "I" => Some(EpicsValue::Double(self.i)),
            "J" => Some(EpicsValue::Double(self.j)),
            "K" => Some(EpicsValue::Double(self.k)),
            "L" => Some(EpicsValue::Double(self.l)),
            "HIHI" => Some(EpicsValue::Double(self.hihi)),
            "HIGH" => Some(EpicsValue::Double(self.high)),
            "LOW" => Some(EpicsValue::Double(self.low)),
            "LOLO" => Some(EpicsValue::Double(self.lolo)),
            "HHSV" => Some(EpicsValue::Short(self.hhsv)),
            "HSV" => Some(EpicsValue::Short(self.hsv)),
            "LSV" => Some(EpicsValue::Short(self.lsv)),
            "LLSV" => Some(EpicsValue::Short(self.llsv)),
            "HYST" => Some(EpicsValue::Double(self.hyst)),
            "LALM" => Some(EpicsValue::Double(self.lalm)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::Double(v) => {
                    self.val = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            "SELM" => match value {
                EpicsValue::Short(v) => {
                    self.selm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SELM".into())),
            },
            "SELN" => match value {
                EpicsValue::Short(v) => {
                    self.seln = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SELN".into())),
            },
            "NVL" => match value {
                EpicsValue::String(s) => {
                    self.nvl = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("NVL".into())),
            },
            "INPA" | "INPB" | "INPC" | "INPD" | "INPE" | "INPF" | "INPG" | "INPH" | "INPI"
            | "INPJ" | "INPK" | "INPL" => match value {
                EpicsValue::String(s) => {
                    match name {
                        "INPA" => self.inpa = s,
                        "INPB" => self.inpb = s,
                        "INPC" => self.inpc = s,
                        "INPD" => self.inpd = s,
                        "INPE" => self.inpe = s,
                        "INPF" => self.inpf = s,
                        "INPG" => self.inpg = s,
                        "INPH" => self.inph = s,
                        "INPI" => self.inpi = s,
                        "INPJ" => self.inpj = s,
                        "INPK" => self.inpk = s,
                        "INPL" => self.inpl = s,
                        _ => unreachable!(),
                    }
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "A" | "B" | "C" | "D" | "E" | "F" | "G" | "H" | "I" | "J" | "K" | "L" => {
                let v = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                match name {
                    "A" => self.a = v,
                    "B" => self.b = v,
                    "C" => self.c = v,
                    "D" => self.d = v,
                    "E" => self.e = v,
                    "F" => self.f = v,
                    "G" => self.g = v,
                    "H" => self.h = v,
                    "I" => self.i = v,
                    "J" => self.j = v,
                    "K" => self.k = v,
                    "L" => self.l = v,
                    _ => unreachable!(),
                }
                Ok(())
            }
            "HIHI" | "HIGH" | "LOW" | "LOLO" | "HYST" | "LALM" => {
                let v = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                match name {
                    "HIHI" => self.hihi = v,
                    "HIGH" => self.high = v,
                    "LOW" => self.low = v,
                    "LOLO" => self.lolo = v,
                    "HYST" => self.hyst = v,
                    "LALM" => self.lalm = v,
                    _ => unreachable!(),
                }
                Ok(())
            }
            "HHSV" | "HSV" | "LSV" | "LLSV" => match value {
                EpicsValue::Short(v) => {
                    match name {
                        "HHSV" => self.hhsv = v,
                        "HSV" => self.hsv = v,
                        "LSV" => self.lsv = v,
                        "LLSV" => self.llsv = v,
                        _ => unreachable!(),
                    }
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        SEL_FIELDS
    }

    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        &[
            ("INPA", "A"),
            ("INPB", "B"),
            ("INPC", "C"),
            ("INPD", "D"),
            ("INPE", "E"),
            ("INPF", "F"),
            ("INPG", "G"),
            ("INPH", "H"),
            ("INPI", "I"),
            ("INPJ", "J"),
            ("INPK", "K"),
            ("INPL", "L"),
        ]
    }
}
