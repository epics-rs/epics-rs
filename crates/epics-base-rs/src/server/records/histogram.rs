use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, MENU_YES_NO, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Histogram record — counts values into buckets.
///
/// C `histogramRecord.c`: bucket counters are `epicsUInt32`
/// (`cvt_dbaddr` sets `dbr_field_type = DBF_ULONG`) and wrap
/// explicitly at `UINT_MAX` (`add_count`: `if (*pdest == UINT_MAX)
/// *pdest = 0; (*pdest)++;`).
///
/// The counters are stored in an `i32` vector — the public field type
/// is unchanged for callers — but each increment goes through
/// `u32`-wrapping arithmetic (`(v as u32).wrapping_add(1) as i32`).
/// The `i32` slot is treated as the two's-complement *bit container*
/// for the C `epicsUInt32`: this wraps at `UINT_MAX` exactly as C
/// does, and never panics on overflow the way a plain signed
/// `i32 += 1` would at 2^31 counts. The CA wire has no unsigned-long
/// DBR type, so `VAL` is exposed as `LongArray` — the same bit
/// pattern a C client sees over `DBR_LONG`.
pub struct HistogramRecord {
    pub val: Vec<i32>, // Bucket counts (C epicsUInt32 bit pattern, wraps at UINT_MAX)
    pub nelm: i32,     // Number of buckets
    pub ulim: f64,     // Upper limit
    pub llim: f64,     // Lower limit
    pub wdth: f64,     // Width of one bucket = (ulim-llim)/nelm
    pub sgnl: f64,     // Signal value to bin (C: DBF_DOUBLE, special(SPC_MOD))
    /// C `field(SVL,DBF_INLINK)` (`histogramRecord.dbd.pod:212`) — "Signal Value
    /// Location", the record's ONLY input link. `histogramRecord` declares NO
    /// INP: its soft device support reads SVL into SGNL
    /// (`devHistogramSoft.c:52` `dbGetLink(&prec->svl, DBR_DOUBLE, &prec->sgnl,
    /// 0, 0)` every process; `:44` `recGblInitConstantLink(&prec->svl,
    /// DBF_DOUBLE, &prec->sgnl)` at init), and `process()` bins whatever SGNL
    /// then holds. The port had no SVL at all and drove the record from a
    /// nonexistent INP, so `field(SVL,"MYSIG")` was dropped at load and the
    /// record was inert.
    pub svl: String,
    pub cmd: i16,   // 0=Read, 1=Clear, 2=Start, 3=Stop
    pub csta: bool, // Counting state — TRUE while counting is enabled
    pub sdel: f64,  // Signal deadband (monitor refresh period, seconds)
    pub mdel: i32,  // Monitor count deadband
    pub mcnt: i32,  // Counts accumulated since last monitor post
    // Simulation block (histogramRecord.dbd.pod:245-282). SIMM is
    // DBF_MENU menu(menuYesNo) (NO/YES), SIMS is DBF_MENU menu(menuAlarmSevr);
    // both served as DBR_ENUM shorts. SIOL/SIML are the simulation
    // input/mode links, SVAL the DBF_DOUBLE simulation value. SSCN (menuScan)
    // and OLDSIMM (menuSimm, SPC_NOMOD) are served by the common-field path
    // (`CommonFields::sscn` / `CommonFields::oldsimm`) — they are framework
    // state written only by the simulation-mode owner
    // (`RecordInstance::rec_gbl_save_simm` / `rec_gbl_check_simm`), never by
    // the record.
    pub simm: i16,
    pub siol: String,
    pub sval: f64,
    pub siml: String,
    pub sims: i16,
    pub sdly: f64,
    /// Did `init_record` pass 1 seed SGNL from a CONSTANT SVL? C
    /// `devHistogramSoft.c::init_record` (44-45): `if
    /// (recGblInitConstantLink(&prec->svl, DBF_DOUBLE, &prec->sgnl)) prec->udf =
    /// FALSE;`. UDF lives in the common fields, which `init_record` cannot
    /// reach, so the outcome is carried here and folded in by
    /// `post_init_finalize_undef` (the same shape aao uses for its constant DOL).
    constant_svl_loaded: bool,
}

