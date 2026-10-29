//! The query commands (`seq_qry.c`). A command finds its instance with
//! [`seq_find_state_set`] and calls the routine of the same name here
//! through [`ProgInstance`]; with no instance, it lists them all, as
//! `seqQryFind` does.
//!
//! C prints here and in `seq_cmd.c` with plain `printf`, and reads the
//! pager's answer with `fgets(stdin)`: the process's own streams, which no
//! iocsh redirect swaps. A failed write is not reported and does not stop
//! the caller, as a failed `printf` does not.

use std::cell::RefCell;
use std::ffi::c_char;
use std::fmt;
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use epics_base_rs::runtime::stdlib::atoi;
use epics_base_rs::runtime::task::thread_by_id;
use epics_base_rs::server::builtin_devices::timestamp::epics_time_to_strftime_in;
use epics_base_rs::types::EpicsValue;
use epics_libcom_rs::cfmt::CFloat;

use crate::c::CSrc;
use crate::prog::{seq_find_state_set, seq_traverse_prog};
use crate::program::ProgOptions;
use crate::pv::{pv_size_n, pv_time_get_current_double};
use crate::pvt::{ProgInstance, Program};
use crate::seq::SeqThreadId;
use crate::state_set::StateSet;
use crate::types::{ChId, MAX_STRING_SIZE, NOEVFLAG};
use crate::var::ProgramVars;

struct Console {
    out: Box<dyn Write>,
    input: Box<dyn BufRead>,
}

thread_local! {
    /// What a test lends in place of the process's streams.
    static CONSOLE: RefCell<Option<Console>> = const { RefCell::new(None) };
}

/// `f` on C's `stdout`. `f` must not print through [`pr`] itself.
pub(crate) fn with_stdout<R>(f: impl FnOnce(&mut dyn Write) -> R) -> R {
    CONSOLE.with_borrow_mut(|c| match c {
        Some(c) => f(&mut c.out),
        None => f(&mut io::stdout()),
    })
}

/// `f` on C's `stdin`.
fn with_stdin<R>(f: impl FnOnce(&mut dyn BufRead) -> R) -> R {
    CONSOLE.with_borrow_mut(|c| match c {
        Some(c) => f(&mut c.input),
        None => f(&mut io::stdin().lock()),
    })
}

/// `printf` on C's `stdout`.
macro_rules! pr {
    ($($arg:tt)*) => {
        $crate::qry::with_stdout(|out| {
            let _ = ::std::io::Write::write_fmt(out, format_args!($($arg)*));
        })
    };
}
pub(crate) use pr;

/// What `f` prints, with `input` as its stdin.
#[cfg(test)]
pub(crate) fn capture(input: &[u8], f: impl FnOnce()) -> String {
    #[derive(Clone, Default)]
    struct Shared(std::rc::Rc<RefCell<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let buf = Shared::default();
    let prev = CONSOLE.replace(Some(Console {
        out: Box::new(buf.clone()),
        input: Box::new(io::Cursor::new(input.to_vec())),
    }));
    f();
    CONSOLE.set(prev);
    String::from_utf8(buf.0.take()).unwrap()
}

/// `%-<width>s`, whose width counts bytes.
fn pr_padded(s: &[u8], width: usize) {
    with_stdout(|out| {
        let _ = out.write_all(s);
        let _ = write!(out, "{:1$}", "", width.saturating_sub(s.len()));
    })
}

/// glibc's `%p` of an `epicsThreadId`.
struct Ptr(Option<SeqThreadId>);

impl fmt::Display for Ptr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(tid) => f.pad(&format!("{:#x}", tid.0)),
            None => f.pad("(nil)"),
        }
    }
}

/// `THREAD_NAME_SIZE` (`seqPvt.h`).
const THREAD_NAME_SIZE: usize = 32;

/// `epicsThreadGetName` into a `THREAD_NAME_SIZE` buffer; empty for a
/// thread that is gone.
fn thread_name(tid: SeqThreadId) -> Vec<u8> {
    let mut name = thread_by_id(tid.0)
        .map(|t| t.name().as_bytes().to_vec())
        .unwrap_or_default();
    name.truncate(THREAD_NAME_SIZE - 1);
    name
}

/// `fgets(buf, size, stdin)`: up to `size - 1` bytes, ending after a
/// newline; what is left of a longer line stays for the next read.
fn fgets(input: &mut dyn BufRead, size: usize) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    while line.len() < size - 1 && line.last() != Some(&b'\n') {
        let buf = input.fill_buf().ok()?;
        if buf.is_empty() {
            break;
        }
        let room = &buf[..buf.len().min(size - 1 - line.len())];
        let n = room
            .iter()
            .position(|&b| b == b'\n')
            .map_or(room.len(), |i| i + 1);
        line.extend_from_slice(&room[..n]);
        input.consume(n);
    }
    (!line.is_empty()).then_some(line)
}

/// `userInput`: how far to move, 0 to quit.
fn user_input() -> i32 {
    pr!("Next? (+/- skip count)\n");
    let Some(mut buf) = with_stdin(|input| fgets(input, 10)) else {
        return 0;
    };
    if buf[0] == b'\n' {
        return 1;
    }
    if (buf[0] == b'-' || buf[0] == b'+') && buf.get(1) == Some(&b'\n') {
        buf[1] = b'1';
    }
    atoi(&buf)
}

