//! `SS_ID` and the calls generated code makes on it (`seqCom.h`,
//! `seq_snc.h`, `seq_if.c`). C's `seq_xxx(ss, ...)` is `ss.xxx(...)` in
//! snake case; snc's defaults for omitted arguments are snc-rs's to fill.
//!
//! Every call runs on the calling state set's thread and blocks it where
//! C's does (a `SYNC` get or put, `pvAssign`), so generated functions are
//! plain synchronous Rust.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use epics_base_rs::runtime::log::{ErrlogSevEnum, errlog_sev_vprintf};
use epics_base_rs::types::WallTime;
use epics_libcom_rs::cfmt::CFloat;

use crate::c::CSrc;
use crate::program::ProgOptions;
use crate::pv::{PvEventType, PvVar, pv_time_get_current_double};
use crate::pvt::{
    BufSrc, Chan, DbChan, ProgDyn, Program, PvMeta, PvReq, QueueElem, SeqQueue, Sscb,
};
use crate::types::{
    ChId, CharPtr, CompType, EfId, NOEVFLAG, PvSevr, PvStat, bit_clear, bit_set, bit_test,
};
use crate::var::{ProgramVars, SeqVar};

/// A pending get or put of one channel in one state set.
type ReqSlot = parking_lot::Mutex<Option<Arc<PvReq>>>;

/// A `PVMETA` message from a C string literal.
fn message(s: &str) -> CharPtr {
    Some(Arc::from(s.as_bytes()))
}

/// `"pvGet"` / `"pvPut"`, as `check_pending` names the call.
fn call_name(ev: PvEventType) -> &'static str {
    match ev {
        PvEventType::Get => "pvGet",
        PvEventType::Put | PvEventType::Monitor => "pvPut",
    }
}

/// `SS_ID`: the state set a generated function runs in.
pub struct StateSet<V: 'static> {
    pub(crate) prog: Arc<Program<V>>,
    pub(crate) index: usize,
}

/// The completion array of `pvPutComplete` / `pvArrayGetComplete`: an
/// integer array variable, one element per channel of the multi-PV array.
pub trait CompleteFlags {
    fn set_flag(&self, index: usize, done: bool);
}

impl<T: From<bool>, const N: usize> CompleteFlags for SeqVar<[T; N]> {
    fn set_flag(&self, index: usize, done: bool) {
        self.modify(|a| a[index] = T::from(done))
    }
}

/// The `seq_if.c` calls other run-time modules make on the program.
impl<V: ProgramVars> Program<V> {
    /// C's `assert(ev_flag > 0 && ev_flag <= sp->numEvFlags)`.
    fn check_ef(&self, ef: EfId) {
        assert!(
            ef.0 > 0 && ef.0 <= self.table.num_ev_flags,
            "event flag {} is not one of 1..={}",
            ef.0,
            self.table.num_ev_flags
        );
    }

    /// `seq_efSet`: takes `dynamic`.
    pub fn ef_set(&self, ef: EfId) {
        let mut d = self.dynamic.lock();
        self.ef_set_locked(&mut d, ef);
    }

    /// `seq_efSet` from a caller that holds `dynamic` (`d`), where C
    /// relocks its recursive `sp->lock`: `proc_db_events`.
    pub fn ef_set_locked(&self, d: &mut ProgDyn, ef: EfId) {
        self.check_ef(ef);
        bit_set(&mut d.ev_flags, ef.0);
        self.ss_wakeup(ef.0);
    }

    /// `seq_exit`: set `die` and wake every state set.
    pub fn exit(&self) {
        self.die.store(true, Ordering::SeqCst);
        // Leave even a thread still waiting for the first connect.
        self.ready.signal();
        self.ss_wakeup(0);
    }
}

impl<V: ProgramVars> StateSet<V> {
    /// `optTest(sp, OPT_SAFE)`.
    fn safe(&self) -> bool {
        self.prog.table.vars.safe()
    }

    fn chan(&self, ch: ChId) -> &Chan<V> {
        &self.prog.chans[ch.index()]
    }

    fn sscb(&self) -> &Sscb<V> {
        &self.prog.ss[self.index]
    }

    /// `ch->dbch`, under `dynamic`.
    fn with_dbch<R>(&self, ch: ChId, f: impl FnOnce(Option<&DbChan>) -> R) -> R {
        f(self.prog.dynamic.lock().dbch(ch))
    }

    /// `metaPtr(ch, ss)`: the state set's copy in safe mode, else the
    /// shared buffer's.
    fn with_meta<R>(&self, ch: ChId, f: impl FnOnce(&mut PvMeta) -> R) -> R {
        if self.safe() {
            f(&mut self.sscb().meta[ch.index()].lock())
        } else {
            f(&mut self.chan(ch).var_lock.lock())
        }
    }

