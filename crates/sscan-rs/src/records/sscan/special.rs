//! `special()` and what it calls: the linear-scan parameter algebra
//! (`adjLinParms`, `changedNpts`), `checkScanLimits`, `previewScan` and the
//! freeze-flag / table-mode bookkeeping (`sscanRecord.c:1260-1845`,
//! `:4544-5530`).

use std::time::Instant;

use epics_base_rs::server::record::Special;

use super::*;

/// `special(111)`..`special(120)`: C `SPC_SC_S`..`SPC_SC_MO`.
const SPC_SC_S: i16 = 111;
const SPC_SC_W: i16 = 115;
const SPC_SC_N: i16 = 116;
const SPC_SC_FFO: i16 = 117;
const SPC_SC_F: i16 = 118;
const SPC_SC_MO: i16 = 120;

/// The code C's "no handler" message prints.
fn special_code(s: Special) -> i16 {
    match s {
        Special::Record(n) => n,
        // `SPC_MOD`.
        _ => 100,
    }
}

impl SscanRecord {
    /// C `special()`. `false` is C's `return(-1)`: the put is refused.
    pub(crate) fn special_field(&mut self, own: OwnFrame<'_>, o: usize, after: bool) -> bool {
        self.clamp_npts();
        if !after {
            return self.special_before(own, o);
        }
        let spc = SSCAN_FIELDS[o].special;
        match spc {
            Special::Mod => return self.special_mod(own, o),
            Special::Record(n @ SPC_SC_S..=SPC_SC_W) => {
                let prev = self.alrt;
                self.alrt = 0;
                self.smsg = PvString::default();
                self.adj_lin_parms(o, n);
                self.post(SMSG);
                if self.alrt != prev {
                    self.post(ALRT);
                }
                if knob(&SSCAN_RECORD_VIEW_POS) != 0 {
                    self.preview_scan();
                }
            }
            Special::Record(SPC_SC_N) => {
                if self.npts > self.mpts {
                    self.npts = self.mpts;
                    self.post(NPTS);
                } else if self.npts < 1 {
                    self.npts = 1;
                    self.post(NPTS);
                }
                let prev = self.alrt;
                self.alrt = 0;
                self.smsg = PvString::default();
                self.npts_cause = -1;
                self.changed_npts();
                self.post(SMSG);
                if self.alrt != prev {
                    self.post(ALRT);
                }
                if knob(&SSCAN_RECORD_VIEW_POS) != 0 {
                    self.preview_scan();
                }
            }
            Special::Record(SPC_SC_FFO) => {
                if self.ffo != 0 && self.pffo == 0 {
                    self.save_frz_flags();
                    self.reset_frz_flags();
                } else if self.ffo == 0 && self.pffo != 0 {
                    self.restore_frz_flags();
                }
            }
            Special::Record(SPC_SC_F) => {
                if self.ffo != 0 {
                    self.reset_frz_flags();
                }
            }
            Special::Record(SPC_SC_MO) => self.step_mode_changed(o),
            _ => self.errlog(&format!(
                "{}:special(), no handler for special_type {}",
                self.name,
                special_code(spc)
            )),
        }
        true
    }

    /// `special(.., after = 0)`.
    fn special_before(&mut self, own: OwnFrame<'_>, o: usize) -> bool {
        self.pffo = self.ffo;
        let pv_field = (PV0..PV0 + NUM_PVS).contains(&o);
        if self.busy != 0 {
            if o == ACQM || o == ACQT || pv_field {
                return false;
            }
            if self.faze > faze::BEFORE_SCAN_WAIT && self.faze < faze::AFTER_SCAN_DO {
                return match o {
                    // C reads CMND before the put lands: the value it tests
                    // is the previous command.
                    CMND => self.cmnd == cmnd::CLEAR_MSG,
                    EXSC | PAUS | WAIT | AWCT | AWAIT | ATIME => true,
                    _ => {
                        self.errlog(&format!(
                            "{}:special:write to field index {} while scan in progress.",
                            self.name,
                            COMMON + o
                        ));
                        false
                    }
                };
            }
        }
        if pv_field {
            let li = o - PV0;
            self.wait_for_connect(own, li);
            // Positioners have a second link, POSITIONER_OUT.
            if li < NUM_POS {
                self.wait_for_connect(own, li + NUM_PVS);
            }
        }
        true
    }

