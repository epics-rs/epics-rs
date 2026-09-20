use super::calc_compile;
use crate::error::{CaError, CaResult};
use crate::server::record::{
    AlarmLimit, AnalogAlarmInput, FieldSlot, InputFetchPolicy, ProcessOutcome, Record,
};
use crate::types::{EpicsValue, PvString};

/// Calc record — evaluates CALC expression with inputs A-U.
///
/// Matches epics-base PR #655 (12 → 21 inputs, A-L → A-U).
pub struct CalcRecord {
    pub val: f64,
    pub calc: String,
    // Display/engineering
    pub egu: PvString,
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
    inpa: String,
    inpb: String,
    inpc: String,
    inpd: String,
    inpe: String,
    inpf: String,
    inpg: String,
    inph: String,
    inpi: String,
    inpj: String,
    inpk: String,
    inpl: String,
    inpm: String,
    inpn: String,
    inpo: String,
    inpp: String,
    inpq: String,
    inpr: String,
    inps: String,
    inpt: String,
    inpu: String,
    /// Bit `i` set ⟺ `INP<i>` is non-empty. Written only by
    /// [`Self::set_inp_link`], the single writer of the 21 texts, so
    /// [`Record::set_input_link_slots`] answers off it without touching them.
    inp_set: u64,
    /// [`Record::input_links_generation`]: moved on by [`Self::set_inp_link`],
    /// the one writer of `INPA..INPU` (private fields — nothing else can
    /// store a text without going through it).
    inp_generation: u64,
    // Input values A-U
    pub vars: [f64; crate::calc::CALC_NARGS],
    // Previous values LA-LU (saved after each process)
    pub prev: [f64; crate::calc::CALC_NARGS],
    // This cycle's `calcPerform` outcome (C `calcRecord.c:121-123`). A per-cycle
    // fact, not record state: `check_alarms` — the owner of this record's alarm
    // transitions — consumes it, so it cannot outlive the cycle that set it.
    calc_alarm: bool,
    // This cycle's `fetch_values()` outcome, pushed by the framework through
    // `set_fetch_gate_failed`. C `calcRecord.c::process` (120) runs
    // `calcPerform` only `if (fetch_values(prec) == 0)`, so a failed input link
    // freezes VAL and UDF and raises no CALC_ALARM — while everything after the
    // calc (LA..LU advance, alarms, monitors, forward link) still runs.
    fetch_gate_failed: bool,
    // This cycle ran `calcPerform` and it SUCCEEDED — the one condition under
    // which C writes `prec->udf` (`calcRecord.c:124`, the `else` of the
    // `calcPerform` test, itself inside the `fetch_values` gate at `:120`).
    // Consumed by `check_alarms`, which owns the write; a cycle that never sets
    // it leaves UDF frozen, which is what the gated arms of C do.
    value_computed: bool,
    // Alarm-range time-constant filter (epics-base calcRecord.c::checkAlarms).
    // AFTC > 0 enables an exponential smoothing of the integer alarmRange
    // (1=Lolo..5=Hihi) so transient excursions don't immediately alarm.
    // AFVL is the filter accumulator state (sign encodes rounding hysteresis).
    pub aftc: f64,
    pub afvl: f64,
    // C `RPCL`. Always a program: an empty or uncompilable CALC carries C's
    // empty `END_EXPRESSION` postfix, which `calcPerform` refuses to run — the
    // record then alarms on every process. See [`calc_compile`].
    rpcl: crate::calc::CompiledExpr,
    // C `prec->name`, handed over at creation by `set_async_context`. Only the
    // record knows it, which is why C prints its bad-CALC report from
    // `init_record`/`special` and not from `postfix()`.
    name: Option<String>,
}

impl Default for CalcRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            calc: String::new(),
            egu: PvString::new(),
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
            inp_set: 0,
            inp_generation: 0,
            vars: [0.0; crate::calc::CALC_NARGS],
            prev: [0.0; crate::calc::CALC_NARGS],
            calc_alarm: false,
            fetch_gate_failed: false,
            value_computed: false,
            aftc: 0.0,
            afvl: 0.0,
            rpcl: crate::calc::CompiledExpr::empty(crate::calc::ExprKind::Numeric),
            name: None,
        }
    }
}

