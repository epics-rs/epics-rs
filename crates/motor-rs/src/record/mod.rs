mod command_planner;
pub mod dbd_generated;
mod field_access;
mod state_machine;
mod status_update;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::record::{
    FieldDesc, ParsedLink, ProcessAction, ProcessOutcome, Record, RecordProcessResult,
    parse_link_v2,
};
use epics_base_rs::types::EpicsValue;

use crate::coordinate;
use crate::device_state::*;
use crate::fields::*;
use crate::flags::*;

/// EPICS Motor Record implementation.
#[derive(Debug, Clone)]
pub struct MotorRecord {
    pub pos: PositionFields,
    pub conv: ConversionFields,
    pub vel: VelocityFields,
    pub retry: RetryFields,
    pub limits: LimitFields,
    pub ctrl: ControlFields,
    pub stat: StatusFields,
    pub pid: PidFields,
    pub disp: DisplayFields,
    pub timing: TimingFields,
    pub internal: InternalFields,
    /// Database-link / menu fields imported from the public C motorRecord.dbd
    /// surface (OUT/RDBL/DOL/OMSL/RLNK/STOO/DINP/RINP/POST).
    pub links: LinkFields,
    /// Alarm-limit fields imported from the public C motorRecord.dbd surface
    /// (HIHI/HIGH/LOW/LOLO/HHSV/LLSV).
    pub alarm: AlarmFields,
    /// Pending event for next process() call
    pending_event: Option<MotorEvent>,
    /// Track which field was last written (for process)
    last_write: Option<CommandSource>,
    /// One-shot request from a runtime JVEL put: re-emit the jog command
    /// with the new JVEL/JAR if a jog is active when the put's process
    /// pass runs (C special() motorRecordJVEL, motorRecord.cc:3059-3072).
    jog_retune_pending: bool,
    /// Shared state mailbox for device communication
    device_state: Option<SharedDeviceState>,
    /// Last seen status sequence number
    last_seen_seq: u64,
    /// Whether initial readback has been performed
    initialized: bool,
    /// Monotonic counter for delay request IDs
    next_delay_id: u64,
}

impl Default for MotorRecord {
    fn default() -> Self {
        Self {
            pos: PositionFields::default(),
            conv: ConversionFields::default(),
            vel: VelocityFields::default(),
            retry: RetryFields::default(),
            limits: LimitFields::default(),
            ctrl: ControlFields::default(),
            stat: StatusFields::default(),
            pid: PidFields::default(),
            disp: DisplayFields::default(),
            timing: TimingFields::default(),
            internal: InternalFields::default(),
            links: LinkFields::default(),
            alarm: AlarmFields::default(),
            pending_event: None,
            last_write: None,
            jog_retune_pending: false,
            device_state: None,
            last_seen_seq: 0,
            initialized: false,
            next_delay_id: 0,
        }
    }
}

/// Motion direction for hardware limit checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MotionDirection {
    Positive,
    Negative,
}

impl MotorRecord {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a motor record wired to a shared device state mailbox.
    pub fn with_device_state(mut self, state: SharedDeviceState) -> Self {
        self.device_state = Some(state);
        self
    }

    /// Set the shared device state (for late injection by device support init).
    pub fn set_device_state(&mut self, state: SharedDeviceState) {
        self.device_state = Some(state);
    }

    /// Set a pending event for the next process() call.
    pub fn set_event(&mut self, event: MotorEvent) {
        self.pending_event = Some(event);
    }

    /// Clear any pending write command source.
    ///
    /// Called by device support init() so that pass0-restored field values
    /// are not interpreted as move commands during PINI processing.
    pub fn clear_last_write(&mut self) {
        self.last_write = None;
    }

    /// C do_work collection gate (motorRecord.cc:1994): `omsl ==
    /// menuOmslclosed_loop && dol.type == DB_LINK`. While it holds, the
    /// entire button/tweak/relative/raw collection block (2008-2198) is
    /// bypassed and VAL arrives only through the DOL link; a constant or
    /// CA DOL leaves the collection active even under closed loop.
    pub(crate) fn closed_loop_dol_collection(&self) -> bool {
        self.links.omsl == 1 && matches!(parse_link_v2(&self.links.dol), ParsedLink::Db(_))
    }

    /// True when a position field (VAL/DVAL/RVAL/RLV) was written during
    /// pass0 — i.e. autosave restored a saved position.
    ///
    /// Device support `init()` uses this to decide whether to reseed the
    /// controller with the restored DVAL. It MUST be queried before
    /// [`clear_last_write`](Self::clear_last_write), which device support
    /// calls later in `init()`.
    ///
    /// This is the correct "was a position restored" signal: a genuine
    /// restored position of exactly `0.0` is indistinguishable from the
    /// field default if you only inspect the DVAL value, but the pass0
    /// write still records `last_write`.
    pub fn was_position_restored(&self) -> bool {
        matches!(
            self.last_write,
            Some(
                CommandSource::Val | CommandSource::Dval | CommandSource::Rval | CommandSource::Rlv
            )
        )
    }

    /// Signal that the external URIP readback link is in error or recovered.
    /// While `urip` is true and `error` is set, new motions are refused and
    /// in-progress motion is stopped (C: `db5da2f0`, `7493d50b`).
    pub fn set_rdbl_error(&mut self, error: bool) {
        self.conv.rdbl_error = error;
    }

    /// Fire the readback output link (RLNK) with the current RBV.
    ///
    /// C `motorRecord.cc:1495` calls `dbPutLink(&rlnk, DBR_DOUBLE, &rbv, 1)`
    /// unconditionally just before `process_exit` on every process pass, so a
    /// record wired through RLNK re-processes whenever the motor record does.
    /// We mirror that by emitting a `WriteDbLink` on every process cycle,
    /// including the move-start `AsyncPendingNotify` (DMOV 1→0) pass: C fires
    /// `dbPutLink` before `monitor()` (motorRecord.cc:1507) and before the
    /// `recGblFwdLink` gate (motorRecord.cc:1509), so it runs even while the
    /// move is still pending. A `dbPutLink` processes a PP readback target even
    /// when RBV is unchanged, so skipping the pending pass would drop one of
    /// the target's process cycles relative to C. The framework runs the
    /// `WriteDbLink` before the deferred FLNK, exactly where C fires it. The
    /// empty link is skipped so motors with no RLNK emit nothing.
    fn push_readback_link_write(&self, actions: &mut Vec<ProcessAction>) {
        if !self.links.rlnk.is_empty() {
            actions.push(ProcessAction::WriteDbLink {
                link_field: "RLNK",
                value: EpicsValue::Double(self.pos.rbv),
            });
        }
    }
}

