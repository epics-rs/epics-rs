use super::*;

impl MotorRecord {
    /// Check if motion has completed and handle post-motion pipeline.
    pub fn check_completion(&mut self) -> ProcessEffects {
        let mut effects = ProcessEffects::default();

        // C `do_work` (motorRecord.cc:1973) calls `enforceMinRetryDeadband`
        // before any retry evaluation, so C's `maybeRetry`
        // (`fabs(diff) >= rdbd`, line 1049) always compares against an
        // RDBD that is at least |MRES|. The retry comparison here is
        // therefore boundary-inclusive with NO `rdbd > 0` guard — but
        // that is only correct when RDBD has actually been enforced.
        // Enforce it here so the completion-time retry evaluation sees a
        // C-faithful RDBD even when this is reached without an
        // intervening field-write pass.
        self.enforce_min_retry_deadband();

        // C: db5da2f0 — if the external URIP readback is in error while a
        // motion is in progress, stop the axis immediately. New moves are
        // already blocked at plan_motion.
        if self.conv.urip
            && self.conv.rdbl_error
            && self.stat.phase != MotionPhase::Idle
            && !self.stat.mip.contains(MipFlags::STOP)
        {
            self.stat.mip.insert(MipFlags::STOP);
            // An error stop post-processes like a commanded stop (sync at
            // completion), not like a Pause (resume armed).
            self.internal.pp = true;
            effects.commands.push(MotorCommand::Stop {
                acceleration: self.move_accel_egu(),
            });
            effects.suppress_forward_link = true;
            return effects;
        }

        // C: after DLY expires and fresh readback arrives, evaluate for retry
        if self.stat.mip.contains(MipFlags::DELAY_ACK) {
            self.stat.mip.remove(MipFlags::DELAY_ACK);
            self.evaluate_position_error_after_delay(&mut effects);
            return effects;
        }

        let driver_done =
            self.stat.msta.contains(MstaFlags::DONE) && !self.stat.msta.contains(MstaFlags::MOVING);

        if !driver_done {
            // Still moving — poll loop is already active, just suppress FLNK.
            effects.suppress_forward_link = true;
            return effects;
        }

        // Check for pending retarget after stop completes
        if self.stat.mip.contains(MipFlags::STOP) {
            if let Some(new_target) = self.internal.pending_retarget.take() {
                // Replan motion to new target
                self.stat.mip = MipFlags::empty();
                self.pos.dval = new_target;
                self.pos.val = coordinate::dial_to_user(new_target, self.conv.dir, self.pos.off);
                if let Ok(rval) = coordinate::dial_to_raw(new_target, self.conv.mres) {
                    self.pos.rval = rval;
                }
                self.plan_absolute_move(&mut effects);
                return effects;
            }
            // Resume a jog/home command that was queued behind this motion.
            // Only an explicit queued request (internal.queued_motion) is
            // replayed — an active jog/home that a plain STOP just halted
            // is NOT (the stop path replaces MIP wholesale and clears the
            // buttons, C motorRecord.cc:1893/1903).
            if let Some(queued) = self.internal.queued_motion.take() {
                self.stat.mip.remove(MipFlags::STOP);
                // C: the queued request armed pp=TRUE (2025/2110), so this
                // stop completion runs postProcess first — VAL/DVAL/RVAL
                // sync to the rest position (826-849) — and only then
                // re-fires the request (858-890). A target written while
                // the stop was in flight is dropped here, like C's replay
                // gate refusing `mip & MIP_STOP` (1385). The re-fired
                // motion starts with pp armed again (887, 2125), so a
                // pause interrupting it syncs at its own stop completion.
                self.sync_positions();
                self.internal.pp = true;
                match queued {
                    QueuedMotion::Home { forward } => {
                        self.set_phase(MotionPhase::Homing);
                        let hw_forward = if self.conv.mres >= 0.0 {
                            forward
                        } else {
                            !forward
                        };
                        self.stat.cdir = hw_forward;
                        effects.commands.push(MotorCommand::Home {
                            forward: hw_forward,
                            velocity: self.vel.hvel,
                            acceleration: self.move_accel_egu(),
                        });
                    }
                    QueuedMotion::Jog { forward } => {
                        self.set_phase(MotionPhase::Jog);
                        self.internal.jog_was_forward = forward;
                        effects.commands.push(MotorCommand::MoveVelocity {
                            direction: forward,
                            velocity: self.vel.jvel,
                            acceleration: self.jog_accel_egu(),
                        });
                    }
                }
                effects.request_poll = true;
                effects.suppress_forward_link = true;
                return effects;
            }
            // Plain stop. C discriminates by pp (motorRecord.cc:1383-1402):
            // a commanded stop (or interrupted jog/home/backlash, all of
            // which armed pp) runs postProcess — VAL/DVAL/RVAL <- readback
            // and MIP_STOP cleared (826-849, 1027-1032) — so maybeRetry
            // never runs (mip == MIP_DONE at its 1432 gate).
            let pp = std::mem::take(&mut self.internal.pp);
            if pp {
                self.sync_positions();
                self.finalize_or_delay(&mut effects);
                return effects;
            }
            // A Pause never set pp: postProcess is skipped, mip stays
            // MIP_STOP, and maybeRetry (1040-1100) runs against the
            // preserved target. A limit switch in the commanded direction
            // blocks the retry (1048).
            let diff = (self.pos.dval - self.pos.drbv).abs();
            let same_polarity = (self.conv.dir == MotorDir::Pos) == (self.conv.mres >= 0.0);
            let user_cdir = if same_polarity {
                self.stat.cdir
            } else {
                !self.stat.cdir
            };
            let ls_blocks = (self.limits.hls && user_cdir) || (self.limits.lls && !user_cdir);
            if diff >= self.retry.rdbd && !ls_blocks && self.retry.rtry != 0 {
                if self.retry.rcnt >= self.retry.rtry {
                    // C 1060-1075: too many retries — give up, MISS
                    // latches, the lasts adopt the unreached target
                    // (finalize_motion's lval/ldvl sync matches 1067-1069).
                    self.retry.miss = true;
                    self.finalize_or_delay(&mut effects);
                } else {
                    // C 1077-1082 with 1356's dmov=TRUE UNMARKed and
                    // reversed: DMOV never posts 1 — the paused move stays
                    // "not done" with mip = MIP_RETRY armed. The Go pass
                    // re-fires it via the move-block `!dmov` gate (2241);
                    // rcnt counts each paused stop against RTRY.
                    self.retry.rcnt += 1;
                    self.stat.mip = MipFlags::RETRY;
                    self.set_phase(MotionPhase::Idle);
                    effects.suppress_forward_link = true;
                }
            } else {
                // Close enough, RTRY disabled, or LS blocked: C maybeRetry
                // else-branch (1084-1099) — collapse to DONE without any
                // sync (the lasts already equal the reached target from
                // the dispatch-time load). MISS clears only on the
                // close-enough/LS path (1087-1091); the rtry==0 path
                // leaves it. DLY arms only on a real completion edge
                // (C 1457 needs a fresh M_DMOV mark): an idle Pause
                // convergence pass (dmov already true) finalizes quietly.
                if diff < self.retry.rdbd || ls_blocks {
                    self.retry.miss = false;
                }
                if self.stat.dmov {
                    self.finalize_motion(&mut effects);
                } else {
                    self.finalize_or_delay(&mut effects);
                }
            }
            return effects;
        }

        match self.stat.phase {
            MotionPhase::MainMove => {
                if self.internal.backlash_pending {
                    self.start_backlash_final(&mut effects);
                } else {
                    self.evaluate_position_error(&mut effects);
                }
            }
            MotionPhase::BacklashFinal => {
                self.evaluate_position_error(&mut effects);
            }
            MotionPhase::Retry => {
                self.evaluate_position_error(&mut effects);
            }
            MotionPhase::Jog | MotionPhase::JogStopping => {
                // C 1357-1364 (9c8a8e8c, PR #56): the driver reported done
                // while the record still thinks it is jogging (no commanded
                // stop — that would have moved us to JogStopping or
                // MIP_STOP). The controller stopped on its own (internal
                // limit, fault, host stop): clear_buttons() so the latched
                // button does not re-fire the jog on the next pass.
                if self.stat.phase == MotionPhase::Jog {
                    self.ctrl.jogf = false;
                    self.ctrl.jogr = false;
                    self.ctrl.homf = false;
                    self.ctrl.homr = false;
                    // C 1357-1364 collapses the sudden stop to MIP_DONE,
                    // so the replay gate (1385) passes — a VAL written
                    // during the jog re-fires now. A commanded stop
                    // (JogStopping = MIP_JOG_STOP) is excluded: the write
                    // is dropped by the sync below, like C postProcess.
                    if self.replay_overtaken_target(&mut effects) {
                        return effects;
                    }
                }
                // C: postProcess syncs VAL<-RBV, DVAL<-DRBV before jog backlash
                // This ensures start_jog_backlash uses the jog-end position as base
                self.sync_positions();
                if self.needs_jog_backlash() {
                    self.start_jog_backlash(&mut effects);
                } else {
                    self.finalize_or_delay(&mut effects);
                }
            }
            MotionPhase::JogBacklash => {
                if self.internal.backlash_pending {
                    // BL1 complete -> start BL2 (final approach)
                    self.start_jog_backlash_final(&mut effects);
                } else {
                    // C: MIP_JOG_BL2 passes the replay gate (1385) — a
                    // VAL written during the jog backlash re-fires here.
                    if self.replay_overtaken_target(&mut effects) {
                        return effects;
                    }
                    // BL2 complete -> finalize
                    self.finalize_or_delay(&mut effects);
                }
            }
            MotionPhase::Homing => {
                self.stat.athm = true;
                // C postProcess home-done (893-906): clear the HOMF/HOMR
                // buttons at home completion. Matters for a home resumed
                // from queued_motion, whose button stayed latched through
                // the stop (the C re-fire path keeps it until done).
                self.ctrl.homf = false;
                self.ctrl.homr = false;
                // C 1387-1397: a VAL written while homing could not
                // dispatch (handle_retarget ignores HOMF/HOMR) — replay
                // it instead of syncing it away. The button clear above
                // doubles as C's VAL-HOMF-VAL infinite-home-loop guard
                // (1389-1393).
                if self.replay_overtaken_target(&mut effects) {
                    return effects;
                }
                // Sync positions after homing
                self.sync_positions();
                self.finalize_or_delay(&mut effects);
            }
            MotionPhase::DelayWait => {
                // Delay already handled
                self.finalize_motion(&mut effects);
            }
            MotionPhase::Idle => {
                // C: ea063f5f — if the record marked an externally initiated
                // move (MIP_EXTERNAL set during process_motor_info), close
                // the loop once the driver reports done. Reseed VAL/DVAL/RVAL
                // from the readback and clear MIP.
                if self.stat.mip.contains(MipFlags::EXTERNAL)
                    && self.stat.msta.contains(MstaFlags::DONE)
                    && !self.stat.movn
                {
                    self.sync_positions();
                    self.finalize_motion(&mut effects);
                }
            }
        }

        effects
    }