    /// [`Self::with_meta`] of an assigned channel, `None` if `ch` is not
    /// assigned.
    fn assigned_meta<R>(&self, ch: ChId, f: impl FnOnce(&mut PvMeta) -> R) -> Option<R> {
        self.pv_assigned(ch).then(|| self.with_meta(ch, f))
    }

    /// `ch->queue`.
    fn queue(&self, ch: ChId) -> Option<&SeqQueue> {
        let def = self.chan(ch).def;
        (def.queue_size > 0).then(|| &self.prog.queues[def.queue_index as usize])
    }

    /// `ss->getReq[ch]` / `ss->putReq[ch]`.
    fn req_slot(&self, ev: PvEventType, ch: ChId) -> &ReqSlot {
        let ss = self.sscb();
        match ev {
            PvEventType::Get => &ss.get_req[ch.index()],
            PvEventType::Put | PvEventType::Monitor => &ss.put_req[ch.index()],
        }
    }

    /// Set `metaPtr(ch, ss)` of an assigned channel to an error; returns
    /// `status`.
    fn set_error(&self, ch: ChId, status: PvStat, message: CharPtr) -> PvStat {
        self.assigned_meta(ch, |m| {
            m.status = status;
            m.severity = PvSevr::Error;
            m.message = message;
        });
        status
    }

    /// `check_connected`.
    fn check_connected(&self, ch: ChId) -> PvStat {
        if self.with_dbch(ch, |d| d.is_some_and(DbChan::connected)) {
            return PvStat::Ok;
        }
        self.set_error(ch, PvStat::Disconn, message("disconnected"))
    }

    /// `completion_timeout`. C picks the text with `pvEventGet ? ...`,
    /// which is always "put" since `pvEventGet` is 0; here `ev` picks it.
    fn completion_timeout(&self, ev: PvEventType, ch: ChId) -> PvStat {
        let text = match ev {
            PvEventType::Get => "get completion failure",
            PvEventType::Put | PvEventType::Monitor => "put completion failure",
        };
        self.set_error(ch, PvStat::Timeout, message(text))
    }

    /// The pv name of an assigned channel, for an errlog line.
    fn db_name(&self, ch: ChId) -> String {
        self.with_dbch(ch, |d| {
            d.map_or_else(|| "(null)".into(), |d| d.db_name.clone())
        })
    }

    /// `check_pending`: wait out (`SYNC`) or refuse (`ASYNC`) a request
    /// of this state set still pending on `ch`.
    fn check_pending(&self, ev: PvEventType, ch: ChId, comp: CompType, tmo: f64) -> PvStat {
        let call = call_name(ev);
        let var_name = self.chan(ch).def.var_name;
        let slot = self.req_slot(ev, ch);
        match comp {
            CompType::Sync => {
                if tmo <= 0.0 {
                    errlog_sev_vprintf(
                        ErrlogSevEnum::Major,
                        format_args!(
                            "{call}({var_name},SYNC,{:.6}): user error (timeout must be positive)\n",
                            CFloat(tmo)
                        ),
                    );
                    return PvStat::Error;
                }
                let start = Instant::now();
                while slot.lock().is_some() {
                    let left = tmo - start.elapsed().as_secs_f64();
                    if self.prog.sync_wait(self.index, left) {
                        let status = self.check_connected(ch);
                        if status != PvStat::Ok {
                            return status;
                        }
                        if tmo - start.elapsed().as_secs_f64() > 0.0 {
                            continue;
                        }
                    }
                    errlog_sev_vprintf(
                        ErrlogSevEnum::Major,
                        format_args!(
                            "{call}(ss {}, var {var_name}, pv {}): failed (timeout \
                             waiting for other {call} requests to finish)\n",
                            self.sscb().def.ss_name,
                            self.db_name(ch)
                        ),
                    );
                    return self.completion_timeout(ev, ch);
                }
            }
            CompType::Async => {
                if slot.lock().is_some() {
                    errlog_sev_vprintf(
                        ErrlogSevEnum::Major,
                        format_args!(
                            "{call}(ss {}, var {var_name}, pv {}): user error \
                             (there is already a {call} pending for this channel/\
                             state set combination)\n",
                            self.sscb().def.ss_name,
                            self.db_name(ch)
                        ),
                    );
                    return PvStat::Error;
                }
            }
            CompType::Default => {}
        }
        PvStat::Ok
    }