impl Record for MotorRecord {
    fn record_type(&self) -> &'static str {
        "motor"
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    fn can_device_write(&self) -> bool {
        true
    }

    fn is_put_complete(&self) -> bool {
        self.stat.dmov
    }

    // C RSET per-field metadata (motorRecord.cc get_units 3156-3208,
    // get_precision 3313-3337, get_graphic_double 3213-3258,
    // get_control_double 3263-3308, get_alarm_double 3344-3361).
    fn field_metadata_override(
        &self,
        field: &str,
    ) -> Option<epics_base_rs::server::record::FieldMetadataOverride> {
        Some(field_access::metadata_override(self, field))
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // Clear the one-pass DIFF/RDIF mark before any `process_motor_info`
        // can set it this cycle (it runs from `determine_event` below and
        // from `do_process`). The post-process snapshot reads it through
        // `force_posted_fields`; a pass that does not recompute DIFF/RDIF
        // (no CALLBACK_DATA) leaves it false and does not force-post.
        self.internal.diff_rdif_marked = false;
        // If wired to device state, determine event from shared mailbox.
        // An event that survived a put-owned pass is a still-pending
        // signal: this pass consumes it before pulling a new one from
        // the mailbox (one signal, one pass — never overwrite).
        if self.pending_event.is_none() && self.device_state.is_some() {
            if let Some(event) = self.determine_event() {
                self.pending_event = Some(event);
            }
        }

        let effects = self.do_process();
        // DMOV=0 means a move started (or sub-step pulse).
        // Flush DMOV=0 even if no commands were emitted (sub-step case).
        let move_started = !self.stat.dmov;

        // Write effects to shared mailbox for DeviceSupport.write() to consume.
        // If a previous batch has not been consumed yet (two process() cycles
        // without an intervening write()), fold the new batch into it rather
        // than overwriting — otherwise the earlier move command is lost.
        if let Some(state) = self.device_state.clone() {
            let actions = self.effects_to_actions(&effects);
            match state.lock() {
                Ok(mut ds) => match ds.pending_actions.take() {
                    Some(mut prev) => {
                        tracing::warn!("motor: pending_actions not yet consumed — merging batches");
                        prev.merge_newer(actions);
                        ds.pending_actions = Some(prev);
                    }
                    None => ds.pending_actions = Some(actions),
                },
                Err(e) => {
                    tracing::error!("device state lock poisoned in process: {e}");
                }
            }
        }

        if move_started && !self.internal.dmov_notified {
            // First DMOV 1→0 transition: flush immediately so monitors see
            // the transition before the move completes.
            self.internal.dmov_notified = true;
            use epics_base_rs::types::EpicsValue;
            let fields = vec![
                ("DMOV".to_string(), EpicsValue::Short(0)),
                ("MOVN".to_string(), EpicsValue::Short(1)),
                ("VAL".to_string(), EpicsValue::Double(self.pos.val)),
                ("DVAL".to_string(), EpicsValue::Double(self.pos.dval)),
                ("RVAL".to_string(), EpicsValue::Int64(self.pos.rval)),
                ("RBV".to_string(), EpicsValue::Double(self.pos.rbv)),
                ("DRBV".to_string(), EpicsValue::Double(self.pos.drbv)),
                // C do_work move block (motorRecord.cc:2248-2256) recomputes
                // diff=dval-drbv + MARK(M_DIFF) and rdif=NINT(diff/mres) +
                // MARK(M_RDIF) before too_small/LVIO, so monitor() posts the
                // new full-distance following error on the move-start pass.
                // `plan_absolute_move` already refreshed pos.diff/pos.rdif.
                ("DIFF".to_string(), EpicsValue::Double(self.pos.diff)),
                ("RDIF".to_string(), EpicsValue::Int64(self.pos.rdif)),
                // C do_work marks M_MIP on every move dispatch and
                // M_RCNT when the retry count resets (motorRecord.cc:
                // 1929-1932), so the move-start monitor() pass posts
                // both.
                (
                    "MIP".to_string(),
                    EpicsValue::Short(self.stat.mip.bits() as i16),
                ),
                ("RCNT".to_string(), EpicsValue::Short(self.retry.rcnt)),
            ];
            // C fires `dbPutLink(&rlnk, ...)` (motorRecord.cc:1495) on this
            // move-start pass too — before `monitor()` and the `recGblFwdLink`
            // gate — so the RLNK readback target still re-processes while the
            // move is pending.
            let mut actions = Vec::new();
            self.push_readback_link_write(&mut actions);
            Ok(ProcessOutcome {
                result: RecordProcessResult::AsyncPendingNotify(fields),
                actions,
                device_did_compute: false,
            })
        } else {
            // Ongoing motion or idle: full snapshot so all changed fields
            // (RBV, DRBV, MSTA, limits, etc.) get posted as monitors.
            if !move_started {
                self.internal.dmov_notified = false;
            }
            let mut outcome = ProcessOutcome::complete();
            self.push_readback_link_write(&mut outcome.actions);
            Ok(outcome)
        }
    }

    /// C process_exit (motorRecord.cc:1509-1510): `if (dmov != 0)
    /// recGblFwdLink(pmr)` — FLNK fires on EVERY pass that exits with
    /// DMOV true, and on nothing else. (`0ef39053` gated this on the
    /// DMOV false→true transition; `c970afbf` reverted it: "Except for
    /// the condition that DMOV == FALSE, FLNK processing was
    /// standard.") Reading DMOV directly at FLNK time IS the C gate by
    /// construction — there is no per-path suppression state to track.
    fn should_fire_forward_link(&self) -> bool {
        self.stat.dmov
    }

