//! Starting a program (`seq_main.c`).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32};
use std::sync::{Arc, OnceLock};

use epics_base_rs::runtime::log::{ErrlogSevEnum, errlog_sev_vprintf};
use epics_base_rs::runtime::stdlib::{scanf_ulong, ulong_to_uint};
use epics_base_rs::runtime::sync::start_latch;
use epics_base_rs::runtime::task::{MandatoryThread, StackSizeClass, StackSpec, ThreadPriority};
use epics_base_rs::server::builtin_devices::timestamp::epics_time_to_strftime_in;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::WallTime;
use parking_lot::Mutex;

use crate::cmd::seq_register_sequencer_program;
use crate::program::{SEQ_MAGIC, SeqProgram, VarModel};
use crate::pv::{PvSystem, pv_size_n};
use crate::pvt::{
    Chan, ProgDyn, Program, PvMeta, SeqEvent, SeqMacros, SeqQueue, Sscb, VarBlock,
    create_or_attach_pv_system,
};
use crate::types::{ChId, NOEVFLAG, nwords};
use crate::var::ProgramVars;

/// `epicsThreadId`: a state set thread's libcom `EPICS ID`, the value
/// `epicsThreadShowAll` lists. `seqShow` prints it `{:#x}` for C's `%p`,
/// and the shell commands parse it back as `findThread`'s `strtoul(, 16)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SeqThreadId(pub u64);

impl SeqThreadId {
    /// `epicsThreadGetIdSelf`.
    pub fn current() -> Self {
        SeqThreadId(epics_base_rs::runtime::task::current_thread_id())
    }
}

/// `seq(&prog, "macros", stackSize)`: start an instance of `prog` whose pv
/// names resolve through `db`'s CA client context. `None` when the start
/// is refused (the table fails [`SeqProgram::validate`], or no thread).
pub fn seq<V: ProgramVars>(
    prog: &'static SeqProgram<V>,
    macro_def: &str,
    stack_size: u32,
    db: &PvDatabase,
) -> Option<SeqThreadId> {
    seq_register_sequencer_program(prog);
    errlog_sev_vprintf(ErrlogSevEnum::Info, format_args!("{}\n", seq_release()));
    if prog.magic != SEQ_MAGIC {
        errlog_sev_vprintf(
            ErrlogSevEnum::Fatal,
            format_args!(
                "seq: illegal magic number in state program.\n      \
                 - probable mismatch between SNC & SEQ versions\n      \
                 - re-compile your program?\n"
            ),
        );
        return None;
    }
    if let Err(e) = prog.validate() {
        errlog_sev_vprintf(ErrlogSevEnum::Fatal, format_args!("seq: {e}\n"));
        return None;
    }
    let init = InitProg::new(prog, SeqMacros::new(prog.params, macro_def))?;
    let (name, priority, stack_size) = thread_params(&init.macros, prog.prog_name, stack_size);

    let (started, wait) = start_latch();
    let tid = Arc::new(OnceLock::new());
    let started_tid = tid.clone();
    let db = db.clone();
    let thread_name = name.clone();
    let spawned = MandatoryThread::new(
        name.clone(),
        ThreadPriority::Custom(priority),
        StackSizeClass::Big,
    )
    .try_spawn_sized(StackSpec::Bytes(stack_size as usize), move || {
        let _ = started_tid.set(SeqThreadId::current());
        drop(started);
        let pv_sys = create_or_attach_pv_system(&db);
        init.build(pv_sys, db, priority, stack_size)
            .sequencer(&thread_name);
    });
    if spawned.is_err() {
        errlog_sev_vprintf(
            ErrlogSevEnum::Fatal,
            format_args!("seq: epicsThreadCreate failed"),
        );
        return None;
    }
    wait.wait();
    let tid = *tid.get()?;
    errlog_sev_vprintf(
        ErrlogSevEnum::Info,
        format_args!(
            "Spawning sequencer program \"{}\", thread {:#x}: \"{name}\"\n",
            prog.prog_name, tid.0
        ),
    );
    Some(tid)
}