    /// Make `req` the pending request of `slot`.
    fn new_req(&self, slot: &ReqSlot, ch: ChId) -> Arc<PvReq> {
        let req = Arc::new(PvReq { ch, ss: self.index });
        let mut pending = slot.lock();
        assert!(pending.is_none(), "check_pending left a request pending");
        *pending = Some(Arc::clone(&req));
        req
    }

    /// `wait_complete`: wait for the pending request's callback.
    fn wait_complete(&self, ev: PvEventType, ch: ChId, tmo: f64) -> PvStat {
        let slot = self.req_slot(ev, ch);
        let start = Instant::now();
        while slot.lock().is_some() {
            // Any event of this state set signals syncSem, and C waits the
            // whole tmo again after each one, so a state set with a busy
            // monitor never timed out. The deadline here is the request's.
            let left = tmo - start.elapsed().as_secs_f64();
            if !self.prog.sync_wait(self.index, left) {
                *slot.lock() = None;
                return self.completion_timeout(ev, ch);
            }
        }
        self.check_connected(ch)
    }

    /// Call the pv layer on `ch`'s pv, cloned out of `dynamic`, with
    /// [`Chan::db_count`] elements.
    fn call_pv(&self, ch: ChId, f: impl FnOnce(&PvVar, u32) -> PvStat) -> PvStat {
        match self.with_dbch(ch, |d| d.and_then(|d| d.pvid.clone())) {
            Some(pvid) => f(&pvid, self.chan(ch).db_count.load(Ordering::Relaxed)),
            // Unassigned since the caller looked, or not connected yet.
            None => PvStat::Error,
        }
    }

    /// `pv_call_failure`: `status` and the pv's message into `metaPtr`'s
    /// copy. Returns the pv's name and message.
    fn pv_call_failure(&self, ch: ChId, status: PvStat) -> (String, Option<Arc<str>>) {
        let (name, mess) = self.with_dbch(ch, |d| match d {
            Some(d) => (
                d.db_name.clone(),
                d.pvid.as_deref().and_then(PvVar::message),
            ),
            None => ("(null)".into(), None),
        });
        self.set_error(ch, status, mess.clone().map(Arc::from));
        (name, mess)
    }

    /// [`Self::pv_call_failure`] and its errlog line: `func` refused
    /// `call`'s request.
    fn call_failure(&self, call: &str, func: &str, ch: ChId, status: PvStat) {
        let (name, mess) = self.pv_call_failure(ch, status);
        errlog_sev_vprintf(
            ErrlogSevEnum::Fatal,
            format_args!(
                "{call}(var {}, pv {name}): {func}() failure: {}\n",
                self.chan(ch).def.var_name,
                mess.as_deref().unwrap_or("(null)")
            ),
        );
    }

    /// `seq_pvArrayGetComplete` / `seq_pvArrayPutComplete` over `done`.
    fn array_complete(
        &self,
        ch: ChId,
        length: u32,
        any: bool,
        complete: Option<&dyn CompleteFlags>,
        done: impl Fn(ChId) -> bool,
    ) -> bool {
        let (mut any_done, mut all_done) = (false, true);
        for n in 0..length {
            let d = done(ch.offset(n.into()));
            any_done |= d;
            all_done &= d;
            match complete {
                Some(c) => c.set_flag(n as usize, d),
                None if any == d => break,
                None => {}
            }
        }
        if any { any_done } else { all_done }
    }

    /// A user error for an unassigned channel, which safe mode takes as
    /// anonymous.
    fn not_assigned(&self, sev: ErrlogSevEnum, call: &str, ch: ChId) {
        if !self.safe() {
            errlog_sev_vprintf(
                sev,
                format_args!(
                    "{call}({}): user error (not assigned to a PV)\n",
                    self.chan(ch).def.var_name
                ),
            );
        }
    }

    /// Clear the flag `ch` is synced to, if any; with `only_if_empty`,
    /// only once `ch`'s queue is empty.
    fn clear_synced_flag(&self, ch: ChId, queue: &SeqQueue, only_if_empty: bool) {
        let mut d = self.prog.dynamic.lock();
        let ef = d.synced_to[ch.index()];
        if ef != NOEVFLAG && (!only_if_empty || queue.is_empty()) {
            bit_clear(&mut d.ev_flags, ef.0);
        }
    }