    /// Input links the framework must resolve via `dbGetLink` BEFORE each
    /// `process()`, so their values are available to the move computation —
    /// exactly the synchronous reads C `motorRecord.cc` performs inside
    /// `do_work`/`process_motor_info`.
    ///
    /// - CLOSED_LOOP DOL → VAL (motorRecord.cc:1994), gated below.
    /// - URIP=Yes RDBL → RRBV: C `process_motor_info` (motorRecord.cc:3687)
    ///   does `dbGetLink(&pmr->rdbl, DBR_DOUBLE, &rdblvalue, ...)` and scales it
    ///   into RRBV (`rrbv = NINT(rdblvalue*rres/mres)`). The scaling lives in
    ///   the status-update path; this just feeds it the raw link value through
    ///   the `RDBL_VAL` carrier. C reads with `dbGetLink` regardless of link
    ///   type, so any RDBL link form is read (the framework skips an empty
    ///   link).
    fn pre_process_actions(&mut self) -> Vec<ProcessAction> {
        let mut actions = Vec::new();

        // CLOSED_LOOP: drive VAL from the DOL link (motorRecord.cc:1994-1999).
        // C reads DOL into .val only when `omsl == closed_loop && dol.type ==
        // DB_LINK`; a constant DOL is initialised once (recGblInitConstantLink,
        // :680) and never re-read, so gate on the link parsing as a local DB
        // link. Writing VAL records a VAL command-source, so the following
        // process() plans the move to the link value.
        //
        // Read only when DMOV (done): C calls do_work — the function holding
        // the DOL read — for `pmr->dmov` or a non-callback process, but NOT on
        // a CALLBACK_DATA poll mid-move (motorRecord.cc:1487-1492). Gating on
        // DMOV mirrors that dominant condition and stops a device poll from
        // re-reading DOL and clobbering the in-flight target every cycle.
        if self.stat.dmov && self.closed_loop_dol_collection() {
            actions.push(ProcessAction::ReadDbLink {
                link_field: "DOL",
                target_field: "VAL",
            });
        }

        // C process_motor_info (3676-3699) is an else-if chain: UEIP=Yes
        // takes the encoder and the RDBL dbGetLink never executes, so a
        // failed RDBL must not stop the axis while the encoder is in use.
        if self.conv.urip && !self.conv.ueip {
            actions.push(ProcessAction::ReadDbLink {
                link_field: "RDBL",
                target_field: "RDBL_VAL",
            });
        }
        actions
    }

    /// Framework report of which requested link reads produced a value
    /// this cycle — the analogue of C inspecting
    /// `RTN_SUCCESS(dbGetLink(&pmr->rdbl, ...))` in `process_motor_info`
    /// (motorRecord.cc:3687-3698). A failed RDBL read latches
    /// `rdbl_error`; the completion/planner paths stop an in-flight
    /// motion and refuse new ones while it is set. An empty RDBL is
    /// skipped by the framework (CONSTANT link in C — a successful
    /// dbGetLink), so it never reports as an error here.
    fn set_resolved_input_links(&mut self, resolved: &[&'static str]) {
        // C closed-loop DOL collection (motorRecord.cc:1994-2005): a
        // failed dbGetLink on DOL sets `udf = TRUE` and aborts the pass
        // (return ERROR — the motion side is inert anyway because the
        // failed read delivers no VAL); a successful read clears it.
        // The outcome is latched here — gated exactly like the
        // pre_process_actions request — and applied to the framework's
        // CommonFields.udf in check_alarms (the C alarm_sub consumer).
        if self.stat.dmov && self.closed_loop_dol_collection() {
            self.internal.dol_udf = Some(!resolved.contains(&"DOL"));
        }
        if self.conv.urip && !self.conv.ueip && !self.links.rdbl.is_empty() {
            self.conv.rdbl_error = !resolved.contains(&"RDBL");
        }
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        field_access::motor_get_field(self, name)
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        field_access::motor_put_field(self, name, value)
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        dbd_generated::MOTOR_FIELDS
    }

    /// C `special()` pass-0 blink (motorRecord.cc:2582-2608): "Someone
    /// wrote to drive field. Blink .dmov unless record is disabled."
    /// Every database put to a drive field drops DMOV before the record
    /// processes — that blink is what carries the put pass through the
    /// do_work move-block entry gate (2240, `dval != ldvl || !dmov`)
    /// even when the target equals the last-dispatched one; the move
    /// block then decides between the sub-step pulse restore
    /// (too_small, 2333-2342), a re-dispatch (2455, `mip == MIP_DONE ||
    /// MIP_RETRY`), and the SET-mode load_pos. The entry gate itself
    /// only ever refuses passes that did NOT come through database put
    /// access — C's closed-loop DOL collection is a bare `dbGetLink`
    /// into VAL (1994) with no special, mirrored here by the framework
    /// link reads landing via `put_field_internal`, which never runs
    /// this hook.
    ///
    /// C's disabled-record gate (2600-2601): the DISP half is enforced
    /// upstream — the framework rejects the put before this hook runs
    /// (`check_put_disabled`, C dbPutField's put-disable test), so no
    /// blink can land on a DISP'd record. The DISA==DISV half is not
    /// reachable from the record trait (DISA/DISV are framework common
    /// fields); a drive put to a processing-disabled motor blinks DMOV
    /// low and it stays low until the record next processes, where C
    /// keeps DMOV=1. Known divergence.
    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if after {
            return Ok(());
        }
        if ["VAL", "DVAL", "RVAL", "RLV", "TWF", "TWR"]
            .iter()
            .any(|f| field.eq_ignore_ascii_case(f))
        {
            self.stat.dmov = false;
        }
        Ok(())
    }

