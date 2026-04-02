use super::*;

impl MotorRecord {
    /// Plan and start a motion from a user write.
    pub fn plan_motion(&mut self, src: CommandSource) -> ProcessEffects {
        let mut effects = ProcessEffects::default();

        // SPMG, STOP, and SYNC always processed regardless of command gate
        match src {
            CommandSource::Spmg
            | CommandSource::Stop
            | CommandSource::Sync
            | CommandSource::Set
            | CommandSource::Cnen => {}
            _ => {
                if !self.can_accept_command() {
                    return effects;
                }
            }
        }

        match src {
            CommandSource::Val | CommandSource::Dval | CommandSource::Rval => {
                // Check for retarget if motion is in progress
                if self.stat.phase != MotionPhase::Idle {
                    let action = self.handle_retarget(self.pos.dval);
                    match action {
                        RetargetAction::Ignore => {
                            return effects;
                        }
                        RetargetAction::StopAndReplan => {
                            // Cancel any pending backlash/retry state
                            self.internal.backlash_pending = false;
                            self.retry.rcnt = 0;
                            self.internal.pending_retarget = Some(self.pos.dval);
                            self.stat.mip.insert(MipFlags::STOP);
                            effects.commands.push(MotorCommand::Stop {
                                acceleration: self.vel.accl,
                            });
                            effects.request_poll = true;
                            effects.suppress_forward_link = true;
                            return effects;
                        }
                        RetargetAction::ExtendMove => {
                            // Cancel any pending backlash/retry state, issue new move
                            self.internal.backlash_pending = false;
                            self.retry.rcnt = 0;
                            // Re-evaluate backlash for new target
                            let backlash =
                                self.needs_backlash_for_move(self.pos.dval, self.pos.drbv);
                            let move_target = if backlash {
                                Self::compute_backlash_pretarget(self.pos.dval, self.retry.bdst)
                            } else {
                                self.pos.dval
                            };
                            self.internal.backlash_pending = backlash;
                            self.stat.tdir = move_target > self.pos.drbv;
                            self.internal.ldvl = self.pos.dval;
                            effects.commands.push(MotorCommand::MoveAbsolute {
                                position: move_target,
                                velocity: self.vel.velo,
                                acceleration: self.vel.accl,
                            });
                            effects.request_poll = true;
                            effects.suppress_forward_link = true;
                            return effects;
                        }
                    }
                }
                self.plan_absolute_move(&mut effects);
            }
            CommandSource::Rlv => {
                // Relative move: VAL += RLV
                self.pos.val += self.pos.rlv;
                self.pos.rlv = 0.0;
                // Cascade from VAL
                if let Ok((dval, rval, off)) = coordinate::cascade_from_val(
                    self.pos.val,
                    self.conv.dir,
                    self.pos.off,
                    self.conv.foff,
                    self.conv.mres,
                    false,
                    self.pos.dval,
                ) {
                    self.pos.dval = dval;
                    self.pos.rval = rval;
                    self.pos.off = off;
                }
                self.plan_absolute_move(&mut effects);
            }
            CommandSource::Stop => {
                self.handle_stop(&mut effects);
            }
            CommandSource::Jogf | CommandSource::Jogr => {
                let forward = src == CommandSource::Jogf;
                let starting = if forward {
                    self.ctrl.jogf
                } else {
                    self.ctrl.jogr
                };
                if starting {
                    self.start_jog(forward, &mut effects);
                } else {
                    self.stop_jog(&mut effects);
                }
            }
            CommandSource::Homf | CommandSource::Homr => {
                let forward = src == CommandSource::Homf;
                self.start_home(forward, &mut effects);
            }
            CommandSource::Twf | CommandSource::Twr => {
                let forward = src == CommandSource::Twf;
                self.handle_tweak(forward, &mut effects);
            }
            CommandSource::Spmg => {
                self.handle_spmg_change(&mut effects);
            }
            CommandSource::Sync => {
                self.sync_positions();
            }
            CommandSource::Set => {
                // SET mode: recalculate RBV from new offset, then issue SetPosition
                self.pos.rbv = coordinate::dial_to_user(self.pos.drbv, self.conv.dir, self.pos.off);
                self.pos.diff = self.pos.dval - self.pos.drbv;
                self.pos.rdif = self.pos.val - self.pos.rbv;
                let raw_pos = self.pos.dval;
                effects
                    .commands
                    .push(MotorCommand::SetPosition { position: raw_pos });
            }
            CommandSource::Cnen => {
                effects.commands.push(MotorCommand::SetClosedLoop {
                    enable: self.ctrl.cnen,
                });
            }
        }

        effects
    }

