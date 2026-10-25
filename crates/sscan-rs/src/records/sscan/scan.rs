//! `process()` and the scan state machine: `initScan`, `contScan`,
//! `endScan`, `readArrays`, `packData`, `afterScan`, `doPuts` and
//! `checkMonitors` (`sscanRecord.c:932-1257`, `:2153-2276`, `:3188-4535`).

use std::time::Instant;

use epics_base_rs::server::record::EventMask;
use epics_base_rs::types::WallTime;

use super::*;
use crate::rec_dyn_link::NOTIFY_IN_PROGRESS;

/// `p_dv *= (1 + DBL_EPSILON)` when a move would not change the drive
/// value: some positioners only move on a change.
fn nudge(dv: &mut f64, old: f64) {
    if *dv == old {
        *dv *= 1.0 + f64::EPSILON;
    }
}

fn secs_since(t: Option<Instant>, now: Instant) -> f64 {
    t.map_or(f64::INFINITY, |t| now.duration_since(t).as_secs_f64())
}

impl SscanRecord {
    pub(crate) fn any_callbacks(&self) -> bool {
        self.num_pos_cb != 0
            || self.num_trig_cb != 0
            || self.num_aread_cb != 0
            || self.num_get_cb != 0
    }

    fn zero_callbacks(&mut self) {
        self.num_pos_cb = 0;
        self.num_trig_cb = 0;
        self.num_aread_cb = 0;
        self.num_get_cb = 0;
    }

    fn clear_waits(&mut self) {
        if self.wait != 0 {
            self.wait = 0;
            self.post(WAIT);
        }
        if self.wcnt != 0 {
            self.wcnt = 0;
            self.post(WCNT);
        }
        if self.wtng != 0 {
            self.wtng = 0;
            self.post(WTNG);
        }
    }

    fn cb_counts(&self) -> String {
        format!(
            "{}_{}_{}_{:02}",
            self.num_pos_cb, self.num_trig_cb, self.num_aread_cb, self.num_get_cb
        )
    }

    /// `recDynLinkGet(.., &v, &nRequest=1, ..)`: `None` leaves C's target
    /// untouched.
    pub(crate) fn link_get1(&mut self, li: usize) -> Option<f64> {
        self.links[li].get(1).and_then(|v| v.first().copied())
    }

    /// Read-back or detector `li`'s value: `dbGet(.., ty, ..)` for a local
    /// PV, else `recDynLinkGet`. `None` leaves C's target untouched.
    fn input_get1(&mut self, own: OwnFrame<'_>, li: usize, ty: DbFieldType) -> Option<f64> {
        if self.lpvt[li].local.is_some() {
            return self.db_get(own, li, ty, 1).and_then(|s| s.value.to_f64());
        }
        self.link_get1(li)
    }

    /// [`Self::input_get1`] of up to `n` elements.
    fn input_get(
        &mut self,
        own: OwnFrame<'_>,
        li: usize,
        ty: DbFieldType,
        n: usize,
    ) -> Option<Vec<f64>> {
        if self.lpvt[li].local.is_none() {
            return self.links[li].get(n).map(<[f64]>::to_vec);
        }
        Some(match self.db_get(own, li, ty, n)?.value {
            EpicsValue::DoubleArray(v) => v.iter().copied().collect(),
            EpicsValue::FloatArray(v) => v.iter().map(|x| f64::from(*x)).collect(),
            v => v.to_f64().into_iter().collect(),
        })
    }

    /// The fly-or-array test C repeats: `p_sm == FLY || acqt == 1D_ARRAY`.
    fn fly_mode(&self, i: usize) -> bool {
        self.pos[i].sm == SM_FLY || self.acqt == ACQT_1D_ARRAY
    }