    /// C `init_record`: on pass 1, once all `field()` values have been
    /// applied, reconcile the rev↔EGU speed pairs (C
    /// `check_speed_and_resolution`) and establish the limit invariant from
    /// the loaded DHLM/DLLM (C `set_dial_highlimit`/`set_dial_lowlimit`),
    /// then enable the runtime cascade semantics in `put_field`. See
    /// `field_access::motor_sync_speed_at_init` and
    /// `field_access::motor_sync_limits_at_init`.
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 1 {
            // C init_record runs check_speed_and_resolution (641) with the
            // resolution triple first (3904-3927), then the speed pairs,
            // then the raw-limit rules.
            field_access::motor_sync_resolution_at_init(self);
            field_access::motor_sync_speed_at_init(self);
            field_access::motor_sync_limits_at_init(self);
            // C 642: the single load-order-independent enforcement of
            // RDBD >= |MRES|, against the reconciled resolution. The RDBD
            // put handler stays inert during load.
            self.enforce_min_retry_deadband();
            // C 692-696: a zero ERES (unset, or loaded as 0) is seeded
            // from the reconciled MRES.
            if self.conv.eres == 0.0 {
                self.conv.eres = self.conv.mres;
            }
            // C 721-723: `dmov = TRUE; movn = FALSE` — DMOV starts done
            // no matter what the load wrote. In particular this
            // neutralizes any `special()` pass-0 blink a load-time /
            // restore write to a drive field may have left behind.
            self.stat.dmov = true;
            self.stat.movn = false;
            // C 734-743: LVIO starts clear; with soft limits enabled
            // (the dial pair not both 0), an initial dial readback
            // outside the dial window by more than one MRES, or an
            // inverted pair, raises it. The slop is the SIGNED
            // resolution (C verbatim — a negative MRES narrows the
            // window).
            self.limits.lvio = false;
            let limits_disabled = self.limits.dhlm == self.limits.dllm && self.limits.dllm == 0.0;
            if !limits_disabled
                && (self.pos.drbv > self.limits.dhlm + self.conv.mres
                    || self.pos.drbv < self.limits.dllm - self.conv.mres
                    || self.limits.dllm > self.limits.dhlm)
            {
                self.limits.lvio = true;
            }
            self.internal.init_invariants_synced = true;
        }
        Ok(())
    }

    fn primary_field(&self) -> &'static str {
        "VAL"
    }

    /// MDEL/ADEL monitor deadband applies to the readback (RBV), not the
    /// VAL setpoint. C `monitor()` gates RBV value/archive monitors on
    /// MDEL/ADEL; VAL is a setpoint that only changes on a move command.
    fn monitor_deadband_value(&self) -> Option<EpicsValue> {
        Some(EpicsValue::Double(self.pos.rbv))
    }

    /// The deadband gates RBV's monitor delivery (C motorRecord.cc
    /// 3468-3507); with this, the framework routes VAL through generic
    /// change-detection — posted only when the setpoint actually moved
    /// (C M_VAL mark semantics), not on every readback poll.
    fn monitor_deadband_field(&self) -> &'static str {
        "RBV"
    }

    /// C `monitor()` posts every field in its list on a cycle whose
    /// alarm transition fired, even when unmarked — `local_mask =
    /// monitor_mask | (MARKED(x) ? DBE_VAL_LOG : 0)` is non-zero for
    /// unmarked fields once `monitor_mask != 0` (motorRecord.cc
    /// 3513-3645), so a `DBE_ALARM`-only subscriber on any of them
    /// observes the alarm moment. The list is C's posting order, minus
    /// RBV (the deadband-gated field, delivered by the deadband
    /// trigger with the same alarm bits) and CNEN (C posts it only on
    /// an actual EA_POSITION-driven change, which generic
    /// change-detection covers). Both raw limit-switch mirrors RHLS
    /// and RLLS are listed: C's M_HLS and M_LLS branches each post one
    /// of them, and on an alarm cycle both branches run.
    fn alarm_cycle_monitored_fields(&self) -> &'static [&'static str] {
        &[
            "RRBV", "DRBV", "DIFF", "RDIF", "MSTA", "VAL", "DVAL", "RVAL", "TDIR", "MIP", "HLM",
            "LLM", "SPMG", "RCNT", "RLV", "OFF", "DHLM", "DLLM", "HLS", "RHLS", "RLLS", "LLS",
            "ATHM", "MRES", "ERES", "UEIP", "LVIO", "STOP", "SBAS", "SREV", "UREV", "VELO", "VBAS",
            "MISS", "MOVN", "DMOV", "STUP", "JOGF", "JOGR", "HOMF", "HOMR", "RHLM", "RLLM",
        ]
    }

    /// C `process_motor_info` (motorRecord.cc:3764-3767) MARKs M_DIFF /
    /// M_RDIF unconditionally every CALLBACK_DATA pass, and `monitor()`
    /// (3522-3531) posts both with `monitor_mask | DBE_VAL_LOG`. So on a
    /// cycle that ran `process_motor_info`, DIFF and RDIF re-post even when
    /// their value did not change — a `camonitor DIFF` on an axis parked at
    /// a constant non-zero following error gets an event each poll. Gated on
    /// the one-pass `diff_rdif_marked` mark so a non-CALLBACK_DATA pass (a
    /// put that ran `do_work`, not `process_motor_info`) does not over-post.
    fn force_posted_fields(&self) -> &'static [&'static str] {
        if self.internal.diff_rdif_marked {
            &["DIFF", "RDIF"]
        } else {
            &[]
        }
    }

    /// C `special()` STUP after-write (motorRecord.cc:3084-3090): a STUP put
    /// of any value other than ON is clamped to OFF and returns ERROR, which
    /// suppresses the pp(TRUE) reprocess — only a `STUP == ON` put runs the
    /// status-update process. `field_access` has already clamped `stup` to 0
    /// for any non-ON put by the time this gate runs, so the post-clamp value
    /// is the deterministic discriminator. Every other pp(TRUE) field
    /// reprocesses normally (default membership test).
    fn processes_after_put(&self, field: &str) -> bool {
        if field.eq_ignore_ascii_case("STUP") {
            return self.stat.stup == 1;
        }
        self.process_passive_fields()
            .iter()
            .any(|f| f.eq_ignore_ascii_case(field))
    }

    /// C never derives the motor's UDF from VAL. `motorRecord.cc` touches
    /// `udf` in exactly three places: init_record (677-681, CONSTANT .dol →
    /// FALSE), the closed-loop DOL collection (1994-2005, DB_LINK read
    /// fail → TRUE / success → FALSE), and alarm_sub (3372 consumes it).
    /// The framework's default `clears_udf()` would instead recompute
    /// `udf = value_is_undefined()` (VAL.is_nan()) every process pass — a
    /// divergence that both fabricates UDF on a transient NaN VAL and, on a
    /// no-DOL-read pass, clobbers the legitimate closed-loop DOL outcome.
    /// Opt out so motor UDF is owned solely by the DOL channel
    /// (`dol_udf` → check_alarms) and the init clear in `initial_readback`.
    fn clears_udf(&self) -> bool {
        false
    }

    /// C `alarm_sub()` (motorRecord.cc:3367-3406) — motor-specific alarm
    /// severities, raised into `nsta`/`nsev` before the framework's
    /// `evaluate_alarms`.
    fn check_alarms(&mut self, common: &mut epics_base_rs::server::record::CommonFields) {
        use epics_base_rs::server::recgbl::{alarm_status, rec_gbl_set_sevr};
        use epics_base_rs::server::record::AlarmSeverity;

        // C closed-loop DOL (1999-2005): the DOL read outcome drives
        // UDF — a failed dbGetLink marks VAL undefined, a successful
        // one clears it. Applied here because this hook is where the
        // record sees CommonFields; a pass without a DOL read leaves
        // udf as-is, like C falling through the collection block.
        if let Some(undefined) = self.internal.dol_udf.take() {
            common.udf = undefined as u8;
        }

        // C 3372-3376: an undefined VAL short-circuits every other check.
        if common.udf != 0 {
            rec_gbl_set_sevr(common, alarm_status::UDF_ALARM, AlarmSeverity::Invalid);
            return;
        }

        // C 3379-3388: limit-switch and soft-limit violations. BOTH the
        // high and low arms gate on HLSV and raise at HLSV — motorRecord
        // has no low-side severity field. Limit switches count even when
        // not in the direction of travel (externally triggered move),
        // and a DVAL outside the dial soft limits alarms too.
        let hlsv = AlarmSeverity::from_u16(self.limits.hlsv as u16);
        if hlsv != AlarmSeverity::NoAlarm {
            if self.limits.hls || self.pos.dval > self.limits.dhlm {
                rec_gbl_set_sevr(common, alarm_status::HIGH_ALARM, hlsv);
                return;
            }
            if self.limits.lls || self.pos.dval < self.limits.dllm {
                rec_gbl_set_sevr(common, alarm_status::LOW_ALARM, hlsv);
                return;
            }
        }

        // C 3392-3398: controller communication error — COMM/INVALID,
        // and the MSTA bit is CLEARED (C `msta.Bits.CNTRL_COMM_ERR = 0`
        // + `MARK(M_MSTA)`) so the alarm is one-shot until the driver
        // re-reports it. The framework builds the monitor snapshot after
        // this hook, so the cleared MSTA posts in the same cycle.
        if self.stat.msta.contains(MstaFlags::COMM_ERR) {
            self.stat.msta.remove(MstaFlags::COMM_ERR);
            rec_gbl_set_sevr(common, alarm_status::COMM_ALARM, AlarmSeverity::Invalid);
        }

        // C 3400-3403: slip/stall or controller problem → STATE/MAJOR.
        // No early return above COMM — C falls through from the comm
        // check, so both can accumulate (set_sevr keeps the higher).
        if self
            .stat
            .msta
            .intersects(MstaFlags::SLIP_STALL | MstaFlags::PROBLEM)
        {
            rec_gbl_set_sevr(common, alarm_status::STATE_ALARM, AlarmSeverity::Major);
        }
    }
}

