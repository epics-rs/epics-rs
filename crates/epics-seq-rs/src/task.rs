//! The state-set threads and the shared-buffer copies (`seq_task.c`).

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Wake, Waker};

use epics_base_rs::runtime::log::{ErrlogSevEnum, errlog_sev_vprintf};
use epics_base_rs::runtime::task::{MandatoryThread, StackSizeClass, StackSpec, ThreadPriority};
use epics_base_rs::runtime::taskwd::{CheckIn, taskwd_insert};
use epics_base_rs::types::EpicsValue;

use crate::program::{ProgOptions, StateOptions};
use crate::pv::pv_time_get_current_double;
use crate::pvt::{ProgDyn, Program, PvMeta, seq_add_prog, seq_del_prog, seq_find_prog};
use crate::seq::SeqThreadId;
use crate::state_set::StateSet;
use crate::types::{ChId, EfId, NOEVFLAG, PvStat, bit_test};
use crate::var::ProgramVars;

/// What [`Program::ss_write_buffer`] copies into the shared buffer: C's
/// `val`, which is a pv's value (`seq_ca.c`) or a state set's variable
/// (`seq_pvPut`, `seq_if.c`).
pub enum BufSrc<'a> {
    Pv(&'a EpicsValue),
    /// State set `ss`'s view of the channel's variable: its own copy
    /// under `+s`, the shared buffer itself otherwise.
    StateSet(usize),
}

