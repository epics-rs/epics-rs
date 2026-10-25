//! `seq_if.c` per C boundary, on a program built as `seq_main.c` builds
//! one, over a database that serves no record.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32};
use std::sync::{Arc, OnceLock};

use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush, errlog_remove_listener};
use epics_base_rs::runtime::task::test_block_on;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use parking_lot::Mutex;

use super::*;
use crate::program::{
    ProgOptions, SEQ_MAGIC, SeqChan, SeqProgram, SeqSs, SeqState, StateOptions, VarModel,
};
use crate::pv::{PvEventType, PvSystem};
use crate::pvt::{Chan, PvReq, QueueElem, SeqEvent, SeqMacros, SeqQueue, VarBlock};
use crate::types::{NOEVFLAG, PrimType, SeqMask, nwords};
use crate::var::{ChanCell, SeqVar, Zeroed};

#[derive(Clone)]
pub(super) struct Vars {
    pub n: SeqVar<i32>,
    pub w: SeqVar<[f64; 4]>,
    pub q: SeqVar<i32>,
    pub m: [SeqVar<i16>; 2],
}

impl ProgramVars for Vars {
    fn zeroed() -> Self {
        Vars {
            n: Zeroed::zeroed(),
            w: Zeroed::zeroed(),
            q: Zeroed::zeroed(),
            m: [Zeroed::zeroed(), Zeroed::zeroed()],
        }
    }
}

/// Event flags 1 and 2; `n` is synced to 1, `q` is `syncQ`ed to 2 with a
/// queue of two, `m` is a two-PV array.
pub(super) const NUM_EV_FLAGS: u32 = 2;
pub(super) const N: ChId = ChId(0);
pub(super) const W: ChId = ChId(1);
pub(super) const Q: ChId = ChId(2);
pub(super) const M: ChId = ChId(3);

fn chan(var: fn(&Vars) -> &dyn ChanCell, name: &'static str, ty: PrimType) -> SeqChan<Vars> {
    SeqChan {
        ch_name: Some(""),
        var,
        var_name: name,
        var_type: ty,
        count: 1,
        event_num: 0,
        ef_id: NOEVFLAG,
        monitored: false,
        queue_size: 0,
        queue_index: 0,
    }
}

fn event(_: &StateSet<Vars>, _: &mut usize, _: &mut usize) -> bool {
    false
}

fn action(_: &StateSet<Vars>, _: usize, _: &mut usize) {}

fn init(_: &Vars) {}

/// The table with `vars` as its model, and `ss` state sets.
pub(super) fn table(
    vars: VarModel<Vars>,
    options: SeqMask,
    ss: usize,
) -> &'static SeqProgram<Vars> {
    let mut chans = vec![
        SeqChan {
            ef_id: EfId(1),
            monitored: true,
            ..chan(|v| &v.n, "n", PrimType::Int)
        },
        SeqChan {
            count: 4,
            ..chan(|v| &v.w, "w", PrimType::Double)
        },
        SeqChan {
            ef_id: EfId(2),
            monitored: true,
            queue_size: 2,
            ..chan(|v| &v.q, "q", PrimType::Int)
        },
        chan(|v| &v.m[0], "m[0]", PrimType::Short),
        chan(|v| &v.m[1], "m[1]", PrimType::Short),
    ];
    for (n, c) in chans.iter_mut().enumerate() {
        c.event_num = NUM_EV_FLAGS + 1 + n as u32;
    }
    let words = nwords(NUM_EV_FLAGS + chans.len() as u32);
    let mask: &'static [SeqMask] = Box::leak(vec![0; words].into_boxed_slice());
    let states: &'static [SeqState<Vars>] = Box::leak(Box::new([SeqState {
        state_name: "s",
        action_func: action,
        event_func: event,
        entry_func: None,
        exit_func: None,
        event_mask: mask,
        options: StateOptions::new(0),
    }]));
    let ss: Vec<SeqSs<Vars>> = (0..ss)
        .map(|_| SeqSs {
            ss_name: "ss",
            states,
        })
        .collect();
    let t = Box::leak(Box::new(SeqProgram {
        magic: SEQ_MAGIC,
        prog_name: "t",
        chan: Box::leak(chans.into_boxed_slice()),
        ss: Box::leak(ss.into_boxed_slice()),
        vars,
        params: "P=xxx:",
        num_ev_flags: NUM_EV_FLAGS,
        options: ProgOptions::new(options),
        init_func: init,
        entry_func: None,
        exit_func: None,
        num_queues: 1,
    }));
    assert_eq!(t.validate(), Ok(()));
    t
}

fn per_chan<T>(nch: usize, f: impl Fn() -> T) -> Box<[T]> {
    (0..nch).map(|_| f()).collect()
}

/// `seq_main.c`'s `seq` and `init_chan`, short of connecting: every
/// channel anonymous, each synced channel on its flag's list.
/// A pv runs on the runtime its database was made in, and stops when the
/// database goes, so both live for the whole test binary: the thread that
/// made the database keeps driving its runtime.
fn pv_db() -> &'static PvDatabase {
    static DB: OnceLock<PvDatabase> = OnceLock::new();
    DB.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            test_block_on(async move {
                tx.send(PvDatabase::new()).unwrap();
                std::future::pending::<()>().await
            })
        });
        rx.recv().unwrap()
    })
}

pub(super) fn program(table: &'static SeqProgram<Vars>) -> Arc<Program<Vars>> {
    program_on(table, pv_db())
}

fn program_on(table: &'static SeqProgram<Vars>, db: &PvDatabase) -> Arc<Program<Vars>> {
    program_over(table, PvSystem::create(db), db)
}

/// [`program_on`] with its pvs on `pv_sys`.
fn program_over(
    table: &'static SeqProgram<Vars>,
    pv_sys: PvSystem,
    db: &PvDatabase,
) -> Arc<Program<Vars>> {
    let vars = match &table.vars {
        VarModel::Static(f) => VarBlock::Static(f()),
        VarModel::Reentrant | VarModel::Safe => VarBlock::Owned(Box::new(Vars::zeroed())),
    };
    let block = match &vars {
        VarBlock::Static(v) => (*v).clone(),
        VarBlock::Owned(v) => (**v).clone(),
    };
    let nch = table.chan.len();
    let mut synced_chans = vec![Vec::new(); NUM_EV_FLAGS as usize + 1];
    for (n, c) in table.chan.iter().enumerate() {
        if c.ef_id != NOEVFLAG {
            synced_chans[c.ef_id.0 as usize].push(ChId(n as u32));
        }
    }
    Arc::new(Program {
        table,
        vars,
        instance: AtomicU32::new(0),
        thread_priority: 0,
        stack_size: 0,
        gate: pv_sys.gate(),
        pv_sys,
        db: db.clone(),
        chans: table
            .chan
            .iter()
            .map(|def| Chan {
                def,
                monitored: AtomicBool::new(def.monitored),
                db_count: AtomicU32::new(def.count),
                var_lock: Mutex::new(PvMeta::zeroed()),
            })
            .collect(),
        queues: Box::new([SeqQueue::new(2).unwrap()]),
        ss: table
            .ss
            .iter()
            .map(|def| Sscb {
                def,
                var: table.vars.safe().then(|| block.clone()),
                thread_id: OnceLock::new(),
                current_state: AtomicI32::new(0),
                next_state: AtomicI32::new(-1),
                prev_state: AtomicI32::new(-1),
                mask: Mutex::new(def.states[0].event_mask),
                time_entered: Mutex::new(0.0),
                wakeup_time: Mutex::new(f64::INFINITY),
                sync_sem: SeqEvent::default(),
                dead: SeqEvent::default(),
                get_req: per_chan(nch, || Mutex::new(None)),
                put_req: per_chan(nch, || Mutex::new(None)),
                meta: per_chan(nch, || Mutex::new(PvMeta::zeroed())),
                dirty: per_chan(nch, || AtomicBool::new(false)),
            })
            .collect(),
        // seqMacParse of the table's params.
        macros: SeqMacros(vec![("P".into(), Some("xxx:".into()))]),
        dynamic: Mutex::new(ProgDyn::new(
            nch,
            vec![0; nwords(NUM_EV_FLAGS + nch as u32)].into(),
            synced_chans.into(),
            table.chan.iter().map(|c| c.ef_id).collect(),
        )),
        die: AtomicBool::new(false),
        ready: SeqEvent::default(),
        dead: SeqEvent::default(),
    })
}

pub(super) fn ss(prog: &Arc<Program<Vars>>, index: usize) -> StateSet<Vars> {
    StateSet {
        prog: Arc::clone(prog),
        index,
    }
}

/// Assign `ch` to `name` with no pv behind it, as `seq_pvAssign` leaves a
/// channel before its connect callback.
pub(super) fn assign(prog: &Program<Vars>, ch: ChId, name: &str, connected: bool) {
    let mut d = prog.dynamic.lock();
    d.init_dbch(ch, name.into(), false);
    d.set_connected(ch, connected);
}

pub(super) fn connect(prog: &Program<Vars>, ch: ChId, connected: bool) {
    prog.dynamic.lock().set_connected(ch, connected).unwrap();
}

/// Whether the state set's `syncSem` was signalled; consumes the signal.
pub(super) fn woken(prog: &Program<Vars>, index: usize) -> bool {
    prog.ss[index].sync_sem.wait_timeout(0.0)
}

/// Set `ef` without `seq_efSet`'s wakeup.
pub(super) fn raise(prog: &Program<Vars>, ef: EfId) {
    bit_set(&mut prog.dynamic.lock().ev_flags, ef.0);
}

pub(super) fn set_mask(prog: &Program<Vars>, index: usize, events: &[u32]) {
    let mut m = vec![0; nwords(NUM_EV_FLAGS + prog.chans.len() as u32)];
    for &e in events {
        bit_set(&mut m, e);
    }
    *prog.ss[index].mask.lock() = Box::leak(m.into_boxed_slice());
}

/// The errlog lines containing `needle` that `f` logs.
pub(super) fn logged(needle: &str, f: impl FnOnce()) -> Vec<String> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let id = {
        let (seen, needle) = (Arc::clone(&seen), needle.to_owned());
        errlog_add_listener(move |m| {
            if m.contains(&needle) {
                seen.lock().push(m.to_owned());
            }
        })
    };
    f();
    errlog_flush();
    errlog_remove_listener(id);
    std::mem::take(&mut *seen.lock())
}