/// `printValue` of what [`crate::ChanCell::load`] returns, which is a
/// `putType` value.
fn print_value(value: &EpicsValue) {
    let char_ = |x: &u8| pr!(" {}", *x as c_char);
    let int = |x: i32| pr!(" {x}");
    let float = |x: f64| pr!(" {}", CFloat(x).g());
    let string = |s: &[u8]| {
        let s = &s[..s.len().min(MAX_STRING_SIZE)];
        let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
        pr!(" \"");
        pr_padded(&s[..end], 0);
        pr!("\"");
    };
    match value {
        EpicsValue::Char(x) => char_(x),
        EpicsValue::CharArray(a) => a.iter().for_each(char_),
        EpicsValue::Short(x) => int((*x).into()),
        EpicsValue::ShortArray(a) => a.iter().for_each(|&x| int(x.into())),
        EpicsValue::Long(x) => int(*x),
        EpicsValue::LongArray(a) => a.iter().for_each(|&x| int(x)),
        EpicsValue::Float(x) => float((*x).into()),
        EpicsValue::FloatArray(a) => a.iter().for_each(|&x| float(x.into())),
        EpicsValue::Double(x) => float(*x),
        EpicsValue::DoubleArray(a) => a.iter().for_each(|&x| float(x)),
        EpicsValue::String(s) => string(s.as_bytes()),
        EpicsValue::StringArray(a) => a.iter().for_each(|s| string(s.as_bytes())),
        // C's switch on any other type prints no element.
        _ => {}
    }
    pr!("\n");
}

/// `seqShowAll`.
fn show_all() {
    let mut prog_count = 0;
    seq_traverse_prog(|sp| {
        sp.show_sp(&mut prog_count);
        false
    });
    if prog_count == 0 {
        pr!("No active state programs\n");
    }
}

/// `seqQryFind`.
fn qry_find(tid: Option<SeqThreadId>) -> Option<(Arc<dyn ProgInstance>, usize)> {
    let found = tid.and_then(seq_find_state_set);
    if found.is_none() {
        if tid.is_some() {
            pr!("No program instance is running thread {}.\n", Ptr(tid));
        }
        show_all();
    }
    found
}

/// `struct seqStats`, which `seqcar` gathers over every instance.
#[derive(Debug, Default)]
pub struct SeqStats {
    pub level: i32,
    pub n_progs: u32,
    pub n_chans: u32,
    pub n_conn: u32,
}

/// `seqShow`: the instance running `tid`, or every instance.
pub fn seq_show(tid: Option<SeqThreadId>) {
    if let Some((sp, _)) = qry_find(tid) {
        sp.show();
    }
}

/// `seqChanShow(tid, "str")`.
pub fn seq_chan_show(tid: Option<SeqThreadId>, chan: Option<&str>) {
    if let Some((sp, ss)) = qry_find(tid) {
        sp.chan_show(ss, chan);
    }
}

/// `seqTraverseProg(seqcarCollect)` at `level`.
fn car_stats(level: i32) -> SeqStats {
    let mut stats = SeqStats {
        level,
        ..SeqStats::default()
    };
    seq_traverse_prog(|sp| {
        sp.car_collect(&mut stats);
        false
    });
    stats
}

/// `seqcar(level)`.
pub fn seqcar(level: i32) {
    let stats = car_stats(level);
    pr!(
        "Total programs={}, channels={}, connected={}, disconnected={}\n",
        stats.n_progs,
        stats.n_chans,
        stats.n_conn,
        stats.n_chans - stats.n_conn
    );
}

/// `seqGatherStats` (`seqStats.h`): `seqcar`'s totals, printing nothing.
pub fn seq_gather_stats() -> SeqStats {
    car_stats(0)
}

/// `seqQueueShow`.
pub fn seq_queue_show(tid: Option<SeqThreadId>) {
    if let Some((sp, _)) = qry_find(tid) {
        sp.queue_show();
    }
}

/// What the queries read of a channel's `dbch`, which C reads unlocked.
struct DbView {
    db_name: String,
    connected: bool,
}

impl<V: ProgramVars> Program<V> {
    fn db_view(&self, n: usize) -> Option<DbView> {
        self.dynamic.lock().dbch(ChId(n as u32)).map(|d| DbView {
            db_name: d.db_name.clone(),
            connected: d.connected(),
        })
    }

    /// `seqQueueElemSize` of queue `nq`: `seq_main.c` creates it with
    /// `pv_size_n(getType, count)` of the first channel that names it.
    fn queue_elem_size(&self, nq: usize) -> usize {
        self.table
            .chan
            .iter()
            .find(|def| def.queue_size > 0 && def.queue_index as usize == nq)
            .map_or(0, |def| pv_size_n(def.var_type.get_type(), def.count))
    }

    /// `seqShow`'s report on this instance and every state set, whose
    /// `pv_get_complete` / `pv_put_complete` it calls (`seq_qry.c:96-106`).
    pub fn show(self: &Arc<Self>) {
        self.show_with(&pv_time_get_current_double)
    }