    /// Either start DLY wait or finalize immediately.
    pub(crate) fn finalize_or_delay(&mut self, effects: &mut ProcessEffects) {
        if self.timing.dly > 0.0 {
            self.set_phase(MotionPhase::DelayWait);
            self.stat.mip.insert(MipFlags::DELAY_REQ);
            effects.schedule_delay = Some(std::time::Duration::from_secs_f64(self.timing.dly));
            effects.suppress_forward_link = true;
        } else {
            self.finalize_motion(effects);
        }
    }

    /// Finalize motion: set Idle, DMOV=true.
    pub(crate) fn finalize_motion(&mut self, _effects: &mut ProcessEffects) {
        self.set_phase(MotionPhase::Idle);
        self.stat.mip = MipFlags::empty();
        self.stat.dmov = true;
        self.stat.movn = false;
        self.retry.rcnt = 0;
        // C postProcess clears pp on entry (825); every normal completion
        // runs it, so a leftover pp from a revived stop (ExtendMove over a
        // decelerating axis) must not leak into the next motion.
        self.internal.pp = false;
        self.internal.backlash_pending = false;
        self.internal.pending_retarget = None;
        self.internal.queued_motion = None;
        self.internal.verify_retarget_on_completion = false;
        // No blanket button clear here: C clears the buttons only at
        // specific sites — the stop/pause pass while moving (1893/1900),
        // a controller that stops a jog on its own (1357-1364, 9c8a8e8c),
        // home completion (postProcess 893-906), and limit/LVIO handling.
        // A jog button latched through SPMG=Pause must survive its stop
        // completion so Go can resume the jog (C 1916-1918).
        // Sync last values
        self.internal.lval = self.pos.val;
        self.internal.ldvl = self.pos.dval;
        self.internal.lrvl = self.pos.rval;
        // SPMG::Move one-shot: restore to Pause after completion
        if self.ctrl.spmg == SpmgMode::Move {
            self.ctrl.spmg = SpmgMode::Pause;
            self.internal.lspg = SpmgMode::Pause;
        }
    }