fn reentrant(ss: usize) -> Arc<Program<Vars>> {
    program(table(VarModel::Reentrant, 0, ss))
}

#[test]
fn ef_set_wakes_the_state_sets_waiting_on_the_flag() {
    let p = reentrant(2);
    set_mask(&p, 0, &[1]);
    set_mask(&p, 1, &[2]);
    let s = ss(&p, 1);
    s.ef_set(EfId(1));
    assert!(s.ef_test(EfId(1)));
    assert!(!s.ef_test(EfId(2)));
    assert!(woken(&p, 0));
    assert!(!woken(&p, 1));
}

#[test]
fn ef_clear_returns_the_old_value_and_wakes_waiters() {
    let p = reentrant(1);
    set_mask(&p, 0, &[2]);
    let s = ss(&p, 0);
    assert!(!s.ef_clear(EfId(2)));
    assert!(woken(&p, 0));
    s.ef_set(EfId(2));
    woken(&p, 0);
    assert!(s.ef_clear(EfId(2)));
    assert!(!s.ef_test(EfId(2)));
    assert!(woken(&p, 0));
}

#[test]
fn ef_test_and_clear_clears_without_waking() {
    let p = reentrant(1);
    set_mask(&p, 0, &[1]);
    let s = ss(&p, 0);
    raise(&p, EfId(1));
    assert!(s.ef_test_and_clear(EfId(1)));
    assert!(!s.ef_test_and_clear(EfId(1)));
    assert!(!woken(&p, 0));
}

#[test]
#[should_panic(expected = "event flag 3 is not one of 1..=2")]
fn a_flag_past_the_declared_ones_is_refused() {
    ss(&reentrant(1), 0).ef_set(EfId(NUM_EV_FLAGS + 1));
}

#[test]
#[should_panic(expected = "event flag 0")]
fn noevflag_is_not_a_flag() {
    ss(&reentrant(1), 0).ef_test(NOEVFLAG);
}

#[test]
fn exit_marks_the_program_dying_and_wakes_every_state_set() {
    let p = reentrant(2);
    set_mask(&p, 0, &[]);
    ss(&p, 1).exit();
    assert!(p.die.load(Ordering::SeqCst));
    assert!(p.ready.wait_timeout(0.0));
    assert!(woken(&p, 0) && woken(&p, 1));
}

fn safe(ss: usize) -> Arc<Program<Vars>> {
    program(table(VarModel::Safe, 0, ss))
}

fn meta(status: PvStat, severity: PvSevr, message: &str, secs: u64) -> PvMeta {
    PvMeta {
        timestamp: WallTime::from_unix(secs, 0),
        status,
        severity,
        message: Some(Arc::from(message.as_bytes())),
    }
}

fn synced(prog: &Program<Vars>, ef: u32) -> Vec<ChId> {
    prog.dynamic.lock().synced_chans[ef as usize].clone()
}

#[test]
fn an_unassigned_channel_reports_its_variable() {
    let p = reentrant(1);
    let s = ss(&p, 0);
    assert_eq!(s.pv_name(W), None);
    assert_eq!(s.pv_count(W), 4);
    assert!(!s.pv_assigned(W));
    assert_eq!(s.pv_status(W), PvStat::Ok);
    assert_eq!(s.pv_severity(W), PvSevr::OK);
    assert_eq!(s.pv_message(W).as_deref(), Some(&b""[..]));
    let before = WallTime::now();
    assert!(s.pv_time_stamp(W) >= before);
}

#[test]
fn an_assigned_channel_reports_the_pv() {
    let p = reentrant(1);
    assign(&p, W, "xxx:w", true);
    p.chans[W.index()].db_count.store(2, Ordering::Relaxed);
    *p.chans[W.index()].var_lock.lock() = meta(PvStat::Hihi, PvSevr::Major, "hihi", 1_000_000_000);
    let s = ss(&p, 0);
    assert_eq!(s.pv_name(W).as_deref(), Some(&b"xxx:w"[..]));
    assert_eq!(s.pv_count(W), 2);
    assert!(s.pv_assigned(W));
    assert_eq!(s.pv_status(W), PvStat::Hihi);
    assert_eq!(s.pv_severity(W), PvSevr::Major);
    assert_eq!(s.pv_message(W).as_deref(), Some(&b"hihi"[..]));
    assert_eq!(s.pv_time_stamp(W), WallTime::from_unix(1_000_000_000, 0));
}

/// `metaPtr`: safe mode reads the state set's copy, not the channel's.
#[test]
fn safe_mode_reports_the_state_set_s_meta_data() {
    let p = safe(2);
    assign(&p, N, "xxx:n", true);
    *p.chans[N.index()].var_lock.lock() = meta(PvStat::Hihi, PvSevr::Major, "shared", 1);
    *p.ss[1].meta[N.index()].lock() = meta(PvStat::Lolo, PvSevr::Minor, "own", 2);
    let s = ss(&p, 1);
    assert_eq!(s.pv_status(N), PvStat::Lolo);
    assert_eq!(s.pv_severity(N), PvSevr::Minor);
    assert_eq!(s.pv_message(N).as_deref(), Some(&b"own"[..]));
    assert_eq!(s.pv_time_stamp(N), WallTime::from_unix(2, 0));
    assert_eq!(ss(&p, 0).pv_status(N), PvStat::Ok);
}

#[test]
fn safe_mode_counts_an_anonymous_channel_as_connected() {
    for (p, anonymous) in [(reentrant(1), false), (safe(1), true)] {
        let s = ss(&p, 0);
        assert_eq!(s.pv_connected(N), anonymous);
        assign(&p, N, "xxx:n", false);
        assert!(!s.pv_connected(N));
        connect(&p, N, true);
        assert!(s.pv_connected(N));
    }
}

#[test]
fn an_array_is_connected_when_every_element_is() {
    let p = reentrant(1);
    let s = ss(&p, 0);
    assign(&p, M, "xxx:m0", true);
    assign(&p, M.offset(1), "xxx:m1", false);
    assert!(s.pv_array_connected(M, 1));
    assert!(!s.pv_array_connected(M, 2));
    assert!(s.pv_array_connected(M, 0));
}

#[test]
fn the_counts_are_the_program_s() {
    let p = reentrant(1);
    let s = ss(&p, 0);
    assign(&p, N, "xxx:n", true);
    assign(&p, W, "xxx:w", false);
    assert_eq!(s.pv_channel_count(), 5);
    assert_eq!(s.pv_assign_count(), 2);
    assert_eq!(s.pv_connect_count(), 1);
}

#[test]
fn opt_get_reads_the_option_bits_and_the_variable_model() {
    let all = ProgOptions::ASYNC | ProgOptions::CONN | ProgOptions::DEBUG | ProgOptions::NEWEF;
    let on = ss(&program(table(VarModel::Safe, all, 1)), 0);
    for opt in ["a", "c", "d", "e", "r", "s", "async"] {
        assert!(on.opt_get(opt), "{opt}");
    }
    for opt in ["", "x", "+a"] {
        assert!(!on.opt_get(opt), "{opt:?}");
    }
    let off = ss(&program(table(VarModel::Static(static_vars), 0, 1)), 0);
    for opt in ["a", "c", "d", "e", "r", "s"] {
        assert!(!off.opt_get(opt), "{opt}");
    }
    let r = ss(&reentrant(1), 0);
    assert!(r.opt_get("r") && !r.opt_get("s"));
}

fn static_vars() -> &'static Vars {
    static V: std::sync::LazyLock<Vars> = std::sync::LazyLock::new(Vars::zeroed);
    &V
}

#[test]
fn delay_lowers_the_wakeup_to_the_earliest_unexpired_delay() {
    let p = reentrant(1);
    let s = ss(&p, 0);
    let entered = pv_time_get_current_double() - 10.0;
    *p.ss[0].time_entered.lock() = entered;
    assert!(s.delay(5.0));
    assert_eq!(*p.ss[0].wakeup_time.lock(), f64::INFINITY);
    assert!(!s.delay(30.0));
    assert_eq!(*p.ss[0].wakeup_time.lock(), entered + 30.0);
    assert!(!s.delay(20.0));
    assert_eq!(*p.ss[0].wakeup_time.lock(), entered + 20.0);
    assert!(!s.delay(25.0));
    assert_eq!(*p.ss[0].wakeup_time.lock(), entered + 20.0);
}

#[test]
fn mac_value_get_reads_the_program_s_macros() {
    let s = ss(&reentrant(1), 0);
    assert_eq!(s.mac_value_get("P").as_deref(), Some(&b"xxx:"[..]));
    assert_eq!(s.mac_value_get("R"), None);
}

#[test]
fn pv_sync_moves_a_channel_between_flags() {
    let p = reentrant(1);
    let s = ss(&p, 0);
    s.pv_sync(N, EfId(2));
    assert_eq!(synced(&p, 1), []);
    assert_eq!(synced(&p, 2), [N, Q]);
    assert_eq!(p.dynamic.lock().synced_to[N.index()], EfId(2));
    s.pv_sync(N, EfId(2));
    assert_eq!(synced(&p, 2), [N, Q]);
    s.pv_sync(Q, NOEVFLAG);
    assert_eq!(synced(&p, 2), [N]);
    assert_eq!(p.dynamic.lock().synced_to[Q.index()], NOEVFLAG);
}

#[test]
fn pv_array_sync_syncs_each_element() {
    let p = reentrant(1);
    ss(&p, 0).pv_array_sync(M, 2, EfId(1));
    assert_eq!(synced(&p, 1), [M.offset(1), M, N]);
    let d = p.dynamic.lock();
    assert_eq!(d.synced_to[M.index()], EfId(1));
    assert_eq!(d.synced_to[M.index() + 1], EfId(1));
}

#[test]
#[should_panic(expected = "event flag 3 is not one of 0..=2")]
fn pv_sync_refuses_an_undeclared_flag() {
    ss(&reentrant(1), 0).pv_sync(N, EfId(NUM_EV_FLAGS + 1));
}