impl CalcRecord {
    /// Construct with a CALC expression, compiled. RPCL is a function of CALC,
    /// so a constructor that sets one must set the other — otherwise the record
    /// carries a CALC it has no program for, and `process` has to guess. (C has
    /// no such window: `init_record` compiles before the record can be
    /// processed.)
    pub fn new(calc: &str) -> Self {
        let mut rec = Self {
            calc: calc.to_string(),
            ..Default::default()
        };
        rec.rpcl = calc_compile::postfix("calc", "CALC", &rec.calc).program;
        rec
    }

    /// The record's own report of a CALC it could not compile
    /// (`calcRecord.c:105-110` from `init_record`, `:145-151` from `special`).
    /// Two errlog records, differing only in the `pmessage` C passes.
    ///
    /// This is the counter-example to "a refused `dbpf` is silent": `dbpf`
    /// prints nothing but its read-back, and the words the user sees come from
    /// the record. They must carry `prec->name`, which is why C prints them
    /// here rather than inside `postfix()` — and why `calc_compile` cannot.
    fn report_bad_calc(&self, pmessage: &str, why: &str) {
        // C `precord ? precord->name : "Unknown"`, reached here only if a
        // record compiled before `set_async_context` ran.
        let name = self.name.as_deref().unwrap_or("Unknown");
        // `S_db_badField` is `M_dbAccess|15`, positive, so C's `errSymLookup`
        // fills the slot (`dbAccessDefs.h:184`).
        crate::server::recgbl::rec_gbl_record_error("Illegal field value", name, pmessage);
        crate::runtime::log::errlog_printf(&format!(
            "{name}.CALC: {why} in expression \"{}\"\n",
            self.calc
        ));
    }

    /// C `calcRecord.c::monitor`: advance the `LX` previous-value field
    /// only when the input `X` actually changed since the last post.
    fn advance_prev(new: f64, prev: &mut f64) {
        if new != *prev {
            *prev = new;
        }
    }

