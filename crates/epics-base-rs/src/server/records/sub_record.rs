use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, InputFetchPolicy, ProcessOutcome, Record, dbd_generated};
use crate::types::{EpicsValue, PvString};

/// Number of subroutine input arguments. C `subRecord.c`:
/// `#define INP_ARG_MAX 21` — fields `A..U` / `INPA..INPU`.
const INP_ARG_MAX: usize = 21;

/// The 21 input value field names `A..U`.
const VAL_NAMES: [&str; INP_ARG_MAX] = [
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
    "T", "U",
];

/// The 21 input link field names `INPA..INPU`.
const INP_NAMES: [&str; INP_ARG_MAX] = [
    "INPA", "INPB", "INPC", "INPD", "INPE", "INPF", "INPG", "INPH", "INPI", "INPJ", "INPK", "INPL",
    "INPM", "INPN", "INPO", "INPP", "INPQ", "INPR", "INPS", "INPT", "INPU",
];

/// (INP link, value field) pairs for the 21 channels.
const INP_VAL_PAIRS: [(&str, &str); INP_ARG_MAX] = [
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
];

/// Sub (subroutine) record — calls a named subroutine function on
/// process. C `subRecord.c` exposes 21 inputs `A..U` fed from links
/// `INPA..INPU`; the subroutine itself is invoked by the framework
/// (`RecordInstance::subroutine`).
pub struct SubRecord {
    pub val: f64,
    pub snam: PvString,
    /// Init-routine name `INAM` (C `subRecord.c::init_record`). When set, the
    /// framework resolves it through the function registry and invokes it
    /// exactly once at iocInit, before SNAM resolution. SPC_NOMOD: set at
    /// `.db` load, not runtime-settable by clients.
    pub inam: PvString,
    /// Input links `INPA..INPU`.
    pub inp: [String; INP_ARG_MAX],
    /// Input values `A..U`.
    pub a: [f64; INP_ARG_MAX],
    /// Monitor / archive deadbands and last-posted trackers. C
    /// `subRecord.c::monitor` (lines 386-394) gates the `VAL` post on the
    /// MDEL/ADEL deadbands against MLST/ALST, and `checkAlarms` (319-373)
    /// tracks LALM for the HIHI/HIGH/LOLO/LOW hysteresis. HIHI/HIGH/LOLO/LOW,
    /// the HxSV/LxSV severities and HYST are framework-common fields
    /// (`RecordInstance` allocates an `AnalogAlarmConfig` for `sub`); only
    /// these record-owned trackers live here, mirroring `calc`.
    pub mdel: f64,
    pub adel: f64,
    pub lalm: f64,
    pub mlst: f64,
    pub alst: f64,
    /// Bad-return severity (`menuAlarmSevr`, default NO_ALARM). C
    /// `subRecord.c::do_sub` raises `SOFT_ALARM` at this severity when the
    /// subroutine returns a negative status (applied by the framework's
    /// `run_registered_subroutine`).
    pub brsv: i16,
}

impl Default for SubRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            snam: PvString::new(),
            inam: PvString::new(),
            inp: std::array::from_fn(|_| String::new()),
            a: [0.0; INP_ARG_MAX],
            mdel: 0.0,
            adel: 0.0,
            lalm: 0.0,
            mlst: 0.0,
            alst: 0.0,
            brsv: 0,
        }
    }
}

static SUB_FIELDS: &[FieldDesc] = dbd_generated::SUB_FIELDS;

