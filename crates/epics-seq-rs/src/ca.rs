//! The program's side of the pv layer (`seq_ca.c`): connecting channels
//! and handling their events.
//!
//! `got_monitor_count` is the number of channels whose `got_monitor` is
//! set: every change of the flag goes through
//! [`ProgDyn::set_got_monitor`]. C
//! decrements the count on each monitor-off whether or not the channel had
//! its first event, so a channel that disconnects before it leaves the
//! count wrapped below zero and a `+c` program waiting in `seq_connect` for
//! good; and a monitor-on clears the flag without the decrement, counting
//! the next event twice.

use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};

use epics_base_rs::runtime::log::{ErrlogSevEnum, errlog_sev_vprintf};
use epics_base_rs::server::snapshot::Snapshot;
use parking_lot::Mutex;

use crate::pv::{NO_CONTEXT, PvEvent, PvEventType, PvVar, pv_time_get_current_double};
use crate::pvt::{ProgDyn, Program, PvMeta, PvReq, QueueElem};
use crate::task::BufSrc;
use crate::types::{ChId, CharPtr, NOEVFLAG, PvSevr, PvStat};
use crate::var::ProgramVars;

impl<V: ProgramVars> Program<V> {
    /// `seq_connect`: create every named channel's pv, then with `wait`
    /// block until all are connected and their first monitors are in.
    pub fn seq_connect(self: &Arc<Self>, wait: bool) -> PvStat {
        for (n, chan) in self.chans.iter().enumerate() {
            let ch = ChId(n as u32);
            // Held across the create, so the connection handler, which runs
            // only after it returns, finds the pv stored.
            let mut d = self.dynamic.lock();
            let Some(dbch) = d.dbch(ch) else {
                continue;
            };
            match self.pv_var_create(ch, &dbch.db_name) {
                Ok(pvid) => d.set_pvid(ch, pvid),
                Err((_, message)) => {
                    errlog_sev_vprintf(
                        ErrlogSevEnum::Fatal,
                        format_args!(
                            "seq_connect(var '{}', pv '{}'): pvVarCreate() failure: {message}\n",
                            chan.def.var_name, dbch.db_name
                        ),
                    );
                    d.drop_uncreated_dbch(ch);
                }
            }
        }
        self.pv_sys.flush();
        if wait {
            self.wait_connected()
        } else {
            PvStat::Ok
        }
    }

    /// `seq_connect`'s wait, reporting the counts at each timeout.
    fn wait_connected(&self) -> PvStat {
        let start = pv_time_get_current_double();
        let mut first = true;
        let mut delay: i32 = 2;
        loop {
            if self.die.load(Ordering::Relaxed) {
                return PvStat::Error;
            }
            let (ac, mc, cc, gmc) = {
                let d = self.dynamic.lock();
                (
                    d.assign_count(),
                    d.monitor_count(),
                    d.connect_count(),
                    d.got_monitor_count(),
                )
            };
            if ac == cc && mc == gmc {
                break;
            }
            if !first {
                // C's `timeNow` is reset at the top of each pass, so it
                // printed "after 0 sec" every time.
                errlog_sev_vprintf(
                    ErrlogSevEnum::Minor,
                    format_args!(
                        "{}[{}](after {} sec): assigned={ac}, connected={cc}, \
                         monitored={mc}, got monitor={gmc}\n",
                        self.table.prog_name,
                        self.instance.load(Ordering::Relaxed),
                        (pv_time_get_current_double() - start) as i32,
                    ),
                );
            }
            first = false;
            self.ready.wait_timeout(f64::from(delay));
            delay = if delay < 3600 {
                (f64::from(delay) * 1.71) as i32
            } else {
                3600
            };
        }
        errlog_sev_vprintf(
            ErrlogSevEnum::Info,
            format_args!(
                "{}[{}]: all channels connected & received 1st monitor\n",
                self.table.prog_name,
                self.instance.load(Ordering::Relaxed)
            ),
        );
        PvStat::Ok
    }

