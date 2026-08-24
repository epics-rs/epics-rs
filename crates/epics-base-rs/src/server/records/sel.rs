use crate::error::{CaError, CaResult};
use crate::server::record::{ProcessOutcome, Record};
use crate::types::EpicsValue;

/// Number of input signals A..L (C `selRecord.c::SEL_MAX`).
const SEL_MAX: usize = 12;

/// Choice labels for the SELM select-mechanism menu, in index order.
/// C `menu(selSELM)` (selRecord.dbd.pod:21-26): 0=Specified, 1=High Signal,
/// 2=Low Signal, 3=Median Signal.
const SELM_CHOICES: &[&str] = &["Specified", "High Signal", "Low Signal", "Median Signal"];

/// Sel (select) record — selects one of A-L based on SELM algorithm.
pub struct SelRecord {
    pub val: f64,
    pub selm: i16, // 0=Specified, 1=High, 2=Low, 3=Median
    /// Selection number. C declares SELN as `DBF_USHORT`
    /// (selRecord.dbd.pod:295): an unsigned 0..65535 index — `do_sel`
    /// rejects values ≥ SEL_MAX.
    pub seln: u16,
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
    /// MLST / ALST — C `selRecord.dbd`, both `DBF_DOUBLE`
    /// `special(SPC_NOMOD)`. `monitor()` deadbands VAL against them
    /// (selRecord.c:316,319). The record served neither, so the
    /// framework had no cell to remember the last posted value in and
    /// treated every cycle as the first one.
    pub mlst: f64,
    pub alst: f64,
    /// True when `do_sel` detected an out-of-range SELN in Specified
    /// mode — raises `SOFT_ALARM/INVALID` in `check_alarms`.
    soft_alarm: bool,
    /// Set by the framework (`set_fetch_gate_failed`) when this cycle's
    /// `fetch_values` failed — the LAST input link read, in any SELM, or the
    /// NVL read in `Specified`. C `selRecord.c::process` (113-115) runs
    /// `do_sel` only when `fetch_values` succeeds, so on failure VAL/UDF
    /// must freeze. Consumed (reset to false) each `process()` so the
    /// by-name dispatch path — which never reports a fetch outcome —
    /// never freezes spuriously.
    fetch_gate_failed: bool,
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
            mlst: 0.0,
            alst: 0.0,
            soft_alarm: false,
            fetch_gate_failed: false,
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

impl Record for SelRecord {
    /// C `selRecord.c::init_record` (:88-110) seeds the A..L inputs to NaN and
    /// returns; it never touches LALM, whose only writes are in
    /// `checkAlarms` (:270-302).
    fn seed_deadband_tracking(&mut self) {}

    fn record_type(&self) -> &'static str {
        "sel"
    }

