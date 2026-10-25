//! The run-time's own state (`seqPvt.h`), shared by the modules that port
//! `seq_main.c`, `seq_task.c`, `seq_ca.c`, `seq_if.c` and `seq_queue.c`.
//!
//! Generated code and companions never name anything here: they reach a
//! program only through [`StateSet`](crate::StateSet). The fields are the
//! starting shape for the run-time bodies and may change without touching
//! snc-rs. Each field's lock and the order the locks nest in are the
//! crate docs' *Locks*.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use epics_base_rs::runtime::general_time::epics_epoch;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::WallTime;
use parking_lot::Mutex;

use crate::program::{SeqChan, SeqProgram, SeqSs};
use crate::pv::{EventGate, PvSystem, PvVar};
use crate::seq::SeqThreadId;
use crate::types::{ChId, CharPtr, EfId, PvSevr, PvStat, SeqMask};
use crate::var::ProgramVars;

pub use crate::cmd::{create_or_attach_pv_system, traverse_sequencer_programs};
pub use crate::mac::SeqMacros;
pub use crate::prog::{
    seq_add_prog, seq_del_prog, seq_find_prog, seq_find_state_set, seq_traverse_prog,
};
pub use crate::qry::SeqStats;
pub use crate::queue::{QueueElem, SeqQueue};
pub use crate::task::BufSrc;

/// `struct pv_meta_data`: what the last get or monitor reported.
#[derive(Debug, Clone)]
pub struct PvMeta {
    pub timestamp: WallTime,
    pub status: PvStat,
    pub severity: PvSevr,
    pub message: CharPtr,
}

impl PvMeta {
    /// C's calloc'd `pv_meta_data`, a channel's before its first get or
    /// monitor: the EPICS epoch, no alarm, no message.
    pub fn zeroed() -> Self {
        PvMeta {
            timestamp: epics_epoch(),
            status: PvStat::Ok,
            severity: PvSevr::None,
            message: CharPtr::default(),
        }
    }
}

/// `struct db_channel`: a channel assigned to a named pv. It lives in a
/// [`ProgDyn`] slot, under `dynamic`, as C's is under `sp->lock`; its meta
/// data is under [`Chan::var_lock`] and its count is [`Chan::db_count`].
/// Its flags change only through [`ProgDyn`], which keeps the counts.
pub struct DbChan {
    /// The name after macro substitution.
    pub db_name: String,
    /// `None` between `seq` naming the channel and `seq_connect`. Cloned
    /// out under `dynamic` for a request made without it.
    pub pvid: Option<Arc<PvVar>>,
    connected: bool,
    got_monitor: bool,
    /// `init_chan` counted this channel in `monitor_count`; only
    /// [`ProgDyn::drop_uncreated_dbch`] releases it.
    monitor_counted: bool,
}

impl DbChan {
    pub fn connected(&self) -> bool {
        self.connected
    }

    pub fn got_monitor(&self) -> bool {
        self.got_monitor
    }
}

/// `struct channel`: an assigned variable of one program instance.
pub struct Chan<V: 'static> {
    pub def: &'static SeqChan<V>,
    pub monitored: AtomicBool,
    /// What C reads as `ch->dbch ? ch->dbch->dbCount : ch->count`: the
    /// elements a buffer copy moves. Written under `dynamic`, read without
    /// it, as C reads it.
    pub db_count: AtomicU32,
    /// `varLock`, and the shared buffer's meta data it guards with the
    /// value, so a value and its meta are written and read as one.
    pub var_lock: Mutex<PvMeta>,
}

/// `struct pvreq`: a pending get or put. A completion whose request is no
/// longer the channel's pending one (`Arc::ptr_eq`) was cancelled.
pub struct PvReq {
    pub ch: ChId,
    pub ss: usize,
}