/// Queue `values` on `q`'s queue, each stamped with its own value.
fn queued(prog: &Program<Vars>, values: &[i32]) {
    for &v in values {
        prog.queues[0].put(QueueElem {
            value: EpicsValue::Long(v),
            meta: meta(PvStat::Hihi, PvSevr::Major, "queued", v as u64),
        });
    }
}

#[test]
fn an_unqueued_channel_is_a_user_error() {
    let p = reentrant(1);
    let s = ss(&p, 0);
    raise(&p, EfId(1));
    let lines = logged("(n): user error", || {
        assert!(!s.pv_get_q(N));
        s.pv_flush_q(N);
    });
    assert_eq!(
        lines,
        [
            "sevr=major pvGetQ(n): user error (not queued)\n",
            "sevr=minor pvFlushQ(n): user error (not queued)\n",
        ]
    );
    assert!(s.ef_test(EfId(1)));
}

#[test]
fn pv_get_q_of_an_empty_queue_clears_the_synced_flag() {
    let p = reentrant(1);
    let s = ss(&p, 0);
    raise(&p, EfId(2));
    assert!(!s.pv_get_q(Q));
    assert!(!s.ef_test(EfId(2)));
}

#[test]
fn pv_get_q_clears_the_flag_with_the_last_element() {
    let p = reentrant(1);
    let s = ss(&p, 0);
    queued(&p, &[1, 2]);
    raise(&p, EfId(2));
    assert!(s.pv_get_q(Q));
    assert_eq!(s.var().q.get(), 1);
    assert!(s.ef_test(EfId(2)));
    assert!(s.pv_get_q(Q));
    assert_eq!(s.var().q.get(), 2);
    assert!(!s.ef_test(EfId(2)));
}

/// getq_cp: the element's alarm and stamp, into `metaPtr`'s copy.
#[test]
fn pv_get_q_takes_the_alarm_and_stamp_but_not_the_message() {
    for p in [reentrant(1), safe(1)] {
        assign(&p, Q, "xxx:q", true);
        queued(&p, &[7]);
        let s = ss(&p, 0);
        assert!(s.pv_get_q(Q));
        assert_eq!(s.var().q.get(), 7);
        assert_eq!(s.pv_status(Q), PvStat::Hihi);
        assert_eq!(s.pv_severity(Q), PvSevr::Major);
        assert_eq!(s.pv_time_stamp(Q), WallTime::from_unix(7, 0));
        assert_eq!(s.pv_message(Q), None);
    }
}

#[test]
fn pv_flush_q_empties_the_queue_and_clears_the_flag() {
    let p = reentrant(1);
    let s = ss(&p, 0);
    queued(&p, &[1, 2]);
    raise(&p, EfId(2));
    s.pv_flush_q(Q);
    assert!(p.queues[0].is_empty());
    assert!(!s.ef_test(EfId(2)));
}

#[test]
fn an_unsynced_queue_leaves_the_flags_alone() {
    let p = reentrant(1);
    let s = ss(&p, 0);
    s.pv_sync(Q, NOEVFLAG);
    raise(&p, EfId(2));
    assert!(!s.pv_get_q(Q));
    queued(&p, &[1]);
    s.pv_flush_q(Q);
    assert!(p.queues[0].is_empty());
    assert!(s.ef_test(EfId(2)));
}

/// Make a request of state set `index` pending on `ch`.
fn pend(prog: &Program<Vars>, ev: PvEventType, index: usize, ch: ChId) -> Arc<PvReq> {
    let req = Arc::new(PvReq { ch, ss: index });
    let ss = &prog.ss[index];
    let slot = match ev {
        PvEventType::Get => &ss.get_req[ch.index()],
        _ => &ss.put_req[ch.index()],
    };
    *slot.lock() = Some(Arc::clone(&req));
    req
}

fn pending(prog: &Program<Vars>, ev: PvEventType, index: usize, ch: ChId) -> Option<Arc<PvReq>> {
    let ss = &prog.ss[index];
    match ev {
        PvEventType::Get => ss.get_req[ch.index()].lock().clone(),
        _ => ss.put_req[ch.index()].lock().clone(),
    }
}

/// What `proc_db_events` does when the request completes, after `ms`.
fn complete_after(
    prog: &Arc<Program<Vars>>,
    ev: PvEventType,
    ch: ChId,
    ms: u64,
) -> std::thread::JoinHandle<()> {
    let prog = Arc::clone(prog);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        let ss = &prog.ss[0];
        match ev {
            PvEventType::Get => *ss.get_req[ch.index()].lock() = None,
            _ => *ss.put_req[ch.index()].lock() = None,
        }
        ss.sync_sem.signal();
    })
}

fn meta_of(s: &StateSet<Vars>, ch: ChId) -> (PvStat, PvSevr, Option<String>) {
    (
        s.pv_status(ch),
        s.pv_severity(ch),
        s.pv_message(ch)
            .map(|m| String::from_utf8_lossy(&m).into_owned()),
    )
}

fn err(status: PvStat, message: Option<&str>) -> (PvStat, PvSevr, Option<String>) {
    (status, PvSevr::Error, message.map(str::to_owned))
}

const OK: (PvStat, PvSevr, Option<String>) = (PvStat::Ok, PvSevr::None, None);

#[test]
fn pv_get_of_an_unassigned_channel_is_a_user_error() {
    let s = ss(&reentrant(1), 0);
    let lines = logged("pvGet(", || {
        assert_eq!(s.pv_get(N, CompType::Sync), PvStat::Error)
    });
    assert_eq!(
        lines,
        ["sevr=major pvGet(n): user error (not assigned to a PV)\n"]
    );
}

#[test]
fn safe_mode_gets_an_anonymous_channel_from_the_shared_buffer() {
    let p = safe(1);
    p.var().n.set(5);
    let s = ss(&p, 0);
    assert_eq!(s.pv_get(N, CompType::Async), PvStat::Ok);
    assert_eq!(s.var().n.get(), 5);
}

#[test]
fn a_sync_get_needs_a_positive_timeout() {
    let p = reentrant(1);
    assign(&p, N, "xxx:n", true);
    let s = ss(&p, 0);
    let lines = logged("user error (timeout", || {
        assert_eq!(s.pv_get_tmo(N, CompType::Sync, 0.0), PvStat::Error);
        assert_eq!(s.pv_get_tmo(N, CompType::Sync, -1.5), PvStat::Error);
    });
    assert_eq!(
        lines,
        [
            "sevr=major pvGet(n,SYNC,0.000000): user error (timeout must be positive)\n",
            "sevr=major pvGet(n,SYNC,-1.500000): user error (timeout must be positive)\n",
        ]
    );
}

#[test]
fn an_async_get_refuses_a_second_one() {
    for (options, comp) in [
        (0, CompType::Async),
        (ProgOptions::ASYNC, CompType::Default),
    ] {
        let p = program(table(VarModel::Reentrant, options, 1));
        assign(&p, N, "xxx:n", true);
        let first = pend(&p, PvEventType::Get, 0, N);
        let s = ss(&p, 0);
        let lines = logged("already a pvGet", || {
            assert_eq!(s.pv_get(N, comp), PvStat::Error)
        });
        assert_eq!(
            lines,
            [
                "sevr=major pvGet(ss ss, var n, pv xxx:n): user error (there is already a pvGet \
              pending for this channel/state set combination)\n"
            ]
        );
        assert!(Arc::ptr_eq(
            &pending(&p, PvEventType::Get, 0, N).unwrap(),
            &first
        ));
    }
}

/// `DEFAULT` without `+a` is `SYNC`: it waits for the pending get.
#[test]
fn a_sync_get_times_out_waiting_for_the_pending_one() {
    let p = reentrant(1);
    assign(&p, N, "xxx:n", true);
    pend(&p, PvEventType::Get, 0, N);
    let s = ss(&p, 0);
    let lines = logged("waiting for other", || {
        assert_eq!(s.pv_get_tmo(N, CompType::Default, 0.05), PvStat::Timeout)
    });
    assert_eq!(
        lines,
        [
            "sevr=major pvGet(ss ss, var n, pv xxx:n): failed (timeout waiting for other pvGet \
          requests to finish)\n"
        ]
    );
    assert_eq!(
        meta_of(&s, N),
        err(PvStat::Timeout, Some("get completion failure"))
    );
}

#[test]
fn a_sync_get_stops_waiting_when_the_channel_disconnects() {
    let p = reentrant(1);
    assign(&p, N, "xxx:n", false);
    pend(&p, PvEventType::Get, 0, N);
    p.ss[0].sync_sem.signal();
    let s = ss(&p, 0);
    assert_eq!(s.pv_get_tmo(N, CompType::Sync, 5.0), PvStat::Disconn);
    assert_eq!(meta_of(&s, N), err(PvStat::Disconn, Some("disconnected")));
    assert!(pending(&p, PvEventType::Get, 0, N).is_some());
}

/// After the pending get completes the new one goes to the pv layer,
/// which refuses it: there is no pv behind the fixture's channels.
#[test]
fn a_refused_get_is_cancelled_and_logged() {
    let p = reentrant(1);
    assign(&p, N, "xxx:n", true);
    pend(&p, PvEventType::Get, 0, N);
    let done = complete_after(&p, PvEventType::Get, N, 30);
    let s = ss(&p, 0);
    let lines = logged("pvVarGetCallback", || {
        assert_eq!(s.pv_get_tmo(N, CompType::Sync, 5.0), PvStat::Error)
    });
    done.join().unwrap();
    assert_eq!(
        lines,
        ["sevr=fatal pvGet(var n, pv xxx:n): pvVarGetCallback() failure: (null)\n"]
    );
    assert!(pending(&p, PvEventType::Get, 0, N).is_none());
    assert_eq!(meta_of(&s, N), err(PvStat::Error, None));
}

/// `check_connected` after the refusal overwrites the call's meta data.
#[test]
fn a_refused_get_on_a_disconnected_channel_reports_the_disconnect() {
    let p = reentrant(1);
    assign(&p, N, "xxx:n", false);
    let s = ss(&p, 0);
    assert_eq!(s.pv_get(N, CompType::Async), PvStat::Error);
    assert_eq!(meta_of(&s, N), err(PvStat::Disconn, Some("disconnected")));
}

