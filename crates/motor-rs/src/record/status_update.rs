use super::*;

impl MotorRecord {
    /// Determine the event for this process cycle by reading shared device state.
    pub(crate) fn determine_event(&mut self) -> Option<MotorEvent> {
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
            if !self.initialized {
                self.initialized = true;
                return Some(MotorEvent::Startup);
            }
            // Always apply readback even in Idle
            if self.stat.phase == MotionPhase::Idle {
                self.process_motor_info(&status);
                return None;
            }
            return Some(MotorEvent::DeviceUpdate(status));
        }
        None
    }

    /// Convert ProcessEffects to DeviceActions for the shared mailbox.
    pub(crate) fn effects_to_actions(&mut self, effects: &ProcessEffects) -> DeviceActions {
        let poll = if effects.request_poll {
            PollDirective::Start
        } else if effects.status_refresh {
            PollDirective::Start
        } else if effects.commands.is_empty() && effects.schedule_delay.is_none() && self.stat.dmov
        {
            PollDirective::Stop
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

    /// Update readback positions from driver status.
    pub fn process_motor_info(&mut self, status: &asyn_rs::interfaces::motor::MotorStatus) {
        // Layer 1: update raw positions
        self.pos.rmp = (status.position / self.conv.mres).round() as i64;

        // REP: use ERES when UEIP is set, MRES otherwise
        let eres_valid = self.conv.eres.is_finite() && self.conv.eres != 0.0;
        if self.conv.ueip && eres_valid {
            self.pos.rep = (status.encoder_position / self.conv.eres).round() as i64;
        } else {
            if self.conv.ueip && !eres_valid {
                tracing::warn!(
                    "UEIP set but ERES invalid ({:.6}), falling back to MRES for REP",
                    self.conv.eres
                );
            }
            self.pos.rep = (status.encoder_position / self.conv.mres).round() as i64;
        }

        // RRBV depends on UEIP
        self.pos.rrbv = if self.conv.ueip {
            self.pos.rep
        } else {
            self.pos.rmp
        };

        // URIP path: use external readback link value with RRES conversion
        if !self.conv.ueip && self.conv.urip && self.initialized {
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

        // MOVN: C uses RAW limit switches (rhls/rlls) with RAW cdir
        // Must compute ls_active BEFORE mapping limits to user coordinates
        let ls_active =
            (status.high_limit && self.stat.cdir) || (status.low_limit && !self.stat.cdir);
        self.stat.movn = !(ls_active || status.done || status.problem);

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

        // Limit switches: map raw -> user based on DIR and MRES sign
        // C: hls = ((dir == Pos) == (mres >= 0)) ? rhls : rlls
        let same_polarity = (self.conv.dir == MotorDir::Pos) == (self.conv.mres >= 0.0);
        if same_polarity {
            self.limits.hls = status.high_limit;
            self.limits.lls = status.low_limit;
        } else {
            self.limits.hls = status.low_limit;
            self.limits.lls = status.high_limit;
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
        // Preserve record-managed bits
        if self.stat.msta.contains(MstaFlags::HOMED) || status.homed {
            msta |= MstaFlags::HOMED;
        }
        // Preserve ENCODER_PRESENT if record set it (via UEIP)
        if self.stat.msta.contains(MstaFlags::ENCODER_PRESENT) {
            msta |= MstaFlags::ENCODER_PRESENT;
        }
        self.stat.msta = msta;

        // C: tdir = msta.RA_DIRECTION (from driver on every poll)
        self.stat.tdir = status.direction;

        // C: devMotorAsyn.c — RVEL is the raw velocity reported by the
        // driver, stored as floor(status.velocity) (motorRecord.dbd RVEL is
        // DBF_LONG "Raw Velocity").
        self.stat.rvel = status.velocity.floor() as i64;

        // LVIO is NOT recomputed here. C re-evaluates it only at
        // enter_do_work (1462-1483: jog from live RBV, home disabled,
        // everything else preserves the latched value) — ported in
        // check_completion's still-moving path — and in the do_work move
        // block / soft-limit puts, ported in plan_absolute_move and
        // detect_inverted_limits.
    }

    /// Sync all positions from readback.
    pub fn sync_positions(&mut self) {
        self.pos.dval = self.pos.drbv;
        self.pos.val = self.pos.rbv;
        self.pos.rval = self.pos.rrbv;
        self.pos.diff = 0.0;
        self.pos.rdif = 0;
        self.internal.lval = self.pos.val;
        self.internal.ldvl = self.pos.dval;
        self.internal.lrvl = self.pos.rval;
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
        self.process_motor_info(status);

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
        } else {
            self.sync_positions();
        }

        // DMOV from driver
        self.stat.dmov = status.done && !status.moving;

        if status.moving {
            // At startup, the poll loop may not be active yet — request it.
            effects.request_poll = true;
        }

        // Check encoder presence
        if self.conv.ueip {
            self.stat.msta.insert(MstaFlags::ENCODER_PRESENT);
        }

        effects
    }
}
