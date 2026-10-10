use crate::error::{CaError, CaResult};
use crate::server::record::{CycleCell, CycleRead, CycleValue};
use crate::server::record::{MENU_SIMM, ProcessOutcome, RawSoftEntry, Record};
use crate::types::{EpicsValue, PvString};

/// Binary input record matching C biRecord behavior.
/// RVAL from device support is converted to VAL (0 or 1).
pub struct BiRecord {
    pub val: u16,
    // RVAL/ORAW/MASK are DBF_ULONG (biRecord.dbd.pod:199/203/208): unsigned
    // 32-bit raw/mask words. C stores epicsUInt32, so high-bit masks must
    // round-trip without sign loss.
    pub rval: u32,
    pub oraw: u32, // old raw value for monitor
    pub mask: u32, // hardware mask from device support
    // Strings
    pub znam: PvString,
    pub onam: PvString,
    // Alarm
    pub zsv: i16,
    pub osv: i16,
    pub cosv: i16,
    /// Alarm filter time constant (seconds), settable `DBF_DOUBLE`. `AFTC > 0`
    /// runs the STATE alarm severity through an exponential low-pass so a
    /// momentary excursion does not raise/clear the alarm until the input has
    /// held the new state for ~`AFTC` seconds. Added to `biRecord` by
    /// EPICS PR #817 (`c9817fa59`); 0 = disabled. See `BiRecord::afvl`.
    pub aftc: f64,
    /// Alarm filter accumulator (`biRecord.c:270` at `678092d03`, `prec->afvl`), `DBF_DOUBLE`
    /// `SPC_NOMOD` — read-only to clients. 0 = initial sample / filter
    /// inactive; the sign encodes the filter's rounding hysteresis.
    pub afvl: f64,
    pub lalm: u16, // last alarm value (for COS alarm)
    // Monitor
    pub mlst: u16, // last monitored value
    // Simulation
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    // SVAL is `DBF_ULONG` (biRecord.dbd.pod:263-265) — the BUFFER C's
    // `readValue` reads SIOL into (`dbGetLink(&prec->siol, DBR_ULONG,
    // &prec->sval)`, biRecord.c:289) before publishing `val = sval`.
    pub sval: u32,
    pub sims: i16,
    pub sdly: f64,
    // Internal: skip RVAL->VAL when soft INP set VAL directly
    skip_convert: bool,
    /// `devBiSoftCallback`'s private `devPvt->buffer.value`: calloc'd 0 and
    /// written only by a conversion that stores, so a read that stores nothing
    /// (a state name, NORD=0) hands back the last delivered value, not VAL.
    async_buffer: u16,
}

impl Default for BiRecord {
    fn default() -> Self {
        Self {
            val: 0,
            rval: 0,
            oraw: 0,
            mask: 0,
            znam: PvString::new(),
            onam: PvString::new(),
            zsv: 0,
            osv: 0,
            cosv: 0,
            aftc: 0.0,
            afvl: 0.0,
            lalm: 0,
            mlst: 0,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sval: 0,
            sims: 0,
            sdly: -1.0,
            skip_convert: false,
            async_buffer: 0,
        }
    }
}

impl BiRecord {
    pub fn new(val: u16) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }
}

impl Record for BiRecord {
    fn acts_before_process(&self) -> bool {
        false
    }

    fn cycle_hooks(&self) -> crate::server::record::CycleHooks {
        crate::server::record::CycleHooks::NONE
    }

    /// UDF belongs to the dset here, not to `process()`. C
    /// `biRecord.c:136-141` assigns `prec->udf = FALSE` at `:139`, INSIDE
    /// `if (status == 0)`, and folds `2` into `0` only at `:141`, so a device
    /// support that wrote VAL never reaches the assignment.
    ///
    /// That is not a hole: the C dsets that return 2 write `prec->udf`
    /// themselves first — `devBiSoft.c:54-59` and `devBiDbState.c:67-70` —
    /// which the port states as `DeviceUdf::Defined`.
    fn rederives_udf_on_computed_read(&self) -> bool {
        false
    }

