//! The `recDynLink` side: `lookupPV`, the link callbacks, `delayCallback`,
//! `checkLink` and `checkConnections` (`sscanRecord.c:2307-3190`).
//!
//! A read-back or detector PV of this database is read through `dbGet`, and
//! its search callback runs at lookup; every other PV goes through a
//! `recDynLink` (C's `useDynLinkAlways`).

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use epics_base_rs::runtime::task::spawn_blocking_background;
use epics_base_rs::server::snapshot::Snapshot;

use crate::rec_dyn_link::{
    ACCESS_READ, ACCESS_WRITE, CaType, Deliver, FATAL_ERROR, Io, LinkEvent, LinkHost, LinkSpec,
};

/// Link events in arrival order. A link task records its event here without
/// the record's lock, then applies the queue under it; `special`, which runs
/// under that lock, applies it itself while it waits for a connect.
#[derive(Default)]
pub(crate) struct Inbox {
    events: Mutex<VecDeque<(usize, u64, LinkEvent)>>,
    arrived: Condvar,
}

impl Inbox {
    fn push(&self, li: usize, generation: u64, event: LinkEvent) {
        self.events
            .lock()
            .unwrap()
            .push_back((li, generation, event));
        self.arrived.notify_all();
    }

    fn pop(&self) -> Option<(usize, u64, LinkEvent)> {
        self.events.lock().unwrap().pop_front()
    }

    /// Wait until an event is queued; `false` at `deadline` with none.
    fn wait(&self, deadline: Instant) -> bool {
        let mut events = self.events.lock().unwrap();
        while events.is_empty() {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            events = self.arrived.wait_timeout(events, left).unwrap().0;
        }
        true
    }
}

use super::*;

impl SscanRecord {
    /// The closure a link's task hands its events to: queued, then applied
    /// under the record's lock from a callback worker, so the link reactor
    /// never waits on that lock and keeps feeding a `special` that holds it.
    fn deliver(&self, li: usize) -> Option<Deliver> {
        let h = self.handle.clone()?;
        let name = self.name.clone();
        let inbox = self.inbox.clone();
        let prio = self.prio;
        Some(Arc::new(move |generation, event| {
            inbox.push(li, generation, event);
            let (h, name) = (h.clone(), name.clone());
            drop(spawn_blocking_background(prio, move |_| {
                with_sscan(&h, &name, |s| s.drain_inbox())
            }));
        }))
    }

    /// Every queued link event, oldest first: `recDynLink` applies it, then
    /// the callback C would call runs.
    fn drain_inbox(&mut self) {
        while let Some((li, generation, event)) = self.inbox.pop() {
            if let Some(cb) = self.links[li].apply(generation, event) {
                self.dispatch(li, cb);
            }
        }
    }

    /// C's wait in `special` for a connect in progress on link `li`
    /// (`sscanRecord.c:1315-1350`): up to `sscanRecordConnectWaitSeconds`.
    /// C's callbacks clear the flags without the record lock this put holds;
    /// the port's run under it, so the wait applies arriving events itself.
    pub(crate) fn wait_for_connect(&mut self, li: usize) {
        let busy = |s: &Self| {
            s.lpvt[li].connect_in_progress
                || (link_type(li) == LinkType::PositionerIn && s.lpvt[li].waiting_for_pos_mon)
        };
        if !busy(self) {
            return;
        }
        self.errlog(&format!(
            "{}:special:connect already in progress for link {}.  Waiting...",
            self.name,
            link_name(li)
        ));
        if li < NUM_PVS && knob(&SSCAN_RECORD_CONNECT_WAIT_SECONDS) < 0 {
            self.errlog(&format!(
                "{}:special: sscanRecordConnectWaitSeconds can't be negative; setting it to zero.",
                self.name
            ));
            SSCAN_RECORD_CONNECT_WAIT_SECONDS.store(0, Ordering::Relaxed);
        }
        // C sleeps a whole second between checks.
        let secs = knob(&SSCAN_RECORD_CONNECT_WAIT_SECONDS).max(0);
        for _ in 0..secs {
            self.drain_inbox();
            if !busy(self) {
                return;
            }
            let second = Instant::now() + std::time::Duration::from_secs(1);
            while self.inbox.wait(second) {
                self.drain_inbox();
            }
        }
        self.drain_inbox();
        if !busy(self) {
            return;
        }
        self.errlog(&format!(
            "{}:special:connect still in progress for link {}.  Trying new PV name.",
            self.name,
            link_name(li)
        ));
    }

