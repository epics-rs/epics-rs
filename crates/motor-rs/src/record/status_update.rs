use super::*;

impl MotorRecord {
    /// C `update_values`'s line (devMotorAsyn.c:445-447), which every
    /// process prints.
    fn trace_update_values(&self, need_update: bool) {
        let trace = self.trace();
        trace.print(
            asyn_rs::trace::TraceMask::IO_DEVICE,
            trace.update_status_reason(),
            format_args!(
                "{} devMotorAsyn::update_values, needUpdate={}\n",
                trace.record(),
                i32::from(need_update)
            ),
        );
    }

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
        // A model-1 driver's `mr->cdir` write is no callback payload: it is
        // the record's own field, seen by any pass.
        let put_pass = self.last_write.is_some() || self.internal.res_reanchor;
        let mailbox = self.device_state.as_ref().and_then(|state| {
            let mut ds = state.lock().ok()?;
            let cdir = ds.driver_cdir.take();
            Some(if put_pass {
                (cdir, None, None)
            } else {
                (cdir, ds.expired_delay_id.take(), ds.update_values())
            })
        });
        if let Some((Some(cdir), ..)) = mailbox {
            self.stat.cdir = cdir;
        }
        if put_pass {
            self.trace_update_values(false);
            return None;
        }
        let (_, delay_id, new_status) = mailbox?;
        self.trace_update_values(new_status.is_some());

        // Check delay expiry first (higher priority)
        if let Some(delay_id) = delay_id {
            if delay_id == self.next_delay_id.wrapping_sub(1) {
                return Some(MotorEvent::DelayExpired);
            }
            // Stale delay -- ignore
        }