    /// `anonymous_put`: a safe-mode put to an anonymous channel, into its
    /// queue or the shared buffer.
    fn anonymous_put(&self, ch: ChId) {
        let chan = self.chan(ch);
        if let Some(queue) = self.queue(ch) {
            // pvGetQ reads an element's meta only once ch is assigned.
            let meta = chan.var_lock.lock();
            let value = (chan.def.var)(self.var()).load(chan.db_count.load(Ordering::Relaxed));
            let full = queue.put(QueueElem {
                value,
                meta: meta.clone(),
            });
            if full {
                errlog_sev_vprintf(
                    ErrlogSevEnum::Minor,
                    format_args!(
                        "pvPut on queued channel '{}' (anonymous): last queue element \
                         overwritten (queue is full)\n",
                        chan.def.var_name
                    ),
                );
            }
        } else {
            let dirtify = chan.monitored.load(Ordering::Relaxed);
            self.prog
                .ss_write_buffer(ch, BufSrc::StateSet(self.index), None, dirtify);
        }
        let mut d = self.prog.dynamic.lock();
        let ef = d.synced_to[ch.index()];
        if ef != NOEVFLAG {
            self.prog.ef_set_locked(&mut d, ef);
        }
        drop(d);
        self.prog.ss_wakeup(chan.def.event_num);
    }

    /// `seq_pvSingleMonitor`.
    fn single_monitor(&self, ch: ChId, turn_on: bool, what: &str) -> PvStat {
        let chan = self.chan(ch);
        if !self.pv_assigned(ch) {
            if self.safe() {
                // C sets it TRUE for pvStopMonitor too.
                chan.monitored.store(turn_on, Ordering::Relaxed);
                return PvStat::Ok;
            }
            errlog_sev_vprintf(
                ErrlogSevEnum::Major,
                format_args!(
                    "{what}({}): user error (not assigned to a PV)\n",
                    chan.def.var_name
                ),
            );
            return PvStat::Error;
        }
        chan.monitored.store(turn_on, Ordering::Relaxed);
        let status = self.prog.seq_camonitor(ch, turn_on);
        if status != PvStat::Ok {
            self.pv_call_failure(ch, status);
        }
        status
    }

    /// [`Self::single_monitor`] of each channel, up to the first failure.
    fn array_monitor(&self, ch: ChId, length: u32, turn_on: bool, what: &str) -> PvStat {
        (0..length)
            .map(|n| self.single_monitor(ch.offset(n.into()), turn_on, what))
            .find(|&status| status != PvStat::Ok)
            .unwrap_or(PvStat::Ok)
    }

    /// `pvPut`'s single channel of `pvPutComplete`.
    fn single_put_complete(&self, ch: ChId) -> bool {
        if !self.pv_assigned(ch) {
            self.not_assigned(ErrlogSevEnum::Major, "pvPutComplete", ch);
            return true;
        }
        if self.req_slot(PvEventType::Put, ch).lock().is_some() {
            return false;
        }
        self.check_connected(ch);
        true
    }

    /// `seqg_var`: the variables this state set sees — the instance's
    /// block, or its own copy under `+s`.
    pub fn var(&self) -> &V {
        self.prog.ss[self.index]
            .var
            .as_ref()
            .unwrap_or_else(|| self.prog.var())
    }

    /// `seq_efSet`.
    pub fn ef_set(&self, ef: EfId) {
        self.prog.ef_set(ef)
    }

    /// `seq_efTest`. In safe mode it also copies in the channels synced to
    /// the flag.
    pub fn ef_test(&self, ef: EfId) -> bool {
        self.prog.check_ef(ef);
        let d = self.prog.dynamic.lock();
        let is_set = bit_test(&d.ev_flags, ef.0);
        if self.safe() {
            self.prog.ss_read_buffer_selective(&d, self.index, ef);
        }
        is_set
    }

    /// `seq_efClear`: whether the flag was set.
    pub fn ef_clear(&self, ef: EfId) -> bool {
        self.prog.check_ef(ef);
        let mut d = self.prog.dynamic.lock();
        let is_set = bit_test(&d.ev_flags, ef.0);
        bit_clear(&mut d.ev_flags, ef.0);
        self.prog.ss_wakeup(ef.0);
        is_set
    }

    /// `seq_efTestAndClear`.
    pub fn ef_test_and_clear(&self, ef: EfId) -> bool {
        self.prog.check_ef(ef);
        let mut d = self.prog.dynamic.lock();
        let is_set = bit_test(&d.ev_flags, ef.0);
        bit_clear(&mut d.ev_flags, ef.0);
        if self.safe() {
            self.prog.ss_read_buffer_selective(&d, self.index, ef);
        }
        is_set
    }

    /// `seq_pvGet`: [`Self::pv_get_tmo`] with [`crate::DEFAULT_TIMEOUT`].
    pub fn pv_get(&self, ch: ChId, comp: CompType) -> PvStat {
        self.pv_get_tmo(ch, comp, crate::DEFAULT_TIMEOUT)
    }

