use super::*;

impl MotorRecord {
    /// Plan and start a motion from a user write.
    pub fn plan_motion(&mut self, src: CommandSource) -> ProcessEffects {
        // Reset DMOV notification flag so the upcoming DMOV 1→0 transition
        // fires AsyncPendingNotify. Without this, back-to-back motions
        // (previous done + new write in same process cycle) would skip
        // the notification because dmov_notified was still true from the
        // previous motion.
        self.internal.dmov_notified = false;
        let mut effects = ProcessEffects::default();

        // C do_work evaluates `pmr->stop` and `spmg != lspg` at the TOP
        // of every put-processing pass, before any other work
        // (motorRecord.cc:1855-1932). A latent stop or SPMG change — a
        // bare dbPut that never processed the record, later overtaken in
        // `last_write` by another field's write — therefore wins over
        // whatever src triggered this pass. The overtaken position write
        // is not replayed: C discards it at stop completion via the
        // `(val != lval) && !(mip & MIP_STOP)` gate (motorRecord.cc:1385)
        // followed by the postProcess VAL<-RBV sync, which the Rust
        // stop-completion path mirrors.
        //
        // When a stop pulse rides the same pass as an SPMG change, C
        // consumes both at once (1858: lspg <- spmg right before the stop
        // branch) — sync LSPG whenever this pass takes a stop path, so an
        // overtaken Pause→Go is not replayed as a resume on a later pass.
        // (This includes src == Spmg with a latent stop: the stop gate
        // below wins the pass and the Spmg arm never runs, like C's
        // stop_or_pause return skipping the Go branch.)
        if self.ctrl.spmg != self.internal.lspg && (self.ctrl.stop || src == CommandSource::Stop) {
            self.internal.lspg = self.ctrl.spmg;
        }
        if self.ctrl.stop && src != CommandSource::Stop {
            self.handle_stop(&mut effects);
            return effects;
        }

        // Latent SPMG change with no stop pulse: run the C top block's
        // SPMG handling now. Stop/Pause halt the axis and return like C;
        // a Go/Move transition falls through to normal dispatch (C falls
        // through to the rest of do_work) unless the transition itself
        // resumed motion — that resume already targets the freshest DVAL,
        // so dispatching src as well would double-plan. For the
        // housekeeping srcs (Sync/Set/Cnen/PcoEnable) the gate defers to
        // the next pass instead: their arms map to C special()-time
        // actions that precede do_work, and a Go-resume must not swallow
        // or reorder them.
        if self.ctrl.spmg != self.internal.lspg && src != CommandSource::Spmg {
            match src {
                CommandSource::Sync
                | CommandSource::Set
                | CommandSource::Cnen
                | CommandSource::PcoEnable => {}
                _ => {
                    self.handle_spmg_change(&mut effects);
                    match self.ctrl.spmg {
                        SpmgMode::Stop | SpmgMode::Pause => return effects,
                        SpmgMode::Go | SpmgMode::Move => {
                            if !effects.commands.is_empty() {
                                return effects;
                            }
                        }
                    }
                }
            }
        }

        // SPMG, STOP, and SYNC always processed regardless of command gate
        match src {
            CommandSource::Spmg
            | CommandSource::Stop
            | CommandSource::Sync
            | CommandSource::Set
            | CommandSource::Cnen
            | CommandSource::PcoEnable => {}
            _ => {
                if !self.can_accept_command() {
                    return effects;
                }
                // C: db5da2f0 + 7493d50b — when URIP=Yes and the external
                // RDBL link is in error, refuse to start a new motion. If a
                // motion is in flight, emit a STOP so the axis halts.
                if self.conv.urip && self.conv.rdbl_error {
                    if self.stat.phase != MotionPhase::Idle {
                        self.stat.mip.insert(MipFlags::STOP);
                        effects.commands.push(MotorCommand::Stop {
                            acceleration: self.move_accel_egu(),
                        });
                    }
                    return effects;
                }
            }
        }

        // C: 0aaf02d7 (2025-02 PR #224) — if a VAL/DVAL/RVAL/RLV write
        // arrives while a home is in progress, clear the HOMF/HOMR buttons.
        // Without this, the next do_work pass re-issues HOME and loops.
        if matches!(
            src,
            CommandSource::Val | CommandSource::Dval | CommandSource::Rval | CommandSource::Rlv
        ) && self.stat.mip.intersects(MipFlags::HOMF | MipFlags::HOMR)
        {
            self.ctrl.homf = false;
            self.ctrl.homr = false;
        }

        // C's home/jog sections run before the tweak/val move blocks and
        // act on latched button STATE every pass — evaluate latent
        // buttons for the move-block srcs so a bare button put overtaken
        // in last_write still fires here. The button srcs dispatch their
        // own arms below; for the housekeeping srcs the arms must run
        // unconditionally (see the latent-SPMG gate above).
        if matches!(
            src,
            CommandSource::Val
                | CommandSource::Dval
                | CommandSource::Rval
                | CommandSource::Rlv
                | CommandSource::Twf
                | CommandSource::Twr
        ) && self.dispatch_latent_buttons(&mut effects)
        {
            return effects;
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
                            // A new explicit command path owns completion now;
                            // disarm any safety-net verify flag armed by a
                            // prior ExtendMove in this motion.
                            self.internal.verify_retarget_on_completion = false;
                            self.internal.pending_retarget = Some(self.pos.dval);
                            self.stat.mip.insert(MipFlags::STOP);
                            effects.commands.push(MotorCommand::Stop {
                                acceleration: self.move_accel_egu(),
                            });
                            effects.request_poll = true;
                            effects.suppress_forward_link = true;
                            return effects;
                        }
                        RetargetAction::ExtendMove => {
                            // C parity (motorRecord.cc:2241, 2532-2535): same-
                            // direction retarget while moving re-enters do_work
                            // and dispatches a new MOVE_ABS/MOVE_REL in-flight.
                            // plan_absolute_move emits the new move and also
                            // updates ldvl/lval/lrvl per C load_pos semantics.
                            //
                            // plan_absolute_move replaces MIP with MOVE,
                            // clearing an in-flight STOP; drop any target
                            // parked behind that stop so it cannot resurrect
                            // over this newer explicit one (invariant:
                            // pending_retarget is Some ⟹ MIP_STOP committed).
                            self.internal.pending_retarget = None;
                            //
                            // Rust-only defensive layer: arm a completion-time
                            // verification so that, if a driver silently ignores
                            // the in-flight retarget and stops at the old target,
                            // we replan once before finalizing — independent of
                            // RTRY/RDBD. Not in C (C assumes driver supports
                            // in-flight target updates); kept here as robustness
                            // against drivers that don't.
                            self.plan_absolute_move(&mut effects);
                            self.internal.verify_retarget_on_completion = true;
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
                    // epics-modules/motor #170 — latest-wins: a fresh jog
                    // command clears a still-latched opposite-direction
                    // button so JOGF and JOGR are never both active.
                    if forward {
                        self.ctrl.jogr = false;
                    } else {
                        self.ctrl.jogf = false;
                    }
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
                // C: rdif = NINT(diff / mres)
                self.pos.rdif = if self.conv.mres != 0.0 {
                    (self.pos.diff / self.conv.mres).round() as i64
                } else {
                    0
                };
                // C: load_pos updates ldvl/lval/lrvl
                self.internal.ldvl = self.pos.dval;
                self.internal.lval = self.pos.val;
                self.internal.lrvl = self.pos.rval;
                // AsynMotor operates in dial coordinates
                effects.commands.push(MotorCommand::SetPosition {
                    position: self.pos.dval,
                });
            }
            CommandSource::Cnen => {
                // C: case motorRecordCNEN — only drives ENABLE/DISABL_TORQUE
                // when the controller reports gain support (MSTA bit
                // GAIN_SUPPORT). Drivers without it would reject the command.
                if self.stat.msta.contains(MstaFlags::GAIN_SUPPORT) {
                    effects.commands.push(MotorCommand::SetClosedLoop {
                        enable: self.ctrl.cnen,
                    });
                }
            }
            CommandSource::PcoEnable => {
                // C: 05b25c1d (PR #248) — push the latched PCO configuration
                // first, then enable/disable so the driver uses fresh params.
                effects.commands.push(MotorCommand::SetPcoConfig {
                    start: self.pco.start,
                    end: self.pco.end,
                    increment: self.pco.increment,
                    pulse_width_us: self.pco.pulse_width_us,
                });
                effects.commands.push(MotorCommand::EnablePco {
                    enable: self.pco.enable,
                });
            }
        }