impl Default for HistogramRecord {
    fn default() -> Self {
        // C `histogramRecord.dbd.pod` `field(NELM,DBF_USHORT){ initial("1") }`:
        // an unset NELM defaults to 1 bucket, not 10. NELM is read-only and
        // sizes VAL, so a .db that omits it must read back NELM=1 / a
        // 1-element VAL to match C. ULIM and LLIM carry NO C `initial(...)`
        // (`field(ULIM,DBF_DOUBLE)` / `field(LLIM,DBF_DOUBLE)`), so C defaults
        // both to 0.0 and `init_record` derives WDTH = (ULIM-LLIM)/NELM = 0.
        let nelm = 1;
        let ulim = 0.0;
        let llim = 0.0;
        Self {
            val: vec![0; nelm as usize],
            nelm,
            ulim,
            llim,
            wdth: (ulim - llim) / nelm as f64,
            sgnl: 0.0,
            svl: String::new(),
            cmd: 0,
            // C init leaves CSTA at its DBD default; the histogram
            // record counts by default (CSTA defaults TRUE) so a
            // freshly created record accumulates without an explicit
            // CMD=2 start.
            csta: true,
            sdel: 0.0,
            mdel: 0,
            mcnt: 0,
            simm: 0,
            siol: String::new(),
            sval: 0.0,
            siml: String::new(),
            sims: 0,
            sdly: -1.0,
            constant_svl_loaded: false,
        }
    }
}

impl HistogramRecord {
    pub fn new(nelm: i32, llim: f64, ulim: f64) -> Self {
        let n = nelm.max(1);
        Self {
            val: vec![0; n as usize],
            nelm,
            ulim,
            llim,
            wdth: (ulim - llim) / n as f64,
            ..Default::default()
        }
    }

    /// Recompute bucket width — C `special` SPC_RESET path
    /// (`prec->wdth = (ulim-llim)/nelm`).
    fn recompute_wdth(&mut self) {
        let n = self.nelm.max(1) as f64;
        self.wdth = (self.ulim - self.llim) / n;
    }

    /// Add one sample. Mirrors C `add_count` (histogramRecord.c:320-352):
    /// early-out when counting is stopped; reject out-of-range signal;
    /// pick the bucket with the closed-upper-edge `temp <= i*wdth`
    /// loop; wrap the `epicsUInt32` counter at `UINT_MAX`.
    pub fn add_count(&mut self) {
        if !self.csta {
            return;
        }
        if self.llim >= self.ulim || self.nelm <= 0 {
            return;
        }
        if self.sgnl < self.llim || self.sgnl >= self.ulim {
            return;
        }
        // C: temp = sgnl - llim; for (i=1; i<=nelm; i++) if (temp <= i*wdth) break;
        let temp = self.sgnl - self.llim;
        let mut i = 1i32;
        while i <= self.nelm {
            if temp <= i as f64 * self.wdth {
                break;
            }
            i += 1;
        }
        // C: pdest = bptr + i - 1. The loop can leave i == nelm+1 only
        // when rounding pushes temp past the last edge; clamp so the
        // index stays inside the buffer.
        let bucket = ((i - 1).max(0) as usize).min(self.val.len().saturating_sub(1));
        if bucket < self.val.len() {
            // C: if (*pdest == UINT_MAX) *pdest = 0; (*pdest)++;
            // The i32 slot holds the epicsUInt32 bit pattern — wrap
            // through u32 so the rollover happens at UINT_MAX.
            self.val[bucket] = (self.val[bucket] as u32).wrapping_add(1) as i32;
            self.mcnt = self.mcnt.saturating_add(1);
        }
    }

    /// Back-compat helper: set SGNL then count one sample.
    pub fn add_sample(&mut self, value: f64) {
        self.sgnl = value;
        self.add_count();
    }

