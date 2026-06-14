//! `longout` — integer output record with optional conditional-output
//! gating via the `OOPT` (output-execution-option) field.
//!
//! Manually implements [`Record`] rather than using `#[derive(EpicsRecord)]`
//! so the trait's [`Record::should_output`] hook can be overridden — the
//! derive macro emits only the four mandatory methods and there's no
//! opt-in for behaviour overrides. Once the macro grows a
//! `should_output_fn` knob this file can switch back to the derive form.

use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, MENU_YES_NO, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue, PvString};

pub struct LongoutRecord {
    pub val: i32,
    pub egu: PvString,
    pub hopr: i32,
    pub lopr: i32,
    pub drvh: i32,
    pub drvl: i32,
    pub hihi: i32,
    pub high: i32,
    pub low: i32,
    pub lolo: i32,
    pub hhsv: i16,
    pub hsv: i16,
    pub lsv: i16,
    pub llsv: i16,
    pub hyst: f64,
    pub lalm: f64,
    pub ivoa: i16,
    pub ivov: i32,
    pub adel: f64,
    pub mdel: f64,
    pub alst: f64,
    pub mlst: f64,
    pub omsl: i16,
    pub dol: String,
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    /// Output Execution Option (epics-base 7.0.8):
    ///
    /// | OOPT | Meaning                |
    /// |------|------------------------|
    /// | 0    | Every Time (default)   |
    /// | 1    | On Change (val ≠ pval) |
    /// | 2    | When Zero              |
    /// | 3    | When Non-zero          |
    /// | 4    | Transition to Zero     |
    /// | 5    | Transition to Non-zero |
    ///
    /// Consulted by [`Record::should_output`] before the framework
    /// writes VAL to the OUT link / device. Unknown values default to
    /// `false` (no output) matching C EPICS.
    pub oopt: i16,
    /// Previous VAL — captured after every output cycle so OOPT=1/4/5
    /// (transition modes) can detect changes. Initially zero; loops
    /// after the first successful output.
    pub pval: i32,
    /// Mirrors C `prec->outpvt` (longoutRecord.c).
    ///
    /// * `true` — force a write on the next process cycle regardless
    ///   of the OOPT=On_Change `val != pval` comparison.
    /// * `false` — On_Change uses the normal comparison.
    ///
    /// Set true at construction (= C `init_record`'s
    /// `outpvt = EXEC_OUTPUT`), cleared after every successful
    /// output (`on_output_complete`). The OUT-change re-trigger
    /// (PR #6c573b4 part 2) is wired through
    /// [`Record::special("OUT", true)`] — `RecordInstance::put_common_field`
    /// fires it after `common.out` is updated, so OUT is owned by
    /// the RecordInstance (common-field side), not by this struct.
    pub first_output_done: bool,
    /// `OOCH` (Output Exec. on OUT Change) — C `menuYesNo`. When YES
    /// (1), changing the OUT field at runtime forces the very next
    /// process cycle to write the current VAL regardless of OOPT
    /// (epics-base PR #6c573b4 part 2, `longoutRecord.c:223`).
    pub ooch: i16,
}

impl Default for LongoutRecord {
    fn default() -> Self {
        Self {
            val: 0,
            egu: PvString::new(),
            hopr: 0,
            lopr: 0,
            drvh: 0, // C defaults both to 0 (equal = no clamping)
            drvl: 0,
            hihi: 0,
            high: 0,
            low: 0,
            lolo: 0,
            hhsv: 0,
            hsv: 0,
            lsv: 0,
            llsv: 0,
            hyst: 0.0,
            lalm: 0.0,
            ivoa: 0,
            ivov: 0,
            adel: 0.0,
            mdel: 0.0,
            alst: 0.0,
            mlst: 0.0,
            omsl: 0,
            dol: String::new(),
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            oopt: 0,
            pval: 0,
            first_output_done: false,
            ooch: 0,
        }
    }
}

