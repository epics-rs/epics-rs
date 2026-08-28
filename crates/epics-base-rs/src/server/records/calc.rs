use super::calc_compile;
use crate::error::{CaError, CaResult};
use crate::server::record::{InputFetchPolicy, ProcessOutcome, Record};
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
        Self::advance_prev(self.a, &mut self.la);
        Self::advance_prev(self.b, &mut self.lb);
        Self::advance_prev(self.c, &mut self.lc);
        Self::advance_prev(self.d, &mut self.ld);
        Self::advance_prev(self.e, &mut self.le);
        Self::advance_prev(self.f, &mut self.lf);
        Self::advance_prev(self.g, &mut self.lg);
        Self::advance_prev(self.h, &mut self.lh);
        Self::advance_prev(self.i, &mut self.li);
        Self::advance_prev(self.j, &mut self.lj);
        Self::advance_prev(self.k, &mut self.lk);
        Self::advance_prev(self.l, &mut self.ll);
        Self::advance_prev(self.m, &mut self.lm);
        Self::advance_prev(self.n, &mut self.ln);
        Self::advance_prev(self.o, &mut self.lo);
        Self::advance_prev(self.p, &mut self.lp);
        Self::advance_prev(self.q, &mut self.lq);
        Self::advance_prev(self.r, &mut self.lr);
        Self::advance_prev(self.s, &mut self.ls);
        Self::advance_prev(self.t, &mut self.lt);
        Self::advance_prev(self.u, &mut self.lu);
    }

    fn get_vars(&self) -> [f64; 21] {
        [
            self.a, self.b, self.c, self.d, self.e, self.f, self.g, self.h, self.i, self.j, self.k,
            self.l, self.m, self.n, self.o, self.p, self.q, self.r, self.s, self.t, self.u,
        ]
    }

    /// Land the calc pass's variable stores back in A..U — the inverse of
    /// [`Self::get_vars`], and the record's ONLY write-back of an engine var set.
    ///
    /// C needs no such step: `calcPerform(&prec->a, &prec->val, rpcl)` is handed
    /// a pointer INTO the record, so its store opcode (`calcPerform.c:101-123`,
    /// `parg[op - STORE_A] = *ptop--`) IS the field write. The engine here
    /// evaluates an owned copy, so `CALC="A:=A+1;A"` incremented a temporary and
    /// dropped it — VAL climbed while A stayed 0 forever.
    ///
    /// Applied on the failure path too: C's stores go into the record as the
    /// expression runs, so the ones a later failing operator did not reach still
    /// stand.
    fn apply_stores(&mut self, vars: &[f64; 21]) {
        [
            self.a, self.b, self.c, self.d, self.e, self.f, self.g, self.h, self.i, self.j, self.k,
            self.l, self.m, self.n, self.o, self.p, self.q, self.r, self.s, self.t, self.u,
        ] = *vars;
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
        let vars = self.get_vars();
        let mut inputs = crate::calc::NumericInputs::with_vars(vars);
        // C `calcPerform(&prec->a, &prec->val, rpcl)` passes `presult =
        // &val`, so the `VAL` token (`FETCH_VAL`, calcPerform.c:73-74)
        // pushes the *previous* VAL. Seed `prev_val` from the current
        // `self.val` before it is overwritten below; otherwise
        // `CALC="VAL+1"` reads 0 every cycle instead of incrementing.
        inputs.prev_val = self.val;
        let outcome = crate::calc::eval(&self.rpcl, &mut inputs);
        // The stores land BEFORE the result and before LA..LU advance: C writes
        // them through `&prec->a` during the perform, so `monitor()` sees the
        // stored A against the old LA and posts it, exactly as it does for an
        // input that changed.
        self.apply_stores(&inputs.vars);
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

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
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
            "INPA" => match value {
                EpicsValue::String(s) => {
                    self.inpa = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPA".into())),
            },
            "INPB" => match value {
                EpicsValue::String(s) => {
                    self.inpb = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPB".into())),
            },
            "INPC" => match value {
                EpicsValue::String(s) => {
                    self.inpc = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPC".into())),
            },
            "INPD" => match value {
                EpicsValue::String(s) => {
                    self.inpd = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPD".into())),
            },
            "INPE" => match value {
                EpicsValue::String(s) => {
                    self.inpe = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPE".into())),
            },
            "INPF" => match value {
                EpicsValue::String(s) => {
                    self.inpf = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPF".into())),
            },
            "INPG" => match value {
                EpicsValue::String(s) => {
                    self.inpg = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPG".into())),
            },
            "INPH" => match value {
                EpicsValue::String(s) => {
                    self.inph = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPH".into())),
            },
            "INPI" => match value {
                EpicsValue::String(s) => {
                    self.inpi = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPI".into())),
            },
            "INPJ" => match value {
                EpicsValue::String(s) => {
                    self.inpj = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPJ".into())),
            },
            "INPK" => match value {
                EpicsValue::String(s) => {
                    self.inpk = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPK".into())),
            },
            "INPL" => match value {
                EpicsValue::String(s) => {
                    self.inpl = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPL".into())),
            },
            "INPM" => match value {
                EpicsValue::String(s) => {
                    self.inpm = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPM".into())),
            },
            "INPN" => match value {
                EpicsValue::String(s) => {
                    self.inpn = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPN".into())),
            },
            "INPO" => match value {
                EpicsValue::String(s) => {
                    self.inpo = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPO".into())),
            },
            "INPP" => match value {
                EpicsValue::String(s) => {
                    self.inpp = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPP".into())),
            },
            "INPQ" => match value {
                EpicsValue::String(s) => {
                    self.inpq = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPQ".into())),
            },
            "INPR" => match value {
                EpicsValue::String(s) => {
                    self.inpr = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPR".into())),
            },
            "INPS" => match value {
                EpicsValue::String(s) => {
                    self.inps = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPS".into())),
            },
            "INPT" => match value {
                EpicsValue::String(s) => {
                    self.inpt = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPT".into())),
            },
            "INPU" => match value {
                EpicsValue::String(s) => {
                    self.inpu = s.as_str_lossy().into_owned();
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

    /// C `calcRecord.c:103`: every CONSTANT input link is loaded into its value
    /// field ONCE, at `init_record` (`recGblInitConstantLink(plink,
    /// DBF_DOUBLE, pvalue)`); `dbGetLink` then delivers nothing for it on
    /// every later process, so a client's `caput REC.A 99` stands.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        crate::server::record::seed_input_links(self.multi_input_links())
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