    /// C `clear_histogram` — zero every bucket and arm a monitor post.
    fn clear_histogram(&mut self) {
        for v in &mut self.val {
            *v = 0;
        }
        self.mcnt = self.mdel + 1;
    }
}

static HISTOGRAM_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        // CA wire has no DBR_ULONG; C `cvt_dbaddr` uses DBF_ULONG
        // internally but a CA client reads it as DBR_LONG.
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "NELM",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "ULIM",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LLIM",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "WDTH",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "SGNL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    // C `field(SVL,DBF_INLINK)` (histogramRecord.dbd.pod:212) — the record's
    // only input link. Membership here is what makes `field(SVL,"MYSIG")` load:
    // `apply_fields` routes a name NOT in `field_list` to the common fields,
    // where it is rejected and skipped.
    FieldDesc {
        name: "SVL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CMD",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "CSTA",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SDEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "MDEL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "MCNT",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "SIMM",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SIOL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "SVAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "SIML",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "SIMS",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SDLY",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
];

/// Choice labels for the histogram command menu, in index order.
/// C `menu(histogramCMD)` (`histogramRecord.dbd.pod`): 0=Read, 1=Clear,
/// 2=Start, 3=Stop. Writing `CMD` triggers the corresponding action on the
/// collection buffer.
const HISTOGRAM_CMD_CHOICES: &[&str] = &["Read", "Clear", "Start", "Stop"];

