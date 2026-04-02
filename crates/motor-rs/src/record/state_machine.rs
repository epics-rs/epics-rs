use super::*;

impl MotorRecord {
    /// Check if motion has completed and handle post-motion pipeline.
    pub fn check_completion(&mut self) -> ProcessEffects {
        let mut effects = ProcessEffects::default();

        let driver_done =
            self.stat.msta.contains(MstaFlags::DONE) && !self.stat.msta.contains(MstaFlags::MOVING);

        if !driver_done {
            // Still moving, keep polling
            effects.request_poll = true;
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
            } else {
                // Plain stop -- finalize
                self.finalize_or_delay(&mut effects);
                return effects;
            }
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
                if self.needs_jog_backlash() {
                    self.start_jog_backlash(&mut effects);
                } else {
                    self.finalize_or_delay(&mut effects);
                }
            }
            MotionPhase::JogBacklash => {
                self.finalize_or_delay(&mut effects);
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
                // Already idle, nothing to do
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
        self.suppress_flnk = false;
        self.retry.rcnt = 0;
        self.internal.backlash_pending = false;
        self.internal.pending_retarget = None;
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

    /// Set motion phase with tracing.
    pub(crate) fn set_phase(&mut self, new_phase: MotionPhase) {
        tracing::debug!("phase transition: {:?} -> {:?}", self.stat.phase, new_phase);
        self.stat.phase = new_phase;
    }

    /// Evaluate position error after motion completes.
    fn evaluate_position_error(&mut self, effects: &mut ProcessEffects) {
        let diff = (self.pos.dval - self.pos.drbv).abs();

        if diff > self.retry.rdbd && self.retry.rcnt < self.retry.rtry && self.retry.rdbd > 0.0 {
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

            let retry_target = self.compute_retry_target();
            effects.commands.push(MotorCommand::MoveAbsolute {
                position: retry_target,
                velocity: self.vel.velo,
                acceleration: self.vel.accl,
            });
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
    fn compute_retry_target(&self) -> f64 {
        let diff = self.pos.dval - self.pos.drbv;
        match self.retry.rmod {
            RetryMode::Default => {
                // C default: move to drbv + rdbd in direction of error
                self.pos.drbv + self.retry.rdbd * diff.signum()
            }
            RetryMode::Arithmetic => {
                let correction = diff * self.retry.frac;
                self.pos.drbv + correction
            }
            RetryMode::Geometric => self.pos.dval,
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
    fn needs_jog_backlash(&self) -> bool {
        if self.retry.bdst == 0.0 {
            return false;
        }
        // JOG backlash when direction was opposite to BDST direction
        let jog_was_forward = self.stat.mip.contains(MipFlags::JOGF);
        let bdst_positive = self.retry.bdst > 0.0;
        !jog_was_forward && bdst_positive || jog_was_forward && !bdst_positive
    }

    /// Start backlash final approach (move from pretarget to dval).
    fn start_backlash_final(&mut self, effects: &mut ProcessEffects) {
        self.internal.backlash_pending = false;
        self.set_phase(MotionPhase::BacklashFinal);
        self.stat.mip = MipFlags::MOVE_BL;
        effects.commands.push(MotorCommand::MoveAbsolute {
            position: self.pos.dval,
            velocity: self.vel.bvel,
            acceleration: self.vel.bacc,
        });
        effects.request_poll = true;
        effects.suppress_forward_link = true;
    }

    /// Start jog backlash correction.
    fn start_jog_backlash(&mut self, effects: &mut ProcessEffects) {
        let target = self.pos.drbv + self.retry.bdst;
        self.set_phase(MotionPhase::JogBacklash);
        self.stat.mip.insert(MipFlags::JOG_BL1);
        effects.commands.push(MotorCommand::MoveAbsolute {
            position: target,
            velocity: self.vel.bvel,
            acceleration: self.vel.bacc,
        });
        effects.request_poll = true;
        effects.suppress_forward_link = true;
    }
}