    /// `seq_disconnect`: destroy every channel's pv, with no lock held
    /// across [`PvVar::destroy`].
    pub fn seq_disconnect(&self) {
        for (n, chan) in self.chans.iter().enumerate() {
            let Some((pvid, db_name)) = self
                .dynamic
                .lock()
                .dbch(ChId(n as u32))
                .and_then(|dbch| Some((dbch.pvid.clone()?, dbch.db_name.clone())))
            else {
                continue;
            };
            if pvid.destroy() != PvStat::Ok {
                errlog_sev_vprintf(
                    ErrlogSevEnum::Fatal,
                    format_args!(
                        "seq_disconnect(var '{}', pv '{db_name}'): pvVarDestroy() failure: {}\n",
                        chan.def.var_name,
                        pvid.message().as_deref().unwrap_or("(null)")
                    ),
                );
            }
        }
        self.pv_sys.flush();
    }

    /// `seq_camonitor`: turn channel `ch`'s monitor on or off. Takes
    /// `dynamic`, so the caller must not hold it; the connection handler,
    /// which does, uses its own locked form.
    pub fn seq_camonitor(self: &Arc<Self>, ch: ChId, on: bool) -> PvStat {
        let pvid = camonitor_begin(&mut self.dynamic.lock(), ch, on);
        match pvid {
            Some(pvid) => self.camonitor_call(ch, &pvid, on),
            None => PvStat::Ok,
        }
    }

    /// [`Self::seq_camonitor`] holding `dynamic` (`d`), which the pv layer
    /// allows: it runs no callback inside the call, and a monitor-off waits
    /// only for this pv's callback, which is the one calling.
    fn seq_camonitor_locked(self: &Arc<Self>, d: &mut ProgDyn, ch: ChId, on: bool) -> PvStat {
        match camonitor_begin(d, ch, on) {
            Some(pvid) => self.camonitor_call(ch, &pvid, on),
            None => PvStat::Ok,
        }
    }

    /// `pvVarMonitorOn` / `pvVarMonitorOff` for `seq_camonitor`.
    fn camonitor_call(self: &Arc<Self>, ch: ChId, pvid: &PvVar, on: bool) -> PvStat {
        let def = self.chans[ch.index()].def;
        let status = if on {
            let prog = Arc::downgrade(self);
            pvid.monitor_on(def.var_type.get_type(), def.count, move |ev| {
                if let Some(prog) = prog.upgrade() {
                    prog.seq_mon_handler(ch, ev)
                }
            })
        } else {
            pvid.monitor_off()
        };
        if status != PvStat::Ok {
            errlog_sev_vprintf(
                ErrlogSevEnum::Fatal,
                format_args!(
                    "seq_camonitor: pvVarMonitor{}(var '{}', pv '{}') failure: {}\n",
                    if on { "On" } else { "Off" },
                    def.var_name,
                    pvid.name(),
                    pvid.message().as_deref().unwrap_or("(null)")
                ),
            );
        }
        status
    }

    /// `pvVarCreate` for channel `ch` with `seq_conn_handler` as its
    /// connection callback: what `seq_connect` and `pvAssign` call. It does
    /// not take `dynamic`; the caller holds it until the pv is stored, as
    /// the handler takes it. A failure carries what C's `pvVarGetMess`
    /// would then return.
    pub fn pv_var_create(
        self: &Arc<Self>,
        ch: ChId,
        name: &str,
    ) -> Result<Arc<PvVar>, (PvStat, &'static str)> {
        let prog = Arc::downgrade(self);
        PvVar::create(&self.pv_sys, name, move |connected| {
            if let Some(prog) = prog.upgrade() {
                prog.seq_conn_handler(ch, connected)
            }
        })
        .map(Arc::new)
        .map_err(|status| (status, NO_CONTEXT))
    }

    /// `seq_conn_handler`.
    fn seq_conn_handler(self: &Arc<Self>, ch: ChId, connected: bool) {
        if self.conn_change(&mut self.dynamic.lock(), ch, connected) {
            // Every state set: the connect and monitor counts act as
            // monitored anonymous channels in when-conditions.
            self.ss_wakeup(0);
        }
    }