        effects
    }

    /// Plan an absolute move to current DVAL.
    pub(crate) fn plan_absolute_move(&mut self, effects: &mut ProcessEffects) {
        // Check soft limits (C: disabled only when dhlm == dllm == 0.0)
        if !(self.limits.dhlm == self.limits.dllm && self.limits.dllm == 0.0) {
            // C: DLLM > DHLM means limits are inverted => always violation
            if self.limits.dllm > self.limits.dhlm {
                self.limits.lvio = true;
                tracing::warn!(
                    "limit violation: inverted limits dllm={:.4} > dhlm={:.4}",
                    self.limits.dllm,
                    self.limits.dhlm
                );
                return;
            }

            let preferred = self.is_preferred_direction(self.pos.dval, self.pos.drbv);

            if preferred {
                // C preferred_dir: check dval against limits
                let target_outside =
                    self.pos.dval > self.limits.dhlm || self.pos.dval < self.limits.dllm;
                if target_outside {
                    // C: allow if dval is closer to valid range than ldvl
                    let ldvl_above = self.internal.ldvl > self.limits.dhlm;
                    let ldvl_below = self.internal.ldvl < self.limits.dllm;
                    let moving_toward_valid = (ldvl_above && self.pos.dval < self.internal.ldvl)
                        || (ldvl_below && self.pos.dval > self.internal.ldvl);
                    if !moving_toward_valid {
                        self.limits.lvio = true;
                        tracing::warn!(
                            "limit violation: dval={:.4}, limits=[{:.4}, {:.4}]",
                            self.pos.dval,
                            self.limits.dllm,
                            self.limits.dhlm
                        );
                        return;
                    }
                }
            } else {
                // C non-preferred: check backlash pretarget against limits
                let pretarget = Self::compute_backlash_pretarget(self.pos.dval, self.retry.bdst);
                if pretarget > self.limits.dhlm || pretarget < self.limits.dllm {
                    self.limits.lvio = true;
                    tracing::warn!(
                        "limit violation: backlash pretarget={:.4}, limits=[{:.4}, {:.4}]",
                        pretarget,
                        self.limits.dllm,
                        self.limits.dhlm
                    );
                    return;
                }
            }
        }
        self.limits.lvio = false;

        // C: too_small check -- suppress moves smaller than one motor step
        if self.conv.mres != 0.0 {
            let npos = (self.pos.dval / self.conv.mres).round() as i64;
            let rpos = (self.pos.drbv / self.conv.mres).round() as i64;
            if (npos - rpos).abs() < 1 {
                // Sub-step move: pulse DMOV 1→0→1 so clients
                // (ophyd/bluesky) detect the move completed.
                // Set dmov=false now; process() will flush DMOV=0 via
                // AsyncPendingNotify, then the immediate re-process
                // (with no pending event) will finalize with DMOV=1.
                self.stat.dmov = false;
                self.stat.movn = true;
                // Request a poll so the next I/O Intr cycle completes
                effects.request_poll = true;
                effects.suppress_forward_link = true;
                return;
            }
        }

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
        self.retry.rcnt = 0;
        self.retry.miss = false;

        // tdir reflects the actual first-command direction
        self.stat.tdir = move_target > self.pos.drbv;
        // C parity (motorRecord.cc do_work): `diff` is recomputed
        // fresh as `dval - drbv` immediately before CDIR is derived.
        // The VAL/DVAL/RVAL/RLV/TWF write paths update `pos.dval` but
        // never refresh `pos.diff` — only `process_motor_info` (poll)
        // and the `Set` branch do. A user VAL write while idle with no
        // poll update in the same cycle would otherwise compute CDIR
        // (and the downstream consumers in `handle_retarget` /
        // `ls_blocks_retry`) from a stale `diff`. Recompute here so
        // CDIR always reflects the target being dispatched.
        self.pos.diff = self.pos.dval - self.pos.drbv;
        // CDIR: commanded direction from the position error
        // C: cdir = (rdif < 0) ? 0 : 1, where rdif = diff/mres
        // When MRES < 0, the sign inverts
        self.stat.cdir = if self.conv.mres >= 0.0 {
            self.pos.diff >= 0.0
        } else {
            self.pos.diff < 0.0
        };

        // Set MIP and phase
        self.stat.mip = MipFlags::MOVE;
        self.set_phase(MotionPhase::MainMove);
        self.internal.backlash_pending = backlash;

        let use_rel = self.use_relative_moves();
        let frac = self.retry.frac;
        // preferred_dir uses the OLD ldvl (the previous dispatched target),
        // so is_preferred_direction must run before we update ldvl below.
        let preferred = self.is_preferred_direction(self.pos.dval, self.pos.drbv);
        let position_error = self.pos.dval - self.pos.drbv;

        // C parity (motorRecord.cc:2469 load_pos): ldvl/lval/lrvl reflect
        // the target being dispatched. For in-flight same-direction retarget,
        // this keeps (dval - ldvl) in the next is_preferred_direction call
        // tracking each successive target, not only the original one.
        self.internal.ldvl = self.pos.dval;
        self.internal.lval = self.pos.val;
        self.internal.lrvl = self.pos.rval;

        // C has 3 cases (do_work lines 2479-2524):
        // Case 1: No backlash OR (preferred + same vel/accel): slew vel, FRAC
        // Case 2: Preferred + within backlash range: backlash vel, FRAC
        // Case 3: Non-preferred (backlash): pretarget, no FRAC
        let same_vel = (self.vel.bvel - self.vel.velo).abs() < 1e-12
            && (self.vel.bacc - self.vel.accl).abs() < 1e-12;
        let within_backlash_range =
            preferred && self.retry.bdst != 0.0 && position_error.abs() <= self.retry.bdst.abs();

        if backlash && !preferred {
            // Case 3: Non-preferred direction: move to pretarget, no FRAC
            self.emit_move(
                effects,
                use_rel,
                move_target - self.pos.drbv,
                move_target,
                self.vel.velo,
                self.move_accel_egu(),
            );
        } else if !backlash || (preferred && same_vel) {
            // Case 1: No backlash or preferred with matching vel/accel
            // Apply FRAC scaling
            self.emit_move(
                effects,
                use_rel,
                position_error * frac,
                self.pos.drbv + position_error * frac,
                self.vel.velo,
                self.move_accel_egu(),
            );
        } else if within_backlash_range {
            // Case 2: Preferred direction, within backlash range
            // Use backlash velocity, apply FRAC
            self.emit_move(
                effects,
                use_rel,
                position_error * frac,
                self.pos.drbv + position_error * frac,
                self.vel.bvel,
                self.backlash_accel_egu(),
            );
        } else {
            // Preferred direction, outside backlash range, vel differs
            // Use slew velocity, apply FRAC
            self.emit_move(
                effects,
                use_rel,
                position_error * frac,
                self.pos.drbv + position_error * frac,
                self.vel.velo,
                self.move_accel_egu(),
            );
        }
        effects.request_poll = true;
        effects.suppress_forward_link = true;
    }

    /// Handle STOP command.
    fn handle_stop(&mut self, effects: &mut ProcessEffects) {
        self.ctrl.stop = false; // pulse field
        self.stop_axis(effects);
    }

    /// The C do_work top-block stop path, shared by the STOP field pulse
    /// and SPMG=Stop (motorRecord.cc:1871: `spmg == motorSPMG_Stop ||
    /// stop == true`). Single owner so the two entries cannot drift.
    fn stop_axis(&mut self, effects: &mut ProcessEffects) {
        let in_motion =
            self.stat.phase != MotionPhase::Idle || self.stat.mip.contains(MipFlags::EXTERNAL);
        if self.stat.mip.contains(MipFlags::RETRY) {
            // C 1874-1885: a stop during retry abandons the retry and
            // reports done immediately (mip = MIP_DONE, dmov = TRUE).
            // No readback sync — the axis is already within RDBD of the
            // target, so the drive fields keep the user target.
            self.finalize_motion(effects);
        } else if in_motion && !self.stat.mip.contains(MipFlags::STOP) {
            // C 1890-1906: while moving, only mark the stop — pp = TRUE,
            // clear_buttons(), mip = MIP_STOP (replaced wholesale,
            // erasing MOVE/JOG/HOME/EXTERNAL bits). The VAL/DVAL/RVAL <-
            // readback sync and DMOV happen once the axis has actually
            // stopped (postProcess, 826-849), mirrored by
            // check_completion's MIP_STOP branch via sync_positions().
            // Syncing or finalizing eagerly here would post a transient
            // mid-deceleration snapshot before the rest position is known.
            self.internal.backlash_pending = false;
            self.ctrl.jogf = false;
            self.ctrl.jogr = false;
            self.ctrl.homf = false;
            self.ctrl.homr = false;
            // C 1901-1905: "When we wait for DLY, keep it. Otherwise the
            // record may lock up."
            if !self.stat.mip.contains(MipFlags::DELAY_REQ) {
                self.stat.mip = MipFlags::STOP;
            }
        } else if in_motion {
            // A stop already in flight (C 1874-1888 early return). The
            // explicit stop drops a parked NTM retarget — C discards the
            // overtaken target at stop completion via the
            // `(val != lval) && !(mip & MIP_STOP)` gate (1385) plus the
            // postProcess sync. A queued jog/home survives, matching C
            // where the buttons/MIP_JOG_REQ outlive the early return and
            // postProcess re-fires the queued request.
            self.internal.pending_retarget = None;
        }
        // else: idle — bare STOP_AXIS only.
        //
        // C motorRecord.cc — STOP_AXIS is sent unconditionally ("just in
        // case"): the driver may still be settling even when the record
        // considers itself idle (e.g. after an InPosition retry, which
        // finalizes the record but leaves the servo moving).
        effects.commands.push(MotorCommand::Stop {
            acceleration: self.move_accel_egu(),
        });
    }

    /// Start jogging.
    fn start_jog(&mut self, forward: bool, effects: &mut ProcessEffects) {
        // C: if motor is moving, stop first then queue jog for after stop.
        // The queued request lives in internal.queued_motion, NOT in the MIP
        // JOGF/JOGR bits — otherwise a plain STOP on an active jog (which also
        // leaves JOGF|STOP set) would be replayed as a queued jog.
        if self.stat.phase != MotionPhase::Idle && self.stat.movn {
            self.stat.mip = MipFlags::STOP;
            self.internal.queued_motion = Some(QueuedMotion::Jog { forward });
            self.internal.backlash_pending = false;
            effects.commands.push(MotorCommand::Stop {
                acceleration: self.move_accel_egu(),
            });
            effects.request_poll = true;
            effects.suppress_forward_link = true;
            return;
        }

        let dir = if forward {
            MotionDirection::Positive
        } else {
            MotionDirection::Negative
        };
        if self.is_blocked_by_hw_limit(dir) {
            return;
        }

        // C: 9e5b5432 PR #99 — refuse a jog command that would push past a
        // soft limit further. Without this the record stays in MIP=JOG_REQ
        // and the button must be released manually. The opposite direction
        // (back inside the soft window) is still allowed.
        if self.jog_violates_soft_limit(forward) {
            if forward {
                self.ctrl.jogf = false;
            } else {
                self.ctrl.jogr = false;
            }
            self.limits.lvio = true;
            effects.suppress_forward_link = true;
            return;
        }

        self.stat.dmov = false;

        if forward {
            self.stat.mip = MipFlags::JOGF;
        } else {
            self.stat.mip = MipFlags::JOGR;
        }
        self.set_phase(MotionPhase::Jog);
        // Remember jog direction for backlash (MIP flags get cleared by stop_jog)
        self.internal.jog_was_forward = forward;

        // CDIR for jog: account for DIR and MRES sign
        // C: cdir computed from jog direction considering dir polarity and MRES sign
        let user_forward = if self.conv.dir == MotorDir::Neg {
            !forward
        } else {
            forward
        };
        self.stat.cdir = if self.conv.mres >= 0.0 {
            user_forward
        } else {
            !user_forward
        };

        effects.commands.push(MotorCommand::MoveVelocity {
            direction: forward,
            velocity: self.vel.jvel,
            acceleration: self.jog_accel_egu(),
        });
        effects.request_poll = true;
        effects.suppress_forward_link = true;
    }

    /// Stop jogging.
    fn stop_jog(&mut self, effects: &mut ProcessEffects) {
        self.stat.mip.insert(MipFlags::JOG_STOP);
        self.set_phase(MotionPhase::JogStopping);
        effects.commands.push(MotorCommand::Stop {
            acceleration: self.jog_accel_egu(),
        });
    }

    /// Whether the limit switch in the (user-direction-adjusted) home
    /// direction blocks a home command. C's home-section entry gate and
    /// start check (motorRecord.cc:2010-2013): HOMF blocked by HLS when
    /// DIR=Pos, by LLS when DIR=Neg; HOMR mirrored.
    fn home_blocked_by_limit(&self, forward: bool) -> bool {
        if forward == (self.conv.dir == MotorDir::Pos) {
            self.limits.hls
        } else {
            self.limits.lls
        }
    }

    /// C do_work evaluates the latched HOMF/HOMR/JOGF/JOGR buttons by
    /// STATE on every put-processing pass — home section
    /// (motorRecord.cc:2010), jog start (2079), jog stop (2148) — before
    /// the tweak/val move blocks, not by which field write triggered the
    /// pass. The Rust dispatcher acts on `last_write`, so a bare button
    /// put (dbPut, no process) overtaken in `last_write` by a later
    /// write would otherwise stay latched and never fire. Called for the
    /// move-block srcs before their own handling; returns true when a
    /// button action consumed the pass (the C sections return OK, so the
    /// position write that triggered the pass is overtaken).
    fn dispatch_latent_buttons(&mut self, effects: &mut ProcessEffects) -> bool {
        // Home (C 2010-2013): pure button-state gate per direction,
        // skipping a direction already homing or blocked by its limit.
        let homf_ready = self.ctrl.homf
            && !self.stat.mip.contains(MipFlags::HOMF)
            && !self.home_blocked_by_limit(true);
        let homr_ready = self.ctrl.homr
            && !self.stat.mip.contains(MipFlags::HOMR)
            && !self.home_blocked_by_limit(false);
        if homf_ready || homr_ready {
            self.start_home(homf_ready, effects);
            return true;
        }

        let jog_active = self
            .stat
            .mip
            .intersects(MipFlags::JOGF | MipFlags::JOGR | MipFlags::JOG_BL1 | MipFlags::JOG_BL2);
        // Jog start (C 2079): a latched button with no jog in flight.
        // The hardware-limit check mirrors C special()'s MIP_JOG_REQ
        // arming gate (3042-3053); on a blocked direction the button
        // stays latched and the pass falls through, like C's home/jog
        // gates failing into the move block. Soft-limit refusal happens
        // inside start_jog like the C section body.
        if !jog_active && (self.ctrl.jogf || self.ctrl.jogr) {
            let forward = self.ctrl.jogf;
            let dir = if forward {
                MotionDirection::Positive
            } else {
                MotionDirection::Negative
            };
            if !self.is_blocked_by_hw_limit(dir) {
                // epics-modules/motor #170 latest-wins, as in the
                // Jogf/Jogr dispatch arm.
                if forward {
                    self.ctrl.jogr = false;
                } else {
                    self.ctrl.jogf = false;
                }
                self.start_jog(forward, effects);
                return true;
            }
            return false;
        }
        // Jog stop (C 2148): a jog in flight whose button is no longer
        // held. With the opposite button latched, stop and queue the
        // reverse jog (start_jog's moving branch) like the dispatch
        // arm's latest-wins path.
        if self.stat.phase == MotionPhase::Jog {
            let held = if self.stat.mip.contains(MipFlags::JOGF) {
                self.ctrl.jogf
            } else {
                self.ctrl.jogr
            };
            if !held {
                if self.ctrl.jogf || self.ctrl.jogr {
                    self.start_jog(self.ctrl.jogf, effects);
                } else {
                    self.stop_jog(effects);
                }
                return true;
            }
        }
        false
    }

    /// Start homing.
    fn start_home(&mut self, forward: bool, effects: &mut ProcessEffects) {
        // C: if motor is moving, stop first then queue home for after stop.
        // Queued request goes in internal.queued_motion (see start_jog).
        if self.stat.phase != MotionPhase::Idle && self.stat.movn {
            self.stat.mip = MipFlags::STOP;
            self.internal.queued_motion = Some(QueuedMotion::Home { forward });
            self.internal.backlash_pending = false;
            self.internal.pending_retarget = None;
            effects.commands.push(MotorCommand::Stop {
                acceleration: self.move_accel_egu(),
            });
            effects.request_poll = true;
            effects.suppress_forward_link = true;
            return;
        }

        // C: check limit switch in direction of home before starting
        // HOMF blocked by HLS (when DIR=Pos) or LLS (when DIR=Neg)
        if self.home_blocked_by_limit(forward) {
            if forward {
                self.ctrl.homf = false;
            } else {
                self.ctrl.homr = false;
            }
            return;
        }

        self.stat.dmov = false;

        if forward {
            self.stat.mip = MipFlags::HOMF;
            self.ctrl.homf = false; // pulse
        } else {
            self.stat.mip = MipFlags::HOMR;
            self.ctrl.homr = false; // pulse
        }
        self.set_phase(MotionPhase::Homing);

        // C: home direction is inverted when MRES is negative
        // if ((MIP_HOMF && mres>0) || (MIP_HOMR && mres<0)) => HOME_FOR else HOME_REV
        let hw_forward = if self.conv.mres >= 0.0 {
            forward
        } else {
            !forward
        };

        // CDIR for homing: C accounts for MRES sign
        self.stat.cdir = if self.conv.mres >= 0.0 {
            forward
        } else {
            !forward
        };

        effects.commands.push(MotorCommand::Home {
            forward: hw_forward,
            velocity: self.vel.hvel,
            acceleration: self.move_accel_egu(),
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

        // C motorRecord.cc — a tweak that changes VAL flows through the same
        // VAL-change path: in SET mode it redefines coordinates rather than
        // moving the motor.
        if self.conv.set && !self.conv.igset {
            // #231: LOAD_POS blocked — refuse the redefinition entirely.
            if self.conv.loadpos_blocked {
                self.pos.val -= delta; // undo: keep VAL/DVAL/OFF consistent
                return;
            }
            if let Ok((dval, rval, off)) = coordinate::cascade_from_val(
                self.pos.val,
                self.conv.dir,
                self.pos.off,
                self.conv.foff,
                self.conv.mres,
                true,
                self.pos.dval,
            ) {
                self.pos.dval = dval;
                self.pos.rval = rval;
                self.pos.off = off;
            }
            effects.commands.push(MotorCommand::SetPosition {
                position: self.pos.dval,
            });
            return;
        }

        // Normal (non-SET) tweak: cascade VAL->DVAL and issue a move.
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
                // C shares one top-block stop path between SPMG=Stop and
                // the STOP field pulse (motorRecord.cc:1871); stop_axis is
                // that shared owner. The drive-field sync and DMOV are
                // deferred to actual stop completion, not applied here.
                self.stop_axis(effects);
            }
            SpmgMode::Pause => {
                if self.stat.phase != MotionPhase::Idle {
                    // C (1898-1906): Pause sends STOP and sets MIP_STOP,
                    // but does NOT set pp — the drive fields keep the
                    // target so Go can resume. `mip = MIP_STOP` erases a
                    // queued jog (MIP_JOG_REQ) and a queued/active home
                    // (MIP_HOMF|HOMR), and clear_buttons() drops the home
                    // buttons: a paused jog resumes from its still-latched
                    // button on Go, a canceled home does not.
                    if matches!(self.internal.queued_motion, Some(QueuedMotion::Home { .. })) {
                        self.ctrl.homf = false;
                        self.ctrl.homr = false;
                    }
                    self.internal.queued_motion = None;
                    self.internal.pending_retarget = None;
                    // C 1901-1905: keep MIP while waiting on DLY.
                    if !self.stat.mip.contains(MipFlags::DELAY_REQ) {
                        self.stat.mip.insert(MipFlags::STOP);
                    }
                    effects.commands.push(MotorCommand::Stop {
                        acceleration: self.move_accel_egu(),
                    });
                }
            }
            SpmgMode::Go => {
                if matches!(old, SpmgMode::Pause) && self.stat.phase == MotionPhase::Idle {
                    // C: on Go, a still-latched jog button resumes jogging;
                    // otherwise replan toward the saved DVAL target.
                    if self.ctrl.jogf || self.ctrl.jogr {
                        let forward = self.ctrl.jogf;
                        self.start_jog(forward, effects);
                    } else if (self.pos.dval - self.pos.drbv).abs() > self.retry.rdbd.max(1e-12) {
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

    /// Helper to emit either MoveRelative or MoveAbsolute.
    fn emit_move(
        &self,
        effects: &mut ProcessEffects,
        use_rel: bool,
        rel_distance: f64,
        abs_position: f64,
        velocity: f64,
        acceleration: f64,
    ) {
        if use_rel {
            effects.commands.push(MotorCommand::MoveRelative {
                distance: rel_distance,
                velocity,
                acceleration,
            });
        } else {
            effects.commands.push(MotorCommand::MoveAbsolute {
                position: abs_position,
                velocity,
                acceleration,
            });
        }
    }

    /// Effective base velocity for acceleration math. Drivers that do not
    /// support a base velocity advertise it via MSTA bit 15
    /// (`VBAS_UNSUPPORTED`, epics-modules/motor #76); for those VBAS is
    /// treated as 0.
    pub(crate) fn effective_vbas(&self) -> f64 {
        if self.stat.msta.contains(MstaFlags::VBAS_UNSUPPORTED) {
            0.0
        } else {
            self.vel.vbas
        }
    }

    /// Acceleration sent to the driver for a normal move, in EGU/sec².
    /// Mirrors C `accEGUfromVelo`: `vmax = fabs(velo)`, `vmin = vbas`; when
    /// ACCU=Accs the EGU/sec² value ACCS is used directly, otherwise
    /// `(vmax - vmin) / accl`, or `vmax / accl` when `vmax <= vmin`
    /// (C: `b201e40e`, PR #75).
    ///
    /// Deviation from C: the result is floored to a strictly positive value.
    /// C lets a 0 acceleration through and the driver layer skips SET_ACCEL;
    /// motor-rs always carries an acceleration in the MotorCommand, so a 0
    /// (unconfigured axis, or ACCU=Accs with ACCS<=0) is replaced by a
    /// nominal positive rate.
    pub(crate) fn move_accel_egu(&self) -> f64 {
        let accl = if self.vel.accl > 0.0 {
            self.vel.accl
        } else {
            0.1
        };
        let vmax = self.vel.velo.abs();
        let vmin = self.effective_vbas();
        let rate = if self.vel.accu == AccsUsed::Accs {
            self.vel.accs
        } else if vmax > vmin {
            (vmax - vmin) / accl
        } else {
            vmax / accl
        };
        if rate > 0.0 {
            rate
        } else {
            vmax.max(1.0) / accl
        }
    }

    /// Acceleration for a backlash move, EGU/sec². Uses BVEL/BACC.
    /// Always strictly positive (see `move_accel_egu`).
    pub(crate) fn backlash_accel_egu(&self) -> f64 {
        let bacc = if self.vel.bacc > 0.0 {
            self.vel.bacc
        } else {
            0.1
        };
        let span = self.vel.bvel - self.effective_vbas();
        let rate = if span > 0.0 {
            span / bacc
        } else {
            self.vel.bvel / bacc
        };
        if rate > 0.0 {
            rate
        } else {
            self.vel.bvel.abs().max(1.0) / bacc
        }
    }

    /// Acceleration for a jog, EGU/sec². JAR is already an EGU/sec² rate;
    /// fall back to the normal move acceleration when JAR is unset.
    pub(crate) fn jog_accel_egu(&self) -> f64 {
        if self.vel.jar > 0.0 {
            self.vel.jar
        } else {
            self.move_accel_egu()
        }
    }

    /// C: use_rel = rtry != 0 && rmod != InPosition && (ueip || urip)
    pub(crate) fn use_relative_moves(&self) -> bool {
        self.retry.rtry != 0
            && self.retry.rmod != RetryMode::InPosition
            && (self.conv.ueip || self.conv.urip)
    }

    /// Check if move is in the preferred direction (same as BDST sign).
    /// C: when use_rel=false, compares dval vs ldvl (previous target).
    ///    when use_rel=true, compares diff (dval - drbv) vs 0.
    fn is_preferred_direction(&self, dval: f64, drbv: f64) -> bool {
        if self.retry.bdst == 0.0 {
            return true;
        }
        let move_dir = if self.use_relative_moves() {
            // use_rel: compare target vs current position
            dval - drbv
        } else {
            // !use_rel: compare target vs previous target (ldvl)
            dval - self.internal.ldvl
        };
        if move_dir == 0.0 {
            return true;
        }
        (move_dir > 0.0) == (self.retry.bdst > 0.0)
    }

    /// Handle a new target (VAL/DVAL/RVAL/RLV write) that arrives while a
    /// motion is in progress.
    ///
    /// C parity (`motorRecord.cc`):
    ///
    /// - `do_work` (line 2241) re-issues the move block on *every*
    ///   `dval != ldvl || !dmov` — completely INDEPENDENT of NTM. A
    ///   VAL/DVAL/RVAL/RLV write during motion always re-dispatches a
    ///   move. An earlier Rust version returned [`RetargetAction::Ignore`]
    ///   whenever `ntm == No` (its `motorRecord.dbd` default, since NTM
    ///   has no `initial()`), silently discarding the write.
    /// - NTM gates only the *opposite-direction* stop-and-replan in the
    ///   `process()` `movn` block (line 1327-1331): when
    ///   `ntm == menuYesNoYES && sign_rdif != cdir &&
    ///   fabs(diff) > ntm_deadband && move_or_retry && !MIP_STOP`, C
    ///   sends `STOP_AXIS`. With `ntm == No` C never stops first — the
    ///   axis retargets directly via the `do_work` re-issue.
    ///
    /// Therefore: a write during motion always re-issues the move
    /// ([`RetargetAction::ExtendMove`], which routes through
    /// `plan_absolute_move` whose own too-small/SPDB deadband checks
    /// suppress no-op moves). NTM only promotes an opposite-direction,
    /// beyond-deadband retarget to [`RetargetAction::StopAndReplan`].
    pub fn handle_retarget(&mut self, new_dval: f64) -> RetargetAction {
        // Only retarget during an active move, retry, or stop
        // deceleration. C's `do_work` re-issue and the `movn`-block STOP
        // both require an in-flight move; MIP_STOP counts as in-flight
        // because the C stop path replaces MIP wholesale (`mip =
        // MIP_STOP`, motorRecord.cc:1903) yet the move block still fires
        // on a new target during the deceleration.
        let in_move = self
            .stat
            .mip
            .intersects(MipFlags::MOVE | MipFlags::RETRY | MipFlags::STOP);
        if !in_move {
            return RetargetAction::Ignore;
        }
        // A stop in flight does not make the record deaf to new targets.
        // C routes a put-initiated process straight to `do_work` even
        // while the axis is decelerating, and the `do_work` move block
        // (motorRecord.cc:2241, `dval != ldvl || !dmov`) never tests
        // MIP_STOP — the new move is dispatched immediately and
        // `mip = MIP_MOVE` replaces the stop state. Discarding the write
        // here instead would let the stop-completion path sync VAL back
        // to RBV and silently lose the target (the kohzuCtl
        // stop-then-rewrite retarget sequence hits exactly this window).
        // Only the NTM stop-first branch is excluded while stopping
        // (C gate at 1331: `(pmr->mip & MIP_STOP) == 0`), so no
        // double-stop is possible.
        if self.stat.mip.contains(MipFlags::STOP) {
            if self.internal.queued_motion.is_some() {
                // An explicit queued jog/home owns this stop's
                // completion — in C the do_work jog/home sections run
                // before the move block and re-fire the queued request,
                // so the position write is overtaken.
                return RetargetAction::Ignore;
            }
            return RetargetAction::ExtendMove;
        }

        let diff = new_dval - self.pos.drbv;
        let deadband = self.timing.ntmf * (self.retry.bdst.abs() + self.retry.rdbd);

        // C `movn`-block STOP_AXIS gate: opposite direction AND error
        // beyond the NTM deadband AND NTM enabled.
        let sign_diff = diff >= 0.0;
        let direction_changed = sign_diff != self.stat.cdir;

        if self.timing.ntm && direction_changed && diff.abs() > deadband {
            RetargetAction::StopAndReplan
        } else {
            // C `do_work` re-issues the move on every DVAL write during
            // motion, regardless of NTM and regardless of direction.
            RetargetAction::ExtendMove
        }
    }

    /// C `enforceMinRetryDeadband` (motorRecord.cc:557): RDBD must be at
    /// least |MRES|. C calls this at init, every do_work pass, and on RDBD/
    /// MRES change. Without it RDBD stays at its 0.0 default and retry never
    /// fires (and MISS never latches), since retry is gated on RDBD > 0.
    pub(crate) fn enforce_min_retry_deadband(&mut self) {
        let min_rdbd = self.conv.mres.abs();
        if self.retry.rdbd < min_rdbd {
            self.retry.rdbd = min_rdbd;
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

    /// Whether a velocity jog in the requested direction would push past a
    /// soft limit in user coordinates. C: `9e5b5432` PR #99.
    fn jog_violates_soft_limit(&self, forward: bool) -> bool {
        // C: soft limits disabled when HLM == LLM == 0.0
        if self.limits.hlm == self.limits.llm && self.limits.llm == 0.0 {
            return false;
        }
        if forward {
            self.pos.val >= self.limits.hlm
        } else {
            self.pos.val <= self.limits.llm
        }
    }

    /// Process the motor record (called by EPICS record support).
    pub fn do_process(&mut self) -> ProcessEffects {
        // C: do_work calls enforceMinRetryDeadband every pass.
        self.enforce_min_retry_deadband();

        // STUP: one-shot status refresh request. C handles STUP at the top of
        // do_work and then *continues* the pass — it does not early-return.
        // Consume the flag here, run the normal process pipeline, and OR the
        // status_refresh into whatever effects result so a user write or
        // device update arriving in the same cycle is not dropped.
        let stup_requested = self.stat.stup > 0;
        if stup_requested {
            self.stat.stup = 0;
        }
        let mut effects = self.do_process_inner();
        if stup_requested {
            effects.status_refresh = true;
        }
        effects
    }

    fn do_process_inner(&mut self) -> ProcessEffects {
        // Sub-step pulse recovery: if DMOV is false but phase is Idle
        // (no real motion started), finalize to restore DMOV=1.
        if !self.stat.dmov && self.stat.phase == MotionPhase::Idle && self.stat.mip.is_empty() {
            let mut effects = ProcessEffects::default();
            self.finalize_motion(&mut effects);
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
                // C: after DLY, request fresh poll then evaluate for retry
                let mut effects = ProcessEffects::default();
                self.stat.mip.remove(MipFlags::DELAY_REQ);
                self.stat.mip.insert(MipFlags::DELAY_ACK);
                effects.status_refresh = true;
                effects.suppress_forward_link = true;
                effects
            }
            None => {
                // C: ea063f5f — an externally initiated move is flagged by
                // process_motor_info() during the Idle-phase readback, which
                // determine_event() reports as no event. The completion
                // pipeline still has to run so MIP_EXTERNAL clears and DMOV
                // returns to 1 once the driver finishes.
                if self.stat.mip.contains(MipFlags::EXTERNAL) {
                    self.check_completion()
                } else {
                    ProcessEffects::default()
                }
            }
        }
    }
}