    /// `special(.., after = 1)` for `SPC_MOD`.
    fn special_mod(&mut self, own: OwnFrame<'_>, o: usize) -> bool {
        match o {
            EXSC => return self.exsc_written(own),
            DDLY | PDLY | RDLY => {
                let tics = self.tics;
                let round = |v: f32| {
                    let v = if v < 0.0 { 0.0 } else { v };
                    (nint(f64::from(v) * tics) / tics) as f32
                };
                match o {
                    DDLY => self.ddly = round(self.ddly),
                    PDLY => self.pdly = round(self.pdly),
                    _ => self.rdly = round(self.rdly),
                }
                // C posts PDLY for an RDLY write (copy-paste slip): a PDLY
                // event for a field that did not change.
                self.post(o);
            }
            PAUS => {
                if self.paus != self.lpau {
                    if self.paus == 0 {
                        self.set_smsg("Scan pause rescinded");
                        if !self.any_callbacks() {
                            if self.wtng != 0 || self.await_ != 0 {
                                self.set_smsg("Waiting for client");
                            } else {
                                self.called_by = called_by::SPECIAL_PAUS;
                                if self.rdly < 0.001 {
                                    self.scan_once();
                                } else {
                                    self.request_delay(self.rdly);
                                }
                            }
                        }
                    } else {
                        self.cancel_delay();
                        self.set_smsg("Scan pause asserted");
                    }
                }
                self.lpau = self.paus;
            }
            CMND => self.command(),
            AWCT => {
                if self.awct < 0 {
                    self.awct = 0;
                    self.post(AWCT);
                }
            }
            WAIT => {
                if self.wait != 0 {
                    self.wcnt += 1;
                    self.post(WCNT);
                } else {
                    if self.wcnt > 0 {
                        self.wcnt -= 1;
                        self.post(WCNT);
                    }
                    if self.wtng != 0 && self.wcnt == 0 {
                        self.wtng = 0;
                        self.post(WTNG);
                        if self.paus != 0 {
                            self.set_smsg("Wait end, but scan is paused ...");
                        } else {
                            self.set_smsg("Scanning ...");
                            self.called_by = called_by::SPECIAL_WAIT;
                            self.scan_once();
                        }
                    }
                }
            }
            AWAIT => {
                if self.await_ != 0 {
                    self.user_set_await = true;
                }
                if self.dstate == dstate::SAVE_DATA_WAIT && self.await_ == 0 {
                    self.called_by = called_by::SPECIAL_AWAIT;
                    self.scan_once();
                }
            }
            ACQM => self.prev_acqm = ACQM_NORMAL,
            REFD => {
                if self.refd < 1 {
                    self.refd = 1;
                    self.post(REFD);
                } else if self.refd > NUM_DET as i16 {
                    self.refd = NUM_DET as i16;
                    self.post(REFD);
                }
            }
            _ if (PV0..PV0 + NUM_PVS).contains(&o) => self.pv_written(own, o - PV0),
            _ => {}
        }
        true
    }

    /// The `EXSC` case of `special`.
    fn exsc_written(&mut self, own: OwnFrame<'_>) -> bool {
        if self.exsc == 0 {
            if self.xsc != 0 {
                self.xsc = 0;
                self.post(XSC);
                self.smsg = msg("Aborting scan");
                self.post_log(SMSG);
            } else if self.faze != faze::IDLE {
                self.kill = self.kill.wrapping_add(1);
                self.errlog(&format!(
                    "{}:special(): Killing scan (kill={}/3).",
                    self.name, self.kill
                ));
                self.smsg = msg(&format!("Killing scan (kill={}/3)", self.kill));
                self.post_log(SMSG);
                self.cancel_delay();
            } else {
                self.smsg = msg("Scan record is idle");
                self.post_log(SMSG);
            }
            return true;
        }
        if self.xsc != 0 {
            self.set_smsg("Already scanning");
            return false;
        }
        if self.busy != 0 {
            self.smsg = msg(if self.dstate == dstate::SAVE_DATA_WAIT {
                "Waiting for saveData"
            } else {
                "Waiting for callback"
            });
            self.post_log(SMSG);
            return false;
        }

        if self.faze != faze::IDLE && self.faze != faze::PREVIEW {
            self.errlog(&format!(
                "Starting new scan with unexpected faze ('{}').",
                faze_name(self.faze)
            ));
        }
        // Renew positioner links so the scan sees current limits.
        let now = Instant::now();
        let lookup = f64::from(knob(&SSCAN_RECORD_LOOKUP_TIME));
        if now.duration_since(self.last_scan_end).as_secs_f64() >= lookup {
            for i in 0..NUM_POS {
                let since = self.lpvt[i]
                    .lookup_time
                    .map_or(f64::INFINITY, |t| now.duration_since(t).as_secs_f64());
                if since >= lookup
                    && !self.lpvt[i].connect_in_progress
                    && !self.lpvt[i + NUM_PVS].connect_in_progress
                    && !self.lpvt[i].waiting_for_pos_mon
                    && !self.pvn[i].as_bytes().is_empty()
                {
                    self.nv[i] = PV_NC;
                    self.bad_input = true;
                    if self.links[i].is_added() {
                        self.links[i].clear();
                        if self.links[i + NUM_PVS].is_added() {
                            self.bad_output = true;
                            self.links[i + NUM_PVS].clear();
                        }
                    }
                    // C then waits for the clear to take; the port's clear
                    // has taken when it returns.
                    self.lpvt[i].lookup_time = Some(now);
                    self.lookup_pv(own, i);
                }
            }
        }
        self.xsc = 1;
        self.post(XSC);
        self.check_connections();
        if self.bad_output || self.bad_input {
            self.set_alrt(1);
            self.set_smsg("Waiting for PV's to connect");
            self.set_faze(faze::SCAN_PENDING);
        } else {
            self.set_alrt(0);
        }
        self.called_by = called_by::SPECIAL_EXSC;
        true
    }

