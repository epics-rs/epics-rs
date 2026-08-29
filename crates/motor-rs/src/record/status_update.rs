use super::*;

impl MotorRecord {
    /// Determine the event for this process cycle by reading shared device state.
    pub(crate) fn determine_event(&mut self) -> Option<MotorEvent> {
        // C dbScanLock serializes one pass per signal: a dbPutField pass
        // never consumes the device callback's payload — the callback's
        // own dbProcess (devMotorAsyn.c statusCallback 757-766, delay
        // callbackFunc) follows with a CALLBACK_DATA pass. When a
        // put-side signal owns this pass (last_write, or the resolution
        // re-anchor mark a pp(TRUE) MRES-family put parked), leave the
        // mailbox untouched — no status consume, no STUP ack, no delay
        // take. The io_intr pulse that announced a pending status or
        // delay expiry is already queued and triggers its own pass.
        //
        // The deferral applies only once the record is anchored: in C,
        // init_record precedes any dbPutField pass (iocInit), so no put
        // can exist before the anchor. A write parked before the first
        // status pulse (an autosave pass-1 restore, or a CA put landing
        // in the init→first-poll window) must NOT turn that pulse into a
        // put pass — the pulse anchors (Startup) and the parked write
        // replays on the pass the anchor's forced refresh queues.
        if (self.last_write.is_some() || self.internal.res_reanchor) && self.initialized {
            return None;
        }
        // Extract data from shared state, then drop the lock before mutating self
        let (delay_id, new_status) = {
            let state = self.device_state.as_ref()?;
            let mut ds = match state.lock() {
                Ok(ds) => ds,
                Err(e) => {
                    tracing::error!("device state lock poisoned in determine_event: {e}");
                    return None;
                }
            };

            let delay_id = ds.expired_delay_id.take();
            let new_status = ds
                .latest_status
                .as_ref()
                .filter(|s| s.seq != self.last_seen_seq)
                .cloned();

            (delay_id, new_status)
        };

        // Check delay expiry first (higher priority)
        if let Some(delay_id) = delay_id {
            if delay_id == self.next_delay_id.wrapping_sub(1) {
                return Some(MotorEvent::DelayExpired);
            }
            // Stale delay -- ignore
        }

        // Check for new motor status
        if let Some(stamped) = new_status {
            self.last_seen_seq = stamped.seq;
            let status = stamped.status;
            // C process_exit (1498-1502): a CALLBACK_DATA pass returns a
            // BUSY STUP to OFF — the GET_INFO response has arrived. The
            // pre-clear value is latched one pass so the done branch can
            // apply the C 1345 gate (the ack is not a motion completion).
            if self.stat.stup == 2 {
                self.stat.stup = 0;
                self.internal.stup_ack = true;
            }
            if !self.initialized {
                self.initialized = true;
                return Some(MotorEvent::Startup);
            }
            // Always apply readback even in Idle
            if self.stat.phase == MotionPhase::Idle {
                self.process_motor_info(&status);
                // C process_reason: this pass IS a CALLBACK_DATA pass
                // even though no event is reported — mark it so
                // do_process_inner routes it through the idle completion
                // pipeline (LOAD_P collapse, pp sync, C 1396-1409) and
                // keeps the put-pass-only implicit GET_INFO (C 2546)
                // off, which is C's status-refresh loop prevention.
                self.internal.idle_status_pass = true;
                return None;
            }
            return Some(MotorEvent::DeviceUpdate(status));
        }
        None
    }

