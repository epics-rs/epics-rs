use std::any::Any;
use std::time::Instant;

use super::dbd_generated;
use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::device_support::Dpvt;
use epics_base_rs::server::recgbl::{self, alarm_status};
use epics_base_rs::server::record::{
    AlarmSeverity, CommonFields, CycleCell, CycleHooks, CycleMarks, CyclePostMask, CycleRead,
    CycleValue, FieldDesc, FieldMetadataOverride, LinkType, ProcessAction, ProcessActions,
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
    pub stpl: PvString,
    /// Controlled value input link (INP) — resolved by framework
    pub inp: PvString,
    /// Output link (OUTL) — resolved by framework
    pub outl: PvString,
    /// Readback trigger link (TRIG)
    pub trig: PvString,
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
    /// This cycle's `do_pid` status, which `epidRecord.c:214` returns:
    /// the `1` of a sub-MDT pass (`devEpidSoft.c:125`). Taken by `process`.
    pub(crate) pid_status: epics_base_rs::error::DbResult,
    /// The `MONITOR_POSTS` entries whose field moved off its own old copy this
    /// cycle, taken by `take_cycle_posted_fields`.
    monitor_marks: u128,
    /// The `dbCommon.udf` byte, pushed by the framework via
    /// [`Record::set_process_context`] immediately before `process()`.
    /// C `epidRecord.c:195` skips `do_pid` only while `udf == TRUE`. The
    /// matching `UDF_ALARM` (C `epidRecord.c:199`,
    /// `recGblSetSevr(pepid,UDF_ALARM,pepid->udfs)`) is raised by the
    /// framework's centralised `rec_gbl_check_udf` after `process()`.
    udf: u8,
    /// UDFS, pushed with `udf` for the gate's raise (`epidRecord.c:199`).
    udfs: AlarmSeverity,
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
    /// This cycle's UDF gate (`epidRecord.c:189-201`), judged once by
    /// [`EpidRecord::judge_gate`] after the STPL read and taken by
    /// `process()`. `None` on a cycle with no input stage (`process_local`),
    /// where `process()` asks the same owner.
    gate: Option<bool>,
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
    /// Whether the record runs `devEpidSoftCallback` (`devEpidSoftCallback.c`,
    /// which drives the TRIG readback link) rather than `devEpidSoft`
    /// (`devEpidSoft.c`, no TRIG). Taken from the dset bound at init through
    /// [`Record::set_process_context`]; a runtime DTYP put leaves it, as C
    /// leaves `prec->dset`.
    async_callback: bool,
    /// This cycle reached a C `udf = FALSE` site; the `check_alarms` hook,
    /// sole owner of `common.udf` ([`Record::clears_udf`] is false), applies
    /// it. C clears `udf` only at init for a CONSTANT `STPL`
    /// ([`Record::post_init_finalize_undef`]) and on a closed-loop
    /// `dbGetLink(stpl)` success (`epidRecord.c:191-193`), so any other byte,
    /// 2 included, stands.
    udf_cleared: bool,
    /// This cycle fired a CA-type TRIG: C `devEpidSoftCallback.c:143-145`
    /// sets `pepid->pact` and returns, and `epidRecord.c:207` returns
    /// before `recGblGetTimeStamp` / `checkAlarms` / `monitor` /
    /// `recGblFwdLink`, so the trigger pass runs none of the tail; the
    /// callback pass runs `do_pid` and the tail once. Set by
    /// [`EpidRecord::gate_after_pre_input_reads`] and taken by `process()`,
    /// which then returns `ProcessOutcome::async_pending()`. Whether the
    /// trigger is in flight is `Dpvt::Triggered`, which a device-link put
    /// replaces as C's clears `pact`.
    trigger_pass: bool,
}

impl Default for EpidRecord {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            val: 0.0,
            smsl: 0,
            stpl: PvString::new(),
            inp: PvString::new(),
            outl: PvString::new(),
            trig: PvString::new(),
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
            pid_status: Ok(()),
            monitor_marks: 0,
            async_callback: false,
            udf: 1,
            udfs: AlarmSeverity::Invalid,
            outl_write: false,
            gate: None,
            outl_seed: None,
            udf_cleared: false,
            trigger_pass: false,
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