#[test]
fn wait_complete_cancels_the_request_on_timeout() {
    for (ev, text) in [
        (PvEventType::Get, "get completion failure"),
        (PvEventType::Put, "put completion failure"),
    ] {
        let p = reentrant(1);
        assign(&p, W, "xxx:w", true);
        pend(&p, ev, 0, W);
        let s = ss(&p, 0);
        assert_eq!(s.wait_complete(ev, W, 0.02), PvStat::Timeout);
        assert!(pending(&p, ev, 0, W).is_none());
        assert_eq!(meta_of(&s, W), err(PvStat::Timeout, Some(text)));
    }
}

#[test]
fn wait_complete_returns_with_the_completion() {
    for connected in [true, false] {
        let p = reentrant(1);
        assign(&p, W, "xxx:w", connected);
        pend(&p, PvEventType::Put, 0, W);
        let done = complete_after(&p, PvEventType::Put, W, 20);
        let s = ss(&p, 0);
        let want = if connected {
            PvStat::Ok
        } else {
            PvStat::Disconn
        };
        assert_eq!(s.wait_complete(PvEventType::Put, W, 5.0), want);
        done.join().unwrap();
    }
}

/// Other events keep signalling syncSem; the timeout still runs out.
#[test]
fn wait_complete_times_out_under_other_wakeups() {
    let p = reentrant(1);
    assign(&p, N, "xxx:n", true);
    pend(&p, PvEventType::Get, 0, N);
    let stop = Arc::new(AtomicBool::new(false));
    let waker = {
        let (p, stop) = (Arc::clone(&p), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                p.ss[0].sync_sem.signal();
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        })
    };
    let start = std::time::Instant::now();
    let status = ss(&p, 0).wait_complete(PvEventType::Get, N, 0.1);
    let took = start.elapsed();
    stop.store(true, Ordering::SeqCst);
    waker.join().unwrap();
    assert_eq!(status, PvStat::Timeout);
    assert!(
        took.as_secs_f64() >= 0.1 && took.as_secs_f64() < 2.0,
        "{took:?}"
    );
}

#[test]
fn pv_get_complete_of_an_unassigned_channel_is_complete() {
    for (p, log) in [(reentrant(1), true), (safe(1), false)] {
        let s = ss(&p, 0);
        let lines = logged("pvGetComplete(", || assert!(s.pv_get_complete(N)));
        let want: &[&str] = if log {
            &["sevr=major pvGetComplete(n): user error (not assigned to a PV)\n"]
        } else {
            &[]
        };
        assert_eq!(lines, want);
    }
}

#[test]
fn pv_get_complete_is_false_while_the_get_is_pending() {
    let p = reentrant(1);
    assign(&p, N, "xxx:n", false);
    pend(&p, PvEventType::Get, 0, N);
    let s = ss(&p, 0);
    assert!(!s.pv_get_complete(N));
    assert_eq!(meta_of(&s, N), OK);
    s.pv_get_cancel(N);
    assert!(s.pv_get_complete(N));
    assert_eq!(meta_of(&s, N), err(PvStat::Disconn, Some("disconnected")));
}

#[test]
fn safe_mode_pv_get_complete_copies_the_value_in() {
    let p = safe(1);
    assign(&p, N, "xxx:n", true);
    p.var().n.set(9);
    let s = ss(&p, 0);
    assert!(s.pv_get_complete(N));
    assert_eq!(s.var().n.get(), 9);
}

/// M[0] pending, M[1] done but disconnected: without a completion array
/// the loop stops at the first element that decides the answer.
#[test]
fn pv_array_get_complete_answers_any_or_all() {
    let setup = || {
        let p = reentrant(1);
        assign(&p, M, "xxx:m0", true);
        assign(&p, M.offset(1), "xxx:m1", false);
        pend(&p, PvEventType::Get, 0, M);
        p
    };
    let p = setup();
    let s = ss(&p, 0);
    assert!(!s.pv_array_get_complete(M, 2, false, None));
    assert_eq!(meta_of(&s, M.offset(1)), OK);
    assert!(s.pv_array_get_complete(M, 2, true, None));
    assert_eq!(
        meta_of(&s, M.offset(1)),
        err(PvStat::Disconn, Some("disconnected"))
    );

    let p = setup();
    let s = ss(&p, 0);
    let flags = SeqVar::new([7i32; 2]);
    assert!(!s.pv_array_get_complete(M, 2, false, Some(&flags)));
    assert_eq!(flags.get(), [0, 1]);
    assert!(s.pv_array_get_complete(M, 2, true, Some(&flags)));
}

#[test]
fn pv_get_cancel_drops_the_pending_get() {
    let p = reentrant(1);
    assign(&p, M, "xxx:m0", true);
    assign(&p, M.offset(1), "xxx:m1", true);
    pend(&p, PvEventType::Get, 0, M);
    pend(&p, PvEventType::Get, 0, M.offset(1));
    pend(&p, PvEventType::Put, 0, M);
    let s = ss(&p, 0);
    s.pv_array_get_cancel(M, 2);
    assert!(pending(&p, PvEventType::Get, 0, M).is_none());
    assert!(pending(&p, PvEventType::Get, 0, M.offset(1)).is_none());
    assert!(pending(&p, PvEventType::Put, 0, M).is_some());
}

#[test]
fn pv_get_cancel_of_an_unassigned_channel_is_a_user_error_unless_safe() {
    for (p, log) in [(reentrant(1), true), (safe(1), false)] {
        let s = ss(&p, 0);
        let lines = logged("pvGetCancel(", || s.pv_get_cancel(N));
        let want: &[&str] = if log {
            &["sevr=minor pvGetCancel(n): user error (not assigned to a PV)\n"]
        } else {
            &[]
        };
        assert_eq!(lines, want);
    }
}

#[test]
fn pv_put_of_an_unassigned_channel_is_a_user_error() {
    let s = ss(&reentrant(1), 0);
    let lines = logged("pvPut(", || {
        assert_eq!(s.pv_put(N, CompType::Default), PvStat::Error)
    });
    assert_eq!(
        lines,
        ["sevr=major pvPut(n): user error (not assigned to a PV)\n"]
    );
}

/// The third put overwrites the newest of the queue's two elements.
#[test]
fn safe_mode_puts_an_anonymous_queued_channel_to_its_queue() {
    let p = safe(1);
    set_mask(&p, 0, &[p.chans[Q.index()].def.event_num]);
    let s = ss(&p, 0);
    let lines = logged("queue is full", || {
        for v in [3, 4, 5] {
            s.var().q.set(v);
            assert_eq!(s.pv_put(Q, CompType::Sync), PvStat::Ok);
        }
    });
    assert_eq!(
        lines,
        [
            "sevr=minor pvPut on queued channel 'q' (anonymous): last queue element \
          overwritten (queue is full)\n"
        ]
    );
    let values: Vec<_> = std::iter::from_fn(|| p.queues[0].get())
        .map(|e| e.value)
        .collect();
    assert_eq!(values, [EpicsValue::Long(3), EpicsValue::Long(5)]);
    assert_eq!(p.var().q.get(), 0);
    assert!(bit_test(&p.dynamic.lock().ev_flags, 2));
    assert!(woken(&p, 0));
}

/// Only a monitored channel's put marks the other state sets dirty.
#[test]
fn safe_mode_puts_an_anonymous_channel_to_the_shared_buffer() {
    let p = safe(2);
    let s = ss(&p, 0);
    s.var().n.set(6);
    s.var().w.set([1.0, 2.0, 3.0, 4.0]);
    assert_eq!(s.pv_put(N, CompType::Default), PvStat::Ok);
    assert_eq!(s.pv_put(W, CompType::Async), PvStat::Ok);
    assert_eq!(p.var().n.get(), 6);
    assert_eq!(p.var().w.get(), [1.0, 2.0, 3.0, 4.0]);
    assert!(p.ss[1].dirty[N.index()].load(Ordering::SeqCst));
    assert!(!p.ss[1].dirty[W.index()].load(Ordering::SeqCst));
    assert!(bit_test(&p.dynamic.lock().ev_flags, 1));
}

#[test]
fn a_put_to_a_disconnected_channel_fails_before_the_pending_check() {
    let p = reentrant(1);
    assign(&p, N, "xxx:n", false);
    pend(&p, PvEventType::Put, 0, N);
    let s = ss(&p, 0);
    for comp in [CompType::Default, CompType::Async, CompType::Sync] {
        let lines = logged("pvPut", || assert_eq!(s.pv_put(N, comp), PvStat::Disconn));
        assert!(lines.is_empty(), "{lines:?}");
    }
    assert_eq!(meta_of(&s, N), err(PvStat::Disconn, Some("disconnected")));
}

#[test]
fn a_sync_put_needs_a_positive_timeout() {
    let p = reentrant(1);
    assign(&p, N, "xxx:n", true);
    let s = ss(&p, 0);
    let lines = logged("user error (timeout", || {
        assert_eq!(s.pv_put_tmo(N, CompType::Sync, 0.0), PvStat::Error)
    });
    assert_eq!(
        lines,
        ["sevr=major pvPut(n,SYNC,0.000000): user error (timeout must be positive)\n"]
    );
}

#[test]
fn an_async_put_refuses_a_second_one() {
    let p = reentrant(1);
    assign(&p, N, "xxx:n", true);
    let first = pend(&p, PvEventType::Put, 0, N);
    let s = ss(&p, 0);
    let lines = logged("already a pvPut", || {
        assert_eq!(s.pv_put(N, CompType::Async), PvStat::Error)
    });
    assert_eq!(
        lines,
        [
            "sevr=major pvPut(ss ss, var n, pv xxx:n): user error (there is already a pvPut \
          pending for this channel/state set combination)\n"
        ]
    );
    assert!(Arc::ptr_eq(
        &pending(&p, PvEventType::Put, 0, N).unwrap(),
        &first
    ));
}