    /// `seq_pvGetTmo`; `tmo` in seconds. Its return is a delivery point of
    /// [`Program::gate`].
    pub fn pv_get_tmo(&self, ch: ChId, comp: CompType, tmo: f64) -> PvStat {
        self.prog
            .gate
            .call(self.index, || self.get_tmo(ch, comp, tmo))
    }

    /// [`Self::pv_get_tmo`]'s request.
    fn get_tmo(&self, ch: ChId, comp: CompType, tmo: f64) -> PvStat {
        let chan = self.chan(ch);
        if !self.pv_assigned(ch) {
            if self.safe() {
                // An anonymous get completes at once, from the shared buffer.
                self.prog.ss_read_buffer(self.index, ch, false);
                return PvStat::Ok;
            }
            self.not_assigned(ErrlogSevEnum::Major, "pvGet", ch);
            return PvStat::Error;
        }
        let comp = match comp {
            CompType::Default if self.prog.table.options.test(ProgOptions::ASYNC) => {
                CompType::Async
            }
            CompType::Default => CompType::Sync,
            comp => comp,
        };
        let status = self.check_pending(PvEventType::Get, ch, comp, tmo);
        if status != PvStat::Ok {
            return status;
        }
        let slot = self.req_slot(PvEventType::Get, ch);
        let req = self.new_req(slot, ch);
        let prog = Arc::clone(&self.prog);
        let status = self.call_pv(ch, |pvid, count| {
            pvid.get_callback(chan.def.var_type.get_type(), count, move |ev| {
                prog.seq_get_handler(&req, ev)
            })
        });
        if status != PvStat::Ok {
            self.call_failure("pvGet", "pvVarGetCallback", ch, status);
            *slot.lock() = None;
            self.check_connected(ch);
            return status;
        }
        if comp == CompType::Sync {
            self.prog.pv_sys.flush();
            let status = self.wait_complete(PvEventType::Get, ch, tmo);
            if status != PvStat::Ok {
                return status;
            }
            if self.safe() {
                self.prog.ss_read_buffer(self.index, ch, false);
            }
        }
        PvStat::Ok
    }

    /// `seq_pvGetQ`: take the oldest queued value into the variable;
    /// false when the queue was empty.
    pub fn pv_get_q(&self, ch: ChId) -> bool {
        let chan = self.chan(ch);
        let Some(queue) = self.queue(ch) else {
            errlog_sev_vprintf(
                ErrlogSevEnum::Major,
                format_args!("pvGetQ({}): user error (not queued)\n", chan.def.var_name),
            );
            return false;
        };
        let elem = queue.get();
        if let Some(elem) = &elem {
            // getq_cp: an assigned channel takes the element's alarm and
            // stamp but keeps its message.
            let assigned = self.pv_assigned(ch);
            let take = |m: &mut PvMeta| {
                if assigned {
                    m.status = elem.meta.status;
                    m.severity = elem.meta.severity;
                    m.timestamp = elem.meta.timestamp;
                }
            };
            let count = chan.db_count.load(Ordering::Relaxed);
            let cell = (chan.def.var)(self.var());
            if self.safe() {
                take(&mut self.sscb().meta[ch.index()].lock());
                cell.store(&elem.value, count);
            } else {
                // The shared buffer: value and meta under one var_lock hold.
                let mut meta = chan.var_lock.lock();
                take(&mut meta);
                cell.store(&elem.value, count);
            }
        }
        self.clear_synced_flag(ch, queue, true);
        elem.is_some()
    }

    /// `seq_pvFlushQ`.
    pub fn pv_flush_q(&self, ch: ChId) {
        let Some(queue) = self.queue(ch) else {
            errlog_sev_vprintf(
                ErrlogSevEnum::Minor,
                format_args!(
                    "pvFlushQ({}): user error (not queued)\n",
                    self.chan(ch).def.var_name
                ),
            );
            return;
        };
        queue.flush();
        self.clear_synced_flag(ch, queue, false);
    }

    /// `seq_pvFreeQ`, which `seqCom.h` defines as `seq_pvFlushQ`.
    pub fn pv_free_q(&self, ch: ChId) {
        self.pv_flush_q(ch)
    }

    /// `seq_pvPut`: [`Self::pv_put_tmo`] with [`crate::DEFAULT_TIMEOUT`].
    pub fn pv_put(&self, ch: ChId, comp: CompType) -> PvStat {
        self.pv_put_tmo(ch, comp, crate::DEFAULT_TIMEOUT)
    }

    /// `seq_pvPutTmo`; `tmo` in seconds. Its return is a delivery point of
    /// [`Program::gate`], which the put's own monitor does not pass.
    pub fn pv_put_tmo(&self, ch: ChId, comp: CompType, tmo: f64) -> PvStat {
        self.prog
            .gate
            .call(self.index, || self.put_tmo(ch, comp, tmo))
    }