    /// C `process()`. `true` when it reaches the `recGblFwdLink` /
    /// `recGblResetAlarms` tail; `false` for each early `return`.
    pub(crate) fn process_pass(&mut self, own: OwnFrame<'_>) -> bool {
        self.clamp_npts();

        if self.kill != 0 {
            self.clear_waits();
            if self.num_pos_cb != 0 {
                self.set_smsg("NOTE: positioner still active");
            } else if self.num_trig_cb != 0 {
                self.set_smsg("NOTE: detector still active");
            } else if self.num_aread_cb != 0 {
                self.set_smsg("NOTE: array-read still active");
            } else if self.num_get_cb != 0 {
                self.set_smsg("NOTE: outstanding getCallback(s)");
            }
            self.set_alrt(0);
            self.zero_callbacks();
            if self.dstate <= dstate::PACKED {
                if self.await_ != 0 && !self.user_set_await {
                    self.await_ = 0;
                    self.post(AWAIT);
                }
                if self.dstate == dstate::ARRAY_READ_WAIT {
                    self.set_dstate(dstate::SAVE_DATA_WAIT);
                } else if self.dstate == dstate::SAVE_DATA_WAIT && self.kill > 2 {
                    self.set_dstate(dstate::PACKED);
                    self.await_ = 0;
                    self.post(AWAIT);
                    self.set_smsg("Abandoning unsaved scan data");
                    self.errlog(&format!(
                        "{}:process(): Abandoning unsaved scan data",
                        self.name
                    ));
                }
                self.pack_data(own);
                self.check_monitors();
            }
            if self.dstate == dstate::SAVE_DATA_WAIT {
                return false;
            }
            self.busy = 0;
            self.post(BUSY);
            self.set_faze(faze::IDLE);
            self.fire_fwd = true;
            self.pxsc = self.xsc as u8;
            self.kill = 0;
            return true;
        }

        if self.xsc != 0 {
            if self.paus != 0 {
                self.set_smsg("Scan is paused ...");
                return false;
            }
            if self.wtng != 0 {
                self.set_smsg("waiting for client ...");
                return false;
            }
        }

        if self.scan_by_search {
            self.scan_by_search = false;
            if self.faze != faze::SCAN_PENDING {
                self.errlog(&format!(
                    "{}:process: Ignoring redundant scanBySearchCallback",
                    self.name
                ));
                return false;
            }
        }

        if self.faze == faze::SCAN_PENDING {
            self.check_connections();
            if self.xsc == 0 || self.bad_output || self.bad_input {
                return false;
            }
            self.set_alrt(0);
        }

        if self.busy != 0 && self.xsc != 0 && self.any_callbacks() {
            if self.paus != 0 {
                self.set_smsg("Scan is paused");
            } else {
                self.set_smsg(&format!(
                    "Already busy! PTAG_CBs={}; CB=0x{:x}",
                    self.cb_counts(),
                    self.called_by
                ));
            }
            return false;
        }

        if self.pxsc == 0 && self.xsc == 1 {
            // Brand new scan.
            if self.busy != 0 {
                // C writes this message and does not post it.
                self.smsg = msg(&format!(
                    "Still busy! PTAG_CBs={}; CB=0x{:x}",
                    self.cb_counts(),
                    self.called_by
                ));
                return false;
            }
            // `recGblGetTimeStamp`: TIME marks the scan's start.
            self.stamp = true;
            self.set_dstate(dstate::UNPACKED);
            self.data = 0;
            self.post_log(DATA);
            self.clear_waits();
            self.zero_callbacks();
            self.set_faze(faze::INIT_SCAN);
            self.busy = 1;
            self.post(BUSY);
            self.init_scan();
        } else if self.pxsc == 1 && self.xsc == 0 {
            // Operator abort.
            if self.any_callbacks() {
                if self.dstate < dstate::PACKED {
                    self.pack_data(own);
                    self.check_monitors();
                }
                self.set_smsg("Abort: waiting for callback(s)");
                return false;
            }
            if self.smsg.as_bytes().is_empty() {
                self.set_smsg("Scan aborted by operator");
            }
            self.clear_waits();
            self.errlog(&format!("{}:process: Scan aborted by operator", self.name));
            self.end_scan(own);
        } else if self.faze == faze::BEFORE_SCAN_WAIT {
            self.init_scan();
        } else if self.xsc == 1 {
            if self.bad_output {
                self.set_alrt(1);
                self.set_smsg("Lost connection to Control PV");
                self.exsc = 0;
                self.post(EXSC);
                self.xsc = 0;
                self.post(XSC);
                self.errlog(&format!(
                    "{}:process: Lost connection to Control PV",
                    self.name
                ));
                self.end_scan(own);
            } else if matches!(
                self.dstate,
                dstate::SAVE_DATA_WAIT | dstate::ARRAY_READ_WAIT | dstate::RECORD_ARRAY_DATA
            ) {
                self.end_scan(own);
            } else {
                self.cont_scan(own);
            }
        } else if self.busy != 0 {
            if self.dstate < dstate::PACKED {
                self.pack_data(own);
                if self.dstate < dstate::PACKED {
                    self.set_smsg("waiting for packData");
                    return false;
                }
            }
            match self.faze {
                faze::RETRACE_WAIT => return false,
                faze::AFTER_SCAN_DO => self.after_scan(),
                faze::AFTER_SCAN_WAIT | faze::SCAN_DONE => self.set_faze(faze::SCAN_DONE),
                faze::READ_DETCTRS | faze::CHECK_MOTORS if self.dstate == dstate::PACKED => {
                    self.end_scan(own)
                }
                _ => self.errlog(&format!(
                    "{}:process: How did I get here? (faze='{}', dstate='{}', calledBy = {})",
                    self.name,
                    faze_name(self.faze),
                    dstate_name(self.dstate),
                    called_by_name(self.called_by)
                )),
            }
        }
        self.check_monitors();

        if self.busy != 0 && self.faze == faze::SCAN_DONE && self.dstate == dstate::POSTED {
            self.busy = 0;
            self.post(BUSY);
            self.set_faze(faze::IDLE);
            self.set_smsg("SCAN Complete");
            self.fire_fwd = true;
        }
        self.pxsc = self.xsc as u8;
        true
    }

    /// C `initScan`.
    pub(crate) fn init_scan(&mut self) {
        self.cpt = 0;
        self.scan_err = 0;
        let ok = |s: &Self, k: usize| s.nv[k] == PV_OK;
        self.val_pos = (0..NUM_POS)
            .rev()
            .find(|&i| ok(self, i))
            .map_or(0, |i| i + 1);
        self.val_rdbk = (0..NUM_POS)
            .rev()
            .find(|&i| ok(self, R1_IN + i))
            .map_or(0, |i| i + 1);
        self.val_det = 0;
        for i in 0..NUM_DET {
            self.acq[i] = ok(self, D1_IN + i);
            if self.acq[i] {
                self.val_det = i + 1;
            }
        }
        self.val_trig = (0..NUM_TRGS)
            .rev()
            .find(|&i| ok(self, T1_OUT + i))
            .map_or(0, |i| i + 1);
        self.val_atrig = usize::from(ok(self, A1_OUT));

        if !self.check_scan_limits() {
            self.xsc = 0;
            self.post(XSC);
            self.exsc = 0;
            self.post(EXSC);
            self.post(SMSG);
            self.post(ALRT);
            return;
        }

        self.have_fly = false;
        self.flying = false;
        for i in 0..self.val_pos {
            if self.nv[i] != PV_OK {
                continue;
            }
            let q = &mut self.pos[i];
            q.dv = if q.sm == SM_TABLE {
                self.pa[i][0]
            } else {
                q.sp
            };
            if q.ar != 0 {
                q.dv += q.pp;
            }
            let pp = q.pp;
            nudge(&mut q.dv, pp);
            self.post(pf(i, p::DV));
            if self.fly_mode(i) {
                self.have_fly = true;
            }
        }

        if self.nv[BS_OUT] == PV_OK && self.faze == faze::INIT_SCAN {
            self.set_faze(faze::BEFORE_SCAN);
            self.set_smsg("Before Scan FLNK ...");
        } else {
            self.set_faze(faze::MOVE_MOTORS);
            self.set_smsg("Scanning ...");
        }
        self.request_do_puts();
    }

