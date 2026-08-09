use std::time::Instant;

use super::dbd_generated;
use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::AsyncDbHandle;
use epics_base_rs::server::record::{
    FieldDesc, LinkType, ProcessAction, ProcessOutcome, Record, link_field_type,
};
use epics_base_rs::server::records::link_status::{
    LINK_CON, LINK_EXT_NC, LinkRole, LinkStatusGen, classify_link,
};
use epics_base_rs::types::EpicsValue;

// `menu(throttleSTS)` indices (throttleRecord.dbd) — STS is set only by a
// real link operation (`valuePut`/`valueSync`), never by the limit block.
const THROTTLE_STS_ERR: i16 = 1; // throttleSTS_ERR
const THROTTLE_STS_SUC: i16 = 2; // throttleSTS_SUC
// `menu(throttleSYNC)` indices: 0=Idle, 1=Process (throttleRecord.dbd).
const THROTTLE_SYNC_IDLE: i16 = 0; // throttleSYNC_IDLE
const THROTTLE_SYNC_PROCESS: i16 = 1; // throttleSYNC_PROC

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
    /// Async DB handle + this record's name, installed by the framework via
    /// `set_async_context` when the record is registered. `None` until then
    /// (e.g. a `process()`-only unit test that never registers). Drives the
    /// two operations C performs off the synchronous `process()` path: the
    /// `SYNC` SINP read (C `valueSync` → `dbGetLink`) and the OV/SIV
    /// link-status classification (C `init_record`/`special` → `dbNameToAddr`).
    async_ctx: Option<(String, AsyncDbHandle)>,
    /// Generation gate for OV/SIV link-status refreshes — only the latest
    /// classification may publish, so an init-time snapshot finishing late
    /// cannot clobber a newer `special()` re-point (mirrors sseq; C
    /// re-validates OV/SIV on every OUT/SINP `special()`). Scoped to the
    /// OV/SIV refresh only; the `SYNC` read is not gated (see `special`).
    link_gen: LinkStatusGen,
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
            async_ctx: None,
            link_gen: LinkStatusGen::default(),
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
    /// Classify the OUT and SINP links into OV/SIV and post the result,
    /// mirroring C `init_record`/`special` link management
    /// (throttleRecord.c:171-205, 339-374): CONSTANT→`Constant`, a PV on
    /// this IOC→`Local PV`, else→`Ext PV NC`. epics-base-rs has no CA
    /// client, so an external link never reaches `Ext PV OK` — C's
    /// `checkLinkCallback` EXT transition (throttleRecord.c:660-740) is
    /// unreachable here, the same limitation as sseq's connection re-poll.
    /// Runs at record init (via `set_async_context`) and on every OUT/SINP
    /// `special()`. A no-op when the record is not registered (no handle).
    fn refresh_link_status(&self) {
        let Some((name, handle)) = &self.async_ctx else {
            return;
        };
        let name = name.clone();
        let handle = handle.clone();
        let out = self.out.clone();
        let sinp = self.sinp.clone();
        let link_gen = self.link_gen.clone();
        // Stamp this refresh so a later re-point (an OUT/SINP `special()`)
        // supersedes an init-time snapshot that finishes late.
        let token = link_gen.next();
        let sched = handle.clone();
        // Through the database's `iocInit` owner — see `schedule_record_init`.
        // The parking key; `name` itself moves into the future below.
        let init_key = name.clone();
        sched.schedule_record_init(&init_key, async move {
            // OUT is written to, SINP is read from — the classifier answers a
            // CONSTANT link's field-type code by direction.
            let (ov, _) = classify_link(&handle, &out, LinkRole::Output);
            let (siv, _) = classify_link(&handle, &sinp, LinkRole::Input);
            if link_gen.is_current(token) {
                let _ = handle.post_fields(
                    &name,
                    vec![
                        ("OV".to_string(), EpicsValue::Short(ov)),
                        ("SIV".to_string(), EpicsValue::Short(siv)),
                    ],
                );
            }
        });
    }

    /// C `valueSync` (throttleRecord.c:616-656): read SINP into VAL as
    /// `DBR_DOUBLE` and post VAL/STS/SYNC — NO OUT write, NO process, NO
    /// FLNK. A CONSTANT SINP (SIV=`Constant`) yields STS=Error with no read
    /// (C's `plink->type == CONSTANT` else branch); a local read failure
    /// also yields STS=Error. SYNC is reset to Idle on completion. Only
    /// called for SIV ∈ {Local, Constant} (the `EXT_NC` skip is in
    /// `special`). Not generation-gated: a rare double-SYNC resolves
    /// last-scheduled-wins, benign because VAL is latest-value anyway.
    fn spawn_value_sync(&self) {
        let Some((name, handle)) = &self.async_ctx else {
            return;
        };
        let name = name.clone();
        let handle = handle.clone();
        let sinp = self.sinp.clone();
        let siv = self.siv;
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            let mut fields: Vec<(String, EpicsValue)> = Vec::with_capacity(3);
            if siv == LINK_CON {
                // C `valueSync`: a CONSTANT SINP is never read → STS=Error.
                fields.push(("STS".to_string(), EpicsValue::Short(THROTTLE_STS_ERR)));
            } else {
                // SIV=Local: C `dbGetLink(SINP, DBR_DOUBLE, &sival)` — read
                // the source coerced to double, regardless of its native type.
                match handle.read_link_value(&sinp).await.and_then(|v| v.to_f64()) {
                    Some(v) => {
                        fields.push(("VAL".to_string(), EpicsValue::Double(v)));
                        fields.push(("STS".to_string(), EpicsValue::Short(THROTTLE_STS_SUC)));
                    }
                    None => fields.push(("STS".to_string(), EpicsValue::Short(THROTTLE_STS_ERR))),
                }
            }
            // C posts SYNC=Idle last (throttleRecord.c:651).
            fields.push(("SYNC".to_string(), EpicsValue::Short(THROTTLE_SYNC_IDLE)));
            let _ = handle.post_fields(&name, fields);
        });
    }

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