    /// C overtaken-target replay (motorRecord.cc:1383-1397): at a motion
    /// completion, `val != lval` means a position was written mid-motion
    /// on a path that could not dispatch it (jog in flight, home in
    /// flight, jog backlash — handle_retarget ignores those). C collapses
    /// MIP to DONE and re-enters do_work so the move block fires toward
    /// the new target instead of postProcess syncing it away. Commanded
    /// stops are excluded by the gate (`!(mip & MIP_STOP)`,
    /// `!(mip & MIP_JOG_STOP)`, 1385-1386) — the callers encode that by
    /// only invoking this from the sudden-jog-stop, home-done and
    /// jog-backlash-done branches. Returns true when the write was
    /// replayed (a sub-step replay quiesces inside plan_absolute_move).
    fn replay_overtaken_target(&mut self, effects: &mut ProcessEffects) -> bool {
        if self.pos.val == self.internal.lval {
            return false;
        }
        self.stat.mip = MipFlags::empty();
        self.set_phase(MotionPhase::Idle);
        self.plan_absolute_move(effects);
        effects.suppress_forward_link = true;
        true
    }

    /// If a same-direction retarget was armed during motion, verify on
    /// completion that the driver actually reached the new target. If it
    /// did not (off by more than one motor step), replan once — bypassing
    /// RTRY/RDBD retry gating. Returns true if a replan was emitted and
    /// the caller should stop further evaluation for this cycle.
    fn check_retarget_verification(&mut self, effects: &mut ProcessEffects) -> bool {
        if !self.internal.verify_retarget_on_completion {
            return false;
        }
        // One-shot: clear the flag before any early return so we never
        // replan twice from the same arm.
        self.internal.verify_retarget_on_completion = false;
        if self.conv.mres == 0.0 {
            return false;
        }
        // Step-based diff: matches plan_absolute_move's too_small check.
        // Round each position to its motor step first, then compare —
        // not round(diff) — so rounding boundaries are consistent with
        // the move-planning gate.
        let npos = (self.pos.dval / self.conv.mres).round() as i64;
        let rpos = (self.pos.drbv / self.conv.mres).round() as i64;
        if (npos - rpos).abs() < 1 {
            return false;
        }
        self.plan_absolute_move(effects);
        effects.suppress_forward_link = true;
        true
    }