#[cfg(test)]
mod tests {
    use epics_base_rs::server::record::FieldDeclaration;

    use super::*;

    #[test]
    fn test_set_mode_updates_offset() {
        let mut rec = MotorRecord::new();
        rec.conv.mres = 0.01;
        rec.pos.dval = 5.0;
        rec.conv.set = true;
        rec.put_field("VAL", EpicsValue::Double(100.0)).unwrap();
        // Offset should be updated, DVAL unchanged
        assert_eq!(rec.pos.dval, 5.0);
        assert_eq!(rec.pos.off, 95.0); // 100 - 1*5
        // C 2206-2227: the FOFF=Variable redefinition completes on the
        // spot — no controller command is dispatched (LOAD_POS belongs
        // to the Frozen/DVAL/RVAL paths) and DMOV stays TRUE.
        assert_eq!(rec.last_write, None);
        assert!(rec.stat.dmov);
        assert!(rec.stat.mip.is_empty());
    }

    #[test]
    fn test_should_fire_forward_link() {
        let mut rec = MotorRecord::new();
        assert!(rec.should_fire_forward_link());

        // C motorRecord.cc:1509-1510: DMOV is the only FLNK gate.
        rec.stat.dmov = false;
        assert!(!rec.should_fire_forward_link());
    }

    // C special() STUP after-write (motorRecord.cc:3084-3090): only a
    // STUP==ON put runs the pp(TRUE) reprocess; any other value is clamped to
    // OFF and C returns ERROR so the record does not process. R24. Fresh
    // records per case so the STUP-in-flight before-write veto (stup != OFF)
    // does not reject the second put.
    #[test]
    fn test_stup_processes_after_put_only_when_on() {
        // STUP == ON -> reprocess.
        let mut on = MotorRecord::new();
        on.put_field("STUP", EpicsValue::Short(1)).unwrap();
        assert_eq!(on.stat.stup, 1);
        assert!(on.processes_after_put("STUP"));

        // STUP == BUSY -> clamped to OFF, no reprocess (C return ERROR).
        let mut off = MotorRecord::new();
        off.put_field("STUP", EpicsValue::Short(2)).unwrap();
        assert_eq!(off.stat.stup, 0);
        assert!(!off.processes_after_put("STUP"));

        // A non-pp(TRUE) config field never reprocesses (default membership).
        assert!(!off.processes_after_put("VELO"));
    }

    // C motorRecord.cc:1495 — every process pass fires the RLNK readback
    // output link with the current RBV via dbPutLink().
    #[test]
    fn rlnk_readback_link_fired_with_rbv_each_process() {
        let mut rec = MotorRecord::new();
        rec.links.rlnk = "readback_sink.PROC".to_string();
        rec.pos.rbv = 12.5;
        let outcome = rec.process().unwrap();
        let rbv = rec.pos.rbv;
        assert!(
            outcome.actions.iter().any(|a| matches!(
                a,
                ProcessAction::WriteDbLink {
                    link_field: "RLNK",
                    value: EpicsValue::Double(v),
                } if *v == rbv
            )),
            "process() must emit a WriteDbLink to RLNK carrying RBV"
        );
    }

    // C motorRecord.cc:1495 fires `dbPutLink(&rlnk, ...)` on the move-start
    // pass too — before `monitor()` (motorRecord.cc:1507) and the
    // `recGblFwdLink` gate (motorRecord.cc:1509) — so the RLNK readback
    // target re-processes even while the move is still pending. The framework
    // executes the emitted WriteDbLink on the AsyncPendingNotify branch.
    #[test]
    fn rlnk_readback_link_fired_on_move_start_async_pending() {
        let mut rec = MotorRecord::new();
        rec.links.rlnk = "readback_sink.PROC".to_string();
        // Enter a move: DMOV goes true→false. The first process() of a fresh
        // move returns the intermediate DMOV=0 notification.
        rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
        rec.set_event(MotorEvent::UserWrite(CommandSource::Val));
        let outcome = rec.process().unwrap();
        assert!(
            matches!(outcome.result, RecordProcessResult::AsyncPendingNotify(_)),
            "first process() of a fresh move must be the move-start notification"
        );
        let rbv = rec.pos.rbv;
        assert!(
            outcome.actions.iter().any(|a| matches!(
                a,
                ProcessAction::WriteDbLink {
                    link_field: "RLNK",
                    value: EpicsValue::Double(v),
                } if *v == rbv
            )),
            "the move-start AsyncPendingNotify cycle must still emit the RLNK readback write"
        );
    }

