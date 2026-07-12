use crate::calc::NumericInputs;
use crate::calc::{CompiledExpr, compile as calc_compile, eval as calc_eval};
use crate::error::{CaError, CaResult};
use crate::server::record::{
    FieldDesc, ProcessAction, ProcessOutcome, Record, RecordProcessResult,
};
use crate::types::{DbFieldType, EpicsValue};

// swait (string wait) record from synApps calc module.
// Functionally equivalent to scalcout, but uses INxN/INxP naming for input links
// (INxN = link name, INxP = process passive flag) instead of INPx.
// Supports 12 numeric inputs (A-L), a CALC expression, and a single output link.
// DOL/DOLD: desired output; DOPT: 0=use CALC, 1=use DOLD; OOPT: output condition.
pub struct SwaitRecord {
    pub val: f64,
    pub calc: String,
    compiled_calc: Option<CompiledExpr>,
    pub oopt: i16,
    pub dopt: i16,
    // DOLN ("DOL PV Name", C `swaitRecord.dbd:150`, DBF_STRING/SPC_MOD) and
    // DOLD ("Desired Output Data", :466). With DOPT="Use DOL", C `execOutput`
    // (swaitRecord.c:763-772) fetches DOLN's PV into DOLD at OUTPUT time and
    // writes DOLD to OUT; see [`Record::output_time_input_links`]. With DOLN
    // unset (C `dolv == NO_PV`) the get is skipped and DOLD keeps whatever was
    // written to it (an operator/client put, or the init value).
    pub doln: String,
    pub dold: f64,
    // OVAL ("Old Value", C `swaitRecord.dbd:440`): the VAL of the PREVIOUS
    // cycle. C sets it at `swaitRecord.c:471` (`pwait->oval = pwait->val`,
    // after the OOPT test that reads it) and never posts it or writes it out —
    // its only consumer is the OOPT comparison (:432-446). It is NOT an output
    // staging cell: `execOutput` composes the output value from VAL or DOLD.
    pub oval: f64,
    // OEVT ("Output Event") — C `swaitRecord.c` `pwait->oevt` (DBF_USHORT).
    // When output fires and `oevt > 0`, `execOutput` posts the numeric
    // software event (`post_event((int)oevt)`, swaitRecord.c:797); see
    // [`Record::output_event`]. swait has no IVOA field, so the post is
    // never suppressed by the framework Don't_drive veto.
    pub oevt: u16,
    // ODLY ("Output Execute Delay", seconds) — C `swaitRecord.c` `pwait->odly`
    // (DBF_FLOAT). When output fires and `odly > 0`, `schedOutput`
    // (swaitRecord.c:719) defers the OUT write + forward link + OEVT post by
    // `odly` seconds via the watchdog, holding the record active (PACT=1); when
    // `odly == 0` it calls `execOutput` immediately. `f32` mirrors the C
    // `float` field so a CA client sees DBR_FLOAT (not DBR_DOUBLE).
    pub odly: f32,
    pub out: String,
    pub prec: i16,
    // MDEL / ADEL (C `swaitRecord.dbd:477-486`, both DBF_DOUBLE): the monitor
    // and archive deadbands `monitor()` (swaitRecord.c:622-640) tests VAL
    // against to build `monitor_mask`. The record had neither field, so both
    // read back as the framework's 0.0 default and every VAL change crossed
    // both deadbands — and a client could not set them at all
    // (`FieldNotFound`). The A..L input posts inherit this mask, which is what
    // makes their DBE_LOG bit conditional (see `fields_posted_with_monitor_mask`).
    pub mdel: f64,
    pub adel: f64,
    // INxN: input link names; INxP: process passive flags (0/1)
    pub inp_names: [String; 12], // INAN..INLN
    pub inp_passive: [i16; 12],  // INAP..INLP
    // numeric input values A-L
    pub num_vals: [f64; 12],
    cached_should_output: bool,
    // ODLY delay state (C `cbStruct.outputWait`, an internal flag — swait has
    // no DLYA database field, unlike scalcout). `output_wait` marks that the
    // current `process()` call is the watchdog continuation re-entry; on it the
    // captured `pending_output` decision is restored so the framework writes
    // OUT + posts OEVT exactly once after the delay.
    output_wait: bool,
    pending_output: bool,
}