impl Record for HistogramRecord {
    fn record_type(&self) -> &'static str {
        "histogram"
    }

    /// C `histogramRecord.c::process` NEVER clears UDF. The record's only two
    /// `prec->udf = FALSE` sites are `clear_histogram` (the CSTA/RESET special,
    /// `:361`) and the simulated SIOL read (`:387`) — so a histogram that has
    /// been counting for hours still reports UDF=1 (softIoc: `dbpf H1.PROC 1`
    /// → `H1.UDF` = 1, SEVR NO_ALARM, because histogram's `checkAlarms` has no
    /// UDF test either).
    fn clears_udf(&self) -> bool {
        false
    }

    /// `histogramRecord.c` has NO `checkAlarms` at all — its `process` goes
    /// straight from `readValue` to `monitor`, so the UDF=1 it carries raises
    /// nothing (softIoc: UDF=1, SEVR NO_ALARM, STAT NO_ALARM).
    fn raises_udf_alarm(&self) -> bool {
        false
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `process` → `add_count(prec)` then `monitor`. The signal
        // is read from the input link by the framework before
        // process(); count it into the current bucket.
        self.add_count();
        Ok(ProcessOutcome::complete())
    }

    /// `histogramRecord.dbd.pod` declares NO `INP` — the record's DBF_INLINK is
    /// `SVL` (:212), read into SGNL by `devHistogramSoft.c`. C's dbd rejects
    /// `field(INP,...)` on a histogram ("field not found"), and so does the
    /// common-field owner via this hook: a histogram must not be driveable from
    /// an INP the C record type does not have, or a `.db` written against the
    /// port is unloadable on a C IOC (and vice versa).
    fn declares_inp_link(&self) -> bool {
        false
    }

    /// C `devHistogramSoft.c::read_histogram` (50-54):
    /// `dbGetLink(&prec->svl, DBR_DOUBLE, &prec->sgnl, 0, 0)` on EVERY process.
    /// A CONSTANT (or unset) SVL is a no-op there — `dbConstGetValue` writes
    /// nothing — and was loaded once at init instead (see [`Self::init_record`]),
    /// so only a real link emits a fetch. Same gate as the aao's DOL.
    ///
    /// The value lands in SGNL through `put_field_internal`, NOT `put_field`:
    /// `dbGetLink` writes the field's memory directly and never runs C's
    /// `special()`, so the SPC_MOD `add_count` on a SGNL *caput* must not fire
    /// here — `process()` performs the cycle's single bin increment
    /// (`histogramRecord.c:218-219`).
    fn pre_input_link_actions(&mut self) -> Vec<crate::server::record::ProcessAction> {
        if crate::server::recgbl::simm::is_constant(&crate::server::record::parse_link_v2(
            &self.svl,
        )) {
            return Vec::new();
        }
        vec![crate::server::record::ProcessAction::ReadDbLink {
            link_field: "SVL",
            target_field: "SGNL",
        }]
    }

    /// Internal (link / framework) delivery of a field value — C's
    /// direct-to-memory write, which does NOT run `special()`. SGNL is the one
    /// field where that distinction is observable on a histogram: its
    /// `put_field` arm carries the SPC_MOD `add_count` (`histogramRecord.c:334`),
    /// and routing a link read through it would count the sample twice (once
    /// here, once in `process()`).
    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if name.eq_ignore_ascii_case("SGNL") {
            self.sgnl = value
                .to_f64()
                .ok_or_else(|| CaError::TypeMismatch("SGNL".into()))?;
            return Ok(());
        }
        crate::server::record::put_field_internal_default(self, name, value)
    }

    /// C `devHistogramSoft.c::init_record` (40-48): a CONSTANT SVL seeds SGNL
    /// once — `recGblInitConstantLink(&prec->svl, DBF_DOUBLE, &prec->sgnl)` —
    /// and the record is then DEFINED. No bin increment: `add_count` runs only
    /// from `process()` and from the SGNL `special()`.
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass != 1 {
            return Ok(());
        }
        let svl = crate::server::record::parse_link_v2(&self.svl);
        if let Some(v) = crate::server::recgbl::simm::constant_load_value(&svl)
            && let Some(f) = v.to_f64()
        {
            self.sgnl = f;
            self.constant_svl_loaded = true;
        }
        Ok(())
    }

    /// The UDF half of the constant-SVL load (`devHistogramSoft.c:44-45`):
    /// `if (recGblInitConstantLink(...)) prec->udf = FALSE;`. UDF is a common
    /// field, which `init_record` cannot reach.
    fn post_init_finalize_undef(&mut self, udf: &mut bool) -> CaResult<()> {
        if self.constant_svl_loaded {
            *udf = false;
        }
        Ok(())
    }

    /// C `histogramRecord.c:384-387` — the simulated SIOL read lands in SGNL,
    /// not in VAL (VAL is the bin-count array):
    ///
    /// ```c
    /// status = dbGetLink(&prec->siol, DBR_DOUBLE, &prec->sval, 0, 0);
    /// if (status == 0) {
    ///     prec->sgnl = prec->sval;
    ///     prec->udf = FALSE;
    /// }
    /// ```
    ///
    /// and `process()` (`:218-219`) then bins it: `if (status == 0)
    /// add_count(prec);`. The framework calls this only on that same
    /// `status == 0`, so the bin increment belongs here — the assignment is a
    /// plain store, NOT the `put_field("SGNL")` path, whose `add_count` is C's
    /// SPC_MOD `special()` hook (`:260-263`) and would count the sample twice.
    fn land_simulated_value(&mut self, value: EpicsValue) -> CaResult<()> {
        self.sgnl = value.to_f64().unwrap_or(0.0);
        self.add_count();
        Ok(())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            // Counters surfaced as-is (the i32 slot is the
            // epicsUInt32 bit pattern C exposes over DBR_LONG).
            "VAL" => Some(EpicsValue::LongArray(self.val.clone())),
            "NELM" => Some(EpicsValue::Long(self.nelm)),
            "ULIM" => Some(EpicsValue::Double(self.ulim)),
            "LLIM" => Some(EpicsValue::Double(self.llim)),
            "WDTH" => Some(EpicsValue::Double(self.wdth)),
            "SGNL" => Some(EpicsValue::Double(self.sgnl)),
            "SVL" => Some(EpicsValue::String(self.svl.clone().into())),
            "CMD" => Some(EpicsValue::Short(self.cmd)),
            "CSTA" => Some(EpicsValue::Short(if self.csta { 1 } else { 0 })),
            "SDEL" => Some(EpicsValue::Double(self.sdel)),
            "MDEL" => Some(EpicsValue::Long(self.mdel)),
            "MCNT" => Some(EpicsValue::Long(self.mcnt)),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SVAL" => Some(EpicsValue::Double(self.sval)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            "SDLY" => Some(EpicsValue::Double(self.sdly)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::LongArray(arr) => {
                    self.val = arr;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            "ULIM" => match value {
                EpicsValue::Double(v) => {
                    self.ulim = v;
                    // C SPC_RESET: recompute width and clear.
                    self.recompute_wdth();
                    self.clear_histogram();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("ULIM".into())),
            },
            "LLIM" => match value {
                EpicsValue::Double(v) => {
                    self.llim = v;
                    self.recompute_wdth();
                    self.clear_histogram();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("LLIM".into())),
            },
            "SGNL" => {
                self.sgnl = value.to_f64().unwrap_or(0.0);
                // C `special` SPC_MOD on SGNL → add_count. This is the
                // `dbPutField` path ONLY — the link/device deliveries of SGNL
                // (SVL read, SIOL simulation read) write `prec->sgnl` directly
                // and never run `special()`, so they go through
                // `put_field_internal` / `land_simulated_value` instead and let
                // `process()` do the single bin increment.
                self.add_count();
                Ok(())
            }
            "SVL" => match value {
                EpicsValue::String(s) => {
                    self.svl = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SVL".into())),
            },
            "CMD" => match value {
                EpicsValue::Short(v) => {
                    // C `special` SPC_CALC: cmd<=1 clear, cmd==2 start,
                    // cmd==3 stop; cmd is always reset to 0 afterwards.
                    self.cmd = v;
                    match v {
                        2 => self.csta = true,
                        3 => self.csta = false,
                        // <= 1 (Read / Clear) clears the histogram.
                        _ => self.clear_histogram(),
                    }
                    self.cmd = 0;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("CMD".into())),
            },
            "CSTA" => match value {
                EpicsValue::Short(v) => {
                    self.csta = v != 0;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("CSTA".into())),
            },
            "SDEL" => match value {
                EpicsValue::Double(v) => {
                    self.sdel = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SDEL".into())),
            },
            "MDEL" => match value {
                EpicsValue::Long(v) => {
                    self.mdel = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("MDEL".into())),
            },
            "SIMM" => match value {
                EpicsValue::Short(v) => {
                    self.simm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIMM".into())),
            },
            "SIOL" => match value {
                EpicsValue::String(s) => {
                    self.siol = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIOL".into())),
            },
            "SVAL" => match value {
                EpicsValue::Double(v) => {
                    self.sval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SVAL".into())),
            },
            "SIML" => match value {
                EpicsValue::String(s) => {
                    self.siml = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIML".into())),
            },
            "SIMS" => match value {
                EpicsValue::Short(v) => {
                    self.sims = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIMS".into())),
            },
            "SDLY" => match value {
                EpicsValue::Double(v) => {
                    self.sdly = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SDLY".into())),
            },
            // C `field(NELM,DBF_USHORT){ promptgroup special(SPC_NOMOD) }`:
            // settable at `.db` load (dbStaticLib bypasses SPC_NOMOD), runtime-
            // immutable. The runtime block is the field_io `read_only` gate (the
            // FieldDesc carries `read_only: true`), so a CA/PVA caput is still
            // rejected; this arm serves only the load path (`apply_fields`),
            // sizing the bin array exactly like `new()`.
            "NELM" => match value {
                EpicsValue::Long(n) => {
                    let n = n.max(1);
                    self.nelm = n as i32;
                    self.val = vec![0; n as usize];
                    self.recompute_wdth();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("NELM".into())),
            },
            // WDTH/MCNT are computed runtime fields (no promptgroup) — never
            // client-writable, even at load. (OLDSIMM is the SPC_NOMOD saved
            // copy, refused by the common-field path that owns it.)
            "WDTH" | "MCNT" => Err(CaError::ReadOnlyField(name.to_string())),
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        HISTOGRAM_FIELDS
    }

    /// `CMD` is `DBF_MENU menu(histogramCMD)` (`histogramRecord.dbd.pod`);
    /// served as `DBR_ENUM` with the Read/Clear/Start/Stop choice labels.
    /// `SIMM` is `DBF_MENU menu(menuYesNo)` (`histogramRecord.dbd.pod:258-262`),
    /// served as `DBR_ENUM` with the NO/YES labels.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "CMD" => Some(HISTOGRAM_CMD_CHOICES),
            "SIMM" => Some(MENU_YES_NO),
            _ => None,
        }
    }

    fn primary_field(&self) -> &'static str {
        "VAL"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // C `histogramRecord.dbd.pod` `field(NELM,DBF_USHORT){ initial("1") }` —
    // a histogram built without an explicit NELM defaults to 1 bucket (and a
    // 1-element VAL), not the old hand-coded 10.
    #[test]
    fn nelm_defaults_to_one_per_dbd_initial() {
        let rec = HistogramRecord::default();
        assert_eq!(rec.nelm, 1);
        assert_eq!(rec.val.len(), 1, "VAL is sized by NELM");
        assert_eq!(rec.get_field("NELM"), Some(EpicsValue::Long(1)));
    }

    #[test]
    fn ulim_llim_default_to_zero_per_absent_dbd_initial() {
        // C `field(ULIM,DBF_DOUBLE)` / `field(LLIM,DBF_DOUBLE)` carry no
        // `initial(...)`, so both default to 0.0 and `init_record` derives
        // WDTH = (ULIM-LLIM)/NELM = 0.
        let rec = HistogramRecord::default();
        assert_eq!(rec.ulim, 0.0, "ULIM has no C initial -> 0.0");
        assert_eq!(rec.llim, 0.0, "LLIM has no C initial -> 0.0");
        assert_eq!(rec.wdth, 0.0, "WDTH = (ULIM-LLIM)/NELM = 0");
    }

    /// a counter at the `UINT_MAX` bit pattern must wrap to 0,
    /// never panic the way a signed `i32 += 1` would at overflow.
    #[test]
    fn counter_wraps_at_u32_max_no_panic() {
        let mut rec = HistogramRecord::new(2, 0.0, 10.0);
        rec.val[0] = u32::MAX as i32; // -1 i32, == UINT_MAX bit pattern
        rec.sgnl = 1.0;
        rec.add_count();
        assert_eq!(rec.val[0], 0, "epicsUInt32 counter wraps UINT_MAX -> 0");
    }

    /// CMD=3 stops counting, CMD=2 resumes it.
    #[test]
    fn cmd_start_stop_pauses_counting() {
        let mut rec = HistogramRecord::new(2, 0.0, 10.0);
        rec.put_field("CMD", EpicsValue::Short(3)).unwrap(); // stop
        assert!(!rec.csta);
        rec.add_sample(1.0);
        assert_eq!(rec.val[0], 0, "stopped histogram must not count");
        rec.put_field("CMD", EpicsValue::Short(2)).unwrap(); // start
        assert!(rec.csta);
        rec.add_sample(1.0);
        assert_eq!(rec.val[0], 1, "started histogram counts again");
    }

    /// SIMM is `menu(menuYesNo)` served as DBR_ENUM with the NO/YES labels.
    #[test]
    fn simm_snapshot_is_enum_with_yesno_labels() {
        use crate::server::record::RecordInstance;
        let mut rec = HistogramRecord::new(2, 0.0, 10.0);
        rec.put_field("SIMM", EpicsValue::Short(1)).unwrap();
        assert_eq!(rec.get_field("SIMM"), Some(EpicsValue::Short(1)));
        let inst = RecordInstance::new("HIST:SIMM".into(), rec);
        let snap = inst.snapshot_for_field("SIMM").unwrap();
        assert_eq!(snap.value, EpicsValue::Enum(1));
        assert_eq!(snap.enums.as_ref().unwrap().strings, vec!["NO", "YES"]);
    }

    /// CMD<=1 clears all buckets.
    #[test]
    fn cmd_clear_zeros_buckets() {
        let mut rec = HistogramRecord::new(2, 0.0, 10.0);
        rec.add_sample(1.0);
        rec.add_sample(6.0);
        rec.put_field("CMD", EpicsValue::Short(1)).unwrap();
        assert_eq!(rec.val, vec![0, 0]);
    }

    /// closed-upper-edge bucket selection — a value exactly on an
    /// internal boundary lands in the lower bucket.
    #[test]
    fn bin_boundary_uses_closed_upper_edge() {
        let mut rec = HistogramRecord::new(2, 0.0, 10.0); // wdth = 5
        rec.add_sample(5.0); // temp=5 <= 1*5 → bucket 0
        assert_eq!(rec.val, vec![1, 0]);
        rec.add_sample(5.0001); // temp>5 → bucket 1
        assert_eq!(rec.val, vec![1, 1]);
    }

    /// Out-of-range signal is rejected (C `add_count` early return).
    #[test]
    fn out_of_range_signal_rejected() {
        let mut rec = HistogramRecord::new(2, 0.0, 10.0);
        rec.add_sample(-1.0);
        rec.add_sample(10.0); // >= ulim → rejected
        rec.add_sample(99.0);
        assert_eq!(rec.val, vec![0, 0]);
    }

    /// Simulation block (histogramRecord.dbd.pod:245-282) is served:
    /// SIML/SIOL links, SVAL (DBF_DOUBLE), SIMS (menuAlarmSevr) round-trip;
    /// OLDSIMM (menuSimm, SPC_NOMOD) is readable but not client-writable;
    /// SIMS/OLDSIMM labels come from the shared menu registry.
    #[test]
    fn sim_block_served_and_oldsimm_read_only() {
        use crate::server::record::RecordInstance;
        let mut rec = HistogramRecord::new(2, 0.0, 10.0);

        rec.put_field("SIML", EpicsValue::String("sim:mode".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("SIML"),
            Some(EpicsValue::String("sim:mode".into()))
        );
        rec.put_field("SIOL", EpicsValue::String("sim:in".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("SIOL"),
            Some(EpicsValue::String("sim:in".into()))
        );
        rec.put_field("SVAL", EpicsValue::Double(3.5)).unwrap();
        assert_eq!(rec.get_field("SVAL"), Some(EpicsValue::Double(3.5)));
        rec.put_field("SIMS", EpicsValue::Short(2)).unwrap();
        assert_eq!(rec.get_field("SIMS"), Some(EpicsValue::Short(2)));

        // OLDSIMM is special(SPC_NOMOD), and lives in the common fields (the
        // simulation-mode owner's latch) — readable, not client-writable.
        let mut inst = RecordInstance::new("HIST:SIM".into(), rec);
        assert!(matches!(
            inst.put_common_field("OLDSIMM", EpicsValue::Short(1)),
            Err(CaError::ReadOnlyField(_))
        ));
        assert_eq!(inst.get_common_field("OLDSIMM"), Some(EpicsValue::Short(0)));

        // SIMS (menuAlarmSevr) and OLDSIMM (menuSimm) resolve centrally.
        let sims = inst.snapshot_for_field("SIMS").unwrap();
        assert_eq!(
            sims.enums.as_ref().unwrap().strings,
            vec!["NO_ALARM", "MINOR", "MAJOR", "INVALID"]
        );
        let old = inst.snapshot_for_field("OLDSIMM").unwrap();
        assert_eq!(
            old.enums.as_ref().unwrap().strings,
            vec!["NO", "YES", "RAW"]
        );
    }
}
