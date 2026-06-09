use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Data fanout record — distributes VAL to up to 16 output links
/// (OUTA..OUTP) selected by SELM/SELN. Manual `Record` impl so the
/// analog limit alarms, MDEL/ADEL deadband and UDF check from C
/// `dfanoutRecord.c` are available (the derive macro provided none).
pub struct DfanoutRecord {
    pub val: f64,
    pub selm: i16,
    /// Link selection. C declares SELN as `DBF_USHORT`
    /// (dfanoutRecord.dbd.pod:188): unsigned 0..65535, used as a bit
    /// mask in `SELM=Mask`.
    pub seln: u16,
    pub outa: String,
    pub outb: String,
    pub outc: String,
    pub outd: String,
    pub oute: String,
    pub outf: String,
    pub outg: String,
    pub outh: String,
    pub outi: String,
    pub outj: String,
    pub outk: String,
    pub outl: String,
    pub outm: String,
    pub outn: String,
    pub outo: String,
    pub outp: String,
    pub dol: String,
    pub omsl: i16,
    pub sell: String,
    pub ivoa: i16,
    pub ivov: f64,
    // Limit-alarm configuration (C `dfanoutRecord.c::checkAlarms`).
    pub hihi: f64,
    pub high: f64,
    pub low: f64,
    pub lolo: f64,
    pub hhsv: i16,
    pub hsv: i16,
    pub lsv: i16,
    pub llsv: i16,
    pub hyst: f64,
    pub lalm: f64,
    // Monitor deadband state.
    pub mdel: f64,
    pub adel: f64,
    pub mlst: f64,
    pub alst: f64,
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
            outi: String::new(),
            outj: String::new(),
            outk: String::new(),
            outl: String::new(),
            outm: String::new(),
            outn: String::new(),
            outo: String::new(),
            outp: String::new(),
            dol: String::new(),
            omsl: 0,
            sell: String::new(),
            ivoa: 0,
            ivov: 0.0,
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
            mdel: 0.0,
            adel: 0.0,
            mlst: f64::NAN,
            alst: f64::NAN,
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
            &self.outh, &self.outi, &self.outj, &self.outk, &self.outl, &self.outm, &self.outn,
            &self.outo, &self.outp,
        ]
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.as_str())
        .collect()
    }

    fn out_slot_mut(&mut self, name: &str) -> Option<&mut String> {
        Some(match name {
            "OUTA" => &mut self.outa,
            "OUTB" => &mut self.outb,
            "OUTC" => &mut self.outc,
            "OUTD" => &mut self.outd,
            "OUTE" => &mut self.oute,
            "OUTF" => &mut self.outf,
            "OUTG" => &mut self.outg,
            "OUTH" => &mut self.outh,
            "OUTI" => &mut self.outi,
            "OUTJ" => &mut self.outj,
            "OUTK" => &mut self.outk,
            "OUTL" => &mut self.outl,
            "OUTM" => &mut self.outm,
            "OUTN" => &mut self.outn,
            "OUTO" => &mut self.outo,
            "OUTP" => &mut self.outp,
            _ => return None,
        })
    }

    fn out_slot(&self, name: &str) -> Option<&str> {
        Some(match name {
            "OUTA" => &self.outa,
            "OUTB" => &self.outb,
            "OUTC" => &self.outc,
            "OUTD" => &self.outd,
            "OUTE" => &self.oute,
            "OUTF" => &self.outf,
            "OUTG" => &self.outg,
            "OUTH" => &self.outh,
            "OUTI" => &self.outi,
            "OUTJ" => &self.outj,
            "OUTK" => &self.outk,
            "OUTL" => &self.outl,
            "OUTM" => &self.outm,
            "OUTN" => &self.outn,
            "OUTO" => &self.outo,
            "OUTP" => &self.outp,
            _ => return None,
        })
    }
}