    /// C `contScan`.
    fn cont_scan(&mut self, own: OwnFrame<'_>) {
        let mut stage = self.faze;
        if stage == faze::TRIG_DETCTRS {
            self.request_do_puts();
            return;
        }
        if stage == faze::CHECK_MOTORS {
            for i in 0..NUM_POS {
                if self.pos[i].r_dl == 0.0 || self.nv[R1_IN + i] != PV_OK || self.nv[i] != PV_OK {
                    continue;
                }
                if let Some(v) = self.input_get1(own, R1_IN + i, DbFieldType::Double) {
                    self.pos[i].r_cv = v;
                }
                let q = &self.pos[i];
                let err = (q.dv - q.r_cv).abs();
                let reason = if q.r_dl > 0.0 && err > q.r_dl {
                    "delta"
                } else if q.r_dl < 0.0 && q.sm != SM_TABLE && err > (q.si * nint(q.r_dl)).abs() {
                    "stepsize"
                } else {
                    continue;
                };
                self.set_smsg(&format!("SCAN Aborted: P{} Error > {reason}", i + 1));
                self.scan_err = 1;
                self.errlog(&format!(
                    "{}: P{} Error > {reason}.  Ending scan.",
                    self.name,
                    i + 1
                ));
                self.end_scan(own);
                return;
            }

            if self.have_fly && !self.flying {
                for i in 0..self.val_pos {
                    if self.nv[i] == PV_OK && self.fly_mode(i) {
                        let npts = self.npts.max(1) as usize;
                        let q = &mut self.pos[i];
                        let old = q.dv;
                        let end = if q.sm == SM_TABLE {
                            self.pa[i][npts - 1]
                        } else {
                            q.ep
                        };
                        q.dv = if q.ar != 0 { q.pp + end } else { end };
                        nudge(&mut q.dv, old);
                    }
                }
            }

            if self.val_trig != 0 || (self.have_fly && self.cpt == 0) {
                self.set_faze(faze::TRIG_DETCTRS);
                self.request_do_puts();
                return;
            }
            self.set_faze(faze::READ_DETCTRS);
            stage = faze::READ_DETCTRS;
        }
        if stage == faze::READ_DETCTRS {
            // The preset keeps a callback from reaching zero before every
            // get is queued.
            self.num_get_cb = 1;
            // A local PV is read by `dbGet` when its data are recorded.
            for i in 0..NUM_POS {
                let li = R1_IN + i;
                if self.nv[li] == PV_OK && self.lpvt[li].local.is_none() {
                    self.get_callback(li, 1);
                }
            }
            for i in 0..self.val_det {
                let li = D1_IN + i;
                if self.acq[i]
                    && self.det_buf[i].is_some()
                    && self.nv[li] == PV_OK
                    && self.lpvt[li].local.is_none()
                {
                    self.get_callback(li, 1);
                }
            }
            self.set_faze(faze::RECORD_SCALAR_DATA);
            self.num_get_cb -= 1;
            if self.num_get_cb != 0 {
                return;
            }
            stage = faze::RECORD_SCALAR_DATA;
        }
        if stage == faze::RECORD_SCALAR_DATA {
            self.record_scalar_data(own);
        }
    }

    /// `numGetCallbacks++` then `recDynLinkGetCallback(.., userGetCallback)`.
    fn get_callback(&mut self, li: usize, n: usize) {
        self.num_get_cb += 1;
        let mut n = n;
        // C ignores the status, so a get that was never queued keeps the
        // count raised and the scan waits for a callback that cannot come.
        if self.links[li].get_callback(&mut n, Cb::UserGet) != 0 {
            self.num_get_cb -= 1;
        }
    }