    /// [`Self::pv_put_tmo`]'s request.
    fn put_tmo(&self, ch: ChId, comp: CompType, tmo: f64) -> PvStat {
        let chan = self.chan(ch);
        if !self.pv_assigned(ch) {
            if self.safe() {
                self.anonymous_put(ch);
                return PvStat::Ok;
            }
            self.not_assigned(ErrlogSevEnum::Major, "pvPut", ch);
            return PvStat::Error;
        }
        let status = self.check_connected(ch);
        if status != PvStat::Ok {
            return status;
        }
        // +a was never honoured for a put: DEFAULT is fire and forget.
        let status = self.check_pending(PvEventType::Put, ch, comp, tmo);
        if status != PvStat::Ok {
            return status;
        }
        let ty = chan.def.var_type.put_type();
        let cell = (chan.def.var)(self.var());
        if comp == CompType::Default {
            let status = self.call_pv(ch, |pvid, count| {
                pvid.put_no_block(ty, count, cell.load(count))
            });
            if status != PvStat::Ok {
                self.call_failure("pvPut", "pvVarPutNoBlock", ch, status);
            }
            return status;
        }
        let slot = self.req_slot(PvEventType::Put, ch);
        let req = self.new_req(slot, ch);
        let prog = Arc::clone(&self.prog);
        let status = self.call_pv(ch, |pvid, count| {
            pvid.put_callback(ty, count, cell.load(count), move |ev| {
                prog.seq_put_handler(&req, ev)
            })
        });
        if status != PvStat::Ok {
            self.call_failure("pvPut", "pvVarPutCallback", ch, status);
            *slot.lock() = None;
            self.check_connected(ch);
            return status;
        }
        if comp == CompType::Sync {
            self.prog.pv_sys.flush();
            return self.wait_complete(PvEventType::Put, ch, tmo);
        }
        PvStat::Ok
    }

    /// `seq_pvGetComplete`. In safe mode a completed get also copies the
    /// value in.
    pub fn pv_get_complete(&self, ch: ChId) -> bool {
        if !self.pv_assigned(ch) {
            self.not_assigned(ErrlogSevEnum::Major, "pvGetComplete", ch);
            return true;
        }
        if self.req_slot(PvEventType::Get, ch).lock().is_some() {
            return false;
        }
        if self.check_connected(ch) == PvStat::Ok && self.safe() {
            self.prog.ss_read_buffer(self.index, ch, false);
        }
        true
    }

    /// `seq_pvPutComplete(ss, ch, length, any, complete)`.
    pub fn pv_put_complete(
        &self,
        ch: ChId,
        length: u32,
        any: bool,
        complete: Option<&dyn CompleteFlags>,
    ) -> bool {
        self.pv_array_put_complete(ch, length, any, complete)
    }

    /// `seq_pvGetCancel`.
    pub fn pv_get_cancel(&self, ch: ChId) {
        if !self.pv_assigned(ch) {
            self.not_assigned(ErrlogSevEnum::Minor, "pvGetCancel", ch);
            return;
        }
        *self.req_slot(PvEventType::Get, ch).lock() = None;
    }

    /// `seq_pvPutCancel`.
    pub fn pv_put_cancel(&self, ch: ChId) {
        if !self.pv_assigned(ch) {
            self.not_assigned(ErrlogSevEnum::Minor, "pvPutCancel", ch);
            return;
        }
        *self.req_slot(PvEventType::Put, ch).lock() = None;
    }

    /// `seq_pvAssignSubst`: `pvAssign` after macro substitution.
    pub fn pv_assign_subst(&self, ch: ChId, name: impl CSrc) -> PvStat {
        let name = String::from_utf8_lossy(&name.to_c_vec()).into_owned();
        self.pv_assign(ch, &*self.prog.mac_eval(&name, 100))
    }