    /// Convert ProcessEffects to DeviceActions for the shared mailbox.
    pub(crate) fn effects_to_actions(&mut self, effects: &ProcessEffects) -> DeviceActions {
        let poll = if effects.request_poll {
            // An explicit poll request (move dispatch, LOAD_POS readback,
            // retarget) wants a fresh status post — Refresh forces it through
            // even if the polled status is unchanged.
            PollDirective::Refresh
        } else if effects.status_refresh {
            // STUP / implicit GET_INFO / DELAY_ACK settle-resume. C's
            // motorUpdateStatus_ forces statusChanged_=1 (asynMotorController
            // .cpp:217-222) so the ack callback always fires and clears
            // STUP=BUSY; Refresh is the analogue. A plain Start would dedup
            // against the already-running poller and leave STUP stranded on a
            // stationary axis whose status never changes.
            PollDirective::Refresh
        } else if effects.commands.is_empty() && effects.schedule_delay.is_none() && self.stat.dmov
        {
            // C asynMotorController::asynMotorPoller (asynMotorController.cpp:
            // 615-696) is a while(1) that NEVER stops idle polling — it polls
            // every idlePollPeriod_ when !anyMoving, so an external move / limit
            // trip / encoder drift while the record is idle is still detected.
            // The MIP_EXTERNAL detector (status_update.rs:239) and the idle-poll
            // button resume (the Idle-arm dispatch_latent_collection) both
            // depend on this poll. Keep the poller alive at the idle rate
            // (effective_poll_interval returns idle_poll_interval once the
            // completing poll cleared last_moving) instead of stopping it; Start
            // is idempotent via the polling_active dedup while already polling,
            // and resumes the poller after a settle delay (ScheduleDelay reset
            // polling_active). The record intentionally never emits Stop — C has
            // no record-driven poller stop; idle_poll_interval == 0 gives C's
            // event-only idle mode (idlePollPeriod_ == 0) at the loop's timed arm.
            PollDirective::Start
        } else {
            PollDirective::None
        };

        let schedule_delay = effects.schedule_delay.map(|dur| {
            let id = self.next_delay_id;
            self.next_delay_id += 1;
            DelayRequest { id, duration: dur }
        });

        DeviceActions {
            commands: effects.commands.clone(),
            poll,
            schedule_delay,
            status_refresh: effects.status_refresh,
        }
    }

    /// Compute DMOV from current state.
    pub fn compute_dmov(&self) -> bool {
        let driver_done =
            self.stat.msta.contains(MstaFlags::DONE) && !self.stat.msta.contains(MstaFlags::MOVING);
        let no_pending = self.stat.phase == MotionPhase::Idle;
        driver_done && no_pending
    }

    /// Update readback positions from driver status. The runtime poll path
    /// uses this (`initcall = false`).
    ///
    /// C `process_motor_info(pmr, initcall)` (motorRecord.cc:3652+) takes the
    /// init flag explicitly; the URIP RDBL scaling is gated `else if
    /// (urip==Yes && initcall==false)` (3682), so the init readback seeds RRBV
    /// from the motor position rather than the external link. The init path
    /// (`device_support::init` + `initial_readback`) therefore calls
    /// `process_motor_info_initcall(.., true)`. (An earlier Rust version gated
    /// URIP on `self.initialized`, but `determine_event` flips that true
    /// *before* dispatching the Startup readback, so the gate never skipped
    /// the call that actually seeds DVAL/VAL.)
    pub fn process_motor_info(&mut self, status: &asyn_rs::interfaces::motor::MotorStatus) {
        self.process_motor_info_initcall(status, false);
    }

