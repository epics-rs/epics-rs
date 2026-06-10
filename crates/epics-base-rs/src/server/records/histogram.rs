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
    pub sgnl: f64,     // Signal value to bin (C: DBF_DOUBLE)
    pub cmd: i16,      // 0=Read, 1=Clear, 2=Start, 3=Stop
    pub csta: bool,    // Counting state — TRUE while counting is enabled
    pub sdel: f64,     // Signal deadband (monitor refresh period, seconds)
    pub mdel: i32,     // Monitor count deadband
    pub mcnt: i32,     // Counts accumulated since last monitor post
    // Simulation block (histogramRecord.dbd.pod:245-282). SIMM is
    // DBF_MENU menu(menuYesNo) (NO/YES), SIMS is DBF_MENU menu(menuAlarmSevr),
    // OLDSIMM is DBF_MENU menu(menuSimm) special(SPC_NOMOD) (read-only saved
    // copy); all served as DBR_ENUM shorts. SIOL/SIML are the simulation
    // input/mode links, SVAL the DBF_DOUBLE simulation value. SSCN
    // (menuScan) is served by the common-field path (common.sscn).
    pub simm: i16,
    pub siol: String,
    pub sval: f64,
    pub siml: String,
    pub sims: i16,
    pub oldsimm: i16,
}

impl Default for HistogramRecord {
    fn default() -> Self {
        let nelm = 10;
        let ulim = 10.0;
        let llim = 0.0;
        Self {
            val: vec![0; nelm as usize],
            nelm,
            ulim,
            llim,
            wdth: (ulim - llim) / nelm as f64,
            sgnl: 0.0,
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
            oldsimm: 0,
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
    // OLDSIMM is special(SPC_NOMOD) — the saved previous simulation mode,
    // not client-writable (histogramRecord.dbd.pod:270-274).
    FieldDesc {
        name: "OLDSIMM",
        dbf_type: DbFieldType::Short,
        read_only: true,
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

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `process` → `add_count(prec)` then `monitor`. The signal
        // is read from the input link by the framework before
        // process(); count it into the current bucket.
        self.add_count();
        Ok(ProcessOutcome::complete())
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
            "OLDSIMM" => Some(EpicsValue::Short(self.oldsimm)),
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
                // C `special` SPC_MOD on SGNL → add_count.
                self.add_count();
                Ok(())
            }
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
            // OLDSIMM is special(SPC_NOMOD) — saved copy, not client-writable.
            "NELM" | "WDTH" | "MCNT" | "OLDSIMM" => Err(CaError::ReadOnlyField(name.to_string())),
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

        // OLDSIMM is special(SPC_NOMOD) — readable, not writable.
        assert!(matches!(
            rec.put_field("OLDSIMM", EpicsValue::Short(1)),
            Err(CaError::ReadOnlyField(_))
        ));
        assert_eq!(rec.get_field("OLDSIMM"), Some(EpicsValue::Short(0)));

        // SIMS (menuAlarmSevr) and OLDSIMM (menuSimm) resolve centrally.
        let inst = RecordInstance::new("HIST:SIM".into(), rec);
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