    /// `contScan`'s `sscanFAZE_RECORD_SCALAR_DATA` case.
    fn record_scalar_data(&mut self, own: OwnFrame<'_>) {
        let cpt = self.cpt.max(0) as usize;
        let fill = self.fill();
        for i in 0..NUM_POS {
            if self.nv[R1_IN + i] == PV_OK {
                if let Some(v) = self.input_get1(own, R1_IN + i, DbFieldType::Double) {
                    self.pos[i].r_cv = v;
                }
            } else if self.lpvt[R1_IN + i].ts {
                let elapsed = WallTime::now().since_unix_epoch().as_secs_f64()
                    - self.time.since_unix_epoch().as_secs_f64();
                self.pos[i].r_cv = elapsed;
            } else if self.nv[i] == PV_OK {
                let q = &self.pos[i];
                self.pos[i].r_cv = if !self.fly_mode(i) || !self.flying {
                    q.dv
                } else if q.sm != SM_TABLE {
                    // C reads `pFill[cpt - 1]`, before the buffer on the
                    // first point (`flying` is already set when it is
                    // recorded); the first point is the start position.
                    match cpt.checked_sub(1) {
                        Some(prev) => self.pos_buf[i][fill][prev] + q.si,
                        None => q.sp + if q.ar != 0 { q.pp } else { 0.0 },
                    }
                } else {
                    self.pa[i][cpt]
                };
            } else {
                self.pos[i].r_cv = f64::from(self.cpt);
            }
            if let Some(slot) = self.pos_buf[i][fill].get_mut(cpt) {
                *slot = self.pos[i].r_cv;
            }
        }

        let add = self.prev_num_pts > self.cpt
            && (self.acqm == ACQM_ADD || (self.acqm == ACQM_ACC && self.prev_acqm == ACQM_ACC));
        for i in 0..self.val_det {
            if !self.acq[i] || self.det_buf[i].is_none() {
                continue;
            }
            let mut cv = if self.nv[D1_IN + i] == PV_OK {
                self.input_get1(own, D1_IN + i, DbFieldType::Float)
                    .map_or(self.det[i].cv, |v| v as f32)
            } else {
                0.0
            };
            let valid = self.valid;
            let buf = self.det_buf[i].as_mut().expect("checked above");
            if add {
                cv += buf[valid][cpt];
            }
            if let Some(slot) = buf[fill].get_mut(cpt) {
                *slot = cv;
            }
            self.det[i].cv = cv;
        }

        if self.copyto != 0 && self.atime >= 0.1 {
            self.copy_last_point(self.cpt, i64::from(self.copyto));
        }
        self.udf_clear = true;
        if self.acqt == ACQT_1D_ARRAY {
            self.cpt = self.npts;
        } else {
            self.cpt += 1;
        }

        if self.cpt < self.npts {
            let cpt = self.cpt as usize;
            for i in 0..self.val_pos {
                if self.nv[i] != PV_OK || self.pos[i].sm == SM_FLY || self.acqt == ACQT_1D_ARRAY {
                    continue;
                }
                let q = &mut self.pos[i];
                let old = q.dv;
                if q.sm == SM_LINEAR {
                    q.dv += q.si;
                } else {
                    q.dv = self.pa[i][cpt];
                    if q.ar != 0 {
                        q.dv += q.pp;
                    }
                }
                nudge(&mut q.dv, old);
            }
            self.set_faze(faze::MOVE_MOTORS);
            self.request_do_puts();
        } else {
            self.end_scan(own);
            self.scan_err = 0;
        }
    }

    /// C `endScan`.
    pub(crate) fn end_scan(&mut self, own: OwnFrame<'_>) {
        if self.dstate < dstate::PACKED {
            self.pack_data(own);
        }
        if self.dstate < dstate::PACKED {
            return;
        }
        self.xsc = 0;
        self.last_scan_end = Instant::now();
        if self.pasm != 0 && self.val_pos != 0 {
            self.set_faze(faze::RETRACE_MOVE);
            self.request_do_puts();
        } else {
            self.after_scan();
        }
    }

    /// C `readArrays`.
    fn read_arrays(&mut self, own: OwnFrame<'_>) {
        let npts = self.npts.max(0) as usize;
        if self.dstate == dstate::ARRAY_READ_WAIT {
            for i in 0..NUM_POS {
                let li = R1_IN + i;
                if self.lpvt[li].nelem > 1 && self.nv[li] == PV_OK && self.lpvt[li].local.is_none()
                {
                    self.queue_array_get(li, npts);
                }
            }
            for i in 0..self.val_det {
                let li = D1_IN + i;
                if self.acq[i]
                    && self.det_buf[i].is_some()
                    && self.lpvt[li].nelem > 1
                    && self.nv[li] == PV_OK
                    && self.lpvt[li].local.is_none()
                {
                    self.queue_array_get(li, npts);
                }
            }
            if self.num_get_cb != 0 {
                self.dstate = dstate::ARRAY_GET_CALLBACK_WAIT;
                return;
            }
            self.dstate = dstate::RECORD_ARRAY_DATA;
        }

        let fill = self.fill();
        for i in 0..NUM_POS {
            let li = R1_IN + i;
            let (sp, si) = (self.pos[i].sp, self.pos[i].si);
            if self.lpvt[li].nelem > 1 {
                if self.nv[li] == PV_OK {
                    if let Some(v) = self.input_get(own, li, DbFieldType::Double, npts) {
                        let n = v.len();
                        let buf = &mut self.pos_buf[i][fill];
                        buf[..n].copy_from_slice(&v);
                        buf[n..npts].fill(0.0);
                    }
                } else {
                    for (j, x) in self.pos_buf[i][fill][..npts].iter_mut().enumerate() {
                        *x = sp + j as f64 * si;
                    }
                }
            } else if self.acqt == ACQT_1D_ARRAY {
                for (j, x) in self.pos_buf[i][fill][..npts].iter_mut().enumerate() {
                    *x = sp + j as f64 * si;
                }
            }
        }

        let add = self.acqm == ACQM_ADD || (self.acqm == ACQM_ACC && self.prev_acqm == ACQM_ACC);
        let valid = self.valid;
        for i in 0..self.val_det {
            let li = D1_IN + i;
            if !self.acq[i] || self.det_buf[i].is_none() || self.lpvt[li].nelem <= 1 {
                continue;
            }
            // C reads into `dataBuffer` when adding to the previous scan.
            let mut read = if add {
                std::mem::take(&mut self.data_buffer)
            } else {
                self.det_buf[i].as_ref().expect("checked")[fill][..npts].to_vec()
            };
            if self.nv[li] == PV_OK {
                if let Some(v) = self.input_get(own, li, DbFieldType::Float, npts) {
                    for (dst, src) in read.iter_mut().zip(&v) {
                        *dst = *src as f32;
                    }
                    read[v.len()..npts].fill(0.0);
                }
            } else {
                read[..npts].fill(0.0);
            }
            let buf = self.det_buf[i].as_mut().expect("checked");
            if add {
                for j in 0..npts {
                    buf[fill][j] = read[j] + buf[valid][j];
                }
                self.data_buffer = read;
            } else {
                buf[fill][..npts].copy_from_slice(&read);
            }
        }

        self.udf_clear = true;
        if self.acqt == ACQT_1D_ARRAY {
            self.cpt = self.npts;
        }
    }