    /// The `CMND` case of `special`.
    fn command(&mut self) {
        if self.cmnd == cmnd::CLEAR_MSG {
            self.set_alrt(0);
            self.set_smsg("");
        }
        if self.xsc != 0 || self.busy != 0 {
            self.cmnd = cmnd::CLEAR_MSG;
            return;
        }
        match self.cmnd {
            cmnd::CHECK_LIMITS => {
                let prev = self.alrt;
                self.alrt = 0;
                self.check_scan_limits();
                self.post(SMSG);
                if self.alrt != prev {
                    self.post(ALRT);
                }
            }
            cmnd::PREVIEW_SCAN => {
                self.set_faze(faze::PREVIEW);
                self.preview_scan();
            }
            cmnd::CLEAR_ALL_PVS | cmnd::CLEAR_POS_PVS_ETC | cmnd::CLEAR_POS_RDBK_PVS_ETC => {
                self.store_scan_passive();
                self.reset_frz_flags();
                for i in 0..NUM_POS {
                    self.pos[i].sm = SM_LINEAR;
                    self.post(pf(i, p::SM));
                    self.pos[i].ar = 0;
                    self.post(pf(i, p::AR));
                }
                self.pasm = pasm::STAY;
                self.post(PASM);
                self.ffo = 0;
                self.post(FFO);
                self.clear_pvs();
            }
            cmnd::CLEAR_POS_PVS | cmnd::CLEAR_POS_RDBK_PVS => self.clear_pvs(),
            _ => {}
        }
    }

    /// `CMND`'s clear of the PV names it selects.
    fn clear_pvs(&mut self) {
        for i in 0..NUM_PVS {
            let lt = link_type(i);
            let pos = lt == LinkType::PositionerIn;
            let rdbk = lt == LinkType::Readback;
            let clear = match self.cmnd {
                cmnd::CLEAR_ALL_PVS => true,
                cmnd::CLEAR_POS_PVS_ETC | cmnd::CLEAR_POS_PVS => pos,
                cmnd::CLEAR_POS_RDBK_PVS_ETC | cmnd::CLEAR_POS_RDBK_PVS => pos || rdbk,
                _ => false,
            };
            if !clear {
                continue;
            }
            self.pvn[i] = PvString::default();
            self.post(PV0 + i);
            self.drop_pv(i);
        }
    }

    /// A PV name that is now empty: C's shared tail of the PV-name case and
    /// `CMND`'s clear.
    fn drop_pv(&mut self, i: usize) {
        if self.nv[i] != NO_PV {
            self.nv[i] = NO_PV;
            self.post(NV0 + i);
            if self.links[i].is_added() {
                self.links[i].clear();
                if i < NUM_POS && self.links[i + NUM_PVS].is_added() {
                    self.links[i + NUM_PVS].clear();
                }
            }
            self.lpvt[i].local = None;
        } else if self.lpvt[i].ts {
            // A cleared `time` read-back: poke the client.
            self.lpvt[i].ts = false;
            self.post(NV0 + i);
        }
    }

    /// The PV-name case of `special`.
    pub(super) fn pv_written(&mut self, own: OwnFrame<'_>, i: usize) {
        let old = self.nv[i];
        if is_blank(&self.pvn[i]) {
            self.pvn[i] = PvString::default();
            self.post(PV0 + i);
        }
        if self.pvn[i].as_bytes().is_empty() {
            self.drop_pv(i);
            return;
        }
        self.nv[i] = PV_NC;
        match link_type(i) {
            LinkType::PositionerIn => {
                self.bad_output = true;
                self.bad_input = true;
            }
            LinkType::Readback | LinkType::Detector => self.bad_input = true,
            _ => self.bad_output = true,
        }
        // Posted before the lookup: the search callback may come at once.
        if self.nv[i] != old {
            self.post(NV0 + i);
        }
        self.lpvt[i].lookup_time = Some(Instant::now());
        self.lookup_pv(own, i);
    }