    #[test]
    fn rlnk_not_fired_when_link_unset() {
        let mut rec = MotorRecord::new();
        assert!(rec.links.rlnk.is_empty());
        let outcome = rec.process().unwrap();
        assert!(
            !outcome.actions.iter().any(|a| matches!(
                a,
                ProcessAction::WriteDbLink {
                    link_field: "RLNK",
                    ..
                }
            )),
            "an unset RLNK must emit no readback link write"
        );
    }

    // C do_work marks M_MIP on the move dispatch and M_RCNT on the retry
    // counter reset (motorRecord.cc:1929-1932); monitor() posts both on
    // the move-start pass.
    #[test]
    fn move_start_notify_carries_mip_and_rcnt() {
        let mut rec = MotorRecord::new();
        rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
        rec.set_event(MotorEvent::UserWrite(CommandSource::Val));
        let outcome = rec.process().unwrap();
        let RecordProcessResult::AsyncPendingNotify(fields) = outcome.result else {
            panic!("first process() of a fresh move must be the move-start notification");
        };
        let mip = rec.stat.mip.bits() as i16;
        assert!(
            fields
                .iter()
                .any(|(n, v)| n == "MIP" && *v == EpicsValue::Short(mip)),
            "move-start notify must carry MIP: {fields:?}"
        );
        assert!(
            fields
                .iter()
                .any(|(n, v)| n == "RCNT" && *v == EpicsValue::Short(rec.retry.rcnt)),
            "move-start notify must carry RCNT: {fields:?}"
        );
    }

    // C monitor() (motorRecord.cc:3468-3507): the MDEL/ADEL deadband
    // gates the RBV post. The framework keys delivery off this hook
    // pair — both must track RBV.
    #[test]
    fn monitor_deadband_hooks_track_rbv() {
        let mut rec = MotorRecord::new();
        rec.pos.rbv = 7.25;
        assert_eq!(rec.monitor_deadband_field(), "RBV");
        assert_eq!(rec.monitor_deadband_value(), Some(EpicsValue::Double(7.25)));
    }

    // C motorRecord.cc:3687 — URIP=Yes pulls the readback from the RDBL link
    // before process(); the status path then scales it into RRBV.
    #[test]
    fn urip_pulls_rdbl_link_before_process() {
        let mut rec = MotorRecord::new();
        rec.conv.urip = true;
        rec.links.rdbl = "ext_readback.RBV".to_string();
        let actions = rec.pre_process_actions();
        assert!(
            actions.iter().any(|a| matches!(
                a,
                ProcessAction::ReadDbLink {
                    link_field: "RDBL",
                    target_field: "RDBL_VAL",
                }
            )),
            "URIP=Yes must request a pre-process read of RDBL into RDBL_VAL"
        );
    }