impl<V: ProgramVars> Program<V> {
    /// `sequencer`: the program's first thread, from `seqAddProg` to its
    /// end. `name` is that thread's, which the other state sets' extend.
    pub(crate) fn sequencer(self: Arc<Self>, name: &str) {
        let _ = self.ss[0].thread_id.set(SeqThreadId::current());
        seq_add_prog(&self);
        self.pv_sys.attach();
        'run: {
            if !self.wait_for_pini() {
                break 'run;
            }
            if self.seq_connect(self.table.options.test(ProgOptions::CONN)) != PvStat::Ok {
                break 'run;
            }
            if self.table.vars.safe() {
                // The first monitor event of an anonymous pv.
                let mut d = self.dynamic.lock();
                for n in 0..self.chans.len() {
                    let ef = d.synced_to[n];
                    if ef != NOEVFLAG && d.dbch(ChId(n as u32)).is_none() {
                        self.ef_set_locked(&mut d, ef);
                    }
                }
            }
            let ss0 = StateSet {
                prog: Arc::clone(&self),
                index: 0,
            };
            if let Some(entry) = self.table.entry_func {
                entry(&ss0);
            }
            // `epicsThreadGetName` into C's 42-byte buffer.
            let mut len = name.len().min(41);
            while !name.is_char_boundary(len) {
                len -= 1;
            }
            for nss in 1..self.ss.len() {
                let ss_name = format!("{}_{nss}", &name[..len]);
                let prog = Arc::clone(&self);
                let thread_name = ss_name.clone();
                let spawned = MandatoryThread::new(
                    ss_name,
                    ThreadPriority::Custom(self.thread_priority),
                    StackSizeClass::Big,
                )
                .try_spawn_sized(StackSpec::Bytes(self.stack_size as usize), move || {
                    prog.ss_entry(nss, &thread_name)
                });
                if spawned.is_err() {
                    // C goes on without the state set and then waits for
                    // its `dead` forever.
                    errlog_sev_vprintf(
                        ErrlogSevEnum::Fatal,
                        format_args!("seq: epicsThreadCreate failed"),
                    );
                    self.ss[nss].dead.signal();
                }
            }
            self.ss_entry(0, name);
            for ss in &self.ss[1..] {
                ss.dead.wait();
            }
            if let Some(exit) = self.table.exit_func {
                exit(&ss0);
            }
        }
        self.seq_disconnect();
        seq_del_prog(&*self);
        errlog_sev_vprintf(
            ErrlogSevEnum::Info,
            format_args!(
                "Instance {} of sequencer program \"{}\" terminated\n",
                self.instance.load(Ordering::Relaxed),
                self.table.prog_name
            ),
        );
    }

    /// The start-up gate `epics_base_rs::server::snl::spawn_program` gave a
    /// ported program: nothing connects until `db`'s PINI pass is done, so
    /// a first read of a `PINI=YES` record sees it processed and a first
    /// write is not undone by the pass. False when the program is stopped
    /// first: the gate waits on the first state set's `syncSem`, which
    /// `seq_exit` signals, and a stray signal left there costs one more
    /// pass over a `when`, as any pv event does.
    fn wait_for_pini(self: &Arc<Self>) -> bool {
        struct Wakeup<V: 'static>(Arc<Program<V>>);
        impl<V: ProgramVars> Wake for Wakeup<V> {
            fn wake(self: Arc<Self>) {
                self.0.ss[0].sync_sem.signal();
            }
        }
        let waker = Waker::from(Arc::new(Wakeup(Arc::clone(self))));
        let mut cx = Context::from_waker(&waker);
        let mut pini = pin!(self.db.wait_for_pini());
        loop {
            if self.die.load(Ordering::Acquire) {
                return false;
            }
            if pini.as_mut().poll(&mut cx).is_ready() {
                return true;
            }
            self.ss[0].sync_sem.wait();
        }
    }

    /// `ss_entry`: state set `nss`'s loop on its thread `name`.
    fn ss_entry(self: &Arc<Self>, nss: usize, name: &str) {
        let ss = &self.ss[nss];
        if nss != 0 {
            let _ = ss.thread_id.set(SeqThreadId::current());
            self.pv_sys.attach();
        }
        let watched = taskwd_insert(name, CheckIn::Unbounded, None);
        let safe = self.table.vars.safe();
        if safe {
            self.ss_read_all_buffer(nss);
        }
        let env = StateSet {
            prog: Arc::clone(self),
            index: nss,
        };
        let mut cur = 0;
        let mut prev = None;
        ss.current_state.store(0, Ordering::Relaxed);
        ss.next_state.store(-1, Ordering::Relaxed);
        ss.prev_state.store(-1, Ordering::Relaxed);
        'run: loop {
            let st = &ss.def.states[cur];
            *ss.mask.lock() = st.event_mask;
            if let Some(entry) = st.entry_func
                && (prev != Some(cur) || st.options.test(StateOptions::DOENTRYFROMSELF))
            {
                entry(&env);
            }
            self.pv_sys.flush();
            // A `when` is tested at least once in a state just entered.
            ss.sync_sem.signal();
            let mut now = pv_time_get_current_double();
            if prev != Some(cur) || !st.options.test(StateOptions::NORESETTIMERS) {
                *ss.time_entered.lock() = now;
            }
            *ss.wakeup_time.lock() = f64::INFINITY;
            let mut trans = 0;
            let mut next = 0;
            loop {
                let timeout = *ss.wakeup_time.lock() - now;
                self.sync_wait(nss, timeout);
                if self.die.load(Ordering::Acquire) {
                    break 'run;
                }
                if safe {
                    self.ss_read_all_buffer(nss);
                }
                *ss.wakeup_time.lock() = f64::INFINITY;
                if (st.event_func)(&env, &mut trans, &mut next) {
                    ss.next_state.store(next as i32, Ordering::Relaxed);
                    if !self.table.options.test(ProgOptions::NEWEF) {
                        let mut d = self.dynamic.lock();
                        for (flags, mask) in d.ev_flags.iter_mut().zip(st.event_mask) {
                            *flags &= !mask;
                        }
                    }
                    break;
                }
                now = pv_time_get_current_double();
            }
            (st.action_func)(&env, trans, &mut next);
            ss.next_state.store(next as i32, Ordering::Relaxed);
            if self.die.load(Ordering::Acquire) {
                break;
            }
            if let Some(exit) = st.exit_func
                && (cur != next || st.options.test(StateOptions::DOEXITTOSELF))
            {
                exit(&env);
            }
            prev = Some(cur);
            ss.prev_state.store(cur as i32, Ordering::Relaxed);
            cur = next;
            ss.current_state.store(cur as i32, Ordering::Relaxed);
        }
        drop(watched);
        if nss != 0 {
            ss.dead.signal();
        }
    }

    /// `epicsEventWaitWithTimeout` on state set `nss`'s `syncSem`, a block
    /// point of [`Program::gate`] when it sleeps and a pass when it finds
    /// `syncSem` raised or has no time to wait.
    pub(crate) fn sync_wait(&self, nss: usize, timeout: f64) -> bool {
        let sem = &self.ss[nss].sync_sem;
        if sem.try_wait() {
            self.gate.pass(nss);
            true
        } else if timeout > 0.0 {
            self.gate.block(nss, || sem.wait_timeout(timeout))
        } else {
            self.gate.pass(nss);
            false
        }
    }

    /// `ss_read_all_buffer`.
    fn ss_read_all_buffer(&self, nss: usize) {
        for n in 0..self.chans.len() {
            self.ss_read_buffer(nss, ChId(n as u32), true);
        }
    }

    /// `ss_wakeup`: signal each state set whose current mask has
    /// `event_num`, or every one for 0. Takes only each [`Sscb`]'s `mask`,
    /// so a caller may hold `dynamic` (C's `sp->lock` is recursive here).
    ///
    /// [`Sscb`]: crate::pvt::Sscb
    pub fn ss_wakeup(&self, event_num: u32) {
        for ss in self.ss.iter() {
            let mask = ss.mask.lock();
            // An empty mask is C's NULL one, before the first state.
            if event_num == 0 || (!mask.is_empty() && bit_test(&mask, event_num)) {
                ss.sync_sem.signal();
            }
        }
    }

    /// `ss_read_buffer`: copy channel `ch`'s shared value and meta into
    /// state set `ss`'s, unless `dirty_only` and it is clean. Takes
    /// `var_lock`.
    pub fn ss_read_buffer(&self, ss: usize, ch: ChId, dirty_only: bool) {
        let sscb = &self.ss[ss];
        let var = sscb
            .var
            .as_ref()
            .expect("a state set has a buffer only under +s");
        let n = ch.index();
        if dirty_only && !sscb.dirty[n].load(Ordering::Relaxed) {
            return;
        }
        let chan = &self.chans[n];
        let meta = chan.var_lock.lock();
        let count = chan.db_count.load(Ordering::Relaxed);
        (chan.def.var)(var).copy_from((chan.def.var)(self.var()), count);
        // C copies an assigned channel's only; an anonymous one's is never
        // read, as `seq_if.c` answers for it without its meta.
        *sscb.meta[n].lock() = meta.clone();
        sscb.dirty[n].store(false, Ordering::Relaxed);
    }

    /// `ss_read_buffer_selective`: [`Self::ss_read_buffer`] with
    /// `dirty_only` for every channel synced to `ef`. `d` is the caller's
    /// hold on `dynamic`, which the synced lists need.
    pub fn ss_read_buffer_selective(&self, d: &ProgDyn, ss: usize, ef: EfId) {
        for &ch in &d.synced_chans[ef.0 as usize] {
            self.ss_read_buffer(ss, ch, true);
        }
    }

    /// `ss_write_buffer`: store `src` ([`Chan::db_count`] elements) and,
    /// when given, `meta` into channel `ch`'s shared buffer under one
    /// `var_lock` hold; under `+s` with `dirtify`, mark it dirty for every
    /// state set. A caller may hold `dynamic`.
    ///
    /// [`Chan::db_count`]: crate::pvt::Chan::db_count
    pub fn ss_write_buffer(&self, ch: ChId, src: BufSrc<'_>, meta: Option<PvMeta>, dirtify: bool) {
        let n = ch.index();
        let chan = &self.chans[n];
        let mut shared_meta = chan.var_lock.lock();
        let count = chan.db_count.load(Ordering::Relaxed);
        let buf = (chan.def.var)(self.var());
        match src {
            BufSrc::Pv(value) => buf.store(value, count),
            // Without `+s` the state set's variable is the buffer itself.
            BufSrc::StateSet(ss) => {
                if let Some(var) = &self.ss[ss].var {
                    buf.copy_from((chan.def.var)(var), count);
                }
            }
        }
        // Only `seq_ca.c` passes meta, for an assigned channel, where C's
        // `ch->dbch && meta` holds.
        if let Some(meta) = meta {
            *shared_meta = meta;
        }
        if dirtify && self.table.vars.safe() {
            for ss in self.ss.iter() {
                ss.dirty[n].store(true, Ordering::Relaxed);
            }
        }
    }
}

