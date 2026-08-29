use std::any::Any;
use std::time::Instant;

use super::dbd_generated;
use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::recgbl::{self, alarm_status};
use epics_base_rs::server::record::{
    AlarmSeverity, CommonFields, FieldDesc, FieldMetadataOverride, LinkType, ProcessAction,
    ProcessContext, ProcessOutcome, Record, link_field_type,
};
use epics_base_rs::types::{EpicsValue, PvString};

/// Record-specific `DBF_MENU` choice tables, in `.dbd` value order (the
/// index↔string mapping is wire-visible to clients). Source: the C
/// `epidRecord.dbd` menu definitions (std module). `FMOD` is
/// `menu(epidFeedbackMode)`; `FBON`/`FBOP` are `menu(epidFeedbackState)`.
/// The alarm severities are shared menus resolved by the base registry.
/// `SMSL` ("Setpoint Mode Select", `epidRecord.dbd:17`) is `menu(menuOmsl)`,
/// but its field *name* is record-specific — the base registry keys the
/// shared `menuOmsl` table by the standard name `OMSL` — so it is mapped
/// per record to [`epics_base_rs::server::record::dbd_generated::MENU_OMSL`].
/// `ReadDbLink` target for the bumpless-transfer OUTL readback.
///
/// Deliberately NOT a `.dbd` field: it names the internal staging cell
/// `EpidRecord::outl_seed`, not a CA-visible one. C reads OUTL inside
/// `do_pid`, after the MDT gate, so the value must not be observable (nor
/// monitor-posted) on a cycle C would have gated — see `pre_process_actions`.
const OUTL_SEED_FIELD: &str = "__OUTL_SEED";

/// Feedback mode for the epid record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i16)]
pub enum FeedbackMode {
    #[default]
    Pid = 0,
    MaxMin = 1,
}

impl From<i16> for FeedbackMode {
    fn from(v: i16) -> Self {
        match v {
            1 => FeedbackMode::MaxMin,
            _ => FeedbackMode::Pid,
        }
    }
}

/// Feedback on/off state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i16)]
pub enum FeedbackState {
    #[default]
    Off = 0,
    On = 1,
}

impl From<i16> for FeedbackState {
    fn from(v: i16) -> Self {
        match v {
            1 => FeedbackState::On,
            _ => FeedbackState::Off,
        }
    }
}

/// Extended PID feedback control record.
///
/// Ported from EPICS std module `epidRecord.c`.
/// Supports PID and Max/Min feedback modes with anti-windup,
/// bumpless turn-on, output deadband, and hysteresis-based alarms.
pub struct EpidRecord {
    // --- PID control ---
    /// Setpoint (VAL)
    pub val: f64,
    /// Setpoint mode: 0=supervisory, 1=closed_loop (SMSL)
    pub smsl: i16,
    /// Setpoint input link (STPL) — resolved by framework
    pub stpl: String,
    /// Controlled value input link (INP) — resolved by framework
    pub inp: String,
    /// Output link (OUTL) — resolved by framework
    pub outl: String,
    /// Readback trigger link (TRIG)
    pub trig: String,
    /// Trigger value (TVAL)
    pub tval: f64,
    /// Controlled value (CVAL), read-only
    pub cval: f64,
    /// Previous controlled value (CVLP), read-only
    pub cvlp: f64,
    /// Output value (OVAL), read-only
    pub oval: f64,
    /// Previous output value (OVLP), read-only
    pub ovlp: f64,
    /// Proportional gain (KP)
    pub kp: f64,
    /// Integral gain — repeats per second (KI)
    pub ki: f64,
    /// Derivative gain (KD)
    pub kd: f64,
    /// Proportional component (P), read-only
    pub p: f64,
    /// Previous P (PP), read-only
    pub pp: f64,
    /// Integral component (I), writable for bumpless init
    pub i: f64,
    /// Previous I (IP)
    pub ip: f64,
    /// Derivative component (D), read-only
    pub d: f64,
    /// Previous D (DP), read-only
    pub dp: f64,
    /// Error = setpoint - controlled value (ERR), read-only
    pub err: f64,
    /// Previous error (ERRP), read-only
    pub errp: f64,
    /// Delta time in seconds (DT), writable for fast mode
    pub dt: f64,
    /// Previous delta time (DTP)
    pub dtp: f64,
    /// Minimum delta time between calculations (MDT)
    pub mdt: f64,
    /// Feedback mode: PID or MaxMin (FMOD)
    pub fmod: i16,
    /// Feedback on/off (FBON)
    pub fbon: i16,
    /// Previous feedback on/off (FBOP)
    pub fbop: i16,
    /// Output deadband (ODEL)
    pub odel: f64,

    // --- Display ---
    /// Display precision (PREC)
    pub prec: i16,
    /// Engineering units (EGU)
    pub egu: PvString,
    /// High operating range (HOPR)
    pub hopr: f64,
    /// Low operating range (LOPR)
    pub lopr: f64,
    /// High drive limit (DRVH)
    pub drvh: f64,
    /// Low drive limit (DRVL)
    pub drvl: f64,

    // --- Alarm ---
    /// Hihi deviation limit (HIHI)
    pub hihi: f64,
    /// Lolo deviation limit (LOLO)
    pub lolo: f64,
    /// High deviation limit (HIGH)
    pub high: f64,
    /// Low deviation limit (LOW)
    pub low: f64,
    /// Hihi severity (HHSV)
    pub hhsv: i16,
    /// Lolo severity (LLSV)
    pub llsv: i16,
    /// High severity (HSV)
    pub hsv: i16,
    /// Low severity (LSV)
    pub lsv: i16,
    /// Alarm deadband / hysteresis (HYST)
    pub hyst: f64,
    /// Last value alarmed (LALM), read-only
    pub lalm: f64,

    // --- Monitor deadband ---
    /// Archive deadband (ADEL)
    pub adel: f64,
    /// Monitor deadband (MDEL)
    pub mdel: f64,
    /// Last value archived (ALST), read-only
    pub alst: f64,
    /// Last value monitored (MLST), read-only
    pub mlst: f64,

    // --- Internal time tracking ---
    /// Current time (CT) — used for delta-T computation
    pub(crate) ct: Instant,
    /// Previous time (CTP) — tracked for monitor change detection
    #[allow(dead_code)]
    pub(crate) ctp: Instant,

