use std::time::Instant;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::{
    FieldDesc, LinkType, ProcessAction, ProcessOutcome, Record, link_field_type,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// Record-specific `DBF_MENU` choice tables, in `.dbd` value order (the
/// index↔string mapping is wire-visible to clients). Source: the C
/// `throttleRecord.dbd` menu definitions (std module). `OV`/`SIV` share
/// `menu(throttleOV)`.
const THROTTLE_WAIT_CHOICES: &[&str] = &["False", "True"];
const THROTTLE_DRVLC_CHOICES: &[&str] = &["Off", "On"];
const THROTTLE_DRVLS_CHOICES: &[&str] = &["Normal", "Low Limit", "High Limit"];
const THROTTLE_STS_CHOICES: &[&str] = &["Unknown", "Error", "Success"];
const THROTTLE_OV_CHOICES: &[&str] = &["Ext PV NC", "Ext PV OK", "Local PV", "Constant"];
const THROTTLE_SYNC_CHOICES: &[&str] = &["Idle", "Process"];

/// Throttle record — rate-limits value changes to prevent device damage.
///
/// Ported from EPICS std module `throttleRecord.c`.
///
/// When VAL is written, the record checks drive limits, optionally clips
/// the value, sets WAIT=True, then writes SENT to the OUT link only after
/// the minimum delay (DLY) has elapsed since the last output. If a new
/// value arrives during the delay, it queues the latest value and sends
/// it when the delay expires.
pub struct ThrottleRecord {
    /// Set value (VAL)
    pub val: f64,
    /// Previous set value (OVAL), read-only
    pub oval: f64,
    /// Last sent value (SENT), read-only
    pub sent: f64,
    /// Previous sent value (OSENT), read-only
    pub osent: f64,
    /// Busy flag (WAIT): 0=False, 1=True, read-only
    pub wait: i16,
    /// High operating range (HOPR)
    pub hopr: f64,
    /// Low operating range (LOPR)
    pub lopr: f64,
    /// High drive limit (DRVLH)
    pub drvlh: f64,
    /// Low drive limit (DRVLL)
    pub drvll: f64,
    /// Limit status: 0=Normal, 1=Low, 2=High (DRVLS), read-only
    pub drvls: i16,
    /// Limit clipping: 0=Off, 1=On (DRVLC)
    pub drvlc: i16,
    /// Code version string (VER), read-only
    pub ver: String,
    /// Record status: 0=Unknown, 1=Error, 2=Success (STS), read-only
    pub sts: i16,
    /// Display precision (PREC)
    pub prec: i16,
    /// Delay display precision (DPREC)
    pub dprec: i16,
    /// Delay between outputs in seconds (DLY)
    pub dly: f64,
    /// Output link (OUT)
    pub out: String,
    /// Output link valid: 0=ExtNC, 1=Ext, 2=Local, 3=Constant (OV), read-only
    pub ov: i16,
    /// Sync input link (SINP)
    pub sinp: String,
    /// Sync input link valid (SIV), read-only
    pub siv: i16,
    /// Sync trigger: 0=Idle, 1=Process (SYNC)
    pub sync: i16,

    // --- Private runtime state ---
    /// Whether limits are active (drvlh > drvll)
    limit_flag: bool,
    /// Whether a delay is currently in progress
    delay_active: bool,
    /// When the last output was sent (for delay enforcement)
    last_send_time: Option<Instant>,
    /// Value queued during delay period (sent when delay expires)
    pending_value: Option<f64>,
    /// Whether the most recent `process()` cycle actually issued an OUT
    /// write. C `throttleRecord.c:308` has `recGblFwdLink` commented out
    /// in `process()`; the forward link fires ONLY inside `valuePut`
    /// (`throttleRecord.c:580`), i.e. only on a cycle where the OUT link
    /// was written. `should_fire_forward_link` returns this flag so a
    /// queuing-during-delay cycle or a rejected out-of-range cycle does
    /// NOT fire FLNK.
    out_written: bool,
}

impl Default for ThrottleRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            oval: 0.0,
            sent: 0.0,
            osent: 0.0,
            wait: 0,
            hopr: 0.0,
            lopr: 0.0,
            drvlh: 0.0,
            drvll: 0.0,
            drvls: 0, // Normal
            drvlc: 0, // Off
            // C `throttleRecord.c:51` `#define VERSION "0-2-1"`,
            // copied into VER by `init_record` pass 0 (line 149).
            ver: "0-2-1".to_string(),
            sts: 0, // Unknown
            prec: 0,
            dprec: 0,
            dly: 0.0,
            out: String::new(),
            ov: 3, // Constant
            sinp: String::new(),
            siv: 3,  // Constant
            sync: 0, // Idle
            limit_flag: false,
            delay_active: false,
            last_send_time: None,
            pending_value: None,
            out_written: false,
        }
    }
}