/// `seqStop`: `seq_exit` on the program running thread `tid`, and return.
/// It does not wait: the first state set waits for the others' `dead`
/// events and then cleans up, as C's does (`seq_task.c:107`).
pub fn seq_stop(tid: SeqThreadId) {
    if let Some(prog) = seq_find_prog(tid) {
        prog.stop();
    }
}

#[cfg(test)]
mod tests {
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::types::WallTime;

    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32};

    use crate::program::VarModel;
    use crate::seq::fixture::{
        Vars, build, build_on, chan, hold_cac_event, program, table, w_chan,
    };
    use crate::types::{PvSevr, PvStat, SeqMask, bit_set};

    const N: ChId = ChId(0);
    const W: ChId = ChId(1);

    /// `n` on "a", synced to flag 1, and anonymous `w`.
    fn prog(vars: VarModel<Vars>, num_ss: usize) -> Arc<Program<Vars>> {
        let n = crate::program::SeqChan {
            ef_id: EfId(1),
            ..chan(Some("a"), true)
        };
        build(table(vec![n, w_chan(None)], vars, "", num_ss))
    }

    fn mask_with(bit: u32) -> &'static [SeqMask] {
        let mut words = vec![0; 1];
        bit_set(&mut words, bit);
        Box::leak(words.into_boxed_slice())
    }

    fn woken(p: &Program<Vars>) -> Vec<bool> {
        p.ss.iter()
            .map(|ss| ss.sync_sem.wait_timeout(0.0))
            .collect()
    }

    fn meta(secs: u64) -> PvMeta {
        PvMeta {
            timestamp: WallTime::from_unix(secs, 0),
            status: PvStat::Hihi,
            severity: PvSevr::Major,
            message: crate::types::CharPtr::default(),
        }
    }

    fn ss_w(p: &Program<Vars>, ss: usize) -> [f64; 4] {
        p.ss[ss].var.as_ref().unwrap().w.get()
    }

    fn dirty(p: &Program<Vars>, ch: ChId) -> Vec<bool> {
        p.ss.iter()
            .map(|ss| ss.dirty[ch.index()].load(Ordering::Relaxed))
            .collect()
    }

    #[test]
    fn event_0_wakes_every_state_set_even_before_its_first_state() {
        let p = prog(VarModel::Reentrant, 2);
        *p.ss[1].mask.lock() = mask_with(4);
        p.ss_wakeup(0);
        assert_eq!(woken(&p), [true, true]);
    }

    #[test]
    fn an_event_wakes_only_the_state_sets_whose_mask_has_it() {
        let p = prog(VarModel::Reentrant, 3);
        *p.ss[0].mask.lock() = mask_with(3);
        *p.ss[1].mask.lock() = mask_with(4);
        p.ss_wakeup(3);
        assert_eq!(woken(&p), [true, false, false]);
    }

    #[test]
    fn a_pv_value_moves_db_count_elements() {
        let p = prog(VarModel::Reentrant, 1);
        p.ss_write_buffer(N, BufSrc::Pv(&EpicsValue::Long(5)), None, false);
        assert_eq!(p.var().n.get(), 7, "dbCount 0 before connect");
        p.chans[0].db_count.store(1, Ordering::Relaxed);
        p.ss_write_buffer(N, BufSrc::Pv(&EpicsValue::Long(5)), None, false);
        assert_eq!(p.var().n.get(), 5);
        p.chans[1].db_count.store(2, Ordering::Relaxed);
        let value = EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0].into());
        p.ss_write_buffer(W, BufSrc::Pv(&value), None, false);
        assert_eq!(p.var().w.get(), [1.0, 2.0, 0.0, 0.0]);
    }

    #[test]
    fn meta_is_written_only_when_given() {
        let p = prog(VarModel::Reentrant, 1);
        p.ss_write_buffer(N, BufSrc::Pv(&EpicsValue::Long(5)), Some(meta(9)), false);
        p.ss_write_buffer(N, BufSrc::Pv(&EpicsValue::Long(6)), None, false);
        let shared = p.chans[0].var_lock.lock();
        assert_eq!(shared.timestamp, WallTime::from_unix(9, 0));
        assert_eq!(
            (shared.status, shared.severity),
            (PvStat::Hihi, PvSevr::Major)
        );
    }

    #[test]
    fn dirtify_under_safe_mode_marks_every_state_set() {
        let p = prog(VarModel::Safe, 2);
        p.ss_write_buffer(W, BufSrc::Pv(&EpicsValue::Double(1.0)), None, false);
        assert_eq!(dirty(&p, W), [false, false]);
        p.ss_write_buffer(W, BufSrc::Pv(&EpicsValue::Double(1.0)), None, true);
        assert_eq!(dirty(&p, W), [true, true]);
        assert_eq!(dirty(&p, N), [false, false]);
    }

    /// Without `+s` there is nothing to mark.
    #[test]
    fn dirtify_without_safe_mode_marks_nothing() {
        let p = prog(VarModel::Reentrant, 2);
        p.ss_write_buffer(W, BufSrc::Pv(&EpicsValue::Double(1.0)), None, true);
        assert!(p.ss.iter().all(|ss| ss.dirty.is_empty()));
    }

    #[test]
    fn a_state_set_put_copies_its_own_variable_under_safe_mode() {
        let p = prog(VarModel::Safe, 2);
        p.ss[1].var.as_ref().unwrap().w.set([4.0, 3.0, 2.0, 1.0]);
        p.ss_write_buffer(W, BufSrc::StateSet(1), None, true);
        assert_eq!(p.var().w.get(), [4.0, 3.0, 2.0, 1.0]);
        assert_eq!(dirty(&p, W), [true, true]);
    }

    #[test]
    fn a_state_set_put_without_safe_mode_leaves_the_buffer() {
        let p = prog(VarModel::Reentrant, 1);
        p.var().w.set([4.0, 3.0, 2.0, 1.0]);
        p.ss_write_buffer(W, BufSrc::StateSet(0), None, false);
        assert_eq!(p.var().w.get(), [4.0, 3.0, 2.0, 1.0]);
    }

    #[test]
    fn a_dirty_only_read_skips_a_clean_channel() {
        let p = prog(VarModel::Safe, 1);
        p.var().w.set([1.0; 4]);
        p.ss_read_buffer(0, W, true);
        assert_eq!(ss_w(&p, 0), [0.0; 4]);
        p.ss_read_buffer(0, W, false);
        assert_eq!(ss_w(&p, 0), [1.0; 4]);
    }

    #[test]
    fn a_read_takes_value_and_meta_and_cleans_the_channel() {
        let p = prog(VarModel::Safe, 2);
        p.chans[0].db_count.store(1, Ordering::Relaxed);
        p.ss_write_buffer(N, BufSrc::Pv(&EpicsValue::Long(5)), Some(meta(9)), true);
        p.ss_read_buffer(0, N, true);
        assert_eq!(p.ss[0].var.as_ref().unwrap().n.get(), 5);
        assert_eq!(p.ss[0].meta[0].lock().timestamp, WallTime::from_unix(9, 0));
        assert_eq!(dirty(&p, N), [false, true]);
        assert_eq!(p.ss[1].var.as_ref().unwrap().n.get(), 7);
    }

    /// The elements past `dbCount` are the state set's own.
    #[test]
    fn a_read_moves_db_count_elements() {
        let p = prog(VarModel::Safe, 1);
        p.var().w.set([1.0; 4]);
        p.ss[0].var.as_ref().unwrap().w.set([9.0; 4]);
        p.chans[1].db_count.store(2, Ordering::Relaxed);
        p.ss_read_buffer(0, W, false);
        assert_eq!(ss_w(&p, 0), [1.0, 1.0, 9.0, 9.0]);
    }

    #[test]
    fn a_selective_read_takes_the_dirty_channels_synced_to_the_flag() {
        let p = prog(VarModel::Safe, 1);
        p.chans[0].db_count.store(1, Ordering::Relaxed);
        p.ss_write_buffer(N, BufSrc::Pv(&EpicsValue::Long(5)), None, true);
        p.ss_write_buffer(W, BufSrc::Pv(&EpicsValue::Double(2.0)), None, true);
        let d = p.dynamic.lock();
        p.ss_read_buffer_selective(&d, 0, EfId(2));
        assert_eq!(p.ss[0].var.as_ref().unwrap().n.get(), 7);
        p.ss_read_buffer_selective(&d, 0, EfId(1));
        assert_eq!(p.ss[0].var.as_ref().unwrap().n.get(), 5);
        assert_eq!(ss_w(&p, 0), [0.0; 4], "w is synced to no flag");
        assert_eq!(dirty(&p, W), [true]);
    }
    #[test]
    fn the_pini_gate_opens_at_once_after_pini() {
        let p = prog(VarModel::Reentrant, 1);
        p.db.mark_pini_done();
        assert!(p.wait_for_pini());
    }

    #[test]
    fn the_pini_gate_opens_when_pini_finishes() {
        let p = prog(VarModel::Reentrant, 1);
        let marker = {
            let db = p.db.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(20));
                db.mark_pini_done();
            })
        };
        assert!(p.wait_for_pini());
        marker.join().unwrap();
    }

    /// `seq_exit` sets `die` and wakes every state set.
    #[test]
    fn a_stop_during_the_pini_gate_ends_it() {
        let p = prog(VarModel::Reentrant, 2);
        let stopper = {
            let p = Arc::clone(&p);
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(20));
                p.die.store(true, Ordering::Release);
                p.ss_wakeup(0);
            })
        };
        assert!(!p.wait_for_pini());
        stopper.join().unwrap();
    }

    /// A database on a reactor that outlives the test: a pv on it is
    /// created, and a name nobody serves never connects.
    fn live_db() -> PvDatabase {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            epics_base_rs::runtime::task::test_block_on(async move {
                tx.send(PvDatabase::new()).unwrap();
                std::future::pending::<()>().await
            })
        });
        rx.recv().unwrap()
    }

    /// Flags 1 and 2 when `sequencer` reaches the program's entry: `n` is
    /// assigned and synced to 1, `w` anonymous and synced to 2.
    fn flags_at_entry(vars: VarModel<Vars>) -> (bool, bool) {
        fn stop(ss: &StateSet<Vars>) {
            ss.prog.die.store(true, Ordering::Release);
        }
        let n = crate::program::SeqChan {
            ef_id: EfId(1),
            ..chan(Some("a"), false)
        };
        let w = crate::program::SeqChan {
            ef_id: EfId(2),
            ..w_chan(None)
        };
        let table = Box::leak(Box::new(crate::program::SeqProgram {
            entry_func: Some(stop),
            ..program(vec![n, w], vars, "", 1)
        }));
        let db = live_db();
        db.mark_pini_done();
        let p = build_on(table, &db);
        Arc::clone(&p).sequencer("t");
        let d = p.dynamic.lock();
        assert!(d.dbch(N).is_some());
        (bit_test(&d.ev_flags, 1), bit_test(&d.ev_flags, 2))
    }

    /// Only under `+s` does `sequencer` raise the flag of each anonymous
    /// synced channel, as a first monitor event would; an assigned one's
    /// is left to its pv.
    #[test]
    fn safe_mode_raises_the_flags_of_the_anonymous_synced_channels() {
        assert_eq!(flags_at_entry(VarModel::Safe), (false, true));
        assert_eq!(flags_at_entry(VarModel::Reentrant), (false, false));
    }

    /// A database serving the ao records the sleep tests' programs use, on
    /// a reactor that outlives the test.
    fn served_db() -> &'static PvDatabase {
        static DB: std::sync::OnceLock<PvDatabase> = std::sync::OnceLock::new();
        DB.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                epics_base_rs::runtime::task::test_block_on(async move {
                    let db = PvDatabase::new();
                    for name in ["d1:n", "d2:n", "d3:n", "d4:n", "d5:n", "d6:n"] {
                        db.add_record(
                            name,
                            Box::new(epics_base_rs::server::records::ao::AoRecord::new(1.5)),
                        )
                        .await
                        .unwrap();
                    }
                    db.mark_pini_done();
                    tx.send(db).unwrap();
                    std::future::pending::<()>().await
                })
            });
            rx.recv().unwrap()
        })
    }

    /// Each program's state sets reach the database `seq` started it on,
    /// not the one the IOC's shared pv system was made over.
    #[test]
    fn a_state_set_reaches_the_database_seq_started_its_program_on() {
        static SEEN: parking_lot::Mutex<Vec<(&str, bool)>> = parking_lot::Mutex::new(Vec::new());
        fn look(ss: &StateSet<Vars>) {
            let served = ss.database().get_pv("d1:n").is_ok();
            SEEN.lock().push((ss.prog.table.prog_name, served));
            ss.prog.die.store(true, Ordering::Release);
        }
        let on = |prog_name| {
            &*Box::leak(Box::new(crate::program::SeqProgram {
                prog_name,
                entry_func: Some(look),
                ..program(vec![], VarModel::Reentrant, "", 1)
            }))
        };
        let other = live_db();
        other.mark_pini_done();
        crate::seq::seq(on("onServed"), "", 0, served_db()).expect("onServed starts");
        crate::seq::seq(on("onOther"), "", 0, &other).expect("onOther starts");
        let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
        while SEEN.lock().len() < 2 {
            assert!(std::time::Instant::now() < deadline, "{:?}", SEEN.lock());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut seen = SEEN.lock().clone();
        seen.sort();
        assert_eq!(seen, [("onOther", false), ("onServed", true)]);
    }

    /// `sequencer` to its end over a one-state program named `pv`, found
    /// by its thread as a registered one is, whose `n` is on `pv`,
    /// monitored and synced to flag 1.
    fn run_one_state(
        pv: &'static str,
        event_func: crate::program::EventFn<Vars>,
        action_func: crate::program::ActionFn<Vars>,
    ) {
        let n = crate::program::SeqChan {
            ef_id: EfId(1),
            ..chan(Some(pv), true)
        };
        let base = program(vec![n], VarModel::Reentrant, "", 1);
        let states = Box::leak(Box::new([crate::program::SeqState {
            state_name: "s",
            action_func,
            event_func,
            entry_func: None,
            exit_func: None,
            event_mask: base.ss[0].states[0].event_mask,
            options: StateOptions::new(0),
        }]));
        let ss = Box::leak(Box::new([crate::program::SeqSs {
            ss_name: "ss",
            states,
        }]));
        let table = Box::leak(Box::new(crate::program::SeqProgram {
            prog_name: pv,
            ss,
            ..base
        }));
        crate::cmd::seq_register_sequencer_program(table);
        let p = build_on(table, served_db());
        p.sequencer("t");
    }

    /// A put of `v` to `pv` from a client context of its own.
    fn post_from_another_client(pv: &'static str, v: f64) {
        std::thread::spawn(move || {
            let sys = crate::pv::PvSystem::create(served_db());
            let pv = crate::pv::PvVar::create(&sys, pv, |_| {}).unwrap();
            let put = pv.put_no_block(crate::types::PvType::Double, 1, EpicsValue::Double(v));
            assert_eq!(put, PvStat::Ok);
        })
        .join()
        .unwrap();
    }

    static RESPONSE_SEEN: AtomicBool = AtomicBool::new(false);

    /// xiahsc's `WAIT_HSC_RESPONSE`: an action polls a synced flag between
    /// `epicsThreadSleep`s and sees the response another client posts, as
    /// C's `CAC-event` delivers while the state set sleeps.
    #[test]
    fn a_flag_polled_between_sleeps_sees_a_post_land() {
        fn event(_: &StateSet<Vars>, _: &mut usize, _: &mut usize) -> bool {
            true
        }
        fn action(ss: &StateSet<Vars>, _: usize, _: &mut usize) {
            ss.ef_clear(EfId(1));
            post_from_another_client("d1:n", 5.0);
            let seen = (0..50).any(|_| {
                ss.ef_test(EfId(1)) || {
                    crate::c::epicsThreadSleep(0.02);
                    false
                }
            });
            RESPONSE_SEEN.store(seen, Ordering::SeqCst);
            ss.prog.die.store(true, Ordering::Release);
        }
        run_one_state("d1:n", event, action);
        assert!(RESPONSE_SEEN.load(Ordering::SeqCst));
    }

    static AT_ONCE_SEEN: AtomicU32 = AtomicU32::new(0);

    /// A sleep that returns at once, as C's `nanosleep` of zero or of a
    /// delay it refuses does, is a delivery point too, as a `when` wait
    /// that finds `syncSem` raised is.
    #[test]
    fn a_sleep_that_returns_at_once_is_a_delivery_point_too() {
        fn event(_: &StateSet<Vars>, _: &mut usize, _: &mut usize) -> bool {
            true
        }
        fn action(ss: &StateSet<Vars>, _: usize, _: &mut usize) {
            for (v, at_once) in [(2.0, 0.0), (3.0, -1.0), (4.0, f64::NAN), (5.0, 1e300)] {
                ss.ef_clear(EfId(1));
                post_from_another_client("d3:n", v);
                crate::c::epicsThreadSleep(at_once);
                if ss.ef_test(EfId(1)) {
                    AT_ONCE_SEEN.fetch_add(1, Ordering::SeqCst);
                }
            }
            ss.prog.die.store(true, Ordering::Release);
        }
        run_one_state("d3:n", event, action);
        assert_eq!(AT_ONCE_SEEN.load(Ordering::SeqCst), 4);
    }

    static OWN_AT_ZERO: AtomicBool = AtomicBool::new(false);

    /// A put's own monitor has landed by the next wait: a zero sleep after
    /// the put finds its flag up.
    #[test]
    fn a_zero_sleep_after_a_put_finds_its_own_monitor() {
        fn event(_: &StateSet<Vars>, _: &mut usize, _: &mut usize) -> bool {
            true
        }
        fn action(ss: &StateSet<Vars>, _: usize, _: &mut usize) {
            ss.ef_clear(EfId(1));
            ss.var().n.set(3);
            assert_eq!(ss.pv_put(N, crate::types::CompType::Default), PvStat::Ok);
            crate::c::epicsThreadSleep(0.0);
            OWN_AT_ZERO.store(ss.ef_test(EfId(1)), Ordering::SeqCst);
            ss.prog.die.store(true, Ordering::Release);
        }
        run_one_state("d4:n", event, action);
        assert!(OWN_AT_ZERO.load(Ordering::SeqCst));
    }

    static PASS: AtomicU32 = AtomicU32::new(0);
    static STALE_ECHO: AtomicBool = AtomicBool::new(true);

    /// pf4's `PVPUT(x); epicsThreadSleep(PVPUT_WAIT); efClear(x_mon)`: the
    /// put's echo lands during the sleep and the clear takes it, so the
    /// next wait sees no flag.
    #[test]
    fn an_own_put_echo_lands_during_a_sleep_before_the_clear() {
        fn event(ss: &StateSet<Vars>, _: &mut usize, _: &mut usize) -> bool {
            PASS.load(Ordering::SeqCst) == 0 || ss.delay(0.2)
        }
        fn action(ss: &StateSet<Vars>, _: usize, _: &mut usize) {
            if PASS.fetch_add(1, Ordering::SeqCst) == 0 {
                ss.var().n.set(0);
                assert_eq!(ss.pv_put(N, crate::types::CompType::Default), PvStat::Ok);
                crate::c::epicsThreadSleep(0.05);
                ss.ef_clear(EfId(1));
            } else {
                STALE_ECHO.store(ss.ef_test(EfId(1)), Ordering::SeqCst);
                ss.prog.die.store(true, Ordering::Release);
            }
        }
        run_one_state("d2:n", event, action);
        assert_eq!(PASS.load(Ordering::SeqCst), 2);
        assert!(!STALE_ECHO.load(Ordering::SeqCst));
    }

    static PUT_THEN_CLEAR: AtomicU32 = AtomicU32::new(0);
    static SEEN_AFTER_CLEAR: AtomicBool = AtomicBool::new(false);

    /// MM4005_trajectoryScan's `pvPut(motorMDVS[j]); efClear(motorMDVSMon)`:
    /// the put's own monitor lands after the clear, so the next wait sees
    /// its flag. `CAC-event` is held until the clear, as the put's return
    /// beats it in C.
    #[test]
    fn an_own_put_monitor_lands_after_the_clear_that_follows_it() {
        fn event(ss: &StateSet<Vars>, _: &mut usize, _: &mut usize) -> bool {
            PUT_THEN_CLEAR.load(Ordering::SeqCst) == 0 || ss.ef_test(EfId(1)) || ss.delay(1.0)
        }
        fn action(ss: &StateSet<Vars>, _: usize, _: &mut usize) {
            if PUT_THEN_CLEAR.fetch_add(1, Ordering::SeqCst) == 0 {
                let (_held, release) = hold_cac_event(&ss.prog.pv_sys, "d6:n", || {
                    post_from_another_client("d6:n", 2.0)
                });
                ss.var().n.set(4);
                assert_eq!(ss.pv_put(N, crate::types::CompType::Default), PvStat::Ok);
                ss.ef_clear(EfId(1));
                let _ = release.send(());
            } else {
                SEEN_AFTER_CLEAR.store(ss.ef_test(EfId(1)), Ordering::SeqCst);
                ss.prog.die.store(true, Ordering::Release);
            }
        }
        run_one_state("d5:n", event, action);
        assert_eq!(PUT_THEN_CLEAR.load(Ordering::SeqCst), 2);
        assert!(SEEN_AFTER_CLEAR.load(Ordering::SeqCst));
    }
}