#[test]
fn a_sync_put_times_out_waiting_for_the_pending_one() {
    let p = reentrant(1);
    assign(&p, N, "xxx:n", true);
    pend(&p, PvEventType::Put, 0, N);
    let s = ss(&p, 0);
    let lines = logged("waiting for other", || {
        assert_eq!(s.pv_put_tmo(N, CompType::Sync, 0.05), PvStat::Timeout)
    });
    assert_eq!(
        lines,
        [
            "sevr=major pvPut(ss ss, var n, pv xxx:n): failed (timeout waiting for other pvPut \
          requests to finish)\n"
        ]
    );
    assert_eq!(
        meta_of(&s, N),
        err(PvStat::Timeout, Some("put completion failure"))
    );
}

/// `DEFAULT` is a put without a request even under `+a`: it neither
/// waits for nor refuses over the pending one. The pv layer refuses it,
/// as there is no pv behind the fixture's channels.
#[test]
fn a_default_put_goes_without_a_request() {
    let p = program(table(VarModel::Reentrant, ProgOptions::ASYNC, 1));
    assign(&p, N, "xxx:n", true);
    let first = pend(&p, PvEventType::Put, 0, N);
    let s = ss(&p, 0);
    let lines = logged("pvPut", || {
        assert_eq!(s.pv_put(N, CompType::Default), PvStat::Error)
    });
    assert_eq!(
        lines,
        ["sevr=fatal pvPut(var n, pv xxx:n): pvVarPutNoBlock() failure: (null)\n"]
    );
    assert!(Arc::ptr_eq(
        &pending(&p, PvEventType::Put, 0, N).unwrap(),
        &first
    ));
    assert_eq!(meta_of(&s, N), err(PvStat::Error, None));
}

#[test]
fn a_refused_put_is_cancelled_and_logged() {
    let p = reentrant(1);
    assign(&p, N, "xxx:n", true);
    pend(&p, PvEventType::Put, 0, N);
    let done = complete_after(&p, PvEventType::Put, N, 30);
    let s = ss(&p, 0);
    let lines = logged("pvVarPutCallback", || {
        assert_eq!(s.pv_put_tmo(N, CompType::Sync, 5.0), PvStat::Error)
    });
    done.join().unwrap();
    assert_eq!(
        lines,
        ["sevr=fatal pvPut(var n, pv xxx:n): pvVarPutCallback() failure: (null)\n"]
    );
    assert!(pending(&p, PvEventType::Put, 0, N).is_none());
    assert_eq!(meta_of(&s, N), err(PvStat::Error, None));
}

#[test]
fn pv_put_complete_of_an_unassigned_channel_is_complete() {
    for (p, log) in [(reentrant(1), true), (safe(1), false)] {
        let s = ss(&p, 0);
        let lines = logged("pvPutComplete(", || {
            assert!(s.pv_put_complete(N, 1, false, None))
        });
        let want: &[&str] = if log {
            &["sevr=major pvPutComplete(n): user error (not assigned to a PV)\n"]
        } else {
            &[]
        };
        assert_eq!(lines, want);
    }
}

/// M[0] put pending, M[1] done but disconnected.
#[test]
fn pv_put_complete_answers_any_or_all() {
    let setup = || {
        let p = reentrant(1);
        assign(&p, M, "xxx:m0", true);
        assign(&p, M.offset(1), "xxx:m1", false);
        pend(&p, PvEventType::Put, 0, M);
        pend(&p, PvEventType::Get, 0, M.offset(1));
        p
    };
    let p = setup();
    let s = ss(&p, 0);
    assert!(!s.pv_put_complete(M, 2, false, None));
    assert_eq!(meta_of(&s, M.offset(1)), OK);
    assert!(s.pv_put_complete(M, 2, true, None));
    assert_eq!(
        meta_of(&s, M.offset(1)),
        err(PvStat::Disconn, Some("disconnected"))
    );

    let p = setup();
    let s = ss(&p, 0);
    let flags = SeqVar::new([7i32; 2]);
    assert!(!s.pv_array_put_complete(M, 2, false, Some(&flags)));
    assert_eq!(flags.get(), [0, 1]);
    assert!(s.pv_array_put_complete(M, 2, true, Some(&flags)));
    s.pv_put_cancel(M);
    assert!(s.pv_put_complete(M, 2, false, None));
}

#[test]
fn pv_put_cancel_drops_the_pending_put() {
    let p = reentrant(1);
    assign(&p, M, "xxx:m0", true);
    assign(&p, M.offset(1), "xxx:m1", true);
    pend(&p, PvEventType::Put, 0, M);
    pend(&p, PvEventType::Put, 0, M.offset(1));
    pend(&p, PvEventType::Get, 0, M);
    let s = ss(&p, 0);
    s.pv_array_put_cancel(M, 2);
    assert!(pending(&p, PvEventType::Put, 0, M).is_none());
    assert!(pending(&p, PvEventType::Put, 0, M.offset(1)).is_none());
    assert!(pending(&p, PvEventType::Get, 0, M).is_some());
}

#[test]
fn pv_put_cancel_of_an_unassigned_channel_is_a_user_error_unless_safe() {
    for (p, log) in [(reentrant(1), true), (safe(1), false)] {
        let s = ss(&p, 0);
        let lines = logged("pvPutCancel(", || s.pv_put_cancel(N));
        let want: &[&str] = if log {
            &["sevr=minor pvPutCancel(n): user error (not assigned to a PV)\n"]
        } else {
            &[]
        };
        assert_eq!(lines, want);
    }
}

fn name_of(s: &StateSet<Vars>, ch: ChId) -> Option<String> {
    s.pv_name(ch)
        .map(|n| String::from_utf8_lossy(&n).into_owned())
}

/// The channel counts again as `count` elements; `monitored` is the
/// variable's and stays.
#[test]
fn pv_assign_of_an_empty_name_unassigns() {
    let p = reentrant(1);
    assign(&p, N, "xxx:n", true);
    assign(&p, W, "xxx:w", false);
    p.chans[W.index()].db_count.store(2, Ordering::Relaxed);
    let s = ss(&p, 0);
    assert_eq!(s.pv_assign(N, ""), PvStat::Ok);
    assert_eq!((s.pv_assign_count(), s.pv_connect_count()), (1, 0));
    assert_eq!(s.pv_assign(W, "\0xxx:w"), PvStat::Ok);
    assert_eq!((s.pv_assign_count(), s.pv_connect_count()), (0, 0));
    assert!(!s.pv_assigned(N) && !s.pv_assigned(W));
    assert_eq!(s.pv_count(W), 4);
    assert!(p.chans[N.index()].monitored.load(Ordering::Relaxed));
    assert_eq!(s.pv_assign(N, ""), PvStat::Ok);
    assert_eq!(s.pv_assign_count(), 0);
}

/// `if (!pvName) pvName = "";`, whether the null is a pointer argument
/// or a `char *` variable.
#[test]
fn pv_assign_of_a_null_name_unassigns() {
    let p = reentrant(1);
    assign(&p, N, "xxx:n", true);
    assign(&p, W, "xxx:w", false);
    let s = ss(&p, 0);
    let null: CharPtr = None;
    assert_eq!(s.pv_assign(N, null.clone()), PvStat::Ok);
    assert_eq!(s.pv_assign(W, SeqVar::new(null)), PvStat::Ok);
    assert!(!s.pv_assigned(N) && !s.pv_assigned(W));
    assert_eq!((s.pv_assign_count(), s.pv_connect_count()), (0, 0));
}

#[test]
fn pv_assign_assigns_an_anonymous_channel() {
    let p = reentrant(1);
    *p.chans[W.index()].var_lock.lock() = meta(PvStat::Hihi, PvSevr::Major, "old", 7);
    let s = ss(&p, 0);
    assert_eq!(s.pv_assign(W, "xxx:w"), PvStat::Ok);
    assert_eq!(name_of(&s, W).as_deref(), Some("xxx:w"));
    assert_eq!((s.pv_assign_count(), s.pv_connect_count()), (1, 0));
    assert_eq!(s.pv_count(W), 0);
    assert!(!s.pv_connected(W));
    assert_eq!(meta_of(&s, W), OK);
    assert_eq!(s.pv_time_stamp(W), PvMeta::zeroed().timestamp);
}

/// The new channel keeps the old one's element count and meta data until
/// it connects, as C reuses the old `DBCHAN`.
#[test]
fn pv_assign_replaces_a_connected_channel() {
    let p = reentrant(1);
    assign(&p, W, "xxx:w", true);
    p.chans[W.index()].db_count.store(2, Ordering::Relaxed);
    *p.chans[W.index()].var_lock.lock() = meta(PvStat::Hihi, PvSevr::Major, "old", 7);
    let s = ss(&p, 0);
    assert_eq!(s.pv_assign(W, "yyy:w"), PvStat::Ok);
    assert_eq!(name_of(&s, W).as_deref(), Some("yyy:w"));
    assert_eq!((s.pv_assign_count(), s.pv_connect_count()), (1, 0));
    assert_eq!(s.pv_count(W), 2);
    assert_eq!(
        meta_of(&s, W),
        (PvStat::Hihi, PvSevr::Major, Some("old".into()))
    );
}

/// C leaves `gotMonitorCount` counting the channel it takes out.
#[test]
fn pv_assign_releases_a_replaced_channel_s_first_monitor() {
    for name in ["yyy:w", ""] {
        let p = reentrant(1);
        assign(&p, W, "xxx:w", true);
        p.dynamic.lock().set_got_monitor(W, true);
        let s = ss(&p, 0);
        assert_eq!(s.pv_assign(W, name), PvStat::Ok);
        assert_eq!(p.dynamic.lock().got_monitor_count(), 0, "pvAssign {name:?}");
    }
}

#[test]
fn pv_assign_subst_expands_the_program_s_macros() {
    let p = reentrant(1);
    let s = ss(&p, 0);
    assert_eq!(s.pv_assign_subst(N, "{P}n"), PvStat::Ok);
    assert_eq!(name_of(&s, N).as_deref(), Some("xxx:n"));
}

fn monitored(s: &StateSet<Vars>, ch: ChId) -> bool {
    s.prog.chans[ch.index()].monitored.load(Ordering::Relaxed)
}