impl Record for ThrottleRecord {
    fn record_type(&self) -> &'static str {
        "throttle"
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
                    // C `valuePut` clears `prec->wait = FALSE` immediately
                    // after the OUT write (throttleRecord.c:575) and only
                    // THEN re-arms the timer (:592-593). WAIT means "an
                    // un-written value is pending", not "a delay timer is
                    // running": the just-drained value is now written, the
                    // queue is empty (`pending_value` was taken), so WAIT
                    // stays clear through the new cooldown. A value that
                    // arrives during it re-sets WAIT in the queue branch
                    // below.
                    if self.dly > 0.0 {
                        self.delay_active = true;
                        self.wait = 0;
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
        // value wins, as in C) so the drain re-process sends it. This is
        // the ONE state where C leaves WAIT set: `process()` set
        // `prec->wait = TRUE` (throttleRecord.c:287) and, with a delay in
        // progress, `enterValue` does NOT call `valuePut` (:525), so
        // nothing clears it — the value is now queued, un-written. WAIT=1
        // therefore means exactly `pending_value.is_some()`; the
        // in-flight ReprocessAfter is left to fire.
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
        // when DLY > 0, and clear WAIT. C `process()` sets `prec->wait =
        // TRUE` before `enterValue` (throttleRecord.c:287), but on this
        // immediate path `valuePut` runs in the same cycle and clears
        // `prec->wait = FALSE` right after the OUT write (:575) BEFORE
        // re-arming the timer. C's WAIT means "an un-written value is
        // pending", not "a delay timer is running": the value just
        // written is no longer pending, so WAIT is clear through the
        // cooldown even though the timer is armed. WAIT is re-set only
        // when a value is queued during the delay (the branch above).
        if self.dly > 0.0 {
            self.delay_active = true;
            self.wait = 0;
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
            // C `special()` OUT/SINP case (throttleRecord.c:339-374,
            // `SPC_MOD`): re-classify the changed link's validity menu
            // (OV for OUT, SIV for SINP) — CONSTANT→`Constant`, a PV on
            // this IOC→`Local PV`, else→`Ext PV NC`. The new link string the
            // put just stored is classified off-thread (needs an async DB
            // lookup, C `dbNameToAddr`); `refresh_link_status` re-does BOTH
            // OV and SIV, which is harmless and keeps a single owner.
            "OUT" | "SINP" => self.refresh_link_status(),
            // C `special()` SYNC case (throttleRecord.c:376-389): a put of
            // `SYNC=Process` triggers `valueSync` — read SINP into VAL and
            // post (NO OUT write, NO process). C gates on `siv`: an
            // unconnected external SINP (`EXT_NC`) is NOT synced (its
            // `checkLink` can never connect here — no CA client), so SYNC
            // is left in `Process`, matching C leaving it pending.
            "SYNC" => {
                if self.sync == THROTTLE_SYNC_PROCESS && self.siv != LINK_EXT_NC {
                    self.spawn_value_sync();
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

    fn declared_fields(&self) -> &'static [FieldDesc] {
        dbd_generated::THROTTLE_FIELDS
    }

    fn declared_noaccess_fields(&self) -> &'static [&'static str] {
        dbd_generated::THROTTLE_NOACCESS
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

    fn set_async_context(&mut self, name: String, db: AsyncDbHandle) {
        self.async_ctx = Some((name, db));
        // C `init_record` classifies the OUT/SINP links into OV/SIV and
        // posts the initial status (throttleRecord.c:171-205). This is the
        // framework's init-time async hook (the handle now exists), so
        // classify here — the record's OUT/SINP db fields are already loaded.
        self.refresh_link_status();
    }

    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        // OV/SIV (link-status classifier) and STS (the SYNC SINP read) are
        // read-only to *clients* — the field_io `SPC_NOMOD` gate rejects a
        // client put — but the trusted out-of-band post (`post_fields` →
        // here) must land. Store them directly; the strict `put_field` arm
        // still rejects a client write (same split sseq uses for its
        // read-only DOLnV/LNKnV diagnostics). Every other field, including
        // the writable VAL/SYNC the SYNC post also writes, falls through to
        // `put_field`.
        match (name, &value) {
            ("OV", EpicsValue::Short(v)) => {
                self.ov = *v;
                Ok(())
            }
            ("SIV", EpicsValue::Short(v)) => {
                self.siv = *v;
                Ok(())
            }
            ("STS", EpicsValue::Short(v)) => {
                self.sts = *v;
                Ok(())
            }
            _ => self.put_field(name, value),
        }
    }
}

#[cfg(test)]
mod menu_choice_tests {
    use super::ThrottleRecord;
    use epics_base_rs::server::record::FieldDeclaration;

    /// The choices a client sees are the DECLARATION's — `throttleRecord.dbd`'s
    /// `menu()` on each field — and the index↔string mapping is wire-visible.
    /// This used to assert them through `Record::menu_field_choices`, a hand
    /// written table that declared the same menus a second time.
    #[test]
    fn throttle_menu_choices_come_from_the_declaration() {
        let rec = ThrottleRecord::default();
        let menu = |name: &str| {
            rec.field_list()
                .iter()
                .find(|f| f.name == name)
                .unwrap_or_else(|| panic!("{name} is declared"))
                .menu
        };
        assert_eq!(menu("WAIT"), Some(&["False", "True"][..]));
        assert_eq!(menu("DRVLC"), Some(&["Off", "On"][..]));
        assert_eq!(
            menu("DRVLS"),
            Some(&["Normal", "Low Limit", "High Limit"][..])
        );
        assert_eq!(menu("STS"), Some(&["Unknown", "Error", "Success"][..]));
        // OV and SIV share menu(throttleOV).
        let ov = &["Ext PV NC", "Ext PV OK", "Local PV", "Constant"][..];
        assert_eq!(menu("OV"), Some(ov));
        assert_eq!(menu("SIV"), Some(ov));
        assert_eq!(menu("SYNC"), Some(&["Idle", "Process"][..]));
        assert_eq!(menu("VAL"), None);
    }
}