    // --- Internal flags ---
    /// Set by the framework (via set_device_did_compute) to indicate
    /// device support's read() already performed the PID computation.
    /// process() checks this to avoid running the built-in PID a second time.
    device_did_compute: bool,
    /// Set by `do_pid` when the `INP` link is a CONSTANT link (a literal
    /// value, not a PV reference). C `devEpidSoft.c:110-112`
    /// (`if (pepid->inp.type == CONSTANT) recGblSetSevr(...,SOFT_ALARM,
    /// INVALID_ALARM)`): with a constant INP there is "nothing to
    /// control", so the PID compute is skipped and SOFT/INVALID is
    /// raised. The framework `check_alarms` hook reads this flag and
    /// applies the severity via `recGblSetSevr`.
    pub inp_constant: bool,
    /// Framework-owned `dbCommon.udf`, pushed by the framework via
    /// [`Record::set_process_context`] immediately before `process()`.
    /// C `epidRecord.c:195` reads `pepid->udf` at the top of
    /// `process()` and skips `do_pid` entirely while it is set. The
    /// matching `UDF_ALARM` (C `epidRecord.c:199`,
    /// `recGblSetSevr(pepid,UDF_ALARM,pepid->udfs)`) is raised by the
    /// framework's centralised `rec_gbl_check_udf` after `process()`.
    udf: bool,
    /// Set by `process()` for a cycle on which the UDF gate skipped
    /// `do_pid`. C `epidRecord.c:201` `return(0)` is reached before
    /// `recGblFwdLink` and before `do_pid` writes the output, so on
    /// such a cycle the framework must NOT write the OUTL link
    /// (`multi_output_links`) or fire the forward link.
    compute_skipped: bool,
    /// True iff the device-support compute decided to write the OUTL
    /// output link this cycle. In C the OUTL `dbPutLink` lives INSIDE
    /// `do_pid` and fires only when `pepid->fbon && outl.type != CONSTANT`
    /// (`devEpidSoft.c:220`, `devEpidSoftCallback.c:256`), and only when
    /// `do_pid` reached that line — i.e. NOT on the sub-MDT early return
    /// (`devEpidSoft.c:125`) nor the CONSTANT-INP early return
    /// (`devEpidSoft.c:110-112`). The Fast support (`devEpidFast.c`)
    /// drives the DAC through its own output port and never writes OUTL.
    /// `do_pid` is the single owner: it clears this at entry (so every
    /// early return leaves it false) and sets it to `fbon != 0` only on
    /// the success path. Records whose device support never calls
    /// `do_pid` (Fast) leave it false → no framework OUTL write, matching
    /// C. The CONSTANT/empty-link skip (`outl.type != CONSTANT`) is the
    /// framework's no-op on a constant OUTL `WriteDbLink`.
    outl_write: bool,
    /// True iff the framework's input-link fetch for `STPL` actually
    /// produced a value this cycle — the framework analogue of C
    /// `RTN_SUCCESS(dbGetLink(&prec->stpl, ...))`. Pushed by the
    /// framework via [`Record::set_resolved_input_links`] after the
    /// `multi_input_links` fetch (STPL is only in that list when
    /// `SMSL == closed_loop`). C `epidRecord.c:191-193` clears `udf`
    /// only on this success — a STPL that is empty, or a DB/CA link
    /// whose fetch failed, leaves `udf` set.
    stpl_resolved: bool,
    /// The OUTL readback captured for THIS cycle's bumpless turn-on, or
    /// `None` if OUTL was not read.
    ///
    /// C reads OUTL *inside* `do_pid`, after the `if (dt<pepid->mdt)
    /// return(1);` gate (`devEpidSoft.c:125`), and lands the value straight in
    /// `do_pid`'s local `i` / `oval` (`:150-158`, `:178-184`) — a sub-MDT (or
    /// UDF-gated) cycle therefore never reads OUTL and never touches `.I` /
    /// `.OVAL`. The framework's `ReadDbLink` can only run *before* `process()`,
    /// so it lands here instead of in the CA-visible field: this cell is the
    /// staging slot, written only by that pre-process read and consumed only
    /// by `do_pid` at C's line. A gated cycle simply leaves it unconsumed —
    /// no field write, no monitor, and FBOP stays 0 so the next full cycle
    /// re-reads and seeds for real.
    pub(crate) outl_seed: Option<f64>,
    /// Framework-owned `dbCommon.dtyp`, pushed by the framework via
    /// [`Record::set_process_context`] before the input-link fetch.
    /// C device support for the epid record lives in two distinct
    /// DSETs — `devEpidSoft` (`devEpidSoft.c`, no TRIG handling) and
    /// `devEpidSoftCallback` (`devEpidSoftCallback.c`, which drives the
    /// TRIG readback link). [`Record::pre_input_link_actions`] checks
    /// this via [`EpidRecord::is_async_callback_dtyp`] to emit the TRIG
    /// write only when the callback DSET (`stdSupport.dbd:14`, DTYP
    /// `"Async Soft Channel"`) is selected.
    dtyp: String,
    /// Epid-owned `dbCommon.udf` projection, returned by
    /// [`Record::value_is_undefined`]. C `epidRecord.c` has
    /// `special = NULL` (line 105) — there is no operator UDF clear,
    /// and `udf` is cleared ONLY by the two C conditions:
    ///
    /// - `epidRecord.c:160-164` init: a CONSTANT `STPL` link holding
    ///   a valid constant clears `udf` (mirrored by
    ///   [`Record::post_init_finalize_undef`] / a CONSTANT `STPL`
    ///   making `value_is_undefined()` return `false`).
    /// - `epidRecord.c:191-193` process: closed-loop (`SMSL=1`) with
    ///   a successful `dbGetLink(stpl)` clears `udf`.
    ///
    /// `process()` recomputes this each cycle; the framework's
    /// post-process `common.udf = value_is_undefined()` then keeps a
    /// supervisory / empty-STPL epid permanently undefined, exactly as
    /// C leaves `udf == TRUE` forever for such a record.
    value_undefined: bool,
    /// Which pass of a CA-type TRIG link this record is in.
    ///
    /// Owned end-to-end by [`EpidRecord::pre_input_link_actions`], the
    /// single site that fires TRIG: it advances `Idle ->
    /// AwaitingCallback` when it fires an asynchronous trigger and back
    /// to `Idle` on the callback (reprocess) pass. This is C's
    /// `pepid->pact`, which `devEpidSoftCallback.c:116` reads as
    /// `if (!pepid->pact)` to skip the whole trigger block on the
    /// second pass.
    ///
    /// C `devEpidSoftCallback.c:143-145`: a CA TRIG link fires the
    /// readback trigger asynchronously (`dbCaPutLinkCallback`), sets
    /// `pepid->pact = TRUE` and `return(0)`. C `epidRecord.c:207`
    /// `if (!pact && pepid->pact) return(0)` then returns BEFORE
    /// `recGblGetTimeStamp` / `checkAlarms` / `monitor` /
    /// `recGblFwdLink` — so the trigger pass runs NONE of the
    /// process tail; the tail runs exactly once, on the callback
    /// (reprocess) pass.
    ///
    /// `process()` reads (but does not clear) `AwaitingCallback` and
    /// returns `ProcessOutcome::async_pending()`, which makes the
    /// framework skip the alarm/timestamp/snapshot/OUT/FLNK tail for
    /// the trigger cycle while still executing the emitted
    /// `WriteDbLink{TRIG}` + `ReprocessAfter`. The reprocess pass runs
    /// `do_pid` and the tail exactly once.
    ca_trig: CaTrigPhase,
}