    /// `SPC_SC_MO`: a positioner entering or leaving table mode.
    fn step_mode_changed(&mut self, o: usize) {
        for i in 0..NUM_POS {
            if o != pf(i, p::SM) {
                continue;
            }
            let sm = self.pos[i].sm;
            if sm == SM_TABLE && self.prev_sm[i] != SM_TABLE {
                self.save_pos_parms(i);
                self.zero_pos_parms(i);
                self.prev_sm[i] = sm;
                if (self.table_pts[i] as i64) < i64::from(self.npts) {
                    self.set_smsg(&format!("Pts in P{} Table < # of steps.", i + 1));
                    if self.alrt == 0 {
                        self.set_alrt(1);
                    }
                }
            } else if sm != SM_TABLE && self.prev_sm[i] == SM_TABLE {
                self.restore_pos_parms(i);
                let prev = self.alrt;
                self.npts_cause = -1;
                self.changed_npts();
                self.post(SMSG);
                if self.alrt != prev {
                    self.post(ALRT);
                }
                self.prev_sm[i] = sm;
            }
        }
    }

    /// C `adjLinParms`: resolve positioner `i`'s linear parameters after
    /// field `o` (special code `spc`) changed.
    fn adj_lin_parms(&mut self, o: usize, spc: i16) {
        if !(POS0..DET0).contains(&o) {
            return;
        }
        let i = (o - POS0) / POS_SIZE;
        if self.pos[i].sm == SM_TABLE {
            self.zero_pos_parms(i);
            self.smsg = msg(&format!("Positioner #{} is in Table Mode !", i + 1));
            self.alrt = 1;
            return;
        }
        let fpts = self.fpts != 0;
        let mpts = self.mpts;
        let q = self.pos[i].clone();
        let (fs, fi, fe, fc, fw) = (q.fs != 0, q.fi != 0, q.fe != 0, q.fc != 0, q.fw != 0);
        let n1 = |s: &Self| f64::from((s.npts - 1).max(1));
        let exceeded = format!("P{} Request Exceeded Maximum Points !", i + 1);
        let constrained = format!("P{} SCAN Parameters Too Constrained !", i + 1);
        // C `npts = <double> + 1`: the conversion truncates. C's conversion of an
        // out-of-range or NaN quotient is undefined (C defect, not copied):
        // `as` saturates, and the `mpts` clamp below takes the result.
        let npts_from = |s: &mut Self, span: f64| {
            s.npts = if s.pos[i].si.abs() <= f64::EPSILON {
                mpts
            } else {
                (span / s.pos[i].si + 1.0) as i32
            };
        };
        macro_rules! set {
            ($f:ident, $off:expr, $v:expr) => {{
                let v = $v;
                self.pos[i].$f = v;
                self.post(pf(i, $off));
            }};
        }
        macro_rules! g {
            ($f:ident) => {
                self.pos[i].$f
            };
        }

        match spc {
            111 => {
                // Start changed.
                if !fi && !fc && !fw {
                    set!(si, p::SI, (g!(ep) - g!(sp)) / n1(self));
                    set!(cp, p::CP, (g!(ep) + g!(sp)) / 2.0);
                    set!(wd, p::WD, g!(ep) - g!(sp));
                } else if !fpts && !fc && !fw {
                    npts_from(self, g!(ep) - g!(sp));
                    if self.npts > mpts {
                        self.npts = mpts;
                        self.smsg = msg(&format!("P{} Request Exceeded Maximum Points!", i + 1));
                        set!(sp, p::SP, g!(ep) - g!(si) * f64::from(self.npts - 1));
                        self.alrt = 1;
                    }
                    self.post(NPTS);
                    set!(cp, p::CP, (g!(ep) + g!(sp)) / 2.0);
                    set!(wd, p::WD, g!(ep) - g!(sp));
                    self.npts_cause = i as i8;
                    self.changed_npts();
                } else if !fe && !fc {
                    set!(ep, p::EP, g!(sp) + n1(self) * g!(si));
                    set!(cp, p::CP, (g!(ep) + g!(sp)) / 2.0);
                } else if !fi && !fe && !fw {
                    set!(wd, p::WD, (g!(cp) - g!(sp)) * 2.0);
                    set!(si, p::SI, g!(wd) / n1(self));
                    set!(ep, p::EP, g!(sp) + g!(wd));
                } else if !fpts && !fe && !fw {
                    npts_from(self, (g!(cp) - g!(sp)) * 2.0);
                    if self.npts > mpts {
                        self.npts = mpts;
                        self.smsg = msg(&format!("P{} Request Exceeded Maximum Points!", i + 1));
                        set!(sp, p::SP, g!(cp) - (g!(si) * n1(self)) / 2.0);
                        self.alrt = 1;
                    }
                    self.post(NPTS);
                    // C posts CP here, which this branch never changes, and
                    // leaves EP's new value unposted.
                    set!(ep, p::EP, g!(sp) + n1(self) * g!(si));
                    set!(wd, p::WD, g!(ep) - g!(sp));
                    self.npts_cause = i as i8;
                    self.changed_npts();
                }
            }
            112 => {
                // Increment changed.
                if !fpts {
                    if g!(si).abs() <= f64::EPSILON {
                        self.npts = mpts;
                    } else {
                        if (g!(si) < 0.0) != (g!(ep) < g!(sp)) {
                            if !fe && !fs && !fw {
                                let t = g!(ep);
                                set!(ep, p::EP, g!(sp));
                                set!(sp, p::SP, t);
                                set!(wd, p::WD, g!(ep) - g!(sp));
                            } else {
                                set!(si, p::SI, (g!(ep) - g!(sp)) / n1(self));
                            }
                        }
                        // Saturates where C's conversion is undefined (see `npts_from`).
                        self.npts = ((g!(ep) - g!(sp)) / g!(si) + 1.0) as i32;
                    }
                    if self.npts > mpts {
                        self.npts = mpts;
                        self.smsg = msg(&exceeded);
                        set!(si, p::SI, (g!(ep) - g!(sp)) / n1(self));
                        self.alrt = 1;
                    }
                    self.post(NPTS);
                    self.npts_cause = i as i8;
                    self.changed_npts();
                } else if !fe && !fc && !fw {
                    set!(ep, p::EP, g!(sp) + n1(self) * g!(si));
                    set!(cp, p::CP, (g!(ep) + g!(sp)) / 2.0);
                    set!(wd, p::WD, g!(ep) - g!(sp));
                } else if !fs && !fc && !fw {
                    set!(sp, p::SP, g!(ep) - n1(self) * g!(si));
                    set!(cp, p::CP, (g!(ep) + g!(sp)) / 2.0);
                    set!(wd, p::WD, g!(ep) - g!(sp));
                } else if !fs && !fe && !fw {
                    set!(sp, p::SP, g!(cp) - (n1(self) * g!(si)) / 2.0);
                    set!(ep, p::EP, g!(sp) + n1(self) * g!(si));
                    set!(wd, p::WD, g!(ep) - g!(sp));
                } else {
                    set!(si, p::SI, (g!(ep) - g!(sp)) / n1(self));
                    self.smsg = msg(&constrained);
                    self.alrt = 1;
                }
            }
            113 => {
                // End changed.
                if !fi && !fc && !fw {
                    set!(si, p::SI, (g!(ep) - g!(sp)) / n1(self));
                    set!(cp, p::CP, (g!(ep) + g!(sp)) / 2.0);
                    set!(wd, p::WD, g!(ep) - g!(sp));
                } else if !fpts && !fc && !fw {
                    npts_from(self, g!(ep) - g!(sp));
                    if self.npts > mpts {
                        self.npts = mpts;
                        self.smsg = msg(&exceeded);
                        set!(ep, p::EP, g!(sp) + g!(si) * n1(self));
                        self.alrt = 1;
                    }
                    self.post(NPTS);
                    set!(cp, p::CP, (g!(ep) + g!(sp)) / 2.0);
                    set!(wd, p::WD, g!(ep) - g!(sp));
                    self.npts_cause = i as i8;
                    self.changed_npts();
                } else if !fs && !fc {
                    set!(sp, p::SP, g!(ep) - n1(self) * g!(si));
                    set!(cp, p::CP, (g!(ep) + g!(sp)) / 2.0);
                } else if !fs && !fw && !fi {
                    set!(wd, p::WD, (g!(ep) - g!(cp)) * 2.0);
                    set!(sp, p::SP, g!(ep) - g!(wd));
                    set!(si, p::SI, (g!(ep) - g!(sp)) / n1(self));
                } else if !fpts && !fs && !fw {
                    npts_from(self, (g!(ep) - g!(cp)) * 2.0);
                    if self.npts > mpts {
                        self.npts = mpts;
                        self.smsg = msg(&exceeded);
                        set!(ep, p::EP, g!(cp) + g!(si) * n1(self) / 2.0);
                        self.alrt = 1;
                    }
                    self.post(NPTS);
                    set!(wd, p::WD, (g!(ep) - g!(cp)) * 2.0);
                    set!(sp, p::SP, g!(ep) - g!(wd));
                    self.npts_cause = i as i8;
                    self.changed_npts();
                } else {
                    set!(ep, p::EP, g!(sp) + n1(self) * g!(si));
                    self.smsg = msg(&constrained);
                    self.alrt = 1;
                }
            }
            114 => {
                // Center changed.
                if !fs && !fe {
                    set!(sp, p::SP, g!(cp) - (n1(self) * g!(si)) / 2.0);
                    set!(ep, p::EP, g!(sp) + n1(self) * g!(si));
                } else if !fe && !fi && !fw {
                    set!(wd, p::WD, (g!(cp) - g!(sp)) * 2.0);
                    set!(ep, p::EP, g!(sp) + g!(wd));
                    set!(si, p::SI, (g!(ep) - g!(sp)) / n1(self));
                } else if !fs && !fi && !fw {
                    set!(wd, p::WD, (g!(ep) - g!(cp)) * 2.0);
                    set!(sp, p::SP, g!(ep) - g!(wd));
                    set!(si, p::SI, (g!(ep) - g!(sp)) / n1(self));
                } else if !fpts && !fe && !fw {
                    npts_from(self, (g!(cp) - g!(sp)) * 2.0);
                    if self.npts > mpts {
                        self.npts = mpts;
                        self.smsg = msg(&exceeded);
                        set!(cp, p::CP, g!(sp) + g!(si) * n1(self) / 2.0);
                        self.alrt = 1;
                    }
                    self.post(NPTS);
                    set!(wd, p::WD, (g!(cp) - g!(sp)) * 2.0);
                    set!(ep, p::EP, g!(sp) + g!(wd));
                    self.npts_cause = i as i8;
                    self.changed_npts();
                } else if !fpts && !fs && !fw {
                    npts_from(self, (g!(ep) - g!(cp)) * 2.0);
                    if self.npts > mpts {
                        self.npts = mpts;
                        self.smsg = msg(&exceeded);
                        set!(cp, p::CP, g!(ep) - g!(si) * n1(self) / 2.0);
                        self.alrt = 1;
                    }
                    self.post(NPTS);
                    set!(wd, p::WD, (g!(ep) - g!(cp)) * 2.0);
                    set!(sp, p::SP, g!(ep) - g!(wd));
                    self.npts_cause = i as i8;
                    self.changed_npts();
                } else {
                    set!(cp, p::CP, g!(sp) + (n1(self) * g!(si)) / 2.0);
                    self.smsg = msg(&constrained);
                    self.alrt = 1;
                }
            }
            115 => {
                // Width changed.
                if !fs && !fi && !fe {
                    set!(si, p::SI, g!(wd) / n1(self));
                    set!(sp, p::SP, g!(cp) - (n1(self) * g!(si)) / 2.0);
                    set!(ep, p::EP, g!(sp) + n1(self) * g!(si));
                } else if !fpts && !fs && !fe {
                    npts_from(self, g!(wd));
                    if self.npts > mpts {
                        self.npts = mpts;
                        self.smsg = msg(&exceeded);
                        set!(wd, p::WD, g!(si) * n1(self));
                        self.alrt = 1;
                    }
                    self.post(NPTS);
                    set!(sp, p::SP, g!(cp) - (n1(self) * g!(si)) / 2.0);
                    set!(ep, p::EP, g!(sp) + n1(self) * g!(si));
                    self.npts_cause = i as i8;
                    self.changed_npts();
                } else if !fc && !fe && !fi {
                    set!(si, p::SI, g!(wd) / n1(self));
                    set!(ep, p::EP, g!(sp) + n1(self) * g!(si));
                    set!(cp, p::CP, (g!(ep) + g!(sp)) / 2.0);
                } else if !fs && !fc && !fi {
                    set!(si, p::SI, g!(wd) / n1(self));
                    set!(sp, p::SP, g!(ep) - n1(self) * g!(si));
                    set!(cp, p::CP, (g!(ep) + g!(sp)) / 2.0);
                } else if !fpts && !fc && !fe {
                    npts_from(self, g!(wd));
                    if self.npts > mpts {
                        self.npts = mpts;
                        self.smsg = msg(&exceeded);
                        set!(wd, p::WD, g!(si) * n1(self));
                        self.alrt = 1;
                    }
                    self.post(NPTS);
                    set!(ep, p::EP, g!(sp) + n1(self) * g!(si));
                    set!(cp, p::CP, (g!(ep) + g!(sp)) / 2.0);
                    self.npts_cause = i as i8;
                    self.changed_npts();
                } else if !fpts && !fs && !fc {
                    npts_from(self, g!(wd));
                    if self.npts > mpts {
                        self.npts = mpts;
                        self.smsg = msg(&exceeded);
                        set!(wd, p::WD, g!(si) * n1(self));
                        self.alrt = 1;
                    }
                    self.post(NPTS);
                    set!(sp, p::SP, g!(ep) - n1(self) * g!(si));
                    set!(cp, p::CP, (g!(ep) + g!(sp)) / 2.0);
                    self.npts_cause = i as i8;
                    self.changed_npts();
                } else {
                    set!(wd, p::WD, g!(ep) - g!(sp));
                    self.smsg = msg(&constrained);
                    self.alrt = 1;
                }
            }
            _ => {}
        }
    }

