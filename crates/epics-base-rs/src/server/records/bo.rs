use crate::error::{CaError, CaResult};
use crate::server::record::{
    FieldMetadataOverride, MENU_SIMM, ProcessAction, ProcessOutcome, Record,
};
use crate::types::{EpicsValue, PvString};

/// Binary output record matching C boRecord behavior.
/// VAL is converted to RVAL using MASK before writing to hardware.
pub struct BoRecord {
    pub val: u16,
    // RVAL/ORAW/RBV/ORBV/MASK are DBF_ULONG (boRecord.dbd.pod:252/256/299/
    // 303/261): unsigned 32-bit raw/readback/mask words. C stores epicsUInt32,
    // so high-bit masks (e.g. 0x80000000) must round-trip without sign loss.
    pub rval: u32,
    pub oraw: u32, // old raw value for monitor
    pub rbv: u32,  // readback value
    pub orbv: u32, // old readback value
    pub mask: u32, // hardware mask from device support
    // Strings
    pub znam: PvString,
    pub onam: PvString,
    // Alarm
    pub zsv: i16,
    pub osv: i16,
    pub cosv: i16,
    pub lalm: u16, // last alarm value (for COS alarm)
    // Monitor
    pub mlst: u16, // last monitored value
    // Output control
    pub omsl: i16,   // 0=supervisory, 1=closed_loop
    pub dol: String, // desired output location link
    pub high: f64,   // seconds to hold output high (toggle delay)
    // Invalid output
    pub ivoa: i16, // 0=Continue, 1=Don't drive, 2=Set to IVOV
    pub ivov: u16, // invalid output value
    // Simulation
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    pub sdly: f64,
    /// Set when a HIGH one-shot timer is in flight. The next
    /// `process()` (the timer-driven reprocess) forces `VAL = 0`,
    /// mirroring C `boRecord.c::myCallbackFunc` which sets
    /// `prec->val = 0` before `dbProcess`.
    high_reset_pending: bool,
    // VAL change gate. C
    // boRecord.c:394-399 monitor() raises DBE_VALUE|DBE_LOG for VAL only
    // when `mlst != val`. Captured during process() because the framework
    // reads monitor_value_changed() after process() has committed mlst.
    value_changed: bool,
    /// Set by `set_device_did_compute(true)` when a device readback has
    /// already produced both RVAL and VAL (`apply_raw_readback`). One-shot:
    /// `process()` then skips the forward `VAL -> RVAL` `val_to_rval()` that
    /// would recompute RVAL from VAL and discard the readback — C `processBo`
    /// sets `rval`/`val` from the callback and returns without re-converting
    /// (devAsynInt32.c:1201-1204 / devAsynUInt32Digital.c:730-733).
    skip_convert: bool,
}

impl Default for BoRecord {
    fn default() -> Self {
        Self {
            val: 0,
            rval: 0,
            oraw: 0,
            rbv: 0,
            orbv: 0,
            mask: 0,
            znam: PvString::new(),
            onam: PvString::new(),
            zsv: 0,
            osv: 0,
            cosv: 0,
            lalm: 0,
            mlst: 0,
            omsl: 0,
            dol: String::new(),
            high: 0.0,
            ivoa: 0,
            ivov: 0,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            sdly: -1.0,
            high_reset_pending: false,
            value_changed: false,
            skip_convert: false,
        }
    }
}

impl BoRecord {
    pub fn new(val: u16) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }

    /// Convert VAL to RVAL using MASK (C: convert val to rval)
    fn val_to_rval(&mut self) {
        if self.mask != 0 {
            if self.val == 0 {
                self.rval = 0;
            } else {
                self.rval = self.mask;
            }
        } else {
            self.rval = self.val as u32;
        }
    }
}

/// C `boRecord.c:85` `int boHIGHprecision = 2;` — the precision
/// `get_precision` serves for `HIGH`, the seconds-to-hold-1 field.
const BO_HIGH_PRECISION: i16 = 2;

/// C `boRecord.c:87` `double boHIGHlimit = 100000;` — the control upper
/// `get_control_double` (`:310-318`) serves for `HIGH`, over a literal `0.0`
/// lower.
const BO_HIGH_LIMIT: f64 = 100000.0;