impl Default for SwaitRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            calc: String::new(),
            compiled_calc: None,
            oopt: 0,
            dopt: 0,
            doln: String::new(),
            dold: 0.0,
            oval: 0.0,
            oevt: 0,
            odly: 0.0,
            out: String::new(),
            prec: 0,
            mdel: 0.0,
            adel: 0.0,
            inp_names: Default::default(),
            inp_passive: [0; 12],
            num_vals: [0.0; 12],
            cached_should_output: true,
            output_wait: false,
            pending_output: false,
        }
    }
}

// Channel letters A-L in order
const CHAN: [char; 12] = ['A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L'];

impl SwaitRecord {
    /// C `swaitRecord.c:304,561` — swait compiles its CALC with the **numeric**
    /// `postfix()`, not `sCalcPostfix()`. Its grammar is therefore epics-base
    /// calc's: no SVAL, no string literals, no sCalc string functions.
    fn recompile(&mut self) {
        self.compiled_calc = calc_compile(&self.calc).ok();
    }

    /// Build the calc inputs. `prev_val` is the cell C passes as `presult`, which
    /// the `VAL` token (`FETCH_VAL`) pushes — for swait that is `&pwait->val`
    /// (C `swaitRecord.c:409`), i.e. the *previous* VAL.
    fn build_inputs(&self, prev_val: f64) -> NumericInputs {
        let mut inputs = NumericInputs::new();
        inputs.vars[..12].copy_from_slice(&self.num_vals[..12]);
        inputs.prev_val = prev_val;
        inputs
    }

    /// C `swaitRecord.c:425-450` — the OOPT switch, whose "old value" operand
    /// is `pwait->oval` (the previous cycle's VAL), passed in as `old` because
    /// C reads it BEFORE `:471` overwrites it with the new VAL.
    ///
    /// "On Change" is a MDEL-deadband test, not an inequality: C
    /// `swaitRecord.c:432` is `if (fabs(pwait->oval - pwait->val) > pwait->mdel)`,
    /// the same rule as calcout (`calcoutRecord.c:257`), sCalcout
    /// (`sCalcoutRecord.c:379`) and aCalcout (`aCalcoutRecord.c:318`). With the
    /// default MDEL=0 the two rules agree; a configured MDEL makes a sub-deadband
    /// change fire the OUT link on the port and not on C.
    fn eval_should_output(&self, old: f64) -> bool {
        match self.oopt {
            0 => true,
            1 => (old - self.val).abs() > self.mdel,
            2 => self.val == 0.0,
            3 => self.val != 0.0,
            4 => old != 0.0 && self.val == 0.0,
            5 => old == 0.0 && self.val != 0.0,
            _ => false,
        }
    }

    fn inp_name_index(name: &str) -> Option<usize> {
        // INxN: INAN, INBN, INCN, INDN, INEN, INFN, INGN, INHN, ININ, INJN, INKN, INLN
        let bytes = name.as_bytes();
        if bytes.len() == 4 && bytes[0] == b'I' && bytes[1] == b'N' && bytes[3] == b'N' {
            CHAN.iter().position(|&c| c == bytes[2] as char)
        } else {
            None
        }
    }

    fn inp_passive_index(name: &str) -> Option<usize> {
        // INxP: INAP, INBP, INCP, ...
        let bytes = name.as_bytes();
        if bytes.len() == 4 && bytes[0] == b'I' && bytes[1] == b'N' && bytes[3] == b'P' {
            CHAN.iter().position(|&c| c == bytes[2] as char)
        } else {
            None
        }
    }

    fn num_val_index(name: &str) -> Option<usize> {
        // Single letter A-L
        if name.len() == 1 {
            CHAN.iter().position(|&c| c.to_string() == name)
        } else {
            None
        }
    }
}