    /// C `changedNpts`: bring every other positioner's linear parameters
    /// into line with a new NPTS.
    pub(crate) fn changed_npts(&mut self) {
        let n1 = f64::from((self.npts - 1).max(1));
        for i in 0..NUM_POS {
            if self.nv[i] == PV_OK && self.pos[i].sm == SM_TABLE {
                if (self.table_pts[i] as i64) < i64::from(self.npts) {
                    self.smsg = msg(&format!("Pts in P{} Table < # of Steps!", i + 1));
                    if self.alrt == 0 {
                        self.alrt = 1;
                    }
                }
                continue;
            }
            if i as i8 == self.npts_cause {
                continue;
            }
            let q = &self.pos[i];
            let state = (q.fs << 4) | (q.fi << 3) | (q.fe << 2) | (q.fc << 1) | q.fw;
            match state {
                0..=7 | 16..=23 => {
                    let q = &mut self.pos[i];
                    q.si = (q.ep - q.sp) / n1;
                    self.post(pf(i, p::SI));
                    let q = &mut self.pos[i];
                    if q.wd != q.ep - q.sp {
                        q.wd = q.ep - q.sp;
                        self.post(pf(i, p::WD));
                    }
                    let q = &mut self.pos[i];
                    if q.cp != (q.ep + q.sp) / 2.0 {
                        q.cp = (q.ep + q.sp) / 2.0;
                        self.post(pf(i, p::CP));
                    }
                }
                8 | 24 => {
                    let q = &mut self.pos[i];
                    q.ep = q.sp + n1 * q.si;
                    q.cp = (q.ep + q.sp) / 2.0;
                    q.wd = q.ep - q.sp;
                    self.post(pf(i, p::EP));
                    self.post(pf(i, p::CP));
                    self.post(pf(i, p::WD));
                }
                12 => {
                    let q = &mut self.pos[i];
                    q.sp = q.ep - n1 * q.si;
                    q.cp = (q.ep + q.sp) / 2.0;
                    q.wd = q.ep - q.sp;
                    self.post(pf(i, p::SP));
                    self.post(pf(i, p::CP));
                    self.post(pf(i, p::WD));
                }
                10 => {
                    let q = &mut self.pos[i];
                    q.sp = q.cp - (n1 * q.si) / 2.0;
                    q.ep = q.sp + n1 * q.si;
                    q.wd = q.ep - q.sp;
                    self.post(pf(i, p::SP));
                    self.post(pf(i, p::EP));
                    self.post(pf(i, p::WD));
                }
                _ => {
                    self.smsg = msg(&format!("P{} SCAN Parameters Too Constrained !", i + 1));
                    self.alrt = 1;
                }
            }
        }
    }