    /// The `sp->lock` half of `seq_conn_handler`: false, waking nobody, when
    /// the channel is anonymous.
    fn conn_change(self: &Arc<Self>, d: &mut ProgDyn, ch: ChId, connected: bool) -> bool {
        let n = ch.index();
        let chan = &self.chans[n];
        let Some(changed) = d.set_connected(ch, connected) else {
            return false;
        };
        if !changed {
            let dbch = d.dbch(ch).expect("set_connected found the channel");
            let args = if connected {
                format_args!(
                    "seq_conn_handler: var '{}', pv '{}': connect event but already connected\n",
                    chan.def.var_name, dbch.db_name
                )
            } else {
                format_args!(
                    "seq_conn_handler(var '{}', pv '{}'): disconnect event but already disconnected\n",
                    chan.def.var_name, dbch.db_name
                )
            };
            errlog_sev_vprintf(ErrlogSevEnum::Minor, args);
            return true;
        }
        let monitored = chan.monitored.load(Ordering::Relaxed);
        if connected {
            let db_count = d
                .dbch(ch)
                .and_then(|dbch| dbch.pvid.as_ref())
                .expect("seq_conn_handler: a connect before the pv was stored")
                .count();
            self.signal_if_ready(d);
            chan.db_count
                .store(chan.def.count.min(db_count), Ordering::Relaxed);
            if monitored {
                self.seq_camonitor_locked(d, ch, true);
            }
        } else {
            if monitored {
                self.seq_camonitor_locked(d, ch, false);
            }
            for ss in &self.ss {
                *ss.get_req[n].lock() = None;
                *ss.put_req[n].lock() = None;
                ss.sync_sem.signal();
            }
        }
        true
    }

    /// Signal `ready` when every channel is connected and every monitored
    /// one has had its first event.
    fn signal_if_ready(&self, d: &ProgDyn) {
        if d.got_monitor_count() == d.monitor_count() && d.connect_count() == d.assign_count() {
            self.ready.signal();
        }
    }

    /// `seq_get_handler`: a get completed; ignored unless `req` is still
    /// its state set's pending get on the channel.
    pub fn seq_get_handler(&self, req: &Arc<PvReq>, ev: PvEvent<'_>) {
        if is_pending(&self.ss[req.ss].get_req[req.ch.index()], req) {
            self.proc_db_events(&mut self.dynamic.lock(), req.ch, Some(req), &ev);
        }
    }

    /// `seq_put_handler`: as [`Self::seq_get_handler`], for a put.
    pub fn seq_put_handler(&self, req: &Arc<PvReq>, ev: PvEvent<'_>) {
        if is_pending(&self.ss[req.ss].put_req[req.ch.index()], req) {
            self.proc_db_events(&mut self.dynamic.lock(), req.ch, Some(req), &ev);
        }
    }

    /// `seq_mon_handler`, under one hold of `dynamic` where C takes it twice.
    fn seq_mon_handler(&self, ch: ChId, ev: PvEvent<'_>) {
        let d = &mut *self.dynamic.lock();
        self.proc_db_events(d, ch, None, &ev);
        if d.set_got_monitor(ch, true) == Some(true) {
            self.signal_if_ready(d);
        }
    }

    /// `proc_db_events`, under `dynamic` (`d`): store the value, then wake
    /// whoever waits for it. `req` is the get or put that completed, `None`
    /// for a monitor.
    fn proc_db_events(
        &self,
        d: &mut ProgDyn,
        ch: ChId,
        req: Option<&Arc<PvReq>>,
        ev: &PvEvent<'_>,
    ) {
        let n = ch.index();
        let Some(dbch) = d.dbch(ch) else {
            return;
        };
        let def = self.chans[n].def;
        if def.queue_size > 0 && ev.evt == PvEventType::Monitor {
            if let Some(value) = ev.value {
                let elem = QueueElem {
                    value: value.value.clone(),
                    meta: value_meta(value, None),
                };
                if self.queues[def.queue_index as usize].put(elem) {
                    errlog_sev_vprintf(
                        ErrlogSevEnum::Minor,
                        format_args!(
                            "monitor event for variable '{}' (pv '{}'): \
                             last queue element overwritten (queue is full)\n",
                            def.var_name, dbch.db_name
                        ),
                    );
                }
            }
        } else if let Some(value) = ev.value {
            let mut meta = value_meta(value, None);
            if meta.severity != PvSevr::None {
                static UNKNOWN: LazyLock<Arc<[u8]>> = LazyLock::new(|| Arc::from(&b"unknown"[..]));
                let message = dbch.pvid.as_ref().and_then(|pvid| pvid.message());
                meta.message = Some(message.map_or_else(|| UNKNOWN.clone(), Arc::from));
            }
            self.ss_write_buffer(
                ch,
                BufSrc::Pv(&value.value),
                Some(meta),
                ev.evt == PvEventType::Monitor,
            );
        }
        match (ev.evt, req) {
            (PvEventType::Put, Some(req)) => self.request_done(&self.ss[req.ss].put_req[n], req),
            (PvEventType::Get, Some(req)) => {
                self.request_done(&self.ss[req.ss].get_req[n], req);
                // Under `+s` a get's effect is local to its state set.
                if !self.table.vars.safe() {
                    self.ss_wakeup(def.event_num);
                }
            }
            _ => self.ss_wakeup(def.event_num),
        }
        let ef = d.synced_to[n];
        if ef != NOEVFLAG {
            self.ef_set_locked(d, ef);
        }
    }