/// `SEQ_RELEASE` (`seq_release.pl`): this crate's version and its build
/// time as Perl's `localtime` prints it.
fn seq_release() -> String {
    let epoch = env!("SEQ_RS_BUILD_EPOCH").parse().unwrap_or(0);
    format!(
        "Sequencer release {}, compiled {}",
        env!("CARGO_PKG_VERSION"),
        epics_time_to_strftime_in(64, "%a %b %e %H:%M:%S %Y", WallTime::from_unix(epoch, 0))
    )
}

/// `THREAD_PRIORITY` (`seqPvt.h`): `seq`'s default and its ceiling.
const THREAD_PRIORITY: u32 = ThreadPriority::Medium.value();

/// The program thread's name, priority and stack size: `seq`'s
/// `stackSize` (0 for `THREAD_STACK_SIZE`) under the `name`, `priority`
/// and `stack` macros, the priority at most [`THREAD_PRIORITY`] and the
/// stack at least `epicsThreadStackSmall`.
fn thread_params(macros: &SeqMacros, prog_name: &str, stack_size: u32) -> (String, u32, u32) {
    let mut stack_size = match stack_size {
        0 => StackSizeClass::Big.bytes() as u32,
        n => n,
    };
    if let Some(v) = macro_uint(macros, "stack") {
        stack_size = v;
    }
    let stack_size = stack_size.max(StackSizeClass::Small.bytes() as u32);
    let name = match macros.value("name") {
        Some(name) if !name.is_empty() => name,
        _ => prog_name,
    };
    let priority = macro_uint(macros, "priority")
        .unwrap_or(THREAD_PRIORITY)
        .min(THREAD_PRIORITY);
    (name.to_owned(), priority, stack_size)
}

/// `sscanf(str, "%ud", &v)` on a non-empty macro value; `None` leaves `v`
/// as it was.
fn macro_uint(macros: &SeqMacros, name: &str) -> Option<u32> {
    let s = macros.value(name).filter(|s| !s.is_empty())?;
    scanf_ulong(s, 10).map(|(v, _)| ulong_to_uint(v))
}

/// `init_chan`'s name buffer.
const NAME_BUFFER: usize = 100;

/// What `init_sprog` and `init_chan` make in `seq`, where a failure
/// refuses the start. [`InitProg::build`] finishes the [`Program`] on its
/// thread after `seqg_init`, so a `+s` copy is of the initialised block.
pub(crate) struct InitProg<V: 'static> {
    table: &'static SeqProgram<V>,
    macros: SeqMacros,
    chans: Vec<Chan<V>>,
    queues: Box<[SeqQueue]>,
    dynamic: ProgDyn,
}

impl<V: ProgramVars> InitProg<V> {
    /// `init_sprog`'s tables and `init_chan`; `None` after C's errlog line.
    pub(crate) fn new(table: &'static SeqProgram<V>, macros: SeqMacros) -> Option<Self> {
        let nch = table.chan.len();
        let mut d = ProgDyn::new(
            nch,
            vec![0; nwords(table.num_ev_flags)].into(),
            vec![Vec::new(); table.num_ev_flags as usize + 1].into(),
            table.chan.iter().map(|c| c.ef_id).collect(),
        );
        // Each queue with its `seqQueueElemSize`.
        let mut queues: Vec<Option<(SeqQueue, usize)>> =
            (0..table.num_queues).map(|_| None).collect();
        let mut chans = Vec::with_capacity(nch);
        for (n, def) in table.chan.iter().enumerate() {
            if def.ef_id != NOEVFLAG {
                // C prepends to the flag's list.
                d.synced_chans[def.ef_id.0 as usize].insert(0, ChId(n as u32));
            }
            let name = def
                .ch_name
                .map_or_else(String::new, |s| macros.eval(s, NAME_BUFFER));
            let named = !name.is_empty();
            if named {
                d.init_dbch(ChId(n as u32), name, def.monitored);
            }
            if def.queue_size > 0 {
                let size = pv_size_n(def.var_type.get_type(), def.count);
                match &queues[def.queue_index as usize] {
                    None => {
                        let Some(q) = SeqQueue::new(def.queue_size as usize) else {
                            errlog_sev_vprintf(
                                ErrlogSevEnum::Fatal,
                                format_args!("init_chan: seqQueueCreate failed\n"),
                            );
                            return None;
                        };
                        queues[def.queue_index as usize] = Some((q, size));
                    }
                    Some((q, elem_size))
                        if q.num_elems != def.queue_size as usize || *elem_size != size =>
                    {
                        errlog_sev_vprintf(
                            ErrlogSevEnum::Fatal,
                            format_args!(
                                "init_chan(varname={}): inconsistent shared queue definitions\n",
                                def.var_name
                            ),
                        );
                        return None;
                    }
                    Some(_) => {}
                }
            }
            chans.push(Chan {
                def,
                monitored: AtomicBool::new(def.monitored),
                // A named channel's `dbCount` is 0 until it connects.
                db_count: AtomicU32::new(if named { 0 } else { def.count }),
                var_lock: Mutex::new(PvMeta::zeroed()),
            });
        }
        // An index no channel names stays empty, C's NULL queue.
        let queues = queues
            .into_iter()
            .map(|q| match q {
                Some((q, _)) => q,
                None => SeqQueue {
                    num_elems: 0,
                    elems: Mutex::new(VecDeque::new()),
                },
            })
            .collect();
        Some(InitProg {
            table,
            macros,
            chans,
            queues,
            dynamic: d,
        })
    }