    #[test]
    fn no_rdbl_pull_when_urip_off() {
        let mut rec = MotorRecord::new();
        // Link configured, but URIP=No — C reads RRBV from the controller.
        rec.conv.urip = false;
        rec.links.rdbl = "ext_readback.RBV".to_string();
        let actions = rec.pre_process_actions();
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                ProcessAction::ReadDbLink {
                    link_field: "RDBL",
                    ..
                }
            )),
            "URIP=No must not read the RDBL link"
        );
    }

    #[test]
    fn no_rdbl_pull_when_ueip_wins() {
        // C process_motor_info (3676-3699) else-if: UEIP=Yes takes the
        // encoder and the RDBL dbGetLink never executes.
        let mut rec = MotorRecord::new();
        rec.conv.urip = true;
        rec.conv.ueip = true;
        rec.links.rdbl = "ext_readback.RBV".to_string();
        let actions = rec.pre_process_actions();
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                ProcessAction::ReadDbLink {
                    link_field: "RDBL",
                    ..
                }
            )),
            "UEIP=Yes must suppress the RDBL read"
        );
    }

    fn has_dol_read(actions: &[ProcessAction]) -> bool {
        actions.iter().any(|a| {
            matches!(
                a,
                ProcessAction::ReadDbLink {
                    link_field: "DOL",
                    target_field: "VAL",
                }
            )
        })
    }

    // C motorRecord.cc:1994 — in CLOSED_LOOP mode VAL is driven by the DOL DB
    // link each idle pass.
    #[test]
    fn closed_loop_db_dol_drives_val() {
        let mut rec = MotorRecord::new();
        rec.links.omsl = 1; // menuOmsl: closed_loop
        rec.links.dol = "setpoint_src.VAL".to_string(); // resolves to a DB link
        assert!(rec.stat.dmov, "record starts done");
        assert!(
            has_dol_read(&rec.pre_process_actions()),
            "CLOSED_LOOP with a DB DOL must read DOL into VAL"
        );
    }

    #[test]
    fn supervisory_omsl_ignores_dol() {
        let mut rec = MotorRecord::new();
        rec.links.omsl = 0; // supervisory
        rec.links.dol = "setpoint_src.VAL".to_string();
        assert!(
            !has_dol_read(&rec.pre_process_actions()),
            "supervisory OMSL must not read DOL"
        );
    }

    // C closed-loop DOL collection (motorRecord.cc:1999-2005): a failed
    // dbGetLink sets udf = TRUE (UDF/INVALID alarm); a successful read
    // clears it. A pass without a DOL read leaves udf untouched.
    #[test]
    fn closed_loop_dol_read_failure_drives_udf() {
        use epics_base_rs::server::record::CommonFields;
        let mut rec = MotorRecord::new();
        rec.links.omsl = 1; // closed_loop
        rec.links.dol = "setpoint_src.VAL".to_string();
        assert!(rec.stat.dmov, "record starts done");
        let mut common = CommonFields::default();

        // Failed read: the resolved report omits DOL.
        rec.set_resolved_input_links(&[]);
        rec.check_alarms(&mut common);
        assert!(common.udf != 0, "failed DOL read marks VAL undefined");

        // Successful read clears it.
        rec.set_resolved_input_links(&["DOL"]);
        rec.check_alarms(&mut common);
        assert!(common.udf == 0, "successful DOL read clears UDF");

        // Supervisory pass (no DOL request): udf stays as-is.
        common.udf = 1;
        rec.links.omsl = 0;
        rec.set_resolved_input_links(&[]);
        rec.check_alarms(&mut common);
        assert!(
            common.udf != 0,
            "a pass without a DOL read leaves udf alone"
        );
    }

    // R61: a settled-idle record keeps the poller alive (Start), never Stop.
    // C asynMotorController::asynMotorPoller (asynMotorController.cpp:615-696)
    // is a while(1) that never stops idle polling, so the MIP_EXTERNAL detector
    // and the idle-poll button resume keep firing. Pre-R61 the settled pass
    // emitted PollDirective::Stop, stranding both end-to-end.
    #[test]
    fn settled_idle_keeps_poller_alive_not_stopped() {
        let mut rec = MotorRecord::new();
        rec.stat.dmov = true;
        // No commands, no schedule_delay, no request_poll / status_refresh.
        let effects = ProcessEffects::default();
        let actions = rec.effects_to_actions(&effects);
        assert_eq!(
            actions.poll,
            PollDirective::Start,
            "a settled idle record must keep the poller alive, not stop it"
        );
    }

    // An explicit refresh request (request_poll / status_refresh — STUP,
    // implicit GET_INFO, settle-resume) must emit PollDirective::Refresh, the
    // forced-post directive (C motorUpdateStatus_ forces statusChanged_=1), not
    // the deduped keep-alive Start — otherwise STUP=BUSY strands on a stationary
    // axis whose status never changes.
    #[test]
    fn refresh_request_forces_poll_directive() {
        let mut rec = MotorRecord::new();
        let status_refresh = ProcessEffects {
            status_refresh: true,
            ..Default::default()
        };
        assert_eq!(
            rec.effects_to_actions(&status_refresh).poll,
            PollDirective::Refresh,
            "status_refresh must force a poll"
        );
        let request_poll = ProcessEffects {
            request_poll: true,
            ..Default::default()
        };
        assert_eq!(
            rec.effects_to_actions(&request_poll).poll,
            PollDirective::Refresh,
            "request_poll must force a poll"
        );
    }

    // R44: C never derives motor UDF from VAL — motorRecord.cc touches udf
    // only at init_record (677-681), the closed-loop DOL collection
    // (1994-2005), and alarm_sub (3372). clears_udf() opts out of the
    // framework's value_is_undefined() per-cycle derivation.
    #[test]
    fn motor_opts_out_of_val_derived_udf() {
        let rec = MotorRecord::new();
        assert!(
            !rec.clears_udf(),
            "motor UDF must not be recomputed from VAL each process pass"
        );
    }

    // C init_record (677-681): a CONSTANT .dol clears UDF at init. An unset
    // DOL is CONSTANT, so the common (no-DOL) axis is defined after init.
    #[test]
    fn constant_dol_clears_udf_at_init() {
        use epics_base_rs::server::record::CommonFields;
        let mut rec = MotorRecord::new();
        rec.links.dol = String::new(); // unset DOL == CONSTANT
        let status = asyn_rs::interfaces::motor::MotorStatus {
            position: 0.0,
            encoder_position: 0.0,
            done: true,
            moving: false,
            ..Default::default()
        };
        rec.initial_readback(&status);
        assert_eq!(
            rec.internal.dol_udf,
            Some(false),
            "init arms the UDF clear for a CONSTANT DOL"
        );
        let mut common = CommonFields::default(); // udf == true (dbCommon default)
        rec.check_alarms(&mut common);
        assert!(common.udf == 0, "CONSTANT DOL motor is defined after init");
    }

    // C leaves udf TRUE for a non-CONSTANT .dol (init_record only clears for
    // CONSTANT, 677-681); only the closed-loop collection's first successful
    // read clears it (1994-2005).
    #[test]
    fn db_link_dol_leaves_udf_undefined_at_init() {
        use epics_base_rs::server::record::CommonFields;
        let mut rec = MotorRecord::new();
        rec.links.omsl = 1; // closed_loop
        rec.links.dol = "setpoint_src.VAL".to_string(); // DB_LINK
        let status = asyn_rs::interfaces::motor::MotorStatus {
            position: 0.0,
            encoder_position: 0.0,
            done: true,
            moving: false,
            ..Default::default()
        };
        rec.initial_readback(&status);
        assert_ne!(
            rec.internal.dol_udf,
            Some(false),
            "a DB_LINK DOL is not cleared at init"
        );
        let mut common = CommonFields::default(); // udf == true
        rec.check_alarms(&mut common);
        assert!(
            common.udf != 0,
            "DB_LINK DOL motor stays undefined until the first DOL read"
        );
    }

    // C reads DOL only when dol.type == DB_LINK; a constant DOL is initialised
    // once and not re-read (motorRecord.cc:1994 vs :680).
    #[test]
    fn closed_loop_constant_dol_not_reread() {
        let mut rec = MotorRecord::new();
        rec.links.omsl = 1;
        rec.links.dol = "5.0".to_string(); // CONSTANT, not a DB link
        assert!(
            !has_dol_read(&rec.pre_process_actions()),
            "a constant DOL must not be re-read each pass"
        );
    }

    // C calls do_work (the DOL read) for pmr->dmov, not on a CALLBACK_DATA
    // poll mid-move (motorRecord.cc:1487-1492).
    #[test]
    fn closed_loop_dol_not_read_mid_move() {
        let mut rec = MotorRecord::new();
        rec.links.omsl = 1;
        rec.links.dol = "setpoint_src.VAL".to_string();
        rec.stat.dmov = false; // a move is in flight
        assert!(
            !has_dol_read(&rec.pre_process_actions()),
            "DOL must not be re-read while a move is in flight"
        );
    }

    // C motorRecord.cc:1509-1510 (restored by c970afbf): FLNK fires on
    // every pass exiting with DMOV true — including a bare idle pass.
    #[test]
    fn test_flnk_fires_on_idle_pass_with_dmov_true() {
        let mut rec = MotorRecord::new();
        assert!(rec.stat.dmov);
        let _ = rec.process();
        assert!(
            rec.should_fire_forward_link(),
            "an idle pass exits with DMOV true — C fires FLNK on it"
        );
    }

    #[test]
    fn test_flnk_fires_on_motion_completion_transition() {
        let mut rec = MotorRecord::new();
        // Enter a move: DMOV goes true→false.
        rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
        rec.set_event(MotorEvent::UserWrite(CommandSource::Val));
        let _ = rec.process();
        assert!(!rec.stat.dmov); // moving
        assert!(!rec.should_fire_forward_link()); // suppressed while moving

        // Driver reports completion; next process finalizes DMOV false→true.
        rec.set_event(MotorEvent::DeviceUpdate(
            asyn_rs::interfaces::motor::MotorStatus {
                position: 10.0,
                encoder_position: 10.0,
                done: true,
                moving: false,
                ..Default::default()
            },
        ));
        let _ = rec.process();
        assert!(rec.stat.dmov); // completed
        assert!(
            rec.should_fire_forward_link(),
            "FLNK must fire on the DMOV false→true completion transition"
        );
    }

    // Public C motorRecord.dbd field surface: every link/menu/alarm/last-value
    // field a CA or PVA client can address must be discoverable through
    // field_list() and round-trip through get/put (writable) or read (NOMOD).
    // Behavioral link routing (DOL closed-loop drive, RDBL readback pull, RLNK
    // forward firing) is deliberately outside this field-presence import.
    #[test]
    fn dbd_link_menu_alarm_fields_are_field_open() {
        use epics_base_rs::types::DbFieldType;

        let mut rec = MotorRecord::new();

        // Writable DBF_INLINK/OUTLINK, DBF_MENU and DBF_DOUBLE alarm fields:
        // present, writable, and surviving a put/get round-trip.
        let writable: &[(&str, DbFieldType, EpicsValue)] = &[
            (
                "RDBL",
                DbFieldType::String,
                EpicsValue::String("$(P)$(R).RBV CP".into()),
            ),
            (
                "DOL",
                DbFieldType::String,
                EpicsValue::String("$(P)setpoint CP".into()),
            ),
            (
                "RLNK",
                DbFieldType::String,
                EpicsValue::String("$(P)done.PROC PP".into()),
            ),
            (
                "OUT",
                DbFieldType::String,
                EpicsValue::String("@asyn(PORT,0)".into()),
            ),
            // `field(OMSL,DBF_MENU)` (motorRecord.dbd:256). The DECLARED type is
            // the enum a client is served (`mapDBFToDBR`); the stored index is
            // still a short. This asserted `Short` while the hand-written table
            // declared it so — the declaration is the `.dbd`'s now.
            ("OMSL", DbFieldType::Enum, EpicsValue::Short(1)),
            ("HIHI", DbFieldType::Double, EpicsValue::Double(95.0)),
        ];
        for (name, dbf, val) in writable {
            let desc = rec
                .field_list()
                .iter()
                .find(|f| f.name == *name)
                .unwrap_or_else(|| panic!("{name} missing from motor field_list()"));
            assert_eq!(desc.dbf_type, *dbf, "{name} dbf_type");
            assert!(!desc.read_only, "{name} must be writable");
            rec.put_field(name, val.clone())
                .unwrap_or_else(|e| panic!("put {name}: {e:?}"));
            assert_eq!(rec.get_field(name), Some(val.clone()), "{name} round-trip");
        }

        // OMSL/HHSV are menu indices and must clamp to their menu range.
        rec.put_field("OMSL", EpicsValue::Short(7)).unwrap();
        assert_eq!(rec.get_field("OMSL"), Some(EpicsValue::Short(1)));

        // Read-only last-value / monitor-map fields (SPC_NOMOD): present,
        // readable, and flagged read-only so the record layer rejects writes.
        let read_only: &[(&str, DbFieldType)] = &[
            ("ALST", DbFieldType::Double),
            ("MLST", DbFieldType::Double),
            // `field(MMAP,DBF_ULONG)` / `field(NMAP,DBF_ULONG)` — the monitor
            // maps are unsigned 32-bit bitmasks. The hand table said DBF_LONG.
            ("MMAP", DbFieldType::ULong),
            ("NMAP", DbFieldType::ULong),
        ];
        for (name, dbf) in read_only {
            let desc = rec
                .field_list()
                .iter()
                .find(|f| f.name == *name)
                .unwrap_or_else(|| panic!("{name} missing from motor field_list()"));
            assert_eq!(desc.dbf_type, *dbf, "{name} dbf_type");
            assert!(desc.read_only, "{name} must be read-only (SPC_NOMOD)");
            assert!(rec.get_field(name).is_some(), "{name} must be readable");
        }
    }
}