    fn dispatch(&mut self, li: usize, cb: Cb) {
        match cb {
            Cb::Search => self.pv_search_callback(li),
            Cb::PosMon => self.pos_mon_callback(li),
            Cb::PosMonGet => self.pos_mon_get_cb(li),
            Cb::Notify => self.notify_callback(li),
            Cb::UserGet => self.user_get_callback(li),
        }
    }

    /// `recDynLinkAddInput` / `recDynLinkAddOutput` on link `li`.
    fn add_link(&mut self, li: usize, pv: &str, io: Io, ca_type: CaType, scalar: bool) {
        let (Some(handle), Some(deliver)) = (self.handle.clone(), self.deliver(li)) else {
            return;
        };
        let spec = LinkSpec {
            io,
            ca_type,
            scalar,
            search_callback: Some(Cb::Search),
            monitor_callback: (li < R1_IN).then_some(Cb::PosMon),
        };
        let host = LinkHost {
            handle: &handle,
            record: self.name.as_bytes(),
        };
        self.links[li].add(host, pv, spec, deliver);
    }

    /// C `dbNameToAddr(pvn[i], pAddr)` for read-back or detector `i`: when
    /// it resolves, the PV is read by `dbGet` and the search callback runs
    /// now, with no link.
    fn lookup_local(&mut self, i: usize) -> bool {
        let nelem = self
            .handle
            .as_ref()
            .and_then(|h| h.name_to_addr(self.pvn[i].as_bytes()));
        self.lpvt[i].local = nelem.map(|n| n as usize);
        if nelem.is_none() {
            return false;
        }
        // C defect, not copied: C `lookupPV` leaves the link of an earlier
        // remote PV open and unused (`sscanRecord.c:2378`, `:2393`).
        if self.links[i].is_added() {
            self.links[i].clear();
        }
        self.pv_search_callback(i);
        true
    }

    /// C `dbGet(puserPvt->pAddr, ty, .., &nRequest = n)` on local link `li`.
    pub(crate) fn db_get(&self, li: usize, ty: DbFieldType, n: usize) -> Option<Snapshot> {
        self.handle.as_ref()?.db_get(
            self.pvn[li % NUM_PVS].as_bytes(),
            ty,
            n,
            Some((self.name.as_bytes(), self as &dyn Record)),
        )
    }

    /// `recDynLinkGetNelem`, or `pAddr->no_elements` for a local PV.
    fn nelem(&self, li: usize) -> usize {
        self.lpvt[li]
            .local
            .unwrap_or_else(|| self.links[li].nelem().unwrap_or(1))
    }

    /// C `lookupPV`: open the link(s) of PV name `i`.
    pub(crate) fn lookup_pv(&mut self, i: usize) {
        // A `.DESC` field is read as `.VAL`.
        let b = self.pvn[i].as_bytes();
        if let Some(dot) = b.iter().rposition(|&c| c == b'.') {
            if b[dot..].starts_with(b".DESC") {
                let mut v = b[..dot].to_vec();
                v.extend_from_slice(b".VAL");
                self.pvn[i] = PvString::from_bytes(v);
                self.post(PV0 + i);
            }
        }
        let pv = String::from_utf8_lossy(self.pvn[i].as_bytes()).into_owned();
        match link_type(i) {
            LinkType::PositionerIn => {
                self.lpvt[i + NUM_PVS].connect_in_progress = true;
                self.add_link(i + NUM_PVS, &pv, Io::Output, CaType::Double, true);
                self.lpvt[i].connect_in_progress = true;
                self.lpvt[i].waiting_for_pos_mon = true;
                self.add_link(i, &pv, Io::Input, CaType::Double, true);
            }
            LinkType::Readback => {
                if pv.starts_with("TIME") || pv.starts_with("time") {
                    self.lpvt[i].ts = true;
                    self.nv[i] = NO_PV;
                    self.post(NV0 + i);
                    return;
                }
                self.lpvt[i].ts = false;
                if !self.lookup_local(i) {
                    self.lpvt[i].connect_in_progress = true;
                    self.add_link(i, &pv, Io::Input, CaType::Double, false);
                }
            }
            LinkType::Detector => {
                if !self.lookup_local(i) {
                    self.lpvt[i].connect_in_progress = true;
                    self.add_link(i, &pv, Io::Input, CaType::Float, false);
                }
            }
            LinkType::Trigger | LinkType::ReadArrayTrig | LinkType::BsAs => {
                self.lpvt[i].connect_in_progress = true;
                self.add_link(i, &pv, Io::Output, CaType::Float, true);
            }
            LinkType::PositionerOut => {}
        }
    }

