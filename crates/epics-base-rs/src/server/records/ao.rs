use crate::error::{CaError, CaResult};
use crate::server::record::{
    FieldMetadataOverride, MENU_SIMM, ProcessOutcome, Record, ValuePostGate,
};
use crate::server::records::convert_phase::ConvertPhase;
use crate::types::{EpicsValue, PvString};

/// Choice labels for the output-increment-format menu, in index order.
/// C `menu(aoOIF)` (`aoRecord.dbd.pod`): 0=Full, 1=Incremental. Selects
/// whether `OVAL`/`OUT` writes the full value or the delta from the
/// previous output.
const AO_OIF_CHOICES: &[&str] = &["Full", "Incremental"];

/// Analog output record with conversion and output policy support.
/// LINR: 0=NO_CONVERSION, 1=SLOPE, 2=LINEAR
pub struct AoRecord {
    // Display
    pub val: f64,
    pub egu: PvString,
    pub hopr: f64,
    pub lopr: f64,
    pub prec: i16,
    pub drvh: f64,
    pub drvl: f64,
    // Conversion
    pub rval: i32,
    pub oraw: i32, // old raw value for monitor
    /// C `aoRecord.c:482` `prec->omod = (prec->oval != value);` — "the output
    /// moved this cycle". It opens the OVAL/RVAL/RBV post block on a cycle
    /// where VAL's own monitor mask is empty (`:535`), which is why it is a
    /// field and not a local: OROC can walk OVAL one step per cycle toward a
    /// VAL that has not moved since MLST.
    pub omod: bool,
    pub rbv: i32,  // readback value
    pub orbv: i32, // old readback value
    pub oval: f64,
    pub linr: i16, // 0=NO_CONVERSION, 1=SLOPE, 2=LINEAR
    pub eguf: f64,
    pub egul: f64,
    pub eslo: f64, // default 1.0
    pub eoff: f64, // engineering offset (defaults to egul for LINEAR)
    // ROFF is DBF_ULONG (aoRecord.dbd.pod:366) — stored unsigned so the full
    // 0..=2^32-1 range round-trips: a client caput 4294967295 reads back as
    // that value (promoted to DBR_DOUBLE over CA) and the RVAL+ROFF convert
    // uses C's epicsUInt32 addend. Storing it signed made a high-bit put read
    // back negative and convert with the wrong sign.
    pub roff: u32,
    /// `ASLO` — raw-to-engineering slope applied ahead of ESLO. `aoRecord.dbd`
    /// gives it NO `initial()`, so C's calloc'd record starts at 0.0 and every
    /// use is guarded (`if (prec->aslo != 0.0) value *= prec->aslo`), i.e. 0.0
    /// means "no ASLO scaling" — it is NOT a unit slope. (`ai` is the asymmetric
    /// twin: `aiRecord.dbd` DOES declare `initial("1")`. Measured: a bare C
    /// `record(ao,..)` serves ASLO=0, a bare `record(ai,..)` serves ASLO=1.)
    pub aslo: f64,
    pub aoff: f64,
    // Output control
    pub omsl: i16,   // 0=supervisory, 1=closed_loop
    pub dol: String, // desired output location link
    pub oif: i16,    // 0=Full, 1=Incremental
    pub oroc: f64,   // output rate of change
    pub pval: f64,   // previous value
    // Invalid output
    pub ivoa: i16, // 0=Continue, 1=Don't drive, 2=Set to IVOV
    pub ivov: f64,
    // Monitor deadband
    pub adel: f64,
    pub mdel: f64,
    pub lalm: f64,
    pub alst: f64,
    pub mlst: f64,
    // Runtime
    /// C `prec->init` — see [`ConvertPhase`]. Set by `init_record` and by an
    /// `SPC_LINCONV` put, cleared at the end of every `process`.
    pub init: ConvertPhase,
    /// Set by `convert()` when a `LINR >= 3` breakpoint-table conversion
    /// fails — the table could not be resolved, or the engineering value
    /// fell past an end of the table. C `aoRecord.c::convert` raises
    /// `SOFT_ALARM/MAJOR_ALARM` and returns (rval left unchanged) in both
    /// cases (aoRecord.c:494-499); `check_alarms` consumes this flag to do
    /// the same.
    bpt_error: bool,
    /// Set by `convert_readback()` when the asyn `raw → eng` readback's
    /// `LINR >= 3` breakpoint conversion fails (out of range or unresolvable
    /// table). C `processAo` raises `WRITE_ALARM/INVALID_ALARM` and returns,
    /// leaving `VAL` unchanged (devAsynInt32.c:988-990) — a different alarm
    /// from the forward `convert()`'s `SOFT/MAJOR`, so it has its own flag.
    /// `check_alarms` consumes it.
    readback_bpt_error: bool,
    /// The database breakpoint-table registry (`install_breaktable_registry`),
    /// used to resolve the `LINR >= 3` table lazily.
    bpt_registry: Option<std::sync::Arc<crate::server::cvt_bpt::BreakTableRegistry>>,
    /// The resolved breakpoint table for the current `LINR` (C `pbrk` cache);
    /// reset to `None` when `LINR` changes so the next conversion re-resolves.
    bpt_table: Option<std::sync::Arc<crate::server::cvt_bpt::BrkTable>>,
    /// Cached last breakpoint interval index (C `lbrk`).
    lbrk: usize,
    /// Set by [`Record::set_device_did_compute`] when asyn device support has
    /// already produced the final `VAL`/`RVAL` from a raw device readback (the
    /// `raw → eng` inverse convert, [`AoRecord::convert_readback`]). When set,
    /// `process()` skips its forward `VAL → RVAL` `convert()` so the readback is
    /// not overwritten — the output analogue of `ai`'s `skip_convert`, and the
    /// faithful mirror of C `processAo`'s readback branch, which sets `rval`/
    /// `val` directly and returns without calling `convertAo` (devAsynInt32.c:
    /// 970-994). One-shot: cleared at the end of `process()`.
    skip_convert: bool,
    // Simulation
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    pub sdly: f64,
}

