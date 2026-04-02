use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Analog output record with conversion and output policy support.
pub struct AoRecord {
    // Display
    pub val: f64,
    pub egu: String,
    pub hopr: f64,
    pub lopr: f64,
    pub prec: i16,
    pub drvh: f64,
    pub drvl: f64,
    // Conversion
    pub rval: i32,
    pub oval: f64,
    pub linr: i16, // 0=NO_CONVERSION, 1=LINEAR
    pub eguf: f64,
    pub egul: f64,
    pub eslo: f64, // default 1.0
    pub roff: i32,
    pub aslo: f64, // default 1.0
    pub aoff: f64,
    // Output control
    pub omsl: i16,   // 0=supervisory, 1=closed_loop
    pub dol: String, // desired output location link
    pub oif: i16,    // 0=Full, 1=Incremental
    pub oroc: f64,   // output rate of change
    pub pval: f64,   // previous value
    // Invalid output
    pub ivoa: i16, // 0=Continue, 1=Don't drive, 2=Set to IVOV
    pub ivov: f64,
    // Monitor deadband
    pub adel: f64,
    pub mdel: f64,
    pub lalm: f64,
    pub alst: f64,
    pub mlst: f64,
    // Runtime
    pub init: bool,
    // Simulation
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
}

impl Default for AoRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            egu: String::new(),
            hopr: 0.0,
            lopr: 0.0,
            prec: 0,
            drvh: 0.0,
            drvl: 0.0,
            rval: 0,
            oval: 0.0,
            linr: 0,
            eguf: 0.0,
            egul: 0.0,
            eslo: 1.0,
            roff: 0,
            aslo: 1.0,
            aoff: 0.0,
            omsl: 0,
            dol: String::new(),
            oif: 0,
            oroc: 0.0,
            pval: 0.0,
            ivoa: 0,
            ivov: 0.0,
            adel: 0.0,
            mdel: 0.0,
            lalm: 0.0,
            alst: 0.0,
            mlst: 0.0,
            init: false,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
        }
    }
}

impl AoRecord {
    pub fn new(val: f64) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }
}

static FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "EGU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "PREC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DRVH",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DRVL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "RVAL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "OVAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LINR",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "EGUF",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "EGUL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "ESLO",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "ROFF",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "ASLO",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "AOFF",
        dbf_type: DbFieldType::Double,
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
        name: "OIF",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "OROC",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "PVAL",
        dbf_type: DbFieldType::Double,
        read_only: true,
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
        name: "ADEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "MDEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LALM",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "ALST",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "MLST",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "INIT",
        dbf_type: DbFieldType::Char,
        read_only: true,
    },
    FieldDesc {
        name: "SIMM",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SIML",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "SIOL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "SIMS",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

/// Try to parse a DOL string as a constant value.
/// Returns Some(f64) if it's a number, None if it's a link (PV name).
fn dol_as_constant(dol: &str) -> Option<f64> {
    let s = dol.trim();
    if s.is_empty() {
        return None;
    }
    s.parse::<f64>().ok()
}

impl Record for AoRecord {
    fn record_type(&self) -> &'static str {
        "ao"
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            // If DOL contains a constant value, use it as the initial VAL.
            // This matches C EPICS: when DOL is a constant (not a link),
            // init_record sets VAL from DOL and marks UDF=false.
            if let Some(v) = dol_as_constant(&self.dol) {
                self.val = v;
                self.oval = v;
            }
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // DOL/OMSL handling is done by the framework (processing.rs) which:
        // - Reads DOL link value before calling process()
        // - Applies OIF=0 (Full) or OIF=1 (Incremental: VAL += DOL)
        // The record only handles constant DOL values that the framework
        // can't resolve (pure numeric strings without a PV target).
        if self.omsl == 1 && !self.dol.is_empty() {
            if let Some(v) = dol_as_constant(&self.dol) {
                self.val = v;
            }
            // PV link DOL: framework already applied the value
        }
        if self.drvh != self.drvl && self.drvh > self.drvl {
            self.val = self.val.clamp(self.drvl, self.drvh);
        }

        if self.oroc != 0.0 && self.init {
            let delta = self.val - self.oval;
            if delta.abs() > self.oroc {
                self.val = self.oval + delta.signum() * self.oroc;
            }
        }

        self.pval = self.oval;
        self.oval = self.val;

        if self.linr == 1 && self.eslo != 0.0 {
            self.rval = ((self.oval - self.egul) / self.eslo) as i32 - self.roff;
        }

        self.init = true;
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "DRVH" => Some(EpicsValue::Double(self.drvh)),
            "DRVL" => Some(EpicsValue::Double(self.drvl)),
            "RVAL" => Some(EpicsValue::Long(self.rval)),
            "OVAL" => Some(EpicsValue::Double(self.oval)),
            "LINR" => Some(EpicsValue::Short(self.linr)),
            "EGUF" => Some(EpicsValue::Double(self.eguf)),
            "EGUL" => Some(EpicsValue::Double(self.egul)),
            "ESLO" => Some(EpicsValue::Double(self.eslo)),
            "ROFF" => Some(EpicsValue::Long(self.roff)),
            "ASLO" => Some(EpicsValue::Double(self.aslo)),
            "AOFF" => Some(EpicsValue::Double(self.aoff)),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "DOL" => Some(EpicsValue::String(self.dol.clone())),
            "OIF" => Some(EpicsValue::Short(self.oif)),
            "OROC" => Some(EpicsValue::Double(self.oroc)),
            "PVAL" => Some(EpicsValue::Double(self.pval)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Double(self.ivov)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "LALM" => Some(EpicsValue::Double(self.lalm)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            "INIT" => Some(EpicsValue::Char(if self.init { 1 } else { 0 })),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
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
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "EGU" => match value {
                EpicsValue::String(v) => {
                    self.egu = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "HOPR" => match value {
                EpicsValue::Double(v) => {
                    self.hopr = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LOPR" => match value {
                EpicsValue::Double(v) => {
                    self.lopr = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "PREC" => match value {
                EpicsValue::Short(v) => {
                    self.prec = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DRVH" => match value {
                EpicsValue::Double(v) => {
                    self.drvh = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DRVL" => match value {
                EpicsValue::Double(v) => {
                    self.drvl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "RVAL" => match value {
                EpicsValue::Long(v) => {
                    self.rval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OVAL" => match value {
                EpicsValue::Double(v) => {
                    self.oval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LINR" => match value {
                EpicsValue::Short(v) => {
                    self.linr = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "EGUF" => match value {
                EpicsValue::Double(v) => {
                    self.eguf = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "EGUL" => match value {
                EpicsValue::Double(v) => {
                    self.egul = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ESLO" => match value {
                EpicsValue::Double(v) => {
                    self.eslo = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ROFF" => match value {
                EpicsValue::Long(v) => {
                    self.roff = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ASLO" => match value {
                EpicsValue::Double(v) => {
                    self.aslo = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "AOFF" => match value {
                EpicsValue::Double(v) => {
                    self.aoff = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OMSL" => match value {
                EpicsValue::Short(v) => {
                    self.omsl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DOL" => match value {
                EpicsValue::String(v) => {
                    self.dol = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OIF" => match value {
                EpicsValue::Short(v) => {
                    self.oif = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OROC" => match value {
                EpicsValue::Double(v) => {
                    self.oroc = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "IVOA" => match value {
                EpicsValue::Short(v) => {
                    self.ivoa = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "IVOV" => match value {
                EpicsValue::Double(v) => {
                    self.ivov = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ADEL" => match value {
                EpicsValue::Double(v) => {
                    self.adel = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "MDEL" => match value {
                EpicsValue::Double(v) => {
                    self.mdel = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LALM" => match value {
                EpicsValue::Double(v) => {
                    self.lalm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ALST" => match value {
                EpicsValue::Double(v) => {
                    self.alst = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "MLST" => match value {
                EpicsValue::Double(v) => {
                    self.mlst = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIMM" => match value {
                EpicsValue::Short(v) => {
                    self.simm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIML" => match value {
                EpicsValue::String(v) => {
                    self.siml = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIOL" => match value {
                EpicsValue::String(v) => {
                    self.siol = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIMS" => match value {
                EpicsValue::Short(v) => {
                    self.sims = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}