    /// Set motion phase with tracing.
    pub(crate) fn set_phase(&mut self, new_phase: MotionPhase) {
        tracing::debug!("phase transition: {:?} -> {:?}", self.stat.phase, new_phase);
        self.stat.phase = new_phase;
    }

    /// Evaluate position error after DLY expires (C: maybeRetry after delay).
    /// Same as evaluate_position_error but finalizes directly (no re-delay).
    fn evaluate_position_error_after_delay(&mut self, effects: &mut ProcessEffects) {
        if self.check_retarget_verification(effects) {
            return;
        }

        let diff = (self.pos.dval - self.pos.drbv).abs();

        // C: compute user_cdir for retry direction check with mapped limit switches
        let same_polarity = (self.conv.dir == MotorDir::Pos) == (self.conv.mres >= 0.0);
        let user_cdir = if same_polarity {
            self.stat.cdir
        } else {
            !self.stat.cdir
        };
        let ls_blocks_retry = (self.limits.hls && user_cdir) || (self.limits.lls && !user_cdir);

        // C `maybeRetry` (motorRecord.cc:1049): the retry comparison is
        // `fabs(pmr->diff) >= pmr->rdbd` — inclusive of the boundary.
        // At `diff == rdbd` C retries; an earlier Rust version used `>`
        // and finalized instead. Retry is gated only on `rtry != 0`
        // (`rcnt < rtry` here); C `maybeRetry` has NO `rdbd > 0`
        // condition — RDBD is kept >= |MRES| by `enforceMinRetryDeadband`,
        // and when RDBD is 0 `fabs(diff) >= 0` is trivially true.
        if diff >= self.retry.rdbd && self.retry.rcnt < self.retry.rtry && !ls_blocks_retry {
            if self.retry.rmod == RetryMode::InPosition {
                // C: InPosition mode re-delays to let servo settle
                self.retry.rcnt += 1;
                self.retry.miss = false;
                self.stat.mip = MipFlags::RETRY;
                self.finalize_or_delay(effects);
                return;
            }

            self.retry.rcnt += 1;
            self.retry.miss = false;
            self.set_phase(MotionPhase::Retry);
            self.stat.mip = MipFlags::RETRY;

            let frac = self.retry.frac;
            if self.use_relative_moves() {
                // C use_rel retry: position = relpos * frac, where relpos is
                // the RMOD-scaled remaining distance (compute_retry_target).
                let retry_target = self.compute_retry_target();
                let rel_distance = (retry_target - self.pos.drbv) * frac;
                effects.commands.push(MotorCommand::MoveRelative {
                    distance: rel_distance,
                    velocity: self.vel.velo,
                    acceleration: self.move_accel_egu(),
                });
            } else {
                // C absolute retry: position = currpos + frac*(newpos-currpos)
                // with currpos = ldvl/mres and newpos = dval/mres. The prior
                // move's load_pos set ldvl = dval, so currpos == newpos and
                // position collapses to dval. RMOD scaling never reaches the
                // absolute path in C — it only scales relpos (use_rel).
                effects.commands.push(MotorCommand::MoveAbsolute {
                    position: self.pos.dval,
                    velocity: self.vel.velo,
                    acceleration: self.move_accel_egu(),
                });
            }
            effects.request_poll = true;
            effects.suppress_forward_link = true;
        } else {
            // C `maybeRetry`: MISS latches when the axis finalizes with
            // `fabs(diff) >= rdbd` (retries exhausted / disabled but not
            // in position). Boundary-inclusive, matching C line 1049.
            if diff >= self.retry.rdbd {
                self.retry.miss = true;
            }
            self.finalize_motion(effects);
        }
    }