    /// `process_motor_info` with C's explicit `initcall`. `initcall = true`
    /// suppresses the URIP RDBL readback scaling (the init readback adopts the
    /// motor position, not the external readback link).
    pub(crate) fn process_motor_info_initcall(
        &mut self,
        status: &asyn_rs::interfaces::motor::MotorStatus,
        initcall: bool,
    ) {
        // C 3671-3675: UEIP=Yes is demoted to No on any poll where the
        // driver reports no encoder. This — not the put handler — is
        // what vets a .db-loaded UEIP=Yes: dbLoadRecords writes field()
        // values raw, and the first poll performs the encoder check.
        // C posts the change without MARKing M_UEIP, so no re-anchor.
        if self.conv.ueip && !status.has_encoder {
            self.conv.ueip = false;
        }

        // Layer 1: update raw positions. C devMotorAsyn.c:452/459 rounds the
        // raw motor and encoder counts with floor(x + 0.5) (half toward +inf),
        // NOT NINT (half away from zero). They differ only at an exact .5 on a
        // negative count (raw -2.5 -> C -2, Rust .round() -> -3). The other raw
        // conversions in this file (rdif, rval, URIP rrbv) use C NINT
        // (motorRecord.cc) == Rust .round(), so they must stay .round().
        self.pos.rmp = (status.position / self.conv.mres + 0.5).floor() as i64;

        // C devMotorAsyn.c:459-464 — REP is the raw encoder count,
        // independent of UEIP (C rounds the count the asyn layer already
        // reports raw; the Rust EGU convention converts at the boundary,
        // so the encoder scale is ERES whether or not the readback uses
        // it). MRES is only a fallback for an invalid runtime ERES.
        let eres_valid = self.conv.eres.is_finite() && self.conv.eres != 0.0;
        if eres_valid {
            self.pos.rep = (status.encoder_position / self.conv.eres + 0.5).floor() as i64;
        } else {
            if self.conv.ueip {
                tracing::warn!(
                    "UEIP set but ERES invalid ({:.6}), falling back to MRES for REP",
                    self.conv.eres
                );
            }
            self.pos.rep = (status.encoder_position / self.conv.mres + 0.5).floor() as i64;
        }

        // RRBV depends on UEIP
        self.pos.rrbv = if self.conv.ueip {
            self.pos.rep
        } else {
            self.pos.rmp
        };

        // URIP path: use external readback link value with RRES conversion.
        // C 3682: skipped on the init call (`initcall == false` required).
        if !self.conv.ueip && self.conv.urip && !initcall {
            if let Some(rdbl_value) = self.conv.rdbl_value {
                let rres = if self.conv.rres != 0.0 {
                    self.conv.rres
                } else {
                    1.0
                };
                self.pos.rrbv = ((rdbl_value * rres) / self.conv.mres).round() as i64;
            }
        }

        // DRBV: use ERES for encoder path (UEIP), MRES for motor position path
        let resolution = if self.conv.ueip && eres_valid {
            self.conv.eres
        } else {
            self.conv.mres
        };
        self.pos.drbv = coordinate::raw_to_dial(self.pos.rrbv, resolution);

        // RBV from DRBV
        self.pos.rbv = coordinate::dial_to_user(self.pos.drbv, self.conv.dir, self.pos.off);

        // DIFF and RDIF
        self.pos.diff = self.pos.dval - self.pos.drbv;
        // C: rdif = NINT(diff / mres) -- raw step difference
        self.pos.rdif = if self.conv.mres != 0.0 {
            (self.pos.diff / self.conv.mres).round() as i64
        } else {
            0
        };
        // C `process_motor_info` MARKs M_DIFF / M_RDIF unconditionally on
        // every CALLBACK_DATA pass (motorRecord.cc:3765,3767); `monitor()`
        // (3522-3531) posts both with `monitor_mask | DBE_VAL_LOG` whether
        // or not the value moved. Record the mark so `force_posted_fields`
        // re-posts DIFF/RDIF this cycle even when unchanged — a settled
        // axis at constant non-zero following error still emits them each
        // poll. The mark is one-pass: reset at the top of `process()`.
        self.internal.diff_rdif_marked = true;

        // MOVN: C uses RAW limit switches (rhls/rlls) with RAW cdir
        // Must compute ls_active BEFORE mapping limits to user coordinates
        let ls_active =
            (status.high_limit && self.stat.cdir) || (status.low_limit && !self.stat.cdir);
        self.stat.movn = !(ls_active || status.done || status.problem);
        // C 3744-3747: a struck limit in the commanded direction or a
        // driver problem releases every latched motion button, so the
        // stopped axis is not re-commanded on the next pass.
        if ls_active || status.problem {
            self.clear_buttons();
        }

        // C: ea063f5f (2008) — when the driver is moving but the record had no
        // pending motion, this is an externally initiated move (someone called
        // the controller directly, or another record drove the same axis).
        // Mark MIP_EXTERNAL and clear DMOV; do_process() routes the next
        // process() into check_completion via the MIP_EXTERNAL bit.
        if self.stat.movn
            && self.stat.dmov
            && self.stat.phase == MotionPhase::Idle
            && !self.stat.mip.contains(MipFlags::EXTERNAL)
        {
            self.stat.dmov = false;
            self.stat.mip |= MipFlags::EXTERNAL;
        }

        // Raw limit switch readbacks (C 3727-3728), then the user-mapped
        // pair derives from them by DIR/MRES polarity (C 3733-3734:
        // hls = ((dir == Pos) == (mres >= 0)) ? rhls : rlls).
        self.limits.rhls = status.high_limit;
        self.limits.rlls = status.low_limit;
        let same_polarity = (self.conv.dir == MotorDir::Pos) == (self.conv.mres >= 0.0);
        if same_polarity {
            self.limits.hls = self.limits.rhls;
            self.limits.lls = self.limits.rlls;
        } else {
            self.limits.hls = self.limits.rlls;
            self.limits.lls = self.limits.rhls;
        }

        // Build MSTA from driver status
        let mut msta = MstaFlags::empty();
        if status.direction {
            msta |= MstaFlags::DIRECTION;
        }
        if status.done {
            msta |= MstaFlags::DONE;
        }
        if status.moving {
            msta |= MstaFlags::MOVING;
        }
        if status.high_limit {
            msta |= MstaFlags::PLUS_LS;
        }
        if status.low_limit {
            msta |= MstaFlags::MINUS_LS;
        }
        if status.home {
            msta |= MstaFlags::HOME_LS;
        }
        if status.encoder_home {
            msta |= MstaFlags::EA_HOME;
        }
        if status.powered {
            msta |= MstaFlags::POSITION;
        }
        if status.problem {
            msta |= MstaFlags::PROBLEM;
        }
        if status.slip_stall {
            msta |= MstaFlags::SLIP_STALL;
        }
        if status.comms_error {
            msta |= MstaFlags::COMM_ERR;
        }
        if status.gain_support {
            msta |= MstaFlags::GAIN_SUPPORT;
        }
        if status.has_encoder {
            msta |= MstaFlags::ENCODER_PRESENT;
        }
        // epics-modules/motor #76 — drivers that don't honor VBAS expose it
        // via MSTA bit 15 so the record can drop VBAS from accel math.
        if !status.vbas_supported {
            msta |= MstaFlags::VBAS_UNSUPPORTED;
        }
        // RA_HOMED (bit 14) mirrors the driver status word like every
        // other MSTA bit. C copies pmr->msta wholesale from the driver
        // each poll (devMotorAsyn.c:467) and never record-manages
        // RA_HOMED — motorRecord.cc writes it nowhere. A record-side
        // sticky latch would report a permanently-homed axis after the
        // driver de-asserts homed (re-home, controller reset, a
        // SetPosition redefine the controller treats as un-homed).
        if status.homed {
            msta |= MstaFlags::HOMED;
        }
        let msta_changed = msta != self.stat.msta;
        self.stat.msta = msta;

        // C monitor() (3541-3549): when the controller supports closed-loop
        // gain (GAIN_SUPPORT), CNEN is a readback of the EA_POSITION
        // (position-maintenance/torque) bit — but the readback lives inside
        // monitor()'s MARKED(M_MSTA) branch, so it refreshes only on a poll
        // where MSTA actually changed, not every poll. Gating on msta_changed
        // keeps a user-written CNEN that the driver has not yet reflected in
        // EA_POSITION from being reverted a poll early.
        if msta_changed && msta.contains(MstaFlags::GAIN_SUPPORT) {
            self.ctrl.cnen = msta.contains(MstaFlags::POSITION);
        }

        // C: tdir = msta.RA_DIRECTION (from driver on every poll)
        self.stat.tdir = status.direction;

        // C 3755-3762: ATHM tracks the motor's home switch — or the
        // encoder's home signal when UEIP=Yes — on every poll. It is
        // pure switch readback, not a "has homed" latch (that is MSTA
        // bit 14, RA_HOMED).
        self.stat.athm = if self.conv.ueip {
            msta.contains(MstaFlags::EA_HOME)
        } else {
            msta.contains(MstaFlags::HOME_LS)
        };

        // C devMotorAsyn.c:469-474 — RVEL is the RAW velocity in steps/s
        // (motorRecord.dbd DBF_LONG "Raw Velocity"), floor()ed as in C.
        // The C asyn layer reports controller units (raw steps); the Rust
        // driver convention is EGU (AsynMotor docs), so convert through
        // MRES at the record boundary exactly like RMP above.
        self.stat.rvel = if self.conv.mres != 0.0 {
            (status.velocity / self.conv.mres).floor() as i64
        } else {
            0
        };

        // LVIO is NOT recomputed here. C re-evaluates it only at
        // enter_do_work (1463-1484: jog from live RBV, home disabled,
        // everything else preserves the latched value) — ported in
        // check_completion's still-moving path — and in the do_work move
        // block / soft-limit puts, ported in plan_absolute_move and
        // detect_inverted_limits.
    }