    /// `readArrays`' `recDynLinkGetCallback` of `npts` elements.
    fn queue_array_get(&mut self, li: usize, npts: usize) {
        let mut n = npts;
        // C counts the get when the queueing FAILED (`if (status)
        // numGetCallbacks++`), so a queued get is not waited for and a failed
        // one is waited for forever; counted on success here.
        if self.links[li].get_callback(&mut n, Cb::UserGet) == 0 {
            self.num_get_cb += 1;
        }
    }

    /// C `copyLastPoint`: repeat point `pn` to `copy_to` (-1: MPTS).
    pub(crate) fn copy_last_point(&mut self, pn: i32, copy_to: i64) {
        let mpts = i64::from(self.mpts);
        let copy_to = if copy_to == -1 {
            mpts
        } else {
            copy_to.min(mpts)
        }
        .max(0) as usize;
        let pn = pn.max(0) as usize;
        let fill = self.fill();
        for i in 0..self.val_det {
            if !self.acq[i] {
                continue;
            }
            if let Some(buf) = self.det_buf[i].as_mut() {
                let b = &mut buf[fill];
                if pn < b.len() {
                    let v = b[pn];
                    for x in b.iter_mut().take(copy_to).skip(pn + 1) {
                        *x = v;
                    }
                }
            }
        }
        let mut i = 0;
        while i == 0 || i < self.val_pos {
            let b = &mut self.pos_buf[i][fill];
            if pn < b.len() {
                let v = b[pn];
                for x in b.iter_mut().take(copy_to).skip(pn + 1) {
                    *x = v;
                }
            }
            i += 1;
        }
    }

    /// C `packData`.
    pub(crate) fn pack_data(&mut self, own: OwnFrame<'_>) {
        if self.dstate == dstate::PACKED {
            return;
        }
        match self.dstate {
            dstate::UNPACKED | dstate::ARRAY_READ_WAIT | dstate::RECORD_ARRAY_DATA => {
                if self.dstate == dstate::UNPACKED {
                    if self.val_atrig != 0 {
                        self.set_dstate(dstate::TRIG_ARRAY_READ);
                        self.request_do_puts();
                        return;
                    }
                    self.dstate = dstate::ARRAY_READ_WAIT;
                }
                self.read_arrays(own);
                if self.dstate == dstate::ARRAY_GET_CALLBACK_WAIT {
                    return;
                }
                if self.await_ != 0 {
                    self.set_dstate(dstate::SAVE_DATA_WAIT);
                    return;
                }
            }
            dstate::SAVE_DATA_WAIT => {
                if self.await_ != 0 {
                    return;
                }
            }
            _ => self.errlog(&format!(
                "{}:packData: unexpected dstate ({})",
                self.name, self.dstate
            )),
        }

        self.set_dstate(dstate::PACKED);
        self.copy_last_point(self.cpt - 1, -1);

        let mut move_to_ref = false;
        if self.pasm >= pasm::PEAK {
            let pos_ok = (0..NUM_POS).any(|i| self.nv[i] == PV_OK);
            // Only a put of REFD is clamped (special); a loaded REFD outside
            // 1..=NUM_DET indexes `acqDet` out of bounds in C (C defect, not copied).
            let refd = (self.refd.clamp(1, NUM_DET as i16) - 1) as usize;
            move_to_ref = pos_ok && self.cpt > 1 && self.acq[refd];
            if !move_to_ref {
                self.set_smsg(&format!("Can't move to {} ", pasm_name(self.pasm)));
                self.set_alrt(1);
            }
        }
        if move_to_ref {
            self.move_to_ref();
        }

        self.valid = 1 - self.valid;
        self.prev_acqm = self.acqm;
        self.prev_num_pts = self.cpt;
    }