    /// Positioner `i`'s drive value at point `j` (`checkScanLimits`,
    /// `previewScan`).
    fn planned_position(&self, i: usize, j: usize) -> f64 {
        let q = &self.pos[i];
        let rel = if q.ar != 0 { q.pp } else { 0.0 };
        match q.sm {
            SM_LINEAR => {
                if q.ar != 0 {
                    (q.pp + q.sp) + j as f64 * q.si
                } else {
                    q.sp + j as f64 * q.si
                }
            }
            SM_TABLE => rel + self.pa[i][j],
            SM_FLY => rel + if j == 0 { q.sp } else { q.ep },
            _ => 0.0,
        }
    }

    /// C `checkScanLimits`: refresh each positioner's prior position, then
    /// check every planned point against its control limits.
    pub(crate) fn check_scan_limits(&mut self) -> bool {
        for i in 0..NUM_POS {
            if self.nv[i] != PV_OK {
                continue;
            }
            let got = self.link_get1(i);
            if let Some(v) = got {
                self.pos[i].pp = v;
            }
            self.post(pf(i, p::PP));
            if got.is_none() {
                self.errlog(&format!(
                    "{}:checkScanLimits: could not get current value",
                    self.name
                ));
                self.set_smsg("Can't get current position");
                if self.alrt == 0 {
                    self.set_alrt(1);
                }
                return false;
            }
        }
        if knob(&SSCAN_RECORD_DONT_CHECK_LIMITS) != 0 && self.xsc != 0 {
            return true;
        }
        for i in 0..NUM_POS {
            if self.nv[i] == PV_OK
                && self.pos[i].sm == SM_TABLE
                && (self.table_pts[i] as i64) < i64::from(self.npts)
            {
                self.set_smsg(&format!("Pts in P{} Table < # of Steps", i + 1));
                if self.alrt == 0 {
                    self.set_alrt(1);
                }
                return false;
            }
        }
        let npts = self.npts.max(0) as usize;
        for i in 0..NUM_POS {
            if self.nv[i] != PV_OK {
                continue;
            }
            for j in 0..npts {
                let value = self.planned_position(i, j);
                let (lr, hr) = (self.pos[i].lr, self.pos[i].hr);
                let side = if lr != 0.0 && value < lr {
                    "< LO_Limit"
                } else if hr != 0.0 && value > hr {
                    "> HI_Limit"
                } else {
                    continue;
                };
                self.smsg = msg(&format!("P{} Value {side} @ point {j}", i + 1));
                self.alrt = 1;
                return false;
            }
        }
        self.smsg = msg("SCAN Values within limits");
        true
    }