    /// Plan an absolute move to current DVAL.
    pub(crate) fn plan_absolute_move(&mut self, effects: &mut ProcessEffects) {
        // Check soft limits
        if coordinate::check_soft_limits(self.pos.dval, self.limits.dhlm, self.limits.dllm) {
            self.limits.lvio = true;
            tracing::warn!(
                "limit violation: dval={:.4}, limits=[{:.4}, {:.4}]",
                self.pos.dval,
                self.limits.dllm,
                self.limits.dhlm
            );
            return;
        }
        self.limits.lvio = false;

        // SPDB deadband: suppress move if already within setpoint deadband
        if self.retry.spdb > 0.0 && (self.pos.dval - self.pos.drbv).abs() <= self.retry.spdb {
            return;
        }

        // Determine if backlash correction is needed
        let backlash = self.needs_backlash_for_move(self.pos.dval, self.pos.drbv);

        // Compute move target: pretarget if backlash, otherwise dval
        let move_target = if backlash {
            Self::compute_backlash_pretarget(self.pos.dval, self.retry.bdst)
        } else {
            self.pos.dval
        };

        // Check hardware limits based on first move direction
        let dir = if move_target > self.pos.drbv {
            MotionDirection::Positive
        } else {
            MotionDirection::Negative
        };
        if self.is_blocked_by_hw_limit(dir) {
            tracing::warn!("hardware limit active, blocking {dir:?} move");
            return;
        }

        // DMOV pulse: set false before starting
        self.stat.dmov = false;
        self.suppress_flnk = true;
        self.retry.rcnt = 0;
        self.retry.miss = false;

        // tdir reflects the actual first-command direction
        self.stat.tdir = move_target > self.pos.drbv;

        // Set MIP and phase
        self.stat.mip = MipFlags::MOVE;
        self.set_phase(MotionPhase::MainMove);
        self.internal.backlash_pending = backlash;

        effects.commands.push(MotorCommand::MoveAbsolute {
            position: move_target,
            velocity: self.vel.velo,
            acceleration: self.vel.accl,
        });
        effects.request_poll = true;
        effects.suppress_forward_link = true;
    }

    /// Handle STOP command.
    fn handle_stop(&mut self, effects: &mut ProcessEffects) {
        self.ctrl.stop = false; // pulse field
        if self.stat.phase != MotionPhase::Idle {
            self.stat.mip.insert(MipFlags::STOP);
            self.internal.backlash_pending = false;
            self.internal.pending_retarget = None;
            effects.commands.push(MotorCommand::Stop {
                acceleration: self.vel.accl,
            });
            // Sync VAL to RBV after stop
            self.pos.val = self.pos.rbv;
            self.pos.dval = self.pos.drbv;
            self.pos.rval = self.pos.rrbv;
        }
    }

    /// Start jogging.
    fn start_jog(&mut self, forward: bool, effects: &mut ProcessEffects) {
        let dir = if forward {
            MotionDirection::Positive
        } else {
            MotionDirection::Negative
        };
        if self.is_blocked_by_hw_limit(dir) {
            return;
        }

        self.stat.dmov = false;
        self.suppress_flnk = true;

        if forward {
            self.stat.mip = MipFlags::JOGF;
        } else {
            self.stat.mip = MipFlags::JOGR;
        }
        self.set_phase(MotionPhase::Jog);

        effects.commands.push(MotorCommand::MoveVelocity {
            direction: forward,
            velocity: self.vel.jvel,
            acceleration: self.vel.jar,
        });
        effects.request_poll = true;
        effects.suppress_forward_link = true;
    }

    /// Stop jogging.
    fn stop_jog(&mut self, effects: &mut ProcessEffects) {
        self.stat.mip.insert(MipFlags::JOG_STOP);
        self.set_phase(MotionPhase::JogStopping);
        effects.commands.push(MotorCommand::Stop {
            acceleration: if self.vel.jar > 0.0 {
                self.vel.jar
            } else {
                self.vel.accl
            },
        });
    }

    /// Start homing.
    fn start_home(&mut self, forward: bool, effects: &mut ProcessEffects) {
        self.stat.dmov = false;
        self.suppress_flnk = true;

        if forward {
            self.stat.mip = MipFlags::HOMF;
            self.ctrl.homf = false; // pulse
        } else {
            self.stat.mip = MipFlags::HOMR;
            self.ctrl.homr = false; // pulse
        }
        self.set_phase(MotionPhase::Homing);

        effects.commands.push(MotorCommand::Home {
            forward,
            velocity: self.vel.hvel,
            acceleration: self.vel.accl,
        });
        effects.request_poll = true;
        effects.suppress_forward_link = true;
    }

    /// Handle tweak (TWF/TWR).
    fn handle_tweak(&mut self, forward: bool, effects: &mut ProcessEffects) {
        if forward {
            self.ctrl.twf = false; // pulse
        } else {
            self.ctrl.twr = false; // pulse
        }

        let dir = if forward {
            MotionDirection::Positive
        } else {
            MotionDirection::Negative
        };
        if self.is_blocked_by_hw_limit(dir) {
            return;
        }

        let delta = if forward {
            self.ctrl.twv
        } else {
            -self.ctrl.twv
        };
        self.pos.val += delta;

        // Cascade from VAL
        if let Ok((dval, rval, off)) = coordinate::cascade_from_val(
            self.pos.val,
            self.conv.dir,
            self.pos.off,
            self.conv.foff,
            self.conv.mres,
            false,
            self.pos.dval,
        ) {
            self.pos.dval = dval;
            self.pos.rval = rval;
            self.pos.off = off;
        }

        self.plan_absolute_move(effects);
    }