/// The `lock`-protected half of `struct program_instance`: `dynamic`.
///
/// The channel slots and their counts are private and change together,
/// only in the methods here. `assign_count`, `connect_count` and
/// `got_monitor_count` are the number of channels in their state;
/// `monitor_count` is C's: the monitored channels `init_chan` named, less
/// those whose create failed. `pvAssign` never changes it.
pub struct ProgDyn {
    /// Per channel: `None` while anonymous.
    dbch: Box<[Option<DbChan>]>,
    /// Event bits for flags and channels.
    pub ev_flags: Box<[SeqMask]>,
    /// The channels each event flag is synced to (index 0 unused).
    pub synced_chans: Box<[Vec<ChId>]>,
    /// The flag each channel is synced to, [`crate::NOEVFLAG`] if none.
    pub synced_to: Box<[EfId]>,
    assign_count: u32,
    connect_count: u32,
    monitor_count: u32,
    got_monitor_count: u32,
}

impl ProgDyn {
    /// `num_chans` anonymous channels and every count zero.
    pub fn new(
        num_chans: usize,
        ev_flags: Box<[SeqMask]>,
        synced_chans: Box<[Vec<ChId>]>,
        synced_to: Box<[EfId]>,
    ) -> Self {
        ProgDyn {
            dbch: (0..num_chans).map(|_| None).collect(),
            ev_flags,
            synced_chans,
            synced_to,
            assign_count: 0,
            connect_count: 0,
            monitor_count: 0,
            got_monitor_count: 0,
        }
    }

    /// Channel `ch`, `None` while anonymous.
    pub fn dbch(&self, ch: ChId) -> Option<&DbChan> {
        self.dbch[ch.index()].as_ref()
    }

    /// `seq_connect`: store named channel `ch`'s created pv. No `&mut
    /// DbChan` leaves here, so nothing can replace a channel without its
    /// counts.
    pub fn set_pvid(&mut self, ch: ChId, pvid: Arc<PvVar>) {
        let dbch = self.dbch[ch.index()].as_mut();
        dbch.expect("set_pvid: the channel is anonymous").pvid = Some(pvid);
    }

    /// `init_chan`: name anonymous channel `ch` `db_name`, counting it in
    /// `monitor_count` when `monitored`, the one place that count grows.
    pub fn init_dbch(&mut self, ch: ChId, db_name: String, monitored: bool) {
        self.insert(ch, db_name, None, monitored);
    }

    /// `pvAssign`: assign anonymous channel `ch` to `db_name` and its new
    /// pv, leaving `monitor_count` as `init_chan` set it, as C does.
    pub fn assign_dbch(&mut self, ch: ChId, db_name: String, pvid: Arc<PvVar>) {
        self.insert(ch, db_name, Some(pvid), false);
    }

    /// The caller empties the slot under the same hold; an occupied one
    /// panics.
    fn insert(
        &mut self,
        ch: ChId,
        db_name: String,
        pvid: Option<Arc<PvVar>>,
        monitor_counted: bool,
    ) {
        let slot = &mut self.dbch[ch.index()];
        assert!(slot.is_none(), "channel {} is assigned", ch.0);
        *slot = Some(DbChan {
            db_name,
            pvid,
            connected: false,
            got_monitor: false,
            monitor_counted,
        });
        self.assign_count += 1;
        if monitor_counted {
            self.monitor_count += 1;
        }
    }

    /// `pvAssign`'s replace and unassign: take channel `ch` out with its
    /// share of `assign_count`, `connect_count` and `got_monitor_count`;
    /// `monitor_count` stays, as `monitored` stays the variable's. The
    /// caller destroys the pv with no lock held, as C does. C's `pvAssign`
    /// leaves `gotMonitorCount` counting a removed channel.
    pub fn unassign_dbch(&mut self, ch: ChId) -> Option<DbChan> {
        self.take(ch)
    }

    /// `seq_connect`'s create failure: take channel `ch` out with every
    /// count it holds, its `init_chan` share of `monitor_count` included.
    /// C frees the channel, leaves `ch->dbch` dangling and keeps its counts
    /// (`seq_ca.c:79-82`), so a `+c` wait never ends: a C defect not copied.
    pub fn drop_uncreated_dbch(&mut self, ch: ChId) {
        if let Some(dbch) = self.take(ch)
            && dbch.monitor_counted
        {
            self.monitor_count -= 1;
        }
    }