/// Which pass of an asynchronous (CA-link) TRIG readback an epid record
/// is in — the port's `pepid->pact` for the trigger path.
///
/// Two variants rather than a bool because the state has exactly one
/// owner and one transition each way; a bool invited a second copy of it
/// to live in the device support, where the record could not see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaTrigPhase {
    /// No trigger outstanding. The next process pass may fire one.
    #[default]
    Idle,
    /// A `dbCaPutLinkCallback`-equivalent trigger is outstanding; the
    /// next pass is the callback pass and must run the PID, not
    /// re-trigger.
    AwaitingCallback,
}

impl Default for EpidRecord {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            val: 0.0,
            smsl: 0,
            stpl: String::new(),
            inp: String::new(),
            outl: String::new(),
            trig: String::new(),
            tval: 0.0,
            cval: 0.0,
            cvlp: 0.0,
            oval: 0.0,
            ovlp: 0.0,
            kp: 0.0,
            ki: 0.0,
            kd: 0.0,
            p: 0.0,
            pp: 0.0,
            i: 0.0,
            ip: 0.0,
            d: 0.0,
            dp: 0.0,
            err: 0.0,
            errp: 0.0,
            dt: 0.0,
            dtp: 0.0,
            mdt: 0.0,
            fmod: 0,
            fbon: 0,
            fbop: 0,
            odel: 0.0,
            prec: 0,
            egu: PvString::new(),
            hopr: 0.0,
            lopr: 0.0,
            drvh: 0.0,
            drvl: 0.0,
            hihi: 0.0,
            lolo: 0.0,
            high: 0.0,
            low: 0.0,
            hhsv: 0,
            llsv: 0,
            hsv: 0,
            lsv: 0,
            hyst: 0.0,
            lalm: 0.0,
            adel: 0.0,
            mdel: 0.0,
            alst: 0.0,
            mlst: 0.0,
            ct: now,
            ctp: now,
            device_did_compute: false,
            inp_constant: false,
            dtyp: String::new(),
            udf: true,
            compute_skipped: false,
            outl_write: false,
            stpl_resolved: false,
            outl_seed: None,
            // C `epidRecord.c` init: `udf` starts TRUE and is cleared
            // only by the two clear-conditions — see `value_undefined`.
            value_undefined: true,
            ca_trig: CaTrigPhase::Idle,
        }
    }
}

impl EpidRecord {
    /// Decide the alarm condition using hysteresis-based threshold
    /// comparison on VAL. Ported from epidRecord.c `checkAlarms()`,
    /// which mirrors `aiRecord.c::checkAlarms` — per-level hysteresis
    /// against VAL with `lalm` tracking the last-alarmed threshold.
    ///
    /// Returns `Some((stat, sevr, alev))` where `stat` is the canonical
    /// `epicsAlarmCondition` status code (`HIHI_ALARM`, `HIGH_ALARM`,
    /// `LOLO_ALARM`, `LOW_ALARM`), `sevr` the configured severity, and
    /// `alev` the threshold that fired (the candidate `lalm` value).
    /// Returns `None` when VAL is inside the (hysteresis-adjusted) limits.
    ///
    /// `lalm` (last-alarmed threshold) is committed by the caller, NOT
    /// here, for the alarm case. C `aiRecord.c:403-406` gates the `lalm`
    /// update on `recGblSetSevr` actually raising the severity:
    /// `if (recGblSetSevr(...)) prec->lalm = alev;`. A lower-severity
    /// alarm that loses to an already-higher pending severity must NOT
    /// advance `lalm`, or the hysteresis band would be silently re-based.
    /// The [`Record::check_alarms`] trait hook below performs that gate.
    ///
    /// The no-alarm case writes `lalm = val` here unconditionally,
    /// matching C `aiRecord.c:409` (`prec->lalm = val;` — not gated).
    pub fn check_alarms(&mut self) -> Option<(u16, AlarmSeverity, f64)> {
        let val = self.val;
        let hyst = self.hyst;
        let lalm = self.lalm;

        // HIHI alarm
        if self.hhsv != 0 && (val >= self.hihi || (lalm == self.hihi && val >= self.hihi - hyst)) {
            return Some((
                alarm_status::HIHI_ALARM,
                AlarmSeverity::from_u16(self.hhsv as u16),
                self.hihi,
            ));
        }

        // LOLO alarm
        if self.llsv != 0 && (val <= self.lolo || (lalm == self.lolo && val <= self.lolo + hyst)) {
            return Some((
                alarm_status::LOLO_ALARM,
                AlarmSeverity::from_u16(self.llsv as u16),
                self.lolo,
            ));
        }

        // HIGH alarm
        if self.hsv != 0 && (val >= self.high || (lalm == self.high && val >= self.high - hyst)) {
            return Some((
                alarm_status::HIGH_ALARM,
                AlarmSeverity::from_u16(self.hsv as u16),
                self.high,
            ));
        }

        // LOW alarm
        if self.lsv != 0 && (val <= self.low || (lalm == self.low && val <= self.low + hyst)) {
            return Some((
                alarm_status::LOW_ALARM,
                AlarmSeverity::from_u16(self.lsv as u16),
                self.low,
            ));
        }

        // No alarm — C `aiRecord.c:409` resets LALM to VAL unconditionally.
        self.lalm = val;
        None
    }

    /// Mark this cycle as a CA-TRIG trigger pass.
    ///
    /// Called by [`crate::device_support::epid_soft_callback::
    /// EpidSoftCallbackDeviceSupport::read`] on the first pass of a
    /// CA-type TRIG link, before `process()` runs. `process()` consumes
    /// the flag and returns `ProcessOutcome::async_pending()` so the
    /// trigger pass skips the process tail (checkAlarms / monitor /
    /// recGblFwdLink) — C `devEpidSoftCallback.c:143-145` +
    /// `epidRecord.c:205-210`. See `EpidRecord::ca_trig`.
    pub fn ca_trig_phase(&self) -> CaTrigPhase {
        self.ca_trig
    }