static DFANOUT_FIELDS: &[FieldDesc] = &[
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
        dbf_type: DbFieldType::UShort,
        read_only: false,
    },
    FieldDesc {
        name: "OUTA",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTB",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTC",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTD",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTE",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTF",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTG",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTH",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTI",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTJ",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTK",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTM",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTO",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTP",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DOL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OMSL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SELL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "IVOA",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IVOV",
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
    FieldDesc {
        name: "MDEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "ADEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "MLST",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "ALST",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
];

impl Record for DfanoutRecord {
    fn record_type(&self) -> &'static str {
        "dfanout"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        DFANOUT_FIELDS
    }

    fn uses_monitor_deadband(&self) -> bool {
        true
    }

    /// dfanout has no value computation in `process()` itself — VAL is
    /// either set by a CA/DB put or fetched from DOL by the framework's
    /// closed-loop binding. The output distribution to OUTA..OUTP is
    /// handled by the framework's multi-output dispatch, and the IVOA
    /// policy is applied there once `check_alarms` raises INVALID.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }

    /// C `dfanoutRecord.c::process` sets `udf = isnan(val)` after a DOL
    /// fetch; a NaN VAL keeps UDF set so the framework raises UDF_ALARM.
    fn value_is_undefined(&self) -> bool {
        self.val.is_nan()
    }

    /// C `dfanoutRecord.c::checkAlarms` — UDF alarm plus analog limit
    /// alarms (HIHI/HIGH/LOW/LOLO) with per-level hysteresis.
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        use crate::server::recgbl::{self, alarm_status};
        use crate::server::record::AlarmSeverity;

        if common.udf {
            recgbl::rec_gbl_set_sevr(common, alarm_status::UDF_ALARM, common.udfs);
            return;
        }

        let val = self.val;
        let hyst = self.hyst;
        let lalm = self.lalm;
        let raised = |common: &crate::server::record::CommonFields, sev: AlarmSeverity| {
            (sev as u16) > (common.nsev as u16)
        };

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
        self.lalm = val;
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        if let Some(s) = self.out_slot(name) {
            return Some(EpicsValue::String(s.to_string().into()));
        }
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "SELM" => Some(EpicsValue::Short(self.selm)),
            "SELN" => Some(EpicsValue::UShort(self.seln)),
            "DOL" => Some(EpicsValue::String(self.dol.clone().into())),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "SELL" => Some(EpicsValue::String(self.sell.clone().into())),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Double(self.ivov)),
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
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if self.out_slot(name).is_some() {
            return match value {
                EpicsValue::String(s) => {
                    *self.out_slot_mut(name).unwrap() = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            };
        }
        match name {
            "VAL" | "IVOV" | "HIHI" | "HIGH" | "LOW" | "LOLO" | "HYST" | "LALM" | "MDEL"
            | "ADEL" | "MLST" | "ALST" => {
                let v = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                match name {
                    "VAL" => self.val = v,
                    "IVOV" => self.ivov = v,
                    "HIHI" => self.hihi = v,
                    "HIGH" => self.high = v,
                    "LOW" => self.low = v,
                    "LOLO" => self.lolo = v,
                    "HYST" => self.hyst = v,
                    "LALM" => self.lalm = v,
                    "MDEL" => self.mdel = v,
                    "ADEL" => self.adel = v,
                    "MLST" => self.mlst = v,
                    "ALST" => self.alst = v,
                    _ => unreachable!(),
                }
                Ok(())
            }
            // SELN is `DBF_USHORT` (dfanoutRecord.dbd.pod:188): client
            // puts arrive converted to UShort; internal SELL link reads
            // still pass a Short. Cast both into the unsigned field
            // (C `dbPut` truncation).
            "SELN" => match value {
                EpicsValue::UShort(v) => {
                    self.seln = v;
                    Ok(())
                }
                EpicsValue::Short(v) => {
                    self.seln = v as u16;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SELM" | "OMSL" | "IVOA" | "HHSV" | "HSV" | "LSV" | "LLSV" => match value {
                EpicsValue::Short(v) => {
                    match name {
                        "SELM" => self.selm = v,
                        "OMSL" => self.omsl = v,
                        "IVOA" => self.ivoa = v,
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
            "DOL" | "SELL" => match value {
                EpicsValue::String(s) => {
                    match name {
                        "DOL" => self.dol = s.as_str_lossy().into_owned(),
                        "SELL" => self.sell = s.as_str_lossy().into_owned(),
                        _ => unreachable!(),
                    }
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }
}
