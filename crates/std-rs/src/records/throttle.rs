use super::dbd_generated;
use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::AsyncDbHandle;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{
    CycleHooks, CycleMarks, CyclePostMask, FieldDesc, FieldMetadataOverride, InputLinkRequest,
    LinkRead, LinkReadAs, LinkType, ProcessAction, ProcessActions, ProcessOutcome, Record,
    link_field_type,
};
use epics_base_rs::server::records::link_status::{
    LINK_EXT_NC, LINK_RECHECK, LinkCheck, LinkCheckSpec, LinkCheckState, LinkRole, LinkRule,
    LinkStatusGen, classify_link, link_is_external,
};
use epics_base_rs::types::{EpicsValue, PvString};
use epics_libcom_rs::cfmt::CFloat;

// `menu(throttleSTS)` indices (throttleRecord.dbd) — STS is set only by a
// real link operation (`valuePut`/`valueSync`), never by the limit block.
const THROTTLE_STS_ERR: i16 = 1; // throttleSTS_ERR
const THROTTLE_STS_SUC: i16 = 2; // throttleSTS_SUC
// `menu(throttleSYNC)` indices: 0=Idle, 1=Process (throttleRecord.dbd).
const THROTTLE_SYNC_IDLE: i16 = 0; // throttleSYNC_IDLE
const THROTTLE_SYNC_PROCESS: i16 = 1; // throttleSYNC_PROC

/// `valuePut`'s and `valueSync`'s `db_post_events` ahead of the forward link,
/// in C order (`throttleRecord.c:568-584`, `:637-652`), then `special()`'s.
static POSTS: [(epics_base_rs::server::record::FieldIx, CyclePostMask); 7] =
    epics_base_rs::server::record::declared_post_table(
        dbd_generated::THROTTLE_FIELDS,
        [
            ("SENT", CyclePostMask::Value),
            ("VAL", CyclePostMask::Value),
            ("STS", CyclePostMask::Value),
            ("SYNC", CyclePostMask::Value),
            ("WAIT", CyclePostMask::Value),
            ("DRVLS", CyclePostMask::Value),
            ("DLY", CyclePostMask::Value),
        ],
    );
const POST_SENT: u64 = 1;
const POST_SYNC_VAL: u64 = 1 << 1;
const POST_SYNC_STS: u64 = 1 << 2;
const POST_SYNC: u64 = 1 << 3;
const POST_WAIT: u64 = 1 << 4;
const POST_DRVLS: u64 = 1 << 5;
const POST_DLY: u64 = 1 << 6;

/// `valuePut`'s posts after the forward link: STS (`throttleRecord.c:592`),
/// then SENT with VALUE|LOG when it moved, the reset's alarm bits alone
/// otherwise (:606-612).
static TAIL: [(epics_base_rs::server::record::FieldIx, CyclePostMask); 3] =
    epics_base_rs::server::record::declared_post_table(
        dbd_generated::THROTTLE_FIELDS,
        [
            ("STS", CyclePostMask::Value),
            ("SENT", CyclePostMask::ResetValueLog),
            ("SENT", CyclePostMask::ResetAlarm),
        ],
    );

/// `process()`'s own posts ahead of `enterValue` (`throttleRecord.c:251-287`).
static PROCESS_POSTS: [(epics_base_rs::server::record::FieldIx, CyclePostMask); 3] =
    epics_base_rs::server::record::declared_post_table(
        dbd_generated::THROTTLE_FIELDS,
        [
            ("VAL", CyclePostMask::ValueLog),
            ("DRVLS", CyclePostMask::Value),
            ("WAIT", CyclePostMask::Value),
        ],
    );
