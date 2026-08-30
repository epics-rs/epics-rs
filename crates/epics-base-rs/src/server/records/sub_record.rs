use crate::error::{CaError, CaResult};
use crate::server::record::{InputFetchPolicy, ProcessOutcome, Record};
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

impl Record for SubRecord {
    fn record_type(&self) -> &'static str {
        "sub"
    }

    /// `subRecord.c:198-204` `get_linkNumber` — same shape as `calc`'s over
    /// `INP_ARG_MAX` (`subRecord.c:89`), also 21.
    fn link_backed_metadata_field(&self, field: &str) -> Option<String> {
        crate::server::record::calc_class_link_backed_metadata_field(field)
    }

    // C `subRecord.c::init_record` returns at `:99` for `pass == 0` and does
    // ALL of its work in pass 1: the constant-link loads, INAM, SNAM, and only
    // then `prec->mlst = prec->alst = prec->lalm = prec->val` (`:130-132`).
    // That tail is unreachable for a record whose SNAM is empty (`:122`) or
    // unregistered (`:128`), so it is performed by the SNAM resolution owner
    // (`ioc_app::wire_subroutine`) and not from here — this ran in pass 0,
    // before the resolution existed, and seeded the trackers of records C
    // leaves at zero.

    /// Empty for the reason the note above `record_type` gives: `sub`'s
    /// tracker seed is `subRecord.c:130-132`, the tail below `init_record`'s
    /// two early returns, and only a record whose SNAM actually resolved
    /// reaches it. The generic tail runs before the resolution and so cannot
    /// answer that; `ioc_app::wire_subroutine`, which performs the resolution,
    /// does it there.
    fn seed_deadband_tracking(&mut self) {}

    /// C `subRecord.c:119-123`: an empty `SNAM` names no subroutine, so
    /// `init_record` prints `"%s.SNAM is empty"`, sets `prec->pact = TRUE` and
    /// returns 0 — the record serves its fields but `dbProcess` takes the
    /// PACT-active branch on every scan. Measured: `caget -t P:SUB.PACT` on a
    /// bare `record(sub,"P:SUB"){}` reads 1.
    ///
    /// The park is NOT permanent, which is what `subRecord.c::special`
    /// (`:170-194`) exists for: `caput P:SUB.SNAM mySub` releases it and the
    /// record runs, `caput P:SUB.SNAM ""` parks a running one. Both directions
    /// fall out of this predicate — the framework re-asks it either side of a
    /// put to [`Record::pact_park_fields`].
    ///
    /// The non-empty-but-unregistered case is NOT this one: C returns
    /// `S_db_BadSub` there (`:125-129`, and again from `special`), which is a
    /// refused put, not a PACT park.
    fn parks_pact(&self) -> bool {
        self.snam.is_empty()
    }

    /// `subRecord.dbd.pod` marks `SNAM` `special(SPC_MOD)` and nothing else that
    /// bears on the park, so SNAM is the whole set.
    fn pact_park_fields(&self) -> &'static [&'static str] {
        &["SNAM"]
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // The subroutine is invoked by the framework via
        // `RecordInstance::subroutine` (it needs the registry of
        // named functions, which the record does not own).
        Ok(ProcessOutcome::complete())
    }

    /// C `subRecord.c::do_sub` writes `prec->udf` in exactly one place — the
    /// `else` arm of `if (status < 0)`, i.e. only where the subroutine ran
    /// (`:434`). An unresolved SNAM returns at `:427` after `BAD_SUB_ALARM`
    /// with UDF untouched, and a failed `fetch_values` never calls `do_sub` at
    /// all (`:146`), so C reports UDF 1 for a `sub` whose SNAM names no
    /// registered function. The per-cycle blanket re-derive cleared it.
    ///
    /// The write lives with the compute, in
    /// [`RecordInstance::do_sub`](crate::server::record::RecordInstance) — the
    /// shared site both `sub` and `aSub` reach.
    fn clears_udf(&self) -> bool {
        false
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

    /// C `subRecord.c::special` (SPC_MOD on SNAM, `:188-193`) resolves the name
    /// via `registryFunctionFind` and returns `S_db_BadSub` for a non-empty
    /// unregistered name — sub has no LFLG, so every SNAM put is validated. The
    /// empty-name case is accepted (C parks PACT instead; see
    /// [`Self::parks_pact`]). The registry lookup itself is
    /// performed by the put owner; see [`Record::is_subroutine_name_field`].
    fn is_subroutine_name_field(&self, field: &str) -> bool {
        field == "SNAM"
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

    /// C `subRecord.c::special` validates every SNAM put via
    /// `registryFunctionFind` (sub has no LFLG); no other field is a
    /// subroutine name.
    #[test]
    fn snam_is_the_subroutine_name_field() {
        let rec = SubRecord::default();
        assert!(rec.is_subroutine_name_field("SNAM"));
        assert!(!rec.is_subroutine_name_field("INAM"));
        assert!(!rec.is_subroutine_name_field("VAL"));
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

    /// Neither init hook seeds MLST/ALST/LALM: C reaches
    /// `subRecord.c:130-132` only past `init_record`'s INAM and SNAM early
    /// returns, so the seed belongs to the SNAM resolution owner
    /// (`ioc_app::wire_subroutine`, covered by
    /// `tests/snam_is_resolved_only_where_c_resolves_it.rs`). Seeding here
    /// gave an unresolved-SNAM record `MLST: 5` where softIoc reads `MLST: 0`.
    #[test]
    fn the_record_itself_seeds_no_tracker() {
        let mut rec = SubRecord::default();
        rec.put_field("VAL", EpicsValue::Double(7.0)).unwrap();
        rec.init_record(0).unwrap();
        rec.init_record(1).unwrap();
        rec.seed_deadband_tracking();
        for field in ["MLST", "ALST", "LALM"] {
            assert_eq!(
                rec.get_field(field),
                Some(EpicsValue::Double(0.0)),
                "{field}"
            );
        }
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