    /// Advance LA..LU to A..U. C `calcRecord.c::monitor` (lines 417-423) does
    /// it inside the per-field change test
    /// (`if (*pnew != *pprev || monitor_mask & DBE_ALARM)`), i.e. only for
    /// inputs that actually changed — so LA..LU means "value of the input as of
    /// the last time a monitor was posted for it".
    ///
    /// `monitor()` runs on EVERY cycle, including one where `fetch_values()`
    /// failed and the calc was skipped (C gates only the `calcPerform` block,
    /// calcRecord.c:119-125), so both paths through `process()` come through
    /// here.
    fn advance_prev_inputs(&mut self) {
        for (new, prev) in self.vars.iter().zip(self.prev.iter_mut()) {
            Self::advance_prev(*new, prev);
        }
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

    fn inp_link_mut(&mut self, idx: usize) -> Option<&mut String> {
        match idx {
            0 => Some(&mut self.inpa),
            1 => Some(&mut self.inpb),
            2 => Some(&mut self.inpc),
            3 => Some(&mut self.inpd),
            4 => Some(&mut self.inpe),
            5 => Some(&mut self.inpf),
            6 => Some(&mut self.inpg),
            7 => Some(&mut self.inph),
            8 => Some(&mut self.inpi),
            9 => Some(&mut self.inpj),
            10 => Some(&mut self.inpk),
            11 => Some(&mut self.inpl),
            12 => Some(&mut self.inpm),
            13 => Some(&mut self.inpn),
            14 => Some(&mut self.inpo),
            15 => Some(&mut self.inpp),
            16 => Some(&mut self.inpq),
            17 => Some(&mut self.inpr),
            18 => Some(&mut self.inps),
            19 => Some(&mut self.inpt),
            20 => Some(&mut self.inpu),
            _ => None,
        }
    }

    /// The single writer of `INPA..INPU`: stores the text and keeps `inp_set`
    /// in step. A slot past `INPU` is ignored, as [`Self::get_inp_link`]
    /// ignores it on the read side.
    pub fn set_inp_link(&mut self, slot: usize, text: impl Into<String>) {
        let Some(link) = self.inp_link_mut(slot) else {
            return;
        };
        *link = text.into();
        let wired = !link.is_empty();
        let bit = 1u64 << slot;
        if wired {
            self.inp_set |= bit;
        } else {
            self.inp_set &= !bit;
        }
        self.inp_generation += 1;
    }

    fn put_inp_link(
        &mut self,
        slot: usize,
        field: &'static str,
        value: EpicsValue,
    ) -> CaResult<()> {
        match value {
            EpicsValue::String(s) => {
                self.set_inp_link(slot, s.as_str_lossy());
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(field.into())),
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
        if let Some(slot) = self.vars.get_mut(idx) {
            *slot = val;
        }
    }
}

/// `A`..`U` → 0..21, the index C's `calcPerform` uses into `&prec->a`.
fn var_index(name: &str) -> Option<usize> {
    match name.as_bytes() {
        [c @ b'A'..=b'U'] => Some(usize::from(c - b'A')),
        _ => None,
    }
}

/// [`Record::field_slot`]'s index for `VAL`, past the `A`..`U` and
/// `LA`..`LU` blocks.
const VAL_SLOT: usize = 2 * crate::calc::CALC_NARGS;

/// `LA`..`LU` → 0..21, the same index into the previous-value block.
fn prev_index(name: &str) -> Option<usize> {
    match name.as_bytes() {
        [b'L', c @ b'A'..=b'U'] => Some(usize::from(c - b'A')),
        _ => None,
    }
}

impl Record for CalcRecord {
    /// C `calcRecord.c::init_record` (:90-114) ends without touching
    /// MLST/ALST/LALM — `sub` and `calcout`, the two records closest to it,
    /// both do seed (`subRecord.c:130-132`, `calcoutRecord.c:217-219`), so
    /// this is per-type and not derivable from the record's shape.
    fn seed_deadband_tracking(&mut self) {}

    fn record_type(&self) -> &'static str {
        "calc"
    }

    /// `calcRecord.c:161-167` `get_linkNumber` — `A`..`U` and `LA`..`LU` both
    /// read their units/precision/graphic/alarm from `INPA`..`INPU`.
    fn link_backed_metadata_field(&self, field: &str) -> Option<String> {
        crate::server::record::calc_class_link_backed_metadata_field(field)
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            // C `calcRecord.c::init_record:105-110` — postfix() into RPCL; a
            // failure is logged (errlog + recGblRecordError) but does NOT abort
            // the record's init (`return 0`). Only `special()` refuses.
            //
            // Unconditional, exactly as in C: an empty CALC is `CALC_ERR_NULL_ARG`
            // there, and the empty program it leaves in RPCL is what makes the
            // record alarm on every process. Skipping the compile for an empty
            // CALC left the port with no program and no alarm.
            let compiled = calc_compile::postfix(self.record_type(), "CALC", &self.calc);
            if let Some(why) = compiled.error_str() {
                self.report_bad_calc("calc: init_record: Illegal CALC field", why);
            }
            self.rpcl = compiled.program;
            if !self.calc.is_empty() {
                self.mlst = self.val;
                self.alst = self.val;
                self.lalm = self.val;
            }
        }
        Ok(())
    }