const PROCESS_POST_VAL: u64 = 1;
const PROCESS_POST_DRVLS: u64 = 1 << 1;
const PROCESS_POST_WAIT: u64 = 1 << 2;

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
    pub ver: PvString,
    /// Record status: 0=Unknown, 1=Error, 2=Success (STS), read-only
    pub sts: i16,
    /// Display precision (PREC)
    pub prec: i16,
    /// Delay display precision (DPREC)
    pub dprec: i16,
    /// Delay between outputs in seconds (DLY)
    pub dly: f64,
    /// Output link (OUT)
    pub out: PvString,
    /// Sync input link (SINP)
    pub sinp: PvString,
    /// OV then SIV, read-only: 0=ExtNC, 1=Ext, 2=Local, 3=Constant.
    link_status: [i16; 2],
    link_check: LinkCheckState,
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
    /// [`POSTS`] marks `valuePut` made, drained by the framework.
    posts: u64,
    /// [`PROCESS_POSTS`] marks `process()` made.
    process_posts: u64,
    /// A `valuePut` ran whose tail is not yet taken.
    tail_due: bool,
    /// `valuePut` ran a wait arm: STS posts after the forward link (:592).
    tail_sts: bool,
    /// The last `process()` found VAL off OVAL (C :298).
    val_moved: bool,
    /// A SYNC request has been made but not yet carried out — C
    /// `rpvtStruct.sync_flag`. Set while a value is still waiting to reach
    /// OUT, because C `valueSync` returns early in that case
    /// (throttleRecord.c:625-627) and the successful `dbPutLink` arm of
    /// `valuePut` finishes the sync instead (:569-570).
    sync_flag: bool,
    /// What `special()` queued: the cooldown re-anchor of a DLY put
    /// (throttleRecord.c:400-408) and the SYNC read. Drained by
    /// `take_special_actions`.
    special_actions: Vec<ProcessAction>,
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
    /// OV/SIV link-status classification (C `init_record`/`special` →
    /// `dbNameToAddr`).
    async_ctx: Option<(epics_base_rs::types::RecordName, AsyncDbHandle)>,
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
            ver: PvString::default(),
            sts: 0, // Unknown
            prec: 0,
            dprec: 0,
            dly: 0.0,
            out: PvString::new(),
            sinp: PvString::new(),
            link_status: [3; 2], // Constant
            link_check: LinkCheckState::default(),
            sync: 0, // Idle
            limit_flag: false,
            delay_active: false,
            pending_value: None,
            posts: 0,
            process_posts: 0,
            val_moved: false,
            tail_due: false,
            tail_sts: false,
            sync_flag: false,
            special_actions: Vec::new(),
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
            "throttle DLY must be finite, got {}",
            CFloat(v).g()
        )));
    }
    if v > MAX_DLY {
        return Err(CaError::InvalidValue(format!(
            "throttle DLY must not exceed {} seconds, got {}",
            CFloat(MAX_DLY).g(),
            CFloat(v).g()
        )));
    }
    Ok(())
}

/// C `checkLink`'s walk: OUT, then SINP (throttleRecord.c:702-747).
static THROTTLE_LINK_CHECK: LinkCheckSpec = LinkCheckSpec {
    links: &[("OUT", "OV"), ("SINP", "SIV")],
    rule: LinkRule::Locality,
    // C `throttleRecord.c:726`, `:732`.
    mask: EventMask::from_bits(EventMask::VALUE.bits() | EventMask::LOG.bits()),
    repost_local: false,
    in_cycle: true,
};