#[cfg(test)]
mod menu_choice_tests {
    use super::MotorRecord;
    use epics_base_rs::server::record::FieldDeclaration;

    /// The choices a client sees are the DECLARATION's — `motorRecord.dbd`'s
    /// `menu()` on each field — and the index↔string mapping is wire-visible.
    /// This used to assert them through `Record::menu_field_choices`, a hand
    /// written table that declared the same menus a second time; `HLSV` needed
    /// a per-record mapping there only because the shared-menu registry keys
    /// `menuAlarmSevr` by the standard field names. The declaration has no such
    /// problem: `field(HLSV,DBF_MENU) { menu(menuAlarmSevr) }` points the
    /// FieldDesc straight at base's `MENU_ALARM_SEVR`.
    #[test]
    fn motor_menu_choices_come_from_the_declaration() {
        let rec = MotorRecord::new();
        let menu = |name: &str| {
            rec.field_list()
                .iter()
                .find(|f| f.name == name)
                .unwrap_or_else(|| panic!("{name} is declared"))
                .menu
        };
        assert_eq!(menu("DIR"), Some(&["Pos", "Neg"][..]));
        assert_eq!(menu("FOFF"), Some(&["Variable", "Frozen"][..]));
        assert_eq!(menu("SPMG"), Some(&["Stop", "Pause", "Move", "Go"][..]));
        assert_eq!(
            menu("RMOD"),
            Some(&["Default", "Arithmetic", "Geometric", "In-Position"][..])
        );
        assert_eq!(menu("STUP"), Some(&["OFF", "ON", "BUSY"][..]));
        // NTM is menu(menuYesNo) — base's table, referenced, not re-declared.
        assert_eq!(menu("NTM"), Some(&["NO", "YES"][..]));
        // HLSV is menu(menuAlarmSevr) — likewise base's.
        assert_eq!(
            menu("HLSV"),
            Some(&["NO_ALARM", "MINOR", "MAJOR", "INVALID"][..])
        );
        // UEIP/URIP share menu(motorUEIP).
        assert_eq!(menu("UEIP"), menu("URIP"));
        assert_eq!(menu("VAL"), None);
    }
}
