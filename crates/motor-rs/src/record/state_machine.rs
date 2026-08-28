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
            // C 3692-3694: the failed-RDBL stop also releases every
            // latched motion button, so the halted axis is not
            // re-commanded by a jog/home still held high.
            self.clear_buttons();
            self.stat.mip.insert(MipFlags::STOP);
            // An error stop post-processes like a commanded stop (sync at
            // completion), not like a Pause (resume armed).
            self.internal.pp = true;
            effects.commands.push(MotorCommand::Stop {
                acceleration: self.move_accel_egu(),
            });
            return effects;
        }

        // C post-delay readback pass (motorRecord.cc:1427-1439):
        // `stup != motorSTUP_ON && mip != MIP_DONE` gates maybeRetry.
        // In C every completion that must not retry-evaluate (commanded
        // stop, jog stop, home done) had postProcess collapse MIP to
        // DONE before the delay armed, so its post-delay pass falls
        // through without maybeRetry; only a move/backlash/retry
        // completion still carries motion bits. Rust keeps the
        // completion-type bits in MIP through DelayWait, so the same
        // key reads as "MIP still carries MOVE/MOVE_BL/RETRY" — the
        // finalize family must not reach the close-enough branch,
        // which clears MISS and restores SPMG Move->Pause. The stup
        // half of the key is structural here: a STUP acknowledgment
        // pass returns before check_completion (do_process_inner).
        if self.stat.mip.contains(MipFlags::DELAY_ACK) {
            self.stat.mip.remove(MipFlags::DELAY_ACK);
            if self
                .stat
                .mip
                .intersects(MipFlags::MOVE | MipFlags::MOVE_BL | MipFlags::RETRY)
            {
                self.evaluate_position_error(&mut effects);
            } else {
                self.finalize_motion(&mut effects);
            }
            return effects;
        }

        // C process (1301/1345) branches the callback on MOVN, not on
        // the raw RA_DONE bit — and process_motor_info (3741-3748)
        // computes movn = 0 for ls_active and RA_PROBLEM too, so an
        // axis stopped by its limit switch or a driver fault completes
        // (maybeRetry's ls_blocks then refuses the retry) instead of
        // waiting forever for a DONE that never comes.
        let driver_done = !self.stat.movn;

        if !driver_done {
            // C movn block (1327-1342): poll-time NTM. The axis moving
            // OPPOSITE to the commanded direction (raw-frame rounded
            // sign vs CDIR, both refreshed by process_motor_info) beyond
            // ntmf*(|bdst|+rdbd) while a move/retry is in flight stops
            // it — "We're going in the wrong direction. Readback
            // problem?". pp = FALSE (1342): the stop completion skips
            // postProcess, so maybeRetry re-evaluates against the intact
            // target and re-dispatches. Runs before the LVIO recompute,
            // like C's movn block preceding enter_do_work.
            if self.timing.ntm
                && (self.pos.rdif >= 0) != self.stat.cdir
                && self.pos.diff.abs()
                    > self.timing.ntmf as f64 * (self.retry.bdst.abs() + self.retry.rdbd)
                && self.stat.mip.intersects(MipFlags::MOVE | MipFlags::RETRY)
                && !self.stat.mip.contains(MipFlags::STOP)
            {
                self.stat.mip.insert(MipFlags::STOP);
                self.internal.pp = false;
                effects.commands.push(MotorCommand::Stop {
                    acceleration: self.move_accel_egu(),
                });
                return effects;
            }
            // C enter_do_work (1463-1484) re-evaluates LVIO on every
            // process pass; for an in-flight motion the poll is the pass
            // where RBV changes. A rising violation outside SET mode
            // stops the axis and releases the buttons in the same pass
            // (1475-1484 sets stop=1, the do_work top block consumes it).
            if self.recompute_lvio_during_motion() {
                self.stop_axis(&mut effects);
                return effects;
            }
            // Still moving — poll loop is already active.
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
            // buttons, C motorRecord.cc:1893/1905). The queue parked MIP
            // as JOGF/JOGR|STOP or HOMF/HOMR|STOP (2028/2111), so dropping
            // STOP leaves the direction bit — like C postProcess re-firing
            // a home with `mip &= ~MIP_STOP` (866) — and the resumed
            // motion stays visible to the LVIO recompute and MIP readback.
            //
            // A queued jog whose button was released while the stop was
            // in flight is dead — C kills it on the release pass itself
            // (2148-2155 drop JOGF/JOGR from MIP); a release that only
            // landed in the button state must not resurrect it here.
            // Fall through to the plain pp stop instead. A queued home
            // re-fires regardless of its button, like C postProcess
            // (856-891 keys on MIP alone).
            if let Some(QueuedMotion::Jog { forward }) = self.internal.queued_motion {
                let held = if forward {
                    self.ctrl.jogf
                } else {
                    self.ctrl.jogr
                };
                if !held {
                    self.internal.queued_motion = None;
                    self.stat.mip.remove(MipFlags::JOGF | MipFlags::JOGR);
                }
            }
            if let Some(queued) = self.internal.queued_motion.take() {
                self.stat.mip.remove(MipFlags::STOP);
                // C: the queued request armed pp=TRUE (2025/2110), so this
                // stop completion runs postProcess first — VAL/DVAL/RVAL
                // sync to the rest position (827-849) — and only then
                // re-fires the request (859-890). A target written while
                // the stop was in flight is dropped here, like C's replay
                // gate refusing `mip & MIP_STOP` (1385). The re-fired
                // motion starts with pp armed again (887, 2125), so a
                // pause interrupting it syncs at its own stop completion.
                self.postprocess_sync();
                self.internal.pp = true;
                match queued {
                    QueuedMotion::Home { forward } => {
                        self.set_phase(MotionPhase::Homing);
                        // C 869: the postProcess home re-fire resets the
                        // retry counter, like the direct dispatch (2069).
                        self.retry.rcnt = 0;
                        let hw_forward = if self.conv.mres >= 0.0 {
                            forward
                        } else {
                            !forward
                        };
                        self.stat.cdir = hw_forward;
                        effects.commands.push(MotorCommand::Home {
                            forward: hw_forward,
                            min_velocity: self.effective_vbas(),
                            velocity: self.vel.hvel,
                            acceleration: self.home_accel_egu(),
                        });
                    }
                    QueuedMotion::Jog { forward } => {
                        self.set_phase(MotionPhase::Jog);
                        // C replays through the same do_work jog section
                        // (2117-2144), re-deriving both the commanded
                        // direction and CDIR — route through the single
                        // jog-emission owner.
                        self.emit_jog(forward, &mut effects);
                    }
                }
                effects.request_poll = true;
                return effects;
            }
            // Plain stop. C discriminates by pp (motorRecord.cc:1382-1402):
            // a commanded stop (or interrupted jog/home/backlash, all of
            // which armed pp) runs postProcess — VAL/DVAL/RVAL <- readback
            // and MIP_STOP cleared (827-849, 1027-1032) — so maybeRetry
            // never runs (mip == MIP_DONE at its 1432 gate).
            let pp = std::mem::take(&mut self.internal.pp);
            if pp {
                self.postprocess_sync();
                self.finalize_or_delay(&mut effects);
                return effects;
            }
            // A Pause never set pp: postProcess is skipped, mip stays
            // MIP_STOP, and maybeRetry (1040-1100) runs against the
            // preserved target — unless a limit switch in the commanded
            // direction ended the coast, which C makes terminal ahead of
            // both the pp test and maybeRetry (1367-1380), so the pause is
            // not resumable and the drive fields adopt the limit readback.
            let diff = (self.pos.dval - self.pos.drbv).abs();
            let ls_blocks = self.sync_if_limit_stopped();
            if diff >= self.retry.rdbd && !ls_blocks && self.retry.rtry != 0 {
                if self.retry.rcnt >= self.retry.rtry {
                    // C 1059-1074: too many retries — give up, MISS
                    // latches, the lasts adopt the unreached target
                    // (finalize_motion's lval/ldvl sync matches 1067-1069).
                    // The give-up pass still counts (`++rcnt > rtry`).
                    self.retry.rcnt += 1;
                    self.retry.miss = true;
                    self.finalize_or_delay(&mut effects);
                    // C maybeRetry give-up (1063-1065) re-arms MIP_JOG_REQ
                    // from the held field and do_work re-fires same-pass —
                    // identical to the evaluate_position_error give-up, since
                    // both reach the one maybeRetry. The gate inside
                    // dispatch_latent_collection (can_accept_command) keeps
                    // this Pause-reached path parked until Go (matching the
                    // retry branch's can_accept_command gate at 240) while an
                    // NTM-stop give-up under Go re-fires the held jog now.
                    self.dispatch_latent_collection(&mut effects, false);
                } else {
                    // C 1077-1082 with 1356's dmov=TRUE UNMARKed and
                    // reversed: DMOV never posts 1 — the move stays "not
                    // done" with mip = MIP_RETRY armed. C's maybeRetry only
                    // arms; the SAME pass re-enters do_work, where SPMG
                    // Stop/Pause returns at 2237 (the armed retry parks
                    // until Go) but Go/Move reaches the 2455 dispatch gate
                    // and re-fires immediately — the poll-time NTM stop
                    // resumes toward the intact target this way.
                    self.retry.rcnt += 1;
                    self.stat.mip = MipFlags::RETRY;
                    if self.can_accept_command() {
                        self.plan_absolute_move(&mut effects);
                    } else {
                        self.set_phase(MotionPhase::Idle);
                    }
                }
            } else {
                // Close enough, RTRY disabled, or LS blocked: C maybeRetry
                // else-branch (1084-1102) — collapse to DONE. Only the LS
                // arm syncs, and `sync_if_limit_stopped` has already done
                // it; on the other two the lasts already equal the reached
                // target from the dispatch-time load. MISS clears and SPMG restores
                // Move->Pause only on the close-enough/LS path
                // (1087-1101); the rtry==0 path leaves both. DLY arms
                // only on a real completion edge (C 1457 needs a fresh
                // M_DMOV mark): an idle Pause convergence pass (dmov
                // already true) finalizes quietly.
                if diff < self.retry.rdbd || ls_blocks {
                    self.retry.miss = false;
                    self.restore_spmg_move_to_pause();
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
                    self.settle_then_evaluate(&mut effects);
                }
            }
            MotionPhase::BacklashFinal => {
                self.settle_then_evaluate(&mut effects);
            }
            MotionPhase::Retry => {
                self.settle_then_evaluate(&mut effects);
            }
            MotionPhase::Jog | MotionPhase::JogStopping => {
                // C 1357-1364 (9c8a8e8c, PR #56): the driver reported done
                // while the record still thinks it is jogging (no commanded
                // stop — that would have moved us to JogStopping or
                // MIP_STOP). The controller stopped on its own (internal
                // limit, fault, host stop): clear_buttons() so the latched
                // button does not re-fire the jog on the next pass.
                if self.stat.phase == MotionPhase::Jog {
                    self.clear_buttons();
                    // C 1357-1364 collapses the sudden stop to MIP_DONE,
                    // so the replay gate (1385) passes — a VAL written
                    // during the jog re-fires now. A commanded stop
                    // (JogStopping = MIP_JOG_STOP) is excluded: the write
                    // is dropped by the sync below, like C postProcess.
                    if self.replay_overtaken_target(&mut effects) {
                        return effects;
                    }
                    // C: with mip collapsed to MIP_DONE, postProcess
                    // matches none of its re-fire branches — the backlash
                    // correction needs MIP_JOG_STOP or MIP_MOVE (908) —
                    // so a controller self-stop syncs and finalizes
                    // WITHOUT a jog backlash move.
                    self.postprocess_sync();
                    self.finalize_or_delay(&mut effects);
                    return effects;
                }
                // Commanded stop (C postProcess MIP_JOG_STOP branch,
                // 908-947): sync VAL<-RBV, DVAL<-DRBV first so
                // start_jog_backlash uses the jog-end position as base,
                // then correct when |BDST| >= |MRES|.
                self.postprocess_sync();
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
                // No ATHM write here: C never sets it outside the poll —
                // process_motor_info owns it as pure home-switch readback
                // (3755-3762).
                // C postProcess home-done (893-906): clear the HOMF/HOMR
                // buttons at home completion. The dispatch leaves the
                // button latched (it reads back 1 for the whole home), so
                // this is the lifecycle owner for the done path; stops
                // and pauses clear via clear_buttons (1893/1899-1900).
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
                self.postprocess_sync();
                self.finalize_or_delay(&mut effects);
            }
            MotionPhase::DelayWait => {
                // Waiting out DLY. A poll landing here only refreshes the
                // readback (process_motor_info already applied it) — C
                // restores dmov=FALSE and exits until the watchdog fires
                // (motorRecord.cc:1441-1455). The DELAY_ACK pass above
                // owns the post-settle evaluation; finalizing here would
                // truncate the delay on the first poll tick.
            }
            MotionPhase::Idle => {
                // C process (1405-1409, postProcess 851-852): the
                // GET_INFO callback after a LOAD_POS collapses MIP_LOAD_P
                // to MIP_DONE and DMOV returns TRUE — no DLY, no retry
                // evaluation, no drive-triplet resync (load_pos synced
                // ldvl/lval/lrvl at dispatch). The pass then continues
                // into do_work like any stopped CALLBACK_DATA pass: the
                // collection below runs the move-block set test, which
                // replays a second redefinition written during the load
                // (its dval != ldvl divergence was parked by the
                // MIP_LOAD_P skip in load_pos's set test). The pp and
                // EXTERNAL blocks below self-gate: load_pos arms
                // neither, and MIP is empty after the collapse.
                if self.stat.mip.contains(MipFlags::LOAD_P)
                    && self.stat.msta.contains(MstaFlags::DONE)
                    && !self.stat.movn
                {
                    self.stat.mip = MipFlags::empty();
                    self.stat.dmov = true;
                }
                // C process (1382-1402): a done status callback with pp
                // armed and no motion runs postProcess, whose first
                // block re-syncs the drive values from the readback
                // (827-849). The SET-mode resolution re-anchor
                // (do_work 1981-1987: pp = TRUE + GET_INFO) completes
                // here, re-deriving VAL/DVAL/RVAL at the new
                // resolution.
                if self.internal.pp && self.stat.msta.contains(MstaFlags::DONE) && !self.stat.movn {
                    self.internal.pp = false;
                    self.postprocess_sync();
                    // This early-return quiesces the axis without finalize_motion
                    // and dispatches nothing, deferring a held button to a later
                    // pass. pp is now cleared, so the next forced poll falls
                    // through to dispatch_latent_collection (427) and resumes it.
                    self.request_poll_for_held_button(&mut effects);
                    return effects;
                }
                // C: ea063f5f — if the record marked an externally initiated
                // move (MIP_EXTERNAL set during process_motor_info), close
                // the loop once the driver reports done. Reseed VAL/DVAL/RVAL
                // from the readback and clear MIP.
                if self.stat.mip.contains(MipFlags::EXTERNAL)
                    && self.stat.msta.contains(MstaFlags::DONE)
                    && !self.stat.movn
                {
                    self.postprocess_sync();
                    self.finalize_motion(&mut effects);
                }
                // C do_work gate (motorRecord.cc:1487-1492): a
                // done-record callback falls into do_work through the
                // dmov arm, where the home/jog/tweak sections act on
                // latched button state. The idle poll is the
                // level-triggered pass that picks up a button latched
                // while its gate failed — limit switch active, or the
                // closed-loop DOL collection bypass — once the gate
                // clears. CALLBACK_DATA pass — the implicit GET_INFO
                // (C 2546) stays off; the pp completion return above
                // skips one pass and the next poll lands here.
                self.dispatch_latent_collection(&mut effects, false);
            }
        }

        effects
    }

    /// Either start DLY wait or finalize immediately.
    pub(crate) fn finalize_or_delay(&mut self, effects: &mut ProcessEffects) {
        if self.timing.dly > 0.0 {
            self.set_phase(MotionPhase::DelayWait);
            self.stat.mip.insert(MipFlags::DELAY_REQ);
            effects.schedule_delay = Some(epics_base_rs::runtime::time::duration_from_secs(
                self.timing.dly,
            ));
        } else {
            self.finalize_motion(effects);
        }
    }

    /// Finalize motion: set Idle, DMOV=true.
    pub(crate) fn finalize_motion(&mut self, effects: &mut ProcessEffects) {
        self.set_phase(MotionPhase::Idle);
        self.stat.mip = MipFlags::empty();
        self.stat.dmov = true;
        self.stat.movn = false;
        // RCNT is NOT zeroed at completion: C resets it only at dispatch
        // sites — a non-retry move (2352-2356), home (2069, 869), and a
        // Move-resume (1931). After a give-up it stays at rtry+1 (the
        // ++rcnt of the exhausted maybeRetry pass) until the next move.
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
        // No SPMG Move->Pause restore here: C performs it only in
        // maybeRetry's close-enough branch (1096-1101) — see
        // restore_spmg_move_to_pause(). A completion that reached this
        // point through that branch has already restored.
        //
        // C 2540-2544: a SYNC latched during the motion consumes at the
        // completion pass (do_work runs on dmov, 1485 gate; the move
        // block no longer fires, so the chain end is reached). After a
        // maybeRetry Move->Pause restore the gate refuses, like C's
        // stop_or_pause return on that pass; completions that keep
        // SPMG=Move reach the chain end and apply, like C.
        self.apply_latent_sync();

        // SYNC is already consumed above and queued_motion was cleared, but a
        // jog/home/tweak button latched across this completion still resumes
        // only on a later pass — request the forced poll that delivers it.
        self.request_poll_for_held_button(effects);
    }

    /// A jog/home/tweak button latched across a quiescing record pass (a motion
    /// completion, or the SET-mode pp-resync early-return) resumes only on a
    /// subsequent pass, where the Idle-arm `dispatch_latent_collection` re-fires
    /// it. The poll loop now notifies the record only on a changed status (the C
    /// `statusChanged_` gate, asynMotorAxis.cpp:316-322), so a now-stationary
    /// axis would never deliver that pass and the held button would strand.
    /// Request one forced poll (→ `PollDirective::Refresh`) so the deferred
    /// level-triggered action gets its pass. C strands this case (`special()`
    /// arms MIP_JOG_REQ only at mip==MIP_DONE, motorRecord.cc:3045, so a button
    /// pressed during a move is never armed); preserving the resume is a
    /// deliberate divergence from C's strand of an actively-held button.
    ///
    /// Single owner of the rule, called at every quiescing site so the closed-
    /// loop DOL pull stays excluded (the change-gate leaves it to CP-link/SCAN,
    /// as C does). Bounded to one forced poll: a limit-blocked jog/home returns
    /// from `dispatch_latent_buttons` without re-entering a quiescing path,
    /// `collect_tweak` clears twf/twr on its first attempt, and the pp-resync
    /// clears `pp` before its forced poll so it cannot re-trigger.
    pub(crate) fn request_poll_for_held_button(&self, effects: &mut ProcessEffects) {
        if self.can_accept_command()
            && (self.ctrl.jogf
                || self.ctrl.jogr
                || self.ctrl.homf
                || self.ctrl.homr
                || self.ctrl.twf
                || self.ctrl.twr)
        {
            effects.request_poll = true;
        }
    }

    /// C maybeRetry close-enough restore (motorRecord.cc:1097-1101): a
    /// motion initiated by the SPMG "Move" one-shot reverts to Pause
    /// when the target is reached. Only that maybeRetry branch
    /// restores — the give-up/MISS branch leaves SPMG=Move (C keeps the
    /// restore commented out at 1062), the rtry==0-far branch (1054)
    /// does too, and completions that never reach maybeRetry (home,
    /// jog, commanded stop with pp, LOAD_POS, limit collapse at
    /// 1405-1408) never restore.
    fn restore_spmg_move_to_pause(&mut self) {
        if self.ctrl.spmg == SpmgMode::Move {
            self.ctrl.spmg = SpmgMode::Pause;
            // Keep the latent-SPMG tracker in step: the restore is not
            // a user transition and must not replay through the
            // Go/Stop machinery.
            self.internal.lspg = SpmgMode::Pause;
        }
    }

    /// C overtaken-target replay (motorRecord.cc:1384-1399): at a motion
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
        true
    }

    /// C enter_do_work LVIO re-evaluation (motorRecord.cc:1463-1483).
    /// Disabled limits (DHLM == DLLM == 0) clear it; an active jog checks
    /// the live readback against the soft limits widened by one second of
    /// jog travel (RBV > HLM - JVEL / RBV < LLM + JVEL) or an inverted
    /// dial pair; a home search disables the check; every other state
    /// preserves the latched value. Returns true on a rising violation
    /// outside SET mode (`!set && !igset`, 1478) — C sets stop = 1 and
    /// clear_buttons() (1480-1482); the caller routes that through
    /// stop_axis, whose in-motion branch clears the same four buttons.
    pub(super) fn recompute_lvio_during_motion(&mut self) -> bool {
        let old_lvio = self.limits.lvio;
        if self.limits.dhlm == self.limits.dllm && self.limits.dllm == 0.0 {
            self.limits.lvio = false;
        } else if self
            .stat
            .mip
            .intersects(MipFlags::JOGF | MipFlags::JOGR | MipFlags::JOG_BL1 | MipFlags::JOG_BL2)
        {
            self.limits.lvio = (self.ctrl.jogf && self.pos.rbv > self.limits.hlm - self.vel.jvel)
                || (self.ctrl.jogr && self.pos.rbv < self.limits.llm + self.vel.jvel)
                || (self.limits.dllm > self.limits.dhlm);
        } else if self.stat.mip.intersects(MipFlags::HOMF | MipFlags::HOMR) {
            self.limits.lvio = false;
        }
        self.limits.lvio && !old_lvio && !self.conv.set && !self.conv.igset
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
        true
    }

    /// Set motion phase with tracing.
    pub(crate) fn set_phase(&mut self, new_phase: MotionPhase) {
        tracing::debug!("phase transition: {:?} -> {:?}", self.stat.phase, new_phase);
        self.stat.phase = new_phase;
    }

    /// C process() completion (motorRecord.cc:1410-1456): with DLY > 0 a
    /// fresh completion edge only arms the delay watchdog
    /// (MIP_DELAY_REQ + callbackRequestDelayed, 1441-1455). The retry
    /// decision (`maybeRetry`, 1431) runs only after the watchdog fires
    /// and the fresh status it requests (GET_INFO, 1416-1426) lands —
    /// never on the raw completion pass. With DLY <= 0 the evaluation
    /// runs on this pass.
    fn settle_then_evaluate(&mut self, effects: &mut ProcessEffects) {
        if self.timing.dly > 0.0 {
            self.set_phase(MotionPhase::DelayWait);
            self.stat.mip.insert(MipFlags::DELAY_REQ);
            effects.schedule_delay = Some(epics_base_rs::runtime::time::duration_from_secs(
                self.timing.dly,
            ));
        } else {
            self.evaluate_position_error(effects);
        }
    }

    /// C "Do another update after LS error" (motorRecord.cc:1367-1380) —
    /// the single owner of the limit-switch completion, predicate and
    /// consequence together. Returns whether a struck limit switch in the
    /// commanded direction terminated this move, having already applied the
    /// drive-field sync that termination implies.
    ///
    /// C tests the limit BEFORE the `pp` test (1382) and the retry gate
    /// (1405) and then `goto process_exit`, so `maybeRetry` never runs on
    /// that pass: an axis that ends on a limit in the commanded direction is
    /// a TERMINAL limit completion whatever `pp` was, forced to `pp = TRUE`
    /// with a GET_INFO and `mip = MIP_DONE` (1376-1377). The next callback's
    /// `postProcess` (827-849) adopts the limit readback into VAL/DVAL/RVAL
    /// and zeroes DIFF/RDIF; the Rust poll already carries that readback
    /// (`process_motor_info` ran this cycle), so the sync applies directly.
    ///
    /// C compares raw `rhls`/`rlls` against raw `cdir`; the user-frame pair
    /// is the same condition, since `process_motor_info` (3733-3734) maps
    /// `hls`/`lls` from `rhls`/`rlls` under the same DIR/MRES polarity flip
    /// applied to `cdir` here.
    ///
    /// Both completion paths that can retry — the inline MIP_STOP (Pause)
    /// branch and [`Self::evaluate_position_error`] — decide through this
    /// one call, so no path can suppress the retry on a limit without also
    /// performing the sync. Holding the two apart is what left the Pause
    /// branch advertising a target the axis never reached after the
    /// positional branch was fixed.
    ///
    /// Scope is the LS-stop ALONE: a plain MOVE_ABS never sets C `pp` (the
    /// dispatch sites at 1983/2025/2110/2125 are SET-mode, HOME and JOG, not
    /// positional moves), so a close-enough or rtry-disabled completion is
    /// still not synced. Do not extend the sync to those branches.
    fn sync_if_limit_stopped(&mut self) -> bool {
        let same_polarity = (self.conv.dir == MotorDir::Pos) == (self.conv.mres >= 0.0);
        let user_cdir = if same_polarity {
            self.stat.cdir
        } else {
            !self.stat.cdir
        };
        let ls_stopped = (self.limits.hls && user_cdir) || (self.limits.lls && !user_cdir);
        if ls_stopped {
            self.postprocess_sync();
        }
        ls_stopped
    }

    /// The C `maybeRetry` port (motorRecord.cc:1042-1104) — the single
    /// post-settle completion evaluation, reached either directly (DLY
    /// <= 0) via [`Self::settle_then_evaluate`] or from the DELAY_ACK
    /// pass after the delay watchdog and its status refresh.
    fn evaluate_position_error(&mut self, effects: &mut ProcessEffects) {
        if self.check_retarget_verification(effects) {
            return;
        }

        let diff = (self.pos.dval - self.pos.drbv).abs();

        let ls_blocks_retry = self.sync_if_limit_stopped();

        // C `maybeRetry` (motorRecord.cc:1049): the retry comparison is
        // `fabs(pmr->diff) >= pmr->rdbd` — inclusive of the boundary.
        // At `diff == rdbd` C retries; an earlier Rust version used `>`
        // and finalized instead. Retry is gated only on `rtry != 0`
        // (`rcnt < rtry` here); C `maybeRetry` has NO `rdbd > 0`
        // condition — RDBD is kept >= |MRES| by `enforceMinRetryDeadband`,
        // and when RDBD is 0 `fabs(diff) >= 0` is trivially true.
        if diff >= self.retry.rdbd && !ls_blocks_retry && self.retry.rtry != 0 {
            if self.retry.rcnt >= self.retry.rtry {
                // C 1059-1073: too many retries — give up and latch MISS
                // (1072). The give-up pass still counts (`++rcnt > rtry`),
                // so RCNT reads rtry+1 until the next dispatch resets it.
                // The retry branches below never touch miss.
                self.retry.rcnt += 1;
                self.retry.miss = true;
                self.finalize_motion(effects);
                // C maybeRetry give-up (motorRecord.cc:1063-1065):
                // `mip = MIP_DONE` then re-arm `MIP_JOG_REQ` from the held
                // jogf/jogr field — unlike the close-enough/rtry==0 branches
                // (1055/1088 `mip &= MIP_JOG_REQ`), which only PRESERVE an
                // already-set bit and during a positional move read 0 because
                // special() arms MIP_JOG_REQ only at mip==MIP_DONE (3042-3053).
                // dmov stays TRUE through give-up, so do_work re-fires the
                // armed jog/home in the SAME process pass (1489 `pmr->dmov`).
                // dispatch_latent_collection is the Rust same-pass do_work
                // re-fire: finalize_motion above set dmov=true and cleared
                // mip, so the gate passes and a held jog/home replays now
                // instead of waiting for the next idle poll.
                self.dispatch_latent_collection(effects, false);
                return;
            }
            if self.retry.rmod == RetryMode::InPosition {
                // C RMOD_I (motorRecord.cc:1432-1438): no move is
                // re-issued — callbackRequestDelayed(dly) re-arms the
                // settle watchdog UNCONDITIONALLY (dly == 0 fires it
                // immediately), counting the cycle against RTRY. DMOV
                // holds FALSE across the whole settle loop (maybeRetry
                // 1077-1080); only the close-enough or give-up branch
                // ends it.
                self.retry.rcnt += 1;
                self.stat.mip = MipFlags::RETRY | MipFlags::DELAY_REQ;
                self.set_phase(MotionPhase::DelayWait);
                effects.schedule_delay = Some(epics_base_rs::runtime::time::duration_from_secs(
                    self.timing.dly,
                ));
                return;
            }

            // C maybeRetry 1077-1082 only arms the retry (mip = MIP_RETRY,
            // dmov = FALSE); the next do_work pass re-enters the FULL move
            // block — entry via !dmov (2241), dispatch gate via MIP_RETRY
            // (2455) — so RMOD scaling with its one-step floors, the
            // retry-deadband too_small suppression, LVIO, and the
            // three-case backlash dispatch all re-run for the retry.
            // Routing through plan_absolute_move replays that block; its
            // dispatch keeps the RETRY bit (C 2461 mip |= MIP_MOVE).
            self.retry.rcnt += 1;
            self.stat.mip = MipFlags::RETRY;
            self.plan_absolute_move(effects);
        } else {
            // Close enough, LS-blocked, or RTRY disabled. C maybeRetry:
            // the close-enough/LS-blocked else-branch clears MISS
            // (1083-1092) and restores SPMG Move->Pause (1096-1101); the
            // rtry==0 branch (1054-1056) leaves both untouched —
            // retry-disabled never latches a miss.
            if diff < self.retry.rdbd || ls_blocks_retry {
                self.retry.miss = false;
                self.restore_spmg_move_to_pause();
            }
            self.finalize_motion(effects);
        }
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
            // C 973: CDIR follows the (frac-scaled) raw-unit stroke.
            self.stat.cdir = rel_distance / self.conv.mres >= 0.0;
            effects.commands.push(MotorCommand::MoveRelative {
                distance: rel_distance,
                min_velocity: self.effective_vbas(),
                velocity: self.backlash_leg_velocity(self.vel.bvel),
                acceleration: self.backlash_accel_egu(),
            });
        } else {
            // C absolute: position = pretarget + frac * (dval - pretarget)
            // = (dval - bdst) + frac * bdst = dval - bdst*(1-frac)
            let pretarget = Self::compute_backlash_pretarget(self.pos.dval, self.retry.bdst);
            let position = pretarget + frac * (self.pos.dval - pretarget);
            // C 967: the FRAC-scaled commanded raw position is posted to
            // RVAL; 973 derives CDIR from the unscaled raw error.
            self.pos.rval = (position / self.conv.mres).round() as i64;
            self.stat.cdir = (self.pos.dval - self.pos.drbv) / self.conv.mres >= 0.0;
            effects.commands.push(MotorCommand::MoveAbsolute {
                position,
                min_velocity: self.effective_vbas(),
                velocity: self.backlash_leg_velocity(self.vel.bvel),
                acceleration: self.backlash_accel_egu(),
            });
        }
        effects.request_poll = true;
    }

    /// Start jog backlash correction (phase 1: move to pretarget at slew velocity).
    /// C has two phases: BL1 moves to (dval - bdst) at slew vel, BL2 moves to dval at backlash vel.
    fn start_jog_backlash(&mut self, effects: &mut ProcessEffects) {
        // dval was synced to drbv by postprocess_sync() above when OMSL
        // is supervisory; under closed loop C 827 skips that sync and
        // the backlash base keeps the DOL target — C verbatim (the
        // MIP_JOG_STOP branch at 908-947 runs after the gated entry).
        // Phase 1 (BL1): move to backlash pretarget (dval - bdst) at slew velocity
        let pretarget = self.pos.dval - self.retry.bdst;
        self.set_phase(MotionPhase::JogBacklash);
        self.stat.mip = MipFlags::JOG_BL1;
        self.internal.backlash_pending = true;
        // CDIR is the sign of the stroke this leg actually commands, in raw
        // units — the same rule every other leg here follows.
        //
        // DEVIATION from C, deliberate — CBUG-B13. C 973 keys CDIR on
        // `relpos = pmr->diff / pmr->mres`, but the JOG_STOP path reaches it
        // through the "sync drive to readback" block at C 827-845, whose MIP
        // predicate forgets to exclude MIP_JOG_STOP and so sets `pmr->diff = 0`
        // first. `(0 < 0.0)` is false, so C publishes cdir = 1 (forward)
        // unconditionally — regardless of which way the take-out leg it
        // dispatches at C 943/945 (toward `dval - bdst`) actually runs. The
        // sibling arms are self-consistent: MIP_MOVE is excluded from that sync
        // so its relpos is live, and the fractional-retry arm re-derives relpos
        // at C 960. Only JOG_STOP keys CDIR on the value it just zeroed.
        self.stat.cdir = (pretarget - self.pos.drbv) / self.conv.mres >= 0.0;
        if self.use_relative_moves() {
            effects.commands.push(MotorCommand::MoveRelative {
                distance: pretarget - self.pos.drbv,
                min_velocity: self.effective_vbas(),
                velocity: self.backlash_leg_velocity(self.vel.velo),
                acceleration: self.move_accel_egu(),
            });
        } else {
            effects.commands.push(MotorCommand::MoveAbsolute {
                position: pretarget,
                min_velocity: self.effective_vbas(),
                velocity: self.backlash_leg_velocity(self.vel.velo),
                acceleration: self.move_accel_egu(),
            });
        }
        effects.request_poll = true;
    }

    /// Start jog backlash phase 2 (final approach at backlash velocity).
    fn start_jog_backlash_final(&mut self, effects: &mut ProcessEffects) {
        let frac = self.retry.frac;
        self.stat.mip = MipFlags::JOG_BL2;
        self.internal.backlash_pending = false;
        let pretarget = Self::compute_backlash_pretarget(self.pos.dval, self.retry.bdst);
        if self.use_relative_moves() {
            // C MIP_JOG_BL1 (motorRecord.cc:1009): the relative final leg
            // is `(relpos - relbpos) * frac` — with relpos = dval - drbv
            // and relbpos = (dval - bdst) - drbv that difference is
            // exactly BDST, independent of where the takeout leg actually
            // stopped. The remaining-error form (dval - drbv) * frac is
            // the MOVE_BL formula (957), not the jog one.
            let rel_distance = self.retry.bdst * frac;
            // C 1020: CDIR follows the scaled raw-unit stroke.
            self.stat.cdir = rel_distance / self.conv.mres >= 0.0;
            effects.commands.push(MotorCommand::MoveRelative {
                distance: rel_distance,
                min_velocity: self.effective_vbas(),
                velocity: self.backlash_leg_velocity(self.vel.bvel),
                acceleration: self.backlash_accel_egu(),
            });
        } else {
            let position = pretarget + frac * (self.pos.dval - pretarget);
            // C 1016: the FRAC-scaled commanded raw position is posted to
            // RVAL; 1020 derives CDIR from the unscaled raw error.
            self.pos.rval = (position / self.conv.mres).round() as i64;
            self.stat.cdir = (self.pos.dval - self.pos.drbv) / self.conv.mres >= 0.0;
            effects.commands.push(MotorCommand::MoveAbsolute {
                position,
                min_velocity: self.effective_vbas(),
                velocity: self.backlash_leg_velocity(self.vel.bvel),
                acceleration: self.backlash_accel_egu(),
            });
        }
        effects.request_poll = true;
    }
}
