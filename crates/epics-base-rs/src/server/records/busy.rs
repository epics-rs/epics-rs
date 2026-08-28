use crate::error::{CaError, CaResult};
use crate::server::record::{
    DelayedCallbackOutcome, FieldMetadataOverride, MENU_YES_NO, ProcessAction, ProcessOutcome,
    Record,
};
use crate::types::{EpicsValue, PvString};

/// EPICS busy record implementation.
///
/// A busy record is a binary output variant that tracks asynchronous operation
/// state. VAL=1 means busy, VAL=0 means done. Forward links fire only when
/// `val == 0 || oval == 0`, suppressing FLNK during sustained busy state (1→1).
#[derive(Debug, Clone)]
pub struct BusyRecord {
    // Primary value
    pub val: u16,
    pub oval: u16,
    // Enum labels
    pub znam: PvString,
    pub onam: PvString,
    // Timing
    pub high: f64,
    // Alarms — ZSV/OSV/COSV are DBF_MENU menu(menuAlarmSevr) (busyRecord.dbd:87-
    // 107). Stored as the RAW epicsEnum16 ordinal, not a clamped enum, exactly as
    // bo does: a numeric `caput .ZSV 4`/`-1` (→65535) must round-trip and its raw
    // ordinal drives the STATE alarm precedence (see check_alarms).
    pub zsv: i16,
    pub osv: i16,
    pub cosv: i16,
    pub lalm: u16,
    // Invalid output — IVOA is DBF_MENU menu(menuIvoa) (busyRecord.dbd:148-153),
    // IVOV is DBF_USHORT (:154-158). Both stored as the RAW carrier, not a
    // clamping enum, exactly as bo: an out-of-range `caput .IVOA 3` keeps its raw
    // ordinal, and `caput .IVOV -1` wraps to 65535 (C's epicsUInt16 store) — the
    // framework reads IVOA back as the raw Short ordinal (processing.rs IVOA gate).
    pub ivoa: i16,
    pub ivov: u16,
    // Output control — OMSL is DBF_MENU menu(menuOmsl) (busyRecord.dbd:18-23);
    // stored raw so an out-of-range ordinal round-trips (mirrors bo).
    pub omsl: i16,
    pub dol: String,
    // Monitoring
    pub mlst: u16,
    // Raw value (Phase B)
    pub rval: u32,
    pub oraw: u32,
    pub mask: u32,
    pub rbv: u32,
    pub orbv: u32,
    // Simulation group (busyRecord.dbd:127-147). busy's `writeValue`
    // (busyRecord.c:389-416) is the bo-shaped OUTPUT redirect: SIML resolves
    // SIMM, SIMM=YES writes VAL out through SIOL instead of the device, and the
    // cycle carries SIMM_ALARM at SIMS. The redirect itself is owned by the
    // framework (`check_simulation_mode` -> `RedirectOutputToSiol`), which reads
    // it off these fields — without them the record looks unconfigured and a
    // simulated busy drives the real output.
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    /// Set by `set_device_did_compute(true)` when a device readback has
    /// already produced both RVAL and VAL (`apply_raw_readback`). One-shot:
    /// `process()` then skips the forward `convert_val_to_rval()` that would
    /// recompute RVAL from VAL and discard the readback — C `processBusy`'s
    /// callback branch sets `rval`/`val` and never re-converts
    /// (devBusyAsyn.c:482-488). Mirrors [`bo`](super::bo).
    skip_convert: bool,
}

impl Default for BusyRecord {
    fn default() -> Self {
        Self {
            val: 0,
            oval: 0,
            znam: PvString::from("Done"),
            onam: PvString::from("Busy"),
            high: 0.0,
            zsv: 0,
            osv: 0,
            cosv: 0,
            lalm: 0,
            ivoa: 0,
            ivov: 0,
            omsl: 0,
            dol: String::new(),
            mlst: 0,
            rval: 0,
            oraw: 0,
            mask: 0,
            rbv: 0,
            orbv: 0,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            skip_convert: false,
        }
    }
}

impl BusyRecord {
    pub fn new() -> Self {
        Self::default()
    }

    /// Convert VAL to RVAL using mask.
    fn convert_val_to_rval(&mut self) {
        if self.mask != 0 {
            self.rval = if self.val == 0 { 0 } else { self.mask };
        } else {
            self.rval = self.val as u32;
        }
    }

    /// Update monitoring fields.
    fn monitor(&mut self) {
        self.oraw = self.rval;
        self.orbv = self.rbv;
    }
}

/// C `busyRecord.c:281` — `if(paddr->pfield == (void *)&prec->high) *precision=2;`
/// A literal, not a `boHIGHprecision`-style exported variable: busy's rset
/// carries no settable analogue of it.
const BUSY_HIGH_PRECISION: i16 = 2;