    /// C `previewScan`: each valid positioner's planned positions into its
    /// detector-`i` array, the step numbers into its read-back array.
    pub(crate) fn preview_scan(&mut self) {
        for i in 0..NUM_POS {
            if self.nv[i] == PV_OK {
                if let Some(v) = self.link_get1(i) {
                    self.pos[i].pp = v;
                }
                self.post(pf(i, p::PP));
            }
        }
        let npts = self.npts.max(0) as usize;
        let mpts = self.mpts_len();
        let valid = self.valid;
        for i in 0..NUM_POS {
            if self.nv[i] != PV_OK {
                continue;
            }
            let values: Vec<f32> = (0..npts)
                .map(|j| self.planned_position(i, j) as f32)
                .collect();
            let pos = &mut self.pos_buf[i][valid];
            for (j, x) in pos[..npts].iter_mut().enumerate() {
                *x = j as f64;
            }
            for j in npts.max(1)..mpts {
                pos[j] = pos[j - 1];
            }
            if let Some(buf) = self.det_buf[i].as_mut() {
                let det = &mut buf[valid];
                det[..npts].copy_from_slice(&values);
                for j in npts.max(1)..mpts {
                    det[j] = det[j - 1];
                }
            }
            self.post(pf(i, p::RA));
            self.post(df(i, d::DA));
            self.post(df(i, d::CA));
            self.post_null_served(EventMask::VALUE);
        }
    }