    /// C `calcRecord.c::special` (lines 139-155). `SPC_CALC` re-compiles RPCL
    /// from the CALC string `dbPut` has already stored, and on failure returns
    /// `S_db_badField` — so the client's write FAILS while the bad expression
    /// stays stored and RPCL is left empty. calcout/scalcout/acalcout make the
    /// opposite choice (store the status in CLCV, accept the put); both
    /// dispositions run off the one compile owner, `calc_compile`.
    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after || !field.eq_ignore_ascii_case("CALC") {
            return Ok(());
        }
        let compiled = calc_compile::postfix(self.record_type(), "CALC", &self.calc);
        let why = compiled.error_str();
        self.rpcl = compiled.program;
        if let Some(why) = why {
            self.report_bad_calc("calc: Illegal CALC field", why);
            return Err(CaError::BadField("calc: Illegal CALC field".into()));
        }
        Ok(())
    }

    /// C hands the record its own name at `dbDefineRecord`; the port hands it
    /// over here. `special()` needs it to name the PV it is refusing.
    fn set_async_context(&mut self, name: String, _db: crate::server::database::AsyncDbHandle) {
        self.name = Some(name);
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `calcRecord.c::process` (119-125):
        //
        // ```c
        // if (fetch_values(prec) == 0) {
        //     if (calcPerform(&prec->a, &prec->val, prec->rpcl)) {
        //         recGblSetSevr(prec, CALC_ALARM, INVALID_ALARM);
        //     } else
        //         prec->udf = isnan(prec->val);
        // }
        // ```
        //
        // A failed input link skips the whole calc: VAL and UDF freeze at the
        // previous cycle's values and CALC_ALARM is neither raised nor cleared.
        // The rest of the cycle is NOT skipped — the LA..LU advance below, the
        // alarm check, the monitors and the forward link all still run, and the
        // inputs that did read still refresh (C's fetch loop does not abort).
        if self.fetch_gate_failed {
            self.advance_prev_inputs();
            return Ok(ProcessOutcome::complete());
        }

        // C `calcRecord.c:121-123` — `calcPerform` runs unconditionally, and a
        // -1 is CALC_ALARM/INVALID with VAL left at its previous value. RPCL is
        // always a program, so there is no "no expression" case to improvise
        // around: an empty or uncompilable CALC IS the empty program, and the
        // engine fails it every cycle.
        // C `calcPerform(&prec->a, &prec->val, rpcl)`: the engine runs on the
        // record's own A..U, so a store opcode IS the field write and lands
        // before the result and before LA..LU advance — `monitor()` sees the
        // stored A against the old LA and posts it, as for an input that
        // changed. `presult = &val` makes the `VAL` token (`FETCH_VAL`,
        // calcPerform.c:73-74) push the *previous* VAL, seeded here from
        // `self.val` before it is overwritten below; otherwise `CALC="VAL+1"`
        // reads 0 every cycle instead of incrementing.
        let outcome = crate::calc::eval_in_place(&self.rpcl, &mut self.vars, self.val);
        match outcome {
            Ok(v) => {
                self.val = v;
                // C `:124` `else prec->udf = isnan(prec->val)` — this arm, and
                // only this arm, defines the record.
                self.value_computed = true;
            }
            Err(_) => self.calc_alarm = true,
        }
        self.advance_prev_inputs();

        // AFVL housekeeping — C `calcRecord.c::checkAlarms` always drives
        // AFVL to 0 when the alarm-range filter is inactive: on UDF
        // (line 302 `prec->afvl = 0`) and whenever `aftc <= 0` (the
        // local `afvl` stays 0 since the `aftc > 0` block is skipped, so
        // line 382 `prec->afvl = afvl` stores 0). The framework's AFTC
        // filter only *maintains* AFVL while `aftc > 0`; without this a
        // stale non-zero accumulator survives an AFTC→0 retune and would
        // mis-seed the filter if AFTC is later re-enabled.
        if self.aftc <= 0.0 || self.val.is_nan() {
            self.afvl = 0.0;
        }
        Ok(ProcessOutcome::complete())
    }

    /// C reads `prec->inpa..inpu` off the record and copies nothing; the generic
    /// `get_field` path hands back an owned `EpicsValue` per link, which is
    /// 21 clones on every cycle of a record that wires none of them.
    /// The framework's per-cycle cells, read as struct members — the
    /// defaults would each cost a full `get_field` name match. `calc`'s
    /// `get_field` has 32 four-character arms, so every one of these is a
    /// walk through that bucket on every scan cycle.
    ///
    /// `hyst` is `None` because `calc` declares no HYST
    /// (`calcRecord.dbd.pod`).
    fn analog_alarm_input(&self) -> Option<AnalogAlarmInput> {
        Some(AnalogAlarmInput {
            val: AlarmLimit::Double(self.val),
            hyst: None,
            lalm: Some(AlarmLimit::Double(self.lalm)),
        })
    }

    fn alarm_filter_cells(&self) -> Option<(f64, f64)> {
        Some((self.aftc, self.afvl))
    }

    fn store_alarm_filter_value(&mut self, afvl: f64) {
        self.afvl = afvl;
    }

    fn store_analog_lalm(&mut self, lalm: AlarmLimit) {
        self.lalm = lalm.as_f64();
    }

    fn val(&self) -> Option<EpicsValue> {
        Some(EpicsValue::Double(self.val))
    }

    fn monitor_deadband_value(&self) -> Option<f64> {
        Some(self.val)
    }

    /// `calc` declares no OVAL, so the default's `get_field("OVAL")` is a
    /// miss through the whole four-character bucket on every cycle.
    fn output_link_value(&self) -> Option<EpicsValue> {
        self.val()
    }

    fn monitor_deadband_cells(&self) -> crate::server::record::MonitorDeadbandCells {
        crate::server::record::MonitorDeadbandCells {
            mdel: Some(self.mdel),
            adel: Some(self.adel),
            mlst: Some(self.mlst),
            alst: Some(self.alst),
        }
    }

    fn store_monitor_last_posted(&mut self, val: f64, mlst: bool, alst: bool) {
        if mlst {
            self.mlst = val;
        }
        if alst {
            self.alst = val;
        }
    }

    fn link_text_ref(&self, link_field: &str) -> Option<&str> {
        // INPA..INPU differ in their last byte alone, so the cycle's 21 asks
        // cost one shape test and one indexed branch instead of 21 name
        // compares.
        let [b'I', b'N', b'P', slot] = *link_field.as_bytes() else {
            return None;
        };
        Some(match slot {
            b'A' => &self.inpa,
            b'B' => &self.inpb,
            b'C' => &self.inpc,
            b'D' => &self.inpd,
            b'E' => &self.inpe,
            b'F' => &self.inpf,
            b'G' => &self.inpg,
            b'H' => &self.inph,
            b'I' => &self.inpi,
            b'J' => &self.inpj,
            b'K' => &self.inpk,
            b'L' => &self.inpl,
            b'M' => &self.inpm,
            b'N' => &self.inpn,
            b'O' => &self.inpo,
            b'P' => &self.inpp,
            b'Q' => &self.inpq,
            b'R' => &self.inpr,
            b'S' => &self.inps,
            b'T' => &self.inpt,
            b'U' => &self.inpu,
            _ => return None,
        })
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        if let Some(i) = var_index(name) {
            return Some(EpicsValue::Double(self.vars[i]));
        }
        if let Some(i) = prev_index(name) {
            return Some(EpicsValue::Double(self.prev[i]));
        }
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "CALC" => Some(EpicsValue::String(self.calc.clone().into())),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "AFTC" => Some(EpicsValue::Double(self.aftc)),
            "AFVL" => Some(EpicsValue::Double(self.afvl)),
            "LALM" => Some(EpicsValue::Double(self.lalm)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            "INPA" => Some(EpicsValue::String(self.inpa.clone().into())),
            "INPB" => Some(EpicsValue::String(self.inpb.clone().into())),
            "INPC" => Some(EpicsValue::String(self.inpc.clone().into())),
            "INPD" => Some(EpicsValue::String(self.inpd.clone().into())),
            "INPE" => Some(EpicsValue::String(self.inpe.clone().into())),
            "INPF" => Some(EpicsValue::String(self.inpf.clone().into())),
            "INPG" => Some(EpicsValue::String(self.inpg.clone().into())),
            "INPH" => Some(EpicsValue::String(self.inph.clone().into())),
            "INPI" => Some(EpicsValue::String(self.inpi.clone().into())),
            "INPJ" => Some(EpicsValue::String(self.inpj.clone().into())),
            "INPK" => Some(EpicsValue::String(self.inpk.clone().into())),
            "INPL" => Some(EpicsValue::String(self.inpl.clone().into())),
            "INPM" => Some(EpicsValue::String(self.inpm.clone().into())),
            "INPN" => Some(EpicsValue::String(self.inpn.clone().into())),
            "INPO" => Some(EpicsValue::String(self.inpo.clone().into())),
            "INPP" => Some(EpicsValue::String(self.inpp.clone().into())),
            "INPQ" => Some(EpicsValue::String(self.inpq.clone().into())),
            "INPR" => Some(EpicsValue::String(self.inpr.clone().into())),
            "INPS" => Some(EpicsValue::String(self.inps.clone().into())),
            "INPT" => Some(EpicsValue::String(self.inpt.clone().into())),
            "INPU" => Some(EpicsValue::String(self.inpu.clone().into())),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if let Some(i) = var_index(name) {
            return match value.to_f64() {
                Some(f) => {
                    self.vars[i] = f;
                    Ok(())
                }
                None => Err(CaError::TypeMismatch(name.into())),
            };
        }
        match name {
            "VAL" => match value {
                EpicsValue::Double(v) => {
                    self.val = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            // C `dbPut` stores the string first and only then runs
            // `special(SPC_CALC)`, which is what re-compiles RPCL and decides
            // whether the put is accepted. `Self::special` owns both — a bad
            // expression must still be stored here (C stores it) so that
            // `caget calc.CALC` reads back what the client wrote.
            "CALC" => match value {
                EpicsValue::String(s) => {
                    self.calc = s.as_str_lossy().into_owned();
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
            "AFTC" => match value {
                EpicsValue::Double(v) => {
                    self.aftc = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "AFVL" => match value {
                EpicsValue::Double(v) => {
                    self.afvl = v;
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
            "INPA" => self.put_inp_link(0, "INPA", value),
            "INPB" => self.put_inp_link(1, "INPB", value),
            "INPC" => self.put_inp_link(2, "INPC", value),
            "INPD" => self.put_inp_link(3, "INPD", value),
            "INPE" => self.put_inp_link(4, "INPE", value),
            "INPF" => self.put_inp_link(5, "INPF", value),
            "INPG" => self.put_inp_link(6, "INPG", value),
            "INPH" => self.put_inp_link(7, "INPH", value),
            "INPI" => self.put_inp_link(8, "INPI", value),
            "INPJ" => self.put_inp_link(9, "INPJ", value),
            "INPK" => self.put_inp_link(10, "INPK", value),
            "INPL" => self.put_inp_link(11, "INPL", value),
            "INPM" => self.put_inp_link(12, "INPM", value),
            "INPN" => self.put_inp_link(13, "INPN", value),
            "INPO" => self.put_inp_link(14, "INPO", value),
            "INPP" => self.put_inp_link(15, "INPP", value),
            "INPQ" => self.put_inp_link(16, "INPQ", value),
            "INPR" => self.put_inp_link(17, "INPR", value),
            "INPS" => self.put_inp_link(18, "INPS", value),
            "INPT" => self.put_inp_link(19, "INPT", value),
            "INPU" => self.put_inp_link(20, "INPU", value),
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    /// C `calcRecord.c:103`: every CONSTANT input link is loaded into its value
    /// field ONCE, at `init_record` (`recGblInitConstantLink(plink,
    /// DBF_DOUBLE, pvalue)`); `dbGetLink` then delivers nothing for it on
    /// every later process, so a client's `caput REC.A 99` stands.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        crate::server::record::seed_input_links(self.multi_input_links())
    }

    /// Answered off `inp_set`, which [`Self::set_inp_link`] keeps in step with
    /// the 21 `INPA..INPU` texts, so the cycle touches none of them.
    /// See [`Record::set_input_link_slots`].
    fn set_input_link_slots(&self) -> Option<(u64, u64)> {
        Some((self.inp_set, 0))
    }

    fn input_links_generation(&self) -> Option<u64> {
        Some(self.inp_generation)
    }

    /// `A`..`U`, `LA`..`LU` and `VAL` — the fields a link reads — by
    /// index into the three `f64` blocks C lays them out in.
    fn field_slot(&self, field: &str) -> Option<FieldSlot> {
        let slot = if let Some(i) = var_index(field) {
            i
        } else if let Some(i) = prev_index(field) {
            crate::calc::CALC_NARGS + i
        } else if field == "VAL" {
            VAL_SLOT
        } else {
            return None;
        };
        Some(FieldSlot(slot as u16))
    }

    fn get_slot_f64(&self, slot: FieldSlot) -> Option<f64> {
        let i = usize::from(slot.0);
        if i < crate::calc::CALC_NARGS {
            Some(self.vars[i])
        } else if i < VAL_SLOT {
            Some(self.prev[i - crate::calc::CALC_NARGS])
        } else if i == VAL_SLOT {
            Some(self.val)
        } else {
            None
        }
    }

    fn put_slot_f64(&mut self, slot: FieldSlot, value: f64) -> bool {
        let i = usize::from(slot.0);
        if i < crate::calc::CALC_NARGS {
            self.vars[i] = value;
            true
        } else {
            false
        }
    }

    /// What `put_field` stores for `A`..`L`, without the three by-name
    /// lookups that precede it on the default path.
    fn put_multi_input_f64(&mut self, field: &'static str, value: f64) -> CaResult<()> {
        match var_index(field) {
            Some(i) => {
                self.vars[i] = value;
                Ok(())
            }
            None => self.put_field_internal(field, EpicsValue::Double(value)),
        }
    }

    fn multi_input_links(&self) -> &'static [(&'static str, &'static str)] {
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

    /// C `calcRecord.c::fetch_values` (427-443) reads every INP link and keeps
    /// the FIRST failing status; `process` (120) gates `calcPerform` on it.
    fn input_fetch_policy(&self) -> InputFetchPolicy {
        InputFetchPolicy::ReadAllGateOnFailure
    }

    fn set_fetch_gate_failed(&mut self, failed: bool) {
        self.fetch_gate_failed = failed;
    }

    /// C `calcRecord.c::process` writes `prec->udf` only inside the
    /// `fetch_values` gate AND only on the `calcPerform` success arm (`:120-124`),
    /// so a cycle whose input link failed, or whose CALC errored, leaves UDF at
    /// its previous value and keeps CALC_ALARM's INVALID standing alone. The
    /// framework's per-cycle blanket re-derived it from VAL on those cycles and
    /// reported a never-computed record as defined. The write lives on the
    /// success arm now — see [`Self::check_alarms`].
    fn clears_udf(&self) -> bool {
        false
    }

    /// C `calcRecord.c:121-123` — a failed `calcPerform` is
    /// `recGblSetSevr(prec, CALC_ALARM, INVALID_ALARM)`, raised in `process()`
    /// BEFORE `checkAlarms(prec)` runs its UDF guard (`:300-303`). So when a
    /// broken CALC leaves VAL undefined, C reports CALC_ALARM, not UDF_ALARM:
    /// `recGblSetSevr` is MAXIMIZE (strict `>`), and both are INVALID.
    ///
    /// Consuming the flag makes it a per-cycle fact: a cycle whose input fetch
    /// failed runs no `calcPerform` (`:120`) and therefore raises nothing — the
    /// stale flag used to re-raise CALC_ALARM on every gated cycle.
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        // C `calcRecord.c:124` — `prec->udf = isnan(prec->val)`, written by the
        // successful `calcPerform` and by nothing else. Applied here because
        // `check_alarms` is the record's only hook holding `CommonFields`, and
        // it runs before `recGblCheckUDF`, matching C's `process` → `checkAlarms`
        // order.
        if std::mem::take(&mut self.value_computed) {
            common.udf = self.value_is_undefined() as u8;
        }
        if std::mem::take(&mut self.calc_alarm) {
            // C `calcRecord.c:122` uses PLAIN `recGblSetSevr(prec, CALC_ALARM,
            // INVALID_ALARM)` — a NULL message (empty namsg). PVA then serves
            // the "CALC" condition string (iocsource.cpp:230-236), which is
            // exactly what pvxs QSRV2 serves. No fabricated amsg literal.
            crate::server::recgbl::rec_gbl_set_sevr(
                common,
                crate::server::recgbl::alarm_status::CALC_ALARM,
                crate::server::record::AlarmSeverity::Invalid,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `VAL` token in a CALC expression must read the *previous* result
    /// value (C `calcPerform` `FETCH_VAL` with `presult = &val`), so a
    /// self-referential `CALC="VAL+1"` counts up. Before the prev_val seed it
    /// read 0 every cycle and stuck at 1.
    #[test]
    fn calc_val_token_reads_previous_val() {
        let mut rec = CalcRecord::new("VAL+1");
        rec.init_record(0).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, 1.0);
        rec.process().unwrap();
        assert_eq!(rec.val, 2.0);
        rec.process().unwrap();
        assert_eq!(rec.val, 3.0);
    }
}