    /// True when DTYP selects the `devEpidSoftCB` DSET — the only epid
    /// device support that touches the TRIG readback link.
    ///
    /// The string is the one `stdSupport.dbd:14` registers,
    /// `device(epid,CONSTANT,devEpidSoftCB,"Async Soft Channel")`, and
    /// is what a `.db` written against the C module sets (this crate
    /// ships one: `db/async_pid_control.db`). epid deliberately reuses
    /// base's soft-channel DTYP strings for record-specific behaviour,
    /// so device selection is by the (record type, DTYP) pair and the
    /// match belongs here in the epid body rather than in base's
    /// `is_soft_dtyp`, which is keyed on DTYP alone.
    pub fn is_async_callback_dtyp(&self) -> bool {
        self.dtyp == "Async Soft Channel"
    }

    /// Owner setter for `EpidRecord::outl_write`. Only `do_pid` calls
    /// this — it clears the flag at entry and re-enables it per `fbon`
    /// on the success path, mirroring C's OUTL `dbPutLink` gate
    /// (`devEpidSoft.c:220`). Keeping it private to a setter preserves
    /// the single-owner invariant.
    pub fn set_outl_write(&mut self, write: bool) {
        self.outl_write = write;
    }

    /// Update monitor tracking fields. Returns list of fields that changed.
    /// Ported from epidRecord.c `monitor()`.
    pub fn update_monitors(&mut self) {
        // Update previous-value fields for change detection
        self.ovlp = self.oval;
        self.pp = self.p;
        self.ip = self.i;
        self.dp = self.d;
        self.dtp = self.dt;
        self.errp = self.err;
        self.cvlp = self.cval;

        // VAL deadband baselines (MLST/ALST) are NOT advanced here. C
        // `epidRecord.c:346-374` `monitor()` computes `delta = mlst - val`,
        // posts VAL when `delta > mdel`, and only THEN sets `mlst = val`
        // — the post and the advance are one owner. In Rust that owner is
        // the framework's `check_deadband_ext`
        // (`record_instance.rs:2180-2203`): it reads MLST, fires the VAL
        // monitor, then advances `mlst`/`alst` via `put_coerced`. Advancing
        // them here (before that runs) made the framework see a zero delta
        // and silently suppress every VAL post. `update_monitors` owns only
        // the epid-specific previous-value fields above (`pp`/`ip`/`dp`/
        // `cvlp`/...), not the MLST/ALST deadband state.
    }
}