    /// `packData`'s after-scan search for a feature of the reference
    /// detector's data (`sscanRecord.c:3940-4090`).
    fn move_to_ref(&mut self) {
        let cpt = self.cpt.max(0) as usize;
        let fill = self.fill();
        let refd = (self.refd.clamp(1, NUM_DET as i16) - 1) as usize;
        // The caller's `pos_ok && acq[refd]` (C `moveToRef`, sscanRecord.c:3930):
        // a detector is PV_OK only after its connect allocated its buffers.
        let first = (0..NUM_POS)
            .find(|&i| self.nv[i] == PV_OK)
            .expect("move_to_ref runs with a valid positioner");
        let ref_buf = self.det_buf[refd]
            .as_ref()
            .expect("an acquired detector has buffers");
        // C smooths in `nullArray`/`nullArray2` and zeroes them after; the
        // port uses its own scratch so the null array stays zero.
        let mut pf: Vec<f32> = ref_buf[fill][..cpt].to_vec();
        let smooth = knob(&SSCAN_FIT_SMOOTH);
        if smooth != 0 {
            for _ in 0..=smooth {
                let prev = pf.clone();
                for i in 1..cpt.saturating_sub(1) {
                    pf[i] = prev[i - 1] / 4.0 + prev[i] / 2.0 + prev[i + 1] / 4.0;
                }
            }
        }
        let test_fit = knob(&SSCAN_TEST_FIT) != 0;
        if test_fit {
            self.test_fit_write(1, &pf);
        }

        let mut found = false;
        let mut ddata = pf;
        match self.pasm {
            pasm::RISING_EDGE | pasm::FALLING_EDGE | pasm::PEAK | pasm::VALLEY => {
                if matches!(self.pasm, pasm::RISING_EDGE | pasm::FALLING_EDGE) {
                    let pbuf = &self.pos_buf[first][fill];
                    let mut deriv = ddata.clone();
                    let mut i = 1;
                    while i + 1 < cpt {
                        let mut d = pbuf[i + 1] - pbuf[i - 1];
                        if d.abs() < 1.0e-6 {
                            d = if d < 0.0 { -1.0e-6 } else { 1.0e-6 };
                        }
                        deriv[i] = (f64::from(ddata[i + 1] - ddata[i - 1]) / d) as f32;
                        i += 1;
                    }
                    deriv[i] = deriv[i - 1];
                    deriv[0] = deriv[1];
                    ddata = deriv;
                    if test_fit {
                        self.test_fit_write(2, &ddata);
                    }
                }
                let mut ave_diff = 0.0f64;
                let (mut high, mut low) = (f64::NEG_INFINITY, f64::INFINITY);
                let (mut hi_ix, mut lo_ix) = (0usize, 0usize);
                for i in 1..cpt {
                    let a = f64::from(ddata[i]);
                    ave_diff += f64::from(ddata[i] - ddata[i - 1]).abs();
                    if a > high {
                        high = a;
                        hi_ix = i;
                    }
                    if a < low {
                        low = a;
                        lo_ix = i;
                    }
                }
                ave_diff /= (cpt as f64 - 1.0).max(1.0);
                let mut mark = None;
                if high - low > 2.0 * ave_diff {
                    mark = Some(if matches!(self.pasm, pasm::PEAK | pasm::RISING_EDGE) {
                        hi_ix
                    } else {
                        lo_ix
                    });
                }
                found = mark.is_some_and(|m| m < cpt);
                for i in 0..self.val_pos {
                    self.pos[i].dv = match mark {
                        Some(m) if found => self.pos_buf[i][fill][m],
                        _ => self.pos[i].pp,
                    };
                }
            }
            pasm::COM => {
                // C nests two loops over `i` here, so after the first valid
                // positioner the outer loop has run off its end and `pPos`
                // is advanced past the positioner whose readbacks it used;
                // each valid positioner gets its own centre of mass instead.
                for j in 0..NUM_POS {
                    if self.nv[j] != PV_OK {
                        continue;
                    }
                    let pbuf = &self.pos_buf[j][fill];
                    let (mut sum1, mut sum2) = (0.0f64, 0.0f64);
                    for i in 1..cpt {
                        let mut d = pbuf[i] - pbuf[i - 1];
                        if d.abs() < 1.0e-6 {
                            d = if d < 0.0 { -1.0e-6 } else { 1.0e-6 };
                        }
                        let y = f64::from((ddata[i] + ddata[i - 1]) / 2.0);
                        sum1 += ((pbuf[i] + pbuf[i - 1]) / 2.0) * y * d;
                        sum2 += y * d;
                    }
                    if sum2.abs() > 1.0e-6 {
                        found = true;
                        self.pos[j].dv = sum1 / sum2;
                    } else {
                        self.pos[j].dv = self.pos[j].pp;
                    }
                }
            }
            _ => {}
        }

        if found {
            self.set_smsg(&format!("{} found.", pasm_name(self.pasm)));
        } else {
            self.set_smsg(&format!("{} NOT found.", pasm_name(self.pasm)));
            self.set_alrt(1);
        }
    }

    /// `sscan_test_fit`: copy the fit's working data into detector `i`'s
    /// fill buffer and post both of its buffers.
    fn test_fit_write(&mut self, i: usize, data: &[f32]) {
        let fill = self.fill();
        if let Some(buf) = self.det_buf[i].as_mut() {
            let n = data.len().min(buf[fill].len());
            buf[fill][..n].copy_from_slice(&data[..n]);
        }
        self.post(df(i, d::DA));
        self.post(df(i, d::CA));
    }

    /// C `afterScan`.
    pub(crate) fn after_scan(&mut self) {
        if self.nv[AS_OUT] == PV_OK {
            self.set_faze(faze::AFTER_SCAN_DO);
            self.request_do_puts();
        } else {
            self.set_faze(faze::SCAN_DONE);
        }
    }

    /// `recDynLinkPutCallback(&caLinkStruct[li], &value, 1, notifyCallback)`.
    fn put_callback(&mut self, li: usize, value: f64) -> i64 {
        self.links[li].put_callback(value, Cb::Notify)
    }

    /// The tail notifyCallback would have run had it counted the triggers
    /// to zero: shared by `doPuts` and `notifyCallback`.
    pub(crate) fn triggers_done(&mut self, called_by: u16) {
        if self.paus != 0 {
            self.set_smsg("Scan paused by operator");
            return;
        }
        if self.ddly < 0.001 {
            if self.wcnt != 0 {
                self.wtng = 1;
                self.post(WTNG);
                self.set_smsg("Waiting for client");
            } else {
                self.called_by = called_by;
                self.scan_once();
            }
        } else {
            self.called_by = called_by;
            self.request_delay(self.ddly);
        }
    }