impl Record for BoRecord {
    fn record_type(&self) -> &'static str {
        "bo"
    }

    /// C `boRecord.c:191-205`: the scalar `dbGetLink(&prec->dol, ..., &val, 0, 0)`
    /// under `dol.type != CONSTANT && omsl == menuOmslclosed_loop`.
    fn fetches_dol_closed_loop(&self) -> bool {
        true
    }

    /// `HIGH` is bo's only DBF_DOUBLE field, and both metadata literals in the
    /// rset are its: `get_units` (`:294-299`) answers `"s"` and `get_precision`
    /// (`:301-308`) answers `boHIGHprecision`; every other field falls to
    /// `recGblGetPrec`.
    ///
    /// Over PVA, pvxs nests the `display.precision` assignment inside the
    /// `DBR_GR_DOUBLE` branch (`iocsource.cpp:288-292`), and bo's rset serves
    /// no graphic limits for `HIGH`, so pvxs never assigns the precision leaf —
    /// `softIocPVX` prints `display.units "s"` and no `display.precision` at
    /// all for `bo.HIGH`. That is CBUG-G1, an upstream metadata-loss bug; the
    /// port declines to reproduce it. `nt::qsrv_marks::property_leaves` gates
    /// `display.precision` on its own `DBR_PRECISION` slot, so this transcribed
    /// value (2) reaches the wire — a deliberate deviation from `softIocPVX`,
    /// carried on the oracle allowlist.
    ///
    /// `get_control_double` (`:310-318`) is the same one-field shape and DOES
    /// reach the wire: `HIGH` alone takes `0.0 .. boHIGHlimit`, and every other
    /// field — `LALM` and `MLST` included — falls to `recGblGetControlDouble`.
    /// That is why `bo` lists nothing in
    /// [`control_explicit_field`](crate::server::record::RecordInstance): the
    /// one field the rset lists is answered here, by a literal, not by the
    /// record's HOPR/LOPR.
    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        field
            .eq_ignore_ascii_case("HIGH")
            .then(|| FieldMetadataOverride {
                units: Some("s".into()),
                precision: Some(BO_HIGH_PRECISION),
                ctrl_limits: Some((BO_HIGH_LIMIT, 0.0)),
                ..Default::default()
            })
    }

    /// C `boRecord.c:192-205`: `process()` clears `udf` to FALSE only on a
    /// successful closed-loop DOL fetch (`if (RTN_SUCCESS(status)) prec->udf =
    /// FALSE;`); the no-DOL arm reads the current VAL and leaves `udf` alone, so
    /// `checkAlarms` (`:371-372`) raises UDF_ALARM every cycle for a bare record.
    /// `udf` is never re-derived from the stored VAL, so bo opts out of the
    /// framework's blanket per-cycle clear. The definers are a direct VAL put
    /// (`dbPut`, `field_io.rs`) and the DOL-apply site (`processing.rs`).
    fn clears_udf(&self) -> bool {
        false
    }

    /// C `boRecord.c:371` tests `if (prec->udf == TRUE)` — exact-one. Combined
    /// with `clears_udf() == false`, a direct `caput X.UDF 255` (or `-1`,
    /// stored `255`) leaves `udf == 255` at `checkAlarms`, and `255 != TRUE`,
    /// so C raises NO UDF_ALARM — STAT/SEVR stay `NO_ALARM`. See
    /// [`Record::udf_alarm_on_exact_one`].
    fn udf_alarm_on_exact_one(&self) -> bool {
        true
    }

    /// C `devBoSoftRaw::write_bo` (`devBoSoftRaw.c:47-54`):
    /// `dbPutLink(&prec->out, DBR_LONG, &prec->rval, 1)` — the RAW word
    /// (`VAL ? MASK : 0`), not the `VAL` that `devBoSoft` writes.
    /// C `devBoSoftRaw.c::write_bo` (65): `dbPutLink(&prec->out, DBR_LONG,
    /// &prec->rval, 1)`. RVAL is DBF_ULONG; C hands its bit pattern to a
    /// DBR_LONG put, so the cast is C's reinterpretation, not a range clamp.
    fn raw_soft_output_value(&self) -> Option<EpicsValue> {
        Some(EpicsValue::Long(self.rval as i32))
    }

    // C recBo.c IVOA=set_to_IVOV: val = ivov; rval = ivov.
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        // IVOV is DBF_USHORT (boRecord.dbd.pod:372); VAL is the binary enum
        // and RVAL is DBF_ULONG. Route the unsigned IVOV value into both.
        let v: u16 = match &ivov {
            EpicsValue::UShort(e) => *e,
            EpicsValue::Enum(e) => *e,
            EpicsValue::Short(s) => *s as u16,
            other => return Err(CaError::TypeMismatch(format!("bo IVOV: {other:?}"))),
        };
        self.put_field("RVAL", EpicsValue::ULong(u32::from(v)))?;
        self.put_field("VAL", EpicsValue::Enum(v))
    }

    /// C `boRecord.c:146-149`:
    /// `if (recGblInitConstantLink(&prec->dol, DBF_USHORT, &ival)) {
    ///      prec->val = !!ival; prec->udf = FALSE; }`
    /// — the constant loads into a temporary and VAL takes its BOOLEAN, so
    /// `field(DOL,"5")` leaves VAL=1. Declared for the init-seed owner, which
    /// applies the `!!` and clears UDF.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        vec![crate::server::record::ConstantInitLink::dol_to_bool_val(
            "DOL", "VAL",
        )]
    }

    /// C `boRecord.c:163-172` — the init tail, run right after the constant
    /// load: convert VAL to RVAL through MASK, then
    /// `mlst = lalm = val; oraw = rval; orbv = rbv`.
    fn seed_deadband_tracking(&mut self) {
        self.val_to_rval();
        self.mlst = self.val;
        self.lalm = self.val;
        self.oraw = self.rval;
        self.orbv = self.rbv;
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // HIGH one-shot: a pending HIGH timer fired and triggered this
        // reprocess. C `boRecord.c::myCallbackFunc` sets `prec->val = 0`
        // before `dbProcess`, driving a momentary output back to Done.
        if self.high_reset_pending {
            self.high_reset_pending = false;
            self.val = 0;
        }

        // DOL/OMSL: a real (DB/CA/PVA) link is fetched and applied to VAL
        // by the framework before process(). A *constant* DOL is applied
        // once at init_record (`recGblInitConstantLink` parity) and is NOT
        // re-sourced here; C `boRecord.c:192` gates the fetch on
        // `!dbLinkIsConstant`, so a client caput to VAL is never clobbered
        // by the constant every cycle.

        // Convert val to rval using mask — unless a device readback
        // (`apply_raw_readback`) already set both RVAL and VAL, in which case
        // skip the forward convert that would recompute RVAL from VAL and
        // discard the readback. One-shot (C `processBo` readback returns
        // without re-converting; a normal process always converts).
        if !self.skip_convert {
            self.val_to_rval();
        }
        self.skip_convert = false;

        self.oraw = self.rval;
        self.orbv = self.rbv;

        // HIGH toggle: if val==1 and high>0, schedule reprocess after HIGH
        // seconds — the reprocess then drives the output back to 0.
        let mut actions = Vec::new();
        if self.val == 1 && self.high > 0.0 {
            self.high_reset_pending = true;
            actions.push(ProcessAction::ReprocessAfter(
                std::time::Duration::from_secs_f64(self.high),
            ));
        }

        // Capture the VAL-change
        // gate now (C boRecord.c:394-399 `mlst != val`); the HIGH toggle does
        // not alter VAL this cycle, so VAL is final here. The framework reads
        // monitor_value_changed() after process().
        self.value_changed = self.mlst != self.val;
        if self.value_changed {
            self.mlst = self.val;
        }

        Ok(ProcessOutcome {
            result: crate::server::record::RecordProcessResult::Complete,
            actions,
            device_did_compute: false,
        })
    }

    fn set_device_did_compute(&mut self, did: bool) {
        self.skip_convert = did;
    }

    /// Device readback (`asyn:READBACK` / SCAN="I/O Intr" / init seed): store
    /// the raw and resolve VAL to 0/1, mirroring C `processBo`/`initBo`
    /// (devAsynInt32.c:1202-1203,1187 / devAsynUInt32Digital.c:731-732,716).
    /// When MASK is set (the digital `asynUInt32Digital` config) RVAL keeps the
    /// masked raw and VAL is `(masked != 0)`; when MASK is 0 (the typical
    /// `asynInt32` `bo`, whose `initBo`/`processBo` do not mask) RVAL is the raw
    /// and VAL is `(raw != 0)` — the `mask != 0` split reproduces both device
    /// supports exactly. Returns `true` so the store reports `computed` and the
    /// framework skips the forward convert (via `set_device_did_compute`).
    fn apply_raw_readback(&mut self, raw: i32) -> bool {
        self.rval = if self.mask != 0 {
            (raw as u32) & self.mask
        } else {
            raw as u32
        };
        self.val = if self.rval != 0 { 1 } else { 0 };
        true
    }

    /// C `boRecord.c::checkAlarms` (`:366-387`) — the UDF alarm FIRST, then the
    /// STATE alarm (ZSV for VAL=0, OSV for VAL=1), then COS (COSV). The udf
    /// raise must lead: C `recGblSetSevr` overrides STAT/SEVR only when the new
    /// severity is strictly greater (recGbl.c:242 `if (nsev < new_sevr)`), so on
    /// a fresh record with `UDFS == ZSV == INVALID` the equal-severity STATE
    /// alarm cannot displace the UDF that was set first — STAT stays UDF. Unlike
    /// `bi`, C bo does NOT early-return on UDF, so STATE/COS still evaluate.
    /// Raising it here (idempotent with the framework's `rec_gbl_check_udf`,
    /// which runs after this hook) is what puts UDF ahead of STATE.
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        use crate::server::recgbl::{self, alarm_status};
        use crate::server::record::AlarmSeverity;

        // C `boRecord.c:371` — `if (prec->udf == TRUE)` (exact-one, see
        // `udf_alarm_on_exact_one`), raised before STATE, with no early return.
        if recgbl::udf_alarm_active(common.udf, true) {
            recgbl::rec_gbl_set_sevr(
                common,
                alarm_status::UDF_ALARM,
                AlarmSeverity::from_u16(common.udfs as u16),
            );
        }

        // STATE/COS use the RAW severity ordinal (ZSV/OSV/COSV are DBF_MENU
        // stored raw i16): C `recGblSetSevr(prec, STATE_ALARM, prec->zsv)`
        // compares the raw `epicsEnum16`, so an out-of-range `ZSV=4`/`65535`
        // numerically exceeds a prior UDF's INVALID(3) and overrides it. See
        // `rec_gbl_set_sevr_raw`. VAL is 0/1, so the branch picks ZSV or OSV.
        let val = self.val;
        let state_sev = if val == 0 { self.zsv } else { self.osv };
        recgbl::rec_gbl_set_sevr_raw(common, alarm_status::STATE_ALARM, state_sev as u16);
        if val != self.lalm {
            recgbl::rec_gbl_set_sevr_raw(common, alarm_status::COS_ALARM, self.cosv as u16);
            self.lalm = val;
        }
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Enum(self.val)),
            "RVAL" => Some(EpicsValue::ULong(self.rval)),
            "ORAW" => Some(EpicsValue::ULong(self.oraw)),
            "RBV" => Some(EpicsValue::ULong(self.rbv)),
            "ORBV" => Some(EpicsValue::ULong(self.orbv)),
            "MASK" => Some(EpicsValue::ULong(self.mask)),
            "ZNAM" => Some(EpicsValue::String(self.znam.clone())),
            "ONAM" => Some(EpicsValue::String(self.onam.clone())),
            "ZSV" => Some(EpicsValue::Short(self.zsv)),
            "OSV" => Some(EpicsValue::Short(self.osv)),
            "COSV" => Some(EpicsValue::Short(self.cosv)),
            "LALM" => Some(EpicsValue::UShort(self.lalm)),
            "MLST" => Some(EpicsValue::UShort(self.mlst)),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "DOL" => Some(EpicsValue::String(self.dol.clone().into())),
            "HIGH" => Some(EpicsValue::Double(self.high)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::UShort(self.ivov)),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            "SDLY" => Some(EpicsValue::Double(self.sdly)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::Enum(v) => {
                    self.val = v;
                    Ok(())
                }
                EpicsValue::Long(v) => {
                    self.val = v as u16;
                    Ok(())
                }
                EpicsValue::Short(v) => {
                    self.val = v as u16;
                    Ok(())
                }
                // C rset `put_enum_str` (boRecord.c), reached from
                // `dbConvert.c::putStringEnum` — one converter, shared with the
                // framework's put paths (see `Record::enum_state_strings`).
                EpicsValue::String(ref s) => {
                    let resolved = crate::server::record::resolve_enum_state_string(
                        "VAL",
                        self.enum_state_strings().as_deref(),
                        s,
                    )?;
                    if let EpicsValue::Enum(v) = resolved {
                        self.val = v;
                    }
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            // RVAL/MASK are DBF_ULONG: a client put arrives as ULong, internal
            // / device-support callers may still pass Long (same bit pattern).
            "RVAL" => match value {
                EpicsValue::ULong(v) => {
                    self.rval = v;
                    Ok(())
                }
                EpicsValue::Long(v) => {
                    self.rval = v as u32;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "MASK" => match value {
                EpicsValue::ULong(v) => {
                    self.mask = v;
                    Ok(())
                }
                EpicsValue::Long(v) => {
                    self.mask = v as u32;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ZNAM" => match value {
                EpicsValue::String(v) => {
                    self.znam = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ONAM" => match value {
                EpicsValue::String(v) => {
                    self.onam = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ZSV" => match value {
                EpicsValue::Short(v) => {
                    self.zsv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OSV" => match value {
                EpicsValue::Short(v) => {
                    self.osv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "COSV" => match value {
                EpicsValue::Short(v) => {
                    self.cosv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            // LALM/MLST are DBF_USHORT: accept a client UShort put, tolerate an
            // internal Enum (same u16 value).
            "LALM" => match value {
                EpicsValue::UShort(v) => {
                    self.lalm = v;
                    Ok(())
                }
                EpicsValue::Enum(v) => {
                    self.lalm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "MLST" => match value {
                EpicsValue::UShort(v) => {
                    self.mlst = v;
                    Ok(())
                }
                EpicsValue::Enum(v) => {
                    self.mlst = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OMSL" => match value {
                EpicsValue::Short(v) => {
                    self.omsl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DOL" => match value {
                EpicsValue::String(v) => {
                    self.dol = v.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "HIGH" => match value {
                EpicsValue::Double(v) => {
                    self.high = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "IVOA" => match value {
                EpicsValue::Short(v) => {
                    self.ivoa = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            // IVOV is DBF_USHORT: accept a client UShort put, tolerate Enum.
            "IVOV" => match value {
                EpicsValue::UShort(v) => {
                    self.ivov = v;
                    Ok(())
                }
                EpicsValue::Enum(v) => {
                    self.ivov = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIMM" => match value {
                EpicsValue::Short(v) => {
                    self.simm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIML" => match value {
                EpicsValue::String(v) => {
                    self.siml = v.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIOL" => match value {
                EpicsValue::String(v) => {
                    self.siol = v.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIMS" => match value {
                EpicsValue::Short(v) => {
                    self.sims = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SDLY" => match value {
                EpicsValue::Double(v) => {
                    self.sdly = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }

    /// `SIMM` is `DBF_MENU menu(menuSimm)` (`boRecord.dbd.pod`): the binary
    /// records carry the three-choice NO/YES/RAW simulation menu. Served as
    /// `DBR_ENUM` with these labels. `SIMS`/`OLDSIMM`/`OMSL`/`IVOA` are
    /// shared menus resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_SIMM),
            _ => None,
        }
    }

    /// C rset `get_enum_strs`/`put_enum_str` (boRecord.c) — ZNAM/ONAM.
    fn enum_state_strings(&self) -> Option<Vec<PvString>> {
        Some(crate::server::record::binary_enum_states(
            &self.znam, &self.onam,
        ))
    }

    /// C `get_enum_str` (boRecord.c:320-339): VAL 0 -> ZNAM, 1 -> ONAM, and any
    /// other index -> `"Illegal_Value"`. Slot 1 is indexed even when ONAM is
    /// empty, so it renders empty — the `no_str` trim in `enum_state_strings`
    /// is the LABEL list's, not this read's.
    fn enum_string_form(&self) -> Option<crate::server::snapshot::EnumStringForm> {
        Some(crate::server::record::binary_enum_string_form(
            &self.znam, &self.onam,
        ))
    }

    /// VAL posts DBE_VALUE|DBE_LOG
    /// only when it changed (C boRecord.c:394-399 `mlst != val`), not every
    /// process cycle. The comparison is captured in process(); see
    /// `value_changed`.
    fn monitor_value_changed(&self) -> Option<bool> {
        Some(self.value_changed)
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }
}
