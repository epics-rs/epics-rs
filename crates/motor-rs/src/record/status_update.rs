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
        // C 3671-3675: UEIP=Yes is demoted to No on any poll where the
        // driver reports no encoder. This — not the put handler — is
        // what vets a .db-loaded UEIP=Yes: dbLoadRecords writes field()
        // values raw, and the first poll performs the encoder check.
        // C posts the change without MARKing M_UEIP, so no re-anchor.
        if self.conv.ueip && !status.has_encoder {
            self.conv.ueip = false;
        }

        // Layer 1: update raw positions
        self.pos.rmp = (status.position / self.conv.mres).round() as i64;

        // C devMotorAsyn.c:459-464 — REP is the raw encoder count,
        // independent of UEIP (C rounds the count the asyn layer already
        // reports raw; the Rust EGU convention converts at the boundary,
        // so the encoder scale is ERES whether or not the readback uses
        // it). MRES is only a fallback for an invalid runtime ERES.
        let eres_valid = self.conv.eres.is_finite() && self.conv.eres != 0.0;
        if eres_valid {
            self.pos.rep = (status.encoder_position / self.conv.eres).round() as i64;
        } else {
            if self.conv.ueip {
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
        if status.encoder_home {
            msta |= MstaFlags::EA_HOME;
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
        // Preserve record-managed bits. EA_PRESENT is NOT preserved:
        // C overwrites pmr->msta wholesale from the driver each poll,
        // and the UEIP demotion above depends on that bit being pure
        // driver truth rather than a record-side latch.
        if self.stat.msta.contains(MstaFlags::HOMED) || status.homed {
            msta |= MstaFlags::HOMED;
        }
        self.stat.msta = msta;

        // C: tdir = msta.RA_DIRECTION (from driver on every poll)
        self.stat.tdir = status.direction;

        // C 3755-3762: ATHM tracks the motor's home switch — or the
        // encoder's home signal when UEIP=Yes — on every poll. It is
        // pure switch readback, not a "has homed" latch (that is MSTA
        // bit 14, RA_HOMED).
        self.stat.athm = if self.conv.ueip {
            msta.contains(MstaFlags::EA_HOME)
        } else {
            msta.contains(MstaFlags::HOME_LS)
        };

        // C devMotorAsyn.c:469-474 — RVEL is the RAW velocity in steps/s
        // (motorRecord.dbd DBF_LONG "Raw Velocity"), floor()ed as in C.
        // The C asyn layer reports controller units (raw steps); the Rust
        // driver convention is EGU (AsynMotor docs), so convert through
        // MRES at the record boundary exactly like RMP above.
        self.stat.rvel = if self.conv.mres != 0.0 {
            (status.velocity / self.conv.mres).floor() as i64
        } else {
            0
        };

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
        } else if self.links.omsl == 1 {
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

        // DMOV from driver
        self.stat.dmov = status.done && !status.moving;

        if status.moving {
            // At startup, the poll loop may not be active yet — request it.
            effects.request_poll = true;
        }

        effects
    }
}