    /// C `pvSearchCallback`: a link (dis)connected.
    fn pv_search_callback(&mut self, li: usize) {
        let pv = li % NUM_PVS;
        let lt = link_type(li);
        let input = matches!(
            lt,
            LinkType::PositionerIn | LinkType::Readback | LinkType::Detector
        );
        let mut st = self.nv[pv];
        self.lpvt[li].connect_in_progress = false;
        if self.lpvt[li].local.is_some() {
            st = PV_OK;
        } else if self.links[li].connection_status() != 0 {
            st |= if input { PV_NO_READ } else { PV_NO_WRITE };
        } else {
            let access = self.links[li].check_access();
            if input {
                if access & ACCESS_READ != 0 {
                    st &= if lt == LinkType::PositionerIn {
                        !PV_NO_READ
                    } else {
                        !PV_NC
                    };
                }
            } else {
                if access & ACCESS_READ != 0 {
                    st &= !PV_NO_READ;
                }
                if access & ACCESS_WRITE != 0 {
                    st &= !PV_NO_WRITE;
                }
            }
        }

        match lt {
            LinkType::PositionerIn if st & PV_NO_READ == 0 => {
                let link = &self.links[li];
                let (units, precision, limits) =
                    (link.units(8), link.precision(), link.control_limits());
                let q = &mut self.pos[pv];
                if let Some(u) = units {
                    q.eu = PvString::from_bytes(u.as_bytes());
                }
                if let Some(pr) = precision {
                    q.pr = pr;
                }
                if let Some((lr, hr)) = limits {
                    (q.lr, q.hr) = (lr, hr);
                }
                for off in [p::EU, p::PR, p::HR, p::LR] {
                    self.post(pf(pv, off));
                }
            }
            LinkType::Readback if st & PV_NO_READ == 0 => {
                let nelem = self.nelem(li);
                if nelem > 1 {
                    self.set_smsg("Array-valued positioner read-back");
                }
                self.lpvt[li].nelem = nelem;
            }
            LinkType::Detector if st == PV_OK => {
                let di = pv - D1_IN;
                let (units, precision, limits) = if self.lpvt[li].local.is_some() {
                    // C's `dbGet` fills an absent property with zeros.
                    match self.db_get(li, DbFieldType::Float, 1) {
                        Some(s) => {
                            let u = s.units().map_or(&[][..], |u| u.as_bytes());
                            (
                                Some(PvString::from_bytes(&u[..u.len().min(15)])),
                                Some(s.precision().unwrap_or(0)),
                                Some(s.graphic_limits().unwrap_or((0.0, 0.0))),
                            )
                        }
                        None => (None, None, None),
                    }
                } else {
                    let link = &self.links[li];
                    (
                        link.units(8).map(|u| PvString::from_bytes(u.as_bytes())),
                        link.precision(),
                        link.graphic_limits(),
                    )
                };
                let q = &mut self.det[di];
                if let Some(u) = units {
                    q.eu = u;
                }
                if let Some(pr) = precision {
                    q.pr = pr;
                }
                if let Some((lr, hr)) = limits {
                    (q.lr, q.hr) = (lr, hr);
                }
                for off in [d::EU, d::PR, d::HR, d::LR] {
                    self.post(df(di, off));
                }
                let nelem = self.nelem(li);
                if nelem > 1 {
                    self.set_smsg("Array-valued detector");
                }
                if self.lpvt[li].nelem == 0 {
                    self.errlog(&format!(
                        "{}: Allocating memory for detector {} (link '{}')",
                        self.name,
                        di + 1,
                        link_name(li)
                    ));
                    let n = self.mpts_len();
                    self.det_buf[di] = Some([vec![0.0; n], vec![0.0; n]]);
                }
                self.lpvt[li].nelem = nelem;
            }
            _ => {}
        }

        if self.nv[pv] != st {
            self.nv[pv] = st;
            self.post(NV0 + pv);
        }
        if st != PV_OK || self.faze != faze::SCAN_PENDING {
            return;
        }
        self.check_connections();
        if self.bad_output || self.bad_input {
            return;
        }
        if self.xsc == 0 {
            self.set_faze(faze::IDLE);
            self.set_smsg("Scan aborted");
            return;
        }
        if (0..NUM_POS).any(|i| self.nv[i] == PV_OK && self.lpvt[i].waiting_for_pos_mon) {
            return;
        }
        self.scan_by_search = true;
        self.called_by = called_by::PVSEARCH;
        self.scan_once();
    }