    fn take(&mut self, ch: ChId) -> Option<DbChan> {
        let dbch = self.dbch[ch.index()].take()?;
        self.assign_count -= 1;
        if dbch.connected {
            self.connect_count -= 1;
        }
        if dbch.got_monitor {
            self.got_monitor_count -= 1;
        }
        Some(dbch)
    }

    /// Set channel `ch`'s `connected` and `connect_count` with it: `None`
    /// when the channel is anonymous, else whether the flag changed.
    pub fn set_connected(&mut self, ch: ChId, connected: bool) -> Option<bool> {
        let dbch = self.dbch[ch.index()].as_mut()?;
        let changed = std::mem::replace(&mut dbch.connected, connected) != connected;
        if changed {
            if connected {
                self.connect_count += 1;
            } else {
                self.connect_count -= 1;
            }
        }
        Some(changed)
    }

    /// Set channel `ch`'s `got_monitor` and `got_monitor_count` with it, as
    /// [`set_connected`](Self::set_connected).
    pub fn set_got_monitor(&mut self, ch: ChId, got: bool) -> Option<bool> {
        let dbch = self.dbch[ch.index()].as_mut()?;
        let changed = std::mem::replace(&mut dbch.got_monitor, got) != got;
        if changed {
            if got {
                self.got_monitor_count += 1;
            } else {
                self.got_monitor_count -= 1;
            }
        }
        Some(changed)
    }

    pub fn assign_count(&self) -> u32 {
        self.assign_count
    }

    pub fn connect_count(&self) -> u32 {
        self.connect_count
    }

    pub fn monitor_count(&self) -> u32 {
        self.monitor_count
    }

    pub fn got_monitor_count(&self) -> u32 {
        self.got_monitor_count
    }
}

/// `epicsEventId`, which every sequencer event is (`seq_main.c`).
pub type SeqEvent = epics_base_rs::runtime::sync::EpicsEvent;

/// `struct state_set`: a state set's control block, shared with the pv
/// callbacks and the other state sets.
pub struct Sscb<V: 'static> {
    pub def: &'static SeqSs<V>,
    /// Safe mode's private copy of the variables, `Some` exactly when the
    /// program's [`VarModel`](crate::VarModel) is `Safe`. Made after
    /// `seqg_init`, before the first connect, as `seq_task.c` does.
    pub var: Option<V>,
    pub thread_id: OnceLock<SeqThreadId>,
    /// State indices, -1 for none.
    pub current_state: AtomicI32,
    pub next_state: AtomicI32,
    pub prev_state: AtomicI32,
    pub mask: Mutex<&'static [SeqMask]>,
    pub time_entered: Mutex<f64>,
    pub wakeup_time: Mutex<f64>,
    /// `syncSem`: a pv callback or another state set has something for
    /// this one.
    pub sync_sem: SeqEvent,
    pub dead: SeqEvent,
    /// Per channel: the pending get / put.
    pub get_req: Box<[Mutex<Option<Arc<PvReq>>>]>,
    pub put_req: Box<[Mutex<Option<Arc<PvReq>>>]>,
    /// Per channel, safe mode: meta data, and whether the buffer changed
    /// since this state set last copied it.
    pub meta: Box<[Mutex<PvMeta>]>,
    pub dirty: Box<[AtomicBool]>,
}

/// Where a program instance's variables are.
pub enum VarBlock<V: 'static> {
    /// `-r`: the program's one static block.
    Static(&'static V),
    /// `+r` / `+s`: this instance's block (the shared buffer under `+s`).
    Owned(Box<V>),
}

/// `struct program_instance`: one started program.
pub struct Program<V: 'static> {
    pub table: &'static SeqProgram<V>,
    pub vars: VarBlock<V>,
    /// Assigned by `seq_add_prog` on the program's first thread; read by
    /// the pv callbacks and the shell.
    pub instance: AtomicU32,
    pub thread_priority: u32,
    pub stack_size: u32,
    pub pv_sys: PvSystem,
    /// The database `seq` started the program on.
    pub db: PvDatabase,
    /// What the program's monitor events pass: each state set is in it
    /// while it runs, so an event lands only while every one is blocked.
    pub gate: Arc<EventGate>,
    pub chans: Box<[Chan<V>]>,
    pub queues: Box<[SeqQueue]>,
    pub ss: Box<[Sscb<V>]>,
    pub macros: SeqMacros,
    pub dynamic: Mutex<ProgDyn>,
    /// Set by `seqStop` / `seq_exit`.
    pub die: AtomicBool,
    /// All channels connected and the first monitors in (`+c`).
    pub ready: SeqEvent,
    pub dead: SeqEvent,
}