    /// Handle SPMG mode change.
    fn handle_spmg_change(&mut self, effects: &mut ProcessEffects) {
        let old = self.internal.lspg;
        let new = self.ctrl.spmg;
        self.internal.lspg = new;

        match new {
            SpmgMode::Stop => {
                if self.stat.phase != MotionPhase::Idle {
                    self.internal.backlash_pending = false;
                    self.internal.pending_retarget = None;
                    effects.commands.push(MotorCommand::Stop {
                        acceleration: self.vel.accl,
                    });
                    // Sync VAL = RBV
                    self.pos.val = self.pos.rbv;
                    self.pos.dval = self.pos.drbv;
                    self.pos.rval = self.pos.rrbv;
                    self.finalize_motion(effects);
                }
            }
            SpmgMode::Pause => {
                if self.stat.phase != MotionPhase::Idle {
                    self.internal.backlash_pending = false;
                    effects.commands.push(MotorCommand::Stop {
                        acceleration: self.vel.accl,
                    });
                    // Keep target (DVAL preserved) for potential resume via Go
                    self.set_phase(MotionPhase::Idle);
                    self.stat.mip = MipFlags::empty();
                    self.stat.dmov = true;
                    self.suppress_flnk = false;
                }
            }
            SpmgMode::Go => {
                // Resume: if coming from Pause and there's a saved target, replan
                if matches!(old, SpmgMode::Pause) && self.stat.phase == MotionPhase::Idle {
                    if (self.pos.dval - self.pos.drbv).abs() > self.retry.rdbd.max(1e-12) {
                        self.plan_absolute_move(effects);
                    }
                }
            }
            SpmgMode::Move => {
                // One-shot: like Go but will restore to Pause after completion
                if matches!(old, SpmgMode::Pause | SpmgMode::Stop)
                    && self.stat.phase == MotionPhase::Idle
                {
                    if (self.pos.dval - self.pos.drbv).abs() > self.retry.rdbd.max(1e-12) {
                        self.plan_absolute_move(effects);
                    }
                }
            }
        }
    }

    /// Handle retarget (NTM) -- new target while moving.
    pub fn handle_retarget(&mut self, new_dval: f64) -> RetargetAction {
        if !self.timing.ntm {
            return RetargetAction::Ignore;
        }

        let _deadband = self.timing.ntmf * (self.retry.bdst.abs() + self.retry.rdbd);
        let old_dval = self.internal.ldvl;
        let direction_changed =
            (new_dval - self.pos.drbv).signum() != (old_dval - self.pos.drbv).signum();

        if direction_changed {
            RetargetAction::StopAndReplan
        } else if (new_dval - self.pos.drbv).abs() < (old_dval - self.pos.drbv).abs() {
            RetargetAction::StopAndReplan
        } else {
            RetargetAction::ExtendMove
        }
    }

    /// Check if a new command can be accepted.
    pub fn can_accept_command(&self) -> bool {
        matches!(self.ctrl.spmg, SpmgMode::Go | SpmgMode::Move)
    }

    /// Check if a hardware limit blocks motion in the given direction.
    fn is_blocked_by_hw_limit(&self, dir: MotionDirection) -> bool {
        match dir {
            MotionDirection::Positive => self.limits.hls,
            MotionDirection::Negative => self.limits.lls,
        }
    }

    /// Process the motor record (called by EPICS record support).
    pub fn do_process(&mut self) -> ProcessEffects {
        // STUP: one-shot status refresh
        if self.stat.stup > 0 {
            self.stat.stup = 0;
            let mut effects = ProcessEffects::default();
            effects.status_refresh = true;
            return effects;
        }

        let event = self.pending_event.take();
        let src = self.last_write.take();

        // User write takes priority: if a field was put while a poll
        // update arrived, handle the write first. The poll status was
        // already applied in determine_event() for Idle phase.
        if let Some(src) = src {
            // If there was also a DeviceUpdate, apply it first so
            // plan_motion sees the latest readback.
            if let Some(MotorEvent::DeviceUpdate(status)) = &event {
                self.process_motor_info(status);
            }
            return self.plan_motion(src);
        }

        match event {
            Some(MotorEvent::Startup) => {
                // Handled by device support init
                ProcessEffects::default()
            }
            Some(MotorEvent::UserWrite(cmd_src)) => self.plan_motion(cmd_src),
            Some(MotorEvent::DeviceUpdate(status)) => {
                self.process_motor_info(&status);
                self.check_completion()
            }
            Some(MotorEvent::DelayExpired) => {
                let mut effects = ProcessEffects::default();
                self.finalize_motion(&mut effects);
                effects
            }
            None => ProcessEffects::default(),
        }
    }
}