    /// C `posMonCallback`: positioner `li`'s value changed.
    fn pos_mon_callback(&mut self, li: usize) {
        // A monitor of the PV the link was retargeted from, before the new
        // PV's search callback.
        if self.nv[li] & PV_NO_READ != 0 {
            self.errlog(&format!(
                "{}:posMonCallback: ignoring obsolete monitor callback (link state: 'PV_NoRead')",
                self.name
            ));
            return;
        }
        match self.link_get1(li) {
            Some(v) => self.pos[li].cv = v,
            None => self.errlog(&format!(
                "{}:posMonCallback: recDynLinkGet returned -1, pvIndex={li}, cv={:.6}",
                self.name, self.pos[li].cv
            )),
        }
        self.post(pf(li, p::CV));
        // The first monitor may be stale: a get confirms it.
        if self.lpvt[li].waiting_for_pos_mon {
            let mut n = 1;
            self.links[li].get_callback(&mut n, Cb::PosMonGet);
        }
    }

    /// C `posMonCallbackGetCB`.
    fn pos_mon_get_cb(&mut self, li: usize) {
        if self.lpvt[li].waiting_for_pos_mon {
            self.lpvt[li].waiting_for_pos_mon = false;
            if let Some(v) = self.link_get1(li) {
                self.pos[li].cv = v;
            }
        }
        if self.faze != faze::SCAN_PENDING {
            return;
        }
        self.check_connections();
        if self.bad_output || self.bad_input {
            return;
        }
        if self.xsc == 0 {
            self.set_faze(faze::IDLE);
            self.set_smsg("Scan aborted");
            return;
        }
        let pending = (0..NUM_POS).any(|i| {
            self.nv[i] == PV_OK
                && (self.lpvt[i].waiting_for_pos_mon
                    || self.lpvt[i].connect_in_progress
                    || self.lpvt[i + NUM_PVS].connect_in_progress)
        });
        if pending {
            return;
        }
        self.scan_by_search = true;
        self.called_by = called_by::POSMON;
        self.scan_once();
    }

    /// C `notifyCallback`: a put-callback on link `li` completed.
    fn notify_callback(&mut self, li: usize) {
        if self.faze == faze::IDLE {
            self.set_smsg("callback while scan record is idle");
            return;
        }
        let status = self.links[li].status;
        if status != 0 {
            self.errlog(&format!("{}: notifyCallback: error {status}", self.name));
        }
        if status == FATAL_ERROR {
            self.xsc = 0;
            self.post(XSC);
            self.exsc = 0;
            self.post(EXSC);
            self.set_smsg("Scan aborted by notifyCallback");
            self.set_alrt(1);
        }
        match link_type(li) {
            LinkType::Trigger => {
                if self.num_trig_cb != 0 {
                    self.num_trig_cb -= 1;
                    if self.num_trig_cb == 0 {
                        self.triggers_done(called_by::NOTIFY_TRIG);
                    }
                }
            }
            LinkType::ReadArrayTrig => {
                if self.num_aread_cb != 0 {
                    self.num_aread_cb -= 1;
                    if self.num_aread_cb == 0 {
                        if self.paus != 0 {
                            self.set_smsg("Scan paused by operator");
                            return;
                        }
                        self.called_by = called_by::NOTIFY_READ_ARRAY_TRIG;
                        self.scan_once();
                    }
                }
            }
            _ => {
                if self.num_pos_cb == 0 {
                    return;
                }
                self.num_pos_cb -= 1;
                if self.num_pos_cb != 0 {
                    return;
                }
                if self.paus != 0 {
                    self.set_smsg("Scan paused by operator");
                    return;
                }
                self.called_by = called_by::NOTIFY;
                if self.faze == faze::RETRACE_WAIT {
                    self.set_faze(faze::AFTER_SCAN_DO);
                    self.scan_once();
                } else if self.faze == faze::AFTER_SCAN_WAIT {
                    self.faze = faze::SCAN_DONE;
                    self.scan_once();
                } else if self.faze != faze::CHECK_MOTORS || self.pdly == 0.0 {
                    self.scan_once();
                } else {
                    self.request_delay(self.pdly);
                }
            }
        }
    }