    /// C `doPuts`: the writes of each scan step, run as a callback.
    pub(crate) fn do_puts(&mut self) {
        if self.paus != 0 {
            self.set_smsg("Scan paused by operator");
            return;
        }

        if self.dstate == dstate::TRIG_ARRAY_READ {
            self.set_dstate(dstate::ARRAY_READ_WAIT);
            if self.nv[A1_OUT] == 0 {
                self.num_aread_cb += 1;
                let status = self.put_callback(A1_OUT, f64::from(self.cd[A1_OUT - T1_OUT]));
                // C decrements once for any failure and again for
                // NOTIFY_IN_PROGRESS, leaving the count one short.
                if status != 0 {
                    self.num_aread_cb -= 1;
                }
                if status == NOTIFY_IN_PROGRESS {
                    self.set_alrt(NOTIFY_IN_PROGRESS as u8);
                    self.set_smsg("Array-read trigger 1 is busy");
                }
            }
            return;
        }

        let mut stage = self.faze;
        if stage == faze::BEFORE_SCAN {
            if self.nv[BS_OUT] == PV_OK {
                let bscd = f64::from(self.cd[BS_OUT - T1_OUT]);
                if self.bswait == LINKWAIT_YES {
                    self.set_faze(faze::BEFORE_SCAN_WAIT);
                    self.num_pos_cb += 1;
                    let status = self.put_callback(BS_OUT, bscd);
                    if status != 0 {
                        self.num_pos_cb -= 1;
                        if status == NOTIFY_IN_PROGRESS {
                            self.set_alrt(NOTIFY_IN_PROGRESS as u8);
                            self.set_smsg("Before-scan link is busy");
                        }
                    }
                } else {
                    self.links[BS_OUT].put(bscd);
                }
            }
            if self.num_pos_cb != 0 {
                return;
            }
            self.faze = faze::MOVE_MOTORS;
            stage = faze::MOVE_MOTORS;
        }

        if stage == faze::MOVE_MOTORS {
            self.num_pos_cb = 1;
            for i in 0..self.val_pos {
                let not_fly = !self.fly_mode(i);
                if self.nv[i] == PV_OK && (not_fly || self.cpt == 0) {
                    self.num_pos_cb += 1;
                    self.faze = faze::CHECK_MOTORS;
                    let status = self.put_callback(P1_OUT + i, self.pos[i].dv);
                    if status != 0 {
                        self.num_pos_cb -= 1;
                        if status == NOTIFY_IN_PROGRESS {
                            self.set_alrt(NOTIFY_IN_PROGRESS as u8);
                            self.set_smsg(&format!("Positioner {i} is already busy"));
                        }
                    }
                }
            }
            self.num_pos_cb -= 1;
            if self.num_pos_cb > 0 {
                self.post(FAZE);
                return;
            }
            stage = faze::TRIG_DETCTRS;
        }

        match stage {
            faze::TRIG_DETCTRS => self.trig_detectors(),
            faze::RETRACE_MOVE => {
                self.num_pos_cb = 1;
                if self.pasm != 0 {
                    for i in 0..self.val_pos {
                        if self.nv[i] != PV_OK {
                            continue;
                        }
                        let pasm = self.pasm;
                        let q = &mut self.pos[i];
                        let old = q.dv;
                        match pasm {
                            pasm::START => {
                                q.dv = if q.sm == SM_TABLE {
                                    self.pa[i][0]
                                } else {
                                    q.sp
                                };
                                if q.ar != 0 {
                                    q.dv += q.pp;
                                }
                            }
                            // `packData` has set `p_dv` already.
                            pasm::PEAK
                            | pasm::VALLEY
                            | pasm::RISING_EDGE
                            | pasm::FALLING_EDGE
                            | pasm::COM => {}
                            _ => q.dv = q.pp,
                        }
                        nudge(&mut q.dv, old);
                        self.set_faze(faze::RETRACE_WAIT);
                        self.num_pos_cb += 1;
                        if self.put_callback(P1_OUT + i, self.pos[i].dv) != 0 {
                            self.num_pos_cb -= 1;
                            self.set_alrt(1);
                            self.set_smsg(&format!("Can't retrace positioner {i}"));
                        }
                    }
                }
                self.num_pos_cb -= 1;
                if self.num_pos_cb > 0 {
                    return;
                }
                self.set_faze(faze::AFTER_SCAN_DO);
                self.after_scan_do();
            }
            faze::AFTER_SCAN_DO => self.after_scan_do(),
            faze::IDLE | faze::PREVIEW => {}
            faze::SCAN_PENDING => self.errlog(&format!(
                "{}:doPuts:SCAN_PENDING -- we shouldn't be here.",
                self.name
            )),
            _ => self.errlog(&format!(
                "{}:doPuts:default -- we shouldn't be here.",
                self.name
            )),
        }
    }