/// The array call stops at its first failure.
#[test]
fn a_monitor_call_on_an_unassigned_channel_is_a_user_error() {
    let s = ss(&reentrant(1), 0);
    let lines = logged("user error (not assigned", || {
        assert_eq!(s.pv_monitor(N), PvStat::Error);
        assert_eq!(s.pv_stop_monitor(N), PvStat::Error);
        assert_eq!(s.pv_array_monitor(M, 2), PvStat::Error);
        assert_eq!(s.pv_array_stop_monitor(M, 2), PvStat::Error);
    });
    assert_eq!(
        lines,
        [
            "sevr=major pvMonitor(n): user error (not assigned to a PV)\n",
            "sevr=major pvStopMonitor(n): user error (not assigned to a PV)\n",
            "sevr=major pvArrayMonitor(m[0]): user error (not assigned to a PV)\n",
            "sevr=major pvArrayStopMonitor(m[0]): user error (not assigned to a PV)\n",
        ]
    );
    assert!(monitored(&s, N));
}

/// C's safe-mode pvStopMonitor turned an anonymous channel's monitor on.
#[test]
fn safe_mode_turns_an_anonymous_channel_s_monitor_on_and_off() {
    let s = ss(&safe(1), 0);
    assert_eq!(s.pv_stop_monitor(N), PvStat::Ok);
    assert!(!monitored(&s, N));
    assert_eq!(s.pv_array_monitor(M, 2), PvStat::Ok);
    assert!(monitored(&s, M) && monitored(&s, M.offset(1)));
    assert_eq!(s.pv_array_stop_monitor(M, 2), PvStat::Ok);
    assert!(!monitored(&s, M) && !monitored(&s, M.offset(1)));
}

/// A destroyed pv refuses `pvVarMonitorOn`; the call takes its status and
/// message into the meta data and keeps the setting.
#[test]
fn a_failed_monitor_call_is_a_pv_call_failure() {
    let p = reentrant(1);
    let s = ss(&p, 0);
    assert_eq!(s.pv_assign(W, "xxx:w"), PvStat::Ok);
    let pvid = p
        .dynamic
        .lock()
        .dbch(W)
        .and_then(|d| d.pvid.clone())
        .unwrap();
    assert_eq!(pvid.destroy(), PvStat::Ok);
    assert_eq!(s.pv_monitor(W), PvStat::Error);
    assert!(monitored(&s, W));
    assert_eq!(
        meta_of(&s, W),
        err(PvStat::Error, Some("Invalid channel identifier"))
    );
}

// ---- seq_if.c over a served record ----

fn served_db() -> &'static PvDatabase {
    static DB: OnceLock<PvDatabase> = OnceLock::new();
    DB.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            test_block_on(async move {
                let db = PvDatabase::new();
                for name in [
                    "rv:a", "rv:b", "rv:c", "rv:d", "rv:u", "rv:e", "rv:f", "rv:g", "rv:h", "rv:i",
                    "rv:j", "rv:k", "rv:l", "rv:m", "rv:n", "rv:o", "rv:p", "rv:q", "rv:r", "rv:s",
                ] {
                    db.add_record(
                        name,
                        Box::new(epics_base_rs::server::records::ao::AoRecord::new(1.5)),
                    )
                    .await
                    .unwrap();
                }
                // A put of 1 completes only when another put sets it back to 0.
                for name in ["rv:busy", "rv:busy2", "rv:busy3"] {
                    db.add_record(
                        name,
                        Box::new(epics_base_rs::server::records::busy::BusyRecord::default()),
                    )
                    .await
                    .unwrap();
                }
                tx.send(db).unwrap();
                std::future::pending::<()>().await
            })
        });
        rx.recv().unwrap()
    })
}

fn until(what: &str, f: impl Fn() -> bool) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !f() {
        assert!(std::time::Instant::now() < end, "{what}");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[test]
fn e2e_sync_and_async_get_put_and_monitor() {
    let p = program_on(table(VarModel::Reentrant, 0, 1), served_db());
    let s = ss(&p, 0);
    assert_eq!(s.pv_assign(W, "rv:a"), PvStat::Ok);
    until("connect", || s.pv_connected(W));
    assert_eq!(s.pv_count(W), 1);
    assert_eq!((s.pv_assign_count(), s.pv_connect_count()), (1, 1));

    assert_eq!(s.pv_get_tmo(W, CompType::Sync, 5.0), PvStat::Ok);
    assert_eq!(s.var().w.get()[0], 1.5);

    s.var().w.set([2.5, 9.0, 9.0, 9.0]);
    assert_eq!(s.pv_put_tmo(W, CompType::Sync, 5.0), PvStat::Ok);
    s.var().w.set([0.0; 4]);
    assert_eq!(s.pv_get_tmo(W, CompType::Sync, 5.0), PvStat::Ok);
    assert_eq!(s.var().w.get(), [2.5, 0.0, 0.0, 0.0]);

    s.var().w.set([3.5, 0.0, 0.0, 0.0]);
    assert_eq!(s.pv_put(W, CompType::Async), PvStat::Ok);
    until("async put", || s.pv_put_complete(W, 1, false, None));
    s.var().w.set([0.0; 4]);
    assert_eq!(s.pv_get(W, CompType::Async), PvStat::Ok);
    until("async get", || s.pv_get_complete(W));
    assert_eq!(s.var().w.get()[0], 3.5);

    // The async get's wakeup may still be pending, so the first event is
    // seen by its count, which it takes after storing and waking.
    let ev = p.chans[W.index()].def.event_num;
    set_mask(&p, 0, &[ev]);
    s.var().w.set([0.0; 4]);
    assert_eq!(s.pv_monitor(W), PvStat::Ok);
    until("first monitor", || {
        p.dynamic.lock().got_monitor_count() == 1
    });
    assert_eq!(s.var().w.get()[0], 3.5);
    assert!(woken(&p, 0));
    // The record posts only a change, and nothing else wakes the state set
    // now: this wakeup is the put's monitor.
    s.var().w.set([4.5, 0.0, 0.0, 0.0]);
    assert_eq!(s.pv_put(W, CompType::Default), PvStat::Ok);
    until("monitor of the put", || woken(&p, 0));
    assert_eq!(s.pv_stop_monitor(W), PvStat::Ok);

    // Reassign: the old pv's callbacks stop and its connection no longer
    // counts. A name nobody serves never connects to change that.
    assert_eq!(s.pv_assign(W, "rv:none"), PvStat::Ok);
    assert_eq!((s.pv_assign_count(), s.pv_connect_count()), (1, 0));
    assert_eq!(s.pv_assign(W, "rv:b"), PvStat::Ok);
    until("reconnect", || s.pv_connected(W));
    assert_eq!((s.pv_assign_count(), s.pv_connect_count()), (1, 1));
    assert_eq!(s.pv_get_tmo(W, CompType::Sync, 5.0), PvStat::Ok);
    assert_eq!(s.var().w.get()[0], 1.5);
    assert_eq!(s.pv_assign(W, ""), PvStat::Ok);
    assert_eq!((s.pv_assign_count(), s.pv_connect_count()), (0, 0));
    assert_eq!(s.pv_count(W), 4);
}

/// `-c`: `seq_connect` does not wait, yet an init reads a served record at
/// once, as C's does: `dbChannelIO` connects inside `ca_create_channel`,
/// which runs `seq_conn_handler` before it returns. `pvAssign` the same.
#[test]
fn e2e_a_served_pv_is_connected_when_its_create_returns() {
    let p = program_on(table(VarModel::Reentrant, 0, 1), served_db());
    p.dynamic.lock().init_dbch(W, "rv:d".into(), false);
    assert_eq!(p.seq_connect(false), PvStat::Ok);
    let s = ss(&p, 0);
    assert!(s.pv_connected(W));
    assert_eq!(
        (s.pv_assign_count(), s.pv_connect_count(), s.pv_count(W)),
        (1, 1, 1)
    );
    assert_eq!(s.pv_get_tmo(W, CompType::Sync, 5.0), PvStat::Ok);
    assert_eq!(s.var().w.get()[0], 1.5);
    s.var().w.set([2.5, 0.0, 0.0, 0.0]);
    assert_eq!(s.pv_put(W, CompType::Default), PvStat::Ok);
    assert_eq!(served_db().get_pv("rv:d").unwrap(), EpicsValue::Double(2.5));

    assert_eq!(s.pv_assign(W, "rv:b"), PvStat::Ok);
    assert!(s.pv_connected(W));
    assert_eq!(
        (s.pv_assign_count(), s.pv_connect_count(), s.pv_count(W)),
        (1, 1, 1)
    );
    assert_eq!(s.pv_get_tmo(W, CompType::Sync, 5.0), PvStat::Ok);
    assert_eq!(s.var().w.get()[0], 1.5);
}

/// C sends an `unsigned short` with putType pvTypeSHORT: 0xffff is -1.
#[test]
fn e2e_unsigned_put_goes_as_the_put_type() {
    use crate::pv::{PvSystem, PvVar};
    use crate::types::PvType;
    let sys = PvSystem::create(served_db());
    let pv = PvVar::create(&sys, "rv:u", |_| {}).unwrap();
    assert!(pv.in_process(), "connected in the create");
    let ty = PrimType::UShort.put_type();
    assert_eq!(ty, PvType::Short);
    let v: SeqVar<u16> = SeqVar::new(0xffff);
    assert_eq!(pv.put_no_block(ty, 1, v.load(1)), PvStat::Ok);
    let got = Arc::new(Mutex::new(None));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let g = Arc::clone(&got);
        assert_eq!(
            pv.get_callback(PvType::TimeDouble, 1, move |ev| {
                *g.lock() = ev.value.map(|s| s.value.clone());
            }),
            PvStat::Ok
        );
        until("get", || got.lock().is_some());
        let val = got.lock().take().unwrap();
        if val != EpicsValue::Double(1.5) || std::time::Instant::now() > deadline {
            assert_eq!(
                val,
                EpicsValue::Double(-1.0),
                "C puts 0xffff as DBR_SHORT -1"
            );
            break;
        }
    }
}

// ---- in-process monitors: one queue per client context, on `CAC-event` ----

/// Put `v` to `name` from a client context of its own on another thread.
fn put_from_another_thread(name: &'static str, v: f64) {
    std::thread::spawn(move || {
        let sys = PvSystem::create(served_db());
        let pv = PvVar::create(&sys, name, |_| {}).unwrap();
        let put = pv.put_no_block(crate::types::PvType::Double, 1, EpicsValue::Double(v));
        assert_eq!(put, PvStat::Ok);
    })
    .join()
    .unwrap();
}

