use super::*;

impl MotorRecord {
    /// Plan and start a motion from a user write.
    pub fn plan_motion(&mut self, src: CommandSource) -> ProcessEffects {
        let mut effects = ProcessEffects::default();

        // C mmap marks live exactly one process pass (monitor() clears
        // them at the end): take the resolution re-anchor mark now, so
        // a pass that returns from the stop/SPMG top block below drops
        // it exactly like C's early return(OK) before line 1938.
        let res_reanchor = std::mem::take(&mut self.internal.res_reanchor);

        // C enter_do_work (1463-1484) re-evaluates LVIO on EVERY process
        // pass before do_work — including put-processing passes, since
        // HLM/LLM/DHLM/DLLM are pp(TRUE). A rising violation sets
        // stop = 1 (1480), which the latent-stop gate below consumes in
        // this same pass, exactly like C's do_work stop branch.
        if self.recompute_lvio_during_motion() {
            self.ctrl.stop = 1;
            self.mark(m::STOP);
        }

        // C do_work evaluates `pmr->stop` and `spmg != lspg` at the TOP
        // of every put-processing pass, before any other work
        // (motorRecord.cc:1854-1934). A latent stop or SPMG change — a
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
        if self.ctrl.spmg != self.internal.lspg
            && (self.ctrl.stop != 0 || src == CommandSource::Stop)
        {
            self.internal.lspg = self.ctrl.spmg;
        }
        if self.ctrl.stop != 0 && src != CommandSource::Stop {
            self.handle_stop(&mut effects);
            return effects;
        }

        // Latent SPMG change with no stop pulse: run the C top block's
        // SPMG handling now. Stop/Pause halt the axis and return like C;
        // a Go/Move transition falls through to normal dispatch (C falls
        // through to the rest of do_work) unless the transition itself
        // resumed motion — that resume already targets the freshest DVAL,
        // so dispatching src as well would double-plan. For the
        // housekeeping srcs (Sync/Set/Cnen) the gate defers to
        // the next pass instead: their arms map to C special()-time
        // actions that precede do_work, and a Go-resume must not swallow
        // or reorder them. A same-value SPMG put is no transition: C's
        // top block does nothing and the pass falls into the sections.
        if self.ctrl.spmg != self.internal.lspg {
            match src {
                CommandSource::Sync | CommandSource::Set | CommandSource::Cnen => {}
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

        // C do_work resolution block (1936-1991): fires after the
        // stop/SPMG top block and before every other section — ungated
        // by stop_or_pause (a steady Stop/Pause does not suppress it) —
        // and ends the pass; a coalesced position write parks as
        // dval != ldvl like in C. The stop srcs instead run their C
        // top-block branch (which returns before 1938), dropping the
        // taken mark with the pass; a coalesced SPMG transition is
        // parked unsynced, so the latent-SPMG gate replays it.
        if res_reanchor && src != CommandSource::Stop {
            self.dispatch_res_reanchor(&mut effects);
            return effects;
        }

        // SPMG, STOP, and SYNC always processed regardless of command gate
        match src {
            CommandSource::Spmg
            | CommandSource::Stop
            | CommandSource::Sync
            | CommandSource::Cnen => {}
            CommandSource::Set => {
                // C 2237: stop_or_pause returns before the move block —
                // a pending SET redefinition keeps its dval != ldvl
                // signal and load_pos dispatches on the Go/Move pass.
                // No tweak collection or RDBL-error rollback here: C's
                // set branch (2257) precedes the readback-validity
                // check, and an RDBL error must not roll back a
                // redefinition. The collection still runs ahead of the
                // return (2204-2235).
                if !self.can_accept_command() {
                    self.collect_drive_fields();
                    return effects;
                }
            }
            _ => {
                // C do_work collection gate (1994-2008): under closed-loop
                // OMSL with a DB-link DOL the whole button/tweak/relative/
                // raw collection block (2008-2198) is bypassed — VAL
                // arrives only through the DOL link. The fields keep the
                // written value (C writes them raw; the collection just
                // never reads them), so a later OMSL flip back to
                // supervisory can act on a held button. The VAL collection
                // (2204) sits outside the C else: Val/Dval stay live for
                // the DOL cascade.
                if self.closed_loop_dol_collection()
                    && matches!(
                        src,
                        CommandSource::Homf
                            | CommandSource::Homr
                            | CommandSource::Jogf
                            | CommandSource::Jogr
                            | CommandSource::Twf
                            | CommandSource::Twr
                            | CommandSource::Rlv
                            | CommandSource::Rval
                    )
                {
                    // C 1994-2007: the closed-loop else bypasses only the
                    // collection sections — the pass still falls through
                    // to the move block / chain end, so a put that
                    // dispatched nothing fires the implicit GET_INFO
                    // (2546). The collection skips its sections under
                    // closed loop on its own.
                    self.dispatch_latent_collection(&mut effects, true);
                    return effects;
                }
                if !self.can_accept_command() {
                    // C home section under stop_or_pause (2016-2021): a
                    // latched home button only drops DMOV and returns —
                    // no MIP change, no dispatch, button stays latched.
                    // The Go pass falls through the top block into the
                    // home section and dispatches it (handle_spmg_change
                    // mirrors that fall-through). The DMOV drop sits
                    // INSIDE the entry gate — a button that fails it
                    // (blocked by its limit switch) changes nothing.
                    if matches!(src, CommandSource::Homf | CommandSource::Homr)
                        && self.latent_home_request().is_some()
                    {
                        self.set_dmov(false);
                    }
                    // C 2167-2181: the tweak fold is NOT gated by
                    // stop_or_pause — only the move dispatch is (the
                    // return at 2237 comes after collection). Collect it
                    // before refusing this pass so the fold lands in
                    // VAL/DVAL and SPMG=Go fires it like any position
                    // write collected while stopped. The fold sits inside
                    // the closed-loop-bypassed collection else, so it
                    // never runs under closed loop with a DB-link DOL.
                    if !self.closed_loop_dol_collection()
                        && matches!(
                            src,
                            CommandSource::Val
                                | CommandSource::Dval
                                | CommandSource::Rval
                                | CommandSource::Rlv
                                | CommandSource::Twf
                                | CommandSource::Twr
                        )
                    {
                        self.collect_tweak();
                    }
                    self.collect_drive_fields();
                    return effects;
                }
            }
        }

        // C: 0aaf02d7 (2025-02 PR #224) — if a VAL/DVAL/RVAL/RLV write
        // arrives while a home is in progress, release the latched
        // buttons. Without this, the next do_work pass re-issues HOME
        // and loops. C's guard (1388-1393) calls clear_buttons() — ALL
        // FOUR buttons (4386-4408) — so a jog latched behind the home
        // dies with it.
        if matches!(
            src,
            CommandSource::Val | CommandSource::Dval | CommandSource::Rval | CommandSource::Rlv
        ) && self.stat.mip.intersects(MipFlags::HOMF | MipFlags::HOMR)
        {
            self.clear_buttons();
        }

        if src == CommandSource::Stop {
            self.handle_stop(&mut effects);
            return effects;
        }

        // C's home (2010) and jog-start (2079) sections run before the
        // jog-stop section and act on latched button STATE every pass —
        // the move-block srcs evaluate latent buttons so a bare button
        // put overtaken in last_write still fires here; the button srcs
        // run their own section. All live inside the closed-loop-bypassed
        // collection else (2008): a latched button never fires under
        // closed loop with a DB-link DOL.
        if !self.closed_loop_dol_collection() {
            match src {
                CommandSource::Val
                | CommandSource::Dval
                | CommandSource::Rval
                | CommandSource::Rlv
                | CommandSource::Twf
                | CommandSource::Twr => {
                    if self.dispatch_latent_buttons(&mut effects) {
                        return effects;
                    }
                }
                CommandSource::Homf | CommandSource::Homr => {
                    // C home-section entry gate (2013-2014) acts on the
                    // latched button STATE the put left behind, not on
                    // src; a direction already homing or blocked by its
                    // limit switch fails the gate and the button stays
                    // latched (C falls through the section).
                    if let Some(forward) = self.latent_home_request() {
                        self.start_home(forward, &mut effects);
                        return effects;
                    }
                }
                CommandSource::Jogf | CommandSource::Jogr => {
                    let forward = src == CommandSource::Jogf;
                    let held = if forward {
                        self.ctrl.jogf
                    } else {
                        self.ctrl.jogr
                    } != 0;
                    // C jog start (2079-2081) fires only on the request
                    // special() armed (3042-3054, idle record only); a
                    // press during a jog falls to jog_stop_section.
                    let starting = self.stat.mip.contains(MipFlags::JOG_REQ)
                        && !self.stat.mip.intersects(Self::MIP_JOG);
                    if held && starting {
                        // C defect: with both buttons latched, 2106 jogs
                        // forward whichever was written. The written
                        // button wins and the other is released
                        // (epics-modules/motor #170).
                        if forward {
                            self.ctrl.jogr = 0;
                        } else {
                            self.ctrl.jogf = 0;
                        }
                        // A hardware-blocked direction: start_jog refuses
                        // and the pass falls on, the button latched.
                        if self.start_jog(forward, &mut effects) {
                            return effects;
                        }
                    }
                }
                _ => {}
            }
        }

        if self.jog_stop_section(&mut effects) {
            // C returns at 2156/2161 before the collection (2187-2235):
            // the written drive field stays raw.
            return effects;
        }

        // C tweak section (2167-2181): runs after the home/jog sections
        // (which return early when they dispatch) and folds latched
        // buttons into VAL on every pass — a bare TWF/TWR put overtaken
        // in last_write rides this pass's val processing as one combined
        // move toward `written value + TWV`. Inside the closed-loop-
        // bypassed collection else: no fold under closed loop with a
        // DB-link DOL.
        let tweak_pending = !self.closed_loop_dol_collection()
            && matches!(
                src,
                CommandSource::Val
                    | CommandSource::Dval
                    | CommandSource::Rval
                    | CommandSource::Rlv
                    | CommandSource::Twf
                    | CommandSource::Twr
            )
            && self.collect_tweak();
        if self.collect_drive_fields() {
            return effects;
        }

        // C: 7493d50b — when URIP=Yes and the external RDBL link
        // is in error, "do not start a new target position move
        // (sans Home search or Jog)". The refusal is the
        // move-block gate (`lvio || rtnstat == FALSE`,
        // 2418-2453): it sits AFTER the home (2010) and jog
        // (2078) sections — those dispatch normally with the
        // readback link down — and the SET branches (2207,
        // 2257-2262) return before reaching it, so a SET-mode
        // write is never rolled back. Stopping an in-flight
        // motion is owned by the poll-time read failure
        // (3687-3697, db5da2f0), ported in check_completion;
        // a refused write while moving only rolls back to the
        // in-flight target like C.
        if self.conv.urip
            && self.conv.rdbl_error
            && (!self.conv.set || self.conv.igset != 0)
            && matches!(
                src,
                CommandSource::Val
                    | CommandSource::Dval
                    | CommandSource::Rval
                    | CommandSource::Rlv
                    | CommandSource::Twf
                    | CommandSource::Twr
            )
        {
            self.refuse_move_restore_lasts();
            return effects;
        }

        match src {
            CommandSource::Val | CommandSource::Dval | CommandSource::Rval | CommandSource::Rlv => {
                // C move-block entry (2241): `dval != ldvl || !dmov`.
                // A database put never fails this gate — the special()
                // pass-0 blink (2591-2620) dropped DMOV before the pass
                // — so a same-value dbPut re-put re-enters and the
                // dispatch gate (2455, `mip == MIP_DONE || MIP_RETRY`)
                // re-sends the move: C retries a missed target on an
                // operator re-put. What the entry gate refuses is the
                // UNBLINKED same-value pass — the closed-loop DOL
                // collection (a bare dbGetLink, 1994, no special) and
                // housekeeping passes — which falls to the chain end
                // instead (latched SYNC, implicit GET_INFO, 2540-2557).
                if self.stat.dmov == 0 || self.pos.dval != self.internal.ldvl {
                    self.plan_absolute_move(&mut effects);
                } else {
                    self.dispatch_latent_collection(&mut effects, true);
                }
            }
            // Returned from the top block before the sections.
            CommandSource::Stop => {}
            CommandSource::Jogf
            | CommandSource::Jogr
            | CommandSource::Homf
            | CommandSource::Homr
            | CommandSource::Spmg
            | CommandSource::Cnen => {
                // The sections did not consume the pass (failed home or
                // jog-start gate, a release, a same-value SPMG, CNEN's
                // pp pass): C falls through to the chain end (implicit
                // GET_INFO, 2546). A pass during motion is consumed by
                // the move block instead (!dmov) — the collection's dmov
                // gate reproduces that.
                self.dispatch_latent_collection(&mut effects, true);
            }
            CommandSource::Twf | CommandSource::Twr => {
                // The fold already ran in the collection above (SET-mode
                // redefinition included); dispatch the pending move (C
                // move block firing on the folded val change). A dbPut
                // to TWF/TWR is blinked (special pass-0, 2591-2620), so
                // even a zero fold (TWV = 0) enters via `!dmov` and
                // too_small pulses DMOV like C; only an unblinked zero
                // fold refuses at the entry gate (2241) — chain end.
                if tweak_pending {
                    if self.stat.dmov == 0 || self.pos.dval != self.internal.ldvl {
                        self.plan_absolute_move(&mut effects);
                    } else {
                        self.dispatch_latent_collection(&mut effects, true);
                    }
                }
            }
            CommandSource::Sync => {
                // C 2540-2544: the latched SYNC applies only when idle
                // (`mip == MIP_DONE`); written during a motion it stays
                // latched and finalize_motion consumes it at completion.
                self.apply_latent_sync();
            }
            CommandSource::Set => {
                // C: the SET-mode redefinition dispatches through the
                // move block (2241-2263) — plan_absolute_move recomputes
                // DIFF/RDIF and its set test routes to load_pos. The
                // entry gate (2241) applies first, but a dbPut to a
                // drive field is blinked (special pass-0, 2591-2620) and
                // always re-enters — C re-sends LOAD_POS on a same-value
                // redefinition re-put. Only an unblinked same-value pass
                // (no fresh drive write) falls to the chain end.
                if self.stat.dmov == 0 || self.pos.dval != self.internal.ldvl {
                    self.plan_absolute_move(&mut effects);
                } else {
                    self.dispatch_latent_collection(&mut effects, true);
                }
            }
        }

        effects
    }

    /// Plan an absolute move to current DVAL: C do_work's move block,
    /// dispatching only at `mip == MIP_DONE || mip == MIP_RETRY`.
    pub(crate) fn plan_absolute_move(&mut self, effects: &mut ProcessEffects) {
        // C move-block entry (2248-2255): DIFF and RDIF are recomputed
        // from the requested target before anything else — they post
        // even when the pass is then suppressed (too_small) or refused
        // (LVIO). The collection updates `pos.dval` but never
        // refreshes them; only the poll does.
        if self.pos.dval != self.internal.ldvl {
            self.mark(m::DVAL);
        }
        let diff = self.pos.dval - self.pos.drbv;
        if self.pos.diff != diff {
            self.pos.diff = diff;
            self.mark(m::DIFF);
        }
        self.pos.rdif = if self.conv.mres != 0.0 {
            coordinate::to_long((self.pos.diff / self.conv.mres).round())
        } else {
            0
        };
        self.mark(m::RDIF);
        // C set test (2257-2263): in SET mode every move-block dispatch
        // is a position redefinition routed to load_pos, never a motion
        // — including deferred passes (Go/Move resume, retry, queued
        // replay). Sits right after the DIFF/RDIF recompute and before
        // too_small/LVIO, as in C.
        if self.conv.set && self.conv.igset == 0 {
            if !self.stat.mip.contains(MipFlags::LOAD_P) {
                self.load_pos(effects);
            }
            return;
        }
        // C 2299-2305: the collection does not know which field moved
        // DVAL, so VAL and RVAL are re-derived from it.
        self.pos.val = coordinate::dial_to_user(self.pos.dval, self.conv.dir, self.pos.off);
        self.mark_val_collected();
        if self.conv.mres != 0.0 {
            self.pos.rval = coordinate::to_long((self.pos.dval / self.conv.mres).round());
        }
        if self.pos.rval != self.internal.lrvl {
            self.mark(m::RVAL);
        }
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
                // C 2334-2343: a pending non-move (paused-RETRY resume
                // whose error drifted under RDBD) completes here with
                // mip = DONE; the quiesce below plus the DMOV pulse
                // recovery in do_process_inner restore DMOV=1.
                let pending_completion = self.stat.dmov == 0;
                if pending_completion
                    && (self.stat.mip.is_empty() || self.stat.mip == MipFlags::RETRY)
                {
                    self.stat.mip = MipFlags::empty();
                    self.mark(m::MIP);
                }
                // C 2345-2347: update the previous-target registers so the
                // suppressed divergence is not re-detected on every later
                // pass (move-block gate, overtaken-target replay).
                self.internal.ldvl = self.pos.dval;
                self.internal.lval = self.pos.val;
                self.internal.lrvl = self.pos.rval;
                // Rust-side deviation from C (deliberate, ophyd/bluesky):
                // a sub-step request pulses DMOV 1→0→1 so clients watching
                // DMOV see the "move" complete; a pending completion runs
                // the same flow to restore DMOV=1. dmov=false posts
                // DMOV=0 on this pass; the immediate re-process
                // (no pending event) finalizes with DMOV=1. An SPDB
                // suppress with DMOV already 1 stays quiet, like C's
                // dmov==FALSE gate (2333).
                if steps < 1 || pending_completion {
                    self.set_dmov(false);
                    self.set_movn(true);
                    effects.request_poll = true;
                }
                return;
            }
        }

        // C 2352-2357: the retry counter resets only when this dispatch is
        // NOT a retry — and BEFORE the LVIO check, so a refused fresh move
        // still zeroes it. A Go resume of a paused move re-enters here
        // with mip = MIP_RETRY (maybeRetry armed it at the pause stop),
        // and C preserves rcnt so repeated Pause/Go cycles count against
        // RTRY.
        if !self.stat.mip.contains(MipFlags::RETRY) {
            if self.retry.rcnt != 0 {
                self.mark(m::RCNT);
            }
            self.retry.rcnt = 0;
        }

        // C 2391-2395: preferred_dir, computed once for the LVIO check
        // and the dispatch case selection.
        let preferred = self.is_preferred_direction(self.pos.dval, self.pos.drbv);
        let old_lvio = self.limits.lvio;

        // C LVIO evaluation (2396-2415), AFTER the too_small suppression:
        // a request whose error is already inside the deadband completes
        // quietly even when the target sits past a limit. The
        // moving-toward-valid exception (2403-2405) is its own arm,
        // granted on DVAL whether or not the move is preferred — only the
        // final else discriminates: preferred checks DVAL, non-preferred
        // checks the backlash pretarget.
        self.limits.lvio = i16::from(
            if self.limits.dhlm == self.limits.dllm && self.limits.dllm == 0.0 {
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
            },
        );
        if self.limits.lvio != old_lvio {
            self.mark(m::LVIO);
        }
        if self.limits.lvio != 0 {
            self.refuse_move_restore_lasts();
            return;
        }

        // C 2457: under any other motion (a home, a jog, an external
        // move) the target stays parked in VAL/DVAL with the lasts
        // untouched, and that motion's completion dispatches it.
        if !(self.stat.mip.is_empty() || self.stat.mip == MipFlags::RETRY) {
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

        // DMOV pulse: set false before starting
        self.set_dmov(false);
        // MISS is not cleared at dispatch: C touches it only inside
        // maybeRetry (1072 set, 1092 clear), so a latched miss stays
        // visible through the next move until its evaluation lands
        // close enough.

        // C 2526: cdir = (rdif < 0) ? 0 : 1 — the INTEGER raw error
        // posted at the move-block entry, so a sub-half-step negative
        // error (rdif rounds to 0) and a zero error under MRES < 0 both
        // read forward, unlike the f64 sign of `diff`.
        self.stat.cdir = i16::from(self.pos.rdif >= 0);

        // Set MIP and phase. C 2461 is `mip |= MIP_MOVE` behind the
        // `mip == MIP_DONE || mip == MIP_RETRY` gate (2455): the RETRY
        // bit survives into the in-flight move, keeping the dispatch
        // marked as a retry (the AJF readback-link retry-count fix).
        let is_retry = self.stat.mip.contains(MipFlags::RETRY);
        self.stat.mip = MipFlags::MOVE;
        self.mark(m::MIP);
        if is_retry {
            self.stat.mip.insert(MipFlags::RETRY);
            self.mark(m::MIP);
        }
        self.set_phase(MotionPhase::MainMove);
        self.internal.backlash_pending = backlash;
        // C 2523: a move that will need a backlash correction arms pp
        // ("do backlash from postprocess()"). Collaterally, a Pause that
        // interrupts such a move syncs at its stop completion instead of
        // arming the paused-resume state — matching C.
        if backlash {
            self.internal.pp = 1;
        }

        let frac = f64::from(self.retry.frac);
        // C 2268: currpos ("where we are") is LDVL — the previously
        // dispatched target, NOT the readback. The absolute FRAC dispatch
        // (2488/2513) walks from the previous target toward the new one:
        // position = currpos + frac * (newpos - currpos). Only the
        // relative legs (relpos/relbpos, 2280-2281) measure from DRBV.
        // Captured before the load_pos update below overwrites ldvl.
        let currpos = self.internal.ldvl;

        // C parity (motorRecord.cc:2469-2471, in do_work's move dispatch —
        // load_pos latches the same three at 3776-3780, but on the LOAD_POS
        // path, not this one): ldvl/lval/lrvl reflect
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
    }

    /// Handle STOP command.
    fn handle_stop(&mut self, effects: &mut ProcessEffects) {
        self.ctrl.stop = 0; // pulse field
        self.mark(m::STOP);
        self.stop_axis(effects);
    }

    /// The C do_work top-block stop path, shared by the STOP field pulse,
    /// SPMG=Stop (motorRecord.cc:1871: `spmg == motorSPMG_Stop ||
    /// stop == true`) and the rising-LVIO stop (1475-1484). Single owner
    /// so the entries cannot drift.
    pub(super) fn stop_axis(&mut self, effects: &mut ProcessEffects) {
        let mip = self.stat.mip;
        // C 1874-1888 leaves an unfinished pp-FALSE stop (a Pause, or an
        // NTM stop's MIP_STOP|MIP_MOVE) as it is, so its completion's
        // maybeRetry, or Go, drives back toward DVAL after the user's
        // STOP. The same branch keeps a home queued behind a stop (mip =
        // HOMF|STOP, 2028-2035), which postProcess (854-889) then starts
        // after the user's STOP. That C defect is not copied: the STOP
        // takes over as a commanded stop (pp, readback sync at
        // completion) and drops the queued home. A jog started while
        // moving (JOGF|STOP, 2106-2111) is never re-fired, so C's branch
        // stands for it.
        let uncommanded_stop =
            mip.contains(MipFlags::STOP) && self.internal.pp == 0 && self.stat.dmov == 0;
        let takes_over = uncommanded_stop || self.internal.queued_motion.is_some();
        if !takes_over && (mip.is_empty() || mip.intersects(MipFlags::STOP | MipFlags::RETRY)) {
            // C 1874-1888: MIP DONE, STOP or RETRY sends STOP_AXIS and
            // returns with MIP untouched, except that a stop during retry
            // abandons the retry and reports done (mip = MIP_DONE, dmov =
            // TRUE) with no readback sync.
            if mip.contains(MipFlags::RETRY) {
                // C drops only MIP here: the next callback marks the still
                // moving axis EXTERNAL with pp (1314-1320) and a VAL parked
                // during the retry replays (1384-1397), overriding the
                // STOP. That C defect is not copied: finalize_motion adopts
                // the parked target into the lasts, so nothing replays.
                self.finalize_motion(effects);
            }
        } else {
            if self.stat.movn != 0 {
                // C 1890-1893: the readback sync and DMOV wait for the axis
                // to stop (postProcess 827-849, check_completion's
                // MIP_STOP branch via postprocess_sync()).
                self.internal.pp = 1;
                self.clear_buttons();
            } else {
                // C 1895-1896: nothing is moving, so the targets take the
                // readback now.
                self.sync_target_position();
            }
            // C 1898-1900.
            if mip.intersects(MipFlags::HOMF | MipFlags::HOMR) {
                self.clear_buttons();
            }
            // C 1902-1907 replaces MIP wholesale, erasing the backlash
            // legs and any queued home/jog with the rest: "When we wait
            // for DLY, keep it. Otherwise the record may lock up."
            self.internal.backlash_pending = false;
            self.internal.queued_motion = None;
            if !mip.contains(MipFlags::DELAY_REQ) {
                self.stat.mip = MipFlags::STOP;
                self.mark(m::MIP);
            }
        }
        // C motorRecord.cc — STOP_AXIS is sent unconditionally ("just in
        // case"): the driver may still be settling even when the record
        // considers itself idle (e.g. after an InPosition retry, which
        // finalizes the record but leaves the servo moving).
        effects.commands.push(MotorCommand::stop_axis());
    }

    /// Start jogging.
    /// Returns true when the pass was consumed in-section like C's jog
    /// section returns (dispatch, queue-behind-stop, or the soft-limit
    /// refusal at 2092-2104); false only for the hardware-blocked
    /// direction, which C never arms (special 3042-3053) — that put
    /// pass falls through to the do_work chain end.
    fn start_jog(&mut self, forward: bool, effects: &mut ProcessEffects) -> bool {
        // C 2106-2115: the dispatch consumes MIP_JOG_REQ wholesale (mip =
        // MIP_JOGF/JOGR) and, while the axis still moves, ORs in MIP_STOP
        // and only stops it. Nothing re-fires the jog at the stop
        // completion (postProcess re-fires only a home).
        if self.stat.phase != MotionPhase::Idle && self.stat.movn != 0 {
            self.stat.mip = if forward {
                MipFlags::JOGF
            } else {
                MipFlags::JOGR
            } | MipFlags::STOP;
            self.mark(m::MIP);
            // C 2110: pp = TRUE — the stop completion post-processes
            // (VAL/DVAL <- readback).
            self.internal.pp = 1;
            self.internal.backlash_pending = false;
            effects.commands.push(MotorCommand::stop_axis());
            return true;
        }

        let dir = if forward {
            MotionDirection::Positive
        } else {
            MotionDirection::Negative
        };
        if self.is_blocked_by_hw_limit(dir) {
            return false;
        }

        // C: 9e5b5432 PR #99 — refuse a jog command that would push past a
        // soft limit further. Without this the record stays in MIP=JOG_REQ
        // and the button must be released manually. The opposite direction
        // (back inside the soft window) is still allowed.
        if self.jog_violates_soft_limit(forward) {
            // C 2095-2104: BOTH buttons release on refusal ("prevent
            // record from locking up in mip = JOG_REQ"), not just the
            // pressed one.
            if self.ctrl.jogf != 0 {
                self.ctrl.jogf = 0;
                self.mark(m::JOGF);
            }
            if self.ctrl.jogr != 0 {
                self.ctrl.jogr = 0;
                self.mark(m::JOGR);
            }
            // C 2092-2104 clears only the buttons and leaves a parked
            // MIP_JOG_REQ in mip (its arming lives in special(), blind to
            // internal clears). Here the request is dropped with its
            // button: JOG_REQ set means a latched, undispatched request.
            // C never marked the request, so its drop posts nothing.
            self.stat.mip.remove(MipFlags::JOG_REQ);
            self.limits.lvio = 1;
            self.mark(m::LVIO);
            // C 2104: the refusal returns in-section — pass consumed.
            return true;
        }

        self.set_dmov(false);
        // C 2125: pp = TRUE at jog dispatch — a pause that interrupts the
        // jog post-processes (syncs) at its stop completion.
        self.internal.pp = 1;

        if forward {
            self.stat.mip = MipFlags::JOGF;
            self.mark(m::MIP);
        } else {
            self.stat.mip = MipFlags::JOGR;
            self.mark(m::MIP);
        }
        self.set_phase(MotionPhase::Jog);
        self.emit_jog(forward, effects);
        // C 2146: the jog dispatch returns in-section — pass consumed.
        true
    }

    /// Dispatch the jog velocity command — the single owner of the
    /// `MoveVelocity` emission and its direction bookkeeping. The jog
    /// button is USER-frame (JOGF = jog toward growing user
    /// coordinate); the device command is DIAL-frame, so the button
    /// direction must fold through DIR: C `motorRecord.cc:2119`
    /// commands the raw velocity `jogv = (jvel * dir) / mres`, whose
    /// dial equivalent is `±jvel * dir`. CDIR stays RAW-frame
    /// (C 2126-2137: jogf, inverted once for MRES<0 and once for
    /// DIR=Neg).
    pub(super) fn emit_jog(&mut self, forward: bool, effects: &mut ProcessEffects) {
        // Remember jog direction for backlash (MIP flags get cleared by stop_jog)
        self.internal.jog_was_forward = forward;
        let dial_forward = forward != (self.conv.dir == MotorDir::Neg);
        self.stat.cdir = i16::from(dial_forward != (self.conv.mres < 0.0));
        let jog = MotorCommand::MoveVelocity {
            min_velocity: self.raw_rate(self.effective_vbas()),
            velocity: self.raw_position(if dial_forward {
                self.vel.jvel
            } else {
                -self.vel.jvel
            }),
            acceleration: self.raw_rate(self.jog_accel_egu()),
        };
        effects.commands.extend(jog.after_speeds(SpeedWrites::JOG));
    }

    /// Stop jogging.
    fn stop_jog(&mut self, effects: &mut ProcessEffects) {
        // C 2152: pp = TRUE — "When stopped, process() will correct
        // backlash" (and sync the drive fields via postProcess).
        self.internal.pp = 1;
        // A jog started while the axis moved (mip = JOGF|STOP) takes the
        // same branch: STOP_AXIS is sent again on top of the in-flight
        // stop, whose completion takes the JOG_STOP backlash (postProcess
        // 908-958).
        let queued = self.stat.mip.contains(MipFlags::STOP);
        // C 2153-2154: mip |= MIP_JOG_STOP; mip &= ~(MIP_JOGF | MIP_JOGR).
        // With the JOG bits gone, the deceleration is no longer "a jog"
        // to the enter_do_work LVIO recompute (1466 gates on MIP_JOG,
        // which excludes JOG_STOP) — the latch is preserved while
        // stopping. The jog direction survives in jog_was_forward. C marks
        // neither write; MIP posts at the stop completion.
        self.stat.mip.insert(MipFlags::JOG_STOP);
        self.stat.mip.remove(MipFlags::JOGF | MipFlags::JOGR);
        if !queued {
            self.set_phase(MotionPhase::JogStopping);
        }
        effects.commands.push(MotorCommand::stop_axis());
    }

    /// Whether the limit switch in the (user-direction-adjusted) home
    /// direction blocks a home command. C's home-section entry gate and
    /// start check (motorRecord.cc:2013-2014): HOMF blocked by HLS when
    /// DIR=Pos, by LLS when DIR=Neg; HOMR mirrored.
    fn home_blocked_by_limit(&self, forward: bool) -> bool {
        if forward == (self.conv.dir == MotorDir::Pos) {
            self.limits.hls != 0
        } else {
            self.limits.lls != 0
        }
    }

    /// C do_work evaluates the latched HOMF/HOMR/JOGF/JOGR buttons by
    /// STATE on every put-processing pass — home section
    /// (motorRecord.cc:2010), jog start (2079) — before
    /// the tweak/val move blocks, not by which field write triggered the
    /// pass. The Rust dispatcher acts on `last_write`, so a bare button
    /// put (dbPut, no process) overtaken in `last_write` by a later
    /// write would otherwise stay latched and never fire. Called for the
    /// move-block srcs before their own handling; returns true when a
    /// button action consumed the pass (the C sections return OK, so the
    /// position write that triggered the pass is overtaken).
    /// C home-section entry gate (motorRecord.cc:2013-2014): a latched
    /// button for a direction not already homing and not blocked by its
    /// limit switch. Returns the home direction, forward winning when
    /// both buttons are latched.
    fn latent_home_request(&self) -> Option<bool> {
        let homf_ready = self.ctrl.homf != 0
            && !self.stat.mip.contains(MipFlags::HOMF)
            && !self.home_blocked_by_limit(true);
        let homr_ready = self.ctrl.homr != 0
            && !self.stat.mip.contains(MipFlags::HOMR)
            && !self.home_blocked_by_limit(false);
        if homf_ready || homr_ready {
            Some(homf_ready)
        } else {
            None
        }
    }

    fn dispatch_latent_buttons(&mut self, effects: &mut ProcessEffects) -> bool {
        // Home (C 2010-2013): pure button-state gate per direction,
        // skipping a direction already homing or blocked by its limit.
        if let Some(forward) = self.latent_home_request() {
            self.start_home(forward, effects);
            return true;
        }

        // Jog start (C 2079-2081): an armed MIP_JOG_REQ with no jog in
        // flight. On a hardware-blocked direction the button stays
        // latched and the pass falls through, like C's home/jog gates
        // failing into the move block. Soft-limit refusal happens inside
        // start_jog like the C section body.
        if !self.stat.mip.intersects(Self::MIP_JOG)
            && self.stat.mip.contains(MipFlags::JOG_REQ)
            && (self.ctrl.jogf != 0 || self.ctrl.jogr != 0)
        {
            let forward = self.ctrl.jogf != 0;
            let dir = if forward {
                MotionDirection::Positive
            } else {
                MotionDirection::Negative
            };
            if !self.is_blocked_by_hw_limit(dir) {
                // C 2106 JOGF precedence; the other button is released as
                // in the Jogf/Jogr dispatch arm (#170).
                if forward {
                    self.ctrl.jogr = 0;
                } else {
                    self.ctrl.jogf = 0;
                }
                self.start_jog(forward, effects);
                return true;
            }
            return false;
        }
        false
    }

    /// C `MIP_JOG` (motorRecord.cc:293): a jog or its backlash in flight.
    const MIP_JOG: MipFlags = MipFlags::JOGF
        .union(MipFlags::JOGR)
        .union(MipFlags::JOG_BL1)
        .union(MipFlags::JOG_BL2);

    /// C do_work jog-stop section (2148-2161), the one owner for every
    /// put-owned pass past the home and jog-start sections: a jog in MIP
    /// (active, or queued behind a stop) with no MIP_JOG_REQ stops, and a
    /// pass during the jog stop or its backlash returns. Returns true
    /// when the pass is consumed. Inside the closed-loop-bypassed
    /// collection else (2008).
    fn jog_stop_section(&mut self, effects: &mut ProcessEffects) -> bool {
        if self.closed_loop_dol_collection() {
            return false;
        }
        let mip = self.stat.mip;
        if !mip.contains(MipFlags::JOG_REQ) && mip.intersects(MipFlags::JOGF | MipFlags::JOGR) {
            self.stop_jog(effects);
            return true;
        }
        mip.intersects(MipFlags::JOG_STOP | MipFlags::JOG_BL1 | MipFlags::JOG_BL2)
    }

    /// C do_work gate (motorRecord.cc:1487-1492): do_work runs on every
    /// put pass (`process_reason != CALLBACK_DATA`) and on done-record
    /// callbacks (the `dmov` arm), and its home (2013), jog (2081), and
    /// tweak (2167-2181) sections act on latched button STATE — not on
    /// which write triggered the pass. plan_motion covers the move-block
    /// srcs and the SPMG=Go resume; this runs the same collection on the
    /// level-triggered passes with no command source (the None event,
    /// the idle poll, the STUP ack), so a button latched while its gate
    /// failed — limit switch active, or the closed-loop DOL collection
    /// bypass (1994-2008) — fires once the gate clears.
    ///
    /// Gates, all C's: SPMG Go/Move (stop_or_pause returns at 2237
    /// before the sections), the closed-loop bypass else (2008), and
    /// dmov (1490). The dmov gate means a put pass during motion defers
    /// the latched state to the completion/idle poll instead of queueing
    /// through the movn branches — a deferral bounded by one poll
    /// period, and it keeps the in-flight retarget invariants owned by
    /// the explicit Val-arm path.
    ///
    /// After the sections, the C else-if chain end runs: the move block
    /// (2241) whose set test replays a parked SET redefinition, the
    /// latent SYNC arm (2540-2544), and the implicit GET_INFO arm
    /// (2546-2557). `put_pass` is C's `proc_ind == NOTHING_DONE`: only
    /// a put/scan pass may fire the implicit GET_INFO — a CALLBACK_DATA
    /// pass firing it would request a fresh status from every status,
    /// an unbounded poll feedback loop C prevents with the same test.
    pub(super) fn dispatch_latent_collection(
        &mut self,
        effects: &mut ProcessEffects,
        put_pass: bool,
    ) {
        if self.stat.dmov == 0 || !self.can_accept_command() {
            return;
        }
        // C closed-loop bypass (1994-2008): its else covers only the
        // home/jog/tweak sections (2013-2197) — the chain end below
        // still runs on a closed-loop pass.
        if !self.closed_loop_dol_collection() {
            if self.dispatch_latent_buttons(effects) {
                return;
            }
            // C tweak section (2167-2181): the fold lands in VAL and the
            // collection and move block below take it, like the Twf/Twr
            // arm. A #231-refused fold consumes the buttons and the pass.
            let had_tweak = self.ctrl.twf != 0 || self.ctrl.twr != 0;
            if !self.collect_tweak() && had_tweak {
                return;
            }
        }
        if self.collect_drive_fields() {
            return;
        }
        // C move block entry (2241): `dval != ldvl || !dmov` — dmov is
        // true past the gate above, so only a diverged DVAL enters: a
        // drive field the collection just propagated, or a SET
        // redefinition parked while a LOAD_POS was in flight (the set
        // test, 2257-2263, routes it to load_pos, which skips while
        // MIP_LOAD_P is set).
        if self.pos.dval != self.internal.ldvl {
            self.plan_absolute_move(effects);
            return;
        }
        // C SYNC arm (2540-2544): chain end, idle and done.
        if self.apply_latent_sync() {
            return;
        }
        // C implicit GET_INFO (2546-2557): a put/scan pass that
        // dispatched nothing refreshes the device status — STUP goes
        // BUSY and GET_INFO is sent; a device that cannot deliver it
        // returns STUP to OFF (here: never enters BUSY, like the
        // explicit driverless STUP put). determine_event's stup==2
        // clear + ack on the next fresh status closes the cycle.
        if put_pass && self.stat.stup == 0 && self.can_get_info() {
            self.stat.stup = 2;
            self.mark(m::STUP);
            effects.commands.push(MotorCommand::Poll);
        }
    }

    /// Start homing.
    fn start_home(&mut self, forward: bool, effects: &mut ProcessEffects) {
        // C: if motor is moving, stop first then queue home for after stop.
        // C 2023-2033: mip = MIP_HOMF/HOMR wholesale, then the movn branch
        // ORs in MIP_STOP — a queued home reads back as HOMF|STOP. The
        // replay discriminator stays internal.queued_motion (see
        // start_jog).
        if self.stat.phase != MotionPhase::Idle && self.stat.movn != 0 {
            self.stat.mip = if forward {
                MipFlags::HOMF
            } else {
                MipFlags::HOMR
            } | MipFlags::STOP;
            self.mark(m::MIP);
            // C 2025: pp = TRUE — the stop completion post-processes
            // (VAL/DVAL <- readback) before the queued home re-fires.
            self.internal.pp = 1;
            self.internal.queued_motion = Some(QueuedMotion::Home { forward });
            self.internal.backlash_pending = false;
            effects.commands.push(MotorCommand::stop_axis());
            return;
        }

        self.set_dmov(false);
        // C 2069: the home dispatch resets the retry counter.
        self.retry.rcnt = 0;
        self.mark(m::RCNT);
        // C 2025: pp = TRUE at home dispatch (set for both the queued and
        // the direct branch before the movn test).
        self.internal.pp = 1;

        // C does NOT clear the button at dispatch: HOMF/HOMR read back 1
        // for the whole home and clear at completion (postProcess
        // 893-906) or on a stop/pause (clear_buttons, 1893/1899-1900).
        // Every caller enters through latent_home_request (the C
        // 2013-2014 gate: button latched, direction not already homing,
        // limit switch clear), so a latched button cannot re-dispatch.
        if forward {
            self.stat.mip = MipFlags::HOMF;
            self.mark(m::MIP);
        } else {
            self.stat.mip = MipFlags::HOMR;
            self.mark(m::MIP);
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
        self.stat.cdir = i16::from(hw_forward);

        let home = MotorCommand::Home {
            forward: hw_forward,
            min_velocity: self.raw_rate(self.effective_vbas()),
            velocity: self.raw_rate(self.vel.hvel),
            acceleration: self.raw_rate(self.home_accel_egu()),
        };
        effects
            .commands
            .extend(home.after_speeds(SpeedWrites::MOVE));
    }

    /// Handle tweak (TWF/TWR).
    /// C tweak collection (motorRecord.cc:2167-2181): fold the latched
    /// tweak button(s) into VAL — `val += twv * (twf ? 1 : -1)`, TWF wins
    /// when both are latched — and release BOTH buttons. The fold acts on
    /// latched button STATE, not on the src that triggered the pass, so a
    /// bare TWF/TWR put overtaken in `last_write` still lands. The VAL
    /// change it makes is collect_drive_fields' (2204-2235). Returns true
    /// when the fold landed in VAL.
    fn collect_tweak(&mut self) -> bool {
        if self.ctrl.twf == 0 && self.ctrl.twr == 0 {
            return false;
        }
        let forward = self.ctrl.twf != 0;
        self.ctrl.twf = 0;
        self.ctrl.twr = 0;

        // No hard-limit gate here: C's fold (motorRecord.cc:2167-2181) is
        // unconditional — `val += twv * dir` runs on the latched button with no
        // hls/lls test, exactly like a direct VAL write. Limits are the move
        // block's business further down (and the driver's, which holds the axis
        // at the switch). Gating the fold made a tweak strictly more restrictive
        // than the VAL write it is shorthand for, and silently ate the button:
        // with the user-direction switch active but the soft target legal, C
        // folds VAL and dispatches.
        let delta = if forward {
            self.ctrl.twv
        } else {
            -self.ctrl.twv
        };
        // #231: LOAD_POS blocked — refuse the Frozen-leg redefinition the
        // fold would make (C load_pos sends LOAD_POS unconditionally,
        // 3811), like the VAL put refusal.
        if self.conv.set
            && self.conv.igset == 0
            && self.conv.foff == FreezeOffset::Frozen
            && self.conv.loadpos_blocked
        {
            return false;
        }
        self.pos.val += delta;
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
                // C (1899-1911): the Pause pass runs unconditionally —
                // even on an idle axis it sets mip = MIP_STOP and sends
                // STOP_AXIS ("just in case" the driver is still settling);
                // the next completion pass collapses the bare MIP_STOP via
                // maybeRetry's close-enough path, and Go round-trips it to
                // DONE (1921-1925). Pause does NOT set pp — the drive
                // fields keep the target so Go can resume. `mip =
                // MIP_STOP` erases a queued jog (MIP_JOG_REQ) and a
                // queued/active home (MIP_HOMF|HOMR).
                // C 1899-1900: `if (mip & MIP_HOME) clear_buttons()` —
                // ALL FOUR buttons (4386-4408), so a jog latched behind
                // the home is canceled with it; a paused plain jog (no
                // HOME bit in MIP) keeps its button and resumes on Go.
                // The MIP test covers both an active home (HOMF/HOMR)
                // and one queued behind a stop (HOMF|STOP).
                if self.stat.mip.intersects(MipFlags::HOMF | MipFlags::HOMR) {
                    self.clear_buttons();
                }
                self.internal.queued_motion = None;
                // C 1902-1907: `mip = MIP_STOP` — wholesale, erasing a
                // parked MIP_JOG_REQ and the JOGF/HOMF bits of a queued
                // request — except while waiting on DLY ("keep it,
                // otherwise the record may lock up").
                if !self.stat.mip.contains(MipFlags::DELAY_REQ) {
                    self.stat.mip = MipFlags::STOP;
                    self.mark(m::MIP);
                }
                effects.commands.push(MotorCommand::stop_axis());
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
                //
                // The Go pass FALLS THROUGH the top block into do_work's
                // home section (2013-2076), which precedes the jog
                // dispatch and overwrites MIP wholesale (2023) — a home
                // button latched while paused (its 2016-2021 return kept
                // it) fires here and wins over a latched jog.
                if let Some(forward) = self.latent_home_request() {
                    self.start_home(forward, effects);
                } else if (self.ctrl.jogf != 0 && self.limits.hls == 0)
                    || (self.ctrl.jogr != 0 && self.limits.lls == 0)
                {
                    let forward = self.ctrl.jogf != 0;
                    self.start_jog(forward, effects);
                } else {
                    if self.stat.mip == MipFlags::STOP {
                        self.stat.mip = MipFlags::empty();
                        self.mark(m::MIP);
                    }
                    // C move-block entry (2241): `dval != ldvl || !dmov`
                    // consumes the pass whether or not the move block's
                    // 2457 gate dispatches — a queued home (HOMF|STOP) or a
                    // jog started while moving (JOGF|STOP) keeps its
                    // direction bits and is NOT re-dispatched here; only
                    // the home re-fires, at stop completion.
                    if self.collect_drive_fields() {
                        return;
                    }
                    let entry = self.stat.dmov == 0 || self.pos.dval != self.internal.ldvl;
                    if entry {
                        self.plan_absolute_move(effects);
                    }
                    if !entry {
                        // C chain end (2540/2546): a Go pass the move
                        // block did not consume applies a latched SYNC
                        // or fires the implicit GET_INFO.
                        self.dispatch_latent_collection(effects, true);
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
                self.mark(m::MIP);
                self.retry.rcnt = 0;
                self.mark(m::RCNT);
                self.internal.queued_motion = None;
                // Like Go, the Move pass falls through to the home
                // section (2013-2076) before the move block — a latched
                // home button fires instead of the move replan.
                if let Some(forward) = self.latent_home_request() {
                    self.start_home(forward, effects);
                } else if self.collect_drive_fields() {
                    // C 2227: the SET redefinition ends the pass.
                } else if self.stat.dmov == 0 || self.pos.dval != self.internal.ldvl {
                    self.plan_absolute_move(effects);
                } else {
                    // C chain end (2540/2546), as in the Go branch.
                    self.dispatch_latent_collection(effects, true);
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
        let min_velocity = self.effective_vbas();
        let leg = if use_rel {
            MotorCommand::MoveRelative {
                distance: self.raw_position(rel_distance),
                min_velocity: self.raw_rate(min_velocity),
                velocity: self.raw_rate(velocity),
                acceleration: self.raw_rate(acceleration),
            }
        } else {
            MotorCommand::MoveAbsolute {
                position: self.raw_position(abs_position),
                min_velocity: self.raw_rate(min_velocity),
                velocity: self.raw_rate(velocity),
                acceleration: self.raw_rate(acceleration),
            }
        };
        effects.commands.extend(leg.after_speeds(SpeedWrites::MOVE));
    }

    /// A dial position or distance in the driver's units, C's raw steps
    /// (`dval / mres`).
    pub(crate) fn raw_position(&self, dial: f64) -> f64 {
        dial / self.conv.mres
    }

    /// A speed or acceleration in the driver's units, C's steps/s and
    /// steps/s² (`velo / fabs(mres)`).
    pub(crate) fn raw_rate(&self, egu: f64) -> f64 {
        egu / self.conv.mres.abs()
    }

    /// set_dial_highlimit / set_dial_lowlimit's write (motorRecord.cc:
    /// 4236-4328): `dial / mres`, to the raw register that dial limit is
    /// under the MRES sign.
    pub(crate) fn dial_limit_command(&self, high: bool, dial: f64) -> MotorCommand {
        let position = self.raw_position(dial);
        if high != (self.conv.mres < 0.0) {
            MotorCommand::SetHighLimit { position }
        } else {
            MotorCommand::SetLowLimit { position }
        }
    }

    /// Effective base velocity for acceleration math: 0 for a driver that
    /// does not support a base velocity (epics-modules/motor #76).
    pub(crate) fn effective_vbas(&self) -> f64 {
        if self.stat.vbas_unsupported {
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
    /// A result `<= 0` is C's skipped SET_ACCEL (motorRecord.cc:940, :2529);
    /// the poller turns it into the axis's stored acceleration.
    pub(crate) fn move_accel_egu(&self) -> f64 {
        let accl = if self.vel.accl > 0.0 {
            self.vel.accl
        } else {
            0.1
        };
        let vmax = self.vel.velo.abs();
        let vmin = self.effective_vbas();
        if self.vel.accu == AccsUsed::Accs {
            self.vel.accs
        } else if vmax > vmin {
            (vmax - vmin) / accl
        } else {
            vmax / accl
        }
    }

    /// Acceleration for a backlash move, EGU/sec². Uses BVEL/BACC; `<= 0`
    /// is C's skipped SET_ACCEL (:956, :1005), as in `move_accel_egu`.
    pub(crate) fn backlash_accel_egu(&self) -> f64 {
        let bacc = if self.vel.bacc > 0.0 {
            self.vel.bacc
        } else {
            0.1
        };
        let span = self.vel.bvel - self.effective_vbas();
        if span > 0.0 {
            span / bacc
        } else {
            self.vel.bvel / bacc
        }
    }

    /// Acceleration for a home, EGU/sec². C derives it from HVEL, not
    /// VELO — both the direct dispatch (motorRecord.cc:2046-2048) and the
    /// queued-home re-fire (859-862) compute
    /// `(hvel - vbase) > 0 ? (hvel - vbase) / accl : hvel / accl`; `<= 0`
    /// is C's skipped SET_ACCEL (:874, :2054), as in `move_accel_egu`.
    pub(crate) fn home_accel_egu(&self) -> f64 {
        let accl = if self.vel.accl > 0.0 {
            self.vel.accl
        } else {
            0.1
        };
        let span = self.vel.hvel - self.effective_vbas();
        if span > 0.0 {
            span / accl
        } else {
            self.vel.hvel / accl
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
        self.mark(m::VAL | m::DVAL | m::RVAL);
        if self.stat.mip.contains(MipFlags::RETRY) {
            self.stat.mip = MipFlags::empty();
            self.mark(m::MIP);
        }
        if self.stat.mip.is_empty() && self.stat.dmov == 0 {
            self.set_dmov(true);
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

    /// C load_pos (motorRecord.cc:3771-3817): calc and load a new raw
    /// position into the controller WITHOUT moving it. The drive triplet
    /// re-anchors (ldvl/lval/lrvl), the FOFF branch keeps either the
    /// user value (Variable: offset recomputed + limits retranslated)
    /// or the offset (Frozen: VAL retranslated), MIP becomes LOAD_P
    /// wholesale and DMOV pulses low (3802-3808). LOAD_POS is followed
    /// by GET_INFO — `request_poll` — and the status callback completes
    /// the cycle (process 1405-1409: MIP_LOAD_P -> MIP_DONE, DMOV ->
    /// TRUE, no DLY and no retry evaluation).
    pub(crate) fn load_pos(&mut self, effects: &mut ProcessEffects) {
        self.internal.ldvl = self.pos.dval;
        self.internal.lval = self.pos.val;
        if self.conv.mres != 0.0 {
            let rval = coordinate::to_long((self.pos.dval / self.conv.mres).round());
            if self.pos.rval != rval {
                self.mark(m::RVAL);
            }
            self.pos.rval = rval;
        }
        self.internal.lrvl = self.pos.rval;
        if self.conv.foff == FreezeOffset::Frozen {
            // C 3782-3791: translate dial to user through the frozen
            // offset.
            self.pos.val = coordinate::dial_to_user(self.pos.dval, self.conv.dir, self.pos.off);
            self.mark(m::VAL);
            self.internal.lval = self.pos.val;
        } else {
            // C 3792-3800: adjust the offset to keep VAL and retranslate
            // the user limits.
            self.pos.off = coordinate::calc_offset(self.pos.val, self.pos.dval, self.conv.dir);
            self.mark(m::OFF);
            self.set_userlimits();
        }
        // C load_pos (motorRecord.cc:3771-3817) marks M_RVAL/M_VAL/M_OFF/
        // M_MIP/M_DMOV but never recomputes or MARKs RBV: the readback is
        // left in the pre-LOAD_POS frame until the GET_INFO callback re-runs
        // process_motor_info (3717), which re-derives RBV from the fresh
        // DRBV and the new OFF. Recomputing RBV here would post it in the
        // new frame one poll early while DRBV is still the old readback.
        self.stat.mip = MipFlags::LOAD_P;
        self.mark(m::MIP);
        self.set_dmov(false);
        effects.commands.push(MotorCommand::SetPosition {
            position: self.raw_position(self.pos.dval),
        });
        // C sends GET_INFO after LOAD_POS (motorRecord.cc:3814-3816).
        effects.commands.push(MotorCommand::Poll);
    }

    /// C do_work resolution block (motorRecord.cc:1937-1991): a runtime
    /// MRES/ERES/UEIP change re-anchors the record. USE mode loads the
    /// controller position at the redefined DVAL (load_pos, 1988-1989);
    /// SET mode arms post-process and re-reads the controller
    /// (1981-1987), so the status callback re-derives the drive values
    /// at the new resolution. Runs ungated by stop_or_pause — a steady
    /// SPMG Stop/Pause does not suppress it in C.
    ///
    /// With an encoder present the ratio goes down first (SET_ENC_RATIO,
    /// 1946-1978).
    pub(crate) fn dispatch_res_reanchor(&mut self, effects: &mut ProcessEffects) {
        let encoder = self.stat.msta.contains(MstaFlags::ENCODER_PRESENT);
        if encoder {
            // C 1950-1959: defend the ratio math against MRES ~ 0 and
            // an unset ERES.
            if self.conv.mres.abs() < 1e-9 {
                self.conv.mres = 1.0;
            }
            if self.conv.eres == 0.0 {
                self.conv.eres = self.conv.mres;
            }
        }
        // C 1971: make sure the retry deadband is achievable.
        self.enforce_min_retry_deadband();
        if encoder {
            // C 1961-1966: the ratio as encoder pulses over motor pulses
            // of one scaled count, divided in devMotorAsyn build_trans.
            let (mres, eres) = (self.conv.mres, self.conv.eres);
            let mut m = 10_000_000.0_f64;
            while m > 1.0 && ((m / eres).abs() > 1e6 || (m / mres).abs() > 1e6) {
                m /= 10.0;
            }
            effects.commands.push(MotorCommand::SetEncoderRatio {
                pulses: [m / eres, m / mres],
            });
        }
        if self.conv.set && self.conv.igset == 0 {
            self.internal.pp = 1;
            effects.commands.push(MotorCommand::Poll);
        } else if !self.stat.mip.contains(MipFlags::LOAD_P) {
            self.load_pos(effects);
        }
    }

    /// C set_userlimits (motorRecord.cc:4334-4348): translate the dial
    /// limits to user limits through DIR/OFF. Single owner — called
    /// wherever OFF or the dial pair changes (offset redefinition, DIR
    /// flip, limit writes, MRES rescale, load_pos Variable leg).
    pub(crate) fn set_userlimits(&mut self) {
        let (hlm, llm) = coordinate::dial_limits_to_user(
            self.limits.dhlm,
            self.limits.dllm,
            self.conv.dir,
            self.pos.off,
        );
        self.limits.hlm = hlm;
        self.limits.llm = llm;
        self.mark(m::HLM | m::LLM);
    }

    /// C do_work collection after the tweak fold (2182-2235), the single
    /// place a drive-field put cascades: RLV folds into VAL and RVAL
    /// propagates to DVAL inside the closed-loop-bypassed else (1994),
    /// then a VAL change either redefines the offset (SET with a variable
    /// offset, 2206-2227) or propagates to DVAL. Returns true when the
    /// redefinition ended the pass (C 2227 return), which C runs ahead of
    /// the stop_or_pause return (2237).
    pub(super) fn collect_drive_fields(&mut self) -> bool {
        if !self.closed_loop_dol_collection() {
            if self.pos.rlv != self.internal.lrlv {
                // #231: LOAD_POS blocked — refuse the Frozen-leg
                // redefinition the fold would make; consume the RLV.
                let refused = self.conv.set
                    && self.conv.igset == 0
                    && self.conv.foff == FreezeOffset::Frozen
                    && self.conv.loadpos_blocked;
                if !refused {
                    self.pos.val += self.pos.rlv;
                }
                self.pos.rlv = 0.0;
                self.mark(m::RLV);
                self.internal.lrlv = self.pos.rlv;
            }
            if self.pos.rval != self.internal.lrvl {
                self.pos.dval = coordinate::raw_to_dial(self.pos.rval, self.conv.mres);
            }
        }
        if self.pos.val != self.internal.lval {
            self.mark(m::VAL);
            if self.conv.set && self.conv.igset == 0 && self.conv.foff == FreezeOffset::Variable {
                self.set_mode_redefine_val(self.pos.val);
                return true;
            }
            self.pos.dval = coordinate::user_to_dial(self.pos.val, self.conv.dir, self.pos.off);
        }
        false
    }

    /// C SET + FOFF=Variable VAL collection (motorRecord.cc:2206-2227):
    /// redefine VAL to `new_val` by adjusting the offset (DVAL
    /// untouched), retranslate the user limits and RBV, sync LVAL, and
    /// complete on the spot — mip = MIP_DONE, dmov = TRUE, no
    /// controller command (LOAD_POS belongs to the Frozen/DVAL/RVAL
    /// redefinition paths). Called only from collect_drive_fields.
    pub(crate) fn set_mode_redefine_val(&mut self, new_val: f64) {
        if let Ok((dval, rval, off)) = coordinate::cascade_from_val(
            new_val,
            self.conv.dir,
            self.pos.off,
            self.conv.foff,
            self.conv.mres,
            true,
            self.pos.dval,
        ) {
            self.pos.val = new_val;
            self.pos.dval = dval;
            self.pos.rval = rval;
            self.pos.off = off;
            self.set_userlimits();
            self.pos.rbv = coordinate::dial_to_user(self.pos.drbv, self.conv.dir, self.pos.off);
            self.internal.lval = self.pos.val;
            self.stat.mip = MipFlags::empty();
            self.stat.dmov = 1;
            // C 2206-2226, all unconditional.
            self.mark(m::VAL | m::OFF | m::RBV | m::MIP | m::DMOV);
            self.set_phase(MotionPhase::Idle);
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

    /// C clear_buttons (motorRecord.cc:4386-4408): release all four
    /// latched motion buttons (JOGF/JOGR/HOMF/HOMR).
    pub(crate) fn clear_buttons(&mut self) {
        let mut released = 0;
        for (button, bit) in [
            (&mut self.ctrl.jogf, m::JOGF),
            (&mut self.ctrl.jogr, m::JOGR),
            (&mut self.ctrl.homf, m::HOMF),
            (&mut self.ctrl.homr, m::HOMR),
        ] {
            if std::mem::take(button) != 0 {
                released |= bit;
            }
        }
        self.mark(released);
    }

    /// C SYNC apply (motorRecord.cc:2540-2544): `else if (sync != 0 &&
    /// mip == MIP_DONE)` at the end of the do_work dispatch chain — the
    /// latched SYNC consumes only on a pass that dispatched nothing, with
    /// the record idle and done. SPMG Stop/Pause never reaches the chain
    /// end (the stop_or_pause return at 2237 precedes it), so the latch
    /// survives a pause and applies on Go. Returns true when it applied.
    pub(super) fn apply_latent_sync(&mut self) -> bool {
        if self.internal.sync == 0
            || self.stat.phase != MotionPhase::Idle
            || !self.stat.mip.is_empty()
            || self.stat.dmov == 0
            || !matches!(self.ctrl.spmg, SpmgMode::Go | SpmgMode::Move)
        {
            return false;
        }
        self.internal.sync = 0;
        self.sync_target_position();
        true
    }

    /// Check if a hardware limit blocks motion in the given direction.
    fn is_blocked_by_hw_limit(&self, dir: MotionDirection) -> bool {
        match dir {
            MotionDirection::Positive => self.limits.hls != 0,
            MotionDirection::Negative => self.limits.lls != 0,
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

        // STUP protocol. C do_work top (1817-1830): STUP=ON transitions to
        // BUSY and fires GET_INFO, then *continues* the pass — it does not
        // early-return — so a user write or device update arriving in the
        // same cycle is not dropped. The data callback returns BUSY to OFF
        // (process_exit 1498-1502, ported in determine_event). C 1824-1828:
        // a device that cannot service GET_INFO (WRITE_MSG returns ERROR,
        // e.g. Soft Channel) drops STUP back to OFF instead of holding
        // BUSY — with no driver attached the refresh has no consumer and
        // the BUSY->OFF callback never arrives, so a stuck BUSY would
        // refuse every later STUP put (special before-write, 2615-2617).
        let stup_requested = self.stat.stup == 1;
        let stup_fired = stup_requested && self.can_get_info();
        if stup_requested {
            self.stat.stup = if stup_fired { 2 } else { 0 };
            self.mark(m::STUP);
        }
        let mut effects = self.do_process_inner();
        if stup_fired {
            // Sent before the rest of the pass, as C's do_work top does.
            effects.commands.insert(0, MotorCommand::Poll);
        }
        effects
    }

    /// Whether `build_trans` would queue a GET_INFO: it returns ERROR with
    /// no device, or, devMotorAsyn's, with the record in COMM_ALARM
    /// (devMotorAsyn.c:530-532).
    fn can_get_info(&self) -> bool {
        self.device_state.as_ref().is_some_and(|state| {
            !self.internal.comm_alarm || state.lock().is_ok_and(|ds| !ds.refuses_in_comm_alarm())
        })
    }

    fn do_process_inner(&mut self) -> ProcessEffects {
        // One-pass STUP acknowledgement mark from determine_event (C
        // process_exit 1498-1502); consumed here so it never outlives
        // the pass that carried the callback.
        let stup_ack = std::mem::take(&mut self.internal.stup_ack);
        // One-pass CALLBACK_DATA mark from determine_event's Idle
        // consume — the C process_reason discriminator. Taken here so
        // it never outlives this pass: when a coalesced user write owns
        // the pass below, the mark drops unconsumed (the readback was
        // already applied; C would have run two passes).
        let idle_status_pass = std::mem::take(&mut self.internal.idle_status_pass);

        // C do_work resolution block (1936-1991) on the pass a
        // pp(TRUE) MRES/SREV/UREV/ERES/UEIP put triggers — in this
        // dispatcher that pass carries no command source. The mark
        // lives exactly one pass: when the pass would take a C
        // top-block stop return (LVIO rising, latent stop pulse, SPMG
        // transition to Stop/Pause), it is dropped unconsumed and the
        // existing latent handling owns the pass.
        if self.internal.res_reanchor && self.last_write.is_none() {
            self.internal.res_reanchor = false;
            // C enter_do_work LVIO (1463-1484) precedes do_work.
            if self.recompute_lvio_during_motion() {
                let mut effects = ProcessEffects::default();
                self.stop_axis(&mut effects);
                return effects;
            }
            let top_block_returns = self.ctrl.stop != 0
                || (self.ctrl.spmg != self.internal.lspg
                    && matches!(self.ctrl.spmg, SpmgMode::Stop | SpmgMode::Pause));
            if !top_block_returns {
                let mut effects = ProcessEffects::default();
                self.dispatch_res_reanchor(&mut effects);
                return effects;
            }
        }

        // Sub-step pulse recovery: if DMOV is false but phase is Idle
        // (no real motion started), finalize to restore DMOV=1. This is
        // the pulse's high half — C's too_small (2333-2347) restores
        // DMOV on the same pass; the Rust pulse splits it so the low
        // half posts, and too_small's lasts adoption (ldvl <- dval) is
        // what marks the pulse as started. Two states share the DMOV-
        // low/Idle/MIP-empty shape and must NOT be finalized:
        //
        // - A put-owned pass: the C special() pass-0 blink (2591-2620)
        //   drops DMOV before the record processes, so a pending
        //   last_write/UserWrite arrives here with DMOV already low —
        //   the blink is the pass's move-block entry ticket (2241), not
        //   a completed pulse, and the pass must reach its dispatch arm.
        // - A parked blinked put (dval != ldvl): written under
        //   SPMG=Pause/Stop, waiting for the Go pass to dispatch — C
        //   keeps DMOV low across the pause ("not done": a move is
        //   pending).
        // - The LS-error update (1367-1378) of a load on a limit: pp is
        //   armed and the GET_INFO pass must run postProcess.
        let put_owned = self.last_write.is_some()
            || matches!(self.pending_event, Some(MotorEvent::UserWrite(_)));
        if !put_owned
            && self.internal.pp == 0
            && self.stat.dmov == 0
            && self.stat.phase == MotionPhase::Idle
            && self.stat.mip.is_empty()
            && self.pos.dval == self.internal.ldvl
        {
            let mut effects = ProcessEffects::default();
            self.finalize_motion(&mut effects);
            return effects;
        }

        // C dbScanLock: one signal owns one pass. A put-owned pass
        // consumes only last_write — pending_event stays untouched for
        // the pass its io_intr pulse triggers, so a coalesced device
        // update keeps its completion pipeline and a coalesced
        // DelayExpired keeps the settle watchdog (a dropped expiry has
        // no second timer; with the poll loop idled by the delay it
        // wedged DMOV low for good). determine_event defers the mailbox
        // consume while a put is pending, so the two cannot co-occur on
        // the live path; not taking the event here keeps the invariant
        // for directly injected events too.
        if let Some(src) = self.last_write.take() {
            return self.plan_motion(src);
        }

        match self.pending_event.take() {
            Some(MotorEvent::UserWrite(cmd_src)) => self.plan_motion(cmd_src),
            Some(MotorEvent::DeviceUpdate(status)) => {
                self.process_motor_info(&status);
                // C 1345: the callback acknowledging a BUSY STUP skips
                // the motor-stopped branch — it is the GET_INFO
                // response, not a motion completion. The moving branch
                // already ran via process_motor_info (poll-time NTM
                // lives in check_completion's still-moving path, which
                // C also runs: the gate only covers the stopped case),
                // and the next poll evaluates completion.
                if stup_ack && self.stat.movn == 0 {
                    // C do_work gate (1487-1492): the GET_INFO ack pass
                    // still reaches do_work through the dmov arm, so a
                    // button/tweak latched while the STUP was in flight
                    // dispatches here even though completion is skipped.
                    // CALLBACK_DATA pass — no implicit GET_INFO (C 2546).
                    let mut effects = ProcessEffects::default();
                    self.dispatch_latent_collection(&mut effects, false);
                    return effects;
                }
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
                // Unmarked: MIP posts when the GET_INFO pass ends the wait
                // (1429-1430).
                if self.stat.mip.contains(MipFlags::DELAY_REQ) {
                    self.stat.mip.remove(MipFlags::DELAY_REQ);
                    self.stat.mip.insert(MipFlags::DELAY_ACK);
                    effects.commands.push(MotorCommand::Poll);
                }
                effects
            }
            None => {
                // A pass with no command source and no device event — in
                // the live IOC this is the process cycle a put to a field
                // that sets no last_write triggers (HLM/LLM/DHLM/DLLM are
                // pp(TRUE) in C). C still runs the enter_do_work LVIO
                // re-evaluation (1463-1484) on it: a limit lowered onto a
                // running jog must stop the axis on the write pass.
                if self.recompute_lvio_during_motion() {
                    let mut effects = ProcessEffects::default();
                    self.stop_axis(&mut effects);
                    return effects;
                }
                // C process (1301-1409): a CALLBACK_DATA pass on an idle
                // record runs the stopped branch — LOAD_P collapse
                // (1405-1409), the pp re-sync (1382-1402), the EXTERNAL
                // close — before falling into do_work. determine_event
                // consumed the status in place and left the one-pass
                // mark; route it through check_completion so the live
                // idle poll reaches that pipeline (its Idle arm), not
                // only the EXTERNAL case.
                if idle_status_pass || self.stat.mip.contains(MipFlags::EXTERNAL) {
                    self.check_completion()
                } else {
                    // C do_work gate (1487-1492): the put pass continues
                    // into do_work, whose home/jog/tweak sections act on
                    // latched button state and whose chain end may fire
                    // the implicit GET_INFO (2546-2557, NOTHING_DONE
                    // passes only — put_pass carries that discriminator).
                    let mut effects = ProcessEffects::default();
                    if !self.jog_stop_section(&mut effects) {
                        self.dispatch_latent_collection(&mut effects, true);
                    }
                    effects
                }
            }
        }
    }
}
