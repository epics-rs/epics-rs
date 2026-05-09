use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Calc record — evaluates CALC expression with inputs A-U.
///
/// Matches epics-base PR #655 (12 → 21 inputs, A-L → A-U).
pub struct CalcRecord {
    pub val: f64,
    pub calc: String,
    // Display/engineering
    pub egu: String,
    pub prec: i16,
    pub hopr: f64,
    pub lopr: f64,
    // Alarm/monitor
    pub adel: f64,
    pub mdel: f64,
    pub lalm: f64,
    pub alst: f64,
    pub mlst: f64,
    // Input link strings (INPA..INPU)
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
    pub inpm: String,
    pub inpn: String,
    pub inpo: String,
    pub inpp: String,
    pub inpq: String,
    pub inpr: String,
    pub inps: String,
    pub inpt: String,
    pub inpu: String,
    // Input values A-U
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
    pub m: f64,
    pub n: f64,
    pub o: f64,
    pub p: f64,
    pub q: f64,
    pub r: f64,
    pub s: f64,
    pub t: f64,
    pub u: f64,
    // Previous values LA-LU (saved after each process)
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
    pub lm: f64,
    pub ln: f64,
    pub lo: f64,
    pub lp: f64,
    pub lq: f64,
    pub lr: f64,
    pub ls: f64,
    pub lt: f64,
    pub lu: f64,
    // CALC_ALARM flag: set when calcPerform fails
    pub calc_alarm: bool,
    // Cached compiled expression (RPCL equivalent)
    rpcl: Option<crate::calc::CompiledExpr>,
}

impl Default for CalcRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            calc: String::new(),
            egu: String::new(),
            prec: 0,
            hopr: 0.0,
            lopr: 0.0,
            adel: 0.0,
            mdel: 0.0,
            lalm: 0.0,
            alst: 0.0,
            mlst: 0.0,
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
            inpm: String::new(),
            inpn: String::new(),
            inpo: String::new(),
            inpp: String::new(),
            inpq: String::new(),
            inpr: String::new(),
            inps: String::new(),
            inpt: String::new(),
            inpu: String::new(),
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
            m: 0.0,
            n: 0.0,
            o: 0.0,
            p: 0.0,
            q: 0.0,
            r: 0.0,
            s: 0.0,
            t: 0.0,
            u: 0.0,
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
            lm: 0.0,
            ln: 0.0,
            lo: 0.0,
            lp: 0.0,
            lq: 0.0,
            lr: 0.0,
            ls: 0.0,
            lt: 0.0,
            lu: 0.0,
            calc_alarm: false,
            rpcl: None,
        }
    }
}

impl CalcRecord {
    pub fn new(calc: &str) -> Self {
        Self {
            calc: calc.to_string(),
            ..Default::default()
        }
    }

    fn get_vars(&self) -> [f64; 21] {
        [
            self.a, self.b, self.c, self.d, self.e, self.f, self.g, self.h, self.i, self.j, self.k,
            self.l, self.m, self.n, self.o, self.p, self.q, self.r, self.s, self.t, self.u,
        ]
    }

    pub fn get_inp_link(&self, idx: usize) -> &str {
        match idx {
            0 => &self.inpa,
            1 => &self.inpb,
            2 => &self.inpc,
            3 => &self.inpd,
            4 => &self.inpe,
            5 => &self.inpf,
            6 => &self.inpg,
            7 => &self.inph,
            8 => &self.inpi,
            9 => &self.inpj,
            10 => &self.inpk,
            11 => &self.inpl,
            12 => &self.inpm,
            13 => &self.inpn,
            14 => &self.inpo,
            15 => &self.inpp,
            16 => &self.inpq,
            17 => &self.inpr,
            18 => &self.inps,
            19 => &self.inpt,
            20 => &self.inpu,
            _ => "",
        }
    }

    /// Get input link strings for external processing.
    pub fn input_links(&self) -> [&str; 21] {
        [
            &self.inpa, &self.inpb, &self.inpc, &self.inpd, &self.inpe, &self.inpf, &self.inpg,
            &self.inph, &self.inpi, &self.inpj, &self.inpk, &self.inpl, &self.inpm, &self.inpn,
            &self.inpo, &self.inpp, &self.inpq, &self.inpr, &self.inps, &self.inpt, &self.inpu,
        ]
    }