    fn record_type(&self) -> &'static str {
        "bi"
    }

    // biRecord.c:112 returns a nonzero dset status before :114-116.
    fn seed_deadband_tracking(&mut self, dset_init: crate::server::device_support::DsetInit) {
        if dset_init != crate::server::device_support::DsetInit::Failed {
            self.mlst = self.val;
            self.lalm = self.val;
            self.oraw = self.rval;
        }
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // Skip RVAL->VAL conversion when soft INP already set VAL (C: status==2)
        if !self.skip_convert {
            if self.rval == 0 {
                self.val = 0;
            } else {
                self.val = 1;
            }
        }
        self.skip_convert = false; // reset for next cycle

        self.oraw = self.rval;
        Ok(ProcessOutcome::complete())
    }

    fn set_device_did_compute(&mut self, did_compute: bool) {
        self.skip_convert = did_compute;
    }

    /// asyn device readback: a raw hardware word read through `asynInt32`
    /// (`processBi` `pr->rval = value`, MASK unset so 0 — `initBi` passes
    /// NULL for the mask, devAsynInt32.c) or `asynUInt32Digital` (`processBi`
    /// `pr->rval = value & mask`, devAsynUInt32Digital.c:689) enters RVAL, and
    /// biRecord's `rval -> 0/1` convert resolves VAL. C returns 0 from
    /// `processBi`, so the record runs that convert; the hook performs the
    /// (identical) convert inline and returns `true` so the framework's
    /// `set_device_did_compute(true)` makes `process()` skip the forward pass
    /// — which would otherwise recompute VAL from the same RVAL (a no-op here,
    /// but the structural guarantee the output family relies on).
    ///
    /// This is the *device-distinct* entry: it is reached only from
    /// `store_read_value`, never from the Soft Channel path, which stays on
    /// `set_val` and writes the resolved link value straight into VAL (C
    /// `devBiSoft` `read_bi` returns 2). Routing the device raw here instead of
    /// through `set_val` keeps `set_val` single-meaning (a Soft Channel `bi`
    /// linked to a non-binary source still passes its value through as C does).
    /// Input twin of `bo::apply_raw_readback` (same `mask != 0` split).
    fn apply_raw_readback(&mut self, raw: i32) -> bool {
        self.rval = if self.mask != 0 {
            (raw as u32) & self.mask
        } else {
            raw as u32
        };
        self.val = if self.rval != 0 { 1 } else { 0 };
        true
    }

    fn refused_async_read_keeps_udf(&self) -> bool {
        true
    }

    fn raw_soft_read_failure_skips_convert(&self) -> bool {
        true
    }

    /// `bi` has an `RVAL → VAL` `convert()` step. A `Soft Channel` `bi`
    /// must skip it — C `devBiSoft.c` `read_bi` returns 2.
    fn soft_channel_skips_convert(&self) -> bool {
        true
    }

    /// C `biRecord.c::checkAlarms` (biRecord.c:232-280 at `678092d03`; the
    /// AFTC/AFVL alarm filter reached `bi` five lines earlier in EPICS PR
    /// #817 `c9817fa59`, so it is `678092d03` that every `biRecord.c` number
    /// in this method's body resolves at, not `c9817fa59` and not `R7.0.10`,
    /// where checkAlarms is :220-243 and has no AFVL) — UDF
    /// alarm, STATE alarm (ZSV/OSV) through the AFTC alarm-range low-pass
    /// filter, and COS alarm (COSV). C `checkAlarms:237-240` raises
    /// `UDF_ALARM/udfs` and returns early when `udf` is set; we mirror that
    /// (raising UDF is idempotent with the framework's own `rec_gbl_check_udf`,
    /// which also runs on the process path). Unlike `mbbiRecord.c`, the `bi`
    /// UDF path does **not** zero `AFVL`, so this matches `biRecord.c` exactly.
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        use crate::server::recgbl::{self, alarm_status};
        use crate::server::record::AlarmSeverity;

        if common.udf != 0 {
            recgbl::rec_gbl_set_sevr(
                common,
                alarm_status::UDF_ALARM,
                AlarmSeverity::from_u16(common.udfs as u16),
            );
            // C biRecord.c:237-240 returns here without touching prec->afvl.
            return;
        }
        let val = self.val;
        // C biRecord.c:242 — `if (val > 1) return;` (no severity, AFVL kept).
        if val > 1 {
            return;
        }
        // C biRecord.c:244-248 — pick the per-state severity (ZSV/OSV).
        let state_sev = if val == 0 { self.zsv } else { self.osv };
        // C biRecord.c:250-270 — the AFTC alarm-range low-pass filter. When
        // AFTC <= 0 the shared helper returns the raw severity and zeroes
        // AFVL, matching `double afvl = 0; ... prec->afvl = afvl;`.
        let (filtered, new_afvl) = super::alarm_filter::aftc_filter(
            state_sev as u16,
            self.aftc,
            self.afvl,
            common.time_last.get(),
            common.time,
        );
        self.afvl = new_afvl;
        // C biRecord.c:272-273 — `recGblSetSevr(prec, STATE_ALARM, asev)`.
        let sev = AlarmSeverity::from_u16(filtered);
        if sev != AlarmSeverity::NoAlarm {
            recgbl::rec_gbl_set_sevr(common, alarm_status::STATE_ALARM, sev);
        }
        // C biRecord.c:276-278 — COS alarm, fires only when VAL != LALM.
        if val != self.lalm {
            let cos_sev = AlarmSeverity::from_u16(self.cosv as u16);
            if cos_sev != AlarmSeverity::NoAlarm {
                recgbl::rec_gbl_set_sevr(common, alarm_status::COS_ALARM, cos_sev);
            }
            self.lalm = val;
        }
    }

    /// C rset `get_enum_strs`/`put_enum_str` (biRecord.c:195-217) — ZNAM/ONAM.
    fn enum_state_strings(&self) -> Option<Vec<PvString>> {
        Some(crate::server::record::binary_enum_states(
            &self.znam, &self.onam,
        ))
    }

    fn enum_str_state(&self, name: &PvString) -> Option<u16> {
        crate::server::record::binary_enum_str_state(&self.znam, &self.onam, name)
    }

    /// `prec->udf = FALSE` (`biRecord.c:227`).
    fn enum_str_clears_udf(&self) -> bool {
        true
    }

    /// C `get_enum_str` (biRecord.c:173-192): VAL 0 -> ZNAM, 1 -> ONAM, and any
    /// other index -> `"Illegal_Value"`. Slot 1 is indexed even when ONAM is
    /// empty, so it renders empty — the `no_str` trim in `enum_state_strings`
    /// is the LABEL list's, not this read's.
    fn enum_string_form(&self) -> Option<crate::server::snapshot::EnumStringForm> {
        Some(crate::server::record::binary_enum_string_form(
            &self.znam, &self.onam,
        ))
    }

    /// `devBiSoft.c:50` reads INP as `DBR_USHORT`, `devBiSoftRaw.c:50` as
    /// `DBR_ULONG`, `devBiSoftCallback.c:62` as `DBR_ENUM`.
    fn soft_input_request(
        &self,
        soft: crate::server::device_support::SoftDtyp,
    ) -> crate::server::record::InputLinkRequest {
        use crate::server::device_support::SoftDtyp;
        use crate::server::record::{InputLinkRequest, LinkReadAs};
        InputLinkRequest::As(match soft {
            SoftDtyp::Plain => LinkReadAs::UShort,
            SoftDtyp::Raw => LinkReadAs::ULong,
            SoftDtyp::Async => LinkReadAs::Enum,
        })
    }

    /// The Async dset copies its buffer into VAL; the other soft dsets never
    /// read `async_buffer`, so taking every delivered value is exact for it.
    fn soft_input_read(&mut self, value: Option<EpicsValue>) -> CaResult<()> {
        match value {
            None => return Ok(()),
            Some(EpicsValue::EnumArray(a)) if a.is_empty() => {
                self.set_val(EpicsValue::Enum(self.async_buffer))?
            }
            Some(value) => self.set_val(value)?,
        }
        self.async_buffer = self.val;
        Ok(())
    }

    /// The Plain dset's `DBR_USHORT` read: VAL is that state index.
    fn soft_input_read_u16(&mut self, value: u16) -> CaResult<()> {
        self.val = value;
        self.async_buffer = value;
        Ok(())
    }

    /// `dbPutFieldLink` rebinds the dset on every INP put (`del_record`, then
    /// `add_record`'s fresh calloc'd devPvt, `dbAccess.c:1158-1197`), so the
    /// Async buffer restarts at 0; a DTYP put alone rebinds nothing.
    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if after && field == "INP" {
            self.async_buffer = 0;
        }
        Ok(())
    }

    /// C `devBiSoftRaw` — `recGblInitConstantLink(&prec->inp, DBF_ULONG,
    /// &prec->rval)` at init, `dbGetLink(.., DBR_ULONG, &prec->rval, ..)` +
    /// `if (prec->mask) prec->rval &= prec->mask;` per read (epics-base
    /// `f2fe9d12`). The mask is in `read_bi` ONLY, so the init constant load is
    /// unmasked.
    fn raw_soft_input(&mut self, entry: RawSoftEntry, value: EpicsValue) -> Option<CaResult<()>> {
        self.rval = match super::raw_soft_rval_u32("bi", &value) {
            Ok(rval) => rval,
            Err(e) => return Some(Err(e)),
        };
        if entry == RawSoftEntry::Read && self.mask != 0 {
            self.rval &= self.mask;
        }
        Some(Ok(()))
    }

    /// `biRecord.c:332` `prec->rval = (unsigned short) prec->sval`.
    fn sval_as_rval(&self) -> Option<EpicsValue> {
        Some(EpicsValue::ULong(u32::from(self.sval as u16)))
    }

    fn link_text_ref(&self, link_field: &str) -> Option<&str> {
        match link_field {
            "SIML" => Some(&self.siml),
            "SIOL" => Some(&self.siol),
            _ => None,
        }
    }

    fn simm_cells(&self) -> Option<crate::server::record::SimmCells> {
        Some(crate::server::record::SimmCells {
            simm: self.simm,
            sims: self.sims,
            sdly: self.sdly,
        })
    }

    fn cycle_cell(&self, cell: CycleCell) -> CycleRead {
        match cell {
            CycleCell::Val => CycleRead::Value(CycleValue::Enum(self.val)),
            CycleCell::Mlst => CycleRead::Value(CycleValue::UShort(self.mlst)),
            CycleCell::Lalm => CycleRead::Value(CycleValue::UShort(self.lalm)),
            CycleCell::Aftc => CycleRead::Value(CycleValue::Double(self.aftc)),
            CycleCell::Afvl => CycleRead::Value(CycleValue::Double(self.afvl)),
            _ => CycleRead::Absent,
        }
    }

    fn put_cycle_cell(&mut self, cell: CycleCell, value: CycleValue) -> CaResult<()> {
        match (cell, value) {
            (CycleCell::Afvl, CycleValue::Double(v)) => self.afvl = v,
            (CycleCell::Lalm, CycleValue::UShort(v)) => self.lalm = v,
            (CycleCell::Mlst, CycleValue::UShort(v)) => self.mlst = v,
            (cell, value) => return self.put_field(cell.name(), value.into()),
        }
        Ok(())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Enum(self.val)),
            "RVAL" => Some(EpicsValue::ULong(self.rval)),
            "ORAW" => Some(EpicsValue::ULong(self.oraw)),
            "MASK" => Some(EpicsValue::ULong(self.mask)),
            "ZNAM" => Some(EpicsValue::String(self.znam.clone())),
            "ONAM" => Some(EpicsValue::String(self.onam.clone())),
            "ZSV" => Some(EpicsValue::Short(self.zsv)),
            "OSV" => Some(EpicsValue::Short(self.osv)),
            "COSV" => Some(EpicsValue::Short(self.cosv)),
            "AFTC" => Some(EpicsValue::Double(self.aftc)),
            "AFVL" => Some(EpicsValue::Double(self.afvl)),
            "LALM" => Some(EpicsValue::UShort(self.lalm)),
            "MLST" => Some(EpicsValue::UShort(self.mlst)),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SVAL" => Some(EpicsValue::ULong(self.sval)),
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
                // C rset `put_enum_str` (biRecord.c:208-217), reached from
                // `dbConvert.c::putStringEnum`. The framework's put paths
                // already resolve a DBR_STRING through `enum_str_state`
                // before they reach here; a direct caller takes the same
                // converter, so there is one string→state rule.
                EpicsValue::String(ref s) => {
                    let resolved =
                        crate::server::record::resolve_enum_state_string("VAL", &*self, s)?;
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
            "AFTC" => match value {
                EpicsValue::Double(v) => {
                    self.aftc = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            // AFVL is SPC_NOMOD (read-only to clients); this arm exists so
            // the framework alarm-filter owner can write the accumulator back.
            "AFVL" => match value {
                EpicsValue::Double(v) => {
                    self.afvl = v;
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
            "SVAL" => match value {
                EpicsValue::ULong(v) => {
                    self.sval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }

    /// `SIMM` is `DBF_MENU menu(menuSimm)` (`biRecord.dbd.pod`): the binary
    /// records carry the three-choice NO/YES/RAW simulation menu. Served as
    /// `DBR_ENUM` with these labels. `SIMS`/`OLDSIMM` are shared menus
    /// resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_SIMM),
            _ => None,
        }
    }

    /// C `biRecord.c:251-256` `monitor()`: `if (prec->mlst != prec->val) { events |=
    /// DBE_VALUE | DBE_LOG; prec->mlst = prec->val; }` — compared and
    /// committed HERE, at C's position, never captured during `process()`.
    fn monitor_value_changed(&mut self) -> Option<bool> {
        let changed = self.mlst != self.val;
        if changed {
            self.mlst = self.val;
        }
        Some(changed)
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }
}
