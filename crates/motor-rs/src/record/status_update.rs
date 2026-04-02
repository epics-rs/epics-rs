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
        self.pos.rmp = (status.position / self.conv.mres).round() as i32;

        // REP: use ERES when UEIP is set, MRES otherwise
        let eres_valid = self.conv.eres.is_finite() && self.conv.eres != 0.0;
        if self.conv.ueip && eres_valid {
            self.pos.rep = (status.encoder_position / self.conv.eres).round() as i32;
        } else {
            if self.conv.ueip && !eres_valid {
                tracing::warn!(
                    "UEIP set but ERES invalid ({:.6}), falling back to MRES for REP",
                    self.conv.eres
                );
            }
            self.pos.rep = (status.encoder_position / self.conv.mres).round() as i32;
        }

        // RRBV depends on UEIP
        self.pos.rrbv = if self.conv.ueip {
            self.pos.rep
        } else {
            self.pos.rmp
        };

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
        self.pos.rdif = self.pos.val - self.pos.rbv;

        // MOVN: true if phase is active OR driver reports moving
        self.stat.movn = self.stat.phase != MotionPhase::Idle || status.moving;

        // Build MSTA from driver status
        let mut msta = MstaFlags::empty();
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
            msta |= MstaFlags::GAIN_SUPPORT;
        }
        if status.problem {
            msta |= MstaFlags::PROBLEM;
        }
        // Preserve record-managed bits
        if self.stat.msta.contains(MstaFlags::HOMED) {
            msta |= MstaFlags::HOMED;
        }
        if self.stat.msta.contains(MstaFlags::ENCODER_PRESENT) {
            msta |= MstaFlags::ENCODER_PRESENT;
        }
        self.stat.msta = msta;

        // Limit switches
        self.limits.hls = status.high_limit;
        self.limits.lls = status.low_limit;

        // Recompute LVIO from current position and soft limits
        self.limits.lvio =
            coordinate::check_soft_limits(self.pos.dval, self.limits.dhlm, self.limits.dllm);
    }

    /// Sync all positions from readback.
    pub fn sync_positions(&mut self) {
        self.pos.dval = self.pos.drbv;
        self.pos.val = self.pos.rbv;
        self.pos.rval = self.pos.rrbv;
        self.pos.diff = 0.0;
        self.pos.rdif = 0.0;
        self.internal.lval = self.pos.val;
        self.internal.ldvl = self.pos.dval;
        self.internal.lrvl = self.pos.rval;
    }

    /// Initial readback and position sync at startup.
    pub fn initial_readback(
        &mut self,
        status: &asyn_rs::interfaces::motor::MotorStatus,
    ) -> ProcessEffects {
        let mut effects = ProcessEffects::default();

        self.process_motor_info(status);
        self.sync_positions();

        // DMOV from driver
        self.stat.dmov = status.done && !status.moving;

        if status.moving {
            effects.request_poll = true;
            effects.suppress_forward_link = true;
        }

        // Check encoder presence
        if self.conv.ueip {
            self.stat.msta.insert(MstaFlags::ENCODER_PRESENT);
        }

        effects
    }
}