    /// True when the record is bound to `devEpidSoftCB`
    /// (`stdSupport.dbd:14`, DTYP `"Async Soft Channel"`) — the only epid
    /// device support that touches the TRIG readback link.
    /// The UDF gate, `epidRecord.c:189-201`, and its one owner: a closed-loop
    /// `dbGetLink(stpl)` success clears UDF (applied by `check_alarms`), and
    /// the cycle runs `do_pid` unless UDF is exactly TRUE.
    fn judge_gate(&mut self, stpl_read: bool) -> bool {
        self.udf_cleared = self.smsl == 1 && stpl_read;
        self.udf_cleared || self.udf != 1
    }

    pub fn is_async_callback_dtyp(&self) -> bool {
        self.async_callback
    }

    /// Owner setter for `EpidRecord::outl_write`. Only `do_pid` calls
    /// this — it clears the flag at entry and re-enables it per `fbon`
    /// on the success path, mirroring C's OUTL `dbPutLink` gate
    /// (`devEpidSoft.c:220`). Keeping it private to a setter preserves
    /// the single-owner invariant.
    pub fn set_outl_write(&mut self, write: bool) {
        self.outl_write = write;
    }

    /// C `monitor()`'s secondaries (`epidRecord.c:376-408`): each posts when it
    /// differs from its own old copy, never from the value last published, and
    /// the copy then takes it.
    pub fn update_monitors(&mut self) {
        let pairs = [
            (self.oval, &mut self.ovlp),
            (self.p, &mut self.pp),
            (self.i, &mut self.ip),
            (self.d, &mut self.dp),
            (self.dt, &mut self.dtp),
            (self.err, &mut self.errp),
            (self.cval, &mut self.cvlp),
        ];
        for (bit, (field, old)) in pairs.into_iter().enumerate() {
            if *old != field {
                self.monitor_marks |= 1 << bit;
                *old = field;
            }
        }

        // VAL deadband baselines (MLST/ALST) are NOT advanced here. C
        // `epidRecord.c:346-374` `monitor()` computes `delta = mlst - val`,
        // posts VAL when `delta > mdel`, and only THEN sets `mlst = val`
        // — the post and the advance are one owner. In Rust that owner is
        // the framework's `check_deadband_ext`: it reads MLST, fires the
        // VAL monitor, then advances `mlst`/`alst` through
        // `Record::put_cycle_cell`. Advancing
        // them here (before that runs) made the framework see a zero delta
        // and silently suppress every VAL post. `update_monitors` owns only
        // the epid-specific previous-value fields above (`pp`/`ip`/`dp`/
        // `cvlp`/...), not the MLST/ALST deadband state.
    }
}

/// C `monitor()`'s secondaries in its post order (`epidRecord.c:376-408`), the
/// bit order of `EpidRecord::update_monitors`. `:376` REASSIGNS `monitor_mask =
/// DBE_LOG|DBE_VALUE`, so unlike VAL they carry no alarm bits. `CT` is
/// `DBF_NOACCESS` (`epidRecord.dbd:226`) and has no CA-visible field.
const MONITOR_POSTS_NAMED: [(&str, CyclePostMask); 7] = [
    ("OVAL", CyclePostMask::ValueLog),
    ("P", CyclePostMask::ValueLog),
    ("I", CyclePostMask::ValueLog),
    ("D", CyclePostMask::ValueLog),
    ("DT", CyclePostMask::ValueLog),
    ("ERR", CyclePostMask::ValueLog),
    ("CVAL", CyclePostMask::ValueLog),
];

const PROCESS_POSTED: [&str; 8] = ["VAL", "OVAL", "P", "I", "D", "DT", "ERR", "CVAL"];
static MONITOR_POSTS: [(epics_base_rs::server::record::FieldIx, CyclePostMask); 7] =
    epics_base_rs::server::record::declared_post_table(
        dbd_generated::EPID_FIELDS,
        MONITOR_POSTS_NAMED,
    );
static PROCESS_POSTED_SET: [epics_base_rs::server::record::FieldIx; 8] =
    epics_base_rs::server::record::declared_set(dbd_generated::EPID_FIELDS, PROCESS_POSTED);