static SWAIT_FIELDS_SCALAR: &[FieldDesc] = &[
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
        name: "DOLN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DOLD",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "OVAL",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "OEVT",
        dbf_type: DbFieldType::UShort,
        read_only: false,
    },
    FieldDesc {
        name: "ODLY",
        dbf_type: DbFieldType::Float,
        read_only: false,
    },
    // OUT and OUTN are intentionally absent: both route to RecordInstance::common.out
    // via put_common_field so that parsed_out is populated for output dispatch.
    // OUTN is swait's output link field name; RecordInstance handles the alias.
    FieldDesc {
        name: "PREC",
        dbf_type: DbFieldType::Short,
        read_only: false,
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
        name: "INAN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INBN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INCN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INDN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INEN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INFN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INGN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INHN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "ININ",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INJN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INKN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INLN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INAP",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "INBP",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "INCP",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "INDP",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "INEP",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "INFP",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "INGP",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "INHP",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "INIP",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "INJP",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "INKP",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "INLP",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

/// Choice labels for the `swait` output-execute-option menu, in index
/// order. C `menu(swaitOOPT)` (`swaitRecord.dbd`): the six `longoutOOPT`
/// choices plus a trailing "Never" (index 6) that suppresses output.
const SWAIT_OOPT_CHOICES: &[&str] = &[
    "Every Time",
    "On Change",
    "When Zero",
    "When Non-zero",
    "Transition To Zero",
    "Transition To Non-zero",
    "Never",
];

/// Choice labels for the `swait` output-data-option menu, in index order.
/// C `menu(swaitDOPT)` (`swaitRecord.dbd`): 0="Use VAL" (the calculated
/// result), 1="Use DOL" (the value fetched through the `DOL` link).
const SWAIT_DOPT_CHOICES: &[&str] = &["Use VAL", "Use DOL"];

impl Record for SwaitRecord {
    fn record_type(&self) -> &'static str {
        "swait"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        SWAIT_FIELDS_SCALAR
    }

    /// Record-specific `DBF_MENU` fields, served as `DBR_ENUM` with the
    /// menu's choice labels in `.dbd` index order (`swaitRecord.dbd`):
    /// `OOPT` is `menu(swaitOOPT)`, `DOPT` is `menu(swaitDOPT)`.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "OOPT" => Some(SWAIT_OOPT_CHOICES),
            "DOPT" => Some(SWAIT_DOPT_CHOICES),
            _ => None,
        }
    }

    fn uses_monitor_deadband(&self) -> bool {
        true
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            self.recompile();
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // ODLY continuation: this is the watchdog re-process scheduled by a
        // previous cycle (C `swaitRecord.c::process` `if (pact && outputWait)
        // execOutput`, line 394). Do NOT re-evaluate CALC / OOPT — C runs
        // `execOutput` directly. Restore the captured output decision so the
        // framework writes the OUT link + posts OEVT this cycle, then clear the
        // wait flag. Mirrors scalcout's `dlya == 1` branch (swait has no DLYA
        // field — the wait state is the internal `output_wait` flag, as C uses
        // `cbStruct.outputWait`).
        if self.output_wait {
            self.output_wait = false;
            self.cached_should_output = self.pending_output;
            self.pending_output = false;
            return Ok(ProcessOutcome::complete());
        }

        // C `swaitRecord.c:432-446` compares against `pwait->oval`, which still
        // holds the PREVIOUS cycle's VAL at this point; `:471` advances it after
        // the OOPT decision.
        let old_val = self.oval;

        if let Some(ref compiled) = self.compiled_calc {
            let mut inputs = self.build_inputs(self.val);
            // C `swaitRecord.c:409` — `calcPerform(&pwait->a, &pwait->val,
            // pwait->rpcl)`: the numeric engine, whose result is a double.
            if let Ok(v) = calc_eval(compiled, &mut inputs) {
                self.val = v;
            }
        }

        // Cache before framework calls should_output() via trait dispatch.
        self.cached_should_output = self.eval_should_output(old_val);
        // C `swaitRecord.c:471`: `pwait->oval = pwait->val;` — unconditional,
        // after the OOPT test. OVAL is the old-value tracker, NOT the output
        // value: `execOutput` composes the output from VAL/DOLD at write time
        // (see `output_link_value`).
        self.oval = self.val;

        // ODLY (C `swaitRecord.c::schedOutput`, lines 719-729): when output
        // should fire and ODLY > 0, defer ONLY the OUT write + OEVT + forward
        // link by ODLY seconds via the watchdog, holding the record active
        // (C keeps PACT=1). The value side (VAL + changed inputs + alarm fields)
        // is NOT deferred: C `process` calls `monitor()` (line 475) on THIS
        // (delay-start) cycle, before returning async — only `execOutput`
        // (delay-end) is delayed, and it posts no monitors. The delaying cycle
        // captures the output decision and suppresses this cycle's OUT/OEVT
        // (`cached_should_output = false`), then re-processes after the delay;
        // the `output_wait` branch above emits the output exactly once.
        if self.cached_should_output && self.odly > 0.0 {
            self.pending_output = self.cached_should_output;
            self.output_wait = true;
            self.cached_should_output = false;
            let delay = std::time::Duration::from_secs_f64(self.odly as f64);
            // `CompleteDeferOutput`, NOT bare `AsyncPending`: swait posts the
            // value side at the START of the delay (C `monitor()` at line 475,
            // reached because `schedOutput` set `async=TRUE` but `process` falls
            // through to `monitor()` before the `if(!async)` forward-link tail).
            // The framework therefore runs its full monitor epilogue this cycle
            // (VAL with MDEL/ADEL deadband + alarm mask, changed inputs) and
            // defers only the OUT/OEVT/FLNK tail, holding PACT for the watchdog
            // window via the `ReprocessAfter` continuation that releases it —
            // matching C `swaitRecord.c:716` "THE RECORD REMAINS ACTIVE WHILE
            // WAITING ON THE WATCHDOG". A bare `AsyncPending` would have deferred
            // the value side to delay-end too (the calcout/scalcout/acalcout
            // shape, whose C `process` returns BEFORE `monitor()`); swait's C
            // does not, so VAL must post now.
            return Ok(ProcessOutcome {
                result: RecordProcessResult::CompleteDeferOutput,
                actions: vec![ProcessAction::ReprocessAfter(delay)],
                device_did_compute: false,
            });
        }

        Ok(ProcessOutcome::complete())
    }

    fn should_output(&self) -> bool {
        self.cached_should_output
    }

    /// C `swaitRecord.c::execOutput` (761-774): the value written to OUT is
    /// composed at output time, not staged during `process()` —
    /// `outValue = pwait->dopt ? pwait->dold : pwait->val`. With DOPT="Use DOL"
    /// the DOLD it reads is the one the framework just refreshed from the DOL
    /// link (see [`Record::output_time_input_links`]).
    fn output_link_value(&self) -> Option<EpicsValue> {
        Some(EpicsValue::Double(if self.dopt == 1 {
            self.dold
        } else {
            self.val
        }))
    }

    /// DOL is read at OUTPUT time, and only under DOPT="Use DOL" — C
    /// `execOutput` (763-772) guards the `recDynLinkGet` with
    /// `if (pwait->dopt)`. With DOPT="Use VAL" the link is never read, so DOLD
    /// keeps its client-put value.
    fn output_time_input_links(&self) -> &'static [(&'static str, &'static str)] {
        if self.dopt == 1 {
            &[("DOLN", "DOLD")]
        } else {
            &[]
        }
    }

    /// C `execOutput` posts the refreshed DOLD with `DBE_VALUE` alone
    /// (swaitRecord.c:770), not the framework default `DBE_VALUE | DBE_LOG`.
    fn value_only_change_fields(&self) -> &'static [&'static str] {
        &["DOLD"]
    }

    /// C `swaitRecord.c::monitor` (646-653) posts a changed input A..L with
    /// `monitor_mask | DBE_VALUE` — no forced `DBE_LOG`:
    ///
    /// ```c
    /// for (i=0, pnew=&pwait->a, pprev=&pwait->la; i<MAX_FIELDS; i++, pnew++, pprev++) {
    ///     if (*pnew != *pprev) {
    ///         db_post_events(pwait, pnew, monitor_mask|DBE_VALUE);
    ///         ...
    /// ```
    ///
    /// so an archiver subscribed `DBE_LOG` to `swait.A` is sent a value only on
    /// a cycle where VAL's own ADEL deadband crossed (which puts `DBE_LOG` into
    /// `monitor_mask`), not on every change. `calcRecord.c:420` writes
    /// `monitor_mask | DBE_VALUE | DBE_LOG` in the same loop and keeps the
    /// framework default.
    fn fields_posted_with_monitor_mask(&self) -> &'static [&'static str] {
        &["A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L"]
    }

    /// `OEVT` ("Output Event"): post the numeric output event when output
    /// fires. C `swaitRecord.c` `execOutput` runs `if (pwait->oevt > 0)
    /// post_event((int)pwait->oevt);` (swaitRecord.c:797) right after the OUT
    /// write / forward link, on every cycle where output fires
    /// (`cached_should_output`). swait has no IVOA field, so — like its C —
    /// the post is never IVOA-suppressed. Stringified so the numeric event
    /// matches a `SCAN="Event"` record's `EVNT`.
    fn output_event(&self) -> Option<String> {
        if self.cached_should_output && self.oevt > 0 {
            Some(self.oevt.to_string())
        } else {
            None
        }
    }

    fn val(&self) -> Option<EpicsValue> {
        Some(EpicsValue::Double(self.val))
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "CALC" => Some(EpicsValue::String(self.calc.clone().into())),
            "OOPT" => Some(EpicsValue::Short(self.oopt)),
            "DOPT" => Some(EpicsValue::Short(self.dopt)),
            "DOLN" => Some(EpicsValue::String(self.doln.clone().into())),
            "DOLD" => Some(EpicsValue::Double(self.dold)),
            "OVAL" => Some(EpicsValue::Double(self.oval)),
            "OEVT" => Some(EpicsValue::UShort(self.oevt)),
            "ODLY" => Some(EpicsValue::Float(self.odly)),
            // OUTN is aliased to common.out via RecordInstance; not stored locally.
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            _ => {
                if let Some(idx) = Self::num_val_index(name) {
                    return Some(EpicsValue::Double(self.num_vals[idx]));
                }
                if let Some(idx) = Self::inp_name_index(name) {
                    return Some(EpicsValue::String(self.inp_names[idx].clone().into()));
                }
                if let Some(idx) = Self::inp_passive_index(name) {
                    return Some(EpicsValue::Short(self.inp_passive[idx]));
                }
                None
            }
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                self.val = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("VAL".into()))?;
            }
            "CALC" => {
                if let EpicsValue::String(s) = value {
                    self.calc = s.as_str_lossy().into_owned();
                    self.recompile();
                } else {
                    return Err(CaError::TypeMismatch("CALC".into()));
                }
            }
            "OOPT" => {
                if let EpicsValue::Short(v) = value {
                    self.oopt = v;
                }
            }
            "DOPT" => {
                if let EpicsValue::Short(v) = value {
                    self.dopt = v;
                }
            }
            "DOLN" => {
                if let EpicsValue::String(s) = value {
                    self.doln = s.as_str_lossy().into_owned();
                } else {
                    return Err(CaError::TypeMismatch("DOLN".into()));
                }
            }
            "DOLD" => {
                self.dold = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("DOLD".into()))?;
            }
            "OEVT" => {
                self.oevt = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("OEVT".into()))?
                    as u16;
            }
            "ODLY" => {
                self.odly = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("ODLY".into()))?
                    as f32;
            }
            // OUTN falls through to put_common_field which mirrors to common.out.
            "MDEL" => {
                self.mdel = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("MDEL".into()))?;
            }
            "ADEL" => {
                self.adel = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("ADEL".into()))?;
            }
            "PREC" => {
                if let EpicsValue::Short(v) = value {
                    self.prec = v;
                }
            }
            _ => {
                if let Some(idx) = Self::num_val_index(name) {
                    self.num_vals[idx] = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                } else if let Some(idx) = Self::inp_name_index(name) {
                    if let EpicsValue::String(s) = value {
                        self.inp_names[idx] = s.as_str_lossy().into_owned();
                    } else {
                        return Err(CaError::TypeMismatch(name.into()));
                    }
                } else if let Some(idx) = Self::inp_passive_index(name) {
                    if let EpicsValue::Short(v) = value {
                        self.inp_passive[idx] = v;
                    } else {
                        return Err(CaError::TypeMismatch(name.into()));
                    }
                } else {
                    return Err(CaError::FieldNotFound(name.to_string()));
                }
            }
        }
        Ok(())
    }

    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        &[
            ("INAN", "A"),
            ("INBN", "B"),
            ("INCN", "C"),
            ("INDN", "D"),
            ("INEN", "E"),
            ("INFN", "F"),
            ("INGN", "G"),
            ("INHN", "H"),
            ("ININ", "I"),
            ("INJN", "J"),
            ("INKN", "K"),
            ("INLN", "L"),
        ]
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;

    /// R9-74: OOPT="On Change" is `fabs(oval - val) > mdel`
    /// (C `swaitRecord.c:432`), not `val != oval`. A change that stays inside
    /// MDEL must not drive OUT; one that crosses it must.
    #[test]
    fn r9_74_swait_on_change_honours_mdel_deadband() {
        let mut rec = SwaitRecord::default();
        rec.put_field("CALC", EpicsValue::String("A".into()))
            .unwrap();
        rec.put_field("OOPT", EpicsValue::Short(1)).unwrap();
        rec.put_field("MDEL", EpicsValue::Double(2.0)).unwrap();

        rec.put_field("A", EpicsValue::Double(1.0)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, 1.0);
        assert!(
            !rec.should_output(),
            "|oval - val| = 1.0 is inside MDEL=2.0 — C does not schedule output"
        );

        rec.put_field("A", EpicsValue::Double(5.0)).unwrap();
        rec.process().unwrap();
        assert!(
            rec.should_output(),
            "|1.0 - 5.0| = 4.0 exceeds MDEL=2.0 — C schedules output"
        );
    }

    /// The CALC `VAL` token reads the *previous* VAL: C `swaitRecord.c:409`
    /// calls `sCalcPerform(&pwait->a, ..., &pwait->val, ...)`, so `FETCH_VAL`
    /// pushes `*presult` = the VAL from the last cycle. `CALC="VAL+A"` therefore
    /// accumulates instead of collapsing to `A` every cycle.
    #[test]
    fn r5_2_sibling_calc_val_token_reads_previous_val() {
        let mut rec = SwaitRecord::default();
        rec.put_field("CALC", EpicsValue::String("VAL+A".into()))
            .unwrap();
        rec.put_field("A", EpicsValue::Double(2.0)).unwrap();

        rec.process().unwrap();
        assert_eq!(rec.val, 2.0, "first cycle: 0 + A");
        rec.process().unwrap();
        assert_eq!(rec.val, 4.0, "second cycle: previous VAL (2) + A");
        rec.process().unwrap();
        assert_eq!(rec.val, 6.0, "third cycle: previous VAL (4) + A");
    }

    /// Boundary: a CALC with no `VAL` token is unaffected by the seeding — the
    /// result is still a pure function of the inputs.
    #[test]
    fn r5_2_sibling_calc_without_val_token_is_unchanged() {
        let mut rec = SwaitRecord::default();
        rec.put_field("CALC", EpicsValue::String("A+1".into()))
            .unwrap();
        rec.put_field("A", EpicsValue::Double(5.0)).unwrap();

        rec.process().unwrap();
        assert_eq!(rec.val, 6.0);
        rec.process().unwrap();
        assert_eq!(rec.val, 6.0, "no VAL token: no accumulation");
    }
}