/// `dbPutNotifyBlocker`: a put callback behind one still in flight on the
/// channel waits for it and, when that wait runs out, is refused with
/// ECA_TIMEOUT, a warning `statFromCA` takes as OK, and never called back,
/// as `ca_array_put_callback` answers the throw.
#[test]
fn e2e_a_put_callback_behind_one_in_flight_is_refused_with_eca_timeout() {
    let sys = PvSystem::create(served_db());
    let pv = PvVar::create(&sys, "rv:busy", |_| {}).unwrap();
    let called = Arc::new(AtomicU32::new(0));
    let put = |called: &Arc<AtomicU32>| {
        let called = Arc::clone(called);
        pv.put_callback(
            crate::types::PvType::Short,
            1,
            EpicsValue::Short(1),
            move |_| {
                called.fetch_add(1, Ordering::Relaxed);
            },
        )
    };
    assert_eq!(put(&called), PvStat::Ok);
    assert_eq!(pv.message(), None);
    assert_eq!(put(&called), PvStat::Ok);
    assert_eq!(
        pv.message().as_deref(),
        Some("User specified timeout on IO operation expired")
    );
    assert_eq!(called.load(Ordering::Relaxed), 0);
}

/// A client context's monitors reach their callbacks one at a time, in
/// post order across its channels, on its `CAC-event` thread, as C
/// `dbContext`'s one event queue delivers them.
#[test]
fn e2e_in_process_monitors_arrive_on_cac_event_in_post_order() {
    let sys = PvSystem::create(served_db());
    let gate = sys.gate();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let _pvs: Vec<PvVar> = ["rv:e", "rv:f"]
        .into_iter()
        .map(|name| {
            let pv = PvVar::create(&sys, name, |_| {}).unwrap();
            let log = Arc::clone(&seen);
            let on = pv.monitor_on(&gate, crate::types::PvType::Double, 1, move |ev| {
                let thread = std::thread::current().name().map(str::to_owned);
                log.lock()
                    .push((name, thread, ev.value.map(|s| s.value.clone())));
            });
            assert_eq!(on, PvStat::Ok);
            pv
        })
        .collect();
    until("the first values", || seen.lock().len() == 2);
    let posts = [("rv:e", 8.5), ("rv:f", 7.5), ("rv:e", 9.5)];
    for (name, v) in posts {
        put_from_another_thread(name, v);
    }
    until("the posts", || seen.lock().len() == 5);
    let cac = Some("CAC-event".to_owned());
    let want: Vec<_> = posts
        .iter()
        .map(|&(name, v)| (name, cac.clone(), Some(EpicsValue::Double(v))))
        .collect();
    assert_eq!(seen.lock()[2..], want);
}

/// A post while the program holds `dynamic` waits on `CAC-event` for the
/// hold, as C's waits on `sp->lock`, and lands once it is let go.
#[test]
fn e2e_a_post_under_a_held_seq_lock_lands_after_its_release() {
    let p = program_on(table(VarModel::Reentrant, 0, 1), served_db());
    let s = ss(&p, 0);
    assert_eq!(s.pv_assign(W, "rv:g"), PvStat::Ok);
    assert_eq!(s.pv_monitor(W), PvStat::Ok);
    until("first monitor", || {
        p.dynamic.lock().got_monitor_count() == 1
    });
    let held = p.dynamic.lock();
    put_from_another_thread("rv:g", 8.5);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(s.var().w.get()[0], 1.5, "delivered under the hold");
    drop(held);
    until("the post", || s.var().w.get()[0] == 8.5);
}

// ---- the gate: monitors land only while every state set is blocked ----

/// `ch` of state set 0 assigned to `name` and monitored, its first value
/// in, and its event in state sets 0 and 1's masks.
fn monitored_on(p: &Arc<Program<Vars>>, ch: ChId, name: &str) {
    let s = ss(p, 0);
    assert_eq!(s.pv_assign(ch, name), PvStat::Ok);
    assert_eq!(s.pv_monitor(ch), PvStat::Ok);
    until("first monitor", || {
        p.dynamic.lock().got_monitor_count() == 1
    });
    for index in 0..p.ss.len() {
        set_mask(p, index, &[p.chans[ch.index()].def.event_num]);
        while woken(p, index) {}
    }
}

/// A put's own monitor has not landed when that put returns, as on C's
/// several CPUs, nor while the state set runs on: the next put's return
/// delivers it, and not that put's own, nor does a wait that finds
/// `syncSem` raised; a wait that blocks does.
#[test]
fn e2e_an_own_put_monitor_lands_at_the_next_delivery_point() {
    let p = program_on(table(VarModel::Reentrant, 0, 1), served_db());
    let s = ss(&p, 0);
    monitored_on(&p, W, "rv:h");
    p.gate.run(0);
    for (v, comp) in [(4.5, CompType::Default), (5.5, CompType::Sync)] {
        while woken(&p, 0) {}
        s.var().w.set([v, 0.0, 0.0, 0.0]);
        assert_eq!(s.pv_put(W, comp), PvStat::Ok);
        s.var().w.set([0.0; 4]);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(s.var().w.get()[0], 0.0, "{comp:?}: at its own return");
        s.var().w.set([v + 10.0, 0.0, 0.0, 0.0]);
        assert_eq!(s.pv_put(W, comp), PvStat::Ok);
        assert_eq!(s.var().w.get()[0], v, "{comp:?}: at the next return");
        assert!(p.sync_wait(0, 5.0), "{comp:?}: the first put's wakeup");
        assert_eq!(s.var().w.get()[0], v, "{comp:?}: at a wait that passes");
        assert!(p.sync_wait(0, 5.0), "{comp:?}: the second put's wakeup");
        assert_eq!(
            s.var().w.get()[0],
            v + 10.0,
            "{comp:?}: at a wait that blocks"
        );
    }
    p.gate.stop(0);
}

/// Another client's post lands at the state set's next delivery point, a
/// put's or a get's return or a wait that blocks, and not between two
/// plain statements.
#[test]
fn e2e_another_clients_post_lands_at_the_next_delivery_point() {
    let p = program_on(table(VarModel::Reentrant, 0, 1), served_db());
    let s = ss(&p, 0);
    monitored_on(&p, W, "rv:p");
    assert_eq!(s.pv_assign(M, "rv:q"), PvStat::Ok);
    until("connect", || s.pv_connected(M));
    p.gate.run(0);
    let points: [(f64, &dyn Fn()); 3] = [
        (4.5, &|| {
            assert_eq!(s.pv_put(M, CompType::Default), PvStat::Ok)
        }),
        (5.5, &|| assert_eq!(s.pv_get(M, CompType::Sync), PvStat::Ok)),
        (6.5, &|| {
            while woken(&p, 0) {}
            assert!(p.sync_wait(0, 5.0))
        }),
    ];
    let mut was = 1.5;
    for (v, point) in points {
        put_from_another_thread("rv:p", v);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(s.var().w.get()[0], was, "{v}: between statements");
        point();
        assert_eq!(s.var().w.get()[0], v);
        was = v;
    }
    p.gate.stop(0);
}

/// Two posts while the state set runs both land at its block, before it
/// runs on: its one evaluation after the wait sees the later value.
#[test]
fn e2e_two_monitors_held_while_running_land_before_one_evaluation() {
    let p = program_on(table(VarModel::Reentrant, 0, 1), served_db());
    let s = ss(&p, 0);
    monitored_on(&p, W, "rv:i");
    p.gate.run(0);
    put_from_another_thread("rv:i", 4.5);
    put_from_another_thread("rv:i", 6.5);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(s.var().w.get()[0], 1.5, "landed while running");
    assert!(p.sync_wait(0, 5.0));
    assert_eq!(s.var().w.get()[0], 6.5);
    assert!(!woken(&p, 0), "two posts, one wakeup");
    p.gate.stop(0);
}

/// With two state sets running, the first to block lets nothing through;
/// the last one does.
#[test]
fn e2e_a_monitor_waits_until_every_state_set_blocks() {
    let p = program_on(table(VarModel::Reentrant, 0, 2), served_db());
    let s = ss(&p, 0);
    monitored_on(&p, W, "rv:j");
    p.gate.run(0);
    p.gate.run(1);
    let prog = Arc::clone(&p);
    let ss1 = std::thread::spawn(move || prog.sync_wait(1, 5.0));
    std::thread::sleep(std::time::Duration::from_millis(50));
    put_from_another_thread("rv:j", 8.5);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(s.var().w.get()[0], 1.5, "landed while state set 0 runs");
    assert!(!ss1.is_finished());
    assert!(p.sync_wait(0, 5.0));
    assert_eq!(s.var().w.get()[0], 8.5);
    assert!(ss1.join().unwrap());
    p.gate.stop(0);
    p.gate.stop(1);
}

/// `seq_connect` returns with a monitored channel's first value in: no
/// state set runs yet, and the first monitors queued at the creates land
/// there, before the first state.
#[test]
fn e2e_first_monitors_land_before_seq_connect_returns() {
    let p = program_on(table(VarModel::Reentrant, 0, 1), served_db());
    let s = ss(&p, 0);
    p.chans[W.index()].monitored.store(true, Ordering::Relaxed);
    p.dynamic.lock().init_dbch(W, "rv:k".into(), true);
    s.var().w.set([7.0, 0.0, 0.0, 0.0]);
    assert_eq!(p.seq_connect(false), PvStat::Ok);
    assert_eq!(p.dynamic.lock().got_monitor_count(), 1);
    assert_eq!(s.var().w.get()[0], 1.5);
}