    /// Clear `slot` and wake its state set. Only while `req` is still the
    /// pending one: a cancel and a new request may have come in since the
    /// handler checked, and C's unconditional clear would lose the new one.
    fn request_done(&self, slot: &Mutex<Option<Arc<PvReq>>>, req: &Arc<PvReq>) {
        {
            let mut pending = slot.lock();
            if pending.as_ref().is_some_and(|p| Arc::ptr_eq(p, req)) {
                *pending = None;
            }
        }
        self.ss[req.ss].sync_sem.signal();
    }
}

/// The `sp->lock` half of `seq_camonitor`: clear `gotMonitor`, and return
/// the pv unless its monitor is already as asked.
fn camonitor_begin(d: &mut ProgDyn, ch: ChId, on: bool) -> Option<Arc<PvVar>> {
    let pvid = d
        .dbch(ch)
        .and_then(|dbch| dbch.pvid.clone())
        .expect("seq_camonitor: the channel has no pv");
    d.set_got_monitor(ch, false);
    (pvid.monitor_is_on() != on).then_some(pvid)
}

fn is_pending(slot: &Mutex<Option<Arc<PvReq>>>, req: &Arc<PvReq>) -> bool {
    slot.lock().as_ref().is_some_and(|p| Arc::ptr_eq(p, req))
}

/// `pv_stamp`, `pv_status` and `pv_severity` of `value`.
fn value_meta(value: &Snapshot, message: CharPtr) -> PvMeta {
    PvMeta {
        timestamp: value.timestamp,
        status: alarm_status(value.alarm.status),
        severity: alarm_severity(value.alarm.severity),
        message,
    }
}

/// `pvStat` numbers the alarm conditions as `epicsAlarmCondition` does. A
/// value past them, which C passes through as a number, has no `PvStat`
/// and reads as an error.
fn alarm_status(status: u16) -> PvStat {
    const BY_CONDITION: [PvStat; 22] = [
        PvStat::Ok,
        PvStat::Read,
        PvStat::Write,
        PvStat::Hihi,
        PvStat::High,
        PvStat::Lolo,
        PvStat::Low,
        PvStat::State,
        PvStat::Cos,
        PvStat::Comm,
        PvStat::Timeout,
        PvStat::HwLimit,
        PvStat::Calc,
        PvStat::Scan,
        PvStat::Link,
        PvStat::Soft,
        PvStat::BadSub,
        PvStat::Udf,
        PvStat::Disable,
        PvStat::Simm,
        PvStat::ReadAccess,
        PvStat::WriteAccess,
    ];
    BY_CONDITION
        .get(usize::from(status))
        .copied()
        .unwrap_or(PvStat::Error)
}