    /// [`Self::show`] reading the time from `now`. The user variables'
    /// length is `size_of::<V>()`, the Rust block's.
    fn show_with(self: &Arc<Self>, now: &dyn Fn() -> f64) {
        let table = self.table;
        let opt = |bit| i32::from(table.options.test(bit));
        let reent = table.vars.reentrant();
        let safe = table.vars.safe();
        let var: *const V = self.var();
        pr!("State Program: \"{}\"\n", table.prog_name);
        pr!("  thread priority = {}\n", self.thread_priority);
        pr!("  number of state sets = {}\n", self.ss.len());
        pr!("  number of syncQ queues = {}\n", self.queues.len());
        if !self.queues.is_empty() {
            pr!("  queue array address = {:p}\n", self.queues.as_ptr());
        }
        pr!("  number of channels = {}\n", self.chans.len());
        let (assign_count, connect_count, monitor_count) = {
            let d = self.dynamic.lock();
            (d.assign_count(), d.connect_count(), d.monitor_count())
        };
        pr!("  number of channels assigned = {assign_count}\n");
        pr!("  number of channels connected = {connect_count}\n");
        pr!("  number of channels monitored = {monitor_count}\n");
        pr!(
            "  options: async={}, debug={}, newef={}, reent={}, conn={}\n",
            opt(ProgOptions::ASYNC),
            opt(ProgOptions::DEBUG),
            opt(ProgOptions::NEWEF),
            i32::from(reent),
            opt(ProgOptions::CONN)
        );
        if reent {
            pr!(
                "  user variables: address = {var:p}, length = {}\n",
                size_of::<V>()
            );
        }
        pr!("\n");

        for (index, ss) in self.ss.iter().enumerate() {
            pr!("  State Set: \"{}\"\n", ss.def.ss_name);
            let tid = ss.thread_id.get().copied();
            if let Some(tid) = tid {
                pr!("  thread name = ");
                pr_padded(&thread_name(tid), 0);
                pr!(";");
            }
            pr!("  Thread id = {}\n", Ptr(tid));

            let states = ss.def.states;
            let state_name = |i: i32| {
                usize::try_from(i)
                    .ok()
                    .and_then(|i| states.get(i))
                    .map_or("", |st| st.state_name)
            };
            pr!("  First state = \"{}\"\n", states[0].state_name);
            pr!(
                "  Current state = \"{}\"\n",
                state_name(ss.current_state.load(Ordering::Relaxed))
            );
            pr!(
                "  Previous state = \"{}\"\n",
                state_name(ss.prev_state.load(Ordering::Relaxed))
            );

            let time_now = now();
            pr!(
                "  Elapsed time since state was entered = {:.2} seconds\n",
                CFloat(time_now - *ss.time_entered.lock())
            );
            pr!(
                "  Wake up delay = {:.2} seconds\n",
                CFloat(*ss.wakeup_time.lock() - time_now)
            );

            let st = StateSet {
                prog: self.clone(),
                index,
            };
            let listed = |&ch: &ChId| safe || st.pv_assigned(ch);
            let chans = || (0..self.chans.len() as u32).map(ChId).filter(listed);
            pr!("  Get in progress = [");
            for ch in chans() {
                pr!("{}", i32::from(!st.pv_get_complete(ch)));
            }
            pr!("]\n");
            pr!("  Put in progress = [");
            for ch in chans() {
                pr!("{}", i32::from(!st.pv_put_complete(ch, 1, false, None)));
            }
            pr!("]\n");

            if safe {
                pr!(
                    "  User variables: address = {var:p}, length = {}\n",
                    size_of::<V>()
                );
            }
            pr!("\n");
        }
    }

    /// `seqShowSP`: this instance's rows of `seqShowAll`, under the header
    /// the first instance (`*prog_count == 0`) prints.
    pub fn show_sp(&self, prog_count: &mut u32) {
        if *prog_count == 0 {
            pr!("Program Name        Thread ID           Thread Name         SS Name\n");
            pr!("------------        ---------           -----------         -------\n");
        }
        *prog_count += 1;
        let mut prog_name = self.table.prog_name;
        for ss in &self.ss {
            let tid = ss.thread_id.get().copied();
            let name = tid.map_or_else(|| b"(no thread)".to_vec(), thread_name);
            pr_padded(prog_name.as_bytes(), 19);
            pr!(" {:<19} ", Ptr(tid));
            pr_padded(&name, 19);
            pr!(" ");
            pr_padded(ss.def.ss_name.as_bytes(), 19);
            pr!("\n");
            prog_name = "";
        }
    }

    /// `seqChanShow` as state set `ss` sees the channels.
    pub fn chan_show(&self, ss: usize, chan: Option<&str>) {
        let sscb = &self.ss[ss];
        let num_chans = self.chans.len();
        pr!("State Program: \"{}\"\n", self.table.prog_name);
        pr!("Number of channels={num_chans}\n");
        pr!("View: State set {}\n", sscb.def.ss_name);

        let (conn_qual, filter) = match chan.map(str::as_bytes) {
            Some([q @ (b'+' | b'-'), rest @ ..]) => (Some(*q), Some(rest)),
            f => (None, f),
        };
        // `valPtr(ch, ss)`.
        let vars = sscb.var.as_ref().unwrap_or_else(|| self.var());

        let mut nch: i32 = 0;
        let mut dn = 1;
        while dn != 0 && nch >= 0 && (nch as usize) < num_chans {
            let n = nch as usize;
            let ch = &self.chans[n];
            let db = self.db_view(n);

            if let Some(filter) = filter {
                let show_all = match conn_qual {
                    Some(b'+') => db.as_ref().is_some_and(|d| d.connected),
                    Some(_) => db.as_ref().is_some_and(|d| !d.connected),
                    None => true,
                };
                let matched = filter.is_empty()
                    || db.as_ref().is_some_and(|d| {
                        d.db_name
                            .as_bytes()
                            .windows(filter.len())
                            .any(|w| w == filter)
                    });
                if !(matched && show_all) {
                    nch += 1;
                    continue;
                }
            }
            pr!("\n#{nch} of {num_chans}:\n");
            pr!("  Variable name: \"{}\"\n", ch.def.var_name);
            pr!("    type = {}\n", ch.def.var_type.c_name());
            pr!("    count = {}\n", ch.def.count);
            pr!("  Value =");
            print_value(&(ch.def.var)(vars).load(ch.def.count));

            match &db {
                Some(d) => pr!("  Assigned to \"{}\"\n", d.db_name),
                None => pr!("  Anonymous\n"),
            }
            if db.as_ref().is_some_and(|d| d.connected) {
                pr!("  Connected\n");
            } else {
                pr!("  Not connected\n");
            }
            if ch.monitored.load(Ordering::Relaxed) {
                pr!("  Monitored\n");
            } else {
                pr!("  Not monitored\n");
            }
            let synced_to = self.dynamic.lock().synced_to[n];
            if synced_to != NOEVFLAG {
                pr!("  Sync'ed to event flag {}\n", synced_to.0);
            } else {
                pr!("  Not sync'ed\n");
            }

            if db.is_some() {
                // `metaPtr(ch, ss)`.
                let meta = if self.table.vars.safe() {
                    sscb.meta[n].lock().clone()
                } else {
                    ch.var_lock.lock().clone()
                };
                pr!("  Status = {}\n", meta.status as i32);
                pr!("  Severity = {}\n", meta.severity as i32);
                let message = &meta.message;
                let message = if message.is_null() {
                    Vec::new()
                } else {
                    message.to_c_vec()
                };
                pr!("  Message = ");
                pr_padded(&message, 0);
                pr!("\n");
                let ts = epics_time_to_strftime_in(28, "%Y-%m-%d %H:%M:%S.%06f", meta.timestamp);
                pr!("  Time stamp = {ts}\n");
            }

            dn = user_input();
            nch = nch.wrapping_add(dn);
        }
    }