    /// Evaluate position error after motion completes.
    fn evaluate_position_error(&mut self, effects: &mut ProcessEffects) {
        let diff = (self.pos.dval - self.pos.drbv).abs();

        // Safety net: if a same-direction retarget happened during this
        // motion, verify the driver reached the new target. If not, replan
        // once — independent of retry settings. Keeps retarget correctness
        // from depending on driver-specific in-flight retarget support.
        if self.check_retarget_verification(effects) {
            return;
        }

        // C: compute user_cdir for retry direction check with mapped limit switches
        let same_polarity = (self.conv.dir == MotorDir::Pos) == (self.conv.mres >= 0.0);
        let user_cdir = if same_polarity {
            self.stat.cdir
        } else {
            !self.stat.cdir
        };
        let ls_blocks_retry = (self.limits.hls && user_cdir) || (self.limits.lls && !user_cdir);

        // C `maybeRetry` (motorRecord.cc:1049): `fabs(pmr->diff) >= pmr->rdbd`
        // — boundary-inclusive. Retry gated only on `rtry != 0`
        // (`rcnt < rtry`); C has no `rdbd > 0` condition.
        if diff >= self.retry.rdbd && self.retry.rcnt < self.retry.rtry && !ls_blocks_retry {
            // InPosition mode: don't reissue, just finalize
            if self.retry.rmod == RetryMode::InPosition {
                self.finalize_or_delay(effects);
                return;
            }

            // Retry
            self.retry.rcnt += 1;
            self.retry.miss = false;
            self.set_phase(MotionPhase::Retry);
            self.stat.mip = MipFlags::RETRY;

            let frac = self.retry.frac;
            if self.use_relative_moves() {
                // C use_rel retry: position = relpos * frac, where relpos is
                // the RMOD-scaled remaining distance (compute_retry_target).
                let retry_target = self.compute_retry_target();
                let rel_distance = (retry_target - self.pos.drbv) * frac;
                effects.commands.push(MotorCommand::MoveRelative {
                    distance: rel_distance,
                    velocity: self.vel.velo,
                    acceleration: self.move_accel_egu(),
                });
            } else {
                // C absolute retry: currpos == newpos == dval (the prior
                // move's load_pos set ldvl = dval), so position is dval.
                // RMOD scaling applies only to the use_rel path.
                effects.commands.push(MotorCommand::MoveAbsolute {
                    position: self.pos.dval,
                    velocity: self.vel.velo,
                    acceleration: self.move_accel_egu(),
                });
            }
            effects.request_poll = true;
            effects.suppress_forward_link = true;
        } else {
            // C `maybeRetry`: MISS latches when finalizing with
            // `fabs(diff) >= rdbd`. Boundary-inclusive, matching C line 1049.
            if diff >= self.retry.rdbd {
                self.retry.miss = true;
            }
            self.finalize_or_delay(effects);
        }
    }