    /// The rest of `init_sprog` and `init_sscb`, then `sequencer`'s
    /// `seqg_init` and `+s` copies.
    pub(crate) fn build(
        self,
        pv_sys: PvSystem,
        db: PvDatabase,
        thread_priority: u32,
        stack_size: u32,
    ) -> Arc<Program<V>> {
        let table = self.table;
        let vars = match table.vars {
            VarModel::Static(block) => VarBlock::Static(block()),
            VarModel::Reentrant | VarModel::Safe => VarBlock::Owned(Box::new(V::zeroed())),
        };
        let var: &V = match &vars {
            VarBlock::Static(v) => v,
            VarBlock::Owned(v) => v,
        };
        (table.init_func)(var);
        let safe = table.vars.safe();
        let nch = self.chans.len();
        let safe_nch = if safe { nch } else { 0 };
        let ss = table
            .ss
            .iter()
            .map(|def| Sscb {
                def,
                var: safe.then(|| var.clone()),
                thread_id: OnceLock::new(),
                current_state: AtomicI32::new(0),
                next_state: AtomicI32::new(0),
                prev_state: AtomicI32::new(0),
                mask: Mutex::new(&[]),
                time_entered: Mutex::new(f64::INFINITY),
                wakeup_time: Mutex::new(f64::INFINITY),
                sync_sem: SeqEvent::default(),
                dead: SeqEvent::default(),
                get_req: (0..nch).map(|_| Mutex::new(None)).collect(),
                put_req: (0..nch).map(|_| Mutex::new(None)).collect(),
                meta: (0..safe_nch)
                    .map(|_| Mutex::new(PvMeta::zeroed()))
                    .collect(),
                dirty: (0..safe_nch).map(|_| AtomicBool::new(false)).collect(),
            })
            .collect();
        Arc::new(Program {
            table,
            vars,
            instance: AtomicU32::new(0),
            thread_priority,
            stack_size,
            gate: pv_sys.gate(),
            pv_sys,
            db,
            chans: self.chans.into(),
            queues: self.queues,
            ss,
            macros: self.macros,
            dynamic: Mutex::new(self.dynamic),
            die: AtomicBool::new(false),
            ready: SeqEvent::default(),
            dead: SeqEvent::default(),
        })
    }
}

/// A program table and builds of it, for the run-time's unit tests.
#[cfg(test)]
pub(crate) mod fixture {
    use super::*;
    use crate::program::{ProgOptions, SeqChan, SeqSs, SeqState, StateOptions};
    use crate::state_set::StateSet;
    use crate::types::{PrimType, SeqMask};
    use crate::var::{SeqVar, Zeroed};