    /// `doPuts`' `sscanFAZE_TRIG_DETCTRS` case.
    fn trig_detectors(&mut self) {
        self.set_faze(faze::READ_DETCTRS);
        if self.awct != 0 {
            self.wcnt = self.awct;
            self.post(WCNT);
        }
        for i in 0..self.val_pos {
            if self.nv[i] == PV_OK && self.fly_mode(i) && self.cpt == 0 {
                self.links[P1_OUT + i].put(self.pos[i].dv);
                self.flying = true;
            }
        }
        if self.val_trig == 0 {
            self.called_by = called_by::DO_PUTS;
            if self.ddly == 0.0 {
                self.scan_once();
            } else {
                self.request_delay(self.ddly);
            }
            return;
        }
        self.num_trig_cb = 1;
        for i in 0..NUM_TRGS {
            if self.nv[T1_OUT + i] != 0 {
                continue;
            }
            self.num_trig_cb += 1;
            if self.put_callback(T1_OUT + i, f64::from(self.cd[i])) != 0 {
                self.num_trig_cb -= 1;
                self.set_alrt(NOTIFY_IN_PROGRESS as u8);
                self.set_smsg(&format!("Detector {} is busy", i + 1));
            }
        }
        self.num_trig_cb -= 1;
        if self.num_trig_cb > 0 {
            return;
        }
        self.triggers_done(called_by::DO_PUTS_TRIG);
    }

    /// `doPuts`' `sscanFAZE_AFTER_SCAN_DO` case.
    fn after_scan_do(&mut self) {
        if self.dstate < dstate::SAVE_DATA_WAIT {
            return;
        }
        let mut n = 0;
        if self.nv[AS_OUT] == PV_OK {
            let ascd = f64::from(self.cd[AS_OUT - T1_OUT]);
            if self.aswait == LINKWAIT_YES {
                self.set_faze(faze::AFTER_SCAN_WAIT);
                self.num_pos_cb += 1;
                if self.put_callback(AS_OUT, ascd) != 0 {
                    self.num_pos_cb -= 1;
                    self.set_alrt(1);
                    self.set_smsg("Can't fire After-scan link");
                } else {
                    n += 1;
                }
            } else {
                self.links[AS_OUT].put(ascd);
            }
        }
        if n == 0 {
            self.faze = faze::SCAN_DONE;
            self.called_by = called_by::DO_PUTS;
            self.scan_once();
        }
    }

    /// C `checkMonitors`.
    pub(crate) fn check_monitors(&mut self) {
        if self.dstate == dstate::POSTED {
            return;
        }
        let now = Instant::now();
        let abort_edge = self.pxsc == 1 && self.xsc == 0;
        if secs_since(self.tolp, now) > MIN_MON || abort_edge {
            self.tolp = Some(now);
            for i in 0..NUM_DET {
                if (self.det[i].lv - self.det[i].cv).abs() > 0.0 {
                    self.post(df(i, d::CV));
                    self.det[i].lv = self.det[i].cv;
                }
            }
            for i in 0..NUM_POS {
                if (self.pos[i].lv - self.pos[i].dv).abs() > 0.0 {
                    self.post(pf(i, p::DV));
                    self.pos[i].lv = self.pos[i].dv;
                }
                if (self.pos[i].r_lv - self.pos[i].r_cv).abs() > 0.0 {
                    self.post(pf(i, p::R_CV));
                    self.pos[i].r_lv = self.pos[i].r_cv;
                }
            }
        }

        if self.pcpt != self.cpt {
            self.post(CPT);
            self.pcpt = self.cpt;
            if self.cpt != 0 {
                self.post(VAL);
            }
        }

        let end_of_scan = self.dstate == dstate::PACKED;
        if !end_of_scan
            && self.atime >= 0.1
            && (secs_since(self.tlap, now) > f64::from(self.atime) || abort_edge)
        {
            self.tlap = Some(now);
            self.post_arrays(EventMask::VALUE);
        }

        if end_of_scan {
            self.set_dstate(dstate::POSTED);
            self.post_arrays(EventMask::VALUE | EventMask::LOG);
            if self.scan_err != 0 {
                self.set_alrt(self.scan_err);
            }
            if i32::from(self.xsc) != i32::from(self.pxsc) {
                self.post(XSC);
            }
            if self.exsc != self.xsc {
                self.exsc = self.xsc;
                self.post(EXSC);
            }
            self.bcpt = self.cpt;
            self.post(BCPT);
            self.data = 1;
            self.post_log(DATA);
            if self.aawait == NOYES_YES {
                self.await_ = 1;
                self.post(AWAIT);
            }
        }
    }

    /// C's posts of both buffers of every positioner and acquired detector,
    /// then of `nullArray`.
    pub(crate) fn post_arrays(&mut self, mask: EventMask) {
        for i in 0..NUM_POS {
            self.post_pos_buffers(i, mask);
        }
        for i in 0..NUM_DET {
            if self.acq[i] {
                self.post_mask(df(i, d::DA), mask);
                self.post_mask(df(i, d::CA), mask);
            }
        }
        self.post_null_served(mask);
    }

    /// C's post of positioner `i`'s buffers. C addresses `PnRA` at `pBufA`
    /// but `PnCA` at `p_ca`, which it never sets, so no post reaches `PnCA`
    /// (CBUG-AB4, sscanRecord.c:822, :1880); both are posted here.
    pub(crate) fn post_pos_buffers(&mut self, i: usize, mask: EventMask) {
        self.post_mask(pf(i, p::RA), mask);
        self.post_mask(pf(i, p::CA), mask);
    }

    /// C's post of `nullArray`. It reaches a detector's `DnnDA`/`DnnCA` only
    /// while their `dbAddr` is `nullArray`, i.e. the detector has no buffers
    /// (`cvt_dbaddr` sets `d_da`, sscanRecord.c:840-844, :1857); D01-D04
    /// always have theirs, acquired or not.
    pub(crate) fn post_null_served(&mut self, mask: EventMask) {
        for i in 0..NUM_DET {
            if self.det_buf[i].is_none() {
                self.post_mask(df(i, d::DA), mask);
                self.post_mask(df(i, d::CA), mask);
            }
        }
    }
}
