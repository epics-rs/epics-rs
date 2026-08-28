use super::dbd_generated;
use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::AsyncDbHandle;
use epics_base_rs::server::record::{
    FieldDesc, FieldMetadataOverride, LinkType, ProcessAction, ProcessOutcome, Record,
    link_field_type,
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
    /// Whether the DLY cooldown timer is armed — C `rpvtStruct.delay_flag`.
    delay_active: bool,
    /// The value waiting to be written to OUT — C `rpvtStruct.wait_flag`
    /// together with the `prec->val` its `valuePut` reads at drain time.
    /// `is_some()` is exactly C's `wait_flag == 1`, the only thing
    /// `valuePut` branches on (throttleRecord.c:551).
    pending_value: Option<f64>,
    /// Set by `set_process_continuation` when the framework is re-entering
    /// `process()` for this record's own `ReprocessAfter` — the port's
    /// stand-in for C's `delayFuncCallback` (throttleRecord.c:530-538),
    /// which is a separate function and needs no marker. Consumed by the
    /// `process()` it marks, so a path that never sets it runs as a fresh
    /// cycle.
    timer_fire: bool,
    /// A SYNC request has been made but not yet carried out — C
    /// `rpvtStruct.sync_flag`. Set while a value is still waiting to reach
    /// OUT, because C `valueSync` returns early in that case
    /// (throttleRecord.c:625-627) and the successful `dbPutLink` arm of
    /// `valuePut` finishes the sync instead (:569-570).
    sync_flag: bool,
    /// A DLY put landed while the cooldown was running, so the timer must be
    /// re-anchored to the new delay — C `special()` cancels and re-requests
    /// the callback (throttleRecord.c:400-408). Drained by
    /// `take_special_actions`.
    rearm_delay: bool,
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
    /// The callback band this record's `PRIO` selects, refreshed from
    /// [`ProcessContext`](epics_base_rs::server::record::ProcessContext)
    /// before every `process()` — C reads `prec->prio` at the same point
    /// (`callbackSetPriority(prec->prio, &pcb->callback)`,
    /// `seqRecord.c:145-146`). Low until the first cycle, the band an
    /// unwritten `PRIO` already has.
    callback_priority: epics_base_rs::runtime::task::CallbackPriority,
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
            pending_value: None,
            timer_fire: false,
            sync_flag: false,
            rearm_delay: false,
            out_written: false,
            async_ctx: None,
            callback_priority: epics_base_rs::runtime::task::CallbackPriority::Low,
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

    /// The body of C `valueSync` past its early return
    /// (throttleRecord.c:628-655): read SINP into VAL as `DBR_DOUBLE` and
    /// post VAL/STS/SYNC — NO OUT write, NO process, NO FLNK. A CONSTANT
    /// SINP (SIV=`Constant`) yields STS=Error with no read (C's
    /// `plink->type == CONSTANT` else branch); a local read failure also
    /// yields STS=Error. SYNC is reset to Idle on completion.
    ///
    /// Reached only through [`Self::value_sync`], which owns the
    /// `wait_flag` deferral C puts in front of it, and only for SIV ∈
    /// {Local, Constant} (the `EXT_NC` skip is in `special`). Not
    /// generation-gated: a rare double-SYNC resolves last-scheduled-wins,
    /// benign because VAL is latest-value anyway.
    ///
    /// Deferred on the process-global background executor, not the ambient
    /// one: both entries are record-support callbacks (`special` for the SYNC
    /// put, `set_out_link_write_status` for the deferred completion), and the
    /// framework drives those from threads with no tokio runtime — a blocking
    /// CA/PVA connection thread through `block_on_sync` → `park_on`, or the
    /// callback pool a record tail was deferred to. Nothing awaited below
    /// needs a reactor: the SINP read is a database call and `post_fields` is
    /// synchronous.
    fn spawn_value_sync(&self) {
        let Some((name, handle)) = &self.async_ctx else {
            return;
        };
        let name = name.clone();
        let handle = handle.clone();
        let sinp = self.sinp.clone();
        let siv = self.siv;
        let prio = self.callback_priority;
        epics_base_rs::runtime::task::spawn_background(prio, async move {
            epics_base_rs::runtime::task::yield_now().await;
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
            // C posts SYNC=Idle last (throttleRecord.c:651-652).
            fields.push(("SYNC".to_string(), EpicsValue::Short(THROTTLE_SYNC_IDLE)));
            let _ = handle.post_fields(&name, fields);
        });
    }

    /// C `valueSync` (throttleRecord.c:616-656) — the single entry to a
    /// SINP sync, and the owner of the deferral in front of it.
    ///
    /// A sync must not overwrite VAL while a value is still waiting to reach
    /// OUT: C marks the request and returns (:623-627), and `valuePut`
    /// finishes it from its successful `dbPutLink` arm (:569-570), so VAL
    /// takes the SINP value read AFTER the queued value went out, not one
    /// read while it was still queued. Without the deferral the port read
    /// SINP at request time and posted a VAL that the later drain never
    /// corrected.
    ///
    /// Both endpoints resolve at std `83c1475`, the revision this record was
    /// written from; they were two lines off against the checkout's `06c6f4a`,
    /// which is a fork branch commit that is on no master.
    fn value_sync(&mut self) {
        self.sync_flag = true;
        if self.pending_value.is_some() {
            return;
        }
        self.spawn_value_sync();
        self.sync_flag = false;
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

    /// C `valuePut` (throttleRecord.c:540-613) — the single owner of the OUT
    /// write, the WAIT clear and the cooldown re-arm.
    ///
    /// Reached from exactly the two places C reaches it from: `enterValue`
    /// when no cooldown is running (:523-524), and the cooldown timer
    /// expiring (:530-538). Which of the two it is does not change what
    /// happens here — C branches only on `wait_flag`, i.e. on whether a value
    /// is actually waiting.
    fn value_put(&mut self, actions: &mut Vec<ProcessAction>) {
        let Some(value) = self.pending_value.take() else {
            // C :597-599 — the timer found nothing waiting. It writes
            // nothing, queues nothing and posts nothing; it only clears
            // `delay_flag`.
            self.delay_active = false;
            return;
        };

        // C :556-587 branches on the OUT link type. A CONSTANT/empty OUT is
        // never written: STS is forced to Error, SENT/OSENT stay put and the
        // forward link does not fire (:583-587). A real link gets the
        // `dbPutLink` — and C reads STS and SENT out of ITS result (:565-575),
        // which the port learns only once the framework has executed this
        // action and called `set_out_link_write_status`. So nothing about the
        // outcome is committed here; only the attempt is. Both arms clear
        // WAIT (:575/:586), and C fires the forward link on the whole
        // non-CONSTANT arm, success or not (:580).
        let out_type = link_field_type(&self.out);
        if out_type == LinkType::Constant || out_type == LinkType::Empty {
            self.sts = THROTTLE_STS_ERR;
            self.out_written = false;
        } else {
            self.out_written = true;
            actions.push(ProcessAction::WriteDbLink {
                link_field: "OUT",
                value: EpicsValue::Double(value),
            });
        }
        self.wait = 0;

        // C :592-593 re-arms unconditionally, even for `delay == 0`: a
        // zero-delay `callbackRequestDelayed` fires at once, finds
        // `wait_flag == 0` and clears `delay_flag` again. The port collapses
        // that round trip — with DLY = 0 there is no cooldown, so the next
        // value goes straight out — rather than spawning a timer task per put
        // whose only job is to switch a flag back off.
        if self.dly > 0.0 {
            self.delay_active = true;
            actions.push(ProcessAction::ReprocessAfter(
                std::time::Duration::from_secs_f64(self.dly),
            ));
        } else {
            self.delay_active = false;
        }
    }
}

impl Record for ThrottleRecord {
    /// The one field C's `get_precision` departs from `prec->prec` for:
    /// `*precision = prec->dprec` when `fieldIndex == throttleRecordDLY`
    /// (`throttleRecord.c:451-464`). Every other field takes PREC through the
    /// generic seed, and DLY is the only DBF_DOUBLE among the two, so without
    /// this the delay's own display precision was unreachable over both CA and
    /// PVA.
    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        field
            .eq_ignore_ascii_case("DLY")
            .then(|| FieldMetadataOverride {
                precision: Some(self.dprec),
                ..Default::default()
            })
    }

    fn record_type(&self) -> &'static str {
        "throttle"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `throttleRecord.c:231-312`. TWO different C entry points arrive at
        // this one Rust function, and the framework's continuation marker is
        // what tells them apart:
        //
        //   * `delayFuncCallback` (:530-538) — the DLY cooldown expiring. C
        //     dispatches it straight to `valuePut()`; the port models the
        //     timer with `ProcessAction::ReprocessAfter`, so it comes back as
        //     a re-entrant `process()` flagged by `set_process_continuation`.
        //   * a fresh put / scan / forward link — C's `process()` proper: the
        //     drive-limit block (:242-283), then `enterValue()` (:517-528),
        //     which marks the value pending and calls `valuePut()` only when
        //     no cooldown is running.
        let mut actions = Vec::new();

        // C `throttleRecord.c:308` keeps `recGblFwdLink` commented out in
        // `process()`; the forward link fires ONLY from `valuePut`'s
        // non-CONSTANT branch (:580). Reset the per-cycle FLNK flag here so a
        // queuing-during-delay cycle, a rejected out-of-range cycle, or a
        // timer fire with nothing waiting does NOT fire FLNK — only a real
        // OUT write (via `value_put`) sets it true.
        self.out_written = false;

        // C `delayFuncCallback`: the cooldown expired, so run `valuePut` and
        // nothing else — no limit block, no `enterValue`, no OVAL update.
        if std::mem::take(&mut self.timer_fire) {
            self.value_put(&mut actions);
            return Ok(ProcessOutcome::complete_with(actions));
        }

        // --- Drive-limit block (C :242-283), every fresh process() ---
        //
        // C restores `prec->val = prec->oval` and sets `proc_flag = 0` on a
        // rejected (out-of-range, clipping Off) value; it does NOT set STS and
        // does NOT touch WAIT. STS is written only after a real link
        // operation (`valuePut` / `valueSync`).
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
            // C `proc_flag == 0`: skip `enterValue` entirely. A cooldown
            // already running is left alone — its timer still fires and
            // drains whatever is waiting. C's end-of-process OVAL block is a
            // no-op here because `val` was just restored to `oval`.
            return Ok(ProcessOutcome::complete_with(actions));
        }

        // C :285-286 — every accepted process marks the record busy; only
        // `valuePut` clears it.
        self.wait = 1;

        // C `enterValue` (:517-528): set `wait_flag` — the waiting value is
        // `prec->val` itself, last one wins — then call `valuePut` only when
        // no cooldown is running. With one running, the timer armed at the
        // last send is still pending and will pick this value up; C requests
        // no second callback here, and neither may the port, or the record
        // would re-anchor its own cooldown on every put.
        self.pending_value = Some(self.val);
        if !self.delay_active {
            self.value_put(&mut actions);
        }

        // OVAL end-of-process update (C :299-303). `prec->oval` (the OVAL
        // field) is distinct from the `prpvt->oval` that `valuePut` hands to
        // `dbPutLink`.
        self.oval = self.val;

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
            // is clamped to 0, and a delay changed while the cooldown is
            // running cancels the in-flight `delayFuncCb` and re-requests
            // it with the NEW delay (:400-408) — so a delay "set crazy
            // big" cannot hold the record forever and a shrunk one takes
            // effect at once. `take_special_actions` carries that re-anchor
            // out as a fresh `ReprocessAfter`; minting its token supersedes
            // the pending one, which IS C's `callbackCancelDelayed`.
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
                if self.delay_active {
                    self.rearm_delay = true;
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
                    self.value_sync();
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
    /// (reset to false each cycle, set true only by `value_put` on a
    /// real OUT write); this hook returns it.
    fn should_fire_forward_link(&self) -> bool {
        self.out_written
    }

    /// Carries out the DLY re-anchor C `special()` performs at
    /// `throttleRecord.c:400-408`. The framework drains this in the same step
    /// as the `special()` that queued it, so a re-arm can never outlive its
    /// put; `self.dly` is already clamped by that `special()`, so the
    /// `Duration` is always representable.
    fn take_special_actions(&mut self) -> Vec<ProcessAction> {
        if std::mem::take(&mut self.rearm_delay) {
            vec![ProcessAction::ReprocessAfter(
                std::time::Duration::from_secs_f64(self.dly),
            )]
        } else {
            Vec::new()
        }
    }

    /// The framework's continuation marker is what separates C's two entry
    /// points into `valuePut`: the DLY cooldown timer firing
    /// (`delayFuncCallback`, throttleRecord.c:530-538) from a fresh
    /// put/scan/forward-link `process()` (:231). C never needed a marker —
    /// the timer dispatches to its own function — so the port takes the one
    /// the framework already computes rather than guessing from a clock,
    /// which a DLY change mid-cooldown silently falsifies.
    fn set_process_continuation(&mut self, continuation: bool) {
        self.timer_fire = continuation;
    }

    /// C `valuePut`'s `dbPutLink` result branch (throttleRecord.c:564-575):
    /// STS is `throttleSTS_SUC` only when the put succeeded and
    /// `throttleSTS_ERR` when it did not, and SENT advances only on success.
    /// The record has a dedicated STS field precisely so a client can tell a
    /// value that reached the device from one that did not, so it is derived
    /// from the put here rather than assumed when the write is emitted.
    fn set_out_link_write_status(
        &mut self,
        link_field: &'static str,
        value: &EpicsValue,
        failed: bool,
    ) {
        if link_field != "OUT" {
            return;
        }
        if failed {
            self.sts = THROTTLE_STS_ERR;
            return;
        }
        self.sts = THROTTLE_STS_SUC;
        // OSENT trails SENT by one send, as the tail of C `valuePut` keeps
        // it (throttleRecord.c:606-612). Not `monitor()` — that function is
        // commented out at the pin (declaration `throttleRecord.c:98`, body
        // `:492-500`), so `valuePut` posts its own monitors inline and there
        // is no `monitor()` to go looking for.
        if let Some(v) = value.to_f64() {
            self.osent = self.sent;
            self.sent = v;
        }
        // C :569-570 — a SYNC deferred behind this value completes here, and
        // only from the successful arm: a put that failed leaves the request
        // standing for the next one.
        if self.sync_flag {
            self.value_sync();
        }
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
            self.pending_value = None;
            self.timer_fire = false;
            self.sync_flag = false;
            self.rearm_delay = false;
            self.out_written = false;
        }
        Ok(())
    }

    fn set_process_context(&mut self, ctx: &epics_base_rs::server::record::ProcessContext) {
        // A `PRIO` written between cycles moves the next one, as in C where
        // `callbackSetPriority` is re-run inside `process()`.
        self.callback_priority = ctx.callback_priority;
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