impl Default for AoRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            egu: PvString::new(),
            hopr: 0.0,
            lopr: 0.0,
            prec: 0,
            drvh: 0.0,
            drvl: 0.0,
            rval: 0,
            oraw: 0,
            omod: false,
            rbv: 0,
            orbv: 0,
            oval: 0.0,
            linr: 0,
            eguf: 0.0,
            egul: 0.0,
            eslo: 1.0,
            eoff: 0.0,
            roff: 0,
            aslo: 0.0,
            aoff: 0.0,
            omsl: 0,
            dol: String::new(),
            oif: 0,
            oroc: 0.0,
            pval: 0.0,
            ivoa: 0,
            ivov: 0.0,
            adel: 0.0,
            mdel: 0.0,
            lalm: 0.0,
            alst: 0.0,
            mlst: 0.0,
            init: ConvertPhase::default(),
            bpt_error: false,
            readback_bpt_error: false,
            bpt_registry: None,
            bpt_table: None,
            lbrk: 0,
            skip_convert: false,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            sdly: -1.0,
        }
    }
}

impl AoRecord {
    pub fn new(val: f64) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }

    /// C `aoRecord.c::convert`: drive-limit clamp, OROC rate-of-change
    /// limiting, engineering→raw linearisation and raw rounding. Factored
    /// out of `process()` so the IVOA=Set_output_to_IVOV path can re-run
    /// the same conversion on `VAL = IVOV` without re-fetching DOL.
    fn convert(&mut self) {
        // Drive limits
        if self.drvh > self.drvl {
            self.val = self.val.clamp(self.drvl, self.drvh);
        }

        // C: value = prec->val, then OROC modifies value (not VAL)
        let mut value = self.val;
        self.pval = value; // pval = drive-limited desired value (like C)

        // OROC: rate of change limiting (C applies unconditionally when oroc != 0)
        if self.oroc != 0.0 {
            let diff = value - self.oval;
            if diff < 0.0 {
                if self.oroc < -diff {
                    value = self.oval - self.oroc;
                }
            } else if self.oroc < diff {
                value = self.oval + self.oroc;
            }
        }

        // C `aoRecord.c:482-483`, in this order: the flag is the comparison
        // against the PREVIOUS oval, then oval takes the new value.
        self.omod = self.oval != value;
        self.oval = value; // oval = rate-limited output value

        // convert(): engineering units to raw value
        // Step 1: linearization (SLOPE or LINEAR)
        self.bpt_error = false;
        // Clear the readback-failure flag so a stale prior readback alarm
        // cannot survive into this forward convert's check_alarms.
        self.readback_bpt_error = false;
        match self.linr {
            1 | 2 => {
                // SLOPE/LINEAR: (value - eoff) / eslo
                if self.eslo == 0.0 {
                    value = 0.0;
                } else {
                    value = (value - self.eoff) / self.eslo;
                }
            }
            0 => {} // NO_CONVERSION
            _ => {
                // LINR >= 3 selects a breakpoint-table linearisation. Resolve
                // the table lazily and cache it (C `cvtEngToRawBpt` resolves
                // via `findBrkTable` on the first call / after a LINR change).
                if self.bpt_table.is_none() {
                    self.bpt_table = self
                        .bpt_registry
                        .as_ref()
                        .and_then(|reg| reg.table_for_linr(self.linr));
                }
                match &self.bpt_table {
                    Some(table) => {
                        let (raw, status) = crate::server::cvt_bpt::cvt_eng_to_raw_bpt(
                            value,
                            table,
                            &mut self.lbrk,
                        );
                        if status == crate::server::cvt_bpt::BptStatus::OutOfRange {
                            // C `aoRecord.c:494-499`: a non-zero
                            // `cvtEngToRawBpt` raises SOFT/MAJOR and returns,
                            // leaving RVAL/ORAW unchanged. OVAL was already
                            // updated above (the OROC step), matching C which
                            // sets `oval` before the linearisation switch.
                            self.bpt_error = true;
                            return;
                        }
                        value = raw;
                    }
                    None => {
                        // Table not found: same early return (rval unchanged).
                        self.bpt_error = true;
                        return;
                    }
                }
            }
        }

        // Step 2: AOFF/ASLO adjustment
        value -= self.aoff;
        if self.aslo != 0.0 {
            value /= self.aslo;
        }

        // Step 3: ROFF subtraction and rounding with i32 saturation
        value -= self.roff as f64;
        if value >= 0.0 {
            if value >= (i32::MAX as f64 - 0.5) {
                self.rval = i32::MAX;
            } else {
                self.rval = (value + 0.5) as i32;
            }
        } else if value > (i32::MIN as f64 - 0.5) {
            self.rval = (value - 0.5) as i32;
        } else {
            self.rval = i32::MIN;
        }
        // NOT `self.oraw = self.rval` — C advances `oraw` only from inside the
        // guarded post (`aoRecord.c:542`), so a cycle that moves RVAL without
        // posting it (a caput to ASLO/AOFF/LINR, then a process with VAL
        // unchanged) must leave ORAW stale for the next guarded cycle to find.
        // `note_secondary_value_posted` is the owner.
    }

    /// C `processAo` readback `raw → eng` inverse convert (devAsynInt32.c:
    /// 973-994), the exact inverse of [`AoRecord::convert`] in reverse field
    /// order: un-ROFF, un-ASLO/AOFF, then the raw→engineering linearisation.
    /// Used when asyn device support reads the device's current value back
    /// into an output record (init seed + driver readback callback) — the
    /// output analogue of `ai`'s `RVAL → VAL` `convert()`. Sets `VAL` from
    /// `RVAL`; the caller has already stored the raw value into `RVAL`.
    fn convert_readback(&mut self) {
        // C: value = rval + roff; if(aslo!=0) value *= aslo; value += aoff;
        let mut value = self.rval as f64 + self.roff as f64;
        if self.aslo != 0.0 {
            value *= self.aslo;
        }
        value += self.aoff;

        // C: NO_CONVERSION → passthrough; LINEAR/SLOPE → value*eslo + eoff;
        // LINR >= 3 → `cvtRawToEngBpt`. On a breakpoint failure (out of range
        // OR unresolvable table) C's asyn ao readback (`processAo`,
        // devAsynInt32.c:988-990) raises `recGblSetSevr(WRITE_ALARM,
        // INVALID_ALARM)` and `return -1`, which SKIPS `pr->val = value`
        // (:993) — so VAL is left unchanged and the record goes INVALID. This
        // is a different alarm from the forward `convert()`'s SOFT/MAJOR, so
        // it sets `readback_bpt_error` (consumed by `check_alarms`).
        self.bpt_error = false;
        self.readback_bpt_error = false;
        match self.linr {
            0 => {}
            1 | 2 => {
                value = value * self.eslo + self.eoff;
            }
            _ => {
                if self.bpt_table.is_none() {
                    self.bpt_table = self
                        .bpt_registry
                        .as_ref()
                        .and_then(|reg| reg.table_for_linr(self.linr));
                }
                match &self.bpt_table {
                    Some(table) => {
                        let (eng, status) = crate::server::cvt_bpt::cvt_raw_to_eng_bpt(
                            value,
                            table,
                            &mut self.lbrk,
                        );
                        if status == crate::server::cvt_bpt::BptStatus::OutOfRange {
                            // C `return -1`: VAL frozen, WRITE/INVALID alarm.
                            self.readback_bpt_error = true;
                            return;
                        }
                        value = eng;
                    }
                    None => {
                        // Unresolvable table: same failure path as C
                        // `cvtRawToEngBpt != 0`.
                        self.readback_bpt_error = true;
                        return;
                    }
                }
            }
        }
        self.val = value;
    }
}