/// `pvAssign` creates its pv under `dynamic`, and the first monitor is
/// queued inside: nothing delivers it there, where `seq_mon_handler` would
/// wait on the creating thread's own hold, and it lands at the state set's
/// block.
#[test]
fn e2e_a_first_monitor_queued_under_the_create_hold_lands_at_the_block() {
    let p = program_on(table(VarModel::Reentrant, 0, 1), served_db());
    p.chans[W.index()].monitored.store(true, Ordering::Relaxed);
    set_mask(&p, 0, &[p.chans[W.index()].def.event_num]);
    let (tx, rx) = std::sync::mpsc::channel();
    let prog = Arc::clone(&p);
    std::thread::spawn(move || {
        prog.gate.run(0);
        let assigned = ss(&prog, 0).pv_assign(W, "rv:l");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let running = prog.dynamic.lock().got_monitor_count();
        while woken(&prog, 0) {}
        let woke = prog.sync_wait(0, 5.0);
        let blocked = prog.dynamic.lock().got_monitor_count();
        prog.gate.stop(0);
        tx.send((assigned, running, woke, blocked)).unwrap();
    });
    let got = rx.recv_timeout(std::time::Duration::from_secs(10));
    assert_eq!(
        got,
        Ok((PvStat::Ok, 0, true, 1)),
        "deadlocked on its own hold"
    );
    assert_eq!(ss(&p, 0).var().w.get()[0], 1.5);
}

/// A monitor turned off while its event is held: the event is dropped at
/// the block, as `db_cancel_event` drops a queued one.
#[test]
fn e2e_a_held_event_of_a_monitor_turned_off_is_dropped() {
    let p = program_on(table(VarModel::Reentrant, 0, 1), served_db());
    let s = ss(&p, 0);
    monitored_on(&p, W, "rv:m");
    p.gate.run(0);
    put_from_another_thread("rv:m", 8.5);
    assert_eq!(s.pv_stop_monitor(W), PvStat::Ok);
    s.var().w.set([3.0, 0.0, 0.0, 0.0]);
    while woken(&p, 0) {}
    assert!(!p.sync_wait(0, 0.1), "the dropped event's wakeup");
    assert_eq!(s.var().w.get()[0], 3.0);
    p.gate.stop(0);
}

/// A `when` wait that finds `syncSem` raised after no call is a pass that
/// takes everything: a monitor held while the state set ran lands there.
#[test]
fn e2e_a_wait_that_finds_sync_sem_raised_lands_held_monitors() {
    let p = program_on(table(VarModel::Reentrant, 0, 1), served_db());
    let s = ss(&p, 0);
    monitored_on(&p, W, "rv:r");
    p.gate.run(0);
    put_from_another_thread("rv:r", 7.5);
    p.ss[0].sync_sem.signal();
    assert!(p.sync_wait(0, 0.0));
    assert_eq!(s.var().w.get()[0], 7.5);
    p.gate.stop(0);
}

/// A wait with no time left after a put does not block: the put's own
/// monitor has not landed after it, and a wait that blocks lands it.
#[test]
fn e2e_a_zero_wait_after_a_put_leaves_its_own_monitor_held() {
    let p = program_on(table(VarModel::Reentrant, 0, 1), served_db());
    let s = ss(&p, 0);
    monitored_on(&p, W, "rv:s");
    p.gate.run(0);
    while woken(&p, 0) {}
    s.var().w.set([4.5, 0.0, 0.0, 0.0]);
    assert_eq!(s.pv_put(W, CompType::Default), PvStat::Ok);
    s.var().w.set([0.0; 4]);
    let woke = p.sync_wait(0, 0.0);
    assert_eq!(s.var().w.get()[0], 0.0, "at the zero wait");
    assert!(!woke);
    assert!(p.sync_wait(0, 5.0));
    assert_eq!(s.var().w.get()[0], 4.5, "at a wait that blocks");
    p.gate.stop(0);
}

/// [`put_from_another_thread`] after 50 ms, while the caller waits.
fn put_from_another_thread_later(name: &'static str, v: f64) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        put_from_another_thread(name, v);
    })
}

/// `wait_complete`'s wait on `syncSem` is a block point: a monitor posted
/// while its SYNC put is outstanding lands there, though the put's return
/// would not deliver it.
#[test]
fn e2e_monitors_land_while_a_sync_put_waits_for_completion() {
    let p = program_on(table(VarModel::Reentrant, 0, 1), served_db());
    let s = ss(&p, 0);
    monitored_on(&p, W, "rv:n");
    assert_eq!(s.pv_assign(M, "rv:busy2"), PvStat::Ok);
    until("connect", || s.pv_connected(M));
    p.gate.run(0);
    s.var().m[0].set(1);
    while woken(&p, 0) {}
    let post = put_from_another_thread_later("rv:n", 8.5);
    assert_eq!(s.pv_put_tmo(M, CompType::Sync, 0.2), PvStat::Timeout);
    post.join().unwrap();
    assert_eq!(s.var().w.get()[0], 8.5);
    p.gate.stop(0);
}

/// So is `check_pending`'s wait for a put still outstanding on the channel.
#[test]
fn e2e_monitors_land_while_a_sync_put_waits_for_one_pending() {
    let p = program_on(table(VarModel::Reentrant, 0, 1), served_db());
    let s = ss(&p, 0);
    monitored_on(&p, W, "rv:o");
    assert_eq!(s.pv_assign(M, "rv:busy3"), PvStat::Ok);
    until("connect", || s.pv_connected(M));
    p.gate.run(0);
    s.var().m[0].set(1);
    assert_eq!(s.pv_put_tmo(M, CompType::Async, 0.0), PvStat::Ok);
    while woken(&p, 0) {}
    let post = put_from_another_thread_later("rv:o", 8.5);
    assert_eq!(s.pv_put_tmo(M, CompType::Sync, 0.2), PvStat::Timeout);
    post.join().unwrap();
    assert_eq!(s.var().w.get()[0], 8.5);
    p.gate.stop(0);
}

#[test]
fn e2e_safe_mode_sync_get_copies_into_the_state_set() {
    let p = program_on(table(VarModel::Safe, 0, 2), served_db());
    let s = ss(&p, 0);
    assert_eq!(s.pv_assign(W, "rv:c"), PvStat::Ok);
    until("connect", || s.pv_connected(W));
    s.var().w.set([6.5, 0.0, 0.0, 0.0]);
    assert_eq!(s.pv_put_tmo(W, CompType::Sync, 5.0), PvStat::Ok);
    s.var().w.set([0.0; 4]);
    assert_eq!(s.pv_get_tmo(W, CompType::Sync, 5.0), PvStat::Ok);
    assert_eq!(s.var().w.get()[0], 6.5);
    let other = ss(&p, 1);
    assert_eq!(other.var().w.get()[0], 0.0);
    assert_eq!(other.pv_get(W, CompType::Async), PvStat::Ok);
    until("async get", || other.pv_get_complete(W));
    assert_eq!(other.var().w.get()[0], 6.5);
}

/// efTest in safe mode copies in the dirty channels synced to the flag.
#[test]
fn ef_test_in_safe_mode_reads_the_synced_channels() {
    let p = safe(2);
    p.var().n.set(8);
    p.ss[1].dirty[N.index()].store(true, Ordering::SeqCst);
    let s = ss(&p, 1);
    assert!(!s.ef_test(EfId(1)));
    assert_eq!(s.var().n.get(), 8);
    p.var().n.set(9);
    p.ss[1].dirty[N.index()].store(true, Ordering::SeqCst);
    assert!(!s.ef_test_and_clear(EfId(1)));
    assert_eq!(s.var().n.get(), 9);
}

/// A failed pvVarCreate logs C's line and leaves the channel anonymous
/// with its own count.
#[test]
fn pv_assign_create_failure_leaves_the_channel_anonymous() {
    // A database that is gone: every create fails, on either backend.
    let p = program_over(
        table(VarModel::Reentrant, 0, 1),
        PvSystem::create(&PvDatabase::new()),
        pv_db(),
    );
    assign(&p, W, "xxx:w", true);
    p.chans[W.index()].db_count.store(2, Ordering::Relaxed);
    let s = ss(&p, 0);
    let lines = logged("pvVarCreate()", || {
        assert_eq!(s.pv_assign(W, "yyy:w"), PvStat::Error)
    });
    assert_eq!(
        lines,
        [
            "sevr=fatal pvAssign(var w, pv yyy:w): pvVarCreate() failure: the IOC's CA client context is gone\n"
        ]
    );
    assert!(!s.pv_assigned(W));
    assert_eq!((s.pv_assign_count(), s.pv_connect_count()), (0, 0));
    assert_eq!(s.pv_count(W), 4);
}

/// check_pending's wait for another SYNC request ends at tmo even while
/// other events signal syncSem.
#[test]
fn check_pending_times_out_under_other_wakeups() {
    let p = reentrant(1);
    assign(&p, N, "xxx:n", true);
    pend(&p, PvEventType::Get, 0, N);
    let stop = Arc::new(AtomicBool::new(false));
    let waker = {
        let (p, stop) = (Arc::clone(&p), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                p.ss[0].sync_sem.signal();
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        })
    };
    let start = std::time::Instant::now();
    let status = ss(&p, 0).pv_get_tmo(N, CompType::Sync, 0.1);
    let took = start.elapsed();
    stop.store(true, Ordering::SeqCst);
    waker.join().unwrap();
    assert_eq!(status, PvStat::Timeout);
    assert!(
        took.as_secs_f64() >= 0.1 && took.as_secs_f64() < 2.0,
        "{took:?}"
    );
}

/// `assign n to "xxx:n"; monitor n;` then `pvAssign(n, "yyy:n")` or
/// `pvAssign(n, "")`, and more after it: C's seqShow still says 1
/// monitored. `n` is declared monitored, so a pvAssign that counted as
/// `init_chan` does would show.
#[test]
fn pv_assign_keeps_the_monitor_count_init_chan_counted() {
    for name in ["yyy:n", ""] {
        let p = reentrant(1);
        let monitored = p.chans[N.index()].def.monitored;
        assert!(monitored);
        p.dynamic.lock().init_dbch(N, "xxx:n".into(), monitored);
        let s = ss(&p, 0);
        for (then, name) in [("xxx:n", name), (name, "zzz:n"), ("zzz:n", "")] {
            assert_eq!(s.pv_assign(N, name), PvStat::Ok);
            assert_eq!(
                p.dynamic.lock().monitor_count(),
                1,
                "pvAssign {name:?} after {then:?}"
            );
        }
        assert_eq!(p.dynamic.lock().assign_count(), 0);
    }
}