    /// Sync all positions from readback.
    pub fn sync_positions(&mut self) {
        self.pos.dval = self.pos.drbv;
        self.pos.val = self.pos.rbv;
        // C 712/843/4455: the synced RVAL is the raw MOTOR command
        // equivalent of the dial value — NINT(dval/mres) — never RRBV,
        // which is encoder counts under UEIP=Yes (or the RDBL scaling
        // under URIP) and differs whenever ERES != MRES.
        self.pos.rval = (self.pos.dval / self.conv.mres).round() as i64;
        self.pos.diff = 0.0;
        self.pos.rdif = 0;
        self.internal.lval = self.pos.val;
        self.internal.ldvl = self.pos.dval;
        self.internal.lrvl = self.pos.rval;
    }

    /// Motion-completion drive resync (C postProcess 827-849).
    ///
    /// C gates the resync on `omsl != menuOmslclosed_loop` (827): under
    /// closed-loop OMSL the drive values belong to the DOL cascade, and a
    /// stop or completion must not overwrite them with the readback. The
    /// remaining C conjuncts — `!(mip & (MIP_MOVE | MIP_MOVE_BL |
    /// MIP_JOG_BL1 | MIP_JOG_BL2))` — hold structurally at the call
    /// sites: the state machine reaches them only at genuine completions,
    /// never with a move/backlash continuation still pending.
    pub(crate) fn postprocess_sync(&mut self) {
        if self.links.omsl != 1 {
            self.sync_positions();
        }
    }