impl Record for AoRecord {
    fn record_type(&self) -> &'static str {
        "ao"
    }

    /// C `aoRecord.c:181-184`: the `!dbLinkIsConstant(&prec->dol) && omsl ==
    /// menuOmslclosed_loop` gate calls `fetch_value` (`:433-455`), which unlike
    /// its siblings reads into a LOCAL — `dbGetLink(&prec->dol, DBR_DOUBLE,
    /// pvalue, 0, 0)` at `:444` — and adds VAL when `OIF = Incremental`
    /// (`:452-453`).
    fn fetches_dol_closed_loop(&self) -> bool {
        true
    }

    /// `aoRecord.c:329-350` `get_control_double` lists `OVAL` and `PVAL`
    /// alongside `VAL` and the alarm bands, all answering the record's
    /// `DRVH`/`DRVL` (not `HOPR`/`LOPR` — `ao` is the output record). The
    /// shared VAL-class set covers `VAL` and the bands; these two are the
    /// record-specific remainder, which would otherwise fall to the
    /// `default:` arm and report the DBF_DOUBLE range of ±1e300.
    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        ["OVAL", "PVAL"]
            .iter()
            .any(|f| field.eq_ignore_ascii_case(f))
            .then(|| FieldMetadataOverride {
                ctrl_limits: Some((self.drvh, self.drvl)),
                ..Default::default()
            })
    }

    /// C `devAoSoftRaw::write_ao` (`devAoSoftRaw.c:43-50`):
    /// `dbPutLink(&prec->out, DBR_LONG, &prec->rval, 1)` — the RAW word, not the
    /// engineering `OVAL` that `devAoSoft` writes.
    /// C `devAoSoftRaw.c::write_ao` (44): `dbPutLink(&prec->out, DBR_LONG,
    /// &prec->rval, 1)` — the raw word, unmasked, as DBR_LONG.
    fn raw_soft_output_value(&self) -> Option<EpicsValue> {
        Some(EpicsValue::Long(self.rval))
    }

    /// C `aoRecord.c:112-113`:
    /// `if (recGblInitConstantLink(&prec->dol, DBF_DOUBLE, &prec->val))
    ///      prec->udf = isnan(prec->val);`
    /// — the constant DOL both seeds VAL and DEFINES the record. Declared for
    /// the init-seed owner, which applies the isnan rule.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        vec![crate::server::record::ConstantInitLink::dol_to_val(
            "DOL", "VAL",
        )]
    }

    /// C `aoRecord.c:156,160-161` — the derived half of the init tail, run
    /// right after the constant load: `oval = pval = val`, then
    /// `oraw = rval; orbv = rbv`. softIoc with `field(DOL,"5")` reports OVAL=5
    /// at init.
    fn init_record_tail(&mut self) {
        self.oval = self.val;
        self.pval = self.val;
        self.oraw = self.rval;
        self.orbv = self.rbv;
    }

    /// C `aoRecord.c:157-159` — `mlst = alst = lalm = val`, all three, which is
    /// what the framework default would do anyway; spelled out because the
    /// derived half above is this record's own.
    fn seed_deadband_tracking(&mut self) {
        self.mlst = self.val;
        self.alst = self.val;
        self.lalm = self.val;
    }

    // C `aoRecord.c::process` IVOA=Set_output_to_IVOV (lines 207-213):
    //   prec->val = prec->ivov; value = prec->ivov; convert(prec, value);
    // The full convert() runs, so OVAL *and* RVAL reflect the converted
    // IVOV — not just OVAL/VAL. Re-running process() here reproduces the
    // C convert() (OROC rate-limit + linearisation + raw rounding) so a
    // hardware ao whose device support writes RVAL drives the correctly
    // converted invalid-output value.
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        let v = ivov
            .to_f64()
            .ok_or_else(|| CaError::TypeMismatch("IVOV".into()))?;
        self.val = v;
        self.process()?;
        Ok(())
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            // Legacy compatibility: if eslo==1.0 && eoff==0.0, set eoff from egul
            let saved_eoff = self.eoff;
            let saved_eslo = self.eslo;

            if self.eslo == 1.0 && self.eoff == 0.0 {
                self.eoff = self.egul;
            }

            // For SLOPE mode, restore user-configured eoff/eslo
            if self.linr == 1 {
                self.eoff = saved_eoff;
                self.eslo = saved_eslo;
            }

            // C `aoRecord.c:120`: `prec->init = TRUE;`
            self.init = ConvertPhase::Initial;
        }
        Ok(())
    }

    /// C `aoRecord.c::monitor` (`:531-549`) posts OVAL from INSIDE
    /// `if (monitor_mask)` with **no test of OVAL's own value**:
    ///
    /// ```c
    /// if(prec->omod) monitor_mask |= (DBE_VALUE|DBE_LOG);
    /// if(monitor_mask) {
    ///     prec->omod = FALSE;
    ///     db_post_events(prec,&prec->oval,monitor_mask);
    /// ```
    ///
    /// On a cycle whose only event is an alarm transition, `monitor_mask` is
    /// the alarm bits alone and `omod` is false (the output did not move), so
    /// C still sends OVAL to a `DBE_ALARM` subscriber. The port's aux post is
    /// change-detected, so an unchanged OVAL reached nobody — this hook is the
    /// unchanged-field arm, and it posts with the alarm bits, which is exactly
    /// `monitor_mask` on such a cycle.
    ///
    /// The CHANGED arm needs nothing: the framework's aux mask is
    /// `alarm_bits | DBE_VALUE | DBE_LOG`, and C's is
    /// `monitor_mask | DBE_VALUE | DBE_LOG` where the deadband bits it adds
    /// (`DBE_VALUE` from MDEL, `DBE_ARCHIVE` from ADEL) are already in that
    /// pair. RVAL/RBV are NOT named here: C gates each on its own
    /// `oraw != rval` / `orbv != rbv` test, so an unchanged one posts on no
    /// cycle at all.
    fn alarm_cycle_monitored_fields(&self) -> &'static [&'static str] {
        &["OVAL"]
    }

    /// C `aoRecord.c:539-548` posts RVAL and RBV from INSIDE
    /// `if (monitor_mask)`, each behind its own `oraw != rval` /
    /// `orbv != rbv` test, with the forced `monitor_mask|DBE_VALUE|DBE_LOG`.
    /// The default change-detected path has neither guard, so a cycle whose
    /// monitor mask is empty — VAL unchanged, no alarm move, `omod` false —
    /// posted an RVAL that C withholds until the next cycle that does fire.
    /// Reachable from a client with `caput AO.ASLO` (or AOFF/LINR) followed by
    /// `caput AO.PROC 1`: measured on softIoc R7.0.10-146, C emits no event
    /// there and leaves ORAW at its previous value.
    fn fields_posted_with_value_mask(&self) -> &'static [(&'static str, ValuePostGate)] {
        &[
            ("RVAL", ValuePostGate::OnChangeForced),
            ("RBV", ValuePostGate::OnChangeForced),
        ]
    }

    /// C `aoRecord.c:535` — `omod` opens the block above without opening VAL's
    /// own post, which is the case OROC creates: VAL sits at the target while
    /// OVAL steps toward it, so VAL crosses no deadband and the output still
    /// has to reach its monitors.
    fn take_secondary_value_mask(&mut self) -> crate::server::recgbl::EventMask {
        if std::mem::take(&mut self.omod) {
            crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG
        } else {
            crate::server::recgbl::EventMask::NONE
        }
    }

    /// C `if(prec->oraw != prec->rval) { db_post_events(...); prec->oraw =
    /// prec->rval; }` (`aoRecord.c:541-543`) and its RBV twin (`:545-548`),
    /// minus the post. `oraw`/`orbv` are written NOWHERE else — `convert()`
    /// used to assign `oraw = rval` eagerly, which made this comparison
    /// unable to ever be true.
    fn take_secondary_value_change(&mut self, field: &str) -> bool {
        match field {
            "RVAL" if self.oraw != self.rval => {
                self.oraw = self.rval;
                true
            }
            "RBV" if self.orbv != self.rbv => {
                self.orbv = self.rbv;
                true
            }
            _ => false,
        }
    }

    /// C never calls `db_post_events` on `oraw`/`orbv` — they are bookkeeping
    /// for the RVAL/RBV posts above, not fields any C `monitor()` emits. The
    /// generic change-detection loop would post them, so they are excluded
    /// from it here; a `caput` to either still posts through the put path, as
    /// it does for every field.
    fn event_posted_fields(&self) -> &'static [&'static str] {
        &["ORAW", "ORBV"]
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // DOL/OMSL handling is done by the framework (processing.rs):
        // - A real (DB/CA/PVA) link is fetched and applied to VAL before
        //   process() (OIF=0 Full, or OIF=1 Incremental: VAL = PVAL + DOL).
        // - A *constant* DOL is applied to VAL once at init_record
        //   (`recGblInitConstantLink` parity) and is NOT re-sourced here;
        //   C `aoRecord.c:442` gates fetch_value on `!dbLinkIsConstant`,
        //   so a client caput to VAL is never clobbered by the constant.
        //
        // skip_convert: asyn device support has already produced VAL/RVAL from
        // a raw device readback via convert_readback(); running the forward
        // convert() would recompute RVAL from VAL (and apply OROC/drive limits
        // C's readback path does not). C `processAo` returns from its readback
        // branch without calling convertAo (devAsynInt32.c:970-994) — mirror
        // that by skipping the forward convert this cycle.
        if !self.skip_convert {
            self.convert();
        }
        self.skip_convert = false;
        // C `aoRecord.c:237`: `prec->init = FALSE;` on the way out of EVERY
        // process, not just the first one.
        self.init = ConvertPhase::Converted;
        Ok(ProcessOutcome::complete())
    }

    fn set_device_did_compute(&mut self, did_compute: bool) {
        self.skip_convert = did_compute;
    }

    /// C `aoRecord.c` closed-loop DOL failure, both halves:
    ///
    /// ```c
    /// prec->val = prec->pval;                       /* fetch_value, :441 */
    /// status = dbGetLink(&prec->dol, DBR_DOUBLE, pvalue, 0, 0);
    /// if (status) { recGblSetSevr(prec, LINK_ALARM, INVALID_ALARM); return status; }
    /// ...
    /// if (!status) convert(prec, value);            /* process, :188 */
    /// ```
    ///
    /// The revert is written before the read, so a client `caput REC.VAL` made
    /// between cycles is discarded ("don't allow dbputs to val field") and a
    /// failed read publishes the last actual output, not the put. Skipping
    /// `convert` then freezes PVAL, OVAL and RVAL — with a non-zero OROC the
    /// output ramp holds instead of taking another step toward a setpoint the
    /// link can no longer confirm.
    fn closed_loop_dol_read_failed(&mut self) {
        self.val = self.pval;
        self.skip_convert = true;
    }

    fn install_breaktable_registry(
        &mut self,
        registry: std::sync::Arc<crate::server::cvt_bpt::BreakTableRegistry>,
    ) {
        self.bpt_registry = Some(registry);
        // Force a re-resolve on the next conversion in case LINR was already
        // set to a breakpoint table before the registry was installed.
        self.bpt_table = None;
    }

    /// Apply a raw device readback to this output record: store the raw value
    /// into `RVAL` and compute the engineering `VAL` via the `raw → eng`
    /// inverse convert (`AoRecord::convert_readback`). Returns `true` so the
    /// asyn store path treats `VAL` as final and skips the framework's forward
    /// convert (paired with `skip_convert` for the process-time readback). C
    /// `processAo`/`initAo` readback: `pr->rval = value; <raw→eng>; pr->val =
    /// value` (devAsynInt32.c:973-994, :955-957).
    fn apply_raw_readback(&mut self, raw: i32) -> bool {
        self.rval = raw;
        self.convert_readback();
        true
    }

    /// Apply an `asynFloat64` device readback: seed `VAL` with the forward
    /// `ASLO`/`AOFF` linear scaling (`VAL = value * ASLO + AOFF`). C `initAo`
    /// (devAsynFloat64.c:628-630) seeds the init read and `processAo`
    /// (:647-649) the output-callback read with the identical conversion;
    /// neither touches `RVAL` (float64 `ao` has no raw path). Returns `true` so
    /// the asyn store path skips the forward convert. The reverse scaling
    /// `(OVAL - AOFF) / ASLO` lives on the device-write side
    /// (devAsynFloat64.c:651-654).
    fn apply_float64_readback(&mut self, raw: f64) -> bool {
        let mut value = raw;
        if self.aslo != 0.0 {
            value *= self.aslo;
        }
        value += self.aoff;
        self.val = value;
        true
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "DRVH" => Some(EpicsValue::Double(self.drvh)),
            "DRVL" => Some(EpicsValue::Double(self.drvl)),
            "RVAL" => Some(EpicsValue::Long(self.rval)),
            "ORAW" => Some(EpicsValue::Long(self.oraw)),
            "RBV" => Some(EpicsValue::Long(self.rbv)),
            "ORBV" => Some(EpicsValue::Long(self.orbv)),
            "OVAL" => Some(EpicsValue::Double(self.oval)),
            "LINR" => Some(EpicsValue::Short(self.linr)),
            "EGUF" => Some(EpicsValue::Double(self.eguf)),
            "EGUL" => Some(EpicsValue::Double(self.egul)),
            "ESLO" => Some(EpicsValue::Double(self.eslo)),
            "EOFF" => Some(EpicsValue::Double(self.eoff)),
            "ROFF" => Some(EpicsValue::ULong(self.roff)),
            "ASLO" => Some(EpicsValue::Double(self.aslo)),
            "AOFF" => Some(EpicsValue::Double(self.aoff)),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "DOL" => Some(EpicsValue::String(self.dol.clone().into())),
            "OIF" => Some(EpicsValue::Short(self.oif)),
            "OROC" => Some(EpicsValue::Double(self.oroc)),
            "PVAL" => Some(EpicsValue::Double(self.pval)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Double(self.ivov)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "LALM" => Some(EpicsValue::Double(self.lalm)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            "INIT" => Some(self.init.as_field()),
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
                EpicsValue::Double(v) => {
                    self.val = v;
                    Ok(())
                }
                EpicsValue::Long(v) => {
                    self.val = v as f64;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "EGU" => match value {
                EpicsValue::String(v) => {
                    self.egu = v;
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
            "PREC" => match value {
                EpicsValue::Short(v) => {
                    self.prec = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DRVH" => match value {
                EpicsValue::Double(v) => {
                    self.drvh = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DRVL" => match value {
                EpicsValue::Double(v) => {
                    self.drvl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "RVAL" => match value {
                EpicsValue::Long(v) => {
                    self.rval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OVAL" => match value {
                EpicsValue::Double(v) => {
                    self.oval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            // SPC_LINCONV parity (aoRecord.c:249-267): LINR / EGUF / EGUL are
            // tagged `special(SPC_LINCONV)` in aoRecord.dbd. C's `special()`
            // rebases `eoff = egul` INSIDE `if ((linr == menuConvertLINEAR) &&
            // pdset->special_linconv)` — the rebase is the device support's,
            // and no soft dset supplies `special_linconv` (`devAoSoftRaw.c`,
            // like `devAiSoftRaw.c:32-35`, leaves that dset slot NULL). On an
            // ao the ungated rebase was worse than on an ai: EOFF feeds the
            // VAL→RVAL convert, so retuning the display range EGUL moved the
            // hardware output. See `ai.rs` for the full C excerpt.
            // `special(SPC_LINCONV)` in aoRecord.dbd — C `special()` sets
            // `prec->init = TRUE` for a put to any of LINR / EGUF / EGUL
            // (aoRecord.c:254), so the retuned conversion starts fresh.
            //
            // The arm C opens with — `if (pdset->common.number < 6) {
            // recGblDbaddrError(S_db_noMod, paddr, "ao: special"); return
            // S_db_noMod; }` (aoRecord.c:250-253), refusing a legacy 5-entry
            // DSET table that has no `special_linconv` slot — is deliberately
            // absent here: every base-supplied soft dset declares `{6, ...}`
            // (devAoSoft.c:39, devAoSoftRaw.c:38) so the refusal is
            // unreachable through base's own device support, and this port's
            // device support is the `DeviceSupport` trait
            // (`server/device_support.rs:96`), which has no `number` field and
            // no function-pointer table, so a short dset is unrepresentable by
            // type rather than rejected at run time.
            "LINR" => match value {
                EpicsValue::Short(v) => {
                    self.linr = v;
                    self.init = ConvertPhase::Initial;
                    // A LINR change re-selects the breakpoint table (C
                    // `special_linconv`); drop the cached table + interval
                    // index so the next conversion resolves the new LINR.
                    self.bpt_table = None;
                    self.lbrk = 0;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "EGUF" => match value {
                EpicsValue::Double(v) => {
                    self.eguf = v;
                    self.init = ConvertPhase::Initial;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "EGUL" => match value {
                EpicsValue::Double(v) => {
                    self.egul = v;
                    self.init = ConvertPhase::Initial;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ESLO" => match value {
                EpicsValue::Double(v) => {
                    self.eslo = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "EOFF" => match value {
                EpicsValue::Double(v) => {
                    self.eoff = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            // DBF_ULONG: accept the native unsigned carrier and the legacy signed
            // Long (bit-preserving), so a full-range caput (4294967295) stores
            // rather than being rejected as a type mismatch.
            "ROFF" => match value {
                EpicsValue::ULong(v) => {
                    self.roff = v;
                    Ok(())
                }
                EpicsValue::Long(v) => {
                    self.roff = v as u32;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ASLO" => match value {
                EpicsValue::Double(v) => {
                    self.aslo = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "AOFF" => match value {
                EpicsValue::Double(v) => {
                    self.aoff = v;
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
            "OIF" => match value {
                EpicsValue::Short(v) => {
                    self.oif = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OROC" => match value {
                EpicsValue::Double(v) => {
                    self.oroc = v;
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
            "IVOV" => match value {
                EpicsValue::Double(v) => {
                    self.ivov = v;
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

    /// `SIMM` is `DBF_MENU menu(menuSimm)` and `OIF` is `menu(aoOIF)`
    /// (`aoRecord.dbd.pod`); both served as `DBR_ENUM` with the menu's
    /// choice labels in `.dbd` index order. `SIMS`/`OLDSIMM`/`OMSL`/`IVOA`/
    /// `LINR` are shared menus resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_SIMM),
            "OIF" => Some(AO_OIF_CHOICES),
            _ => None,
        }
    }

    /// Breakpoint-table failures raise the same alarms C does. The forward
    /// `convert()` (eng→raw) raises `SOFT_ALARM/MAJOR_ALARM` (aoRecord.c:
    /// 494-499); the asyn `convert_readback()` (raw→eng) raises
    /// `WRITE_ALARM/INVALID_ALARM` (devAsynInt32.c:988). The two are mutually
    /// exclusive within a single process (forward XOR readback), each clearing
    /// the other's flag, so at most one fires.
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        // C uses the no-message `recGblSetSevr` on BOTH ao bpt paths —
        // forward `aoRecord.c:497` and asyn readback `devAsynInt32.c:988` —
        // so the ao AMSG stays empty on a bpt failure. This is asymmetric
        // with ai (`aiRecord.c:435` uses `recGblSetSevrMsg(…,"BPT Error")`):
        // ai carries the message, ao does not. Match C — no message here.
        if self.bpt_error {
            crate::server::recgbl::rec_gbl_set_sevr(
                common,
                crate::server::recgbl::alarm_status::SOFT_ALARM,
                crate::server::record::AlarmSeverity::Major,
            );
        }
        if self.readback_bpt_error {
            crate::server::recgbl::rec_gbl_set_sevr(
                common,
                crate::server::recgbl::alarm_status::WRITE_ALARM,
                crate::server::record::AlarmSeverity::Invalid,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPC_LINCONV parity (aoRecord.c:255-265). This test previously asserted
    /// the OPPOSITE — that an EGUL put under LINR=LINEAR rebases `eoff = egul`
    /// — and so pinned the defect (R18-97). C's rebase lives INSIDE
    /// `if ((linr == menuConvertLINEAR) && pdset->special_linconv)`, and the
    /// soft dsets supply no `special_linconv` (`devAiSoftRaw.c:32-35` leaves
    /// that dset slot NULL), so on a soft record the rebase never runs.
    ///
    /// Oracle (softIoc 7.0.10.1-DEV, `DTYP="Raw Soft Channel"`, LINR=LINEAR,
    /// EOFF=0): `dbpf T:AO.EGUL 3.5` → `dbgf T:AO.EOFF` = **0**. EGUL is a
    /// display-range field; retuning it must not move the hardware output
    /// through `RVAL = (val - eoff) / eslo`.
    #[test]
    fn egul_put_under_linear_does_not_rebase_eoff() {
        let mut rec = AoRecord::default();
        rec.linr = 2; // LINEAR
        rec.eoff = 1.5;

        rec.put_field("EGUL", EpicsValue::Double(7.25)).unwrap();

        assert_eq!(
            rec.eoff, 1.5,
            "no soft dset provides special_linconv, so C leaves EOFF alone"
        );
        assert_eq!(rec.egul, 7.25, "the display range itself is written");
    }

    /// The same holds for the other two `special(SPC_LINCONV)` fields — the
    /// gate is the dset, not the field.
    #[test]
    fn linr_and_eguf_puts_do_not_rebase_eoff() {
        let mut rec = AoRecord::default();
        rec.eoff = 1.5;
        rec.egul = 7.25;

        rec.put_field("LINR", EpicsValue::Short(2)).unwrap();
        assert_eq!(rec.eoff, 1.5, "LINR put: EOFF untouched");

        rec.put_field("EGUF", EpicsValue::Double(100.0)).unwrap();
        assert_eq!(rec.eoff, 1.5, "EGUF put: EOFF untouched");
    }

    /// SLOPE mode (LINR=1) likewise leaves user-configured eoff/eslo alone —
    /// this arm was already correct, and stays a regression guard.
    #[test]
    fn egul_put_under_slope_does_not_touch_eoff() {
        let mut rec = AoRecord::default();
        rec.linr = 1; // SLOPE
        rec.eoff = 1.5;

        rec.put_field("EGUL", EpicsValue::Double(7.25)).unwrap();

        assert_eq!(
            rec.eoff, 1.5,
            "EGUL put under SLOPE must leave user-configured eoff alone"
        );
    }

    /// A monotonic-up ramp table: raw 0..300 -> eng 0..30, slope 0.1.
    fn ramp_registry() -> std::sync::Arc<crate::server::cvt_bpt::BreakTableRegistry> {
        let mut reg = crate::server::cvt_bpt::BreakTableRegistry::new();
        reg.insert(
            crate::server::cvt_bpt::BrkTable::build(
                "ramp",
                &[(0.0, 0.0), (100.0, 10.0), (300.0, 30.0)],
            )
            .unwrap(),
        );
        std::sync::Arc::new(reg)
    }

    /// LINR >= 3 forward `convert()` (eng -> raw) resolves the table and
    /// inverts it (C `cvtEngToRawBpt`).
    #[test]
    fn linr_breaktable_converts_eng_to_raw_in_range() {
        let mut rec = AoRecord::default();
        rec.install_breaktable_registry(ramp_registry());
        rec.aslo = 1.0;
        rec.aoff = 0.0;
        rec.roff = 0;
        rec.put_field(
            "LINR",
            EpicsValue::Short(crate::server::cvt_bpt::LINR_FIRST_USER_TABLE),
        )
        .unwrap(); // "ramp" -> first user-table index (15)
        rec.val = 5.0; // engineering

        rec.convert();

        // eng 5 -> raw 50 (inverse of the ramp).
        assert_eq!(rec.rval, 50);
        assert!(
            !rec.bpt_error,
            "in-range conversion must not raise BPT error"
        );
    }

    /// Out of range: ao DOES early-return (aoRecord.c:494-499) — RVAL is left
    /// unchanged and the error is flagged, unlike ai which keeps the
    /// extrapolated value.
    #[test]
    fn linr_breaktable_out_of_range_early_returns_keeping_rval() {
        let mut rec = AoRecord::default();
        rec.install_breaktable_registry(ramp_registry());
        rec.aslo = 1.0;
        rec.put_field(
            "LINR",
            EpicsValue::Short(crate::server::cvt_bpt::LINR_FIRST_USER_TABLE),
        )
        .unwrap(); // "ramp" -> first user-table index (15)
        rec.rval = 12345; // sentinel: must survive the early return
        rec.val = 1000.0; // eng past the table's high end (30)

        rec.convert();

        assert!(rec.bpt_error, "out-of-range must raise BPT error");
        assert_eq!(rec.rval, 12345, "ao must NOT update RVAL on a BPT failure");
    }

    /// A LINR selecting no registered table early-returns with the error
    /// flagged and RVAL unchanged.
    #[test]
    fn linr_breaktable_not_found_early_returns_keeping_rval() {
        let mut rec = AoRecord::default();
        rec.install_breaktable_registry(ramp_registry());
        rec.aslo = 1.0;
        rec.put_field(
            "LINR",
            EpicsValue::Short(crate::server::cvt_bpt::LINR_FIRST_USER_TABLE + 1),
        )
        .unwrap(); // user-block index with no loaded table
        rec.rval = 777;
        rec.val = 5.0;

        rec.convert();

        assert!(rec.bpt_error, "unresolved table must raise BPT error");
        assert_eq!(
            rec.rval, 777,
            "ao must NOT update RVAL when table is missing"
        );
    }

    /// In-range asyn readback `convert_readback()` (raw -> eng) applies the
    /// table and raises no error.
    #[test]
    fn linr_breaktable_readback_converts_raw_to_eng_in_range() {
        let mut rec = AoRecord::default();
        rec.install_breaktable_registry(ramp_registry());
        rec.aslo = 1.0;
        rec.aoff = 0.0;
        rec.roff = 0;
        rec.put_field(
            "LINR",
            EpicsValue::Short(crate::server::cvt_bpt::LINR_FIRST_USER_TABLE),
        )
        .unwrap(); // "ramp" -> first user-table index (15)
        rec.rval = 200; // raw from the device

        rec.convert_readback();

        // raw 200 in [100,300] -> eng = 10 + (200-100)*0.1 = 20.0.
        assert_eq!(rec.val, 20.0);
        assert!(!rec.readback_bpt_error, "in-range readback must not error");
        assert!(!rec.bpt_error);
    }

    /// Out-of-range asyn readback: C `processAo` raises WRITE/INVALID and
    /// `return -1`, leaving VAL unchanged (devAsynInt32.c:988-993).
    #[test]
    fn linr_breaktable_readback_out_of_range_freezes_val_and_flags() {
        let mut rec = AoRecord::default();
        rec.install_breaktable_registry(ramp_registry());
        rec.aslo = 1.0;
        rec.aoff = 0.0;
        rec.roff = 0;
        rec.put_field(
            "LINR",
            EpicsValue::Short(crate::server::cvt_bpt::LINR_FIRST_USER_TABLE),
        )
        .unwrap(); // "ramp" -> first user-table index (15)
        rec.val = 99.0; // sentinel: must survive the readback failure
        rec.rval = 400; // raw past the table's high end (300)

        rec.convert_readback();

        assert!(
            rec.readback_bpt_error,
            "out-of-range readback must raise the readback BPT error"
        );
        assert_eq!(rec.val, 99.0, "C return -1 skips VAL update on failure");
    }

    /// Unresolvable-table asyn readback: same WRITE/INVALID + VAL-frozen path.
    #[test]
    fn linr_breaktable_readback_not_found_freezes_val_and_flags() {
        let mut rec = AoRecord::default();
        rec.install_breaktable_registry(ramp_registry());
        rec.aslo = 1.0;
        rec.put_field(
            "LINR",
            EpicsValue::Short(crate::server::cvt_bpt::LINR_FIRST_USER_TABLE + 1),
        )
        .unwrap(); // user-block index with no loaded table
        rec.val = 42.0;
        rec.rval = 200;

        rec.convert_readback();

        assert!(
            rec.readback_bpt_error,
            "unresolved table must flag readback"
        );
        assert_eq!(rec.val, 42.0, "VAL frozen when the table is missing");
    }

    /// The readback failure flag maps to WRITE_ALARM/INVALID_ALARM in
    /// check_alarms — distinct from the forward path's SOFT/MAJOR.
    #[test]
    fn readback_bpt_error_raises_write_invalid_alarm() {
        use crate::server::recgbl::alarm_status;
        use crate::server::record::{AlarmSeverity, CommonFields};

        let mut rec = AoRecord::default();
        rec.readback_bpt_error = true;
        let mut common = CommonFields::default();

        rec.check_alarms(&mut common);

        // check_alarms raises the *pending* alarm (nsev/nsta); the commit to
        // sevr/stat happens later in rec_gbl_reset_alarms.
        assert_eq!(common.nsev, AlarmSeverity::Invalid);
        assert_eq!(common.nsta, alarm_status::WRITE_ALARM);
        // C uses no-message recGblSetSevr on the ao readback bpt path
        // (devAsynInt32.c:988) — AMSG stays empty, unlike ai's "BPT Error".
        assert!(
            common.namsg.is_empty(),
            "ao bpt readback AMSG must be empty"
        );
    }

    /// The forward-convert failure (`bpt_error`) raises SOFT/MAJOR with an
    /// empty AMSG — C `aoRecord.c:497` uses no-message `recGblSetSevr`.
    #[test]
    fn forward_bpt_error_raises_soft_major_with_empty_amsg() {
        use crate::server::recgbl::alarm_status;
        use crate::server::record::{AlarmSeverity, CommonFields};

        let mut rec = AoRecord::default();
        rec.bpt_error = true;
        let mut common = CommonFields::default();

        rec.check_alarms(&mut common);

        assert_eq!(common.nsev, AlarmSeverity::Major);
        assert_eq!(common.nsta, alarm_status::SOFT_ALARM);
        assert!(common.namsg.is_empty(), "ao bpt forward AMSG must be empty");
    }
}