    #[derive(Clone)]
    pub(crate) struct Vars {
        pub(crate) n: SeqVar<i32>,
        pub(crate) w: SeqVar<[f64; 4]>,
    }

    impl ProgramVars for Vars {
        fn zeroed() -> Self {
            Vars {
                n: Zeroed::zeroed(),
                w: Zeroed::zeroed(),
            }
        }
    }

    fn event(_: &StateSet<Vars>, _: &mut usize, _: &mut usize) -> bool {
        false
    }

    fn action(_: &StateSet<Vars>, _: usize, _: &mut usize) {}

    /// `seqg_init`: `n` starts at 7.
    fn init(v: &Vars) {
        v.n.set(7);
    }

    /// `int n` on `ch_name`, with no sync and no queue.
    pub(crate) fn chan(ch_name: Option<&'static str>, monitored: bool) -> SeqChan<Vars> {
        SeqChan {
            ch_name,
            var: |v| &v.n,
            var_name: "n",
            var_type: PrimType::Int,
            count: 1,
            event_num: 0,
            ef_id: NOEVFLAG,
            monitored,
            queue_size: 0,
            queue_index: 0,
        }
    }

    /// `double w[4]` on `ch_name`.
    pub(crate) fn w_chan(ch_name: Option<&'static str>) -> SeqChan<Vars> {
        SeqChan {
            var: |v| &v.w,
            var_name: "w",
            var_type: PrimType::Double,
            count: 4,
            ..chan(ch_name, true)
        }
    }

