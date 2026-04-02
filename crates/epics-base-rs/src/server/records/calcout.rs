use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Calcout record — calc with output.
pub struct CalcoutRecord {
    pub val: f64,
    pub calc: String,
    pub oopt: i16, // Output Option: 0=Every, 1=OnChange, 2=WhenZero, 3=WhenNonzero, 4=TransZero, 5=TransNonzero
    pub dopt: i16, // Data Option: 0=Use CALC, 1=Use OCAL
    pub ocal: String,
    pub oval: f64,
    pub ivoa: i16, // Invalid Output Action: 0=Continue, 1=Don't drive, 2=Set to IVOV
    pub ivov: f64,
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
    // Previous values LA-LL
    pub la: f64,
    pub lb: f64,
    pub lc: f64,
    pub ld: f64,
    pub le: f64,
    pub lf: f64,
    pub lg: f64,
    pub lh: f64,
    pub li: f64,
    pub lj: f64,
    pub lk: f64,
    pub ll: f64,
    // Previous value for transition detection
    prev_val: f64,
}

impl Default for CalcoutRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            calc: String::new(),
            oopt: 0,
            dopt: 0,
            ocal: String::new(),
            oval: 0.0,
            ivoa: 0,
            ivov: 0.0,
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
            la: 0.0,
            lb: 0.0,
            lc: 0.0,
            ld: 0.0,
            le: 0.0,
            lf: 0.0,
            lg: 0.0,
            lh: 0.0,
            li: 0.0,
            lj: 0.0,
            lk: 0.0,
            ll: 0.0,
            prev_val: 0.0,
        }
    }
}

impl CalcoutRecord {
    fn get_vars(&self) -> [f64; 12] {
        [
            self.a, self.b, self.c, self.d, self.e, self.f, self.g, self.h, self.i, self.j, self.k,
            self.l,
        ]
    }

    fn should_output(&self) -> bool {
        match self.oopt {
            0 => true,                                            // Every Time
            1 => (self.val - self.prev_val).abs() > f64::EPSILON, // On Change
            2 => self.val == 0.0,                                 // When Zero
            3 => self.val != 0.0,                                 // When Non-zero
            4 => self.prev_val != 0.0 && self.val == 0.0,         // Transition to Zero
            5 => self.prev_val == 0.0 && self.val != 0.0,         // Transition to Non-zero
            _ => true,
        }
    }
}

static CALCOUT_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "CALC",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OOPT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DOPT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "OCAL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OVAL",
        dbf_type: DbFieldType::Double,
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
        name: "LA",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LB",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LC",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LD",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LE",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LF",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LG",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LH",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LI",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LJ",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LK",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LL",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
];

impl Record for CalcoutRecord {
    fn record_type(&self) -> &'static str {
        "calcout"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.prev_val = self.val;
        if !self.calc.is_empty() {
            let vars = self.get_vars();
            let mut inputs = crate::calc::NumericInputs::new();
            inputs.vars[..12].copy_from_slice(&vars);
            self.val = crate::calc::calc(&self.calc, &mut inputs)
                .map_err(|e| CaError::CalcError(e.to_string()))?;
        }

        if self.should_output() {
            if self.dopt == 1 && !self.ocal.is_empty() {
                let vars = self.get_vars();
                let mut inputs = crate::calc::NumericInputs::new();
                inputs.vars[..12].copy_from_slice(&vars);
                self.oval = crate::calc::calc(&self.ocal, &mut inputs)
                    .map_err(|e| CaError::CalcError(e.to_string()))?;
            } else {
                self.oval = self.val;
            }
        }
        // Save current values to LA-LL for next cycle
        self.la = self.a;
        self.lb = self.b;
        self.lc = self.c;
        self.ld = self.d;
        self.le = self.e;
        self.lf = self.f;
        self.lg = self.g;
        self.lh = self.h;
        self.li = self.i;
        self.lj = self.j;
        self.lk = self.k;
        self.ll = self.l;
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "CALC" => Some(EpicsValue::String(self.calc.clone())),
            "OOPT" => Some(EpicsValue::Short(self.oopt)),
            "DOPT" => Some(EpicsValue::Short(self.dopt)),
            "OCAL" => Some(EpicsValue::String(self.ocal.clone())),
            "OVAL" => Some(EpicsValue::Double(self.oval)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Double(self.ivov)),
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
            "LA" => Some(EpicsValue::Double(self.la)),
            "LB" => Some(EpicsValue::Double(self.lb)),
            "LC" => Some(EpicsValue::Double(self.lc)),
            "LD" => Some(EpicsValue::Double(self.ld)),
            "LE" => Some(EpicsValue::Double(self.le)),
            "LF" => Some(EpicsValue::Double(self.lf)),
            "LG" => Some(EpicsValue::Double(self.lg)),
            "LH" => Some(EpicsValue::Double(self.lh)),
            "LI" => Some(EpicsValue::Double(self.li)),
            "LJ" => Some(EpicsValue::Double(self.lj)),
            "LK" => Some(EpicsValue::Double(self.lk)),
            "LL" => Some(EpicsValue::Double(self.ll)),
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
            "CALC" => match value {
                EpicsValue::String(s) => {
                    self.calc = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("CALC".into())),
            },
            "OOPT" => match value {
                EpicsValue::Short(v) => {
                    self.oopt = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OOPT".into())),
            },
            "DOPT" => match value {
                EpicsValue::Short(v) => {
                    self.dopt = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("DOPT".into())),
            },
            "OCAL" => match value {
                EpicsValue::String(s) => {
                    self.ocal = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OCAL".into())),
            },
            "OVAL" => match value {
                EpicsValue::Double(v) => {
                    self.oval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OVAL".into())),
            },
            "IVOA" => match value {
                EpicsValue::Short(v) => {
                    self.ivoa = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("IVOA".into())),
            },
            "IVOV" => match value {
                EpicsValue::Double(v) => {
                    self.ivov = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("IVOV".into())),
            },
            "INPA" => match value {
                EpicsValue::String(s) => {
                    self.inpa = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPA".into())),
            },
            "INPB" => match value {
                EpicsValue::String(s) => {
                    self.inpb = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPB".into())),
            },
            "INPC" => match value {
                EpicsValue::String(s) => {
                    self.inpc = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPC".into())),
            },
            "INPD" => match value {
                EpicsValue::String(s) => {
                    self.inpd = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPD".into())),
            },
            "INPE" => match value {
                EpicsValue::String(s) => {
                    self.inpe = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPE".into())),
            },
            "INPF" => match value {
                EpicsValue::String(s) => {
                    self.inpf = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPF".into())),
            },
            "INPG" => match value {
                EpicsValue::String(s) => {
                    self.inpg = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPG".into())),
            },
            "INPH" => match value {
                EpicsValue::String(s) => {
                    self.inph = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPH".into())),
            },
            "INPI" => match value {
                EpicsValue::String(s) => {
                    self.inpi = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPI".into())),
            },
            "INPJ" => match value {
                EpicsValue::String(s) => {
                    self.inpj = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPJ".into())),
            },
            "INPK" => match value {
                EpicsValue::String(s) => {
                    self.inpk = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPK".into())),
            },
            "INPL" => match value {
                EpicsValue::String(s) => {
                    self.inpl = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPL".into())),
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
        CALCOUT_FIELDS
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

    fn should_output(&self) -> bool {
        self.should_output()
    }

    fn can_device_write(&self) -> bool {
        // calcout has a soft OUT link, not device support
        false
    }
}