impl Record for BusyRecord {
    /// C `busyRecord.c:277-284` `get_precision`: `HIGH` — busy's only
    /// `DBF_DOUBLE` field, so the only one past `dbAccess.c:388-389`'s
    /// float/double gate — takes 2, everything else `recGblGetPrec`'s
    /// memset zero. busy's rset NULLs `get_units` and `get_control_double`
    /// (`:54`, `:60`), which is the whole difference from the `bo`
    /// [`FieldMetadataOverride`] this mirrors: no `"s"`, no `0 .. 100000`.
    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        field
            .eq_ignore_ascii_case("HIGH")
            .then(|| FieldMetadataOverride {
                precision: Some(BUSY_HIGH_PRECISION),
                ..Default::default()
            })
    }

    fn record_type(&self) -> &'static str {
        "busy"
    }

    /// C `busyRecord.c:151-159`:
    /// `if (prec->dol.type == CONSTANT) { unsigned short ival = 0;
    ///      if (recGblInitConstantLink(&prec->dol, DBF_USHORT, &ival)) {
    ///          prec->val = ival ? 1 : 0; prec->udf = FALSE; } }`
    /// — boRecord's shape: the constant loads into a temporary and VAL takes
    /// its BOOLEAN, so `field(DOL,"5")` leaves VAL=1.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        vec![crate::server::record::ConstantInitLink::dol_to_bool_val(
            "DOL", "VAL",
        )]
    }

    /// C `busyRecord.c:176-179`, the last statement of `init_record`: VAL is
    /// converted to RVAL through MASK, with the `mask == 0` arm passing VAL
    /// through as `(epicsUInt32)prec->val` rather than zeroing RVAL. It runs
    /// after the constant DOL load two dozen lines above it, so a
    /// `field(DOL,"5")` busy reaches its first process with RVAL already
    /// holding MASK.
    fn init_record_tail(&mut self) {
        self.convert_val_to_rval();
    }

    /// C `busyRecord.c:127-181` assigns neither `mlst` nor `lalm`, unlike
    /// `boRecord.c:172-173` which seeds both from VAL — so busy's first
    /// `monitor()` (`busyRecord.c:365`, `mlst != prec->val`) must find them at
    /// 0 even when a constant DOL has already driven VAL to 1, and post the
    /// change C posts. busy serves both fields to clients, so the framework
    /// default would try to seed them; that attempt is inert today only because
    /// busy's `put_field` binds no MLST/LALM arm, and this override is what
    /// keeps it inert if one is ever added.
    fn seed_deadband_tracking(&mut self) {}

    /// C `busyRecord.c:196-208`: the scalar `dbGetLink(&prec->dol, ..., &prec->val, 0, 0)`
    /// under `dol.type != CONSTANT && omsl == menuOmslclosed_loop`.
    fn fetches_dol_closed_loop(&self) -> bool {
        true
    }

    /// C `busyRecord.c:195-208`: `process()` clears `udf` to FALSE only on a
    /// successful closed-loop DOL fetch (`if(status==0){ prec->val = val;
    /// prec->udf = FALSE; }`, `:202-204`); a bare (no-DOL) record reads the
    /// stored VAL and leaves `udf` alone, so `checkAlarms` (`:337`) raises
    /// UDF_ALARM every cycle. busy is boRecord's process verbatim here — it
    /// never re-derives `udf` from the stored VAL, so it opts out of the
    /// framework's blanket per-cycle clear (mirrors [`bo`](super::bo)).
    fn clears_udf(&self) -> bool {
        false
    }

    /// C `busyRecord.c:337` tests `if (prec->udf == TRUE)` — exact-one. Combined
    /// with `clears_udf() == false`, a direct `caput X.UDF 255` (or `-1`, stored
    /// `255`) leaves `udf == 255` at `checkAlarms`, and `255 != TRUE`, so C
    /// raises NO UDF_ALARM — STAT/SEVR stay `NO_ALARM`. bo shares this
    /// (`boRecord.c:371`); see [`Record::udf_alarm_on_exact_one`].
    fn udf_alarm_on_exact_one(&self) -> bool {
        true
    }

    /// W10-E5. `busyRecord.c:399-401` — a failed SIML read returns from
    /// `writeValue` BEFORE `write_busy` and before the SIOL `dbPutLink`:
    ///
    /// ```c
    /// status=dbGetLink(&prec->siml,DBR_USHORT, &prec->simm,0,0);
    /// if (status)
    ///     return(status);
    /// ```
    ///
    /// busy is the only record in the port that does this — the recGblGetSimm
    /// records' equivalent branch is dead code (`recGblGetSimm` always returns
    /// 0, recGbl.c:456) and swait never tests the status (swaitRecord.c:402).
    fn aborts_on_failed_siml_read(&self) -> bool {
        true
    }

    /// C `boRecord.c::process` IVOA=set_to_IVOV: `val = ivov` then
    /// `rval = (epicsUInt32)val` (busy shares boRecord's process).
    /// OVAL is the *saved previous* VAL and is NOT overwritten by the
    /// C `busyRecord.c:235-243` (module `busy` at `R1-7-4-6-g2dfe92d`) is
    /// `boRecord.c:230-238` transcribed: `prec->val = prec->ivov;` then the
    /// record's OWN `/* convert val to rval */` block, so RVAL is IVOV run
    /// through the MASK rule, never IVOV itself.
    ///
    /// The hand-rolled `RVAL = IVOV` this replaces made the arm a complete
    /// no-op: `get_field("IVOV")` returns the native `UShort` (`:389`), which
    /// fell to the `other => other.clone()` default and hit `put_field("RVAL",
    /// UShort)`'s `TypeMismatch` (RVAL is DBF_ULONG), so the `?` aborted
    /// before VAL was written and an INVALID `busy` at IVOA=Set_output_to_IVOV
    /// drove its stale value instead of IVOV. The framework's IVOA owner
    /// discarded that error until `c00e980e` gave it a `debug_assert`.
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        // IVOV is DBF_USHORT (`busyRecord.dbd:154`); VAL is the binary enum.
        let v: u16 = match &ivov {
            EpicsValue::UShort(e) => *e,
            EpicsValue::Enum(e) => *e,
            EpicsValue::Short(s) => *s as u16,
            other => return Err(CaError::TypeMismatch(format!("busy IVOV: {other:?}"))),
        };
        self.put_field("VAL", EpicsValue::Enum(v))?;
        self.convert_val_to_rval();
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // Step 1: DOL reading handled by framework (OMSL=ClosedLoop)

        // Step 2: VAL → RVAL conversion — unless a device readback
        // (`apply_raw_readback`) already set both RVAL and VAL, in which case
        // skip the forward convert that would recompute RVAL from VAL and
        // discard the readback. One-shot, mirrors bo (C `processBusy`'s
        // callback branch returns without re-converting).
        if !self.skip_convert {
            self.convert_val_to_rval();
        }
        self.skip_convert = false;

        // Step 3: Save current VAL before write (for FLNK decision)
        self.oval = self.val;

        // Step 4: alarm raising is owned by the trait check_alarms() hook (STATE
        // vs COS, raw menu ordinals), and the INVALID-output IVOA policy is
        // enforced by the framework which gates the OUT write on
        // common.sevr == Invalid.

        // Step 5: Monitor
        self.monitor();

        // Step 6: HIGH one-shot — C `busyRecord.c:258-262` arms
        // `callbackRequestDelayed` on every process that leaves VAL at 1, and
        // the timer, never a process cycle, is what drives the flag back to
        // Done. Arming carries no record state, so a scan step's own FLNK or a
        // caput inside the window re-arms rather than releasing the busy flag
        // early — which is the whole point of BUSY=1 gating a synApps step.
        let mut actions = Vec::new();
        if self.val == 1 && self.high > 0.0 {
            actions.push(ProcessAction::DelayedCallbackAfter(
                crate::runtime::time::duration_from_secs(self.high),
            ));
        }

        // Step 7: FLNK handled by should_fire_forward_link()
        Ok(ProcessOutcome::complete_with(actions))
    }

    /// C `busyRecord.c::myCallbackFunc` (:107-124), boRecord's verbatim — the
    /// HIGH timer's own body and the only writer of the one-shot's
    /// `prec->val = 0`.
    fn delayed_callback_fire(&mut self, pact: bool) -> DelayedCallbackOutcome {
        if !pact {
            self.val = 0;
            return DelayedCallbackOutcome::Reprocess;
        }
        // Mid-async-cycle: C changes nothing and waits another full HIGH.
        // The conversion is `runtime::time::duration_from_secs`, the same one
        // `process()` arms with, so `self.high` cannot mean two deadlines in
        // two adjacent functions: a HIGH past `Duration`'s range (`caput
        // REC.HIGH 1e300`, which C ACCEPTS) becomes `Duration::MAX`, C's
        // queued callback that no comparison ever reaches. `Drop` is then left
        // with its one meaning — the one-shot is no longer live — instead of
        // also standing for "the delay would not fit".
        match (self.val == 1 && self.high > 0.0)
            .then(|| crate::runtime::time::duration_from_secs(self.high))
        {
            Some(delay) => DelayedCallbackOutcome::Rearm(delay),
            None => DelayedCallbackOutcome::Drop,
        }
    }

    /// C `busyRecord.c::checkAlarms` (`:332-357`, boRecord's verbatim) — the UDF
    /// alarm FIRST, then the STATE alarm (ZSV for VAL=0, OSV for VAL=1), then COS
    /// (COSV). The udf raise must lead: C `recGblSetSevr` overrides STAT/SEVR only
    /// on a strictly greater severity, so on a fresh record with `UDFS == INVALID`
    /// the equal-severity STATE alarm cannot displace the UDF that was set first —
    /// STAT stays UDF. busy does NOT early-return on UDF, so STATE/COS still
    /// evaluate. Raising it here (idempotent with the framework's
    /// `rec_gbl_check_udf`, which runs after this hook) is what puts UDF ahead of
    /// STATE. Raising STATE=INVALID into `common` also lets the framework's IVOA
    /// handler gate the OUT-link write (IVOA="Don't drive" → `skip_out`).
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        use crate::server::recgbl::{self, alarm_status};
        use crate::server::record::AlarmSeverity;

        // C `busyRecord.c:337` — `if (prec->udf == TRUE)` (exact-one, see
        // `udf_alarm_on_exact_one`), raised before STATE with no early return.
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
        // numerically exceeds a prior UDF's INVALID(3) and overrides it (see
        // `rec_gbl_set_sevr_raw`). `raw_sevr == 0` is a no-op. Mirrors bo.
        let state_sev = if self.val == 0 { self.zsv } else { self.osv };
        recgbl::rec_gbl_set_sevr_raw(common, alarm_status::STATE_ALARM, state_sev as u16);
        if self.val != self.lalm {
            recgbl::rec_gbl_set_sevr_raw(common, alarm_status::COS_ALARM, self.cosv as u16);
            self.lalm = self.val;
        }
    }

    fn should_fire_forward_link(&self) -> bool {
        self.val == 0 || self.oval == 0
    }

    fn can_device_write(&self) -> bool {
        true
    }

    fn set_device_did_compute(&mut self, did: bool) {
        self.skip_convert = did;
    }

    /// Device readback (devBusyAsyn's always-on output callback): store the
    /// raw and resolve VAL to 0/1, mirroring C `processBusy`'s callback branch
    /// (devBusyAsyn.c:484-487 `pr->rval = value; pr->val = (pr->rval) ? 1 :
    /// 0`). No MASK split — busy has only the Int32 device support
    /// (`asynBusyInt32`), whose readback is unmasked. Returns `true` so the
    /// store reports `computed` and the framework skips the forward convert
    /// (via `set_device_did_compute`).
    fn apply_raw_readback(&mut self, raw: i32) -> bool {
        self.rval = raw as u32;
        self.val = if self.rval != 0 { 1 } else { 0 };
        true
    }

    // NO `is_put_complete` override — busy completes its put-callback
    // synchronously, like bo. C `busyRecord.c:273` clears `pact = FALSE` at the
    // tail of every process cycle; only async device support that set `pact`
    // itself (`:254`) holds the callback, and the soft support this port models
    // (`devBusySoft.c::write_busy` is a bare `dbPutLink`, never touching `pact`)
    // does not. A prior `is_put_complete() == self.val == 0` modelled the
    // asynBusy hold, but this record's `process()` is synchronous (never returns
    // `AsyncPendingNotify`), so the phantom hold only wedged the put-notify:
    // once VAL was driven to 1 the callback never completed, and every following
    // `ca_put_callback` was refused, so the
    // out-of-range VAL puts C posts (2, 3 → "Illegal_Value") never processed.
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Enum(self.val)),
            "OVAL" => Some(EpicsValue::Enum(self.oval)),
            "ZNAM" => Some(EpicsValue::String(self.znam.clone())),
            "ONAM" => Some(EpicsValue::String(self.onam.clone())),
            "HIGH" => Some(EpicsValue::Double(self.high)),
            "ZSV" => Some(EpicsValue::Short(self.zsv)),
            "OSV" => Some(EpicsValue::Short(self.osv)),
            "COSV" => Some(EpicsValue::Short(self.cosv)),
            "LALM" => Some(EpicsValue::Enum(self.lalm)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::UShort(self.ivov)),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "DOL" => Some(EpicsValue::String(self.dol.clone().into())),
            "MLST" => Some(EpicsValue::Enum(self.mlst)),
            // RVAL/ORAW/MASK/RBV/ORBV are DBF_ULONG (boRecord.dbd.pod:252,256,
            // 261,299,303; busyRecord.dbd:55,59,69,108,112) — serve the native
            // u32 as the unsigned carrier so a high-bit raw/mask value does not
            // round-trip through a sign-losing `as i32`.
            "RVAL" => Some(EpicsValue::ULong(self.rval)),
            "ORAW" => Some(EpicsValue::ULong(self.oraw)),
            "MASK" => Some(EpicsValue::ULong(self.mask)),
            "RBV" => Some(EpicsValue::ULong(self.rbv)),
            "ORBV" => Some(EpicsValue::ULong(self.orbv)),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                self.val = match value {
                    EpicsValue::Enum(v) => v,
                    EpicsValue::Short(v) => v as u16,
                    EpicsValue::Long(v) => v as u16,
                    EpicsValue::Double(v) => v as u16,
                    // C rset `put_enum_str` (`busyRecord.c`): an EXACT,
                    // case-sensitive `strncmp` against ZNAM/ONAM, else
                    // `S_db_badChoice` — the put fails and nothing is stored.
                    // This arm used to match case-insensitively and then coerce
                    // any unmatched name to state 0, so `caput BUSY Opne` drove
                    // the record to Done and reported success.
                    EpicsValue::String(ref s) => {
                        let resolved = crate::server::record::resolve_enum_state_string(
                            "VAL",
                            self.enum_state_strings().as_deref(),
                            s,
                        )?;
                        match resolved {
                            EpicsValue::Enum(v) => v,
                            _ => return Err(CaError::TypeMismatch(name.to_string())),
                        }
                    }
                    _ => return Err(CaError::TypeMismatch(name.to_string())),
                };
                Ok(())
            }
            "ZNAM" => {
                if let EpicsValue::String(s) = value {
                    self.znam = s;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            "ONAM" => {
                if let EpicsValue::String(s) = value {
                    self.onam = s;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            "HIGH" => {
                if let EpicsValue::Double(v) = value {
                    self.high = v;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            // ZSV/OSV/COSV are DBF_MENU menu(menuAlarmSevr): store the RAW
            // epicsEnum16 ordinal the central menu converter resolved, mirroring
            // bo — an out-of-range numeric put keeps its bit pattern so it
            // round-trips and drives the raw STATE-alarm precedence.
            "ZSV" => {
                if let EpicsValue::Short(v) = value {
                    self.zsv = v;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            "OSV" => {
                if let EpicsValue::Short(v) = value {
                    self.osv = v;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            "COSV" => {
                if let EpicsValue::Short(v) = value {
                    self.cosv = v;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            // IVOA is DBF_MENU menu(menuIvoa): store the RAW epicsEnum16 ordinal
            // the central menu converter resolved (mirrors bo) — an out-of-range
            // numeric put keeps its value and round-trips.
            "IVOA" => {
                if let EpicsValue::Short(v) = value {
                    self.ivoa = v;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            // IVOV is DBF_USHORT: accept the coerced unsigned carrier (a
            // `caput -1` is wrapped to 65535 by `coerce_put_value`), tolerate an
            // internal Enum (same u16). Served as UShort so the coercion routes a
            // string put through the numeric parser, not the ZNAM/ONAM enum-state
            // resolver — mirrors bo (which the enum serving broke: `-1` was
            // rejected as an unknown state name and IVOV stayed 0).
            "IVOV" => match value {
                EpicsValue::UShort(v) => {
                    self.ivov = v;
                    Ok(())
                }
                EpicsValue::Enum(v) => {
                    self.ivov = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.to_string())),
            },
            // OMSL is DBF_MENU menu(menuOmsl): store raw ordinal (mirrors bo). The
            // framework reads it back as the raw Short and compares == closed_loop.
            "OMSL" => {
                if let EpicsValue::Short(v) = value {
                    self.omsl = v;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            "DOL" => {
                if let EpicsValue::String(s) = value {
                    self.dol = s.as_str_lossy().into_owned();
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            // MASK is DBF_ULONG (boRecord.dbd.pod:261, busyRecord.dbd:69):
            // accept the native unsigned carrier and tolerate the legacy signed
            // `Long` (reinterpret preserves the bit pattern for a high-bit mask).
            "MASK" => {
                self.mask = match value {
                    EpicsValue::ULong(v) => v,
                    EpicsValue::Long(v) => v as u32,
                    _ => return Err(CaError::TypeMismatch(name.to_string())),
                };
                Ok(())
            }
            // RVAL is the converted output value (C `boRecord.h`
            // `DBF_ULONG`). Writable so the IVOA=set_to_IVOV policy
            // (`apply_invalid_output_value`) can drive it directly.
            "RVAL" => {
                self.rval = match value {
                    EpicsValue::ULong(v) => v,
                    EpicsValue::Long(v) => v as u32,
                    EpicsValue::Enum(v) => v as u32,
                    EpicsValue::Short(v) => v as u32,
                    EpicsValue::Double(v) => v as u32,
                    _ => return Err(CaError::TypeMismatch(name.to_string())),
                };
                Ok(())
            }
            "SIMM" => match value {
                EpicsValue::Short(v) => {
                    self.simm = v;
                    Ok(())
                }
                EpicsValue::Enum(v) => {
                    self.simm = v as i16;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.to_string())),
            },
            "SIML" => match value {
                EpicsValue::String(v) => {
                    self.siml = v.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.to_string())),
            },
            "SIOL" => match value {
                EpicsValue::String(v) => {
                    self.siol = v.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.to_string())),
            },
            "SIMS" => match value {
                EpicsValue::Short(v) => {
                    self.sims = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.to_string())),
            },
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    /// `SIMM` is `DBF_MENU menu(menuYesNo)` (`busyRecord.dbd:137-141`) — the
    /// two-choice NO/YES menu, not the NO/YES/RAW `menuSimm` of the base binary
    /// records. Served as `DBR_ENUM` with those labels; `SIMS` is the shared
    /// `menuAlarmSevr`, resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_YES_NO),
            _ => None,
        }
    }

    /// C rset `get_enum_strs`/`put_enum_str` (`busyRecord.c`, the bo pair
    /// verbatim) — ZNAM/ONAM.
    fn enum_state_strings(&self) -> Option<Vec<PvString>> {
        Some(crate::server::record::binary_enum_states(
            &self.znam, &self.onam,
        ))
    }

    /// C `get_enum_str` (busyRecord.c:286-306): VAL 0 -> ZNAM, 1 -> ONAM, and any
    /// other index -> `"Illegal_Value"`. Slot 1 is indexed even when ONAM is
    /// empty, so it renders empty — the `no_str` trim in `enum_state_strings`
    /// is the LABEL list's, not this read's.
    fn enum_string_form(&self) -> Option<crate::server::snapshot::EnumStringForm> {
        Some(crate::server::record::binary_enum_string_form(
            &self.znam, &self.onam,
        ))
    }

    /// C `busyRecord.c:365-369` `monitor()`: `if (prec->mlst != prec->val) { events |=
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::record::dbd_generated;
    use crate::types::DbFieldType;

    #[test]
    fn test_default() {
        let rec = BusyRecord::default();
        assert_eq!(rec.val, 0);
        assert_eq!(rec.oval, 0);
        assert_eq!(rec.znam, "Done");
        assert_eq!(rec.onam, "Busy");
        assert_eq!(rec.high, 0.0);
        assert_eq!(rec.zsv, 0);
        assert_eq!(rec.osv, 0);
        assert_eq!(rec.cosv, 0);
        assert_eq!(rec.ivoa, 0);
        assert_eq!(rec.omsl, 0);
        assert_eq!(rec.mlst, 0);
        assert_eq!(rec.mask, 0);
        assert_eq!(rec.rval, 0);
    }

    #[test]
    fn test_record_type() {
        let rec = BusyRecord::new();
        assert_eq!(rec.record_type(), "busy");
    }

    #[test]
    fn test_can_device_write() {
        let rec = BusyRecord::new();
        assert!(rec.can_device_write());
    }

    /// devBusyAsyn.c:484-487 — the driver callback stores the raw unmasked
    /// (`pr->rval = value`) and maps VAL to 0/1; the following process() must
    /// not recompute RVAL from VAL (the one-shot `set_device_did_compute`
    /// gate), or a non-0/1 raw readback would be discarded.
    #[test]
    fn raw_readback_is_kept_through_process() {
        let mut rec = BusyRecord::new();
        assert!(rec.apply_raw_readback(5));
        assert_eq!((rec.rval, rec.val), (5, 1));
        rec.set_device_did_compute(true);
        rec.process().unwrap();
        assert_eq!(rec.rval, 5, "readback RVAL survives the process cycle");
        // One-shot: the next (non-readback) process converts VAL forward again.
        rec.process().unwrap();
        assert_eq!(rec.rval, 1);
    }

    /// The release cycle: driver clears the param to 0, the readback maps
    /// VAL 1 → 0, and that process fires FLNK (`val == 0 || oval == 0`) —
    /// the completion a `caput -c`/`wait=True` client blocks on.
    #[test]
    fn raw_readback_zero_releases_and_fires_flnk() {
        let mut rec = BusyRecord::new();
        rec.put_field("VAL", EpicsValue::Enum(1)).unwrap();
        rec.process().unwrap();
        assert!(!rec.should_fire_forward_link(), "sustained busy: no FLNK");
        assert!(rec.apply_raw_readback(0));
        assert_eq!((rec.rval, rec.val), (0, 0));
        rec.set_device_did_compute(true);
        rec.process().unwrap();
        assert!(rec.should_fire_forward_link());
    }

    #[test]
    fn test_get_put_field_val() {
        let mut rec = BusyRecord::new();
        rec.put_field("VAL", EpicsValue::Enum(1)).unwrap();
        assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Enum(1)));
        assert_eq!(rec.val, 1);

        rec.put_field("VAL", EpicsValue::Short(0)).unwrap();
        assert_eq!(rec.val, 0);

        rec.put_field("VAL", EpicsValue::Double(1.0)).unwrap();
        assert_eq!(rec.val, 1);
    }

    #[test]
    fn test_get_put_field_roundtrip() {
        let mut rec = BusyRecord::new();

        // String fields
        rec.put_field("ZNAM", EpicsValue::String("Idle".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("ZNAM"),
            Some(EpicsValue::String("Idle".into()))
        );

        rec.put_field("ONAM", EpicsValue::String("Active".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("ONAM"),
            Some(EpicsValue::String("Active".into()))
        );

        // Double field
        rec.put_field("HIGH", EpicsValue::Double(2.5)).unwrap();
        assert_eq!(rec.get_field("HIGH"), Some(EpicsValue::Double(2.5)));

        // Short fields (enums)
        rec.put_field("ZSV", EpicsValue::Short(1)).unwrap();
        assert_eq!(rec.get_field("ZSV"), Some(EpicsValue::Short(1)));

        rec.put_field("OSV", EpicsValue::Short(2)).unwrap();
        assert_eq!(rec.get_field("OSV"), Some(EpicsValue::Short(2)));

        rec.put_field("COSV", EpicsValue::Short(3)).unwrap();
        assert_eq!(rec.get_field("COSV"), Some(EpicsValue::Short(3)));

        rec.put_field("IVOA", EpicsValue::Short(1)).unwrap();
        assert_eq!(rec.get_field("IVOA"), Some(EpicsValue::Short(1)));

        // IVOV is DBF_USHORT — served/accepted as the unsigned carrier.
        rec.put_field("IVOV", EpicsValue::UShort(1)).unwrap();
        assert_eq!(rec.get_field("IVOV"), Some(EpicsValue::UShort(1)));

        rec.put_field("OMSL", EpicsValue::Short(1)).unwrap();
        assert_eq!(rec.get_field("OMSL"), Some(EpicsValue::Short(1)));

        rec.put_field("DOL", EpicsValue::String("some:link".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("DOL"),
            Some(EpicsValue::String("some:link".into()))
        );

        // MASK is DBF_ULONG — legacy signed `Long` puts are still accepted,
        // but the field reads back as the unsigned carrier.
        rec.put_field("MASK", EpicsValue::Long(0xFF)).unwrap();
        assert_eq!(rec.get_field("MASK"), Some(EpicsValue::ULong(0xFF)));
    }

    /// IVOA/OMSL are DBF_MENU stored raw: a numeric out-of-range put keeps its
    /// ordinal (no clamp to 0), matching C `*pfield = (epicsEnum16)val`. The
    /// clamping enum stored 0 and served the in-range label instead.
    #[test]
    fn menu_out_of_range_puts_store_raw_ordinal() {
        let mut rec = BusyRecord::new();

        // menuIvoa has 3 choices (0..2); 3 is out of range.
        rec.put_field("IVOA", EpicsValue::Short(3)).unwrap();
        assert_eq!(rec.get_field("IVOA"), Some(EpicsValue::Short(3)));

        // menuOmsl has 2 choices (0..1); 2 is out of range.
        rec.put_field("OMSL", EpicsValue::Short(2)).unwrap();
        assert_eq!(rec.get_field("OMSL"), Some(EpicsValue::Short(2)));
    }

    /// IVOV is DBF_USHORT: the coerced put of `-1` arrives as the wrapped
    /// `UShort(65535)` (C's `epicsUInt16` store), and busy accepts it — the enum
    /// serving previously routed the string `-1` through the ZNAM/ONAM enum-state
    /// resolver, which rejected it, so the put failed and IVOV stayed 0.
    #[test]
    fn ivov_accepts_wrapped_ushort_put() {
        let mut rec = BusyRecord::new();
        rec.put_field("IVOV", EpicsValue::UShort(65535)).unwrap();
        assert_eq!(rec.get_field("IVOV"), Some(EpicsValue::UShort(65535)));
    }

    /// RVAL/ORAW/MASK/RBV/ORBV are DBF_ULONG (boRecord.dbd.pod:252-303,
    /// busyRecord.dbd:55-112): a high-bit raw/mask value must round-trip
    /// through the unsigned carrier without the sign loss the old
    /// `Long(x as i32)`/`x as i32` serving caused.
    #[test]
    fn test_ulong_raw_fields_high_bit_roundtrip() {
        let mut rec = BusyRecord::new();
        let hi: u32 = 0x8000_0001;

        // Native unsigned carrier put for the writable fields.
        rec.put_field("MASK", EpicsValue::ULong(hi)).unwrap();
        assert_eq!(rec.get_field("MASK"), Some(EpicsValue::ULong(hi)));
        rec.put_field("RVAL", EpicsValue::ULong(hi)).unwrap();
        assert_eq!(rec.get_field("RVAL"), Some(EpicsValue::ULong(hi)));

        // Read-only raw fields serve the unsigned carrier verbatim.
        rec.oraw = hi;
        rec.rbv = hi;
        rec.orbv = hi;
        assert_eq!(rec.get_field("ORAW"), Some(EpicsValue::ULong(hi)));
        assert_eq!(rec.get_field("RBV"), Some(EpicsValue::ULong(hi)));
        assert_eq!(rec.get_field("ORBV"), Some(EpicsValue::ULong(hi)));

        // The advertised dbf type is unsigned for the whole raw family.
        for name in ["RVAL", "ORAW", "MASK", "RBV", "ORBV"] {
            let desc = dbd_generated::BUSY_FIELDS
                .iter()
                .find(|f| f.name == name)
                .unwrap();
            assert_eq!(desc.dbf_type, DbFieldType::ULong, "{name} dbf_type");
        }
    }

    /// C `busyRecord.c::put_enum_str` is an exact, case-SENSITIVE `strncmp`
    /// against ZNAM/ONAM and returns `S_db_badChoice` on no match — the put
    /// FAILS and nothing is stored.
    ///
    /// This test previously asserted the opposite on both counts (a
    /// case-insensitive match, and — through `parse::<u16>().unwrap_or(0)` —
    /// silent coercion of an unmatched name to state 0), so it was green over
    /// the defect R19-24 names: `caput BUSY <typo>` drove the record to Done
    /// and reported success.
    #[test]
    fn put_enum_str_is_exact_case_sensitive_and_rejects_unknown_names() {
        let mut rec = BusyRecord::new();

        rec.put_field("VAL", EpicsValue::String("Done".into()))
            .unwrap();
        assert_eq!(rec.val, 0);

        rec.put_field("VAL", EpicsValue::String("Busy".into()))
            .unwrap();
        assert_eq!(rec.val, 1);

        // Wrong case is NOT a state name: C `strncmp` fails, the put is
        // rejected, VAL keeps its previous value.
        assert!(
            rec.put_field("VAL", EpicsValue::String("done".into()))
                .is_err(),
            "C put_enum_str is case-sensitive"
        );
        assert_eq!(rec.val, 1, "a rejected put stores nothing");

        // An unmatched name is S_db_badChoice, never state 0.
        assert!(
            rec.put_field("VAL", EpicsValue::String("Opne".into()))
                .is_err()
        );
        assert_eq!(rec.val, 1);

        // The numeric fallback C's `putStringEnum` applies after badChoice:
        // an index below `no_str` is accepted.
        rec.put_field("VAL", EpicsValue::String("0".into()))
            .unwrap();
        assert_eq!(rec.val, 0);
        assert!(
            rec.put_field("VAL", EpicsValue::String("2".into()))
                .is_err(),
            "index >= no_str (2) is badChoice"
        );

        // Custom ZNAM/ONAM.
        rec.znam = "Off".into();
        rec.onam = "On".into();
        rec.put_field("VAL", EpicsValue::String("Off".into()))
            .unwrap();
        assert_eq!(rec.val, 0);
        rec.put_field("VAL", EpicsValue::String("On".into()))
            .unwrap();
        assert_eq!(rec.val, 1);
    }

    // --- process() tests ---

    #[test]
    fn test_process_updates_oval() {
        let mut rec = BusyRecord::new();
        rec.val = 1;
        rec.process().unwrap();
        assert_eq!(rec.oval, 1);

        rec.val = 0;
        rec.process().unwrap();
        assert_eq!(rec.oval, 0);
    }

    #[test]
    fn test_mask_conversion() {
        let mut rec = BusyRecord::new();
        rec.mask = 0xFF;
        rec.val = 1;
        rec.process().unwrap();
        assert_eq!(rec.rval, 0xFF);

        rec.val = 0;
        rec.process().unwrap();
        assert_eq!(rec.rval, 0);
    }

    #[test]
    fn test_mask_zero_passthrough() {
        let mut rec = BusyRecord::new();
        rec.mask = 0;
        rec.val = 1;
        rec.process().unwrap();
        assert_eq!(rec.rval, 1);

        rec.val = 0;
        rec.process().unwrap();
        assert_eq!(rec.rval, 0);
    }

    #[test]
    fn test_state_alarm_zsv() {
        use crate::server::record::{AlarmSeverity, CommonFields};
        let mut rec = BusyRecord::new();
        rec.zsv = 1; // MINOR
        rec.val = 0;
        // Clear the default UDF to isolate the STATE path (as bo_state_alarm_osv).
        let mut common = CommonFields {
            udf: 0,
            ..Default::default()
        };
        rec.check_alarms(&mut common);
        assert_eq!(common.nsev, AlarmSeverity::Minor);
        assert_eq!(
            common.nsta,
            crate::server::recgbl::alarm_status::STATE_ALARM
        );
    }

    #[test]
    fn test_state_alarm_osv() {
        use crate::server::record::{AlarmSeverity, CommonFields};
        let mut rec = BusyRecord::new();
        rec.osv = 2; // MAJOR
        rec.val = 1;
        let mut common = CommonFields {
            udf: 0,
            ..Default::default()
        };
        rec.check_alarms(&mut common);
        assert_eq!(common.nsev, AlarmSeverity::Major);
        assert_eq!(
            common.nsta,
            crate::server::recgbl::alarm_status::STATE_ALARM
        );
    }

    /// A numeric `caput .ZSV 4` stores the raw out-of-range ordinal (busy is a
    /// DBF_MENU stored as raw i16). C hands it straight to `recGblSetSevr`, so a
    /// raw ordinal numerically greater than a prior UDF's INVALID(3) overrides
    /// it: STAT becomes STATE (the displayed SEVR still clamps to INVALID).
    #[test]
    fn test_state_alarm_raw_ordinal_overrides_udf() {
        use crate::server::record::{AlarmSeverity, CommonFields};
        let mut rec = BusyRecord::new();
        rec.put_field("ZSV", EpicsValue::Short(4)).unwrap();
        assert_eq!(
            rec.get_field("ZSV"),
            Some(EpicsValue::Short(4)),
            "raw round-trip"
        );
        rec.val = 0;
        let mut common = CommonFields::default(); // udf=1, udfs=INVALID
        rec.check_alarms(&mut common);
        assert_eq!(
            common.nsta,
            crate::server::recgbl::alarm_status::STATE_ALARM,
            "raw ZSV=4 > UDF's INVALID(3) overrides the UDF that was set first"
        );
        assert_eq!(common.nsev, AlarmSeverity::Invalid, "displayed SEVR clamps");
    }

    #[test]
    fn test_cos_alarm() {
        use crate::server::record::CommonFields;
        let mut rec = BusyRecord::new();
        rec.cosv = 1; // MINOR
        rec.lalm = 0;
        rec.val = 1; // changed from lalm=0
        // C `busyRecord.c:337` raises UDF_ALARM (at UDFS=INVALID) before COS;
        // `CommonFields::default()` has udf=1, so clear it to isolate the COS
        // path (as the bi/bo parity tests do).
        let mut common = CommonFields {
            udf: 0,
            ..Default::default()
        };
        rec.check_alarms(&mut common);
        // COS alarm fires and advances lalm.
        assert_eq!(rec.lalm, 1);
        assert_eq!(common.nsev, crate::server::record::AlarmSeverity::Minor);

        // Same val — no COS change.
        let mut common2 = CommonFields {
            udf: 0,
            ..Default::default()
        };
        rec.check_alarms(&mut common2);
        assert_eq!(rec.lalm, 1);
        assert_eq!(common2.nsev, crate::server::record::AlarmSeverity::NoAlarm);
    }

    #[test]
    fn test_cos_alarm_severity() {
        use crate::server::record::{AlarmSeverity, CommonFields};
        let mut rec = BusyRecord::new();
        rec.cosv = 2; // MAJOR
        rec.osv = 1; // MINOR
        rec.lalm = 0;
        rec.val = 1;
        // Clear the default UDF to isolate STATE/COS.
        let mut common = CommonFields {
            udf: 0,
            ..Default::default()
        };
        rec.check_alarms(&mut common);
        // COS (Major) > OSV (Minor), so the raised severity is Major, at COS.
        assert_eq!(common.nsev, AlarmSeverity::Major);
        assert_eq!(common.nsta, crate::server::recgbl::alarm_status::COS_ALARM);
    }

    #[test]
    fn test_monitor_mlst() {
        // `mlst` is committed by `monitor_value_changed`, which is C's
        // `monitor()` — so a cycle here is `process()` then that hook.
        let cycle = |rec: &mut BusyRecord| {
            rec.process().unwrap();
            rec.monitor_value_changed()
        };

        let mut rec = BusyRecord::new();
        rec.val = 1;
        assert_eq!(cycle(&mut rec), Some(true));
        assert_eq!(rec.mlst, 1);

        // Same val — mlst stays
        assert_eq!(cycle(&mut rec), Some(false));
        assert_eq!(rec.mlst, 1);

        rec.val = 0;
        assert_eq!(cycle(&mut rec), Some(true));
        assert_eq!(rec.mlst, 0);
    }

    // --- FLNK semantics tests ---

    #[test]
    fn test_flnk_0_to_1() {
        let mut rec = BusyRecord::new();
        rec.val = 1;
        rec.oval = 0;
        assert!(rec.should_fire_forward_link());
    }

    #[test]
    fn test_flnk_1_to_1() {
        let mut rec = BusyRecord::new();
        rec.val = 1;
        rec.oval = 1;
        assert!(!rec.should_fire_forward_link());
    }

    #[test]
    fn test_flnk_1_to_0() {
        let mut rec = BusyRecord::new();
        rec.val = 0;
        rec.oval = 1;
        assert!(rec.should_fire_forward_link());
    }

    #[test]
    fn test_flnk_0_to_0() {
        let mut rec = BusyRecord::new();
        rec.val = 0;
        rec.oval = 0;
        assert!(rec.should_fire_forward_link());
    }

    // --- FLNK after process() ---

    #[test]
    fn test_flnk_after_process_busy_start() {
        let mut rec = BusyRecord::new();
        rec.val = 1;
        rec.process().unwrap();
        // After process: val=1, oval=1 (set during process)
        // But FLNK decision in C code uses oval saved *before* write.
        // In our impl, oval is set to val at process start, so oval=1.
        // 0→1 transition: we need to check the val/oval after process.
        // oval was set to val (1) during process, so val=1, oval=1 → false.
        // Wait — the C code saves oval=val BEFORE write, meaning before device
        // support might change val. In our pure record process, val doesn't change
        // during write. So for a simple 0→1 put: val=1, oval=1 after process.
        // FLNK = val==0 || oval==0 → false.
        //
        // But in C code line 271: if val==0 || oval==0 → fire FLNK.
        // For the transition 0→1:
        //   Before process: val=1 (just put), oval=0 (from last process)
        //   Process starts: oval = val = 1
        //   After process: val=1, oval=1 → FLNK = false
        //
        // Hmm, but the plan says 0→1 should fire FLNK (oval=0).
        // The key insight: oval is NOT set in the current process, it was set
        // in the PREVIOUS process cycle. Let me re-read the C code...
        //
        // Actually re-reading C code line 220: prec->oval = prec->val
        // This saves the current val into oval. So when we PUT val=1:
        //   process(): oval = val = 1
        //   FLNK check: val=1, oval=1 → false
        //
        // But the FIRST time val transitions 0→1, what was oval before?
        // It was 0 from the previous process (or default).
        // Wait — line 220 sets oval = val at the START of each process.
        // So oval always equals val at FLNK check time... unless async
        // device support changes val after oval is saved (line 220 is before write).
        //
        // For the synchronous case (no async device support), the plan's table
        // describes the state ENTERING process, not after. The actual FLNK check
        // uses the values AT CHECK TIME:
        //   val=1 (unchanged), oval=1 (just saved) → false
        //
        // This means for synchronous device support, FLNK only fires when val==0.
        // The oval==0 case handles async: device support sets val=1 while
        // oval was saved as 0.
        //
        // For our tests, just verify the process() behavior directly.
        assert_eq!(rec.val, 1);
        assert_eq!(rec.oval, 1);
        // val=1, oval=1 → FLNK = false (correct for sync)
        assert!(!rec.should_fire_forward_link());
    }

    #[test]
    fn test_flnk_after_process_done() {
        let mut rec = BusyRecord::new();
        // Simulate: was busy, now done
        rec.val = 0;
        rec.oval = 1; // from previous process where val was 1
        rec.process().unwrap();
        // After process: oval = val = 0
        assert_eq!(rec.val, 0);
        assert_eq!(rec.oval, 0);
        // val=0 → FLNK fires
        assert!(rec.should_fire_forward_link());
    }

    // --- IVOA / alarm-raising tests ---
    //
    // IVOA policy is enforced by the framework (processing.rs), which
    // gates the OUT write on `common.sevr == Invalid`. The record's
    // job is to raise the INVALID severity via `check_alarms` and to
    // apply IVOV via `apply_invalid_output_value`.

    #[test]
    fn test_check_alarms_raises_invalid_state() {
        use crate::server::record::{AlarmSeverity, CommonFields};
        let mut rec = BusyRecord::new();
        rec.osv = 3; // INVALID
        rec.val = 1;
        // Clear the default UDF (udf=1) to isolate the STATE path; the UDF-first
        // precedence is covered by `check_alarms_udf_precedes_state`.
        let mut common = CommonFields {
            udf: 0,
            ..Default::default()
        };
        rec.check_alarms(&mut common);
        // INVALID state severity propagates into common — the
        // framework's IVOA "Don't drive" then suppresses the OUT write.
        assert_eq!(common.nsev, AlarmSeverity::Invalid);
        assert_eq!(
            common.nsta,
            crate::server::recgbl::alarm_status::STATE_ALARM
        );
    }

    /// C `busyRecord.c::checkAlarms:337-350` raises UDF_ALARM (at UDFS=INVALID)
    /// BEFORE the STATE alarm, and `recGblSetSevr` overrides only on a strictly
    /// greater severity — so on a `udf=1` record the equal-severity INVALID STATE
    /// alarm cannot displace it: STAT stays UDF. This is the divergence the
    /// oracle saw as `stat C='UDF' port='NO_ALARM'` after a `pp(TRUE)` put.
    #[test]
    fn check_alarms_udf_precedes_state() {
        use crate::server::record::{AlarmSeverity, CommonFields};
        let mut rec = BusyRecord::new();
        rec.osv = 3; // INVALID
        rec.val = 1;
        // Default udf=1 (a fresh, never-DOL-sourced record).
        let mut common = CommonFields::default();
        rec.check_alarms(&mut common);
        assert_eq!(common.nsev, AlarmSeverity::Invalid);
        assert_eq!(
            common.nsta,
            crate::server::recgbl::alarm_status::UDF_ALARM,
            "UDF is raised first and the equal-severity STATE cannot displace it"
        );
    }

    #[test]
    fn test_check_alarms_no_alarm_when_severities_unset() {
        use crate::server::record::{AlarmSeverity, CommonFields};
        let mut rec = BusyRecord::new();
        rec.val = 0;
        // Clear the default UDF (udf=1) so this exercises the "no severity set"
        // path rather than the UDF alarm.
        let mut common = CommonFields {
            udf: 0,
            ..Default::default()
        };
        rec.check_alarms(&mut common);
        assert_eq!(common.nsev, AlarmSeverity::NoAlarm);
    }

    #[test]
    fn test_apply_invalid_output_value() {
        let mut rec = BusyRecord::new();
        rec.val = 1;
        rec.rval = 1;
        rec.apply_invalid_output_value(EpicsValue::Enum(0)).unwrap();
        // IVOA=SetOutputToIvov path: VAL/OVAL/RVAL all become IVOV.
        assert_eq!(rec.val, 0);
        assert_eq!(rec.oval, 0);
        assert_eq!(rec.rval, 0);
    }

    // --- State transition cycle ---

    #[test]
    fn test_state_transition_cycle() {
        // `mlst` is C `monitor()`'s tracker, committed by
        // `monitor_value_changed`; `oval` is the record's own, committed by
        // `process()`. A cycle runs both.
        let cycle = |rec: &mut BusyRecord| {
            rec.process().unwrap();
            rec.monitor_value_changed();
        };

        let mut rec = BusyRecord::new();

        // Start idle
        assert_eq!(rec.val, 0);
        cycle(&mut rec);
        assert_eq!(rec.oval, 0);
        assert_eq!(rec.mlst, 0);

        // Go busy
        rec.val = 1;
        cycle(&mut rec);
        assert_eq!(rec.oval, 1);
        assert_eq!(rec.mlst, 1);
        assert_eq!(rec.rval, 1);

        // Stay busy (re-process)
        cycle(&mut rec);
        assert_eq!(rec.oval, 1);
        assert!(!rec.should_fire_forward_link());

        // Go done
        rec.val = 0;
        cycle(&mut rec);
        assert_eq!(rec.oval, 0);
        assert_eq!(rec.mlst, 0);
        assert_eq!(rec.rval, 0);
        assert!(rec.should_fire_forward_link());
    }
}