    /// [`program`], leaked.
    pub(crate) fn table(
        chan: Vec<SeqChan<Vars>>,
        vars: VarModel<Vars>,
        params: &'static str,
        num_ss: usize,
    ) -> &'static SeqProgram<Vars> {
        Box::leak(Box::new(program(chan, vars, params, num_ss)))
    }

    /// Two event flags, `num_ss` state sets of one state each.
    pub(crate) fn program(
        chan: Vec<SeqChan<Vars>>,
        vars: VarModel<Vars>,
        params: &'static str,
        num_ss: usize,
    ) -> SeqProgram<Vars> {
        let words = nwords(2 + chan.len() as u32);
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
        let ss: Vec<SeqSs<Vars>> = (0..num_ss)
            .map(|_| SeqSs {
                ss_name: "ss",
                states,
            })
            .collect();
        SeqProgram {
            magic: SEQ_MAGIC,
            prog_name: "t",
            chan: Box::leak(chan.into_boxed_slice()),
            ss: Box::leak(ss.into_boxed_slice()),
            vars,
            params,
            num_ev_flags: 2,
            options: ProgOptions::new(0),
            init_func: init,
            entry_func: None,
            exit_func: None,
            num_queues: 0,
        }
    }

    /// `table` built with no macros, its thread at priority 50 with a
    /// 4096-byte stack, its pvs over a database that is gone.
    pub(crate) fn build(table: &'static SeqProgram<Vars>) -> Arc<Program<Vars>> {
        InitProg::new(table, SeqMacros::default()).unwrap().build(
            PvSystem::create(&PvDatabase::new()),
            PvDatabase::new(),
            50,
            4096,
        )
    }

    /// [`build`] with its pvs on `db`.
    pub(crate) fn build_on(
        table: &'static SeqProgram<Vars>,
        db: &PvDatabase,
    ) -> Arc<Program<Vars>> {
        InitProg::new(table, SeqMacros::default()).unwrap().build(
            PvSystem::create(db),
            db.clone(),
            50,
            4096,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use epics_base_rs::runtime::general_time::EPICS_EPOCH_UNIX_SECS;

    use super::fixture::{Vars, build as build_table, chan, table, w_chan};
    use super::*;
    use crate::program::SeqChan;
    use crate::types::{EfId, PvSevr, PvStat};

    fn init_prog(chan: Vec<SeqChan<Vars>>, macro_def: &str) -> InitProg<Vars> {
        let t = table(chan, VarModel::Reentrant, "P=xxx:", 1);
        InitProg::new(t, SeqMacros::new(t.params, macro_def)).unwrap()
    }

    fn build(vars: VarModel<Vars>, num_ss: usize) -> Arc<Program<Vars>> {
        build_table(table(
            vec![chan(Some("a"), true), w_chan(None)],
            vars,
            "",
            num_ss,
        ))
    }

    fn name(p: &InitProg<Vars>, n: usize) -> Option<&str> {
        p.dynamic.dbch(ChId(n as u32)).map(|c| c.db_name.as_str())
    }

    #[test]
    fn a_channel_name_goes_through_the_macros() {
        let p = init_prog(vec![chan(Some("{P}n"), true)], "P=ioc:");
        assert_eq!(name(&p, 0), Some("ioc:n"));
        assert_eq!(p.dynamic.assign_count(), 1);
    }

    /// `assign x;`, `assign x to ""` and a name whose macros expand to
    /// nothing all wait for `pvAssign`.
    #[test]
    fn an_absent_or_empty_name_leaves_the_channel_anonymous() {
        let p = init_prog(
            vec![
                chan(None, true),
                chan(Some(""), true),
                chan(Some("{Q}"), true),
            ],
            "",
        );
        assert!((0..3).all(|n| name(&p, n).is_none()));
        assert_eq!(p.dynamic.assign_count(), 0);
        assert_eq!(p.dynamic.monitor_count(), 0);
    }

    #[test]
    fn a_name_keeps_99_bytes() {
        let p = init_prog(
            vec![chan(Some("{L}"), true)],
            &format!("L={}", "x".repeat(120)),
        );
        assert_eq!(name(&p, 0), Some("x".repeat(99).as_str()));
    }

    #[test]
    fn only_a_named_monitored_channel_counts_as_monitored() {
        let p = init_prog(
            vec![
                chan(Some("a"), true),
                chan(Some("b"), false),
                chan(None, true),
            ],
            "",
        );
        assert_eq!(p.dynamic.assign_count(), 2);
        assert_eq!(p.dynamic.monitor_count(), 1);
        assert_eq!(p.dynamic.connect_count(), 0);
    }

    /// C's `ch->dbch ? ch->dbch->dbCount : ch->count` before any connect.
    #[test]
    fn db_count_is_0_named_and_the_count_anonymous() {
        let p = init_prog(vec![w_chan(Some("a")), w_chan(None)], "");
        let count = |n: usize| {
            p.chans[n]
                .db_count
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        assert_eq!((count(0), count(1)), (0, 4));
    }

    #[test]
    fn a_flag_lists_its_last_synced_channel_first() {
        let synced = |ef| SeqChan {
            ef_id: EfId(ef),
            ..chan(None, true)
        };
        let p = init_prog(vec![synced(1), synced(2), synced(1), chan(None, true)], "");
        let d = &p.dynamic;
        assert_eq!(d.synced_chans.len(), 3);
        assert_eq!(d.synced_chans[1], [ChId(2), ChId(0)]);
        assert_eq!(d.synced_chans[2], [ChId(1)]);
        assert_eq!(&*d.synced_to, &[EfId(1), EfId(2), EfId(1), NOEVFLAG]);
        assert_eq!(d.ev_flags.len(), nwords(2));
    }

    #[test]
    fn every_channel_starts_with_zeroed_meta() {
        let p = init_prog(vec![chan(Some("a"), true)], "");
        let meta = p.chans[0].var_lock.lock();
        assert_eq!(
            meta.timestamp,
            WallTime::from_unix(EPICS_EPOCH_UNIX_SECS, 0)
        );
        assert_eq!(
            (meta.status, meta.severity, meta.message.is_null()),
            (PvStat::Ok, PvSevr::None, true)
        );
    }

    #[test]
    fn safe_mode_copies_the_initialised_block_to_each_state_set() {
        let p = build(VarModel::Safe, 2);
        p.var().n.set(8);
        for ss in p.ss.iter() {
            assert_eq!(ss.var.as_ref().unwrap().n.get(), 7);
            assert_eq!((ss.meta.len(), ss.dirty.len()), (2, 2));
        }
    }

    #[test]
    fn reentrant_mode_has_one_initialised_block() {
        let p = build(VarModel::Reentrant, 2);
        assert_eq!(p.var().n.get(), 7);
        for ss in p.ss.iter() {
            assert!(ss.var.is_none());
            assert_eq!((ss.meta.len(), ss.dirty.len()), (0, 0));
        }
    }

    #[test]
    fn static_mode_starts_share_the_block() {
        static BLOCK: LazyLock<Vars> = LazyLock::new(Vars::zeroed);
        let a = build(VarModel::Static(|| &BLOCK), 1);
        let b = build(VarModel::Static(|| &BLOCK), 1);
        assert!(std::ptr::eq(a.var(), b.var()));
        assert_eq!(BLOCK.n.get(), 7);
    }

    /// `init_sscb`: state 0, no mask, timers at infinity, no thread yet.
    #[test]
    fn a_state_set_starts_idle() {
        let p = build(VarModel::Reentrant, 1);
        let ss = &p.ss[0];
        use std::sync::atomic::Ordering::Relaxed;
        assert_eq!(
            (
                ss.current_state.load(Relaxed),
                ss.next_state.load(Relaxed),
                ss.prev_state.load(Relaxed)
            ),
            (0, 0, 0)
        );
        assert!(ss.mask.lock().is_empty());
        assert_eq!(*ss.time_entered.lock(), f64::INFINITY);
        assert_eq!(*ss.wakeup_time.lock(), f64::INFINITY);
        assert!(ss.thread_id.get().is_none());
        assert_eq!((ss.get_req.len(), ss.put_req.len()), (2, 2));
        assert_eq!((p.thread_priority, p.stack_size), (50, 4096));
    }

    fn params(macro_def: &str, stack_size: u32) -> (String, u32, u32) {
        thread_params(&SeqMacros::new("", macro_def), "prog", stack_size)
    }

    fn bytes(class: StackSizeClass) -> u32 {
        class.bytes() as u32
    }

    #[test]
    fn with_no_macros_the_thread_is_the_program_s_at_medium_on_a_big_stack() {
        assert_eq!(
            params("", 0),
            ("prog".into(), 50, bytes(StackSizeClass::Big))
        );
        assert_eq!(params("name=", 0).0, "prog");
    }

    #[test]
    fn the_name_macro_names_the_thread() {
        assert_eq!(params("name=seq1", 0).0, "seq1");
    }

    #[test]
    fn the_stack_macro_overrides_the_argument_down_to_a_small_stack() {
        let small = bytes(StackSizeClass::Small);
        assert_eq!(params("", small + 1).2, small + 1);
        assert_eq!(params(&format!("stack={}", small + 7), 4).2, small + 7);
        assert_eq!(params("stack=1", 0).2, small);
        assert_eq!(params("", 3).2, small);
    }

    /// `%u` leaves the value on a matching failure, and wraps a negative.
    #[test]
    fn a_macro_scanf_cannot_read_leaves_the_value() {
        let big = bytes(StackSizeClass::Big);
        assert_eq!(params("stack=x", 0).2, big);
        assert_eq!(params("stack=", 0).2, big);
        assert_eq!(params("priority=x", 0).1, 50);
        assert_eq!(params("stack=-1", 0).2, u32::MAX);
    }

    #[test]
    fn the_priority_macro_is_capped_at_medium() {
        assert_eq!(params("priority=20", 0).1, 20);
        assert_eq!(params("priority=90", 0).1, 50);
        assert_eq!(params("priority=-1", 0).1, 50);
    }

    /// `seq_release.pl`: Perl's scalar `localtime`, the day space-padded.
    #[test]
    fn the_banner_is_the_release_and_a_ctime_date() {
        let banner = seq_release();
        let date = banner
            .strip_prefix(&format!(
                "Sequencer release {}, compiled ",
                env!("CARGO_PKG_VERSION")
            ))
            .unwrap();
        let fields: Vec<&str> = date.split_whitespace().collect();
        assert_eq!(fields.len(), 5, "{date}");
        assert_eq!(date.len(), 24, "{date}");
        assert!(!date.contains('%'), "{date}");
    }
}
