use super::*;

impl MotorRecord {
    /// Check if motion has completed and handle post-motion pipeline.
    pub fn check_completion(&mut self) -> ProcessEffects {
        let mut effects = ProcessEffects::default();

        // C: db5da2f0 — if the external URIP readback is in error while a
        // motion is in progress, stop the axis immediately. New moves are
        // already blocked at plan_motion.
        if self.conv.urip
            && self.conv.rdbl_error
            && self.stat.phase != MotionPhase::Idle
            && !self.stat.mip.contains(MipFlags::STOP)
        {
            self.stat.mip.insert(MipFlags::STOP);
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
            // replayed — an active jog/home that a plain STOP just halted is
            // NOT, even though its JOGF/HOMF MIP bit is still set.
            if let Some(queued) = self.internal.queued_motion.take() {
                self.stat.mip.remove(MipFlags::STOP);
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
            // Plain stop -- sync target to readback then finalize
            // C: postProcess syncs VAL<-RBV, DVAL<-DRBV after stop
            self.sync_positions();
            self.finalize_or_delay(&mut effects);
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
                    // BL2 complete -> finalize
                    self.finalize_or_delay(&mut effects);
                }
            }
            MotionPhase::Homing => {
                self.stat.athm = true;
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
        self.suppress_flnk = false;
        self.retry.rcnt = 0;
        self.internal.backlash_pending = false;
        self.internal.pending_retarget = None;
        self.internal.queued_motion = None;
        self.internal.verify_retarget_on_completion = false;
        // C: 9c8a8e8c (PR #56) — clear motion buttons. When the controller
        // stops on its own (internal limit, fault, host-issued stop) the
        // JOGF/JOGR/HOMF/HOMR buttons must reset, otherwise the next process
        // pass re-fires the motion.
        self.ctrl.jogf = false;
        self.ctrl.jogr = false;
        self.ctrl.homf = false;
        self.ctrl.homr = false;
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

        if diff > self.retry.rdbd
            && self.retry.rcnt < self.retry.rtry
            && self.retry.rdbd > 0.0
            && !ls_blocks_retry
        {
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
            if diff > self.retry.rdbd && self.retry.rdbd > 0.0 {
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

        if diff > self.retry.rdbd
            && self.retry.rcnt < self.retry.rtry
            && self.retry.rdbd > 0.0
            && !ls_blocks_retry
        {
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
            if diff > self.retry.rdbd && self.retry.rdbd > 0.0 {
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