impl<V> Program<V> {
    /// The instance's variable block: the shared buffer under `+s`.
    pub fn var(&self) -> &V {
        match &self.vars {
            VarBlock::Static(v) => v,
            VarBlock::Owned(v) => v,
        }
    }
}

/// A program instance, whatever its variable type: what the registry holds
/// and `seq_qry.c` walks. Each query is the `seq_qry.c` routine of the
/// same name on [`Program`].
pub trait ProgInstance: Send + Sync {
    fn prog_name(&self) -> &'static str;

    fn instance(&self) -> u32;

    fn num_ss(&self) -> usize;

    /// State set `ss`'s thread, `None` until it starts.
    fn thread_id(&self, ss: usize) -> Option<SeqThreadId>;

    /// `seqStop`'s `seq_exit(sp->ss)`.
    fn stop(&self);

    /// [`Program::gate`].
    fn gate(&self) -> &EventGate;

    /// An `Arc`, because `seqShow` asks each state set's
    /// `pvGetComplete` / `pvPutComplete`.
    fn show(self: Arc<Self>);

    fn show_sp(&self, prog_count: &mut u32);

    fn chan_show(&self, ss: usize, chan: Option<&str>);

    fn queue_show(&self);

    fn car_collect(&self, stats: &mut SeqStats);
}