/// Upper bound (exclusive) on the `DLY` field, in seconds.
///
/// `process()` converts `self.dly` into a `std::time::Duration` via
/// `Duration::from_secs_f64`, which panics not only on a non-finite
/// argument but on any finite value too large for a `Duration` to
/// represent (≈ `u64::MAX` seconds ≈ 1.8e19, message "value is either
/// too big or NaN"). A CA put of e.g. `DLY = 1e300` is a perfectly
/// finite f64 and would otherwise slip past an `is_finite()` guard and
/// panic the record task.
///
/// A throttle delay of 24 hours is already far past any realistic
/// device-protection interval, so this finite cap is the operational
/// ceiling for `DLY`. It is also orders of magnitude below the
/// `Duration` overflow point, so any `self.dly` accepted by the writer
/// guard is guaranteed safe for `Duration::from_secs_f64`.
const MAX_DLY: f64 = 86_400.0;

/// Validate a candidate `DLY` value (seconds).
///
/// Returns `Ok(())` only for a value that can never make
/// `Duration::from_secs_f64(self.dly)` panic in `process()`: it must
/// be finite and at most [`MAX_DLY`]. A negative value is accepted
/// here — C `special()` clamps it to 0 and `process()` treats any
/// `dly <= 0.0` as "no delay" without constructing a `Duration` — so
/// negativity is not a panic hazard. This is the single guard every
/// writer of `self.dly` must pass through to hold the invariant
/// "`self.dly` can never make `Duration::from_secs_f64` panic".
fn validate_dly(v: f64) -> CaResult<()> {
    if !v.is_finite() {
        return Err(CaError::InvalidValue(format!(
            "throttle DLY must be finite, got {v}"
        )));
    }
    if v > MAX_DLY {
        return Err(CaError::InvalidValue(format!(
            "throttle DLY must not exceed {MAX_DLY} seconds, got {v}"
        )));
    }
    Ok(())
}

impl ThrottleRecord {
    /// Check drive limits and optionally clip the value.
    ///
    /// Mirrors the limit block of C `throttleRecord.c:242-283`. When
    /// `limit_flag` is set the value is tested against the low limit
    /// first, then the high limit (same order as C lines 246/260).
    /// `DRVLS` is updated to the resulting limit status; when limits
    /// are inactive it is forced to Normal (C line 275 sets
    /// `throttleDRVLS_NORM`).
    ///
    /// Returns `Ok(value)` when the value is acceptable (clipped to the
    /// limit when `DRVLC` is On), or `Err(())` when it is out of range
    /// and clipping is Off — C's `proc_flag = 0` rejection path. C does
    /// **not** touch `STS` on a rejection (lines 254-257, 268-271); the
    /// caller must not set it either.
    fn check_limits(&mut self, val: f64) -> Result<f64, ()> {
        if !self.limit_flag {
            self.drvls = 0; // throttleDRVLS_NORM
            return Ok(val);
        }

        if val < self.drvll {
            self.drvls = 1; // throttleDRVLS_LOW
            if self.drvlc == 1 {
                return Ok(self.drvll);
            }
            return Err(());
        }

        if val > self.drvlh {
            self.drvls = 2; // throttleDRVLS_HIGH
            if self.drvlc == 1 {
                return Ok(self.drvlh);
            }
            return Err(());
        }

        self.drvls = 0; // throttleDRVLS_NORM
        Ok(val)
    }

