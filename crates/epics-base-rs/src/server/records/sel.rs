use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Sel (select) record — selects one of A-L based on SELM algorithm.
pub struct SelRecord {
    pub val: f64,
    pub selm: i16,   // 0=Specified, 1=High, 2=Low, 3=Median
    pub seln: i16,   // Selection number (for Specified mode)
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
            a: 0.0,
            b: 0.0,
            c: 0.0,
            d: 0.0,
            e: 0.0,
            f: 0.0,
            g: 0.0,
            h: 0.0,
            i: 0.0,
            j: 0.0,
            k: 0.0,
            l: 0.0,
        }
    }
}

impl SelRecord {
    fn get_values(&self) -> [f64; 12] {
        [
            self.a, self.b, self.c, self.d, self.e, self.f, self.g, self.h, self.i, self.j, self.k,
            self.l,
        ]
    }

    fn get_value_by_index(&self, idx: usize) -> f64 {
        self.get_values().get(idx).copied().unwrap_or(0.0)
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
];

impl Record for SelRecord {
    fn record_type(&self) -> &'static str {
        "sel"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let vals = self.get_values();
        self.val = match self.selm {
            0 => self.get_value_by_index(self.seln as usize),
            1 => vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            2 => vals.iter().cloned().fold(f64::INFINITY, f64::min),
            3 => {
                let mut sorted = vals.to_vec();
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let mid = sorted.len() / 2;
                if sorted.len() % 2 == 0 {
                    (sorted[mid - 1] + sorted[mid]) / 2.0
                } else {
                    sorted[mid]
                }
            }
            _ => self.val,
        };
        Ok(ProcessOutcome::complete())
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