impl ThrottleRecord {
    /// Classify the OUT and SINP links into OV/SIV, mirroring C
    /// `init_record`/`special` link management (throttleRecord.c:171-205,
    /// 339-374): CONSTANT→`Constant`, a PV on this IOC→`Local PV`,
    /// else→`Ext PV NC`, which `checkLinkCallback` re-reads after `recheck`
    /// (throttleRecord.c:222, :370). C classifies, and posts whatever its
    /// value, bit 0 OV and bit 1 SIV of `links`: both at init, the put link's
    /// alone on an OUT/SINP `special()`, which leaves the other status as it
    /// stands. A no-op when the record is not registered (no handle).
    fn refresh_link_status(&self, links: u64, recheck: Option<std::time::Duration>) {
        let Some((name, handle)) = &self.async_ctx else {
            return;
        };
        let name = name.clone();
        let handle = handle.clone();
        let classified: Vec<(usize, &'static str, PvString, LinkRole)> = [
            (0, "OV", self.out.clone(), LinkRole::Output),
            (1, "SIV", self.sinp.clone(), LinkRole::Input),
        ]
        .into_iter()
        .filter(|&(i, ..)| links & 1 << i != 0)
        .collect();
        // Stamp this refresh so a later re-point of the same link (an
        // OUT/SINP `special()`) supersedes an init-time snapshot that
        // finishes late.
        let token = self.link_gen.next_for(links);
        let sched = handle.clone();
        // Through the database's `iocInit` owner — see `schedule_record_init`.
        // The parking key; `name` itself moves into the future below.
        let init_key = name.clone();
        sched.schedule_record_init(&init_key, async move {
            let rule = THROTTLE_LINK_CHECK.rule;
            let fields = classified
                .into_iter()
                .map(|(i, field, link, role)| {
                    let (status, _) = classify_link(&handle, &link, role, rule);
                    (i, field.to_string(), EpicsValue::Short(status))
                })
                .collect();
            token.post_each(&handle, &name, fields, THROTTLE_LINK_CHECK.mask);
            handle.link_check_classified(&name, recheck);
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
    ///
    /// Past the early return, the SINP `dbGetLink` (:631) is a `GetLink`
    /// pushed onto `actions`, finished in `link_read_result`, so a failed
    /// read's LINK alarm folds into `valuePut`'s reset as C's does.
    fn value_sync(&mut self, actions: &mut Vec<ProcessAction>) {
        self.sync_flag = true;
        if self.pending_value.is_some() {
            return;
        }
        actions.push(ProcessAction::GetLink { link_field: "SINP" });
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
            // C :597-599 — the timer found nothing waiting: it clears
            // `delay_flag` and runs only the tail.
            self.delay_active = false;
            self.value_put_tail();
            return;
        };

        // C :556-587 branches on the OUT link type. A CONSTANT/empty OUT is
        // never written: STS is forced to Error and the forward link does not
        // fire (:583-587). A real link gets the `dbPutLink`, and C reads STS
        // and SENT out of ITS result (:565-575), which the port learns in
        // `set_out_link_write_status`; C fires the forward link on the whole
        // non-CONSTANT arm, success or not (:580).
        let out_type = link_field_type(&self.out);
        if out_type == LinkType::Constant || out_type == LinkType::Empty {
            self.sts = THROTTLE_STS_ERR;
            self.out_written = false;
            self.value_put_done();
        } else {
            self.out_written = true;
            actions.push(ProcessAction::WriteDbLink {
                link_field: "OUT",
                value: EpicsValue::Double(value),
            });
        }

        // C :592-593 re-arms unconditionally, even for `delay == 0`: a
        // zero-delay `callbackRequestDelayed` fires at once, finds
        // `wait_flag == 0` and clears `delay_flag` again. The port collapses
        // that round trip — with DLY = 0 there is no cooldown, so the next
        // value goes straight out — rather than spawning a timer task per put
        // whose only job is to switch a flag back off.
        if self.dly > 0.0 {
            self.delay_active = true;
            actions.push(ProcessAction::ValuePutAfter(
                std::time::Duration::from_secs_f64(self.dly),
            ));
        } else {
            self.delay_active = false;
        }
    }

    /// C :575-589 — WAIT clears and STS posts on both OUT arms.
    fn value_put_done(&mut self) {
        self.wait = 0;
        self.posts |= POST_WAIT;
        self.tail_sts = true;
        self.value_put_tail();
    }

    /// C :602-612, taken by `take_value_put_tail` after the forward link.
    fn value_put_tail(&mut self) {
        self.tail_due = true;
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
        // C `throttleRecord.c:231-312`: the drive-limit block (:242-283), then
        // `enterValue()` (:517-528), which marks the value pending and calls
        // `valuePut()` only when no cooldown is running. The cooldown itself
        // is `delayed_value_put`, never a process cycle.
        let mut actions = Vec::new();

        // C `throttleRecord.c:308` keeps `recGblFwdLink` commented out in
        // `process()`; the forward link fires ONLY from `valuePut`'s
        // non-CONSTANT branch (:580). Reset the per-cycle FLNK flag here so a
        // queuing-during-delay cycle or a rejected out-of-range cycle does NOT
        // fire FLNK — only a real OUT write (via `value_put`) sets it true.
        self.out_written = false;

        // --- Drive-limit block (C :242-283) ---
        //
        // C restores `prec->val = prec->oval` and sets `proc_flag = 0` on a
        // rejected (out-of-range, clipping Off) value; it does NOT set STS and
        // does NOT touch WAIT. STS is written only after a real link
        // operation (`valuePut` / `valueSync`).
        let drvls = self.drvls;
        let proc_flag = match self.check_limits(self.val) {
            Ok(clamped) => {
                if clamped != self.val {
                    self.process_posts |= PROCESS_POST_VAL;
                }
                self.val = clamped;
                true
            }
            Err(()) => {
                self.val = self.oval;
                false
            }
        };
        if self.limit_flag && self.drvls != drvls {
            self.process_posts |= PROCESS_POST_DRVLS;
        }

        if proc_flag {
            // C :285-287 — every accepted process marks the record busy; only
            // `valuePut` clears it.
            self.wait = 1;
            self.process_posts |= PROCESS_POST_WAIT;

            // C :289-293 `checkLink` when OUT is a CA link, on accepted
            // cycles only. C keys this on `outLinkStat`, which its SINP
            // `special()` also sets (:370, a copy of the OUT arm) and which
            // lags an OUT re-point until the next `checkLink`; OV is what
            // OUT is now.
            if link_is_external(self.link_status[0]) {
                actions.push(ProcessAction::CheckLinks);
            }

            // C `enterValue` (:517-528): set `wait_flag` — the waiting value
            // is `prec->val` itself, last one wins — then call `valuePut` only
            // when no cooldown is running. With one running, the timer armed
            // at the last send picks this value up; C requests no second
            // callback here, and neither may the port, or the record would
            // re-anchor its own cooldown on every put.
            self.pending_value = Some(self.val);
            if !self.delay_active {
                self.value_put(&mut actions);
            }
        }

        // OVAL end-of-process update (C :297-302), whose VAL post is
        // `monitor_value_changed`'s answer. `prec->oval` (the OVAL field) is
        // distinct from the `prpvt->oval` that `valuePut` hands to
        // `dbPutLink`.
        self.val_moved = self.oval != self.val;
        self.oval = self.val;

        Ok(ProcessOutcome::complete_with(actions))
    }

    fn can_device_write(&self) -> bool {
        true
    }

    fn special(
        &mut self,
        field: &str,
        after: bool,
        _common: &epics_base_rs::server::record::CommonFields,
    ) -> CaResult<()> {
        if !after {
            return Ok(());
        }
        match field {
            // C `special()` DLY case (lines 392-409). A negative delay
            // is clamped to 0 and posted (:393-398), and a delay changed while the cooldown is
            // running cancels the in-flight `delayFuncCb` and re-requests
            // it with the NEW delay (:400-408) — so a delay "set crazy
            // big" cannot hold the record forever and a shrunk one takes
            // effect at once. `take_special_actions` carries that re-anchor
            // out as a fresh `ValuePutAfter`; minting its token supersedes
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
                    self.posts |= POST_DLY;
                } else if validate_dly(self.dly).is_err() {
                    // Non-finite or >= MAX_DLY: clamp to the operational
                    // ceiling so `process()` never panics.
                    self.dly = MAX_DLY;
                }
                if self.delay_active {
                    self.special_actions.push(ProcessAction::ValuePutAfter(
                        std::time::Duration::from_secs_f64(self.dly),
                    ));
                }
            }
            // C `special()` DRVLH/DRVLL case (lines 411-440). When the
            // new limits disable limiting (`drvlh <= drvll`) DRVLS goes
            // Normal. When limiting is (re)enabled DRVLS is recomputed
            // immediately against the *current* VAL — Low if below the
            // low limit, High if above the high limit, else Normal. A moved
            // DRVLS posts (:434-439).
            "DRVLH" | "DRVLL" => {
                self.limit_flag = self.drvlh > self.drvll;
                let drvls = if !self.limit_flag {
                    0 // throttleDRVLS_NORM
                } else if self.val < self.drvll {
                    1 // throttleDRVLS_LOW
                } else if self.val > self.drvlh {
                    2 // throttleDRVLS_HIGH
                } else {
                    0 // throttleDRVLS_NORM
                };
                if self.drvls != drvls {
                    self.drvls = drvls;
                    self.posts |= POST_DRVLS;
                }
            }
            // C `special()` OUT/SINP case (throttleRecord.c:339-374,
            // `SPC_MOD`): re-classify the changed link's validity menu
            // (OV for OUT, SIV for SINP) — CONSTANT→`Constant`, a PV on
            // this IOC→`Local PV`, else→`Ext PV NC`. The new link string the
            // put just stored is classified off-thread (needs an async DB
            // lookup, C `dbNameToAddr`). The other link's status stands.
            "OUT" => self.refresh_link_status(1, Some(LINK_RECHECK)),
            "SINP" => self.refresh_link_status(1 << 1, Some(LINK_RECHECK)),
            // C `special()` SYNC case (throttleRecord.c:376-389): a put of
            // `SYNC=Process` triggers `valueSync` — read SINP into VAL and
            // post (NO OUT write, NO process) — unless SIV still reads
            // `Ext PV NC` after the `checkLink` that `special_checks_links`
            // ran.
            "SYNC" if self.sync == THROTTLE_SYNC_PROCESS && self.link_status[1] != LINK_EXT_NC => {
                let mut actions = std::mem::take(&mut self.special_actions);
                self.value_sync(&mut actions);
                self.special_actions = actions;
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
            "VER" => Some(EpicsValue::String(self.ver.clone())),
            "STS" => Some(EpicsValue::Short(self.sts)),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "DPREC" => Some(EpicsValue::Short(self.dprec)),
            "DLY" => Some(EpicsValue::Double(self.dly)),
            "OUT" => Some(EpicsValue::String(self.out.clone())),
            "OV" => Some(EpicsValue::Short(self.link_status[0])),
            "SINP" => Some(EpicsValue::String(self.sinp.clone())),
            "SIV" => Some(EpicsValue::Short(self.link_status[1])),
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
                    self.out = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SINP" => match value {
                EpicsValue::String(v) => {
                    self.sinp = v;
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
            "OVAL" => match value {
                EpicsValue::Double(v) => {
                    self.oval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SENT" => match value {
                EpicsValue::Double(v) => {
                    self.sent = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OSENT" => match value {
                EpicsValue::Double(v) => {
                    self.osent = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "WAIT" => match value {
                EpicsValue::Short(v) => {
                    self.wait = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DRVLS" => match value {
                EpicsValue::Short(v) => {
                    self.drvls = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "VER" => match value {
                EpicsValue::String(v) => {
                    self.ver = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "STS" => match value {
                EpicsValue::Short(v) => {
                    self.sts = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OV" => match value {
                EpicsValue::Short(v) => {
                    self.link_status[0] = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIV" => match value {
                EpicsValue::Short(v) => {
                    self.link_status[1] = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        dbd_generated::THROTTLE_FIELDS
    }

    fn declared_dbaddr_fields(&self) -> Option<&'static [&'static str]> {
        Some(dbd_generated::THROTTLE_DBADDR)
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
        std::mem::take(&mut self.special_actions)
    }

    /// C `valueSync`'s `dbGetLink` result (`throttleRecord.c:631-652`).
    fn link_read_result(&mut self, _link_field: &'static str, read: LinkRead) -> ProcessActions {
        match read {
            LinkRead::Value(v) if let Some(v) = v.to_f64() => {
                self.val = v;
                self.sts = THROTTLE_STS_SUC;
                self.posts |= POST_SYNC_VAL;
            }
            _ => self.sts = THROTTLE_STS_ERR,
        }
        self.sync = THROTTLE_SYNC_IDLE;
        self.posts |= POST_SYNC_STS | POST_SYNC;
        ProcessActions::new()
    }

    /// `dbGetLink(&prec->sinp, DBR_DOUBLE, ..)` (`throttleRecord.c:631`).
    fn input_link_request(&self, _link_field: &str) -> InputLinkRequest {
        InputLinkRequest::As(LinkReadAs::Double)
    }

    /// C `delayFuncCallback` (`throttleRecord.c:530-538`): `valuePut` alone,
    /// with no limit block, no `enterValue` and no OVAL update.
    fn delayed_value_put(&mut self) -> ProcessActions {
        self.out_written = false;
        let mut actions = Vec::new();
        self.value_put(&mut actions);
        actions.into()
    }

    /// `valuePut` posts by hand and stamps only ahead of its own reset
    /// (`throttleRecord.c:602`); `process()` posts WAIT/VAL/DRVLS itself.
    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::CONTEXT
            .union(CycleHooks::PROCESS_POSTED)
            .union(CycleHooks::HAND_POSTED_OWN_STAMP)
    }

    /// C runs `checkLink` only for an `Ext PV NC` SINP (throttleRecord.c:381).
    fn special_checks_links(&self, put_field: &str) -> bool {
        put_field == "SYNC" && self.sync != THROTTLE_SYNC_IDLE && self.link_status[1] == LINK_EXT_NC
    }

    fn link_check(&mut self) -> Option<LinkCheck<'_>> {
        Some(LinkCheck {
            spec: &THROTTLE_LINK_CHECK,
            statuses: &mut self.link_status,
            field_types: &mut [],
            tail: None,
            state: &mut self.link_check,
        })
    }

    fn take_cycle_posted_fields(&mut self) -> CycleMarks {
        CycleMarks::of(&POSTS, u128::from(std::mem::take(&mut self.posts)))
    }

    fn take_value_put_tail(&mut self) -> CycleMarks {
        if !std::mem::take(&mut self.tail_due) {
            return CycleMarks::NONE;
        }
        let moved = self.osent != self.sent;
        self.osent = self.sent;
        let sts = std::mem::take(&mut self.tail_sts) as u128;
        CycleMarks::of(&TAIL, sts | if moved { 1 << 1 } else { 1 << 2 })
    }

    fn take_process_posted_fields(&mut self) -> CycleMarks {
        CycleMarks::of(
            &PROCESS_POSTS,
            u128::from(std::mem::take(&mut self.process_posts)),
        )
    }

    /// C posts VAL, WAIT, SENT, STS and DRVLS from a process cycle, the last
    /// four only where it calls `db_post_events` on them.
    fn process_posted_fields(&self) -> Option<epics_base_rs::server::record::DeclaredFields> {
        Some(
            &const {
                epics_base_rs::server::record::declared_set(
                    dbd_generated::THROTTLE_FIELDS,
                    ["VAL", "WAIT", "SENT", "STS", "DRVLS"],
                )
            },
        )
    }

    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &["WAIT", "SENT", "STS", "DRVLS"]
    }

    /// C `checkAlarms` tests `udf == TRUE` (`throttleRecord.c:479`): a UDF
    /// of 2 written through an NPP link raises nothing at the next cooldown.
    fn udf_alarm_on_exact_one(&self) -> bool {
        true
    }

    /// C `process()`'s closing VAL post (`throttleRecord.c:296-302`).
    fn monitor_value_changed(&mut self) -> Option<bool> {
        Some(std::mem::take(&mut self.val_moved))
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
    ) -> ProcessActions {
        if link_field != "OUT" {
            return ProcessActions::new();
        }
        let mut actions = Vec::new();
        if failed {
            self.sts = THROTTLE_STS_ERR;
        } else {
            self.sts = THROTTLE_STS_SUC;
            if let Some(v) = value.to_f64() {
                self.sent = v;
            }
            self.posts |= POST_SENT;
            // C :569-570 — a SYNC deferred behind this value completes here,
            // and only from the successful arm: a put that failed leaves the
            // request standing for the next one.
            if self.sync_flag {
                self.value_sync(&mut actions);
            }
        }
        self.value_put_done();
        actions.into()
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        // C `init_record` (throttleRecord.c:133-228). Pass 0 copies the
        // VERSION string (`:51`) into VER over any `.db` value (line 149).
        //
        // Pass 1 (C lines 156-167): STS is reset to Unknown and VAL to
        // 0, and `limit_flag` is derived from `drvlh > drvll`. C also
        // resets the private delay/wait/sync flags to 0 — mirrored by
        // the runtime-state fields below.
        if pass == 0 {
            self.ver = "0-2-1".into();
        }
        if pass == 1 {
            self.sts = 0; // throttleSTS_UNK
            self.val = 0.0;
            self.limit_flag = self.drvlh > self.drvll;
            self.delay_active = false;
            self.pending_value = None;
            self.sync_flag = false;
            self.special_actions.clear();
            self.out_written = false;
        }
        Ok(())
    }

    fn set_process_context(&mut self, ctx: &epics_base_rs::server::record::ProcessContext) {
        // A `PRIO` written between cycles moves the next one, as in C where
        // `callbackSetPriority` is re-run inside `process()`.
        self.callback_priority = ctx.callback_priority;
    }

    fn set_async_context(&mut self, name: epics_base_rs::types::RecordName, db: AsyncDbHandle) {
        self.async_ctx = Some((name, db));
        // C `init_record` classifies the OUT/SINP links into OV/SIV and
        // posts the initial status (throttleRecord.c:171-205). This is the
        // framework's init-time async hook (the handle now exists), so
        // classify here — the record's OUT/SINP db fields are already loaded.
        self.refresh_link_status(0b11, THROTTLE_LINK_CHECK.rule.init_recheck());
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
                self.link_status[0] = *v;
                Ok(())
            }
            ("SIV", EpicsValue::Short(v)) => {
                self.link_status[1] = *v;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_dly_names_the_value_as_c_percent_g() {
        let text = |v| match validate_dly(v) {
            Err(CaError::InvalidValue(msg)) => msg,
            other => panic!("{other:?}"),
        };
        assert_eq!(text(f64::NAN), "throttle DLY must be finite, got nan");
        assert_eq!(
            text(1e20),
            "throttle DLY must not exceed 86400 seconds, got 1e+20"
        );
    }
}