    /// Send the value to the output — C `throttleRecord.c::valuePut`
    /// (lines 540-594).
    ///
    /// C `valuePut` line 557 branches on the OUT link type:
    ///   - `if (plink->type != CONSTANT)` — `dbPutLink` is issued and
    ///     STS is set from its result (`throttleSTS_SUC` on success,
    ///     `throttleSTS_ERR` on failure), SENT/OSENT advance, the
    ///     forward link fires (line 580).
    ///   - `else` (CONSTANT/empty OUT) — no write happens, STS is forced
    ///     to `throttleSTS_ERR`, SENT/OSENT do NOT advance, no FLNK.
    ///
    /// Returns `true` when the caller must emit the `WriteDbLink{OUT}`
    /// action (a real, non-CONSTANT link). The port cannot observe the
    /// `dbPutLink` result inline, so a real link is treated optimistically
    /// as STS=Success — the emitted write either lands or the framework
    /// raises its own link alarm.
    fn send_value(&mut self, value: f64) -> bool {
        if link_field_type(&self.out) == LinkType::Constant
            || link_field_type(&self.out) == LinkType::Empty
        {
            // CONSTANT / empty OUT — C `valuePut` else branch: STS=Error,
            // SENT/OSENT unchanged, no write, no FLNK.
            self.sts = 1; // throttleSTS_ERR
            self.out_written = false;
            return false;
        }
        self.osent = self.sent;
        self.sent = value;
        self.last_send_time = Some(Instant::now());
        self.sts = 2; // throttleSTS_SUC
        self.out_written = true;
        true
    }

    /// Check if the delay period has elapsed since last send.
    fn delay_elapsed(&self) -> bool {
        if self.dly <= 0.0 {
            return true;
        }
        match self.last_send_time {
            Some(t) => t.elapsed().as_secs_f64() >= self.dly,
            None => true, // Never sent before
        }
    }
}

static FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "OVAL",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "SENT",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "OSENT",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "WAIT",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DRVLH",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DRVLL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DRVLS",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "DRVLC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "VER",
        dbf_type: DbFieldType::String,
        read_only: true,
    },
    FieldDesc {
        name: "STS",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "PREC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DPREC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DLY",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "OUT",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "SINP",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "SIV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "SYNC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

impl Record for ThrottleRecord {
    fn record_type(&self) -> &'static str {
        "throttle"
    }

    fn pre_process_actions(&mut self) -> Vec<ProcessAction> {
        // When SYNC=1, read SINP into VAL BEFORE process() runs.
        // This matches C EPICS where dbGetLink is synchronous/immediate.
        if self.sync == 1 {
            self.sync = 0;
            return vec![ProcessAction::ReadDbLink {
                link_field: "SINP",
                target_field: "VAL",
            }];
        }
        Vec::new()
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `throttleRecord.c:231-312`. The control flow here mirrors C's
        // `process()`:
        //
        //   1. The drive-limit block (C lines 242-283) runs on EVERY
        //      process() call, regardless of whether a delay is pending.
        //      It updates DRVLS and, on a clip-off out-of-range value,
        //      sets `proc_flag = 0` (reject: restore `val = oval`, skip
        //      the send).
        //   2. If `proc_flag` (C lines 285-296): the value is "entered".
        //      C `enterValue()` sets `wait_flag = 1`; if no delay is in
        //      progress (`!delay_flag`) it calls `valuePut()` to write
        //      OUT immediately and arm the delay timer. If a delay IS in
        //      progress the value just waits — the running delay timer
        //      will pick up the latest `prec->val` when it fires.
        //
        // The Rust port has no `callbackRequestDelayed` handle, so the
        // delay timer is modelled by `ReprocessAfter`: the current cycle
        // writes OUT, then the framework re-invokes `process()` after
        // DLY. `delay_active` is C's `delay_flag`; `pending_value` plus
        // re-entry through this same limit block reproduces C taking the
        // latest limit-checked `prec->val` at timer-fire time.
        let mut actions = Vec::new();

        // C `throttleRecord.c:308` keeps `recGblFwdLink` commented out in
        // `process()`; the forward link fires ONLY from `valuePut`'s
        // non-CONSTANT branch (line 580). Reset the per-cycle FLNK flag
        // here so a queuing-during-delay cycle, a rejected out-of-range
        // cycle, or a drain with nothing queued does NOT fire FLNK —
        // only a real OUT write (via `send_value`) sets it true.
        self.out_written = false;

        // --- Drain path: the post-delay timer callback (C `valuePut()`
        //     reached via `delayFuncCallback`, lines 530-538/540-594) ---
        //
        // C runs the drain in `valuePut()`, a code path SEPARATE from
        // `process()`: it does NOT re-run the drive-limit block and does
        // NOT touch the OVAL end-of-process update. The port models the
        // timer with `ReprocessAfter`, so the drain arrives as a
        // re-entrant `process()` call — identified here by an armed
        // delay whose window has elapsed. It must therefore short-circuit
        // BEFORE the limit block so a previously limit-checked queued
        // value is sent as-is and DRVLS (set by the queuing process()) is
        // left intact.
        if self.delay_active && self.delay_elapsed() {
            self.delay_active = false;
            self.wait = 0;
            match self.pending_value.take() {
                // C `valuePut`: `wait_flag` set -> a value arrived during
                // the delay; send the (already limit-checked) queued
                // value, set SENT/OSENT/STS, and re-arm the timer.
                Some(pv) => {
                    // C `valuePut`: a CONSTANT/empty OUT yields STS=Error
                    // and no write; a real link yields the WriteDbLink.
                    if self.send_value(pv) {
                        actions.push(ProcessAction::WriteDbLink {
                            link_field: "OUT",
                            value: EpicsValue::Double(self.sent),
                        });
                    }
                    if self.dly > 0.0 {
                        self.delay_active = true;
                        self.wait = 1;
                        let delay = std::time::Duration::from_secs_f64(self.dly);
                        actions.push(ProcessAction::ReprocessAfter(delay));
                    }
                    return Ok(ProcessOutcome::complete_with(actions));
                }
                // C `valuePut`: `wait_flag` clear -> nothing queued; the
                // callback merely clears `delay_flag` (line 597).
                None => {
                    return Ok(ProcessOutcome::complete_with(actions));
                }
            }
        }

        // --- Step 1: drive-limit block (C lines 242-283), runs on every
        //     fresh process() call ---
        //
        // C restores `prec->val = prec->oval` and sets `proc_flag = 0` on
        // a rejected (out-of-range, clipping Off) value; it does NOT set
        // STS and does NOT touch WAIT. STS is only ever written after a
        // real link operation (valuePut / valueSync).
        let proc_flag = match self.check_limits(self.val) {
            Ok(clamped) => {
                self.val = clamped;
                true
            }
            Err(()) => {
                self.val = self.oval;
                false
            }
        };

        if !proc_flag {
            // Rejected: skip enterValue entirely (C `proc_flag == 0`).
            // A delay already in progress is left running — its
            // ReprocessAfter still fires and drains whatever value was
            // queued. C's end-of-process OVAL block is a no-op here
            // because `val` was just restored to `oval`.
            return Ok(ProcessOutcome::complete_with(actions));
        }

        // OVAL end-of-process update (C lines 299-303): on a fresh,
        // accepted process() OVAL tracks the just-checked VAL.
        self.oval = self.val;

        // --- Step 2: enterValue() (C lines 518-528) ---
        //
        // A delay timer is in progress. C `enterValue()` sets
        // `wait_flag = 1` and returns; the running `delayFuncCb` will
        // call `valuePut()` and send whatever `prec->val` is when it
        // fires. The port stashes the latest limit-checked value (last
        // value wins, as in C) so the drain re-process sends it. WAIT
        // stays True; the in-flight ReprocessAfter is left to fire.
        if self.delay_active {
            self.pending_value = Some(self.val);
            self.wait = 1;
            let remaining = self.dly
                - self
                    .last_send_time
                    .map(|t| t.elapsed().as_secs_f64())
                    .unwrap_or(0.0);
            let delay = std::time::Duration::from_secs_f64(remaining.max(0.001));
            actions.push(ProcessAction::ReprocessAfter(delay));
            return Ok(ProcessOutcome::complete_with(actions));
        }

        // No delay in progress: send immediately (C `enterValue` calls
        // `valuePut` directly when `!delay_flag`). C `valuePut` writes the
        // OUT link and sets SENT/OSENT and STS=Success only for a
        // non-CONSTANT OUT; a CONSTANT/empty OUT yields STS=Error and no
        // write.
        if self.send_value(self.val) {
            actions.push(ProcessAction::WriteDbLink {
                link_field: "OUT",
                value: EpicsValue::Double(self.sent),
            });
        }

        // Arm the delay timer (C `callbackRequestDelayed`, lines 592-593)
        // when DLY > 0. WAIT is True for the duration of the delay: C
        // sets `prec->wait = TRUE` before enterValue, and although
        // `valuePut` clears it after the OUT write, the freshly-armed
        // timer means the operator-visible post-cycle state is Busy
        // until the drain completes.
        if self.dly > 0.0 {
            self.delay_active = true;
            self.wait = 1;
            let delay = std::time::Duration::from_secs_f64(self.dly);
            actions.push(ProcessAction::ReprocessAfter(delay));
            return Ok(ProcessOutcome::complete_with(actions));
        }

        // No delay: C `valuePut` sets WAIT=False after the immediate
        // write (lines 575/587).
        self.delay_active = false;
        self.wait = 0;
        Ok(ProcessOutcome::complete_with(actions))
    }

    fn can_device_write(&self) -> bool {
        true
    }

    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
            return Ok(());
        }
        match field {
            // C `special()` DLY case (lines 392-409). A negative delay
            // is clamped to 0. C also cancels/restarts the in-flight
            // `delayFuncCb` so a previously-set huge delay does not keep
            // the record Busy; the port re-derives the remaining delay
            // from `last_send_time` + the new DLY on the next process,
            // so a shrunk DLY takes effect on the next drain attempt.
            //
            // `special()` runs after the field write. `put_field("DLY")`
            // already rejects non-finite and huge-but-finite values via
            // `validate_dly`, so a CA/db path can never leave `self.dly`
            // out of range here. The clamp below additionally enforces
            // the `Duration::from_secs_f64` invariant for any other
            // writer of `self.dly` (e.g. in-process callers), so every
            // reader downstream of `special()` is safe.
            "DLY" => {
                if self.dly < 0.0 {
                    self.dly = 0.0;
                } else if validate_dly(self.dly).is_err() {
                    // Non-finite or >= MAX_DLY: clamp to the operational
                    // ceiling so `process()` never panics.
                    self.dly = MAX_DLY;
                }
            }
            // C `special()` DRVLH/DRVLL case (lines 411-440). When the
            // new limits disable limiting (`drvlh <= drvll`) DRVLS goes
            // Normal. When limiting is (re)enabled DRVLS is recomputed
            // immediately against the *current* VAL — Low if below the
            // low limit, High if above the high limit, else Normal.
            "DRVLH" | "DRVLL" => {
                self.limit_flag = self.drvlh > self.drvll;
                if !self.limit_flag {
                    self.drvls = 0; // throttleDRVLS_NORM
                } else if self.val < self.drvll {
                    self.drvls = 1; // throttleDRVLS_LOW
                } else if self.val > self.drvlh {
                    self.drvls = 2; // throttleDRVLS_HIGH
                } else {
                    self.drvls = 0; // throttleDRVLS_NORM
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "OVAL" => Some(EpicsValue::Double(self.oval)),
            "SENT" => Some(EpicsValue::Double(self.sent)),
            "OSENT" => Some(EpicsValue::Double(self.osent)),
            "WAIT" => Some(EpicsValue::Short(self.wait)),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "DRVLH" => Some(EpicsValue::Double(self.drvlh)),
            "DRVLL" => Some(EpicsValue::Double(self.drvll)),
            "DRVLS" => Some(EpicsValue::Short(self.drvls)),
            "DRVLC" => Some(EpicsValue::Short(self.drvlc)),
            "VER" => Some(EpicsValue::String(self.ver.clone().into())),
            "STS" => Some(EpicsValue::Short(self.sts)),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "DPREC" => Some(EpicsValue::Short(self.dprec)),
            "DLY" => Some(EpicsValue::Double(self.dly)),
            "OUT" => Some(EpicsValue::String(self.out.clone().into())),
            "OV" => Some(EpicsValue::Short(self.ov)),
            "SINP" => Some(EpicsValue::String(self.sinp.clone().into())),
            "SIV" => Some(EpicsValue::Short(self.siv)),
            "SYNC" => Some(EpicsValue::Short(self.sync)),
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
            "DRVLH" => match value {
                EpicsValue::Double(v) => {
                    self.drvlh = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DRVLL" => match value {
                EpicsValue::Double(v) => {
                    self.drvll = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DRVLC" => match value {
                EpicsValue::Short(v) => {
                    self.drvlc = v;
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
            "DPREC" => match value {
                EpicsValue::Short(v) => {
                    self.dprec = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DLY" => match value {
                EpicsValue::Double(v) => {
                    // C `throttleRecord.c` models the delay with
                    // `Duration::from_secs_f64(self.dly)` in `process()`,
                    // which panics not only on a non-finite argument but
                    // on any finite value too large for a `Duration`
                    // (≈ 1.8e19; message "value is either too big or
                    // NaN"). C's `special()` DLY handler (lines 392-409)
                    // only ever anticipated a negative delay; a CA put of
                    // `+inf`, `NaN`, or a huge-but-finite f64 like `1e300`
                    // is not a value any real delay can represent. Reject
                    // it here, at the single writer of `self.dly`, so the
                    // record task can never panic — `validate_dly` is the
                    // gate that holds the invariant "`self.dly` can never
                    // make `Duration::from_secs_f64` panic".
                    validate_dly(v)?;
                    self.dly = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OUT" => match value {
                EpicsValue::String(v) => {
                    self.out = v.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SINP" => match value {
                EpicsValue::String(v) => {
                    self.sinp = v.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SYNC" => match value {
                EpicsValue::Short(v) => {
                    self.sync = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            // Read-only fields
            "OVAL" | "SENT" | "OSENT" | "WAIT" | "DRVLS" | "VER" | "STS" | "OV" | "SIV" => {
                Err(CaError::ReadOnlyField(name.into()))
            }
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        FIELDS
    }

    /// Record-specific `DBF_MENU` fields, served as `DBR_ENUM` with the
    /// menu's choice labels in `.dbd` index order (C `throttleRecord.dbd`):
    /// `WAIT`=`throttleWAIT`, `DRVLC`=`throttleDRVLC`,
    /// `DRVLS`=`throttleDRVLS`, `STS`=`throttleSTS`,
    /// `OV`/`SIV`=`throttleOV`, `SYNC`=`throttleSYNC`.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "WAIT" => Some(THROTTLE_WAIT_CHOICES),
            "DRVLC" => Some(THROTTLE_DRVLC_CHOICES),
            "DRVLS" => Some(THROTTLE_DRVLS_CHOICES),
            "STS" => Some(THROTTLE_STS_CHOICES),
            "OV" | "SIV" => Some(THROTTLE_OV_CHOICES),
            "SYNC" => Some(THROTTLE_SYNC_CHOICES),
            _ => None,
        }
    }

    /// C `throttleRecord.c:308` keeps `recGblFwdLink(prec)` commented
    /// out in `process()` — the forward link is fired ONLY from
    /// `valuePut`'s non-CONSTANT branch (`throttleRecord.c:580`), i.e.
    /// only on a cycle where a real OUT write actually occurred. The
    /// framework default fires FLNK every `process()`, which would also
    /// fire it on a queuing-during-delay cycle, a rejected out-of-range
    /// cycle, a drain with nothing queued, and a CONSTANT-OUT cycle —
    /// none of which write OUT in C. `process()` maintains `out_written`
    /// (reset to false each cycle, set true only by `send_value` on a
    /// real OUT write); this hook returns it.
    fn should_fire_forward_link(&self) -> bool {
        self.out_written
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        // C `init_record` (throttleRecord.c:133-228). Pass 0 copies the
        // VERSION string into VER; the Rust port sets VER in `Default`
        // instead (the framework constructs the record before init).
        //
        // Pass 1 (C lines 156-167): STS is reset to Unknown and VAL to
        // 0, and `limit_flag` is derived from `drvlh > drvll`. C also
        // resets the private delay/wait/sync flags to 0 — mirrored by
        // the runtime-state fields below.
        if pass == 1 {
            self.sts = 0; // throttleSTS_UNK
            self.val = 0.0;
            self.limit_flag = self.drvlh > self.drvll;
            self.delay_active = false;
            self.last_send_time = None;
            self.pending_value = None;
            self.out_written = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod menu_choice_tests {
    use super::ThrottleRecord;
    use epics_base_rs::server::record::Record;

    // Choice tables must match throttleRecord.dbd menu value order — the
    // index↔string mapping is wire-visible to clients.
    #[test]
    fn throttle_menu_field_choices_match_dbd() {
        let rec = ThrottleRecord::default();
        assert_eq!(rec.menu_field_choices("WAIT"), Some(&["False", "True"][..]));
        assert_eq!(rec.menu_field_choices("DRVLC"), Some(&["Off", "On"][..]));
        assert_eq!(
            rec.menu_field_choices("DRVLS"),
            Some(&["Normal", "Low Limit", "High Limit"][..])
        );
        assert_eq!(
            rec.menu_field_choices("STS"),
            Some(&["Unknown", "Error", "Success"][..])
        );
        // OV and SIV share menu(throttleOV).
        let ov = &["Ext PV NC", "Ext PV OK", "Local PV", "Constant"][..];
        assert_eq!(rec.menu_field_choices("OV"), Some(ov));
        assert_eq!(rec.menu_field_choices("SIV"), Some(ov));
        assert_eq!(
            rec.menu_field_choices("SYNC"),
            Some(&["Idle", "Process"][..])
        );
        assert_eq!(rec.menu_field_choices("VAL"), None);
    }
}