    pub fn set_var(&mut self, idx: usize, val: f64) {
        match idx {
            0 => self.a = val,
            1 => self.b = val,
            2 => self.c = val,
            3 => self.d = val,
            4 => self.e = val,
            5 => self.f = val,
            6 => self.g = val,
            7 => self.h = val,
            8 => self.i = val,
            9 => self.j = val,
            10 => self.k = val,
            11 => self.l = val,
            12 => self.m = val,
            13 => self.n = val,
            14 => self.o = val,
            15 => self.p = val,
            16 => self.q = val,
            17 => self.r = val,
            18 => self.s = val,
            19 => self.t = val,
            20 => self.u = val,
            _ => {}
        }
    }
}

static CALC_FIELDS: &[FieldDesc] = &[
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
        name: "EGU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "PREC",
        dbf_type: DbFieldType::Short,
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
        read_only: true,
    },
    FieldDesc {
        name: "ALST",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "MLST",
        dbf_type: DbFieldType::Double,
        read_only: true,
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
        name: "INPM",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPO",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPP",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPQ",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPR",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPS",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPT",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPU",
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
        name: "M",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "N",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "O",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "P",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "Q",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "R",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "S",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "T",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "U",
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
    FieldDesc {
        name: "LM",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LN",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LO",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LP",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LQ",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LR",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LS",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LT",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LU",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
];

impl Record for CalcRecord {
    fn record_type(&self) -> &'static str {
        "calc"
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 && !self.calc.is_empty() {
            // Compile CALC expression and cache it (like C's RPCL)
            self.rpcl = crate::calc::compile(&self.calc).ok();
            self.mlst = self.val;
            self.alst = self.val;
            self.lalm = self.val;
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if let Some(ref compiled) = self.rpcl {
            let vars = self.get_vars();
            let mut inputs = crate::calc::NumericInputs::with_vars(vars);
            // C sets CALC_ALARM on failure but continues processing
            match crate::calc::eval(compiled, &mut inputs) {
                Ok(v) => {
                    self.val = v;
                    self.calc_alarm = false;
                }
                Err(_) => {
                    self.calc_alarm = true;
                }
            }
        } else if !self.calc.is_empty() {
            // Fallback: try to compile and eval (e.g., if CALC was set after init)
            match crate::calc::calc(
                &self.calc,
                &mut crate::calc::NumericInputs::with_vars(self.get_vars()),
            ) {
                Ok(v) => {
                    self.val = v;
                    self.calc_alarm = false;
                    // Cache for next time
                    self.rpcl = crate::calc::compile(&self.calc).ok();
                }
                Err(_) => {
                    self.calc_alarm = true;
                }
            }
        }
        // Save current values to LA-LU for next cycle
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
        self.lm = self.m;
        self.ln = self.n;
        self.lo = self.o;
        self.lp = self.p;
        self.lq = self.q;
        self.lr = self.r;
        self.ls = self.s;
        self.lt = self.t;
        self.lu = self.u;
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "CALC" => Some(EpicsValue::String(self.calc.clone())),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "LALM" => Some(EpicsValue::Double(self.lalm)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            "CALC_ALARM" => Some(EpicsValue::Char(if self.calc_alarm { 1 } else { 0 })),
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
            "INPM" => Some(EpicsValue::String(self.inpm.clone())),
            "INPN" => Some(EpicsValue::String(self.inpn.clone())),
            "INPO" => Some(EpicsValue::String(self.inpo.clone())),
            "INPP" => Some(EpicsValue::String(self.inpp.clone())),
            "INPQ" => Some(EpicsValue::String(self.inpq.clone())),
            "INPR" => Some(EpicsValue::String(self.inpr.clone())),
            "INPS" => Some(EpicsValue::String(self.inps.clone())),
            "INPT" => Some(EpicsValue::String(self.inpt.clone())),
            "INPU" => Some(EpicsValue::String(self.inpu.clone())),
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
            "M" => Some(EpicsValue::Double(self.m)),
            "N" => Some(EpicsValue::Double(self.n)),
            "O" => Some(EpicsValue::Double(self.o)),
            "P" => Some(EpicsValue::Double(self.p)),
            "Q" => Some(EpicsValue::Double(self.q)),
            "R" => Some(EpicsValue::Double(self.r)),
            "S" => Some(EpicsValue::Double(self.s)),
            "T" => Some(EpicsValue::Double(self.t)),
            "U" => Some(EpicsValue::Double(self.u)),
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
            "LM" => Some(EpicsValue::Double(self.lm)),
            "LN" => Some(EpicsValue::Double(self.ln)),
            "LO" => Some(EpicsValue::Double(self.lo)),
            "LP" => Some(EpicsValue::Double(self.lp)),
            "LQ" => Some(EpicsValue::Double(self.lq)),
            "LR" => Some(EpicsValue::Double(self.lr)),
            "LS" => Some(EpicsValue::Double(self.ls)),
            "LT" => Some(EpicsValue::Double(self.lt)),
            "LU" => Some(EpicsValue::Double(self.lu)),
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
                    // Recompile on CALC change (like C special SPC_CALC)
                    self.rpcl = crate::calc::compile(&s).ok();
                    self.calc = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("CALC".into())),
            },
            "EGU" => match value {
                EpicsValue::String(s) => {
                    self.egu = s;
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
            "INPM" => match value {
                EpicsValue::String(s) => {
                    self.inpm = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPM".into())),
            },
            "INPN" => match value {
                EpicsValue::String(s) => {
                    self.inpn = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPN".into())),
            },
            "INPO" => match value {
                EpicsValue::String(s) => {
                    self.inpo = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPO".into())),
            },
            "INPP" => match value {
                EpicsValue::String(s) => {
                    self.inpp = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPP".into())),
            },
            "INPQ" => match value {
                EpicsValue::String(s) => {
                    self.inpq = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPQ".into())),
            },
            "INPR" => match value {
                EpicsValue::String(s) => {
                    self.inpr = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPR".into())),
            },
            "INPS" => match value {
                EpicsValue::String(s) => {
                    self.inps = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPS".into())),
            },
            "INPT" => match value {
                EpicsValue::String(s) => {
                    self.inpt = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPT".into())),
            },
            "INPU" => match value {
                EpicsValue::String(s) => {
                    self.inpu = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPU".into())),
            },
            "A" => match value {
                EpicsValue::Double(v) => {
                    self.a = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.a = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("A".into()))
                    }
                }
            },
            "B" => match value {
                EpicsValue::Double(v) => {
                    self.b = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.b = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("B".into()))
                    }
                }
            },
            "C" => match value {
                EpicsValue::Double(v) => {
                    self.c = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.c = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("C".into()))
                    }
                }
            },
            "D" => match value {
                EpicsValue::Double(v) => {
                    self.d = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.d = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("D".into()))
                    }
                }
            },
            "E" => match value {
                EpicsValue::Double(v) => {
                    self.e = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.e = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("E".into()))
                    }
                }
            },
            "F" => match value {
                EpicsValue::Double(v) => {
                    self.f = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.f = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("F".into()))
                    }
                }
            },
            "G" => match value {
                EpicsValue::Double(v) => {
                    self.g = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.g = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("G".into()))
                    }
                }
            },
            "H" => match value {
                EpicsValue::Double(v) => {
                    self.h = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.h = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("H".into()))
                    }
                }
            },
            "I" => match value {
                EpicsValue::Double(v) => {
                    self.i = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.i = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("I".into()))
                    }
                }
            },
            "J" => match value {
                EpicsValue::Double(v) => {
                    self.j = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.j = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("J".into()))
                    }
                }
            },
            "K" => match value {
                EpicsValue::Double(v) => {
                    self.k = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.k = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("K".into()))
                    }
                }
            },
            "L" => match value {
                EpicsValue::Double(v) => {
                    self.l = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.l = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("L".into()))
                    }
                }
            },
            "M" => match value {
                EpicsValue::Double(v) => {
                    self.m = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.m = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("M".into()))
                    }
                }
            },
            "N" => match value {
                EpicsValue::Double(v) => {
                    self.n = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.n = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("N".into()))
                    }
                }
            },
            "O" => match value {
                EpicsValue::Double(v) => {
                    self.o = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.o = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("O".into()))
                    }
                }
            },
            "P" => match value {
                EpicsValue::Double(v) => {
                    self.p = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.p = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("P".into()))
                    }
                }
            },
            "Q" => match value {
                EpicsValue::Double(v) => {
                    self.q = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.q = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("Q".into()))
                    }
                }
            },
            "R" => match value {
                EpicsValue::Double(v) => {
                    self.r = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.r = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("R".into()))
                    }
                }
            },
            "S" => match value {
                EpicsValue::Double(v) => {
                    self.s = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.s = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("S".into()))
                    }
                }
            },
            "T" => match value {
                EpicsValue::Double(v) => {
                    self.t = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.t = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("T".into()))
                    }
                }
            },
            "U" => match value {
                EpicsValue::Double(v) => {
                    self.u = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.u = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch("U".into()))
                    }
                }
            },
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        CALC_FIELDS
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
            ("INPM", "M"),
            ("INPN", "N"),
            ("INPO", "O"),
            ("INPP", "P"),
            ("INPQ", "Q"),
            ("INPR", "R"),
            ("INPS", "S"),
            ("INPT", "T"),
            ("INPU", "U"),
        ]
    }
}