    /// `seq_pvAssign`; `""` unassigns.
    pub fn pv_assign(&self, ch: ChId, name: impl CSrc) -> PvStat {
        let name = if name.is_null() {
            String::new()
        } else {
            String::from_utf8_lossy(&name.to_c_vec()).into_owned()
        };
        let chan = self.chan(ch);
        let mut status = PvStat::Ok;
        let mut replaced = false;
        let mut d = self.prog.dynamic.lock();
        // C installs its channel after relocking without looking again, so
        // a pvAssign racing this one leaked a live channel; here the slot
        // is empty under the hold that fills it.
        // The monitor dies with the pv; monitored stays, as the new channel
        // inherits it.
        while let Some(old) = d.unassign_dbch(ch) {
            replaced = true;
            drop(d);
            let destroyed = old.pvid.as_deref().map_or(PvStat::Ok, PvVar::destroy);
            d = self.prog.dynamic.lock();
            if destroyed != PvStat::Ok {
                status = destroyed;
                errlog_sev_vprintf(
                    ErrlogSevEnum::Fatal,
                    format_args!(
                        "pvAssign(var {}, pv {}): pvVarDestroy() failure: {}\n",
                        chan.def.var_name,
                        old.db_name,
                        old.pvid
                            .as_deref()
                            .and_then(PvVar::message)
                            .as_deref()
                            .unwrap_or("(null)")
                    ),
                );
            }
        }
        if name.is_empty() {
            chan.db_count.store(chan.def.count, Ordering::Relaxed);
            return status;
        }
        let created = self.prog.pv_var_create(&mut d, ch, &name, |d, pvid| {
            // C reuses a replaced channel's dbCount and meta data and
            // callocs a new one's.
            if !replaced {
                chan.db_count.store(0, Ordering::Relaxed);
                *chan.var_lock.lock() = PvMeta::zeroed();
            }
            d.assign_dbch(ch, name.clone(), pvid);
        });
        match created {
            Ok(()) => PvStat::Ok,
            Err((status, mess)) => {
                // C frees the channel and leaves ch->dbch pointing at it.
                chan.db_count.store(chan.def.count, Ordering::Relaxed);
                errlog_sev_vprintf(
                    ErrlogSevEnum::Fatal,
                    format_args!(
                        "pvAssign(var {}, pv {name}): pvVarCreate() failure: {mess}\n",
                        chan.def.var_name
                    ),
                );
                status
            }
        }
    }

    /// `seq_pvMonitor`.
    pub fn pv_monitor(&self, ch: ChId) -> PvStat {
        self.single_monitor(ch, true, "pvMonitor")
    }

    /// `seq_pvStopMonitor`.
    pub fn pv_stop_monitor(&self, ch: ChId) -> PvStat {
        self.single_monitor(ch, false, "pvStopMonitor")
    }

    /// `seq_pvSync`; [`crate::NOEVFLAG`] removes the sync.
    pub fn pv_sync(&self, ch: ChId, ef: EfId) {
        self.pv_array_sync(ch, 1, ef)
    }

    /// `seq_pvName`: the name after substitution, null when unassigned.
    pub fn pv_name(&self, ch: ChId) -> CharPtr {
        self.with_dbch(ch, |d| d.map(|d| Arc::from(d.db_name.as_bytes())))
    }

    /// `seq_pvCount`.
    pub fn pv_count(&self, ch: ChId) -> u32 {
        self.chan(ch).db_count.load(Ordering::Relaxed)
    }

    /// `seq_pvStatus`.
    pub fn pv_status(&self, ch: ChId) -> PvStat {
        self.assigned_meta(ch, |m| m.status).unwrap_or(PvStat::Ok)
    }

    /// `seq_pvSeverity`.
    pub fn pv_severity(&self, ch: ChId) -> PvSevr {
        self.assigned_meta(ch, |m| m.severity).unwrap_or(PvSevr::OK)
    }

    /// `seq_pvTimeStamp`: now, for an unassigned channel.
    pub fn pv_time_stamp(&self, ch: ChId) -> WallTime {
        self.assigned_meta(ch, |m| m.timestamp)
            .unwrap_or_else(WallTime::now)
    }

    /// `seq_pvMessage`: `""` for an unassigned channel.
    pub fn pv_message(&self, ch: ChId) -> CharPtr {
        self.assigned_meta(ch, |m| m.message.clone())
            .unwrap_or_else(|| Some(Arc::from(&b""[..])))
    }

    /// `seq_pvAssigned`.
    pub fn pv_assigned(&self, ch: ChId) -> bool {
        self.with_dbch(ch, |d| d.is_some())
    }

    /// `seq_pvConnected`: in safe mode an anonymous channel counts as
    /// connected.
    pub fn pv_connected(&self, ch: ChId) -> bool {
        self.with_dbch(ch, |d| d.map_or(self.safe(), DbChan::connected))
    }

    /// `seq_pvIndex`: the channel itself.
    pub fn pv_index(&self, ch: ChId) -> ChId {
        ch
    }

    /// `seq_pvFlush`.
    pub fn pv_flush(&self) {
        self.prog.pv_sys.flush();
    }