/// As [`alarm_status`], for `epicsAlarmSeverity`.
fn alarm_severity(severity: u16) -> PvSevr {
    match severity {
        0 => PvSevr::None,
        1 => PvSevr::Minor,
        2 => PvSevr::Major,
        3 => PvSevr::Invalid,
        _ => PvSevr::Error,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32};
    use std::time::Duration;

    use epics_base_rs::runtime::task::test_block_on;
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::server::records::waveform::WaveformRecord;
    use epics_base_rs::types::{DbFieldType, EpicsValue, WallTime};

    use super::*;
    use crate::program::{
        ProgOptions, SEQ_MAGIC, SeqChan, SeqProgram, SeqSs, SeqState, StateOptions, VarModel,
    };
    use crate::pv::PvSystem;
    use crate::pvt::{Chan, SeqEvent, SeqMacros, Sscb, VarBlock};
    use crate::queue::SeqQueue;
    use crate::state_set::StateSet;
    use crate::types::{EfId, PrimType, PvType, bit_test};
    use crate::var::{SeqVar, Zeroed};

    #[derive(Clone)]
    struct Vars {
        x: SeqVar<f64>,
    }

    impl ProgramVars for Vars {
        fn zeroed() -> Self {
            Vars {
                x: Zeroed::zeroed(),
            }
        }
    }

    fn event(_: &StateSet<Vars>, _: &mut usize, _: &mut usize) -> bool {
        false
    }

    fn action(_: &StateSet<Vars>, _: usize, _: &mut usize) {}

    fn init(_: &Vars) {}

    static STATES: [SeqState<Vars>; 1] = [SeqState {
        state_name: "s",
        action_func: action,
        event_func: event,
        entry_func: None,
        exit_func: None,
        event_mask: &[0],
        options: StateOptions::new(0),
    }];

    static SS: [SeqSs<Vars>; 2] = [
        SeqSs {
            ss_name: "a",
            states: &STATES,
        },
        SeqSs {
            ss_name: "b",
            states: &STATES,
        },
    ];

    /// Channel `x` on "X" with event 1, synced to `ef_id` and in a queue of
    /// `queue_size` (none for 0), in a program of one event flag.
    fn table(vars: VarModel<Vars>, ef_id: EfId, queue_size: u32) -> &'static SeqProgram<Vars> {
        Box::leak(Box::new(SeqProgram {
            magic: SEQ_MAGIC,
            prog_name: "t",
            chan: Box::leak(Box::new([SeqChan {
                ch_name: Some("X"),
                var: |v: &Vars| &v.x,
                var_name: "x",
                var_type: PrimType::Double,
                count: 1,
                event_num: 1,
                ef_id,
                monitored: true,
                queue_size,
                queue_index: 0,
            }])),
            ss: &SS,
            vars,
            params: "",
            num_ev_flags: 1,
            options: ProgOptions::new(ProgOptions::CONN),
            init_func: init,
            entry_func: None,
            exit_func: None,
            num_queues: queue_size.min(1),
        }))
    }

    fn meta() -> PvMeta {
        PvMeta {
            timestamp: WallTime::from_unix(0, 0),
            status: PvStat::Ok,
            severity: PvSevr::None,
            message: None,
        }
    }

    fn reqs() -> Box<[Mutex<Option<Arc<PvReq>>>]> {
        Box::new([Mutex::new(None)])
    }

    /// [`prog_of`] a plain table.
    fn prog(monitored: bool) -> Arc<Program<Vars>> {
        prog_of(table(VarModel::Reentrant, NOEVFLAG, 0), monitored)
    }

    /// `table`'s one channel, assigned to a pv that never connects;
    /// `monitored` as given, with `monitor_count` to match.
    fn prog_of(table: &'static SeqProgram<Vars>, monitored: bool) -> Arc<Program<Vars>> {
        let def = &table.chan[0];
        let mut synced = vec![Vec::new(); 2];
        if def.ef_id != NOEVFLAG {
            synced[def.ef_id.0 as usize].push(X);
        }
        let mut d = ProgDyn::new(1, Box::new([0]), synced.into(), Box::new([def.ef_id]));
        d.init_dbch(X, "X".into(), monitored);
        d.set_pvid(X, Arc::new(PvVar::unconnected("X")));
        let ss = SS.iter().map(|def| Sscb {
            def,
            var: table.vars.safe().then(Vars::zeroed),
            thread_id: OnceLock::new(),
            current_state: AtomicI32::new(0),
            next_state: AtomicI32::new(0),
            prev_state: AtomicI32::new(-1),
            mask: Mutex::new(&[0]),
            time_entered: Mutex::new(0.0),
            wakeup_time: Mutex::new(0.0),
            sync_sem: SeqEvent::default(),
            dead: SeqEvent::default(),
            get_req: reqs(),
            put_req: reqs(),
            meta: Box::new([Mutex::new(meta())]),
            dirty: Box::new([AtomicBool::new(false)]),
        });
        Arc::new(Program {
            table,
            vars: VarBlock::Owned(Box::new(Vars::zeroed())),
            instance: AtomicU32::new(0),
            thread_priority: 0,
            stack_size: 0,
            pv_sys: PvSystem::create(&PvDatabase::new()),
            chans: Box::new([Chan {
                def,
                monitored: AtomicBool::new(monitored),
                db_count: AtomicU32::new(1),
                var_lock: Mutex::new(meta()),
            }]),
            queues: (0..table.num_queues)
                .map(|_| SeqQueue::new(def.queue_size as usize).unwrap())
                .collect(),
            ss: ss.collect(),
            macros: SeqMacros::default(),
            dynamic: Mutex::new(d),
            die: AtomicBool::new(false),
            ready: SeqEvent::default(),
            dead: SeqEvent::default(),
        })
    }

    const X: ChId = ChId(0);

    fn conn(p: &Arc<Program<Vars>>, connected: bool) {
        assert!(p.conn_change(&mut p.dynamic.lock(), X, connected));
    }

    fn counts(p: &Program<Vars>) -> (u32, u32) {
        let d = p.dynamic.lock();
        (d.connect_count(), d.got_monitor_count())
    }

    fn pvid(p: &Program<Vars>) -> Arc<PvVar> {
        p.dynamic
            .lock()
            .dbch(X)
            .and_then(|dbch| dbch.pvid.clone())
            .unwrap()
    }

    fn first_event(p: &Program<Vars>) {
        let d = &mut *p.dynamic.lock();
        d.set_got_monitor(X, true);
        p.signal_if_ready(d);
    }

    fn value(v: f64, severity: u16) -> Snapshot {
        Snapshot::new(
            EpicsValue::Double(v),
            severity,
            severity,
            WallTime::from_unix(1, 0),
        )
    }

    fn pv_event(evt: PvEventType, value: Option<&Snapshot>) -> PvEvent<'_> {
        PvEvent {
            evt,
            ty: PvType::TimeDouble,
            count: 1,
            value,
            status: PvStat::Ok,
            severity: PvSevr::None,
            message: None,
        }
    }

    /// `evt` on `x`: a get or put as state set 0's pending request.
    fn deliver(p: &Program<Vars>, evt: PvEventType, value: Option<&Snapshot>) {
        let ev = pv_event(evt, value);
        let req = Arc::new(PvReq { ch: X, ss: 0 });
        match evt {
            PvEventType::Get => {
                *p.ss[0].get_req[0].lock() = Some(req.clone());
                p.seq_get_handler(&req, ev);
            }
            PvEventType::Put => {
                *p.ss[0].put_req[0].lock() = Some(req.clone());
                p.seq_put_handler(&req, ev);
            }
            PvEventType::Monitor => p.seq_mon_handler(X, ev),
        }
    }

    /// `wait_connected` on another thread: still waiting after 200 ms, and
    /// back with `Ok` once `release` has run.
    fn waits_until(p: &Program<Vars>, release: impl FnOnce()) {
        std::thread::scope(|s| {
            let (tx, rx) = std::sync::mpsc::channel();
            s.spawn(move || tx.send(p.wait_connected()).unwrap());
            assert!(
                rx.recv_timeout(Duration::from_millis(200)).is_err(),
                "returned early"
            );
            release();
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(PvStat::Ok));
        });
    }

    #[test]
    fn a_connect_counts_once_and_takes_the_smaller_count() {
        let p = prog(false);
        conn(&p, true);
        assert_eq!(counts(&p), (1, 0));
        assert!(p.ready.wait_timeout(0.0), "all channels are in");
        // The pv reports 0 elements until its server answers.
        assert_eq!(p.chans[0].db_count.load(Ordering::Relaxed), 0);
        conn(&p, true);
        assert_eq!(counts(&p), (1, 0), "a second connect changes nothing");
        assert!(!p.ready.wait_timeout(0.0));

        // A pv of more elements than the variable's one.
        let db = PvDatabase::new();
        let wf = test_block_on(async {
            db.add_record("WF", Box::new(WaveformRecord::new(10, DbFieldType::Double)))
                .await
                .unwrap();
            db.ca_client().create_channel("WF").await.unwrap()
        });
        conn(&p, false);
        p.dynamic
            .lock()
            .set_pvid(X, Arc::new(PvVar::on_channel("WF", wf)));
        conn(&p, true);
        assert_eq!(p.chans[0].db_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_connection_change_of_an_anonymous_channel_wakes_nobody() {
        let p = prog(false);
        drop(p.dynamic.lock().unassign_dbch(X));
        p.seq_conn_handler(X, true);
        p.seq_conn_handler(X, false);
        for ss in &p.ss {
            assert!(!ss.sync_sem.wait_timeout(0.0));
        }
    }

    #[test]
    fn a_disconnect_ends_every_state_sets_pending_requests() {
        let p = prog(false);
        conn(&p, true);
        p.ready.wait_timeout(0.0);
        for (ss, slot) in [(0, &p.ss[0].get_req[0]), (1, &p.ss[1].put_req[0])] {
            *slot.lock() = Some(Arc::new(PvReq { ch: X, ss }));
        }
        conn(&p, false);
        assert_eq!(counts(&p), (0, 0));
        for ss in &p.ss {
            assert!(ss.get_req[0].lock().is_none() && ss.put_req[0].lock().is_none());
            assert!(ss.sync_sem.wait_timeout(0.0));
        }
        conn(&p, false);
        assert_eq!(counts(&p), (0, 0), "a second disconnect changes nothing");
    }

    #[test]
    fn a_monitored_channel_is_ready_after_its_first_event() {
        let p = prog(true);
        conn(&p, true);
        assert!(pvid(&p).monitor_is_on());
        assert!(!p.ready.wait_timeout(0.0), "no monitor yet");
        first_event(&p);
        assert_eq!(counts(&p), (1, 1));
        assert!(p.ready.wait_timeout(0.0));
        conn(&p, false);
        assert!(!pvid(&p).monitor_is_on());
        assert_eq!(counts(&p), (0, 0));
    }

    #[test]
    fn a_disconnect_before_the_first_event_leaves_the_count_at_zero() {
        let p = prog(true);
        conn(&p, true);
        conn(&p, false);
        assert_eq!(counts(&p), (0, 0), "C wraps it below zero here");
        conn(&p, true);
        first_event(&p);
        assert_eq!(counts(&p), (1, 1));
        assert!(p.ready.wait_timeout(0.0));
    }

    #[test]
    fn monitor_on_again_counts_the_next_event_once() {
        let p = prog(true);
        conn(&p, true);
        first_event(&p);
        assert_eq!(p.seq_camonitor(X, true), PvStat::Ok);
        assert!(pvid(&p).monitor_is_on());
        assert_eq!(counts(&p), (1, 0));
        first_event(&p);
        assert_eq!(counts(&p), (1, 1), "C counts it twice");
        assert_eq!(p.seq_camonitor(X, false), PvStat::Ok);
        assert_eq!(counts(&p), (1, 0));
    }

    #[test]
    fn a_completion_for_a_replaced_request_leaves_the_new_one() {
        let p = prog(false);
        let old = Arc::new(PvReq { ch: X, ss: 0 });
        let new = Arc::new(PvReq { ch: X, ss: 0 });
        let slot = &p.ss[0].get_req[0];
        *slot.lock() = Some(new.clone());
        p.request_done(slot, &old);
        assert!(slot.lock().as_ref().is_some_and(|r| Arc::ptr_eq(r, &new)));
        p.request_done(slot, &new);
        assert!(slot.lock().is_none());
    }

    #[test]
    fn a_completion_of_a_request_no_longer_pending_is_dropped() {
        let p = prog(false);
        let old = Arc::new(PvReq { ch: X, ss: 0 });
        let new = Arc::new(PvReq { ch: X, ss: 0 });
        *p.ss[0].get_req[0].lock() = Some(new.clone());
        *p.ss[0].put_req[0].lock() = Some(new.clone());
        let v = value(2.5, 0);
        p.seq_get_handler(&old, pv_event(PvEventType::Get, Some(&v)));
        p.seq_put_handler(&old, pv_event(PvEventType::Put, None));
        assert_eq!(p.var().x.get(), 0.0);
        assert!(!p.ss[0].sync_sem.wait_timeout(0.0));
        for slot in [&p.ss[0].get_req[0], &p.ss[0].put_req[0]] {
            assert!(slot.lock().as_ref().is_some_and(|r| Arc::ptr_eq(r, &new)));
        }
    }

    #[test]
    fn the_first_monitor_event_makes_the_program_ready() {
        let p = prog(true);
        conn(&p, true);
        assert!(!p.ready.wait_timeout(0.0));
        deliver(&p, PvEventType::Monitor, Some(&value(2.5, 0)));
        assert_eq!(counts(&p), (1, 1));
        assert!(p.ready.wait_timeout(0.0));
    }

    /// Under `+s` a get is its own state set's: it wakes only the requester
    /// and marks nothing dirty, where a monitor wakes every state set waiting
    /// on the channel's event and marks it dirty for each.
    #[test]
    fn a_get_under_safe_wakes_only_its_requester() {
        for (vars, evt, woken, dirty) in [
            (VarModel::Reentrant, PvEventType::Get, [true, true], false),
            (VarModel::Safe, PvEventType::Get, [true, false], false),
            (VarModel::Safe, PvEventType::Monitor, [true, true], true),
        ] {
            let p = prog_of(table(vars, NOEVFLAG, 0), false);
            for ss in &p.ss {
                *ss.mask.lock() = &[1 << 1];
            }
            deliver(&p, evt, Some(&value(2.5, 0)));
            assert_eq!(p.var().x.get(), 2.5);
            for (ss, woken) in p.ss.iter().zip(woken) {
                assert_eq!(ss.sync_sem.wait_timeout(0.0), woken, "{evt:?}");
                assert_eq!(ss.dirty[0].load(Ordering::Relaxed), dirty, "{evt:?}");
            }
        }
    }

    #[test]
    fn every_event_on_a_synced_channel_sets_its_flag() {
        for (evt, v) in [
            (PvEventType::Get, Some(value(1.0, 0))),
            (PvEventType::Put, None),
            (PvEventType::Monitor, Some(value(1.0, 0))),
        ] {
            let p = prog_of(table(VarModel::Reentrant, EfId(1), 0), false);
            deliver(&p, evt, v.as_ref());
            assert!(bit_test(&p.dynamic.lock().ev_flags, 1), "{evt:?}");
        }
    }

    #[test]
    fn an_alarmed_value_carries_the_pvs_message() {
        let p = prog(false);
        let message = || p.chans[0].var_lock.lock().message.clone();
        deliver(&p, PvEventType::Monitor, Some(&value(1.0, 0)));
        assert_eq!(message(), None);
        deliver(&p, PvEventType::Monitor, Some(&value(1.0, 2)));
        assert_eq!(message().as_deref(), Some(&b"unknown"[..]));
        // A refused get leaves its message in the pv.
        pvid(&p).get_callback(PvType::TimeDouble, 1, |_| {});
        let refused = pvid(&p).message().unwrap();
        deliver(&p, PvEventType::Monitor, Some(&value(1.0, 2)));
        assert_eq!(message().as_deref(), Some(refused.as_bytes()));
    }

    #[test]
    fn a_queued_channels_monitor_goes_to_its_queue_not_the_buffer() {
        let p = prog_of(table(VarModel::Reentrant, NOEVFLAG, 1), false);
        deliver(&p, PvEventType::Monitor, Some(&value(1.0, 0)));
        deliver(&p, PvEventType::Monitor, Some(&value(2.0, 0)));
        assert_eq!(p.queues[0].used(), 1);
        assert_eq!(
            p.queues[0].get().unwrap().value,
            EpicsValue::Double(2.0),
            "a full queue's newest is overwritten"
        );
        assert_eq!(p.var().x.get(), 0.0);
    }

    #[test]
    fn a_failed_create_takes_the_channel_and_its_counts() {
        let p = prog(true);
        assert_eq!(p.seq_connect(false), PvStat::Ok);
        let d = p.dynamic.lock();
        assert!(d.dbch(X).is_none());
        assert_eq!((d.assign_count(), d.monitor_count()), (0, 0));
    }

    #[test]
    fn the_connect_wait_lasts_until_each_channel_and_first_monitor_is_in() {
        let p = prog(false);
        waits_until(&p, || conn(&p, true));
        let p = prog(true);
        conn(&p, true);
        waits_until(&p, || first_event(&p));
    }

    #[test]
    fn disconnect_destroys_the_pv() {
        let p = prog(false);
        let pvid = pvid(&p);
        p.seq_disconnect();
        assert_eq!(
            pvid.get_callback(PvType::TimeDouble, 1, |_| {}),
            PvStat::Error
        );
    }

    #[test]
    fn a_create_without_a_context_fails_with_the_reason() {
        let p = prog(false);
        let Err((status, message)) = p.pv_var_create(X, "X") else {
            panic!("created without a reactor");
        };
        assert_eq!((status, message), (PvStat::Error, NO_CONTEXT));
    }

    #[test]
    fn the_alarm_status_and_severity_keep_their_numbers() {
        for n in 0..22u16 {
            assert_eq!(alarm_status(n) as i32, i32::from(n));
        }
        assert_eq!(alarm_status(22), PvStat::Error);
        for n in 0..4u16 {
            assert_eq!(alarm_severity(n) as i32, i32::from(n));
        }
        assert_eq!(alarm_severity(4), PvSevr::Error);
    }
}