    /// `seqQueueShow` on this instance.
    pub fn queue_show(&self) {
        pr!("State Program: \"{}\"\n", self.table.prog_name);
        pr!("Number of queues = {}\n", self.queues.len());
        let mut nq: i32 = 0;
        let mut dn = 1;
        while dn != 0 && nq >= 0 && (nq as usize) < self.queues.len() {
            let q = &self.queues[nq as usize];
            pr!(
                "  Queue #{nq}: numElems={}, used={}, elemSize={}\n",
                q.num_elems(),
                q.used(),
                self.queue_elem_size(nq as usize)
            );
            dn = user_input();
            nq = nq.wrapping_add(dn);
        }
    }

    /// `seqcarCollect`.
    pub fn car_collect(&self, stats: &mut SeqStats) {
        stats.n_progs += 1;
        let mut printed_prog_name = false;
        for (n, ch) in self.chans.iter().enumerate() {
            let db = self.db_view(n);
            let connected = db.as_ref().is_some_and(|d| d.connected);
            if db.is_some() {
                stats.n_chans += 1;
            }
            if connected {
                stats.n_conn += 1;
            }
            if stats.level > 1 || (stats.level == 1 && db.is_some() && !connected) {
                if !printed_prog_name {
                    pr!("  Program \"{}\"\n", self.table.prog_name);
                    printed_prog_name = true;
                }
                match db {
                    Some(d) => pr!(
                        "    Variable \"{}\" {}connected to PV \"{}\"\n",
                        ch.def.var_name,
                        if d.connected { "" } else { "not " },
                        d.db_name
                    ),
                    None => pr!("    Variable \"{}\" not assigned to PV\n", ch.def.var_name),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::any::Any;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32};

    use epics_base_rs::runtime::task::{
        StackSizeClass, ThreadPriority, current_thread_id, spawn_dedicated_thread,
    };
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::types::WallTime;
    use parking_lot::Mutex;

    use crate::cmd::seq_register_sequencer_program;
    use crate::mac::SeqMacros;
    use crate::prog::{seq_add_prog, seq_del_prog};
    use crate::program::{SEQ_MAGIC, SeqChan, SeqProgram, SeqSs, SeqState, StateOptions, VarModel};
    use crate::pv::PvSystem;
    use crate::pvt::{Chan, ProgDyn, PvMeta, PvReq, SeqEvent, Sscb, VarBlock};
    use crate::types::{EfId, PrimType, PvSevr, PvStat};
    use crate::var::{ChanCell, ElemKind};

    /// Serialises the tests that put instances in the process-wide
    /// registry or print all of it.
    static REGISTRY: Mutex<()> = Mutex::new(());

    /// A cell whose value is fixed, as `SeqVar::load` is the pv layer's.
    #[derive(Clone)]
    struct Fixed(EpicsValue);

    impl ChanCell for Fixed {
        fn kind(&self) -> ElemKind {
            ElemKind::F64
        }

        fn count(&self) -> u32 {
            1
        }

        fn load(&self, _: u32) -> EpicsValue {
            self.0.clone()
        }

        fn store(&self, _: &EpicsValue, _: u32) {
            unreachable!()
        }

        fn copy_from(&self, _: &dyn ChanCell, _: u32) {
            unreachable!()
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[derive(Clone)]
    struct Vars {
        d: Fixed,
        c: Fixed,
        s: Fixed,
    }

    impl ProgramVars for Vars {
        fn zeroed() -> Self {
            Vars {
                d: Fixed(EpicsValue::DoubleArray(vec![1.5, -2.0].into())),
                c: Fixed(EpicsValue::Char(200)),
                s: Fixed(EpicsValue::String("hello".into())),
            }
        }
    }

    fn event(_: &StateSet<Vars>, _: &mut usize, _: &mut usize) -> bool {
        false
    }

    fn action(_: &StateSet<Vars>, _: usize, _: &mut usize) {}

    fn init(_: &Vars) {}

    fn leak<T>(t: T) -> &'static T {
        Box::leak(Box::new(t))
    }

    fn chan(
        var_name: &'static str,
        var: fn(&Vars) -> &dyn ChanCell,
        var_type: PrimType,
        count: u32,
    ) -> SeqChan<Vars> {
        SeqChan {
            ch_name: None,
            var,
            var_name,
            var_type,
            count,
            event_num: 0,
            ef_id: NOEVFLAG,
            monitored: false,
            queue_size: 0,
            queue_index: 0,
        }
    }

    /// `d` anonymous, `c` and `s` for [`assigned`].
    fn three_chans() -> Vec<SeqChan<Vars>> {
        vec![
            chan("d", |v| &v.d, PrimType::Double, 2),
            SeqChan {
                monitored: true,
                ..chan("c", |v| &v.c, PrimType::UChar, 1)
            },
            chan("s", |v| &v.s, PrimType::String, 1),
        ]
    }

    /// State sets `ss_names`, each with states `init` and `run`.
    fn table(
        prog_name: &'static str,
        vars: VarModel<Vars>,
        chan: Vec<SeqChan<Vars>>,
        ss_names: &[&'static str],
    ) -> SeqProgram<Vars> {
        let states: &'static [SeqState<Vars>] = Box::leak(
            ["init", "run"]
                .map(|state_name| SeqState {
                    state_name,
                    action_func: action,
                    event_func: event,
                    entry_func: None,
                    exit_func: None,
                    event_mask: &[],
                    options: StateOptions::new(0),
                })
                .into(),
        );
        let ss: Box<[SeqSs<Vars>]> = ss_names
            .iter()
            .map(|&ss_name| SeqSs { ss_name, states })
            .collect();
        SeqProgram {
            magic: SEQ_MAGIC,
            prog_name,
            chan: Box::leak(chan.into_boxed_slice()),
            ss: Box::leak(ss),
            vars,
            params: "",
            num_ev_flags: 2,
            options: ProgOptions::new(ProgOptions::CONN),
            init_func: init,
            entry_func: None,
            exit_func: None,
            num_queues: 0,
        }
    }

    fn meta(status: PvStat, severity: PvSevr, message: Option<&str>) -> PvMeta {
        PvMeta {
            timestamp: WallTime::UNIX_EPOCH,
            status,
            severity,
            message: message.map_or_else(crate::types::CharPtr::default, |m| {
                crate::types::CharPtr::from_bytes(m.as_bytes())
            }),
        }
    }

    fn disconnected() -> PvMeta {
        meta(PvStat::Disconn, PvSevr::Invalid, Some("disconnected"))
    }

    /// An instance of `table` with every channel anonymous and every state
    /// set threadless, in its first state.
    fn instance(table: &'static SeqProgram<Vars>) -> Program<Vars> {
        let n = table.chan.len();
        Program {
            table,
            vars: VarBlock::Owned(Box::new(Vars::zeroed())),
            instance: AtomicU32::new(0),
            thread_priority: 50,
            stack_size: 0,
            gate: Arc::default(),
            pv_sys: PvSystem::create(&PvDatabase::new()),
            db: PvDatabase::new(),
            chans: table
                .chan
                .iter()
                .map(|def| Chan {
                    def,
                    monitored: AtomicBool::new(def.monitored),
                    db_count: AtomicU32::new(def.count),
                    var_lock: Mutex::new(disconnected()),
                })
                .collect(),
            queues: Box::new([]),
            ss: table
                .ss
                .iter()
                .map(|def| Sscb {
                    def,
                    var: table.vars.safe().then(Vars::zeroed),
                    thread_id: OnceLock::new(),
                    current_state: AtomicI32::new(0),
                    next_state: AtomicI32::new(-1),
                    prev_state: AtomicI32::new(-1),
                    mask: Mutex::new(&[]),
                    time_entered: Mutex::new(0.0),
                    wakeup_time: Mutex::new(f64::INFINITY),
                    sync_sem: SeqEvent::default(),
                    dead: SeqEvent::default(),
                    get_req: (0..n).map(|_| Mutex::new(None)).collect(),
                    put_req: (0..n).map(|_| Mutex::new(None)).collect(),
                    meta: (0..n).map(|_| Mutex::new(disconnected())).collect(),
                    dirty: (0..n).map(|_| AtomicBool::new(false)).collect(),
                })
                .collect(),
            macros: SeqMacros(Vec::new()),
            dynamic: Mutex::new(ProgDyn::new(
                n,
                Box::new([0]),
                vec![Vec::new(); 3].into(),
                vec![NOEVFLAG; n].into(),
            )),
            die: AtomicBool::new(false),
            ready: SeqEvent::default(),
            dead: SeqEvent::default(),
        }
    }

    /// `names` (channel, pv) assigned as `init_chan` assigns them.
    fn assign(prog: &Program<Vars>, names: &[(u32, &str)]) {
        let mut d = prog.dynamic.lock();
        for &(ch, name) in names {
            let monitored = prog.chans[ch as usize].def.monitored;
            d.init_dbch(ChId(ch), name.into(), monitored);
        }
    }

    /// [`three_chans`] as `seq` and the pv layer leave them: `c` connected
    /// to `ioc:c`, synced to flag 2, its meta OK; `s` assigned to `ioc:s`
    /// and disconnected.
    fn assigned(prog: &Program<Vars>) {
        assign(prog, &[(1, "ioc:c"), (2, "ioc:s")]);
        let mut d = prog.dynamic.lock();
        d.set_connected(ChId(1), true);
        d.set_got_monitor(ChId(1), true);
        d.synced_to[1] = EfId(2);
        *prog.chans[1].var_lock.lock() = meta(PvStat::Ok, PvSevr::None, None);
    }

    /// `prog` in the registry, as `seq_add_prog` leaves a started instance,
    /// until dropped.
    struct Running(Arc<Program<Vars>>);

    impl Running {
        fn new(prog: Program<Vars>) -> Self {
            seq_register_sequencer_program(prog.table);
            let prog = Arc::new(prog);
            seq_add_prog(&prog);
            Running(prog)
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            seq_del_prog(&*self.0);
        }
    }

    /// The `#n` of each channel `out` shows.
    fn shown(out: &str) -> Vec<&str> {
        out.lines()
            .filter_map(|l| l.strip_prefix('#'))
            .map(|l| l.split(' ').next().unwrap())
            .collect()
    }

    #[test]
    fn with_no_instance_the_queries_list_none() {
        let _registry = REGISTRY.lock();
        assert_eq!(
            capture(b"", || seq_show(None)),
            "No active state programs\n"
        );
        assert_eq!(
            capture(b"", || seq_chan_show(Some(SeqThreadId(1)), None)),
            "No program instance is running thread 0x1.\nNo active state programs\n"
        );
        assert_eq!(
            capture(b"", || seqcar(2)),
            "Total programs=0, channels=0, connected=0, disconnected=0\n"
        );
    }

    #[test]
    fn show_all_lists_each_state_set_under_one_header() {
        let _registry = REGISTRY.lock();
        let prog = instance(leak(table(
            "qryAll",
            VarModel::Reentrant,
            Vec::new(),
            &["ss1", "ss2"],
        )));
        prog.ss[1].thread_id.set(SeqThreadId(0xabc)).unwrap();
        let _running = Running::new(prog);
        let table = "Program Name        Thread ID           Thread Name         SS Name\n\
                     ------------        ---------           -----------         -------\n\
                     qryAll              (nil)               (no thread)         ss1                \n                    \
                     0xabc                                   ss2                \n";
        assert_eq!(capture(b"", || seq_show(None)), table);
        assert_eq!(
            capture(b"", || seq_queue_show(Some(SeqThreadId(0xab)))),
            format!("No program instance is running thread 0xab.\n{table}")
        );
    }

    #[test]
    fn show_reports_the_instance_and_each_state_set() {
        const NAME: &str = "qryShowThreadWithANameOverThirtyOneBytes";
        let (tx, rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let thread = spawn_dedicated_thread(
            NAME.into(),
            ThreadPriority::Medium,
            StackSizeClass::Small,
            move || {
                tx.send(current_thread_id()).unwrap();
                let _ = done_rx.recv();
            },
        )
        .unwrap();
        let tid = rx.recv().unwrap();

        let prog = instance(leak(SeqProgram {
            options: ProgOptions::new(ProgOptions::CONN | ProgOptions::ASYNC),
            ..table(
                "qryShow",
                VarModel::Reentrant,
                three_chans(),
                &["ss1", "ss2"],
            )
        }));
        *prog.ss[0].time_entered.lock() = 97.5;
        let ss2 = &prog.ss[1];
        ss2.thread_id.set(SeqThreadId(tid)).unwrap();
        ss2.current_state.store(1, Ordering::Relaxed);
        ss2.prev_state.store(0, Ordering::Relaxed);
        *ss2.time_entered.lock() = 100.0;
        *ss2.wakeup_time.lock() = 99.0;
        assign(&prog, &[(0, "ioc:d"), (1, "ioc:c"), (2, "ioc:s")]);
        {
            let mut d = prog.dynamic.lock();
            d.set_connected(ChId(0), true);
            d.set_connected(ChId(1), true);
        }
        let prog = Arc::new(prog);
        let var: *const Vars = prog.var();

        let out = capture(b"", || prog.show_with(&|| 100.0));
        assert_eq!(
            out,
            format!(
                "State Program: \"qryShow\"\n  \
                 thread priority = 50\n  \
                 number of state sets = 2\n  \
                 number of syncQ queues = 0\n  \
                 number of channels = 3\n  \
                 number of channels assigned = 3\n  \
                 number of channels connected = 2\n  \
                 number of channels monitored = 1\n  \
                 options: async=1, debug=0, newef=0, reent=1, conn=1\n  \
                 user variables: address = {var:p}, length = {len}\n\
                 \n  \
                 State Set: \"ss1\"\n  \
                 Thread id = (nil)\n  \
                 First state = \"init\"\n  \
                 Current state = \"init\"\n  \
                 Previous state = \"\"\n  \
                 Elapsed time since state was entered = 2.50 seconds\n  \
                 Wake up delay = inf seconds\n  \
                 Get in progress = [000]\n  \
                 Put in progress = [000]\n\
                 \n  \
                 State Set: \"ss2\"\n  \
                 thread name = {name};  Thread id = {tid:#x}\n  \
                 First state = \"init\"\n  \
                 Current state = \"run\"\n  \
                 Previous state = \"init\"\n  \
                 Elapsed time since state was entered = 0.00 seconds\n  \
                 Wake up delay = -1.00 seconds\n  \
                 Get in progress = [000]\n  \
                 Put in progress = [000]\n\
                 \n",
                len = size_of::<Vars>(),
                name = &NAME[..THREAD_NAME_SIZE - 1],
            )
        );
        drop(done_tx);
        thread.join().unwrap();
    }

    #[test]
    fn show_prints_the_user_variables_per_variable_model() {
        fn shared() -> &'static Vars {
            static VARS: OnceLock<Vars> = OnceLock::new();
            VARS.get_or_init(Vars::zeroed)
        }
        let show = |vars| {
            let prog = Arc::new(instance(leak(table("qryVars", vars, Vec::new(), &["ss"]))));
            capture(b"", || prog.show_with(&|| 0.0))
        };

        let out = show(VarModel::Static(shared));
        assert!(out.contains(" reent=0, "), "{out}");
        assert!(!out.contains("ser variables"), "{out}");

        let out = show(VarModel::Safe);
        assert!(out.contains(" reent=1, "), "{out}");
        assert_eq!(out.matches("  user variables: address = ").count(), 1);
        assert!(
            out.contains("  Put in progress = []\n  User variables: address = "),
            "{out}"
        );
    }

    /// `seq_qry.c:96-106`: the digits cover the assigned channels, and in
    /// safe mode every channel.
    #[test]
    fn show_lists_the_requests_in_progress_per_channel() {
        let in_progress = |vars| {
            let prog = instance(leak(table("qryReq", vars, three_chans(), &["ss"])));
            assigned(&prog);
            let ss = &prog.ss[0];
            *ss.get_req[1].lock() = Some(Arc::new(PvReq { ch: ChId(1), ss: 0 }));
            *ss.put_req[2].lock() = Some(Arc::new(PvReq { ch: ChId(2), ss: 0 }));
            let prog = Arc::new(prog);
            let out = capture(b"", || prog.show_with(&|| 0.0));
            out.lines()
                .filter(|l| l.contains(" in progress = "))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            in_progress(VarModel::Reentrant),
            ["  Get in progress = [10]", "  Put in progress = [01]"]
        );
        assert_eq!(
            in_progress(VarModel::Safe),
            ["  Get in progress = [010]", "  Put in progress = [001]"]
        );
    }

    fn chan_program(prog_name: &'static str, tid: u64) -> Running {
        let prog = instance(leak(table(
            prog_name,
            VarModel::Reentrant,
            three_chans(),
            &["ss1"],
        )));
        assigned(&prog);
        prog.ss[0].thread_id.set(SeqThreadId(tid)).unwrap();
        Running::new(prog)
    }

    #[test]
    fn chan_show_pages_through_every_channel() {
        let _registry = REGISTRY.lock();
        let _running = chan_program("qryChan", 0xc0);
        let out = capture(b"\n\n\n", || seq_chan_show(Some(SeqThreadId(0xc0)), None));
        assert_eq!(
            out,
            format!(
                "State Program: \"qryChan\"\n\
                 Number of channels=3\n\
                 View: State set ss1\n\
                 \n#0 of 3:\n  \
                 Variable name: \"d\"\n    \
                 type = double\n    \
                 count = 2\n  \
                 Value = 1.5 -2\n  \
                 Anonymous\n  \
                 Not connected\n  \
                 Not monitored\n  \
                 Not sync'ed\n\
                 Next? (+/- skip count)\n\
                 \n#1 of 3:\n  \
                 Variable name: \"c\"\n    \
                 type = unsigned char\n    \
                 count = 1\n  \
                 Value = {c}\n  \
                 Assigned to \"ioc:c\"\n  \
                 Connected\n  \
                 Monitored\n  \
                 Sync'ed to event flag 2\n  \
                 Status = 0\n  \
                 Severity = 0\n  \
                 Message = \n  \
                 Time stamp = <undefined>\n\
                 Next? (+/- skip count)\n\
                 \n#2 of 3:\n  \
                 Variable name: \"s\"\n    \
                 type = string\n    \
                 count = 1\n  \
                 Value = \"hello\"\n  \
                 Assigned to \"ioc:s\"\n  \
                 Not connected\n  \
                 Not monitored\n  \
                 Not sync'ed\n  \
                 Status = -2\n  \
                 Severity = 3\n  \
                 Message = disconnected\n  \
                 Time stamp = <undefined>\n\
                 Next? (+/- skip count)\n",
                // C's `%d` of a `char`, whose sign is the platform's.
                c = 200u8 as c_char,
            )
        );
    }

    #[test]
    fn chan_show_moves_by_the_answer_and_stops_outside_the_range() {
        let _registry = REGISTRY.lock();
        let _running = chan_program("qryPager", 0xc1);
        let walk = |input: &[u8]| {
            let out = capture(input, || seq_chan_show(Some(SeqThreadId(0xc1)), None));
            shown(&out).join(",")
        };
        assert_eq!(walk(b""), "0");
        assert_eq!(walk(b"q\n"), "0");
        assert_eq!(walk(b"+2\n"), "0,2");
        assert_eq!(walk(b"\n-\n5\n"), "0,1,0");
        assert_eq!(walk(b"+\n+\n-2\n-\n"), "0,1,2,0");
    }

    #[test]
    fn chan_show_filters_by_connection_and_name() {
        let _registry = REGISTRY.lock();
        let _running = chan_program("qryFilter", 0xc2);
        let filter = |chan: &str| {
            let out = capture(b"\n\n\n", || {
                seq_chan_show(Some(SeqThreadId(0xc2)), Some(chan))
            });
            shown(&out).join(",")
        };
        assert_eq!(filter(""), "0,1,2");
        assert_eq!(filter("+"), "1");
        assert_eq!(filter("-"), "2");
        assert_eq!(filter("ioc"), "1,2");
        assert_eq!(filter("c:s"), "2");
        assert_eq!(filter("-ioc:c"), "");
        assert_eq!(filter("d"), "");
    }

    /// `seq_qry.c:168`: a channel the filter skips moves on by one, whatever
    /// the last answer's step (`:217`) was.
    #[test]
    fn chan_show_skips_a_filtered_channel_by_one_after_any_step() {
        let _registry = REGISTRY.lock();
        let _running = chan_program("qrySkip", 0xc5);
        let out = capture(b"+\n-2\n\n\n", || {
            seq_chan_show(Some(SeqThreadId(0xc5)), Some("ioc"))
        });
        assert_eq!(shown(&out).join(","), "1,2,1,2");
    }

    #[test]
    fn chan_show_in_safe_mode_shows_the_state_sets_copy() {
        let mut prog = instance(leak(table(
            "qrySafe",
            VarModel::Safe,
            three_chans(),
            &["ss1"],
        )));
        assigned(&prog);
        prog.ss[0].var = Some(Vars {
            c: Fixed(EpicsValue::Char(7)),
            ..Vars::zeroed()
        });
        *prog.ss[0].meta[1].lock() = meta(PvStat::High, PvSevr::Minor, Some("ss"));
        let out = capture(b"", || prog.chan_show(0, Some("ioc:c")));
        assert!(
            out.contains(
                "  Value = 7\n  Assigned to \"ioc:c\"\n  Connected\n  Monitored\n  \
                 Sync'ed to event flag 2\n  Status = 4\n  Severity = 1\n  Message = ss\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn seqcar_reports_per_level() {
        let _registry = REGISTRY.lock();
        let _running = chan_program("qryCar", 0xc3);
        let total = "Total programs=1, channels=2, connected=1, disconnected=1\n";
        assert_eq!(capture(b"", || seqcar(0)), total);
        assert_eq!(
            capture(b"", || seqcar(1)),
            format!(
                "  Program \"qryCar\"\n    Variable \"s\" not connected to PV \"ioc:s\"\n{total}"
            )
        );
        assert_eq!(
            capture(b"", || seqcar(2)),
            format!(
                "  Program \"qryCar\"\n    \
                 Variable \"d\" not assigned to PV\n    \
                 Variable \"c\" connected to PV \"ioc:c\"\n    \
                 Variable \"s\" not connected to PV \"ioc:s\"\n{total}"
            )
        );
    }

    #[test]
    fn gather_stats_counts_as_seqcar_and_prints_nothing() {
        let _registry = REGISTRY.lock();
        let _car = chan_program("qryGather", 0xc6);
        let _bare = Running::new(instance(leak(table(
            "qryGatherBare",
            VarModel::Reentrant,
            Vec::new(),
            &["ss"],
        ))));
        let mut stats = SeqStats::default();
        assert_eq!(capture(b"", || stats = seq_gather_stats()), "");
        assert_eq!((stats.n_progs, stats.n_chans, stats.n_conn), (2, 2, 1));
    }

    #[test]
    fn queue_show_with_no_queue_prints_the_count_only() {
        let _registry = REGISTRY.lock();
        let _running = chan_program("qryQueue", 0xc4);
        assert_eq!(
            capture(b"\n", || seq_queue_show(Some(SeqThreadId(0xc4)))),
            "State Program: \"qryQueue\"\nNumber of queues = 0\n"
        );
    }

    #[test]
    fn a_queue_holds_the_get_type_of_its_first_channel() {
        let queued = |var_name, var_type, count, queue_index| SeqChan {
            queue_size: 5,
            queue_index,
            ..chan(var_name, |v| &v.d, var_type, count)
        };
        let prog = instance(leak(table(
            "qryElem",
            VarModel::Reentrant,
            vec![
                chan("x", |v| &v.d, PrimType::Double, 1),
                queued("q0", PrimType::Double, 3, 0),
                queued("q1", PrimType::String, 1, 1),
                queued("q1b", PrimType::Short, 1, 1),
            ],
            &["ss"],
        )));
        assert_eq!(prog.queue_elem_size(0), 24 + 2 * 8);
        assert_eq!(prog.queue_elem_size(1), 52);
    }

    #[test]
    fn fgets_stops_at_a_newline_or_nine_bytes() {
        let mut input = io::Cursor::new(b"123456789012\nab".to_vec());
        assert_eq!(fgets(&mut input, 10).as_deref(), Some(&b"123456789"[..]));
        assert_eq!(fgets(&mut input, 10).as_deref(), Some(&b"012\n"[..]));
        assert_eq!(fgets(&mut input, 10).as_deref(), Some(&b"ab"[..]));
        assert_eq!(fgets(&mut input, 10), None);
    }

    #[test]
    fn user_input_reads_a_step() {
        let answer = |input: &[u8]| {
            let mut dn = None;
            let out = capture(input, || dn = Some(user_input()));
            assert_eq!(out, "Next? (+/- skip count)\n");
            dn.unwrap()
        };
        assert_eq!(answer(b"\n"), 1);
        assert_eq!(answer(b"+\n"), 1);
        assert_eq!(answer(b"-\n"), -1);
        assert_eq!(answer(b"-3\n"), -3);
        assert_eq!(answer(b" 4x\n"), 4);
        assert_eq!(answer(b"x\n"), 0);
        assert_eq!(answer(b"-"), 0);
        assert_eq!(answer(b""), 0);
    }

    #[test]
    fn print_value_prints_each_element_as_its_put_type() {
        let print = |v: EpicsValue| capture(b"", || print_value(&v));
        assert_eq!(print(EpicsValue::Short(-3)), " -3\n");
        assert_eq!(print(EpicsValue::LongArray(vec![1, -2].into())), " 1 -2\n");
        assert_eq!(
            print(EpicsValue::FloatArray(vec![0.1, 1e10].into())),
            " 0.1 1e+10\n"
        );
        assert_eq!(print(EpicsValue::Double(f64::NAN)), " nan\n");
        let long = "x".repeat(MAX_STRING_SIZE + 5);
        assert_eq!(
            print(EpicsValue::StringArray(
                vec!["a".into(), b"b\0c".as_slice().into(), long.as_str().into()].into()
            )),
            format!(" \"a\" \"b\" \"{}\"\n", &long[..MAX_STRING_SIZE])
        );
        assert_eq!(print(EpicsValue::Enum(1)), "\n");
    }
}