        // Check for new motor status
        if let Some(status) = new_status {
            // C process_exit (1498-1502): a CALLBACK_DATA pass returns a
            // BUSY STUP to OFF — the GET_INFO response has arrived. The
            // pre-clear value is latched one pass so the done branch can
            // apply the C 1345 gate (the ack is not a motion completion).
            if self.stat.stup == 2 {
                self.stat.stup = 0;
                self.mark(m::STUP);
                self.internal.stup_ack = true;
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
        let schedule_delay = effects.schedule_delay.map(|dur| {
            let id = self.next_delay_id;
            self.next_delay_id += 1;
            DelayRequest { id, duration: dur }
        });

        DeviceActions {
            commands: self
                .transaction(effects.commands.clone())
                .unwrap_or_default(),
            force_poll: effects.request_poll,
            schedule_delay,
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
    /// (`initial_readback`) therefore calls
    /// `process_motor_info_initcall(.., true)`.
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
            self.mark(m::UEIP_POSTED);
        }
        let (old_rrbv, old_drbv, old_rbv) = (self.pos.rrbv, self.pos.drbv, self.pos.rbv);
        let (old_tdir, old_movn) = (self.stat.tdir, self.stat.movn);
        let (old_hls, old_lls, old_athm) = (self.limits.hls, self.limits.lls, self.stat.athm);

        // Layer 1: the raw counts, devMotorAsyn update_values
        // (devMotorAsyn.c:452-464): `(epicsInt32)floor(x + 0.5)`, half toward
        // +inf and INT_MIN out of range (x86) — not NINT, which the other raw
        // conversions in this file (rdif, rval, URIP rrbv) use.
        self.pos.rmp = epics_base_rs::types::c_cast::f64_to_i32((status.position + 0.5).floor());
        self.pos.rep =
            epics_base_rs::types::c_cast::f64_to_i32((status.encoder_position + 0.5).floor());
        let eres_valid = self.conv.eres.is_finite() && self.conv.eres != 0.0;

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
                self.pos.rrbv = coordinate::to_long(((rdbl_value * rres) / self.conv.mres).round());
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
        // C 3711-3720.
        if self.pos.rrbv != old_rrbv {
            self.mark(m::RRBV);
        }
        if self.pos.drbv != old_drbv {
            self.mark(m::DRBV);
        }
        if self.pos.rbv != old_rbv {
            self.mark(m::RBV);
        }

        // DIFF and RDIF
        self.pos.diff = self.pos.dval - self.pos.drbv;
        // C: rdif = NINT(diff / mres) -- raw step difference
        self.pos.rdif = if self.conv.mres != 0.0 {
            coordinate::to_long((self.pos.diff / self.conv.mres).round())
        } else {
            0
        };
        // C 3764-3767: marked whether or not they moved.
        self.mark(m::DIFF | m::RDIF);

        // MOVN: C uses RAW limit switches (rhls/rlls) with RAW cdir
        // Must compute ls_active BEFORE mapping limits to user coordinates
        let ls_active =
            (status.high_limit && self.stat.cdir) || (status.low_limit && !self.stat.cdir);
        self.stat.movn = !(ls_active || status.done || status.problem);
        if self.stat.movn != old_movn {
            self.mark(m::MOVN);
        }
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
        // C runs this in process() only, never at init_record.
        if !initcall
            && self.stat.movn
            && self.stat.dmov
            && self.stat.phase == MotionPhase::Idle
            && !self.stat.mip.contains(MipFlags::EXTERNAL)
        {
            self.stat.dmov = false;
            self.stat.mip |= MipFlags::EXTERNAL;
            // C process 1315-1318.
            self.mark(m::DMOV | m::MIP);
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
        if self.limits.hls != old_hls {
            self.mark(m::HLS);
        }
        if self.limits.lls != old_lls {
            self.mark(m::LLS);
        }

        let msta = MstaFlags::from_status(status);
        let msta_changed = msta != self.stat.msta;
        self.stat.msta = msta;
        self.stat.vbas_unsupported = !status.vbas_supported;
        // C process() 1277-1278, right after update_values.
        if msta_changed {
            self.mark(m::MSTA);
        }

        // C: tdir = msta.RA_DIRECTION (from driver on every poll)
        self.stat.tdir = status.direction;
        if self.stat.tdir != old_tdir {
            self.mark(m::TDIR);
        }

        // C 3755-3762: ATHM tracks the motor's home switch — or the
        // encoder's home signal when UEIP=Yes — on every poll. It is
        // pure switch readback, not a "has homed" latch (that is MSTA
        // bit 14, RA_HOMED).
        self.stat.athm = if self.conv.ueip {
            msta.contains(MstaFlags::EA_HOME)
        } else {
            msta.contains(MstaFlags::HOME_LS)
        };
        if self.stat.athm != old_athm {
            self.mark(m::ATHM);
        }

        // C devMotorAsyn.c:469-474.
        self.stat.rvel = epics_base_rs::types::c_cast::f64_to_i32(status.velocity.floor());

        // LVIO is NOT recomputed here. C re-evaluates it only at
        // enter_do_work (1463-1484: jog from live RBV, home disabled,
        // everything else preserves the latched value) — ported in
        // check_completion's still-moving path — and in the do_work move
        // block / soft-limit puts, ported in plan_absolute_move and
        // detect_inverted_limits.

        if initcall {
            // C init_record 721-724, right after its process_motor_info(true):
            // DMOV starts done and MOVN clear whatever the driver reports; a
            // moving axis is picked up as external on the next pass.
            self.stat.dmov = true;
            self.stat.movn = false;
            self.mark(m::DMOV | m::MOVN);
        }
    }

    /// Sync all positions from readback.
    pub fn sync_positions(&mut self) {
        self.pos.dval = self.pos.drbv;
        self.pos.val = self.pos.rbv;
        // C 712/843/4455: the synced RVAL is the raw MOTOR command
        // equivalent of the dial value — NINT(dval/mres) — never RRBV,
        // which is encoder counts under UEIP=Yes (or the RDBL scaling
        // under URIP) and differs whenever ERES != MRES.
        self.pos.rval = coordinate::to_long((self.pos.dval / self.conv.mres).round());
        self.pos.diff = 0.0;
        self.pos.rdif = 0;
        self.internal.lval = self.pos.val;
        self.internal.ldvl = self.pos.dval;
        self.internal.lrvl = self.pos.rval;
        self.mark(m::VAL | m::DVAL | m::RVAL);
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
            self.mark(m::DIFF | m::RDIF);
        }
    }

    /// C `init_record`'s device half (motorRecord.cc:646-733 with
    /// devMotorAsyn.c `init_controller`), run from the device init and never
    /// on a process pass: C processes a record only from its callbacks, and
    /// a record on a missing axis never gets one. The commands it returns
    /// are init_controller's writes.
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
        // No put precedes init_record in C: a pass-0 restore is a plain
        // field write, not a move.
        self.last_write = None;
        self.finish_init();

        // Capture the autosaved target before the driver readback overwrites it.
        let autosaved_dval = self.pos.dval;
        let autosaved_rval = self.pos.rval;
        // C init_record runs `process_motor_info(pmr, true)` — the init call
        // skips URIP RDBL scaling and seeds the readback from motor position.
        self.process_motor_info_initcall(status, true);

        // motordevCom sends INIT before anything else (motordevCom.cc:
        // 309-327); devMotorAsyn has no INIT, and its drivers ignore it.
        if !self.links.init.is_empty() {
            effects.commands.push(MotorCommand::Primitive {
                init: self.links.init.clone(),
            });
        }

        // devMotorSim has no init_controller (devMotorSim.c init_record):
        // no ratio download and no position restore.
        let init_controller = !self.trace().is_motor_interface();

        // C init_controller (devMotorAsyn.c:183-198) downloads the encoder
        // ratio before anything else, parked write or not.
        let ratio = if self.conv.eres == 0.0 {
            1.0
        } else {
            self.conv.mres / self.conv.eres
        };
        if init_controller {
            effects
                .commands
                .push(MotorCommand::InitEncoderRatio { ratio });
        }

        // C: devMotorAsyn.c init_controller — RSTM restore decision.
        // rdbd = max(|RDBD|, |MRES|); dval_non_zero_pos_near_zero is true when
        // the autosaved DVAL is meaningful but the driver currently sits near
        // zero (i.e. the controller lost its position across the IOC restart).
        //
        // C compares `fabs(status.position * mres)` — the motor position,
        // not DRBV, which follows the encoder when UEIP=Yes.
        let rdbd = self.retry.rdbd.abs().max(self.conv.mres.abs());
        let motor_dial = status.position * self.conv.mres;
        let dval_non_zero_pos_near_zero =
            autosaved_dval.abs() > rdbd && self.conv.mres != 0.0 && motor_dial.abs() < rdbd;
        let mut restore = init_controller
            && self
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
            // C init_controller has no such check, so nothing to print.
            if (rval_implied_dval - autosaved_dval).abs() > rdbd {
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
            effects.commands.push(MotorCommand::InitPosition {
                position: self.raw_position(autosaved_dval),
            });
        } else {
            self.trace().print(
                asyn_rs::trace::TraceMask::FLOW,
                self.trace().status_reason(),
                format_args!(
                    "devMotorAsyn::init_controller, {} setting of position not required, position={:.6}, mres={:.6}, dval={autosaved_dval:.6}, rdbd={rdbd:.6}",
                    self.trace().record(),
                    status.position,
                    self.conv.mres,
                ),
            );
            if self.links.omsl == 1 {
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
        }

        effects
    }
}