    /// C `userGetCallback`: a get the scan queued completed.
    fn user_get_callback(&mut self, li: usize) {
        let status = self.links[li].status;
        if status != 0 {
            self.errlog(&format!(
                "{}:userGetCallback: error {status} on link '{}'.  Retrying.",
                self.name,
                link_name(li)
            ));
            let mut n = if self.faze == faze::RECORD_SCALAR_DATA {
                1
            } else {
                self.npts.max(0) as usize
            };
            self.links[li].get_callback(&mut n, Cb::UserGet);
            return;
        }
        if !self.links[li].get_callback_in_progress {
            self.errlog(&format!(
                "{}:userGetCallback:callback while getCallbackInProgress==0 ignored",
                self.name
            ));
            return;
        }
        self.links[li].get_callback_in_progress = false;
        self.num_get_cb -= 1;
        if self.num_get_cb < 0 {
            self.errlog(&format!(
                "{}:userGetCallback:ERROR: numGetCallbacks<0 ({})",
                self.name, self.num_get_cb
            ));
            self.num_get_cb = 0;
        }
        if self.num_get_cb != 0 {
            return;
        }
        if self.paus != 0 {
            self.set_smsg("Scan paused by operator");
            return;
        }
        if self.dstate == dstate::ARRAY_GET_CALLBACK_WAIT {
            self.dstate = dstate::RECORD_ARRAY_DATA;
        }
        self.called_by = called_by::USERGETCALLBACK;
        self.scan_once();
    }

    /// C `delayCallback`: a `DDLY`/`PDLY`/`RDLY` wait ran out.
    pub(crate) fn delay_callback(&mut self) {
        if self.wcnt != 0 {
            self.wtng = 1;
            self.post(WTNG);
            self.set_smsg("Waiting for client");
        } else {
            self.called_by |= called_by::DELAY;
            self.scan_once();
        }
    }

    /// C `checkLink`: whether link `li` is unusable for `need`; updates PV
    /// state `pv` from what the link says.
    fn check_link(&mut self, li: usize, need: i32, pv: usize) -> bool {
        if self.lpvt[li].connect_in_progress || self.lpvt[li].waiting_for_pos_mon {
            return true;
        }
        let old = self.nv[pv];
        let mut bad = false;
        // A local PV has no link to check.
        if self.lpvt[li].local.is_none() {
            if self.links[li].connection_status() == 0 {
                if self.links[li].check_access() & need == 0 {
                    self.nv[pv] |= if need == ACCESS_READ {
                        PV_NO_READ
                    } else {
                        PV_NO_WRITE
                    };
                    bad = true;
                } else {
                    // Write access implies read.
                    self.nv[pv] &= if need == ACCESS_READ {
                        !PV_NO_READ
                    } else {
                        !PV_NC
                    };
                }
            } else {
                self.nv[pv] = PV_NC;
                bad = true;
            }
        }
        if self.nv[pv] != old {
            self.post(NV0 + pv);
        }
        bad
    }

    /// C `checkConnections`: recompute `badOutputPv` / `badInputPv`.
    pub(crate) fn check_connections(&mut self) {
        let (mut bad_out, mut bad_in) = (false, false);
        for j in 0..NUM_PVS {
            if self.nv[j] == NO_PV {
                continue;
            }
            match link_type(j) {
                LinkType::PositionerIn => {
                    bad_out |= self.check_link(j, ACCESS_READ, j);
                    bad_out |= self.check_link(j + NUM_PVS, ACCESS_WRITE, j);
                }
                LinkType::Readback | LinkType::Detector => {
                    bad_in |= self.check_link(j, ACCESS_READ, j);
                }
                _ => bad_out |= self.check_link(j, ACCESS_WRITE, j),
            }
        }
        self.bad_output = bad_out;
        self.bad_input = bad_in;
    }
}