    /// `seq_delay`: whether `delay` seconds have passed since the state
    /// was entered; arms the wakeup when not.
    pub fn delay(&self, delay: f64) -> bool {
        let now = pv_time_get_current_double();
        let ss = self.sscb();
        let time_expired = *ss.time_entered.lock() + delay;
        let expired = time_expired <= now;
        if !expired {
            let mut wakeup = ss.wakeup_time.lock();
            if time_expired < *wakeup {
                *wakeup = time_expired;
            }
        }
        expired
    }

    /// `seq_macValueGet`: null when the macro is not defined.
    pub fn mac_value_get(&self, name: impl CSrc) -> CharPtr {
        // Every macro name is UTF-8, so other bytes name none.
        let name = name.to_c_vec();
        let value = self.prog.macros.value(std::str::from_utf8(&name).ok()?)?;
        Some(Arc::from(value.as_bytes()))
    }

    /// `seq_exit`: stop every state set of the program after this
    /// action.
    pub fn exit(&self) {
        self.prog.exit()
    }

    /// `seq_pvChannelCount`.
    pub fn pv_channel_count(&self) -> u32 {
        self.prog.chans.len() as u32
    }

    /// `seq_pvConnectCount`.
    pub fn pv_connect_count(&self) -> u32 {
        self.prog.dynamic.lock().connect_count()
    }

    /// `seq_pvAssignCount`.
    pub fn pv_assign_count(&self) -> u32 {
        self.prog.dynamic.lock().assign_count()
    }

    /// `seq_optGet`: the option named by the first character of `opt`.
    pub fn opt_get(&self, opt: impl CSrc) -> bool {
        let options = self.prog.table.options;
        match opt.with_c_bytes(|b| b.first().copied()) {
            Some(b'a') => options.test(ProgOptions::ASYNC),
            Some(b'c') => options.test(ProgOptions::CONN),
            Some(b'd') => options.test(ProgOptions::DEBUG),
            Some(b'e') => options.test(ProgOptions::NEWEF),
            Some(b'r') => self.prog.table.vars.reentrant(),
            Some(b's') => self.safe(),
            _ => false,
        }
    }

    /// `seq_pvArrayGetComplete`.
    pub fn pv_array_get_complete(
        &self,
        ch: ChId,
        length: u32,
        any: bool,
        complete: Option<&dyn CompleteFlags>,
    ) -> bool {
        self.array_complete(ch, length, any, complete, |c| self.pv_get_complete(c))
    }

    /// `seq_pvArrayPutComplete`.
    pub fn pv_array_put_complete(
        &self,
        ch: ChId,
        length: u32,
        any: bool,
        complete: Option<&dyn CompleteFlags>,
    ) -> bool {
        self.array_complete(ch, length, any, complete, |c| self.single_put_complete(c))
    }

    /// `seq_pvArrayGetCancel`.
    pub fn pv_array_get_cancel(&self, ch: ChId, length: u32) {
        for n in 0..length {
            self.pv_get_cancel(ch.offset(n.into()));
        }
    }

    /// `seq_pvArrayPutCancel`.
    pub fn pv_array_put_cancel(&self, ch: ChId, length: u32) {
        for n in 0..length {
            self.pv_put_cancel(ch.offset(n.into()));
        }
    }

    /// `seq_pvArrayMonitor`.
    pub fn pv_array_monitor(&self, ch: ChId, length: u32) -> PvStat {
        self.array_monitor(ch, length, true, "pvArrayMonitor")
    }

    /// `seq_pvArrayStopMonitor`.
    pub fn pv_array_stop_monitor(&self, ch: ChId, length: u32) -> PvStat {
        self.array_monitor(ch, length, false, "pvArrayStopMonitor")
    }

    /// `seq_pvArraySync`.
    pub fn pv_array_sync(&self, ch: ChId, length: u32, ef: EfId) {
        assert!(
            ef.0 <= self.prog.table.num_ev_flags,
            "event flag {} is not one of 0..={}",
            ef.0,
            self.prog.table.num_ev_flags
        );
        let mut guard = self.prog.dynamic.lock();
        let d = &mut *guard;
        for n in 0..length {
            let this = ch.offset(n.into());
            let old = d.synced_to[this.index()];
            if old == ef {
                continue;
            }
            if old != NOEVFLAG {
                d.synced_chans[old.0 as usize].retain(|&c| c != this);
            }
            d.synced_to[this.index()] = ef;
            if ef != NOEVFLAG {
                // C pushes onto the head of its list.
                d.synced_chans[ef.0 as usize].insert(0, this);
            }
        }
    }

    /// `seq_pvArrayConnected`.
    pub fn pv_array_connected(&self, ch: ChId, length: u32) -> bool {
        (0..length).all(|n| self.pv_connected(ch.offset(n.into())))
    }
}

#[cfg(test)]
mod tests;
