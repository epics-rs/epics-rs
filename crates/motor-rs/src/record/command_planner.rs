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

        // C enter_do_work (1462-1483) re-evaluates LVIO on EVERY process
        // pass before do_work — including put-processing passes, since
        // HLM/LLM/DHLM/DLLM are pp(TRUE). A rising violation sets
        // stop = 1 (1480), which the latent-stop gate below consumes in
        // this same pass, exactly like C's do_work stop branch.
        if self.recompute_lvio_during_motion() {
            self.ctrl.stop = true;
        }

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
                    // C 2167-2181: the tweak fold is NOT gated by
                    // stop_or_pause — only the move dispatch is (the
                    // return at 2237 comes after collection). Collect it
                    // before refusing this pass so the fold lands in
                    // VAL/DVAL and SPMG=Go fires it like any position
                    // write collected while stopped.
                    if matches!(
                        src,
                        CommandSource::Val
                            | CommandSource::Dval
                            | CommandSource::Rval
                            | CommandSource::Rlv
                            | CommandSource::Twf
                            | CommandSource::Twr
                    ) {
                        self.collect_tweak(&mut effects);
                    }
                    return effects;
                }
                // C: db5da2f0 + 7493d50b — when URIP=Yes and the external
                // RDBL link is in error, refuse to start a new motion. If a
                // motion is in flight, emit a STOP so the axis halts.
                if self.conv.urip && self.conv.rdbl_error {
                    if self.stat.phase != MotionPhase::Idle {
                        self.stat.mip.insert(MipFlags::STOP);
                        // An error stop post-processes like a commanded
                        // stop: sync the drive fields at completion rather
                        // than arming the paused-resume state.
                        self.internal.pp = true;
                        effects.commands.push(MotorCommand::Stop {
                            acceleration: self.move_accel_egu(),
                        });
                    } else {
                        // C couples the failed-RDBL refusal into the same
                        // restore gate as LVIO (2434: `lvio || rtnstat ==
                        // FALSE`): the refused target rolls back.
                        self.refuse_move_restore_lasts();
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

        // C tweak section (2167-2181): runs after the home/jog sections
        // (which return early when they dispatch) and folds latched
        // buttons into VAL on every pass — a bare TWF/TWR put overtaken
        // in last_write rides this pass's val processing as one combined
        // move toward `written value + TWV`.
        let tweak_pending = matches!(
            src,
            CommandSource::Val
                | CommandSource::Dval
                | CommandSource::Rval
                | CommandSource::Rlv
                | CommandSource::Twf
                | CommandSource::Twr
        ) && self.collect_tweak(&mut effects);

        match src {
            CommandSource::Val | CommandSource::Dval | CommandSource::Rval | CommandSource::Rlv => {
                // C 2187-2193: RLV folds into VAL in the do_work
                // collection section ("Later, we'll act on this") and the
                // pass proceeds exactly like a VAL change — including the
                // in-flight retarget handling below. A separate RLV arm
                // that dispatched directly bypassed the NTM stop-first
                // path and the retarget invariants.
                if src == CommandSource::Rlv {
                    self.pos.val += self.pos.rlv;
                    self.pos.rlv = 0.0;
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
                }
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
                            // C 1341: pp = FALSE — "Don't post process the
                            // previous move." The replanned target must not
                            // be synced away at the stop completion.
                            self.internal.pp = false;
                            self.internal.pending_retarget = Some(self.pos.dval);
                            self.stat.mip.insert(MipFlags::STOP);
                            effects.commands.push(MotorCommand::Stop {
                                acceleration: self.move_accel_egu(),
                            });
                            effects.request_poll = true;
                            return effects;
                        }
                        RetargetAction::ExtendMove => {
                            // Deliberate divergence from C. C never sends a
                            // move while one is in flight: do_work's move
                            // block is *entered* on every in-flight write
                            // (motorRecord.cc:2240, `dval != ldvl || !dmov`)
                            // and val/dval/rval take the new target, but the
                            // command dispatch is gated at 2455 on
                            // `mip == MIP_DONE || mip == MIP_RETRY` — the
                            // target is parked and dispatched only at
                            // completion (maybeRetry at 1431 measures diff
                            // against the new dval, and the same pass
                            // re-enters do_work where the gate passes). Here
                            // the new move is emitted immediately so
                            // controllers that accept on-the-fly retargets
                            // track the newest target without the parked
                            // round trip. plan_absolute_move also updates
                            // ldvl/lval/lrvl as the C dispatch block does
                            // (2469-2471).
                            //
                            // plan_absolute_move replaces MIP with MOVE,
                            // clearing an in-flight STOP; drop any target
                            // parked behind that stop so it cannot resurrect
                            // over this newer explicit one (invariant:
                            // pending_retarget is Some ⟹ MIP_STOP committed).
                            self.internal.pending_retarget = None;
                            //
                            // Completion-time verification: if a driver
                            // silently ignores the in-flight retarget and
                            // stops at the old target, replan once before
                            // finalizing — independent of RTRY/RDBD. For such
                            // controllers this restores C's park-then-
                            // dispatch-at-completion behavior.
                            self.plan_absolute_move(&mut effects);
                            self.internal.verify_retarget_on_completion = true;
                            return effects;
                        }
                    }
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
                } else if matches!(self.internal.queued_motion, Some(QueuedMotion::Jog { .. })) {
                    // C 2148-2155 on a queued jog (mip = JOGF|STOP): the
                    // release pass takes the stop-jogging branch — the
                    // JOGF/JOGR bits drop out of MIP and MIP_JOG_STOP
                    // rides the already-in-flight stop, so the request is
                    // dead at the stop completion (postProcess re-fires
                    // nothing). pp was armed by the queue branch; the
                    // completion syncs and finalizes like a plain stop.
                    self.internal.queued_motion = None;
                    self.stat.mip.remove(MipFlags::JOGF | MipFlags::JOGR);
                    self.stat.mip.insert(MipFlags::JOG_STOP);
                } else if self.stat.mip.intersects(MipFlags::JOGF | MipFlags::JOGR) {
                    self.stop_jog(&mut effects);
                }
                // else: C 2148 gate — no jog in MIP (idle, or already
                // decelerating with the bits dropped), the release is a
                // no-op.
            }
            CommandSource::Homf | CommandSource::Homr => {
                let forward = src == CommandSource::Homf;
                self.start_home(forward, &mut effects);
            }
            CommandSource::Twf | CommandSource::Twr => {
                // The fold already ran in the collection above (SET-mode
                // redefinition included); dispatch the pending move (C
                // move block firing on the folded val change).
                if tweak_pending {
                    self.plan_absolute_move(&mut effects);
                }
            }
            CommandSource::Spmg => {
                self.handle_spmg_change(&mut effects);
            }
            CommandSource::Sync => {
                // C 2540-2544: the latched SYNC applies only when idle
                // (`mip == MIP_DONE`); written during a motion it stays
                // latched and finalize_motion consumes it at completion.
                self.apply_latent_sync();
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
        // C move-block entry (2248-2255): DIFF and RDIF are recomputed
        // from the requested target before anything else — they post
        // even when the pass is then suppressed (too_small) or refused
        // (LVIO). The VAL/DVAL/RVAL/RLV/TWF write paths update
        // `pos.dval` but never refresh them; only the poll does.
        self.pos.diff = self.pos.dval - self.pos.drbv;
        self.pos.rdif = if self.conv.mres != 0.0 {
            (self.pos.diff / self.conv.mres).round() as i64
        } else {
            0
        };
        // C too_small (2308-2348): a RETRY dispatch is too small when the
        // remaining error is under RDBD in steps (2329-2330); a plain
        // dispatch when under one motor step or inside the open SPDB
        // window around DRBV (2313-2326).
        if self.conv.mres != 0.0 {
            let npos = (self.pos.dval / self.conv.mres).round() as i64;
            let rpos = (self.pos.drbv / self.conv.mres).round() as i64;
            let steps = (npos - rpos).abs();
            let too_small = if self.stat.mip.contains(MipFlags::RETRY) {
                let rdbd_steps = (self.retry.rdbd / self.conv.mres.abs()).round() as i64;
                steps < rdbd_steps
            } else {
                steps < 1
                    || (self.retry.spdb > 0.0
                        && (self.pos.dval - self.retry.spdb) < self.pos.drbv
                        && (self.pos.dval + self.retry.spdb) > self.pos.drbv)
            };
            if too_small {
                // C 2333-2342: a pending non-move (paused-RETRY resume
                // whose error drifted under RDBD) completes here with
                // mip = DONE; the quiesce below plus the DMOV pulse
                // recovery in do_process_inner restore DMOV=1.
                let pending_completion = !self.stat.dmov;
                if pending_completion
                    && (self.stat.mip.is_empty() || self.stat.mip == MipFlags::RETRY)
                {
                    self.stat.mip = MipFlags::empty();
                }
                // C 2343-2347: update the previous-target registers so the
                // suppressed divergence is not re-detected on every later
                // pass (move-block gate, overtaken-target replay).
                self.internal.ldvl = self.pos.dval;
                self.internal.lval = self.pos.val;
                self.internal.lrvl = self.pos.rval;
                // Rust-side deviation from C (deliberate, ophyd/bluesky):
                // a sub-step request pulses DMOV 1→0→1 so clients watching
                // DMOV see the "move" complete; a pending completion runs
                // the same flow to restore DMOV=1. dmov=false flushes
                // DMOV=0 via AsyncPendingNotify; the immediate re-process
                // (no pending event) finalizes with DMOV=1. An SPDB
                // suppress with DMOV already 1 stays quiet, like C's
                // dmov==FALSE gate (2333).
                if steps < 1 || pending_completion {
                    self.stat.dmov = false;
                    self.stat.movn = true;
                    effects.request_poll = true;
                }
                return;
            }
        }

        // C 2351-2356: the retry counter resets only when this dispatch is
        // NOT a retry — and BEFORE the LVIO check, so a refused fresh move
        // still zeroes it. A Go resume of a paused move re-enters here
        // with mip = MIP_RETRY (maybeRetry armed it at the pause stop),
        // and C preserves rcnt so repeated Pause/Go cycles count against
        // RTRY.
        if !self.stat.mip.contains(MipFlags::RETRY) {
            self.retry.rcnt = 0;
        }

        // C 2391-2395: preferred_dir, computed once for the LVIO check
        // and the dispatch case selection.
        let preferred = self.is_preferred_direction(self.pos.dval, self.pos.drbv);

        // C LVIO evaluation (2396-2415), AFTER the too_small suppression:
        // a request whose error is already inside the deadband completes
        // quietly even when the target sits past a limit. The
        // moving-toward-valid exception (2403-2405) is its own arm,
        // granted on DVAL whether or not the move is preferred — only the
        // final else discriminates: preferred checks DVAL, non-preferred
        // checks the backlash pretarget.
        self.limits.lvio = if self.limits.dhlm == self.limits.dllm && self.limits.dllm == 0.0 {
            false
        } else if self.limits.dllm > self.limits.dhlm {
            true
        } else if (self.pos.dval > self.limits.dhlm && self.pos.dval < self.internal.ldvl)
            || (self.pos.dval < self.limits.dllm && self.pos.dval > self.internal.ldvl)
        {
            false
        } else if preferred {
            self.pos.dval > self.limits.dhlm || self.pos.dval < self.limits.dllm
        } else {
            let bdstpos = Self::compute_backlash_pretarget(self.pos.dval, self.retry.bdst);
            bdstpos > self.limits.dhlm || bdstpos < self.limits.dllm
        };
        if self.limits.lvio {
            tracing::warn!(
                "limit violation: dval={:.4}, bdst={:.4}, limits=[{:.4}, {:.4}]",
                self.pos.dval,
                self.retry.bdst,
                self.limits.dllm,
                self.limits.dhlm
            );
            self.refuse_move_restore_lasts();
            return;
        }

        // C dispatch case selection (do_work 2479-2524):
        //   Case 1 (2479-2489): backlash disabled (|BDST| < |MRES|), or
        //     preferred direction with BVEL == VELO and BACC == ACCL —
        //     one leg at slew speed, FRAC applied.
        //   Case 2 (2491-2517): preferred direction, already within one
        //     backlash of the target — one leg at backlash speed, FRAC.
        //   Case 3 (2518-2524): everything else — two legs through the
        //     pretarget (dval - bdst), final approach from postprocess
        //     (pp = TRUE). A preferred move outside the range with
        //     BVEL != VELO takes this path too: the final BDST stretch
        //     is always traversed at backlash speed.
        // C compares BVEL/VELO and BACC/ACCL with exact == (2487-2488).
        let same_vel = self.vel.bvel == self.vel.velo && self.vel.bacc == self.vel.accl;
        let use_rel = self.use_relative_moves();

        // C 2280-2281: the relative legs measure from DRBV. A RETRY
        // dispatch RMOD-scales both — arithmetic (rtry-rcnt+1)/rtry
        // (2358-2368) or geometric 1/2^(rcnt-1) (2370-2383) — and each
        // scaled leg is floored at one raw step (the ±1 clamps). RMOD_D
        // scales nothing and has no floor; non-retry dispatches are
        // untouched. The clamp works in signed-MRES steps like C.
        let mut relpos = self.pos.dval - self.pos.drbv;
        let mut relbpos =
            Self::compute_backlash_pretarget(self.pos.dval, self.retry.bdst) - self.pos.drbv;
        if self.stat.mip.contains(MipFlags::RETRY) {
            let factor = match self.retry.rmod {
                RetryMode::Arithmetic => {
                    let rtry = f64::from(self.retry.rtry);
                    Some((rtry - f64::from(self.retry.rcnt) + 1.0) / rtry)
                }
                RetryMode::Geometric => Some(1.0 / 2.0_f64.powi(i32::from(self.retry.rcnt) - 1)),
                _ => None,
            };
            if let Some(factor) = factor {
                let mres = self.conv.mres;
                let scale_and_floor = |egu: f64| {
                    let steps = egu * factor / mres;
                    let steps = if steps.abs() < 1.0 {
                        if steps > 0.0 { 1.0 } else { -1.0 }
                    } else {
                        steps
                    };
                    steps * mres
                };
                relpos = scale_and_floor(relpos);
                relbpos = scale_and_floor(relbpos);
            }
        }

        let within_range = if use_rel {
            // C 2493: relbpos in (signed-MRES) steps against one step —
            // the RMOD-scaled value, since C scales before the dispatch.
            let relbpos_steps = relbpos / self.conv.mres;
            (self.retry.bdst >= 0.0 && relbpos_steps <= 1.0)
                || (self.retry.bdst < 0.0 && relbpos_steps >= 1.0)
        } else {
            // C 2284/2496: |newpos - currpos| <= rbdst1, with
            // rbdst1 = 1 + |bdst|/|mres| steps and currpos = LDVL —
            // in EGU: |dval - ldvl| <= |mres| + |bdst|.
            (self.pos.dval - self.internal.ldvl).abs()
                <= self.conv.mres.abs() + self.retry.bdst.abs()
        };
        let single_leg_slew =
            self.retry.bdst.abs() < self.conv.mres.abs() || (preferred && same_vel);
        let single_leg_bvel = !single_leg_slew && preferred && within_range;
        // Case 3: the two-leg correction through the pretarget.
        let backlash = !single_leg_slew && !single_leg_bvel;

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
        // MISS is not cleared at dispatch: C touches it only inside
        // maybeRetry (1072 set, 1092 clear), so a latched miss stays
        // visible through the next move until its evaluation lands
        // close enough.

        // tdir reflects the actual first-command direction
        self.stat.tdir = move_target > self.pos.drbv;
        // C 2526: cdir = (rdif < 0) ? 0 : 1 — the INTEGER raw error
        // posted at the move-block entry, so a sub-half-step negative
        // error (rdif rounds to 0) and a zero error under MRES < 0 both
        // read forward, unlike the f64 sign of `diff`.
        self.stat.cdir = self.pos.rdif >= 0;

        // Set MIP and phase. C 2461 is `mip |= MIP_MOVE` behind the
        // `mip == MIP_DONE || mip == MIP_RETRY` gate (2455): the RETRY
        // bit survives into the in-flight move, keeping the dispatch
        // marked as a retry (the AJF readback-link retry-count fix).
        let is_retry = self.stat.mip.contains(MipFlags::RETRY);
        self.stat.mip = MipFlags::MOVE;
        if is_retry {
            self.stat.mip.insert(MipFlags::RETRY);
        }
        self.set_phase(MotionPhase::MainMove);
        self.internal.backlash_pending = backlash;
        // C 2523: a move that will need a backlash correction arms pp
        // ("do backlash from postprocess()"). Collaterally, a Pause that
        // interrupts such a move syncs at its stop completion instead of
        // arming the paused-resume state — matching C.
        if backlash {
            self.internal.pp = true;
        }

        let frac = self.retry.frac;
        // C 2268: currpos ("where we are") is LDVL — the previously
        // dispatched target, NOT the readback. The absolute FRAC dispatch
        // (2488/2513) walks from the previous target toward the new one:
        // position = currpos + frac * (newpos - currpos). Only the
        // relative legs (relpos/relbpos, 2280-2281) measure from DRBV.
        // Captured before the load_pos update below overwrites ldvl.
        let currpos = self.internal.ldvl;

        // C parity (motorRecord.cc:2469 load_pos): ldvl/lval/lrvl reflect
        // the target being dispatched. For in-flight same-direction retarget,
        // this keeps (dval - ldvl) in the next is_preferred_direction call
        // tracking each successive target, not only the original one.
        self.internal.ldvl = self.pos.dval;
        self.internal.lval = self.pos.val;
        self.internal.lrvl = self.pos.rval;

        if backlash {
            // Case 3 (C 2518-2524): first leg to the pretarget at slew
            // speed, no FRAC; the final approach runs from postprocess.
            // The relative leg is the (RMOD-scaled) relbpos.
            self.emit_move(
                effects,
                use_rel,
                relbpos,
                move_target,
                self.vel.velo,
                self.move_accel_egu(),
            );
        } else if single_leg_slew {
            // Case 1 (C 2479-2489): one leg at slew speed; FRAC scales
            // the (RMOD-scaled) relpos (2487-2488).
            self.emit_move(
                effects,
                use_rel,
                relpos * frac,
                currpos + frac * (self.pos.dval - currpos),
                self.vel.velo,
                self.move_accel_egu(),
            );
        } else {
            // Case 2 (C 2491-2517): one leg at backlash speed, FRAC.
            self.emit_move(
                effects,
                use_rel,
                relpos * frac,
                currpos + frac * (self.pos.dval - currpos),
                self.vel.bvel,
                self.backlash_accel_egu(),
            );
        }
        effects.request_poll = true;
    }

    /// Handle STOP command.
    fn handle_stop(&mut self, effects: &mut ProcessEffects) {
        self.ctrl.stop = false; // pulse field
        self.stop_axis(effects);
    }

    /// The C do_work top-block stop path, shared by the STOP field pulse,
    /// SPMG=Stop (motorRecord.cc:1871: `spmg == motorSPMG_Stop ||
    /// stop == true`) and the rising-LVIO stop (1476-1482). Single owner
    /// so the entries cannot drift.
    pub(super) fn stop_axis(&mut self, effects: &mut ProcessEffects) {
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
            self.internal.pp = true;
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
        } else if self.stat.mip.contains(MipFlags::JOG_REQ) {
            // Idle with a parked jog request. C's stop branch early-
            // returns only for mip DONE/STOP/RETRY (1874-1888); a parked
            // MIP_JOG_REQ falls through to the wholesale `mip = MIP_STOP`
            // (1901-1905) — an explicit stop kills the parked request.
            // The button itself survives (clear_buttons runs only in the
            // movn branch, 1893), so Go can still re-arm from it.
            self.stat.mip = MipFlags::STOP;
        }
        // else: idle — bare STOP_AXIS only (C 1874-1888 early return for
        // mip DONE/STOP/RETRY keeps MIP untouched).
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
        // C 2106-2113: the dispatch consumes MIP_JOG_REQ wholesale (mip =
        // MIP_JOGF/JOGR) and the movn branch ORs in MIP_STOP, so a queued
        // jog reads back as JOGF|STOP. The replay discriminator stays
        // internal.queued_motion — a plain STOP wholesale-replaces MIP and
        // clears the buttons (C 1893/1903), so it never resembles this
        // state, and only an explicit queued request is re-fired.
        if self.stat.phase != MotionPhase::Idle && self.stat.movn {
            self.stat.mip = if forward {
                MipFlags::JOGF
            } else {
                MipFlags::JOGR
            } | MipFlags::STOP;
            // C 2110: pp = TRUE — the stop completion post-processes
            // (VAL/DVAL <- readback) before the queued jog re-fires.
            self.internal.pp = true;
            self.internal.queued_motion = Some(QueuedMotion::Jog { forward });
            self.internal.backlash_pending = false;
            effects.commands.push(MotorCommand::Stop {
                acceleration: self.move_accel_egu(),
            });
            effects.request_poll = true;
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
            // C 2095-2104: BOTH buttons release on refusal ("prevent
            // record from locking up in mip = JOG_REQ"), not just the
            // pressed one.
            self.ctrl.jogf = false;
            self.ctrl.jogr = false;
            // C 2092-2104 clears only the buttons and leaves a parked
            // MIP_JOG_REQ in mip (its arming lives in special(), blind to
            // internal clears). Here the request is dropped with its
            // button: JOG_REQ set means a latched, undispatched request.
            self.stat.mip.remove(MipFlags::JOG_REQ);
            self.limits.lvio = true;
            return;
        }

        self.stat.dmov = false;
        // C 2125: pp = TRUE at jog dispatch — a pause that interrupts the
        // jog post-processes (syncs) at its stop completion.
        self.internal.pp = true;

        if forward {
            self.stat.mip = MipFlags::JOGF;
        } else {
            self.stat.mip = MipFlags::JOGR;
        }
        self.set_phase(MotionPhase::Jog);
        self.emit_jog(forward, effects);
    }

    /// Dispatch the jog velocity command — the single owner of the
    /// `MoveVelocity` emission and its direction bookkeeping. The jog
    /// button is USER-frame (JOGF = jog toward growing user
    /// coordinate); the device command is DIAL-frame, so the button
    /// direction must fold through DIR: C `motorRecord.cc:2119`
    /// commands the raw velocity `jogv = (jvel * dir) / mres`, whose
    /// dial equivalent is `±jvel * dir`. CDIR stays RAW-frame
    /// (C 2126-2137: jogf, inverted once for MRES<0 and once for
    /// DIR=Neg). Both the fresh dispatch and the queued-jog replay
    /// (C re-enters the same do_work jog section on replay) come
    /// through here so the commanded direction and the CDIR ledger
    /// cannot disagree.
    pub(super) fn emit_jog(&mut self, forward: bool, effects: &mut ProcessEffects) {
        // Remember jog direction for backlash (MIP flags get cleared by stop_jog)
        self.internal.jog_was_forward = forward;
        let dial_forward = forward != (self.conv.dir == MotorDir::Neg);
        self.stat.cdir = dial_forward != (self.conv.mres < 0.0);
        effects.commands.push(MotorCommand::MoveVelocity {
            direction: dial_forward,
            velocity: self.vel.jvel,
            acceleration: self.jog_accel_egu(),
        });
        effects.request_poll = true;
    }

    /// Stop jogging.
    fn stop_jog(&mut self, effects: &mut ProcessEffects) {
        // C 2152: pp = TRUE — "When stopped, process() will correct
        // backlash" (and sync the drive fields via postProcess).
        self.internal.pp = true;
        // C 2153-2154: mip |= MIP_JOG_STOP; mip &= ~(MIP_JOGF | MIP_JOGR).
        // With the JOG bits gone, the deceleration is no longer "a jog"
        // to the enter_do_work LVIO recompute (1466 gates on MIP_JOG,
        // which excludes JOG_STOP) — the latch is preserved while
        // stopping. The jog direction survives in jog_was_forward.
        self.stat.mip.insert(MipFlags::JOG_STOP);
        self.stat.mip.remove(MipFlags::JOGF | MipFlags::JOGR);
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
        // C 2023-2033: mip = MIP_HOMF/HOMR wholesale, then the movn branch
        // ORs in MIP_STOP — a queued home reads back as HOMF|STOP. The
        // replay discriminator stays internal.queued_motion (see
        // start_jog).
        if self.stat.phase != MotionPhase::Idle && self.stat.movn {
            self.stat.mip = if forward {
                MipFlags::HOMF
            } else {
                MipFlags::HOMR
            } | MipFlags::STOP;
            // C 2025: pp = TRUE — the stop completion post-processes
            // (VAL/DVAL <- readback) before the queued home re-fires.
            self.internal.pp = true;
            self.internal.queued_motion = Some(QueuedMotion::Home { forward });
            self.internal.backlash_pending = false;
            self.internal.pending_retarget = None;
            effects.commands.push(MotorCommand::Stop {
                acceleration: self.move_accel_egu(),
            });
            effects.request_poll = true;
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
        // C 2069: the home dispatch resets the retry counter.
        self.retry.rcnt = 0;
        // C 2025: pp = TRUE at home dispatch (set for both the queued and
        // the direct branch before the movn test).
        self.internal.pp = true;

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
    }

    /// Handle tweak (TWF/TWR).
    /// C tweak collection (motorRecord.cc:2167-2181) plus the VAL change
    /// it feeds (2204-2235): fold the latched tweak button(s) into VAL —
    /// `val += twv * (twf ? 1 : -1)`, TWF wins when both are latched —
    /// and release BOTH buttons. The fold acts on latched button STATE,
    /// not on the src that triggered the pass, so a bare TWF/TWR put
    /// overtaken in `last_write` still lands. In SET mode the VAL change
    /// redefines coordinates without moving (C 2207-2227). Returns true
    /// when a non-SET fold landed in DVAL — a move toward it is now
    /// pending and the caller's pass dispatches (or, under SPMG
    /// Stop/Pause, SPMG=Go later fires it via `dval != ldvl`).
    fn collect_tweak(&mut self, effects: &mut ProcessEffects) -> bool {
        if !self.ctrl.twf && !self.ctrl.twr {
            return false;
        }
        let forward = self.ctrl.twf;
        self.ctrl.twf = false;
        self.ctrl.twr = false;

        let dir = if forward {
            MotionDirection::Positive
        } else {
            MotionDirection::Negative
        };
        if self.is_blocked_by_hw_limit(dir) {
            return false;
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
                return false;
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
            return false;
        }

        // Normal (non-SET) tweak: cascade VAL->DVAL; the caller moves.
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
        true
    }

    /// Handle SPMG mode change.
    fn handle_spmg_change(&mut self, effects: &mut ProcessEffects) {
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
                // C (1898-1911): the Pause pass runs unconditionally —
                // even on an idle axis it sets mip = MIP_STOP and sends
                // STOP_AXIS ("just in case" the driver is still settling);
                // the next completion pass collapses the bare MIP_STOP via
                // maybeRetry's close-enough path, and Go round-trips it to
                // DONE (1922-1925). Pause does NOT set pp — the drive
                // fields keep the target so Go can resume. `mip =
                // MIP_STOP` erases a queued jog (MIP_JOG_REQ) and a
                // queued/active home (MIP_HOMF|HOMR), and clear_buttons()
                // drops the home buttons: a paused jog resumes from its
                // still-latched button on Go, a canceled home does not.
                if matches!(self.internal.queued_motion, Some(QueuedMotion::Home { .. })) {
                    self.ctrl.homf = false;
                    self.ctrl.homr = false;
                }
                self.internal.queued_motion = None;
                self.internal.pending_retarget = None;
                // C 1901-1905: `mip = MIP_STOP` — wholesale, erasing a
                // parked MIP_JOG_REQ and the JOGF/HOMF bits of a queued
                // request — except while waiting on DLY ("keep it,
                // otherwise the record may lock up").
                if !self.stat.mip.contains(MipFlags::DELAY_REQ) {
                    self.stat.mip = MipFlags::STOP;
                }
                effects.commands.push(MotorCommand::Stop {
                    acceleration: self.move_accel_egu(),
                });
            }
            SpmgMode::Go => {
                // C top-block Go branch (1909-1925) runs UNGATED — there
                // is no idle/phase test, so a Go written while the pause
                // stop is still decelerating acts on this very pass: a
                // still-latched jog button re-arms the jog (MIP_JOG_REQ;
                // start_jog queues it when the axis is still moving,
                // C 2106); else a bare mip == MIP_STOP collapses to DONE
                // and the pass falls through to the move block (2241),
                // whose dispatch re-fires on `dval != ldvl || !dmov` —
                // the new MOVE supersedes the decelerating stop in the
                // controller. Gating this on an idle phase consumed the
                // Go edge with no replay: the paused move never resumed
                // (stuck at DMOV=0, MIP=RETRY, SPMG reading Go).
                //
                // C 1914: `(jogf && !hls) || (jogr && !lls)` — a latched
                // button at its limit switch does not re-arm; the pass
                // falls to the STOP collapse instead of leaving the
                // record stuck in MIP_STOP.
                if (self.ctrl.jogf && !self.limits.hls) || (self.ctrl.jogr && !self.limits.lls) {
                    let forward = self.ctrl.jogf;
                    self.start_jog(forward, effects);
                } else {
                    if self.stat.mip == MipFlags::STOP {
                        self.stat.mip = MipFlags::empty();
                    }
                    // C 2455 dispatch gate: only mip == MIP_DONE or
                    // MIP_RETRY may issue the move. A queued jog/home
                    // (JOGF|STOP, HOMF|STOP) keeps its direction bits, so
                    // it is NOT re-dispatched here — it replays at stop
                    // completion exactly as before.
                    if (self.stat.mip.is_empty() || self.stat.mip == MipFlags::RETRY)
                        && (!self.stat.dmov || self.pos.dval != self.internal.ldvl)
                    {
                        self.plan_absolute_move(effects);
                    }
                }
            }
            SpmgMode::Move => {
                // One-shot: like Go but will restore to Pause after
                // completion. C top-block else-branch (1927-1933) runs
                // ungated and assigns mip = MIP_DONE WHOLESALE with
                // rcnt = 0 — a Move resume abandons the retry accounting
                // AND any queued jog/home (their MIP direction bits are
                // overwritten), then the move block re-fires exactly as
                // for Go.
                self.stat.mip = MipFlags::empty();
                self.retry.rcnt = 0;
                self.internal.queued_motion = None;
                if !self.stat.dmov || self.pos.dval != self.internal.ldvl {
                    self.plan_absolute_move(effects);
                }
            }
        }
        // C 2540-2544: a Go/Move pass that resumed nothing reaches the
        // do_work chain end, where a SYNC latched under Stop/Pause
        // applies. The internal gates refuse while a resume is in flight
        // (phase left Idle) — the latch then consumes at its completion.
        self.apply_latent_sync();
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

    /// C refusal of a new motion (2434-2452, the `lvio || rtnstat == FALSE`
    /// gate): the drive fields roll back to the last dispatched target
    /// (VAL/DVAL/RVAL <- LVAL/LDVL/LRVL), an armed retry collapses to
    /// DONE, and a pending completion (DMOV still low with nothing left
    /// in flight) finishes with DMOV = TRUE.
    pub(crate) fn refuse_move_restore_lasts(&mut self) {
        self.pos.val = self.internal.lval;
        self.pos.dval = self.internal.ldvl;
        self.pos.rval = self.internal.lrvl;
        if self.stat.mip.contains(MipFlags::RETRY) {
            self.stat.mip = MipFlags::empty();
        }
        if self.stat.mip.is_empty() && !self.stat.dmov {
            self.stat.dmov = true;
            self.set_phase(MotionPhase::Idle);
        }
    }

    /// C postProcess floors a backlash-leg velocity that does not exceed
    /// VBAS to one raw step/s above it (936-937, 953-954, 1002-1003):
    /// raw `vel = vbase + 1` is `vbas + |mres|` in EGU. The move-block
    /// dispatch (2240-2540) has no such clamp — postProcess legs only.
    pub(crate) fn backlash_leg_velocity(&self, vel: f64) -> f64 {
        let vbas = self.effective_vbas();
        if vel <= vbas {
            vbas + self.conv.mres.abs()
        } else {
            vel
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
        // C preferred_dir (motorRecord.cc:2391-2395):
        //   ((use_rel == false) && ((dval > ldvl) == (bdst > 0))) ||
        //   ((use_rel == true)  && ((diff > 0)    == (bdst > 0)))
        // Strict comparisons, no equality slack: a retry (dval == ldvl)
        // with BDST > 0 is NON-preferred, which is what makes C retries
        // re-run the backlash correction.
        let toward_positive = if self.use_relative_moves() {
            dval - drbv > 0.0
        } else {
            dval > self.internal.ldvl
        };
        toward_positive == (self.retry.bdst > 0.0)
    }

    /// Handle a new target (VAL/DVAL/RVAL/RLV write) that arrives while a
    /// motion is in progress.
    ///
    /// C behavior (`motorRecord.cc`), for reference:
    ///
    /// - A put-initiated process always reaches `do_work`, even while the
    ///   axis is moving (gate at 1487-1491, `process_reason !=
    ///   CALLBACK_DATA`). The move block is *entered* on every
    ///   `dval != ldvl || !dmov` (2240) — independent of NTM — and
    ///   val/dval/rval take the new target, but the command dispatch
    ///   inside is gated at 2455 on `mip == MIP_DONE || mip == MIP_RETRY`,
    ///   so C never sends a new move while MIP_MOVE or MIP_STOP is
    ///   active. The parked target converges at completion: maybeRetry
    ///   (1431) measures `diff` against the *new* dval, collapses MIP to
    ///   RETRY/DONE, and the same pass re-enters `do_work` where the 2455
    ///   gate now passes and dispatches. An earlier Rust version returned
    ///   [`RetargetAction::Ignore`] whenever `ntm == No` (its
    ///   `motorRecord.dbd` default, since NTM has no `initial()`),
    ///   silently discarding the write — NTM does not gate this at all.
    /// - NTM gates only the *opposite-direction* stop-first path in the
    ///   `process()` `movn` block (1326-1341): when
    ///   `ntm == menuYesNoYES && sign_rdif != cdir &&
    ///   fabs(diff) > ntm_deadband && move_or_retry && !MIP_STOP`, C
    ///   sends `STOP_AXIS` with `pp = FALSE` so the new target survives
    ///   the stop completion and dispatches from there.
    ///
    /// Rust divergence (deliberate): [`RetargetAction::ExtendMove`]
    /// dispatches the new target in-flight instead of parking it until
    /// completion, so controllers that accept on-the-fly retargets track
    /// the newest target immediately; the completion-time verification
    /// armed by the ExtendMove arm in `plan_motion` restores C's
    /// park-then-dispatch behavior for controllers that ignore in-flight
    /// retargets. `plan_absolute_move`'s own too-small/SPDB deadband
    /// checks suppress no-op moves. NTM mapping matches C: only an
    /// opposite-direction, beyond-deadband retarget promotes to
    /// [`RetargetAction::StopAndReplan`].
    pub fn handle_retarget(&mut self, new_dval: f64) -> RetargetAction {
        // Only retarget during an active move, retry, or stop
        // deceleration. MIP_STOP counts as in-flight because the C stop
        // path replaces MIP wholesale (`mip = MIP_STOP`,
        // motorRecord.cc:1903) while the axis keeps decelerating.
        let in_move = self
            .stat
            .mip
            .intersects(MipFlags::MOVE | MipFlags::RETRY | MipFlags::STOP);
        if !in_move {
            return RetargetAction::Ignore;
        }
        // Deliberate divergence from C: a target written during a
        // commanded stop deceleration is honored here. In C the write
        // reaches the do_work move block (entry 2240) but the 2455
        // dispatch gate refuses while MIP_STOP, and the completion
        // replay (motorRecord.cc:1384-1386) excludes MIP_STOP /
        // MIP_JOG_STOP, so postProcess (827-849) syncs VAL/DVAL back to
        // the readback — the write is silently lost (the kohzuCtl
        // stop-then-rewrite retarget sequence hits exactly this window).
        // Retargeting through the stop keeps the explicit user target
        // instead; `mip = MIP_MOVE` replaces the stop state. The NTM
        // stop-first branch stays excluded while stopping (C gate at
        // 1330: `(pmr->mip & MIP_STOP) == 0`), so no double-stop is
        // possible.
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
        // beyond the NTM deadband AND NTM enabled. The direction sign is
        // C 1303 `sign_rdif = (rdif < 0) ? 0 : 1` — the RAW-frame INTEGER
        // error, like CDIR itself (2526): under MRES < 0 the dial sign
        // inverts, and a sub-half-step error rounds to 0 (forward).
        let rdif = if self.conv.mres != 0.0 {
            (diff / self.conv.mres).round() as i64
        } else {
            0
        };
        let sign_rdif = rdif >= 0;
        let direction_changed = sign_rdif != self.stat.cdir;

        if self.timing.ntm && direction_changed && diff.abs() > deadband {
            RetargetAction::StopAndReplan
        } else {
            // Same-direction or within-deadband: extend the move
            // in-flight. C parks the new target until completion behind
            // the 2455 dispatch gate; see the doc comment above for the
            // divergence rationale.
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

    /// C SYNC apply (motorRecord.cc:2540-2544): `else if (sync != 0 &&
    /// mip == MIP_DONE)` at the end of the do_work dispatch chain — the
    /// latched SYNC consumes only on a pass that dispatched nothing, with
    /// the record idle and done. SPMG Stop/Pause never reaches the chain
    /// end (the stop_or_pause return at 2237 precedes it), so the latch
    /// survives a pause and applies on Go. Returns true when it applied.
    pub(super) fn apply_latent_sync(&mut self) -> bool {
        if !self.internal.sync
            || self.stat.phase != MotionPhase::Idle
            || !self.stat.mip.is_empty()
            || !self.stat.dmov
            || !matches!(self.ctrl.spmg, SpmgMode::Go | SpmgMode::Move)
        {
            return false;
        }
        self.internal.sync = false;
        self.sync_positions();
        true
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
        // C 2085-2086: soft limits disabled when DHLM == DLLM == 0.0 —
        // the DIAL pair, like the move-block gate (a nonzero OFF leaves
        // the user pair offset, so HLM == LLM == 0.0 is the wrong test).
        if self.limits.dhlm == self.limits.dllm && self.limits.dllm == 0.0 {
            return false;
        }
        // C 2089: inverted limits always refuse.
        if self.limits.dllm > self.limits.dhlm {
            return true;
        }
        // C 2087-2088: USER frame, strict compare, one JVEL of margin —
        // a jog must have room to decelerate before the limit.
        if forward {
            self.pos.val > self.limits.hlm - self.vel.jvel
        } else {
            self.pos.val < self.limits.llm + self.vel.jvel
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
        let jvel_retune = std::mem::take(&mut self.jog_retune_pending);
        let mut effects = self.do_process_inner();
        if stup_requested {
            effects.status_refresh = true;
        }
        // C special() motorRecordJVEL (motorRecord.cc:3059-3072): a JVEL
        // write landing on an active jog retunes it in place — C sends
        // SET_ACCEL + JOG_VELOCITY immediately from special(). Every put
        // is followed by a process pass here, so the JVEL put arm parks a
        // one-shot request and this pass re-emits the jog command with
        // the new JVEL/JAR. Direction comes from the active MIP jog bit,
        // folded to the dial frame exactly as emit_jog does.
        if jvel_retune && self.stat.mip.intersects(MipFlags::JOGF | MipFlags::JOGR) {
            let forward = self.stat.mip.contains(MipFlags::JOGF);
            let dial_forward = forward != (self.conv.dir == MotorDir::Neg);
            effects.commands.push(MotorCommand::MoveVelocity {
                direction: dial_forward,
                velocity: self.vel.jvel,
                acceleration: self.jog_accel_egu(),
            });
            effects.request_poll = true;
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
                // C callbackFunc (motorRecord.cc:460-480): the delay
                // watchdog may have been rescinded between arming and
                // firing — a new move dispatched during the wait replaces
                // MIP wholesale and drops DELAY_REQ. Only a still-armed
                // DELAY_REQ turns into ACK (plus the GET_INFO-equivalent
                // status refresh); an orphaned expiry does nothing, so it
                // cannot inject a stale completion evaluation into the
                // new motion.
                let mut effects = ProcessEffects::default();
                if self.stat.mip.contains(MipFlags::DELAY_REQ) {
                    self.stat.mip.remove(MipFlags::DELAY_REQ);
                    self.stat.mip.insert(MipFlags::DELAY_ACK);
                    effects.status_refresh = true;
                }
                effects
            }
            None => {
                // A pass with no command source and no device event — in
                // the live IOC this is the process cycle a put to a field
                // that sets no last_write triggers (HLM/LLM/DHLM/DLLM are
                // pp(TRUE) in C). C still runs the enter_do_work LVIO
                // re-evaluation (1462-1483) on it: a limit lowered onto a
                // running jog must stop the axis on the write pass.
                if self.recompute_lvio_during_motion() {
                    let mut effects = ProcessEffects::default();
                    self.stop_axis(&mut effects);
                    return effects;
                }
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