    /// Compute retry target based on retry mode.
    /// Matches C motorRecord.cc do_work() retry logic.
    fn compute_retry_target(&self) -> f64 {
        match self.retry.rmod {
            RetryMode::Default => {
                // C default: move to the original target position (dval)
                self.pos.dval
            }
            RetryMode::Arithmetic => {
                // C: relpos *= (rtry - rcnt + 1) / rtry
                // relpos is the remaining distance from current position to target
                let relpos = self.pos.dval - self.pos.drbv;
                let rtry = self.retry.rtry as f64;
                let rcnt = self.retry.rcnt as f64;
                let factor = if rtry > 0.0 {
                    (rtry - rcnt + 1.0) / rtry
                } else {
                    1.0
                };
                self.pos.drbv + relpos * factor
            }
            RetryMode::Geometric => {
                // C: relpos *= 1 / (2 ^ (rcnt - 1))
                let relpos = self.pos.dval - self.pos.drbv;
                let power = (self.retry.rcnt - 1).max(0) as u32;
                let factor = 1.0 / (2.0_f64.powi(power as i32));
                self.pos.drbv + relpos * factor
            }
            RetryMode::InPosition => {
                // InPosition: don't reissue move, just wait for driver
                self.pos.dval
            }
        }
    }

    /// Check if backlash correction is needed for a move from current position to dval.
    /// Backlash is needed when the direction of travel opposes the BDST sign direction.
    pub(crate) fn needs_backlash_for_move(&self, dval: f64, drbv: f64) -> bool {
        if self.retry.bdst == 0.0 {
            return false;
        }
        // C: disable backlash when |BDST| < |MRES| (less than one step)
        if self.retry.bdst.abs() < self.conv.mres.abs() {
            return false;
        }
        let move_direction = dval - drbv;
        if move_direction == 0.0 {
            return false;
        }
        // Need backlash if move direction opposes BDST sign
        // (i.e., approaching target from the wrong side)
        (move_direction > 0.0) != (self.retry.bdst > 0.0)
    }