    /// C `selRecord.c::do_sel` — pick VAL per SELM, updating SELN for
    /// the High/Low/Median algorithms and setting VAL=NaN (→ UDF) when
    /// no valid input exists or the selected input is undefined.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.soft_alarm = false;
        // C `selRecord.c::process` (113-115) wraps the WHOLE of `do_sel` in
        // `if (RTN_SUCCESS(fetch_values(prec)))` — one gate for every SELM,
        // not a Specified-mode special case. A failed read leaves VAL (and
        // UDF, which derives from it) and SELN at the previous cycle's value,
        // and raises none of `do_sel`'s own alarms. Consumed as a one-shot so
        // the by-name path, which files no fetch report, never freezes.
        if std::mem::take(&mut self.fetch_gate_failed) {
            return Ok(ProcessOutcome::complete());
        }
        let vals = self.get_values();
        match self.selm {
            0 => {
                // Specified: index by SELN. C `do_sel` raises
                // SOFT_ALARM/INVALID and returns (VAL unchanged) when
                // SELN >= SEL_MAX. SELN is `DBF_USHORT` (u16), so an
                // out-of-range client value lands as a large index.
                let idx = self.seln as usize;
                if idx >= SEL_MAX {
                    self.soft_alarm = true;
                } else {
                    // C: `prec->val = val;` then `udf = isnan(val)`.
                    self.val = vals[idx];
                }
            }
            1 => {
                // High Signal: max of non-NaN values; SELN = winning index.
                // C `selRecord.c:361-368` seeds `val = -epicsINF` and,
                // when every input is NaN, leaves `val` at -inf. `prec->
                // val = val` then assigns -inf and `prec->udf =
                // isnan(prec->val)` (selRecord.c:401-402) — `isnan(-inf)`
                // is FALSE, so UDF stays CLEAR. An all-NaN High selection
                // therefore yields VAL = -inf, not NaN.
                let mut val = f64::NEG_INFINITY;
                for (i, &v) in vals.iter().enumerate() {
                    if !v.is_nan() && val < v {
                        val = v;
                        self.seln = i as u16;
                    }
                }
                self.val = val;
            }
            2 => {
                // Low Signal: min of non-NaN values; SELN = winning index.
                // C `selRecord.c:369-377` seeds `val = epicsINF`; an
                // all-NaN selection leaves VAL = +inf with UDF CLEAR —
                // see the SELM=High branch above for the `isnan`
                // reasoning.
                let mut val = f64::INFINITY;
                for (i, &v) in vals.iter().enumerate() {
                    if !v.is_nan() && val > v {
                        val = v;
                        self.seln = i as u16;
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
                self.seln = count as u16;
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
        if common.udf != 0 {
            recgbl::rec_gbl_set_sevr(
                common,
                alarm_status::UDF_ALARM,
                AlarmSeverity::from_u16(common.udfs as u16),
            );
            return;
        }

        let val = self.val;
        let hyst = self.hyst;
        let lalm = self.lalm;

        // hihi
        let hhsv = AlarmSeverity::from_u16(self.hhsv as u16);
        if hhsv != AlarmSeverity::NoAlarm
            && (val >= self.hihi || (lalm == self.hihi && val >= self.hihi - hyst))
        {
            if recgbl::rec_gbl_set_sevr(common, alarm_status::HIHI_ALARM, hhsv) {
                self.lalm = self.hihi;
            }
            return;
        }
        // lolo
        let llsv = AlarmSeverity::from_u16(self.llsv as u16);
        if llsv != AlarmSeverity::NoAlarm
            && (val <= self.lolo || (lalm == self.lolo && val <= self.lolo + hyst))
        {
            if recgbl::rec_gbl_set_sevr(common, alarm_status::LOLO_ALARM, llsv) {
                self.lalm = self.lolo;
            }
            return;
        }
        // high
        let hsv = AlarmSeverity::from_u16(self.hsv as u16);
        if hsv != AlarmSeverity::NoAlarm
            && (val >= self.high || (lalm == self.high && val >= self.high - hyst))
        {
            if recgbl::rec_gbl_set_sevr(common, alarm_status::HIGH_ALARM, hsv) {
                self.lalm = self.high;
            }
            return;
        }
        // low
        let lsv = AlarmSeverity::from_u16(self.lsv as u16);
        if lsv != AlarmSeverity::NoAlarm
            && (val <= self.low || (lalm == self.low && val <= self.low + hyst))
        {
            if recgbl::rec_gbl_set_sevr(common, alarm_status::LOW_ALARM, lsv) {
                self.lalm = self.low;
            }
            return;
        }
        // out of alarm by at least hyst
        self.lalm = val;
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            // SELM is DBF_MENU (selRecord.dbd.pod:290): served as DBR_ENUM,
            // the choice labels come from menu_field_choices.
            "SELM" => Some(EpicsValue::Enum(self.selm as u16)),
            "SELN" => Some(EpicsValue::UShort(self.seln)),
            "NVL" => Some(EpicsValue::String(self.nvl.clone().into())),
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
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
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
            // SELM is DBF_MENU (selRecord.dbd.pod:290): a client put arrives
            // converted to Enum; internal callers may still pass a Short
            // index. Store the menu index either way.
            "SELM" => match value {
                EpicsValue::Enum(v) => {
                    self.selm = v as i16;
                    Ok(())
                }
                EpicsValue::Short(v) => {
                    self.selm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SELM".into())),
            },
            "SELN" => match value {
                // DBF_USHORT (selRecord.dbd.pod:295): client puts arrive
                // converted to UShort; internal SELL/NVL link reads still
                // pass a Short. Cast both into the unsigned field (C
                // DBF_USHORT truncation).
                EpicsValue::UShort(v) => {
                    self.seln = v;
                    Ok(())
                }
                EpicsValue::Short(v) => {
                    self.seln = v as u16;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SELN".into())),
            },
            "NVL" => match value {
                EpicsValue::String(s) => {
                    self.nvl = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("NVL".into())),
            },
            "INPA" | "INPB" | "INPC" | "INPD" | "INPE" | "INPF" | "INPG" | "INPH" | "INPI"
            | "INPJ" | "INPK" | "INPL" => match value {
                EpicsValue::String(s) => {
                    let s = s.as_str_lossy().into_owned();
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
            "HIHI" | "HIGH" | "LOW" | "LOLO" | "HYST" | "LALM" | "MLST" | "ALST" => {
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
                    "MLST" => self.mlst = v,
                    "ALST" => self.alst = v,
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

    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SELM" => Some(SELM_CHOICES),
            _ => None,
        }
    }

    /// C `selRecord.c:99-105`: `recGblInitConstantLink(&prec->nvl, DBF_USHORT,
    /// &prec->seln)` then the INPA..INPL loop. A constant NVL is what makes
    /// `field(NVL,"2")` select INPC — at init, once; `dbGetLink` delivers
    /// nothing for it at process, so a `caput REC.SELN` is not stomped.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        let mut seeds = vec![crate::server::record::ConstantInitLink::new("NVL", "SELN")];
        seeds.extend(crate::server::record::seed_input_links(
            self.multi_input_links(),
        ));
        seeds
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

    /// C `selRecord.c::fetch_values` (lines 421-431): in `Specified`
    /// (SELM==0) mode only `INP[SELN]` is read; `High`/`Low`/`Median` read
    /// every input to compare them. `selector` is the NVL-resolved SELN for
    /// this cycle (the framework reads NVL before the input fetch), else the
    /// record's current SELN. A SELN >= SEL_MAX fetches nothing — C
    /// bounds-checks before the input read.
    /// C `selRecord.c::fetch_values` returns the status of its LAST
    /// `dbGetLink` (`:433-436`), which `process` gates `do_sel` on.
    fn input_fetch_policy(&self) -> crate::server::record::InputFetchPolicy {
        crate::server::record::InputFetchPolicy::ReadAllGateOnLastFailure
    }

    fn select_input_links(
        &self,
        selector: Option<u16>,
    ) -> Option<Vec<(&'static str, &'static str)>> {
        if self.selm == 0 {
            let idx = selector.unwrap_or(self.seln) as usize;
            Some(
                self.multi_input_links()
                    .get(idx)
                    .map(|&pair| vec![pair])
                    .unwrap_or_default(),
            )
        } else {
            None
        }
    }

    /// C `selRecord.c::process` (113-115) gates `do_sel` on `fetch_values`
    /// success in EVERY mode. `fetch_values` (`:433-436`) assigns `status`
    /// unguarded on every pass and returns it, so the gate is the LAST link
    /// read: INPL's for High/Low/Median, the selected input's (or a failed
    /// NVL read) for Specified. A failed INPA therefore does NOT gate a High
    /// selection — that asymmetry is C's, from the overwritten `status`, and
    /// the port matches it rather than inventing an "any input failed" rule.
    fn set_fetch_gate_failed(&mut self, failed: bool) {
        self.fetch_gate_failed = failed;
    }
}