impl Record for SubRecord {
    fn record_type(&self) -> &'static str {
        "sub"
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            // C `subRecord.c::init_record` (lines 130-132) seeds the
            // monitor/archive/alarm trackers from the loaded VAL so the
            // first process does not post or alarm on an unchanged value.
            self.mlst = self.val;
            self.alst = self.val;
            self.lalm = self.val;
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // The subroutine is invoked by the framework via
        // `RecordInstance::subroutine` (it needs the registry of
        // named functions, which the record does not own).
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => return Some(EpicsValue::Double(self.val)),
            "SNAM" => return Some(EpicsValue::String(self.snam.clone())),
            "INAM" => return Some(EpicsValue::String(self.inam.clone())),
            "MDEL" => return Some(EpicsValue::Double(self.mdel)),
            "ADEL" => return Some(EpicsValue::Double(self.adel)),
            "LALM" => return Some(EpicsValue::Double(self.lalm)),
            "MLST" => return Some(EpicsValue::Double(self.mlst)),
            "ALST" => return Some(EpicsValue::Double(self.alst)),
            "BRSV" => return Some(EpicsValue::Short(self.brsv)),
            _ => {}
        }
        if let Some(idx) = INP_NAMES.iter().position(|&n| n == name) {
            return Some(EpicsValue::String(self.inp[idx].clone().into()));
        }
        if let Some(idx) = VAL_NAMES.iter().position(|&n| n == name) {
            return Some(EpicsValue::Double(self.a[idx]));
        }
        None
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                return match value {
                    EpicsValue::Double(v) => {
                        self.val = v;
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch("VAL".into())),
                };
            }
            "SNAM" => {
                return match value {
                    EpicsValue::String(s) => {
                        self.snam = s;
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch("SNAM".into())),
                };
            }
            "INAM" => {
                return match value {
                    EpicsValue::String(s) => {
                        self.inam = s;
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch("INAM".into())),
                };
            }
            "MDEL" | "ADEL" | "LALM" | "MLST" | "ALST" => {
                let v = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                match name {
                    "MDEL" => self.mdel = v,
                    "ADEL" => self.adel = v,
                    "LALM" => self.lalm = v,
                    "MLST" => self.mlst = v,
                    "ALST" => self.alst = v,
                    _ => unreachable!(),
                }
                return Ok(());
            }
            "BRSV" => {
                self.brsv = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("BRSV".into()))?
                    as i16;
                return Ok(());
            }
            _ => {}
        }
        if let Some(idx) = INP_NAMES.iter().position(|&n| n == name) {
            return match value {
                EpicsValue::String(s) => {
                    self.inp[idx] = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            };
        }
        if let Some(idx) = VAL_NAMES.iter().position(|&n| n == name) {
            self.a[idx] = value
                .to_f64()
                .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
            return Ok(());
        }
        Err(CaError::FieldNotFound(name.to_string()))
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        SUB_FIELDS
    }

    /// C `subRecord.c:104`: every CONSTANT input link is loaded into its value
    /// field ONCE, at `init_record` (`recGblInitConstantLink(plink,
    /// DBF_DOUBLE, pvalue)`); `dbGetLink` then delivers nothing for it on
    /// every later process, so a client's `caput REC.A 99` stands.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        crate::server::record::seed_input_links(self.multi_input_links())
    }

    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        &INP_VAL_PAIRS
    }

    /// C `subRecord.c::fetch_values` (407-418) returns -1 on the first failed
    /// `dbGetLink` and `process` (146) then skips `do_sub`.
    fn input_fetch_policy(&self) -> InputFetchPolicy {
        InputFetchPolicy::AbortOnFirstFailure
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// inputs M..U and INPM..INPU exist (C `INP_ARG_MAX == 21`).
    #[test]
    fn inputs_m_through_u_present() {
        let mut rec = SubRecord::default();
        for name in ["M", "Q", "U"] {
            rec.put_field(name, EpicsValue::Double(2.5)).unwrap();
            assert_eq!(rec.get_field(name), Some(EpicsValue::Double(2.5)));
        }
        for name in ["INPM", "INPR", "INPU"] {
            rec.put_field(name, EpicsValue::String("src".into()))
                .unwrap();
            assert_eq!(rec.get_field(name), Some(EpicsValue::String("src".into())));
        }
    }

    /// All 21 input channels are wired into `multi_input_links`.
    #[test]
    fn twenty_one_multi_input_links() {
        let rec = SubRecord::default();
        assert_eq!(rec.multi_input_links().len(), 21);
    }

    /// The monitor/archive deadbands and the LALM/MLST/ALST trackers
    /// round-trip through get/put (C `subRecord.c` MDEL/ADEL/LALM/MLST/ALST).
    #[test]
    fn deadband_and_tracker_fields_round_trip() {
        let mut rec = SubRecord::default();
        for name in ["MDEL", "ADEL", "LALM", "MLST", "ALST"] {
            rec.put_field(name, EpicsValue::Double(3.5)).unwrap();
            assert_eq!(rec.get_field(name), Some(EpicsValue::Double(3.5)));
        }
    }

    /// `init_record(0)` seeds MLST/ALST/LALM from the loaded VAL so the
    /// first process does not post or alarm on an unchanged value
    /// (C `subRecord.c::init_record` lines 130-132).
    #[test]
    fn init_seeds_trackers_from_val() {
        let mut rec = SubRecord::default();
        rec.put_field("VAL", EpicsValue::Double(7.0)).unwrap();
        rec.init_record(0).unwrap();
        assert_eq!(rec.get_field("MLST"), Some(EpicsValue::Double(7.0)));
        assert_eq!(rec.get_field("ALST"), Some(EpicsValue::Double(7.0)));
        assert_eq!(rec.get_field("LALM"), Some(EpicsValue::Double(7.0)));
    }

    /// BRSV round-trips as a `menuAlarmSevr` index (C `subRecord.c` BRSV,
    /// the severity used when the subroutine returns a negative status).
    #[test]
    fn brsv_round_trips() {
        let mut rec = SubRecord::default();
        assert_eq!(rec.get_field("BRSV"), Some(EpicsValue::Short(0)));
        rec.put_field("BRSV", EpicsValue::Short(2)).unwrap();
        assert_eq!(rec.get_field("BRSV"), Some(EpicsValue::Short(2)));
    }
}