    /// Compute the backlash pre-target position.
    /// The pre-target overshoots past dval so the final approach comes from the BDST direction.
    pub(crate) fn compute_backlash_pretarget(dval: f64, bdst: f64) -> f64 {
        dval - bdst
    }

    /// Check if jog backlash is needed.
    /// C: jog backlash is performed unconditionally when |BDST| >= |MRES|.
    fn needs_jog_backlash(&self) -> bool {
        self.retry.bdst != 0.0 && self.retry.bdst.abs() >= self.conv.mres.abs()
    }

    /// Start backlash final approach (move from pretarget to dval).
    fn start_backlash_final(&mut self, effects: &mut ProcessEffects) {
        self.internal.backlash_pending = false;
        self.set_phase(MotionPhase::BacklashFinal);
        self.stat.mip = MipFlags::MOVE_BL;
        let frac = self.retry.frac;
        if self.use_relative_moves() {
            // C relative: relpos = (dval - drbv) * frac / mres
            let rel_distance = (self.pos.dval - self.pos.drbv) * frac;
            effects.commands.push(MotorCommand::MoveRelative {
                distance: rel_distance,
                velocity: self.vel.bvel,
                acceleration: self.backlash_accel_egu(),
            });
        } else {
            // C absolute: position = pretarget + frac * (dval - pretarget)
            // = (dval - bdst) + frac * bdst = dval - bdst*(1-frac)
            let pretarget = Self::compute_backlash_pretarget(self.pos.dval, self.retry.bdst);
            let position = pretarget + frac * (self.pos.dval - pretarget);
            effects.commands.push(MotorCommand::MoveAbsolute {
                position,
                velocity: self.vel.bvel,
                acceleration: self.backlash_accel_egu(),
            });
        }
        effects.request_poll = true;
        effects.suppress_forward_link = true;
    }

    /// Start jog backlash correction (phase 1: move to pretarget at slew velocity).
    /// C has two phases: BL1 moves to (dval - bdst) at slew vel, BL2 moves to dval at backlash vel.
    fn start_jog_backlash(&mut self, effects: &mut ProcessEffects) {
        // dval was synced to drbv by sync_positions() above
        // Phase 1 (BL1): move to backlash pretarget (dval - bdst) at slew velocity
        let pretarget = self.pos.dval - self.retry.bdst;
        self.set_phase(MotionPhase::JogBacklash);
        self.stat.mip = MipFlags::JOG_BL1;
        self.internal.backlash_pending = true;
        if self.use_relative_moves() {
            effects.commands.push(MotorCommand::MoveRelative {
                distance: pretarget - self.pos.drbv,
                velocity: self.vel.velo,
                acceleration: self.move_accel_egu(),
            });
        } else {
            effects.commands.push(MotorCommand::MoveAbsolute {
                position: pretarget,
                velocity: self.vel.velo,
                acceleration: self.move_accel_egu(),
            });
        }
        effects.request_poll = true;
        effects.suppress_forward_link = true;
    }

    /// Start jog backlash phase 2 (final approach at backlash velocity).
    fn start_jog_backlash_final(&mut self, effects: &mut ProcessEffects) {
        let frac = self.retry.frac;
        self.stat.mip = MipFlags::JOG_BL2;
        self.internal.backlash_pending = false;
        let pretarget = Self::compute_backlash_pretarget(self.pos.dval, self.retry.bdst);
        if self.use_relative_moves() {
            let rel_distance = (self.pos.dval - self.pos.drbv) * frac;
            effects.commands.push(MotorCommand::MoveRelative {
                distance: rel_distance,
                velocity: self.vel.bvel,
                acceleration: self.backlash_accel_egu(),
            });
        } else {
            let position = pretarget + frac * (self.pos.dval - pretarget);
            effects.commands.push(MotorCommand::MoveAbsolute {
                position,
                velocity: self.vel.bvel,
                acceleration: self.backlash_accel_egu(),
            });
        }
        effects.request_poll = true;
        effects.suppress_forward_link = true;
    }
}