impl Record for EpidRecord {
    fn record_type(&self) -> &'static str {
        "epid"
    }

    /// MLST and ALST are `SPC_NOMOD` to a client, so `put_field` refuses them;
    /// the deadband's advance comes through here instead. Without it MLST
    /// stayed 0 and a held VAL posted on every cycle.
    fn cycle_cell(&self, cell: CycleCell) -> CycleRead {
        match cell {
            CycleCell::Val => CycleRead::Value(CycleValue::Double(self.val)),
            CycleCell::Mdel => CycleRead::Value(CycleValue::Double(self.mdel)),
            CycleCell::Adel => CycleRead::Value(CycleValue::Double(self.adel)),
            CycleCell::Mlst => CycleRead::Value(CycleValue::Double(self.mlst)),
            CycleCell::Alst => CycleRead::Value(CycleValue::Double(self.alst)),
            _ => CycleRead::ByName,
        }
    }

    fn put_cycle_cell(&mut self, cell: CycleCell, value: CycleValue) -> CaResult<()> {
        match (cell, value) {
            (CycleCell::Mlst, CycleValue::Double(v)) => self.mlst = v,
            (CycleCell::Alst, CycleValue::Double(v)) => self.alst = v,
            (cell, value) => return self.put_field(cell.name(), value.into()),
        }
        Ok(())
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
    fn pre_process_actions(&mut self) -> ProcessActions {
        // The staged readback is per-cycle: whatever a previous cycle left
        // behind must not be mistaken for this cycle's OUTL value.
        self.outl_seed = None;
        let edge = self.fbon != 0 && self.fbop == 0;
        if edge
            && !matches!(
                link_field_type(&self.outl),
                LinkType::Constant | LinkType::Empty
            )
        {
            return ProcessActions::from(vec![ProcessAction::ReadDbLink {
                link_field: "OUTL",
                target_field: OUTL_SEED_FIELD,
            }]);
        }
        ProcessActions::new()
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
        // The gate itself is `judge_gate`'s, judged after the STPL read.

        // CA-TRIG trigger pass — C `devEpidSoftCallback.c:143-145` +
        // `epidRecord.c:205-210`. `pre_input_link_actions` ran first
        // this cycle, saw a CA-type TRIG link and fired the asynchronous
        // readback trigger (`WriteDbLink{TRIG}` + `ReprocessAfter`) — the
        // analogue of C `do_pid` setting `pepid->pact = TRUE` and
        // `return(0)`.
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
        if std::mem::take(&mut self.trigger_pass) {
            self.device_did_compute = false;
            return Ok(ProcessOutcome::async_pending());
        }

        let open = match self.gate.take() {
            Some(open) => open,
            None => self.judge_gate(false),
        };
        if !open && !self.device_did_compute {
            // C `epidRecord.c:195-202`: while `udf==TRUE`, skip
            // `do_pid` entirely and `return 0` — *before*
            // `recGblGetTimeStamp`, `checkAlarms`, `monitor` and
            // `recGblFwdLink`, so nothing is stamped, committed,
            // posted, written to OUTL or forward-linked. The UDF raise
            // (:199) stays pending for the next `recGblResetAlarms`.
            return Ok(ProcessOutcome::complete_no_emit_raising(
                alarm_status::UDF_ALARM,
                self.udfs,
            ));
        }

        if !self.device_did_compute {
            crate::device_support::epid_soft::EpidSoftDeviceSupport::do_pid(self);
        }
        self.device_did_compute = false; // Reset for next cycle
        let status = std::mem::replace(&mut self.pid_status, Ok(()));

        // Alarm evaluation is NOT done here. The framework invokes the
        // `Record::check_alarms` trait hook (below) after `process()`,
        // which is where the computed severity is applied to SEVR/STAT
        // via `recGblSetSevr`. Calling the inherent `check_alarms` here
        // would advance `lalm` an extra time and double-step the
        // hysteresis state, so it is deliberately omitted.
        self.update_monitors();

        // Device support actions are now merged by the framework
        let actions = Vec::new();
        Ok(ProcessOutcome {
            status,
            ..ProcessOutcome::complete_with(actions)
        })
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
        if std::mem::take(&mut self.udf_cleared) {
            common.udf = 0;
        }
        // C `devEpidSoft.c:110-112` / `devEpidSoftCallback.c:112-114`:
        // a CONSTANT `INP` link means "nothing to control" — raise
        // SOFT_ALARM/INVALID_ALARM. `do_pid` set `inp_constant` and
        // skipped the compute; apply the severity here (the framework
        // calls this hook after `process()`).
        if self.inp_constant {
            recgbl::rec_gbl_set_sevr(common, alarm_status::SOFT_ALARM, AlarmSeverity::Invalid);
        }
        // C `epidRecord.c:309` `if (pepid->udf == TRUE) return`: the UDF_ALARM
        // itself is raised by `rec_gbl_check_udf` (`udf_alarm_on_exact_one`).
        if common.udf == 1 {
            return;
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

    fn take_cycle_posted_fields(&mut self) -> CycleMarks {
        CycleMarks::of(&MONITOR_POSTS, std::mem::take(&mut self.monitor_marks))
    }

    /// `monitor()` posts VAL and its secondaries alone.
    fn process_posted_fields(&self) -> Option<epics_base_rs::server::record::DeclaredFields> {
        Some(&PROCESS_POSTED_SET)
    }

    /// The secondaries post from their own old copies, so a put that moved one
    /// must not make the next cycle's walk post it again.
    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &PROCESS_POSTED[1..]
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "SMSL" => Some(EpicsValue::Short(self.smsl)),
            "STPL" => Some(EpicsValue::String(self.stpl.clone())),
            "INP" => Some(EpicsValue::String(self.inp.clone())),
            "OUTL" => Some(EpicsValue::String(self.outl.clone())),
            "TRIG" => Some(EpicsValue::String(self.trig.clone())),
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
                    self.stpl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "INP" => match value {
                EpicsValue::String(v) => {
                    self.inp = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OUTL" => match value {
                EpicsValue::String(v) => {
                    self.outl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "TRIG" => match value {
                EpicsValue::String(v) => {
                    self.trig = v;
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
            "CVAL" => match value {
                EpicsValue::Double(v) => {
                    self.cval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "CVLP" => match value {
                EpicsValue::Double(v) => {
                    self.cvlp = v;
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
            "OVLP" => match value {
                EpicsValue::Double(v) => {
                    self.ovlp = v;
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
            "PP" => match value {
                EpicsValue::Double(v) => {
                    self.pp = v;
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
            "DP" => match value {
                EpicsValue::Double(v) => {
                    self.dp = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DTP" => match value {
                EpicsValue::Double(v) => {
                    self.dtp = v;
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
            "ERRP" => match value {
                EpicsValue::Double(v) => {
                    self.errp = v;
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
            "FBOP" => match value {
                EpicsValue::Short(v) => {
                    self.fbop = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        dbd_generated::EPID_FIELDS
    }

    fn declared_dbaddr_fields(&self) -> Option<&'static [&'static str]> {
        Some(dbd_generated::EPID_DBADDR)
    }

    fn declared_noaccess_fields(&self) -> &'static [&'static str] {
        dbd_generated::EPID_NOACCESS
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }

    /// `common.udf` is the `check_alarms` hook's alone: C never re-derives
    /// it from VAL, so a byte of 2 put through a link stands.
    fn clears_udf(&self) -> bool {
        false
    }

    fn soft_read_clears_udf(&self) -> bool {
        false
    }

    fn rederives_udf_on_computed_read(&self) -> bool {
        false
    }

    /// C tests `udf == TRUE` at both `epidRecord.c:195` and `:309`.
    fn udf_alarm_on_exact_one(&self) -> bool {
        true
    }

    fn set_device_did_compute(&mut self, did_compute: bool) {
        self.device_did_compute = did_compute;
    }

    /// C `epidRecord.c:195` reads `pepid->udf` at the top of
    /// `process()`. The framework owns `dbCommon.udf`; this hook
    /// captures it so `process()` can gate `do_pid` on it.
    fn set_process_context(&mut self, ctx: &ProcessContext) {
        self.udf = ctx.udf;
        self.udfs = ctx.udfs;
        self.async_callback =
            ctx.soft == Some(epics_base_rs::server::device_support::SoftDtyp::Async);
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
    /// Both TRIG link types are fired from `gate_after_pre_input_reads`,
    /// behind the UDF gate, the single site that writes TRIG. C branches on the link type inside one
    /// `if (!pepid->pact)` block (`devEpidSoftCallback.c:116-147`): a
    /// DB link is written synchronously and falls through to the PID in
    /// the same pass; a CA link cannot be waited on, so C fires
    /// `dbCaPutLinkCallback`, sets `pact` and returns, and the PID runs
    /// on the callback pass. That `pact` is `Dpvt::Triggered` here, and
    /// its arm below is C's `!pepid->pact` guard: on the callback pass
    /// the trigger block is skipped entirely. A device-link put replaces
    /// the `dpvt` (`dbAccess.c:1192-1194`), so the callback a put lands
    /// before fires the trigger again, as C's does.
    fn pre_input_link_actions(
        &mut self,
        _dol_is_constant: bool,
        dpvt: &mut Dpvt,
    ) -> ProcessActions {
        self.trigger_pass = false;
        self.gate = None;
        // C `epidRecord.c:187` `if (!pact)`: the CA-TRIG callback pass
        // (`devEpidSoftCallback.c:116`) takes no setpoint, no gate and no
        // second trigger.
        if self.is_async_callback_dtyp() && matches!(dpvt, Dpvt::Triggered) {
            *dpvt = Dpvt::Null;
            self.gate = Some(true);
            return ProcessActions::new();
        }
        // C `epidRecord.c:190`, ahead of the gate it decides.
        if self.smsl == 1 {
            return ProcessActions::from(vec![ProcessAction::ReadDbLink {
                link_field: "STPL",
                target_field: "VAL",
            }]);
        }
        ProcessActions::new()
    }

    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::ALL.union(CycleHooks::PRE_INPUT_GATE)
    }

    fn gate_after_pre_input_reads(
        &mut self,
        resolved: &[&'static str],
        dpvt: &mut Dpvt,
    ) -> Option<ProcessActions> {
        if self.gate.is_some() {
            return Some(ProcessActions::new());
        }
        let open = self.judge_gate(resolved.contains(&"STPL"));
        self.gate = Some(open);
        if !open {
            return None;
        }
        if !self.is_async_callback_dtyp() {
            return Some(ProcessActions::new());
        }
        let write = ProcessAction::WriteDbLink {
            link_field: "TRIG",
            value: EpicsValue::Double(self.tval),
        };
        Some(match link_field_type(&self.trig) {
            // C `ptriglink->type != CA_LINK` (`devEpidSoftCallback.c:117`),
            // a PV Access link's `JSON_LINK` too: `dbPutLink`, and the PID
            // runs in this pass.
            LinkType::Db | LinkType::Json => ProcessActions::from(vec![write]),
            LinkType::Ca => {
                *dpvt = Dpvt::Triggered;
                self.trigger_pass = true;
                ProcessActions::from(vec![
                    write,
                    ProcessAction::ReprocessAfter(std::time::Duration::from_millis(1)),
                ])
            }
            // C `ptriglink->type` is CONSTANT/empty: `dbPutLink` to a
            // constant link is a no-op, and the PID runs in this pass.
            LinkType::Constant | LinkType::Empty | LinkType::Other => ProcessActions::new(),
        })
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
    fn post_init_finalize_undef(&mut self, udf: &mut u8) -> CaResult<()> {
        let parsed = epics_base_rs::server::record::parse_link_v2(&self.stpl);
        if parsed.link_type() == LinkType::Constant {
            if let Some(EpicsValue::Double(v)) = parsed.constant_value() {
                self.val = v;
                *udf = 0;
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

    /// C reads INP inside `do_pid` (`devEpidSoft.c:113`,
    /// `devEpidSoftCallback.c:149`), behind the `udf == TRUE` return
    /// (`epidRecord.c:195`): a shut gate reads nothing.
    fn narrows_input_links(&self) -> bool {
        true
    }

    fn select_input_links(
        &self,
        _selector: Option<u16>,
    ) -> Option<&'static [(&'static str, &'static str)]> {
        (self.gate == Some(false)).then_some(&[])
    }

    fn multi_input_links(&self) -> &'static [(&'static str, &'static str)] {
        // STPL is read ahead of the gate, in `pre_input_link_actions`.
        &[("INP", "CVAL")]
    }

    fn declares_multi_output_links(&self) -> bool {
        true
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
        if !self.outl_write {
            return &[];
        }
        // OUTL -> OVAL (output link)
        static LINKS: &[(&str, &str)] = &[("OUTL", "OVAL")];
        LINKS
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
        let inst = RecordInstance::new("PID:SMSL", rec);

        let snap = inst.snapshot_for_field("SMSL").unwrap();
        assert_eq!(snap.value, EpicsValue::Enum(1));
        assert_eq!(
            snap.enums.as_ref().unwrap().strings,
            vec!["supervisory", "closed_loop"]
        );
    }
}