impl Record for EpidRecord {
    fn record_type(&self) -> &'static str {
        "epid"
    }

    /// `epidRecord.c:238-261` and `:263-286` are one switch with TWO windows,
    /// not one: `VAL`/`HIHI`/`HIGH`/`LOW`/`LOLO`/`CVAL` answer `hopr`/`lopr`,
    /// and `OVAL`/`P`/`I`/`D` — the controller output and its three gain terms
    /// — answer `drvh`/`drvl` instead, because they live on the actuator's
    /// scale and not the process variable's. The first window is the
    /// record-level cache; the second cannot be, since a record has only one
    /// cache and epid needs two ranges at once.
    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        ["OVAL", "P", "I", "D"]
            .iter()
            .any(|f| field.eq_ignore_ascii_case(f))
            .then(|| FieldMetadataOverride {
                disp_limits: Some((self.drvh, self.drvl)),
                ctrl_limits: Some((self.drvh, self.drvl)),
                ..Default::default()
            })
    }

    /// Bumpless-transfer readback — C `devEpidSoft.c:153-158` (PID) and
    /// `devEpidSoft.c:178-184` / `devEpidSoftCallback.c:214-220`
    /// (MaxMin).
    ///
    /// On the feedback OFF->ON edge (`FBOP==0 && FBON!=0`) C seeds the
    /// turn-on state from the `OUTL` output link's *actual current
    /// value* via `dbGetLink(&pepid->outl, DBR_DOUBLE, ...)`, guarded by
    /// `outl.type != CONSTANT`. The seeded field differs by FMOD:
    ///
    ///   - PID (`fmod==0`), C `devEpidSoft.c:155`:
    ///     `dbGetLink(&pepid->outl, DBR_DOUBLE, &i, ...)` — the OUTL
    ///     readback lands in the integral term `I`.
    ///   - MaxMin (`fmod==1`), C `devEpidSoft.c:181` /
    ///     `devEpidSoftCallback.c:217`:
    ///     `dbGetLink(&pepid->outl, DBR_DOUBLE, &oval, ...)` — the OUTL
    ///     readback lands in the output value `OVAL`.
    ///
    /// The Rust framework's `ReadDbLink` pre-process action performs
    /// exactly that synchronous read of the DB link's target value, but it
    /// can only run BEFORE `process()` / `do_pid`, whereas C reads OUTL
    /// *after* the `dt < MDT` gate (`devEpidSoft.c:125`) and the record's
    /// UDF gate (`epidRecord.c:195`). So the read does NOT land in `.I` /
    /// `.OVAL` here — it lands in `EpidRecord::outl_seed`, and `do_pid`
    /// consumes it at C's line. A cycle that C would have gated leaves the
    /// staged value unconsumed: no field write, no monitor, and FBOP stays
    /// 0 so the next ungated cycle re-reads and seeds for real.
    ///
    /// `FBOP` still holds the *previous* cycle's `FBON` at this point
    /// (it is committed at the end of `do_pid`), so the edge is
    /// detectable here. The action is emitted only for a non-CONSTANT
    /// `OUTL` link, mirroring C's `outl.type != CONSTANT` guard — for a
    /// CONSTANT/empty `OUTL` nothing is staged and the seeded field keeps
    /// its prior value.
    fn pre_process_actions(&mut self) -> Vec<ProcessAction> {
        // The staged readback is per-cycle: whatever a previous cycle left
        // behind must not be mistaken for this cycle's OUTL value.
        self.outl_seed = None;
        let edge = self.fbon != 0 && self.fbop == 0;
        if edge {
            match link_field_type(&self.outl) {
                LinkType::Db | LinkType::Ca => {
                    return vec![ProcessAction::ReadDbLink {
                        link_field: "OUTL",
                        target_field: OUTL_SEED_FIELD,
                    }];
                }
                _ => {}
            }
        }
        Vec::new()
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // In the C code, process() always calls pdset->do_pid() — a custom
        // device support function unique to the epid record. In Rust, the
        // framework has a generic DeviceSupport trait with read()/write()
        // and no custom function pointers.
        //
        // For non-"Soft Channel" DTYPs (e.g. "Fast Epid"), the framework
        // calls DeviceSupport::read() BEFORE process(). That read() runs
        // the driver-specific PID and sets pid_done = true.
        //
        // For "Soft Channel" or no device support, the framework skips
        // read(), so pid_done stays false and process() runs the built-in
        // PID here.

        // C `epidRecord.c:189-203`: the UDF gate is taken only on the
        // non-callback pass (`if (!pact)`). `device_did_compute` is the
        // Rust equivalent of "device support already ran do_pid" — the
        // callback pass — so the gate applies only when it is false.
        //
        // C `epidRecord.c` clears `udf` ONLY at two sites (`special` is
        // NULL — there is no operator UDF clear):
        //   - `epidRecord.c:160-164` init: a CONSTANT `STPL` link with a
        //     valid constant. A constant link's value never changes, so
        //     it is "defined" on every cycle thereafter.
        //   - `epidRecord.c:191-193` process: closed-loop (`SMSL=1`)
        //     with `RTN_SUCCESS(dbGetLink(&prec->stpl, ...))` — an
        //     ACTUAL fetch success. `self.stpl_resolved` is the
        //     framework's report of exactly that (a STPL that is empty,
        //     or whose DB/CA fetch failed, leaves it false).
        // Otherwise `udf` stays TRUE forever and C `epidRecord.c:195`
        // `return(0)` skips `do_pid` every cycle — e.g. a supervisory
        // (`SMSL=0`) epid with an empty/non-constant STPL NEVER runs
        // `do_pid`.
        //
        // `self.udf` is the framework `dbCommon.udf` pushed before
        // `process()`; it is last cycle's value because the framework
        // recomputes `common.udf` (from `value_is_undefined()`) only
        // *after* `process()`. C reads `pepid->udf` at process-start
        // identically. `udf` is sticky-false: once C clears it, it is
        // never re-set — so the gate keys off `self.udf`, and a closed-
        // loop epid whose STPL later fails keeps running `do_pid`.
        //
        // `value_undefined` is recomputed here for the framework's
        // post-process `common.udf = value_is_undefined()`.
        self.compute_skipped = false;

        // CA-TRIG trigger pass — C `devEpidSoftCallback.c:143-145` +
        // `epidRecord.c:205-210`. `pre_input_link_actions` ran first
        // this cycle, saw a CA-type TRIG link, fired the asynchronous
        // readback trigger (`WriteDbLink{TRIG}` + `ReprocessAfter`) and
        // moved `ca_trig` to `AwaitingCallback` — the analogue of C
        // `do_pid` setting `pepid->pact = TRUE` and `return(0)`. The
        // phase is NOT cleared here: `pre_input_link_actions` owns both
        // transitions, and clears it on the callback pass.
        //
        // C `epidRecord.c:207` `if (!pact && pepid->pact) return(0)`
        // then returns BEFORE `recGblGetTimeStamp` / `checkAlarms` /
        // `monitor` / `recGblFwdLink`: the trigger pass runs NONE of
        // the process tail. Return `async_pending` so the framework
        // skips the alarm/timestamp/snapshot/OUT/FLNK tail for this
        // cycle. The emitted actions were merged by the framework and
        // are still executed; the reprocess pass runs
        // `do_pid` and the tail exactly once.
        //
        // `device_did_compute` is cleared here because the trigger pass
        // performed NO compute — without this reset the reprocess pass
        // could observe a stale `true`.
        if self.ca_trig == CaTrigPhase::AwaitingCallback {
            self.device_did_compute = false;
            return Ok(ProcessOutcome::async_pending());
        }

        // C clear-conditions, evaluated at process-start:
        //  - CONSTANT STPL link  → init `recGblInitConstantLink` cleared
        //    udf permanently (`epidRecord.c:160-164`).
        //  - closed-loop STPL fetch succeeded this cycle
        //    (`epidRecord.c:191-193`).
        //
        // `stpl_resolved` is a per-cycle signal: consume it and reset
        // so a later `process_local`-path cycle (which performs no
        // link resolution and never calls `set_resolved_input_links`)
        // cannot read a stale "resolved" from an earlier links-path
        // cycle.
        let stpl_resolved = self.stpl_resolved;
        self.stpl_resolved = false;
        let stpl_clears_udf =
            link_field_type(&self.stpl) == LinkType::Constant || (self.smsl == 1 && stpl_resolved);
        // udf state this cycle: undefined unless already cleared
        // (`!self.udf`) or a clear-condition fires now.
        self.value_undefined = self.udf && !stpl_clears_udf;
        if !self.device_did_compute {
            if self.value_undefined {
                // C `epidRecord.c:195-202`: while `udf==TRUE`, skip
                // `do_pid` entirely and `return 0` — *before*
                // `recGblGetTimeStamp`, `checkAlarms`, `monitor` and
                // `recGblFwdLink`. The framework's centralised UDF
                // check (`rec_gbl_check_udf`, run after process())
                // raises `UDF_ALARM` with `udfs` severity, matching C's
                // `recGblSetSevr(pepid, UDF_ALARM, pepid->udfs)`.
                //
                // `update_monitors()` is deliberately NOT called here:
                // C's early `return(0)` skips `monitor()`, so the
                // previous-value fields (`pp`/`ip`/`dp`/...) and the
                // `mlst`/`alst` deadband baselines must NOT advance
                // while the record is undefined.
                //
                // C `return(0)` is reached before `recGblFwdLink` and
                // the `do_pid` output write. The Rust framework drives
                // the OUTL write (`multi_output_links`) and FLNK; flag
                // this cycle so `multi_output_links` and
                // `should_fire_forward_link` suppress them — otherwise
                // a stale OVAL would be pushed to the OUTL target.
                self.device_did_compute = false;
                self.compute_skipped = true;
                return Ok(ProcessOutcome::complete());
            }
        }

        if !self.device_did_compute {
            crate::device_support::epid_soft::EpidSoftDeviceSupport::do_pid(self);
        }
        self.device_did_compute = false; // Reset for next cycle

        // Alarm evaluation is NOT done here. The framework invokes the
        // `Record::check_alarms` trait hook (below) after `process()`,
        // which is where the computed severity is applied to SEVR/STAT
        // via `recGblSetSevr`. Calling the inherent `check_alarms` here
        // would advance `lalm` an extra time and double-step the
        // hysteresis state, so it is deliberately omitted.
        self.update_monitors();

        // Device support actions are now merged by the framework
        let actions = Vec::new();
        Ok(ProcessOutcome::complete_with(actions))
    }

    /// Per-record alarm hook — C `epidRecord.c::checkAlarms`.
    ///
    /// The framework calls this after `process()`; it computes the
    /// HIHI/HIGH/LOW/LOLO condition (with `lalm` hysteresis) via the
    /// inherent [`EpidRecord::check_alarms`] and applies the result to
    /// the record's pending alarm state with `recGblSetSevr`. That
    /// accumulates into `nsta`/`nsev` (raise-only / maximize-severity),
    /// which the framework later transfers to `STAT`/`SEVR` via
    /// `recGblResetAlarms`. Returning `None` raises nothing, so a value
    /// that stays inside the limits leaves the record un-alarmed and a
    /// held value does not re-fire.
    fn check_alarms(&mut self, common: &mut CommonFields) {
        // C `devEpidSoft.c:110-112` / `devEpidSoftCallback.c:112-114`:
        // a CONSTANT `INP` link means "nothing to control" — raise
        // SOFT_ALARM/INVALID_ALARM. `do_pid` set `inp_constant` and
        // skipped the compute; apply the severity here (the framework
        // calls this hook after `process()`).
        if self.inp_constant {
            recgbl::rec_gbl_set_sevr(common, alarm_status::SOFT_ALARM, AlarmSeverity::Invalid);
        }
        if let Some((stat, sevr, alev)) = EpidRecord::check_alarms(self) {
            // C `aiRecord.c:405-406`: `if (recGblSetSevr(...)) prec->lalm = alev;`
            // — the LALM update is gated on `recGblSetSevr` returning TRUE,
            // i.e. on the alarm actually raising the pending severity.
            if recgbl::rec_gbl_set_sevr(common, stat, sevr) {
                self.lalm = alev;
            }
        }
    }

    /// C `epidRecord.c:376` REASSIGNS `monitor_mask = DBE_LOG|DBE_VALUE` after
    /// VAL's own post, so every secondary the rest of `monitor()` posts
    /// (:377-406) carries a LITERAL `DBE_VALUE | DBE_LOG` — this cycle's alarm
    /// bits are discarded, unlike VAL's post (:373), which keeps them. A
    /// `DBE_ALARM`-only subscriber on `.OVAL`/`.P`/`.I`/... is therefore
    /// notified on no cycle at all.
    ///
    /// C's list is OVAL, P, I, D, CT, DT, ERR, CVAL; `CT` is `DBF_NOACCESS`
    /// (`epidRecord.dbd:226`) and has no CA-visible field, leaving these seven.
    fn fields_posted_without_alarm_bits(&self) -> &'static [&'static str] {
        &["OVAL", "P", "I", "D", "DT", "ERR", "CVAL"]
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "SMSL" => Some(EpicsValue::Short(self.smsl)),
            "STPL" => Some(EpicsValue::String(self.stpl.clone().into())),
            "INP" => Some(EpicsValue::String(self.inp.clone().into())),
            "OUTL" => Some(EpicsValue::String(self.outl.clone().into())),
            "TRIG" => Some(EpicsValue::String(self.trig.clone().into())),
            "TVAL" => Some(EpicsValue::Double(self.tval)),
            "CVAL" => Some(EpicsValue::Double(self.cval)),
            "CVLP" => Some(EpicsValue::Double(self.cvlp)),
            "OVAL" => Some(EpicsValue::Double(self.oval)),
            "OVLP" => Some(EpicsValue::Double(self.ovlp)),
            "KP" => Some(EpicsValue::Double(self.kp)),
            "KI" => Some(EpicsValue::Double(self.ki)),
            "KD" => Some(EpicsValue::Double(self.kd)),
            "P" => Some(EpicsValue::Double(self.p)),
            "PP" => Some(EpicsValue::Double(self.pp)),
            "I" => Some(EpicsValue::Double(self.i)),
            "IP" => Some(EpicsValue::Double(self.ip)),
            "D" => Some(EpicsValue::Double(self.d)),
            "DP" => Some(EpicsValue::Double(self.dp)),
            "ERR" => Some(EpicsValue::Double(self.err)),
            "ERRP" => Some(EpicsValue::Double(self.errp)),
            "DT" => Some(EpicsValue::Double(self.dt)),
            "DTP" => Some(EpicsValue::Double(self.dtp)),
            "MDT" => Some(EpicsValue::Double(self.mdt)),
            "FMOD" => Some(EpicsValue::Short(self.fmod)),
            "FBON" => Some(EpicsValue::Short(self.fbon)),
            "FBOP" => Some(EpicsValue::Short(self.fbop)),
            "ODEL" => Some(EpicsValue::Double(self.odel)),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "DRVH" => Some(EpicsValue::Double(self.drvh)),
            "DRVL" => Some(EpicsValue::Double(self.drvl)),
            "HIHI" => Some(EpicsValue::Double(self.hihi)),
            "LOLO" => Some(EpicsValue::Double(self.lolo)),
            "HIGH" => Some(EpicsValue::Double(self.high)),
            "LOW" => Some(EpicsValue::Double(self.low)),
            "HHSV" => Some(EpicsValue::Short(self.hhsv)),
            "LLSV" => Some(EpicsValue::Short(self.llsv)),
            "HSV" => Some(EpicsValue::Short(self.hsv)),
            "LSV" => Some(EpicsValue::Short(self.lsv)),
            "HYST" => Some(EpicsValue::Double(self.hyst)),
            "LALM" => Some(EpicsValue::Double(self.lalm)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
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
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SMSL" => match value {
                EpicsValue::Short(v) => {
                    self.smsl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "STPL" => match value {
                EpicsValue::String(v) => {
                    self.stpl = v.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "INP" => match value {
                EpicsValue::String(v) => {
                    self.inp = v.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OUTL" => match value {
                EpicsValue::String(v) => {
                    self.outl = v.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "TRIG" => match value {
                EpicsValue::String(v) => {
                    self.trig = v.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "TVAL" => match value {
                EpicsValue::Double(v) => {
                    self.tval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "KP" => match value {
                EpicsValue::Double(v) => {
                    self.kp = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "KI" => match value {
                EpicsValue::Double(v) => {
                    self.ki = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "KD" => match value {
                EpicsValue::Double(v) => {
                    self.kd = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "I" => match value {
                EpicsValue::Double(v) => {
                    self.i = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "IP" => match value {
                EpicsValue::Double(v) => {
                    self.ip = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DT" => match value {
                EpicsValue::Double(v) => {
                    self.dt = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "MDT" => match value {
                EpicsValue::Double(v) => {
                    self.mdt = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "FMOD" => match value {
                EpicsValue::Short(v) => {
                    self.fmod = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "FBON" => match value {
                EpicsValue::Short(v) => {
                    self.fbon = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ODEL" => match value {
                EpicsValue::Double(v) => {
                    self.odel = v;
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
            "HIHI" => match value {
                EpicsValue::Double(v) => {
                    self.hihi = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LOLO" => match value {
                EpicsValue::Double(v) => {
                    self.lolo = v;
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
            "LOW" => match value {
                EpicsValue::Double(v) => {
                    self.low = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "HHSV" => match value {
                EpicsValue::Short(v) => {
                    self.hhsv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LLSV" => match value {
                EpicsValue::Short(v) => {
                    self.llsv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "HSV" => match value {
                EpicsValue::Short(v) => {
                    self.hsv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LSV" => match value {
                EpicsValue::Short(v) => {
                    self.lsv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "HYST" => match value {
                EpicsValue::Double(v) => {
                    self.hyst = v;
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
            // Read-only fields
            "CVAL" | "CVLP" | "OVAL" | "OVLP" | "P" | "PP" | "D" | "DP" | "ERR" | "ERRP"
            | "DTP" | "FBOP" | "LALM" | "ALST" | "MLST" => Err(CaError::ReadOnlyField(name.into())),
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        dbd_generated::EPID_FIELDS
    }

    fn declared_noaccess_fields(&self) -> &'static [&'static str] {
        dbd_generated::EPID_NOACCESS
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }

    /// C `epidRecord.c` UDF ownership — see `EpidRecord::value_undefined`.
    ///
    /// The framework's post-`process()` step runs
    /// `common.udf = value_is_undefined()` (gated on `clears_udf()`,
    /// left at its `true` default). Returning the epid-owned
    /// `value_undefined` — recomputed in `process()` from the two C
    /// clear-conditions — keeps `udf` TRUE for a supervisory / empty-
    /// STPL epid (so its UDF gate fires every cycle, as C does) and
    /// clears it only on a CONSTANT STPL or a successful closed-loop
    /// `dbGetLink(stpl)`.
    ///
    /// The default `value_is_undefined()` keys off `VAL` being NaN,
    /// which for an epid (`VAL` defaults to a finite `0.0`, never NaN)
    /// would wrongly clear `udf` after the first cycle — the bug this
    /// override fixes.
    fn value_is_undefined(&self) -> bool {
        self.value_undefined
    }

    fn set_device_did_compute(&mut self, did_compute: bool) {
        self.device_did_compute = did_compute;
    }

    /// C `epidRecord.c:195` reads `pepid->udf` at the top of
    /// `process()`. The framework owns `dbCommon.udf`; this hook
    /// captures it so `process()` can gate `do_pid` on it.
    fn set_process_context(&mut self, ctx: &ProcessContext) {
        self.udf = ctx.udf;
        self.dtyp.clear();
        self.dtyp.push_str(&ctx.dtyp);
    }

    /// C `devEpidSoftCallback.c:120-132` — the DB-type TRIG readback
    /// link write.
    ///
    /// `devEpidSoftCallback.c::do_pid`, within ONE process pass, does:
    ///   1. `if (ptriglink->type != CA_LINK)` →
    ///      `dbPutLink(ptriglink, DBR_DOUBLE, &pepid->tval, 1)`
    ///      (`devEpidSoftCallback.c:121-127`) — a synchronous write that
    ///      processes the triggered source chain;
    ///   2. `dbGetLink(&pepid->inp, DBR_DOUBLE, &pepid->cval, ...)`
    ///      (`devEpidSoftCallback.c:149`) — read CVAL from INP;
    ///   3. run the PID.
    ///
    /// So for a DB-type TRIG link the trigger write must land BEFORE
    /// this cycle's `INP -> CVAL` fetch. The framework resolves input
    /// links before `pre_process_actions`, so the TRIG write is emitted
    /// here, from `pre_input_link_actions`, which the framework runs
    /// strictly before the input-link fetch.
    ///
    /// Only the `devEpidSoftCallback` DSET drives the TRIG link —
    /// `devEpidSoft` (`devEpidSoft.c`) and `devEpidFast`
    /// (`devEpidFast.c`) contain no reference to `trig` at all. That
    /// DSET is selected by `stdSupport.dbd:14`
    /// `device(epid,CONSTANT,devEpidSoftCB,"Async Soft Channel")`, so
    /// the gate is the dbd DTYP string and nothing else.
    ///
    /// Both TRIG link types are fired from here, making this the single
    /// site that writes TRIG. C branches on the link type inside one
    /// `if (!pepid->pact)` block (`devEpidSoftCallback.c:116-147`): a
    /// DB link is written synchronously and falls through to the PID in
    /// the same pass; a CA link cannot be waited on, so C fires
    /// `dbCaPutLinkCallback`, sets `pact` and returns, and the PID runs
    /// on the callback pass. `ca_trig` is that `pact`, and the
    /// `AwaitingCallback` arm below is C's `!pepid->pact` guard: on the
    /// callback pass the trigger block is skipped entirely.
    fn pre_input_link_actions(&mut self) -> Vec<ProcessAction> {
        if !self.is_async_callback_dtyp() {
            return Vec::new();
        }
        // C `devEpidSoftCallback.c:116` `if (!pepid->pact)` — the
        // callback pass re-processes to run the PID, never to re-fire.
        if self.ca_trig == CaTrigPhase::AwaitingCallback {
            self.ca_trig = CaTrigPhase::Idle;
            return Vec::new();
        }
        let write = ProcessAction::WriteDbLink {
            link_field: "TRIG",
            value: EpicsValue::Double(self.tval),
        };
        match link_field_type(&self.trig) {
            LinkType::Db => vec![write],
            LinkType::Ca => {
                self.ca_trig = CaTrigPhase::AwaitingCallback;
                vec![
                    write,
                    ProcessAction::ReprocessAfter(std::time::Duration::from_millis(1)),
                ]
            }
            // C `ptriglink->type` is CONSTANT/empty: `dbPutLink` to a
            // constant link is a no-op, and the PID runs in this pass.
            LinkType::Constant | LinkType::Empty | LinkType::Other => Vec::new(),
        }
    }

    /// Framework report of which `multi_input_links` fetches produced a
    /// value this cycle — the analogue of C
    /// `RTN_SUCCESS(dbGetLink(&prec->stpl, ...))` (`epidRecord.c:191`).
    /// `STPL` is only ever in `multi_input_links` when
    /// `SMSL == closed_loop`; its presence here means the closed-loop
    /// setpoint fetch actually succeeded this cycle. A STPL that is
    /// empty, or a DB/CA link whose fetch failed, is absent — so
    /// `stpl_resolved` is reset to false and `udf` is not cleared.
    fn set_resolved_input_links(&mut self, resolved: &[&'static str]) {
        self.stpl_resolved = resolved.contains(&"STPL");
    }

    /// C `epidRecord.c:160-164` `init_record`: when `STPL` is a
    /// CONSTANT link holding a valid constant, `recGblInitConstantLink`
    /// seeds `VAL` from the constant and `udf` is cleared. The
    /// framework owns `dbCommon.udf`; this hook is its controlled
    /// access point. Runs once after `init_record`.
    ///
    /// For `SMSL == closed_loop` the framework also fetches `STPL` into
    /// `VAL` via `multi_input_links` every cycle; the constant seed
    /// here matters for the supervisory (`SMSL=0`) case and for the
    /// first cycle before any process.
    fn post_init_finalize_undef(&mut self, udf: &mut bool) -> CaResult<()> {
        let parsed = epics_base_rs::server::record::parse_link_v2(&self.stpl);
        if parsed.link_type() == LinkType::Constant {
            if let Some(EpicsValue::Double(v)) = parsed.constant_value() {
                self.val = v;
                *udf = false;
                self.value_undefined = false;
            }
        }
        Ok(())
    }

    fn put_field_internal(
        &mut self,
        name: &str,
        value: EpicsValue,
    ) -> epics_base_rs::error::CaResult<()> {
        // Bypass read-only checks for framework-internal writes (ReadDbLink).
        // This allows the framework to write to CVAL, OVAL, etc. from link resolution.
        match name {
            // The bumpless-transfer OUTL readback. Staged, not committed:
            // `do_pid` moves it into `I` (PID) or `OVAL` (MaxMin) at C's line,
            // after the MDT gate. See `OUTL_SEED_FIELD`.
            OUTL_SEED_FIELD => match value {
                EpicsValue::Double(v) => {
                    self.outl_seed = Some(v);
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "CVAL" => match value {
                EpicsValue::Double(v) => {
                    self.cval = v;
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
            "P" => match value {
                EpicsValue::Double(v) => {
                    self.p = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "D" => match value {
                EpicsValue::Double(v) => {
                    self.d = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ERR" => match value {
                EpicsValue::Double(v) => {
                    self.err = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            _ => self.put_field(name, value),
        }
    }

    /// C `epidRecord.c:158-164`:
    ///
    /// ```c
    /// if (pepid->stpl.type == CONSTANT) {
    ///     if (recGblInitConstantLink(&pepid->stpl, DBF_DOUBLE, &pepid->val))
    ///         pepid->udf = FALSE;
    /// }
    /// ```
    ///
    /// The setpoint of a constant-STPL epid is loaded ONCE, here — at process
    /// `dbGetLink(&prec->stpl, ...)` delivers nothing (it returns success, so
    /// the closed-loop UDF clear at `:191` still fires), which is why an
    /// operator `caput REC.VAL` on a constant-STPL epid holds.
    ///
    /// INP is NOT seeded: a constant INP means "nothing to control" in C —
    /// `devEpidSoft.c` raises SOFT/INVALID rather than reading a value.
    fn constant_init_links(&self) -> Vec<epics_base_rs::server::record::ConstantInitLink> {
        vec![epics_base_rs::server::record::ConstantInitLink::dol_to_val(
            "STPL", "VAL",
        )]
    }

    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        // INP -> CVAL is always resolved.
        // STPL -> VAL is only resolved when SMSL == closed_loop (1).
        // In supervisory mode (SMSL=0), the operator sets VAL directly
        // and STPL must not overwrite it.
        if self.smsl == 1 {
            // closed_loop: fetch setpoint from STPL into VAL
            static WITH_STPL: &[(&str, &str)] = &[("STPL", "VAL"), ("INP", "CVAL")];
            WITH_STPL
        } else {
            // supervisory: VAL is set by operator, don't fetch STPL
            static WITHOUT_STPL: &[(&str, &str)] = &[("INP", "CVAL")];
            WITHOUT_STPL
        }
    }

    fn multi_output_links(&self) -> &[(&'static str, &'static str)] {
        // C `epidRecord.c:195-202`: on a UDF-gated cycle `process()`
        // returns before `do_pid` writes the output — suppress the
        // OUTL->OVAL write so a stale OVAL is not pushed downstream.
        //
        // C `devEpidSoft.c:220` / `devEpidSoftCallback.c:256`: the OUTL
        // `dbPutLink` fires only when `fbon && outl.type != CONSTANT`,
        // and only when `do_pid` reached that line (not the sub-MDT or
        // CONSTANT-INP early returns); the Fast support never writes
        // OUTL. `do_pid` owns `outl_write` and encodes exactly that
        // condition, so the framework OUTL write is gated on it.
        if self.compute_skipped || !self.outl_write {
            return &[];
        }
        // OUTL -> OVAL (output link)
        static LINKS: &[(&str, &str)] = &[("OUTL", "OVAL")];
        LINKS
    }

    fn should_fire_forward_link(&self) -> bool {
        // C `epidRecord.c:201` `return(0)` on a UDF-gated cycle is
        // reached before `recGblFwdLink` — no forward link this cycle.
        !self.compute_skipped
    }
}

#[cfg(test)]
mod menu_choice_tests {
    use super::EpidRecord;
    use epics_base_rs::server::record::{FieldDeclaration, Record, RecordInstance};
    use epics_base_rs::types::EpicsValue;

    /// The choices a client sees are the DECLARATION's — `epidRecord.dbd`'s
    /// `menu()` on each field — and the index↔string mapping is wire-visible.
    /// This used to assert them through `Record::menu_field_choices`, a hand
    /// written table that declared the same menus a second time; `SMSL` needed
    /// a per-record mapping there only because the shared-menu registry keys
    /// `menuOmsl` by the field name `OMSL`. The declaration has no such
    /// problem: the `.dbd` says `field(SMSL,DBF_MENU) { menu(menuOmsl) }`, so
    /// the FieldDesc points straight at base's `MENU_OMSL`.
    #[test]
    fn epid_menu_choices_come_from_the_declaration() {
        let rec = EpidRecord::default();
        let menu = |name: &str| {
            rec.field_list()
                .iter()
                .find(|f| f.name == name)
                .unwrap_or_else(|| panic!("{name} is declared"))
                .menu
        };
        assert_eq!(menu("FMOD"), Some(&["PID", "Max/Min"][..]));
        let fbstate = &["Off", "On"][..];
        assert_eq!(menu("FBON"), Some(fbstate));
        assert_eq!(menu("FBOP"), Some(fbstate));
        assert_eq!(menu("SMSL"), Some(&["supervisory", "closed_loop"][..]));
        assert_eq!(menu("VAL"), None);
    }

    // End-to-end: SMSL is served as Short; the base snapshot path promotes
    // it to DBR_ENUM and attaches the menuOmsl labels.
    #[test]
    fn epid_smsl_snapshot_is_enum_with_labels() {
        let mut rec = EpidRecord::default();
        rec.put_field("SMSL", EpicsValue::Short(1)).unwrap(); // closed_loop
        let inst = RecordInstance::new("PID:SMSL".into(), rec);

        let snap = inst.snapshot_for_field("SMSL").unwrap();
        assert_eq!(snap.value, EpicsValue::Enum(1));
        assert_eq!(
            snap.enums.as_ref().unwrap().strings,
            vec!["supervisory", "closed_loop"]
        );
    }
}