    /// Initial readback and position sync at startup.
    ///
    /// On entry `self.pos.dval` holds the autosave-restored target (or 0 if
    /// none). C: `devMotorAsyn.c::init_controller` consults RSTM to decide
    /// whether to push that value back into the driver instead of adopting
    /// the driver's current readback.
    pub fn initial_readback(
        &mut self,
        status: &asyn_rs::interfaces::motor::MotorStatus,
    ) -> ProcessEffects {
        let mut effects = ProcessEffects::default();

        // Capture the autosaved target before the driver readback overwrites it.
        let autosaved_dval = self.pos.dval;
        let autosaved_rval = self.pos.rval;
        // C init_record runs `process_motor_info(pmr, true)` — the init call
        // skips URIP RDBL scaling and seeds the readback from motor position.
        self.process_motor_info_initcall(status, true);

        // DMOV from driver
        self.stat.dmov = status.done && !status.moving;

        if status.moving {
            // At startup, the poll loop may not be active yet — request it.
            effects.request_poll = true;
        }

        // C init_record (motorRecord.cc:677-681): a CONSTANT .dol clears UDF
        // at init (`pmr->udf = FALSE`) and seeds VAL from the constant. The
        // motor never derives UDF from VAL otherwise — clears_udf() returns
        // false so the framework's value_is_undefined() path is off — so this
        // is the only init UDF clear for the common (no-DOL / literal-DOL)
        // axis. A DB_LINK / CA DOL is left undefined until the closed-loop
        // collection's first successful read clears it (1994-2005), exactly
        // like C leaving udf TRUE for a non-CONSTANT .dol. An unset DOL has
        // C link type CONSTANT (recGblInitConstantLink is a no-op leaving VAL
        // at 0), so ParsedLink::None counts alongside a literal Constant.
        if matches!(
            parse_link_v2(&self.links.dol),
            ParsedLink::None | ParsedLink::Constant(_)
        ) {
            self.internal.dol_udf = Some(false);
        }

        // A write parked before this anchor (an autosave pass-1 restore or
        // a CA put in the init→first-poll window — a window C cannot have,
        // since init_record precedes any dbPutField pass) means the drive
        // triplet already carries that write's target, not saved state.
        // Anchor the readback only: no RSTM decision (its DVAL input is
        // gone, and a SetPosition to the parked target would redefine the
        // axis to where the put asks it to MOVE), no drive-field sync (it
        // would destroy the target), and no lasts anchoring (C anchors
        // them to the pre-put values, which is what they still hold — an
        // anchored-to-target lval would swallow the replay's val != lval
        // change detection). The forced refresh queues the replay pass.
        if self.last_write.is_some() {
            effects.request_poll = true;
            return effects;
        }

        // C: devMotorAsyn.c init_controller — RSTM restore decision.
        // rdbd = max(|RDBD|, |MRES|); dval_non_zero_pos_near_zero is true when
        // the autosaved DVAL is meaningful but the driver currently sits near
        // zero (i.e. the controller lost its position across the IOC restart).
        //
        // C compares `fabs(status.position * mres)` — the *motor* position
        // dial value, always via MRES. Use the motor raw position (RMP), not
        // DRBV, since DRBV follows the encoder (ERES) when UEIP=Yes.
        let rdbd = self.retry.rdbd.abs().max(self.conv.mres.abs());
        let motor_dial = coordinate::raw_to_dial(self.pos.rmp, self.conv.mres);
        let dval_non_zero_pos_near_zero =
            autosaved_dval.abs() > rdbd && self.conv.mres != 0.0 && motor_dial.abs() < rdbd;
        let mut restore = self
            .conv
            .rstm
            .should_restore(self.use_relative_moves(), dval_non_zero_pos_near_zero);

        // epics-modules/motor #231 — if LOAD_POS is blocked for this axis
        // (absolute encoder), never push a SetPosition; adopt driver readback.
        if restore && self.conv.loadpos_blocked {
            restore = false;
        }

        // epics-modules/motor #196 — guard against an MRES change since the
        // autosave was written. If both DVAL and RVAL were autosaved, they
        // must satisfy DVAL ≈ RVAL * MRES under the *current* MRES. A mismatch
        // means MRES changed; restoring would place the axis at the wrong
        // position, so skip the restore and adopt the driver readback instead.
        if restore && autosaved_rval != 0 {
            let rval_implied_dval = autosaved_rval as f64 * self.conv.mres;
            if (rval_implied_dval - autosaved_dval).abs() > rdbd {
                tracing::warn!(
                    "RSTM restore skipped: autosaved DVAL {:.4} inconsistent with \
                     RVAL {} * MRES {:.6} = {:.4} — MRES likely changed since autosave",
                    autosaved_dval,
                    autosaved_rval,
                    self.conv.mres,
                    rval_implied_dval,
                );
                restore = false;
            }
        }

        if restore {
            // Adopt the autosaved DVAL: keep record coordinates and tell the
            // driver to redefine its current position to that value.
            self.pos.dval = autosaved_dval;
            self.pos.val = coordinate::dial_to_user(autosaved_dval, self.conv.dir, self.pos.off);
            if let Ok(rval) = coordinate::dial_to_raw(autosaved_dval, self.conv.mres) {
                self.pos.rval = rval;
            }
            self.internal.ldvl = autosaved_dval;
            self.internal.lval = self.pos.val;
            self.internal.lrvl = self.pos.rval;
            effects.commands.push(MotorCommand::SetPosition {
                position: autosaved_dval,
            });
        } else if self.links.omsl == 1 {
            // C init_record (705-714) skips the readback adoption under
            // closed-loop OMSL — the DOL link owns the drive-value
            // initialization — but still anchors the change-detection
            // lasts (730-732).
            self.internal.lval = self.pos.val;
            self.internal.ldvl = self.pos.dval;
            self.internal.lrvl = self.pos.rval;
        } else {
            self.sync_positions();
        }

        effects
    }
}