    /// C `saveFrzFlags`.
    pub(crate) fn save_frz_flags(&mut self) {
        self.frz_fpts = self.fpts;
        for i in 0..NUM_POS {
            let q = &self.pos[i];
            self.frz[i] = [q.fs, q.fi, q.fc, q.fe, q.fw];
        }
    }

    /// C `resetFrzFlags`: clear every freeze flag, posting those that change.
    pub(crate) fn reset_frz_flags(&mut self) {
        if self.fpts != 0 {
            self.fpts = 0;
            self.post(FPTS);
        }
        for i in 0..NUM_POS {
            for off in [p::FS, p::FI, p::FC, p::FE, p::FW] {
                let flag = self.frz_flag(i, off);
                if *flag != 0 {
                    *flag = 0;
                    self.post(pf(i, off));
                }
            }
        }
    }

    /// C `restoreFrzFlags`: posts each flag restored set.
    fn restore_frz_flags(&mut self) {
        self.fpts = self.frz_fpts;
        if self.fpts != 0 {
            self.post(FPTS);
        }
        for i in 0..NUM_POS {
            let saved = self.frz[i];
            for (k, off) in [p::FS, p::FI, p::FC, p::FE, p::FW].into_iter().enumerate() {
                let flag = self.frz_flag(i, off);
                *flag = saved[k];
                if saved[k] != 0 {
                    self.post(pf(i, off));
                }
            }
        }
    }

    fn frz_flag(&mut self, i: usize, off: usize) -> &mut u16 {
        let q = &mut self.pos[i];
        match off {
            p::FS => &mut q.fs,
            p::FI => &mut q.fi,
            p::FC => &mut q.fc,
            p::FE => &mut q.fe,
            _ => &mut q.fw,
        }
    }

    /// C `savePosParms`.
    fn save_pos_parms(&mut self, i: usize) {
        let q = &self.pos[i];
        self.pos_parms[i] = PosParms {
            sp: q.sp,
            si: q.si,
            ep: q.ep,
            cp: q.cp,
            wd: q.wd,
        };
    }

    /// C `zeroPosParms`.
    fn zero_pos_parms(&mut self, i: usize) {
        self.set_pos_parms(i, PosParms::default());
    }

    /// C `restorePosParms`.
    fn restore_pos_parms(&mut self, i: usize) {
        self.set_pos_parms(i, self.pos_parms[i]);
    }

    fn set_pos_parms(&mut self, i: usize, v: PosParms) {
        let q = &mut self.pos[i];
        (q.sp, q.si, q.ep, q.cp, q.wd) = (v.sp, v.si, v.ep, v.cp, v.wd);
        for off in [p::SP, p::SI, p::EP, p::CP, p::WD] {
            self.post(pf(i, off));
        }
    }
}