impl LongoutRecord {
    pub fn new(val: i32) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }

    /// Compute whether the framework should propagate VAL to the OUT
    /// link / device on this process cycle, based on OOPT semantics.
    /// Public so unit tests don't have to spin up the full processing
    /// loop to exercise each menu value.
    pub fn compute_should_output(&self) -> bool {
        // C parity (longoutRecord.c::conditional_write,
        // PR #6c573b4): the first-cycle force-emit applies **only**
        // to OOPT=On_Change (case 1 — `outpvt == EXEC_OUTPUT`).
        // Modes When_Zero / When_Non_zero / Transitions evaluate
        // their condition normally on the first cycle. Earlier this
        // fn generalised the force-emit across every OOPT, which
        // silently emitted output on initial putValues for the
        // value-only modes (e.g. When_Non_zero + initial val=0).
        match self.oopt {
            0 => true,
            1 => !self.first_output_done || self.val != self.pval,
            2 => self.val == 0,
            3 => self.val != 0,
            4 => self.pval != 0 && self.val == 0,
            5 => self.pval == 0 && self.val != 0,
            _ => false,
        }
    }
}

static LONGOUT_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "EGU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "DRVH",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "DRVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "HIHI",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "HIGH",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "LOW",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "LOLO",
        dbf_type: DbFieldType::Long,
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
        read_only: false,
    },
    FieldDesc {
        name: "IVOA",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IVOV",
        dbf_type: DbFieldType::Long,
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
    FieldDesc {
        name: "OOPT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "PVAL",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "OOCH",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

/// Choice labels for the output-execute-option menu, in index order.
/// C `menu(longoutOOPT)` (`longoutRecord.dbd.pod:23-29`). A distinct menu
/// from `calcoutOOPT`, with the same six choices and no trailing "Never".
const LONGOUT_OOPT_CHOICES: &[&str] = &[
    "Every Time",
    "On Change",
    "When Zero",
    "When Non-zero",
    "Transition To Zero",
    "Transition To Non-zero",
];

impl Record for LongoutRecord {
    fn record_type(&self) -> &'static str {
        "longout"
    }

    /// C `longoutRecord.c::convert` (lines 436-441): clamp VAL into the
    /// drive-limit window `[DRVL, DRVH]` every process cycle, but only
    /// when `DRVH > DRVL` (equal limits = no clamping). Without this an
    /// operator or DOL link writing outside the window propagates the
    /// unclamped value to the OUT link / device.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if self.drvh > self.drvl {
            self.val = self.val.clamp(self.drvl, self.drvh);
        }
        Ok(ProcessOutcome::complete())
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        LONGOUT_FIELDS
    }

    /// `DBF_MENU` fields, served as `DBR_ENUM` with the menu's choice labels
    /// in `.dbd` index order (`longoutRecord.dbd.pod`). `OOPT` is
    /// `menu(longoutOOPT)` (lines 23-29,454-458); `OOCH` and `SIMM` are
    /// `menu(menuYesNo)` (two-choice NO/YES — the integer record's `SIMM`
    /// menu, unlike the analog/binary `menuSimm`), reusing the shared yes/no
    /// table. `OOPT`/`OOCH` were added in EPICS 7.0.8. `SIMS`/`OLDSIMM`/
    /// `OMSL`/`IVOA` are shared menus resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "OOPT" => Some(LONGOUT_OOPT_CHOICES),
            "OOCH" | "SIMM" => Some(MENU_YES_NO),
            _ => None,
        }
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Long(self.val)),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "HOPR" => Some(EpicsValue::Long(self.hopr)),
            "LOPR" => Some(EpicsValue::Long(self.lopr)),
            "DRVH" => Some(EpicsValue::Long(self.drvh)),
            "DRVL" => Some(EpicsValue::Long(self.drvl)),
            "HIHI" => Some(EpicsValue::Long(self.hihi)),
            "HIGH" => Some(EpicsValue::Long(self.high)),
            "LOW" => Some(EpicsValue::Long(self.low)),
            "LOLO" => Some(EpicsValue::Long(self.lolo)),
            "HHSV" => Some(EpicsValue::Short(self.hhsv)),
            "HSV" => Some(EpicsValue::Short(self.hsv)),
            "LSV" => Some(EpicsValue::Short(self.lsv)),
            "LLSV" => Some(EpicsValue::Short(self.llsv)),
            "HYST" => Some(EpicsValue::Double(self.hyst)),
            "LALM" => Some(EpicsValue::Double(self.lalm)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Long(self.ivov)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "DOL" => Some(EpicsValue::String(self.dol.clone().into())),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            "OOPT" => Some(EpicsValue::Short(self.oopt)),
            "PVAL" => Some(EpicsValue::Long(self.pval)),
            "OOCH" => Some(EpicsValue::Short(self.ooch)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.validate_put(name, &value)?;
        match name {
            "VAL" => {
                if let EpicsValue::Long(v) = value {
                    self.val = v;
                }
            }
            "EGU" => {
                if let EpicsValue::String(v) = value {
                    self.egu = v;
                }
            }
            "HOPR" => {
                if let EpicsValue::Long(v) = value {
                    self.hopr = v;
                }
            }
            "LOPR" => {
                if let EpicsValue::Long(v) = value {
                    self.lopr = v;
                }
            }
            "DRVH" => {
                if let EpicsValue::Long(v) = value {
                    self.drvh = v;
                }
            }
            "DRVL" => {
                if let EpicsValue::Long(v) = value {
                    self.drvl = v;
                }
            }
            "HIHI" => {
                if let EpicsValue::Long(v) = value {
                    self.hihi = v;
                }
            }
            "HIGH" => {
                if let EpicsValue::Long(v) = value {
                    self.high = v;
                }
            }
            "LOW" => {
                if let EpicsValue::Long(v) = value {
                    self.low = v;
                }
            }
            "LOLO" => {
                if let EpicsValue::Long(v) = value {
                    self.lolo = v;
                }
            }
            "HHSV" => {
                if let EpicsValue::Short(v) = value {
                    self.hhsv = v;
                }
            }
            "HSV" => {
                if let EpicsValue::Short(v) = value {
                    self.hsv = v;
                }
            }
            "LSV" => {
                if let EpicsValue::Short(v) = value {
                    self.lsv = v;
                }
            }
            "LLSV" => {
                if let EpicsValue::Short(v) = value {
                    self.llsv = v;
                }
            }
            "HYST" => {
                if let EpicsValue::Double(v) = value {
                    self.hyst = v;
                }
            }
            "LALM" => {
                if let EpicsValue::Double(v) = value {
                    self.lalm = v;
                }
            }
            "IVOA" => {
                if let EpicsValue::Short(v) = value {
                    self.ivoa = v;
                }
            }
            "IVOV" => {
                if let EpicsValue::Long(v) = value {
                    self.ivov = v;
                }
            }
            "ADEL" => {
                if let EpicsValue::Double(v) = value {
                    self.adel = v;
                }
            }
            "MDEL" => {
                if let EpicsValue::Double(v) = value {
                    self.mdel = v;
                }
            }
            "ALST" => {
                if let EpicsValue::Double(v) = value {
                    self.alst = v;
                }
            }
            "MLST" => {
                if let EpicsValue::Double(v) = value {
                    self.mlst = v;
                }
            }
            "OMSL" => {
                if let EpicsValue::Short(v) = value {
                    self.omsl = v;
                }
            }
            "DOL" => {
                if let EpicsValue::String(v) = value {
                    self.dol = v.as_str_lossy().into_owned();
                }
            }
            "SIMM" => {
                if let EpicsValue::Short(v) = value {
                    self.simm = v;
                }
            }
            "SIML" => {
                if let EpicsValue::String(v) = value {
                    self.siml = v.as_str_lossy().into_owned();
                }
            }
            "SIOL" => {
                if let EpicsValue::String(v) = value {
                    self.siol = v.as_str_lossy().into_owned();
                }
            }
            "SIMS" => {
                if let EpicsValue::Short(v) = value {
                    self.sims = v;
                }
            }
            "OOPT" => {
                if let EpicsValue::Short(v) = value {
                    self.oopt = v;
                }
            }
            "PVAL" => {
                // Read-only — validate_put already rejected the call.
                return Err(CaError::ReadOnlyField("PVAL".into()));
            }
            "OOCH" => {
                if let EpicsValue::Short(v) = value {
                    self.ooch = v;
                }
            }
            _ => return Err(CaError::FieldNotFound(name.to_string())),
        }
        self.on_put(name);
        Ok(())
    }

    /// epics-base 7.0.8: gate the OUT-link / device write on OOPT.
    /// Returns `false` for OOPT modes whose condition isn't met
    /// (On Change / transition / when-zero / when-non-zero). The
    /// framework's processing.rs reads this before issuing the
    /// device write, so a `should_output() == false` cycle skips
    /// the device call entirely.
    fn should_output(&self) -> bool {
        self.compute_should_output()
    }

    /// Latch PVAL after a successful output so OOPT=1/4/5 transition
    /// detection on the next cycle has the right reference point.
    /// Also marks `first_output_done` so subsequent cycles apply the
    /// real OOPT comparison rather than the first-cycle force-emit.
    fn on_output_complete(&mut self) {
        self.pval = self.val;
        self.first_output_done = true;
    }

    /// C `longoutRecord.c::special` (PR #6c573b4 part 2): when the
    /// OUT link is rewritten at runtime and `OOCH=YES`, force the
    /// next process cycle to emit output regardless of OOPT=On_Change
    /// `val == pval` suppression. We fire on `after=true` because the
    /// common `out` field is set inside `RecordInstance::put_common_field`
    /// AFTER the `special(name, false)` validation hook — so by the
    /// time we run, `instance.common.out` already holds the new value
    /// (we don't need to read it; just react to the field name).
    fn special(&mut self, field: &str, after: bool) -> crate::error::CaResult<()> {
        if after && field == "OUT" && self.ooch == 1 {
            self.first_output_done = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oopt_every_time_default() {
        let r = LongoutRecord::new(42);
        assert!(r.compute_should_output());
    }

    #[test]
    fn seed_deadband_tracking_seeds_trackers_from_val() {
        use crate::server::record::Record;
        // C `longoutRecord.c` init_record ends with mlst=alst=lalm=val, so a
        // record initialised to a nonzero VAL posts no spurious first-cycle
        // monitor (DELTA(mlst,val)==0). The trackers default to 0.0, which
        // conflates "never published" with "published 0"; the framework seed
        // must lift them to val via the generic get_field/put_field path.
        let mut r = LongoutRecord::new(5);
        assert_eq!(r.mlst, 0.0, "precondition: tracker is the 0.0 default");
        assert_eq!(r.alst, 0.0);
        assert_eq!(r.lalm, 0.0);
        r.seed_deadband_tracking();
        assert_eq!(r.mlst, 5.0, "mlst seeded from val");
        assert_eq!(r.alst, 5.0, "alst seeded from val");
        assert_eq!(r.lalm, 5.0, "lalm seeded from val");
        // First process leaves val unchanged: DELTA(mlst=5, val=5)=0 is not
        // > mdel(0), so no DBE_VALUE/DBE_LOG post — matching C.
        assert!((5.0_f64 - r.mlst).abs() <= 0.0);
    }

    #[test]
    fn oopt_on_change() {
        let mut r = LongoutRecord::new(0);
        r.oopt = 1;
        r.first_output_done = true; // simulate post-first-cycle state
        r.pval = 5;
        r.val = 5;
        assert!(!r.compute_should_output(), "val==pval suppresses output");
        r.val = 7;
        assert!(r.compute_should_output(), "val!=pval permits output");
    }

    #[test]
    fn oopt_when_zero_and_nonzero() {
        let mut r = LongoutRecord::new(0);
        r.first_output_done = true;
        r.oopt = 2; // When Zero
        r.val = 0;
        assert!(r.compute_should_output());
        r.val = 1;
        assert!(!r.compute_should_output());

        r.oopt = 3; // When Non-zero
        r.val = 0;
        assert!(!r.compute_should_output());
        r.val = 5;
        assert!(r.compute_should_output());
    }

    #[test]
    fn oopt_transitions() {
        let mut r = LongoutRecord::new(0);
        r.first_output_done = true;
        r.oopt = 4; // Transition to Zero
        r.pval = 5;
        r.val = 0;
        assert!(r.compute_should_output(), "nonzero→zero transition fires");
        r.pval = 0;
        r.val = 0;
        assert!(!r.compute_should_output(), "zero→zero suppressed");

        r.oopt = 5; // Transition to Non-zero
        r.pval = 0;
        r.val = 5;
        assert!(r.compute_should_output(), "zero→nonzero transition fires");
        r.pval = 5;
        r.val = 5;
        assert!(!r.compute_should_output(), "nonzero→nonzero suppressed");
    }

    #[test]
    fn oopt_unknown_value_suppresses_output() {
        // C EPICS treats unknown OOPT as "don't output" to fail-safe.
        let mut r = LongoutRecord::new(0);
        r.first_output_done = true;
        r.oopt = 99;
        assert!(!r.compute_should_output());
    }

    /// epics-base PR #6c573b4: the very first process cycle must
    /// always emit, even when OOPT is in a transition mode whose
    /// comparison says "no change". Before the fix, OOPT=1/4 with
    /// the default val=pval=0 would silently swallow the initial
    /// device write.
    #[test]
    fn oopt_on_change_first_cycle_forces_output() {
        let mut r = LongoutRecord::new(0);
        r.oopt = 1; // On Change
        // val == pval == 0, first_output_done still false (default).
        assert!(
            r.compute_should_output(),
            "first cycle must force output regardless of OOPT comparison"
        );
        // Simulate the framework calling on_output_complete after
        // the device write succeeds.
        r.on_output_complete();
        assert!(r.first_output_done);
        // Next cycle with val still equal to pval honours OOPT=1.
        assert!(
            !r.compute_should_output(),
            "post-first cycle val==pval honours OOPT=On Change suppression"
        );
    }

    #[test]
    fn oopt_transition_modes_do_not_force_first_cycle() {
        // C parity (longoutRecord.c::conditional_write,
        // PR #6c573b4): only OOPT=On_Change (case 1) gets the
        // first-cycle force-emit via `outpvt == EXEC_OUTPUT`.
        // Transition modes (4=To_Zero, 5=To_Non_zero) and value
        // modes (2=When_Zero, 3=When_Non_zero) evaluate their
        // condition normally on the first cycle. With val == pval
        // == 0:
        //   OOPT=4 (To Zero):    pval!=0 && val==0 → false → no out
        //   OOPT=5 (To Non-zero): pval==0 && val!=0 → false → no out
        let mut r = LongoutRecord::new(0);
        r.oopt = 4;
        assert!(
            !r.compute_should_output(),
            "OOPT=4 (Transition_To_Zero) first cycle val==pval==0: no output"
        );
        r.oopt = 5;
        assert!(
            !r.compute_should_output(),
            "OOPT=5 (Transition_To_Non_zero) first cycle val==pval==0: no output"
        );
    }

    #[test]
    fn put_get_oopt_roundtrip() {
        let mut r = LongoutRecord::new(0);
        r.put_field("OOPT", EpicsValue::Short(3)).unwrap();
        assert_eq!(r.get_field("OOPT"), Some(EpicsValue::Short(3)));
        assert_eq!(r.oopt, 3);
    }

    #[test]
    fn pval_is_read_only_via_put_field() {
        let mut r = LongoutRecord::new(0);
        let err = r.put_field("PVAL", EpicsValue::Long(42)).unwrap_err();
        assert!(matches!(err, CaError::ReadOnlyField(_)));
    }

    /// PR #6c573b4 part 2 (`longoutRecord.c:222-225`): writing OUT at
    /// runtime with `OOCH = YES` re-arms the next-cycle write under
    /// OOPT=On_Change. Without OOCH=YES the OUT change is silent (C
    /// requires the explicit opt-in). Exercised through the
    /// `Record::special("OUT", true)` hook the way
    /// `RecordInstance::put_common_field` fires it on a real OUT
    /// rewrite.
    #[test]
    fn ooch_yes_out_change_forces_next_on_change_write() {
        let mut r = LongoutRecord::new(0);
        r.oopt = 1; // On_Change
        r.first_output_done = true; // post-first-cycle: no force pending
        r.val = 5;
        r.pval = 5; // val == pval — normally On_Change would skip.
        assert!(
            !r.compute_should_output(),
            "baseline: val==pval should suppress On_Change output"
        );
        // OOCH=YES then OUT change (fired via `special(OUT, true)`).
        r.ooch = 1;
        r.special("OUT", true).unwrap();
        assert!(
            r.compute_should_output(),
            "OOCH=YES + OUT change must force the next On_Change write"
        );
    }

    #[test]
    fn ooch_no_out_change_is_silent() {
        let mut r = LongoutRecord::new(0);
        r.oopt = 1; // On_Change
        r.first_output_done = true;
        r.val = 5;
        r.pval = 5;
        // OOCH=NO (default 0) — OUT change must NOT trigger a write.
        r.special("OUT", true).unwrap();
        assert!(
            !r.compute_should_output(),
            "OOCH=NO: OUT change is silent, val==pval still suppresses output"
        );
    }
}