impl<V: ProgramVars> ProgInstance for Program<V> {
    fn prog_name(&self) -> &'static str {
        self.table.prog_name
    }

    fn instance(&self) -> u32 {
        self.instance.load(Ordering::Relaxed)
    }

    fn num_ss(&self) -> usize {
        self.ss.len()
    }

    fn gate(&self) -> &EventGate {
        &self.gate
    }

    fn thread_id(&self, ss: usize) -> Option<SeqThreadId> {
        self.ss[ss].thread_id.get().copied()
    }

    fn stop(&self) {
        self.exit()
    }

    fn show(self: Arc<Self>) {
        Program::show(&self)
    }

    fn show_sp(&self, prog_count: &mut u32) {
        Program::show_sp(self, prog_count)
    }

    fn chan_show(&self, ss: usize, chan: Option<&str>) {
        Program::chan_show(self, ss, chan)
    }

    fn queue_show(&self) {
        Program::queue_show(self)
    }

    fn car_collect(&self, stats: &mut SeqStats) {
        Program::car_collect(self, stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Channel 0 anonymous; 1 assigned; 2 connected; 3 monitored from the
    /// start, connected, with its first monitor in.
    fn dynamic() -> ProgDyn {
        let mut d = ProgDyn::new(4, Box::new([]), Box::new([]), Box::new([]));
        for (ch, monitored) in [(1, false), (2, false), (3, true)] {
            d.init_dbch(ChId(ch), String::new(), monitored);
        }
        assert_eq!(d.set_connected(ChId(2), true), Some(true));
        assert_eq!(d.set_connected(ChId(3), true), Some(true));
        assert_eq!(d.set_got_monitor(ChId(3), true), Some(true));
        d
    }

    fn counts(d: &ProgDyn) -> [u32; 4] {
        [
            d.assign_count(),
            d.connect_count(),
            d.monitor_count(),
            d.got_monitor_count(),
        ]
    }

    #[test]
    fn init_counts_the_channel_and_a_monitor_only_when_monitored() {
        assert_eq!(counts(&dynamic()), [3, 2, 1, 1]);
    }

    #[test]
    fn removing_an_anonymous_channel_releases_nothing() {
        let mut d = dynamic();
        assert!(d.unassign_dbch(ChId(0)).is_none());
        d.drop_uncreated_dbch(ChId(0));
        assert_eq!(counts(&d), [3, 2, 1, 1]);
    }

    /// `assign x to "a"; monitor x;` then `pvAssign(x, "")`: C's seqShow
    /// still says 1 monitored.
    #[test]
    fn unassigning_releases_its_state_counts_and_keeps_monitor_count() {
        for (ch, after) in [(1, [2, 2, 1, 1]), (2, [2, 1, 1, 1]), (3, [2, 1, 1, 0])] {
            let mut d = dynamic();
            assert!(d.unassign_dbch(ChId(ch)).is_some());
            assert!(d.dbch(ChId(ch)).is_none());
            assert_eq!(counts(&d), after, "channel {ch}");
        }
    }

    #[test]
    fn unassigning_every_channel_leaves_only_monitor_count() {
        let mut d = dynamic();
        for ch in 0..4 {
            d.unassign_dbch(ChId(ch));
        }
        assert_eq!(counts(&d), [0, 0, 1, 0]);
    }

    /// The same, then `pvAssign(x, "b")`: still 1 monitored, and the new
    /// channel starts unconnected.
    #[test]
    fn a_channel_assigned_again_starts_unconnected_and_keeps_monitor_count() {
        let mut d = dynamic();
        assert!(d.unassign_dbch(ChId(3)).is_some());
        d.insert(ChId(3), "new".into(), None, false);
        assert_eq!(counts(&d), [3, 1, 1, 0]);
        let c = d.dbch(ChId(3)).expect("assigned");
        assert_eq!(
            (c.db_name.as_str(), c.connected(), c.got_monitor()),
            ("new", false, false)
        );
    }

    #[test]
    fn a_create_failure_releases_the_channel_and_its_init_monitor_count() {
        let mut d = ProgDyn::new(2, Box::new([]), Box::new([]), Box::new([]));
        d.init_dbch(ChId(0), "a".into(), true);
        d.init_dbch(ChId(1), "b".into(), false);
        d.drop_uncreated_dbch(ChId(0));
        assert!(d.dbch(ChId(0)).is_none());
        assert_eq!(counts(&d), [1, 0, 0, 0]);
        d.drop_uncreated_dbch(ChId(1));
        assert_eq!(counts(&d), [0, 0, 0, 0]);
    }

    /// A channel `pvAssign` added holds no share of `monitor_count`.
    #[test]
    fn a_create_failure_of_a_reassigned_channel_keeps_monitor_count() {
        let mut d = dynamic();
        assert!(d.unassign_dbch(ChId(3)).is_some());
        d.insert(ChId(3), "new".into(), None, false);
        d.drop_uncreated_dbch(ChId(3));
        assert_eq!(counts(&d), [2, 1, 1, 0]);
    }

    #[test]
    #[should_panic(expected = "channel 3 is assigned")]
    fn naming_an_assigned_channel_panics() {
        dynamic().init_dbch(ChId(3), "new".into(), false);
    }

    #[test]
    fn a_flag_already_as_asked_counts_nothing() {
        let mut d = dynamic();
        assert_eq!(d.set_connected(ChId(2), true), Some(false));
        assert_eq!(d.set_got_monitor(ChId(1), false), Some(false));
        assert_eq!(counts(&d), [3, 2, 1, 1]);
    }

    #[test]
    fn a_flag_change_counts_once_each_way() {
        let mut d = dynamic();
        assert_eq!(d.set_connected(ChId(1), true), Some(true));
        assert_eq!(d.set_got_monitor(ChId(3), false), Some(true));
        assert_eq!(counts(&d), [3, 3, 1, 0]);
        assert_eq!(d.set_connected(ChId(1), false), Some(true));
        assert_eq!(counts(&d), [3, 2, 1, 0]);
    }

    #[test]
    fn an_anonymous_channel_has_no_flag_to_set() {
        let mut d = dynamic();
        assert_eq!(d.set_connected(ChId(0), true), None);
        assert_eq!(d.set_got_monitor(ChId(0), true), None);
        assert_eq!(counts(&d), [3, 2, 1, 1]);
    }
}
