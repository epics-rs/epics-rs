//! The start-up gate of a ported SNL program — the wait C's `seq` does for
//! every program before its first state runs.
//!
//! A C sequencer program runs no state until every PV it declared has
//! connected (`seq_main.c`: the program thread blocks in its start-up wait
//! until `pvConnectCount() == pvAssignCount()`), and a channel to the IOC's
//! own records connects only once `iocRun` has started rsrv — after
//! `initialProcess`, so the program's first read of a `PINI=YES` record sees
//! the processed value and its first write is not undone by the PINI pass.
//! The port's programs reach records directly, so nothing connects and
//! nothing waited: a program started from `st.cmd` ran ahead of the PINI
//! pass, read the unprocessed defaults, and had what it wrote overwritten
//! by PINI. [`spawn_program`] is that wait, at the one point every `seqStart`
//! passes through, so a program cannot be started ahead of PINI at all.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use crate::error::CaResult;
use crate::runtime::task::BlockingBridge;
use crate::server::database::{PvDatabase, parse_pv_name};
use crate::server::record::{ProcessCompletion, PutRefused, ambient_write_origin_scope};
use crate::types::EpicsValue;
use epics_libcom_rs::cfmt::CFloat;

/// Start `run` as a task once the database has finished its PINI pass.
///
/// `run` is the program's `run(config, db)` with the config already bound;
/// `program` and `macros` are what [`seq`] takes.
pub fn spawn_program<F, Fut, E>(
    bridge: &BlockingBridge,
    db: &PvDatabase,
    program: &'static str,
    macros: &SeqMacros,
    run: F,
) where
    F: FnOnce(PvDatabase) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), E>> + Send + 'static,
    E: std::fmt::Display,
{
    let db = db.clone();
    seq(bridge, program, macros, async move {
        db.wait_for_pini().await;
        run(db).await
    });
}

/// Start `program` as C `seq` does (`seq_main.c:55-145`): the sequencer
/// release and the spawn go to errlog, the task named by the `name` macro,
/// else by the program (`:110-115`). A failed program leaves one error line
/// naming it.
pub fn seq<Fut, E>(bridge: &BlockingBridge, program: &'static str, macros: &SeqMacros, task: Fut)
where
    Fut: Future<Output = Result<(), E>> + Send + 'static,
    E: std::fmt::Display,
{
    use crate::runtime::log::{ErrlogSevEnum, errlog_sev_vprintf};
    errlog_sev_vprintf(
        ErrlogSevEnum::Info,
        format_args!(
            "Sequencer release {}, compiled {}\n",
            env!("CARGO_PKG_VERSION"),
            env!("EPICS_VCS_VERSION_DATE")
        ),
    );
    let thread: Arc<str> = match macros.value("name") {
        Some(name) if !name.is_empty() => name.into(),
        _ => program.into(),
    };
    // The task's own allocation stands in for C's `epicsThreadId`.
    let held = Arc::clone(&thread);
    bridge.spawn(async move {
        let _thread = held;
        if let Err(e) = task.await {
            crate::stderr_println!("{program} error: {e}");
        }
    });
    errlog_sev_vprintf(
        ErrlogSevEnum::Info,
        format_args!(
            "Spawning sequencer program \"{program}\", thread {:p}: \"{thread}\"\n",
            Arc::as_ptr(&thread).cast::<u8>()
        ),
    );
}

/// A program's macro table (`PROG::macros`, `seq_mac.c`), built as `seq`
/// builds it: the `program` statement's parameters, then the command line's
/// over them (`seq_main.c:93-96`).
#[derive(Debug, Default, Clone)]
pub struct SeqMacros(Vec<(String, Option<String>)>);

impl SeqMacros {
    pub fn new(params: &str, macro_def: &str) -> Self {
        let mut macros = Self::default();
        macros.parse(params);
        macros.parse(macro_def);
        macros
    }

    /// `seqMacParse`: a value ends at a blank or a comma, and parsing stops
    /// at the first thing that is neither a name nor a separator. A name
    /// given without a value keeps the value it had.
    pub fn parse(&mut self, mac_str: &str) {
        fn blanks(s: &str) -> &str {
            s.trim_start_matches(' ')
        }
        let mut s = mac_str;
        while !s.is_empty() {
            s = blanks(s);
            // `seqMacParseName`
            if !s.starts_with(|c: char| c.is_ascii_alphabetic()) {
                break;
            }
            let len = s
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(s.len());
            let (name, rest) = s.split_at(len);
            let at = match self.0.iter().position(|(n, _)| n == name) {
                Some(at) => at,
                None => {
                    self.0.push((name.to_string(), None));
                    self.0.len() - 1
                }
            };
            s = blanks(rest);
            if let Some(rest) = s.strip_prefix(',') {
                s = rest;
                continue;
            }
            let Some(rest) = s.strip_prefix('=') else {
                break;
            };
            s = blanks(rest);
            // `seqMacParseValue`
            let len = s.find([' ', ',']).unwrap_or(s.len());
            let (value, rest) = s.split_at(len);
            self.0[at].1 = Some(value.to_string());
            let Some(rest) = blanks(rest).strip_prefix(',') else {
                break;
            };
            s = rest;
        }
    }

    /// `seqMacValGet`: `None` for a name never defined or never given a
    /// value.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.as_deref())
    }
}

/// `DEFAULT_TIMEOUT` (`seqCom.h:44`): how long a `SYNC` put or get waits.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a `pvPut`/`pvGet` failed — C's `pvStat` past `pvStatOK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PvStat {
    /// `pvStatERROR`: the user error `check_pending` refuses.
    Error,
    /// `pvStatTIMEOUT`.
    Timeout,
    /// `pvStatDISCONN`: no such record.
    Disconn,
}

impl std::fmt::Display for PvStat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::Disconn => "disconnected",
        })
    }
}

impl std::error::Error for PvStat {}

enum Request {
    Put {
        name: Arc<str>,
        value: EpicsValue,
        origin: u64,
        /// The pending-put slot and the id the put holds it with: `SYNC` and
        /// `ASYNC` puts take a put callback, `DEFAULT` ones do not.
        callback: Option<(Arc<watch::Sender<Option<u64>>>, u64)>,
    },
    Get {
        name: Arc<str>,
        reply: oneshot::Sender<CaResult<EpicsValue>>,
    },
    /// A `string` variable's `pvGet`, which asks for `DBR_STRING`.
    GetText {
        name: Arc<str>,
        reply: oneshot::Sender<CaResult<String>>,
    },
    Flush {
        reply: oneshot::Sender<()>,
    },
}

/// A program's CA circuit: its puts and gets reach the database in the order
/// it made them, on a task of their own, as `pvVarPutNoBlock` and
/// `pvVarGetCallback` hand them to the client library and rsrv serves them
/// (`seq_if.c:490-528`). A `DEFAULT` put therefore never waits for the record,
/// and a get made after a put reads what the put wrote.
pub struct PutQueue {
    set: StateSet,
    done_rx: mpsc::UnboundedReceiver<Arc<str>>,
}

impl PutQueue {
    /// Open the circuit for the state set `ss` (named in `check_pending`'s
    /// messages).
    pub fn new(db: &PvDatabase, ss: &str) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        crate::runtime::task::spawn_background(
            crate::runtime::task::CallbackPriority::Medium,
            serve(db.clone(), rx, done_tx),
        );
        Self {
            set: StateSet {
                db: db.clone(),
                ss: ss.into(),
                tx,
            },
            done_rx,
        }
    }

    /// Another state set of the program, on the same circuit: seq gives a
    /// program one CA context for all its state sets.
    pub fn state_set(&self, ss: &str) -> StateSet {
        StateSet {
            ss: ss.into(),
            ..self.set.clone()
        }
    }

    /// See [`StateSet::pv`].
    pub fn pv(&self, var: &str, name: &str) -> SeqPv {
        self.set.pv(var, name)
    }

    /// See [`StateSet::pv_with_origin`].
    pub fn pv_with_origin(&self, var: &str, name: &str, origin: u64) -> SeqPv {
        self.set.pv_with_origin(var, name, origin)
    }

    /// See [`StateSet::flush`].
    pub async fn flush(&self) {
        self.set.flush().await
    }

    /// The next channel whose `SYNC`/`ASYNC` put completed — where C's put
    /// callback sets the event flag the variable is synced to
    /// (`seq_ca.c:331-333`). A completion that arrives after its put timed out
    /// is not one (`seq_ca.c:172-174`).
    pub async fn completion(&mut self) -> Arc<str> {
        self.done_rx
            .recv()
            .await
            .expect("the circuit runs while its queue holds a request sender")
    }

    /// [`Self::completion`] if one is already queued.
    pub fn try_completion(&mut self) -> Option<Arc<str>> {
        self.done_rx.try_recv().ok()
    }
}

/// One state set's end of a [`PutQueue`].
#[derive(Clone)]
pub struct StateSet {
    db: PvDatabase,
    ss: Arc<str>,
    tx: mpsc::UnboundedSender<Request>,
}

impl StateSet {
    /// The program variable `var` assigned to `name` — snc's `varName`,
    /// with `[i]` for an element of an array assigned to a list
    /// (`gen_tables.c`), which `check_pending`'s messages print.
    pub fn pv(&self, var: &str, name: &str) -> SeqPv {
        self.pv_with_origin(var, name, 0)
    }

    /// [`Self::pv`], its puts tagged with `origin` as
    /// [`crate::server::database::db_access::DbChannel::with_origin`] tags them.
    pub fn pv_with_origin(&self, var: &str, name: &str, origin: u64) -> SeqPv {
        SeqPv {
            db: self.db.clone(),
            ss: self.ss.clone(),
            var: var.into(),
            name: name.into(),
            origin,
            tx: self.tx.clone(),
            slot: Arc::new(watch::channel(None).0),
        }
    }

    /// `pvSysFlush` at a state's entry (`seq_task.c:303-304`): return once
    /// the database has taken every request made before it, as the loopback
    /// IOC has by the time the state's `when` clauses run.
    pub async fn flush(&self) {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Request::Flush { reply }).is_ok() {
            let _ = rx.await;
        }
    }
}

/// The server side of the circuit: one request at a time, in order. A put
/// callback's completion is waited for off the circuit, as rsrv answers a
/// `WRITE_NOTIFY` once the record finishes without holding up the requests
/// behind it.
async fn serve(
    db: PvDatabase,
    mut rx: mpsc::UnboundedReceiver<Request>,
    done: mpsc::UnboundedSender<Arc<str>>,
) {
    while let Some(request) = rx.recv().await {
        match request {
            Request::Put {
                name,
                value,
                origin,
                callback: None,
            } => {
                let (record, field) = parse_pv_name(&*name);
                let _ = db
                    .put_record_field_from_ca_no_notify_with_origin(record, field, value, origin)
                    .await;
            }
            Request::Put {
                name,
                value,
                origin,
                callback: Some((slot, id)),
            } => {
                let (record, field) = parse_pv_name(&*name);
                let put = {
                    let held = db.lock_for_put(record, field, &value);
                    let _origin = ambient_write_origin_scope(origin);
                    db.put_record_field_notify_already_locked(held, value)
                };
                let done = done.clone();
                let finish = move || {
                    let ours = slot.send_if_modified(|pending| {
                        let ours = *pending == Some(id);
                        if ours {
                            *pending = None;
                        }
                        ours
                    });
                    if ours {
                        let _ = done.send(name);
                    }
                };
                // A refused put still answers when the process it drove ends.
                match PutRefused::outcome(put).0 {
                    ProcessCompletion::Async(rx) => {
                        crate::runtime::task::spawn_background(
                            crate::runtime::task::CallbackPriority::Medium,
                            async move {
                                let _ = rx.await;
                                finish();
                            },
                        );
                    }
                    _ => finish(),
                }
            }
            Request::Get { name, reply } => {
                let _ = reply.send(db.get_pv(&*name));
            }
            Request::GetText { name, reply } => {
                let _ = reply.send(db.get_pv(&*name).map(|v| db.pv_dbr_string(&name, &v)));
            }
            Request::Flush { reply } => {
                let _ = reply.send(());
            }
        }
    }
}

/// One of a program's channels — C's `CHAN` with its `putReq` slot
/// (`seq_if.c:441-528`).
#[derive(Clone)]
pub struct SeqPv {
    db: PvDatabase,
    ss: Arc<str>,
    var: Arc<str>,
    name: Arc<str>,
    origin: u64,
    tx: mpsc::UnboundedSender<Request>,
    /// The id of the put callback outstanding, if one is.
    slot: Arc<watch::Sender<Option<u64>>>,
}

static NEXT_PUT: AtomicU64 = AtomicU64::new(1);

impl SeqPv {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `check_connected`: the record is there.
    fn connected(&self) -> Result<(), PvStat> {
        let (record, _) = parse_pv_name(&*self.name);
        match self.db.get_record(record) {
            Some(_) => Ok(()),
            None => Err(PvStat::Disconn),
        }
    }

    fn send_put(&self, value: EpicsValue, id: Option<u64>) {
        let _ = self.tx.send(Request::Put {
            name: self.name.clone(),
            value,
            origin: self.origin,
            callback: id.map(|id| (self.slot.clone(), id)),
        });
    }

    /// `pvPut(var)`: fire and forget (`seq_if.c:490-503`).
    pub fn put(&self, value: EpicsValue) -> Result<(), PvStat> {
        self.connected()?;
        self.send_put(value, None);
        Ok(())
    }

    /// `pvPut(var, SYNC)`: wait out a put still pending on the channel, then
    /// put with a callback and wait for it (`seq_if.c:85-127`, `:519-526`).
    pub async fn put_sync(&self, value: EpicsValue) -> Result<(), PvStat> {
        self.put_sync_tmo(value, DEFAULT_TIMEOUT).await
    }

    /// [`Self::put_sync`] with `pvPutTmo`'s timeout.
    pub async fn put_sync_tmo(&self, value: EpicsValue, tmo: Duration) -> Result<(), PvStat> {
        if tmo.is_zero() {
            self.report(format_args!(
                "pvPut({},SYNC,{:.6}): user error (timeout must be positive)",
                self.var,
                CFloat(tmo.as_secs_f64())
            ));
            return Err(PvStat::Error);
        }
        let started = crate::runtime::task::Instant::now();
        let mut pending = self.slot.subscribe();
        if crate::runtime::task::timeout(tmo, pending.wait_for(Option::is_none))
            .await
            .is_err()
        {
            self.report(format_args!(
                "pvPut(ss {}, var {}, pv {}): failed (timeout waiting for other pvPut requests to finish)",
                self.ss, self.var, self.name
            ));
            return Err(PvStat::Timeout);
        }
        self.connected()?;
        let id = NEXT_PUT.fetch_add(1, Ordering::Relaxed);
        self.slot.send_replace(Some(id));
        self.send_put(value, Some(id));
        let left = tmo.saturating_sub(started.elapsed());
        let done = crate::runtime::task::timeout(left, pending.wait_for(|p| *p != Some(id))).await;
        if done.is_err() {
            // `wait_complete` cancels the request; its callback is then ignored.
            self.slot.send_if_modified(|p| {
                let ours = *p == Some(id);
                if ours {
                    *p = None;
                }
                ours
            });
            return Err(PvStat::Timeout);
        }
        self.connected()
    }

    /// `pvPut(var, ASYNC)`: put with a callback and return; refused while one
    /// is pending on the channel (`seq_if.c:128-139`).
    pub fn put_async(&self, value: EpicsValue) -> Result<(), PvStat> {
        if self.slot.borrow().is_some() {
            self.report(format_args!(
                "pvPut(ss {}, var {}, pv {}): user error (there is already a pvPut pending for this channel/state set combination)",
                self.ss, self.var, self.name
            ));
            return Err(PvStat::Error);
        }
        self.connected()?;
        let id = NEXT_PUT.fetch_add(1, Ordering::Relaxed);
        self.slot.send_replace(Some(id));
        self.send_put(value, Some(id));
        Ok(())
    }

    /// `pvPutComplete(var)`: no put callback is outstanding
    /// (`seq_if.c:576-582`).
    pub fn put_complete(&self) -> bool {
        self.slot.borrow().is_none()
    }

    fn report(&self, message: std::fmt::Arguments<'_>) {
        crate::runtime::log::errlog_sev_vprintf(
            crate::runtime::log::ErrlogSevEnum::Major,
            format_args!("{message}\n"),
        );
    }

    /// `pvGet(var)` — `SYNC` without `+a` (`seq_if.c:211-214`): the value
    /// once every request made before it has been served, or `None` when the
    /// record is not there or the get times out.
    pub async fn get(&self) -> Option<EpicsValue> {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(Request::Get {
            name: self.name.clone(),
            reply,
        });
        match crate::runtime::task::timeout(DEFAULT_TIMEOUT, rx).await {
            Ok(Ok(Ok(v))) => Some(v),
            _ => None,
        }
    }

    pub async fn get_f64(&self) -> f64 {
        self.get().await.and_then(|v| v.to_f64()).unwrap_or(0.0)
    }

    pub async fn get_i16(&self) -> i16 {
        self.get()
            .await
            .and_then(|v| v.to_f64())
            .map(|f| f as i16)
            .unwrap_or(0)
    }

    /// See `DbChannel::get_i32` for why a non-`Long` goes through
    /// `to_dbf_i32`.
    pub async fn get_i32(&self) -> i32 {
        self.get()
            .await
            .and_then(|v| match v {
                EpicsValue::Long(i) => Some(i),
                other => other.to_dbf_i32(),
            })
            .unwrap_or(0)
    }

    pub async fn get_string(&self) -> String {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(Request::GetText {
            name: self.name.clone(),
            reply,
        });
        match crate::runtime::task::timeout(DEFAULT_TIMEOUT, rx).await {
            Ok(Ok(Ok(text))) => text,
            _ => String::new(),
        }
    }

    /// `pvPut(var)` of a double — the `DbChannel` spelling the ports use.
    pub async fn put_f64_process(&self, v: f64) -> Result<(), PvStat> {
        self.put(EpicsValue::Double(v))
    }

    pub async fn put_i16_process(&self, v: i16) -> Result<(), PvStat> {
        self.put(EpicsValue::Short(v))
    }

    pub async fn put_i32_process(&self, v: i32) -> Result<(), PvStat> {
        self.put(EpicsValue::Long(v))
    }

    pub async fn put_string_process(&self, v: &str) -> Result<(), PvStat> {
        self.put(EpicsValue::String(v.to_string().into()))
    }

    /// `pvPut(var, SYNC)` of a double.
    pub async fn put_f64_sync(&self, v: f64) -> Result<(), PvStat> {
        self.put_sync(EpicsValue::Double(v)).await
    }

    pub async fn put_i16_sync(&self, v: i16) -> Result<(), PvStat> {
        self.put_sync(EpicsValue::Short(v)).await
    }

    pub async fn put_i32_sync(&self, v: i32) -> Result<(), PvStat> {
        self.put_sync(EpicsValue::Long(v)).await
    }

    pub async fn put_string_sync(&self, v: &str) -> Result<(), PvStat> {
        self.put_sync(EpicsValue::String(v.to_string().into()))
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_command_line_overrides_the_program_parameters() {
        let m = SeqMacros::new("P=xxx:, M=m9, Y_OFF=35", "P=a:,Y_OFF=40");
        assert_eq!(m.value("P"), Some("a:"));
        assert_eq!(m.value("M"), Some("m9"));
        assert_eq!(m.value("Y_OFF"), Some("40"));
        assert_eq!(m.value("Q"), None);
    }

    #[test]
    fn a_name_without_a_value_keeps_the_one_it_had() {
        let m = SeqMacros::new("A=1", "A, B");
        assert_eq!(m.value("A"), Some("1"));
        assert_eq!(m.value("B"), None);
    }

    #[test]
    fn an_empty_value_is_a_value() {
        let m = SeqMacros::new("P=xxx:", "P=,H= ");
        assert_eq!(m.value("P"), Some(""));
        assert_eq!(m.value("H"), Some(""));
    }

    /// A value ends at a blank; what follows it is not a comma, so
    /// `seqMacParse` stops there.
    #[test]
    fn parsing_stops_where_the_grammar_breaks() {
        let m = SeqMacros::new("", "P=a b,Q=1");
        assert_eq!(m.value("P"), Some("a"));
        assert_eq!(m.value("Q"), None);
        let m = SeqMacros::new("", "_X=1,Y=2");
        assert_eq!(m.value("_X"), None);
        assert_eq!(m.value("Y"), None);
        let m = SeqMacros::new("", "X:1,Y=2");
        assert_eq!(m.value("Y"), None);
        let m = SeqMacros::new("", " A = 1 , B_2=x=y");
        assert_eq!(m.value("A"), Some("1"));
        assert_eq!(m.value("B_2"), Some("x=y"));
    }

    /// Poll until `probe` holds, or give up.
    async fn eventually(probe: impl Fn() -> bool) -> bool {
        for _ in 0..300 {
            if probe() {
                return true;
            }
            crate::runtime::task::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    fn started_flag() -> (
        Arc<AtomicBool>,
        impl FnOnce(PvDatabase) -> std::future::Ready<Result<(), String>> + Send + 'static,
    ) {
        let started = Arc::new(AtomicBool::new(false));
        let mark = Arc::clone(&started);
        (started, move |_db: PvDatabase| {
            mark.store(true, Ordering::Release);
            std::future::ready(Ok(()))
        })
    }

    #[epics_macros_rs::epics_test]
    async fn a_program_started_before_pini_runs_only_after_it() {
        let db = PvDatabase::new();
        let bridge = BlockingBridge::capture();
        let (started, run) = started_flag();
        spawn_program(&bridge, &db, "probe", &SeqMacros::default(), run);

        crate::runtime::task::sleep(Duration::from_millis(100)).await;
        assert!(
            !started.load(Ordering::Acquire),
            "the program ran before the PINI pass was published"
        );

        db.mark_pini_done();
        assert!(
            eventually(|| started.load(Ordering::Acquire)).await,
            "the program did not start once PINI was published"
        );
    }

    #[epics_macros_rs::epics_test]
    async fn a_program_started_after_pini_runs_at_once() {
        let db = PvDatabase::new();
        db.mark_pini_done();
        let bridge = BlockingBridge::capture();
        let (started, run) = started_flag();
        spawn_program(&bridge, &db, "probe", &SeqMacros::default(), run);

        assert!(eventually(|| started.load(Ordering::Acquire)).await);
    }

    use crate::server::records::ao::AoRecord;
    use crate::server::records::busy::BusyRecord;

    /// An `ao` `A` and a `busy` `B` behind one program's queue.
    async fn circuit() -> (PvDatabase, PutQueue) {
        let db = PvDatabase::new();
        db.add_record("A", Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
        db.add_record("B", Box::new(BusyRecord::default()))
            .await
            .unwrap();
        let q = PutQueue::new(&db, "ss");
        (db, q)
    }

    fn f64_of(db: &PvDatabase, pv: &str) -> f64 {
        db.get_pv(pv).unwrap().to_f64().unwrap()
    }

    /// The busy record's own reset, as its driver would make it. It waits for
    /// the circuit's put to set B first: a reset served ahead of it would
    /// leave the put pending for good.
    async fn release(db: &PvDatabase) {
        assert!(
            eventually(|| f64_of(db, "B") == 1.0).await,
            "the put never set B"
        );
        db.put_record_field_from_ca_no_notify("B", "VAL", EpicsValue::Short(0))
            .await
            .unwrap();
    }

    #[epics_macros_rs::epics_test]
    async fn a_default_put_returns_while_the_record_is_locked() {
        let (db, q) = circuit().await;
        let a = q.pv("a", "A");
        let held = db.lock_for_put("A", "VAL", &EpicsValue::Double(0.0));
        assert_eq!(a.put(EpicsValue::Double(4.0)), Ok(()));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(f64_of(&db, "A"), 0.0, "the put ran past the lock");
        drop(held);
        assert!(eventually(|| f64_of(&db, "A") == 4.0).await);
    }

    #[epics_macros_rs::epics_test]
    async fn a_get_after_default_puts_reads_the_last_of_them() {
        let (_db, q) = circuit().await;
        let a = q.pv("a", "A");
        for v in 1..=50 {
            a.put(EpicsValue::Double(f64::from(v))).unwrap();
        }
        assert_eq!(a.get_f64().await, 50.0);
    }

    /// A flush returns once the puts before it are in the database.
    #[epics_macros_rs::epics_test]
    async fn a_flush_returns_with_the_puts_before_it_taken() {
        let (db, q) = circuit().await;
        let a = q.pv("a", "A");
        for v in 1..=50 {
            a.put(EpicsValue::Double(f64::from(v))).unwrap();
        }
        q.flush().await;
        assert_eq!(f64_of(&db, "A"), 50.0);
    }

    /// The state sets of a program share its circuit.
    #[epics_macros_rs::epics_test]
    async fn another_state_set_reads_behind_the_puts_of_the_first() {
        let (_db, q) = circuit().await;
        let a = q.pv("a", "A");
        let other = q.state_set("other").pv("a", "A");
        for v in 1..=50 {
            a.put(EpicsValue::Double(f64::from(v))).unwrap();
        }
        assert_eq!(other.get_f64().await, 50.0);
    }

    /// `putReq` is per channel and state set (`seq_if.c:128-139`): one state
    /// set's pending put does not refuse another's.
    #[epics_macros_rs::epics_test]
    async fn a_pending_put_refuses_only_its_own_state_set() {
        let (db, q) = circuit().await;
        let b = q.pv("b", "B");
        let other = q.state_set("other").pv("b", "B");
        b.put_async(EpicsValue::Short(1)).unwrap();
        assert_eq!(b.put_async(EpicsValue::Short(1)), Err(PvStat::Error));
        assert_eq!(other.put_async(EpicsValue::Short(1)), Ok(()));
        release(&db).await;
    }

    #[epics_macros_rs::epics_test]
    async fn a_put_to_no_record_is_disconnected() {
        let (_db, q) = circuit().await;
        let x = q.pv("x", "X");
        assert_eq!(x.put(EpicsValue::Double(1.0)), Err(PvStat::Disconn));
        assert_eq!(x.put_async(EpicsValue::Double(1.0)), Err(PvStat::Disconn));
        assert_eq!(
            x.put_sync(EpicsValue::Double(1.0)).await,
            Err(PvStat::Disconn)
        );
        assert!(x.put_complete());
    }

    #[epics_macros_rs::epics_test]
    async fn a_sync_put_to_a_synchronous_record_completes_at_once() {
        let (db, mut q) = circuit().await;
        let a = q.pv("a", "A");
        assert_eq!(a.put_sync(EpicsValue::Double(3.0)).await, Ok(()));
        assert_eq!(f64_of(&db, "A"), 3.0);
        assert!(a.put_complete());
        assert_eq!(&*q.completion().await, "A");
    }

    #[epics_macros_rs::epics_test]
    async fn a_sync_put_waits_for_the_record_to_finish() {
        let (db, mut q) = circuit().await;
        let b = q.pv("b", "B");
        let resetter = db.clone();
        let reset = crate::runtime::task::spawn_background(
            crate::runtime::task::CallbackPriority::Medium,
            async move {
                crate::runtime::task::sleep(Duration::from_millis(300)).await;
                release(&resetter).await;
            },
        );
        let started = std::time::Instant::now();
        assert_eq!(b.put_sync(EpicsValue::Short(1)).await, Ok(()));
        assert!(started.elapsed() >= Duration::from_millis(300));
        let _ = reset.await;
        assert_eq!(&*q.completion().await, "B");
    }

    /// A refused put whose record still processes (calcout's `A` is pp,
    /// `ODLY` holds the process) completes when that process does: C's
    /// callback comes from `dbNotifyCompletion` (`dbNotify.c:528-530`).
    #[epics_macros_rs::epics_test]
    async fn a_refused_sync_put_waits_for_the_process_it_started() {
        let (db, _q) = circuit().await;
        db.add_record(
            "C",
            Box::new(crate::server::records::calcout::CalcoutRecord::default()),
        )
        .await
        .unwrap();
        db.put_record_field_from_ca("C", "ODLY", EpicsValue::Double(0.3))
            .await
            .unwrap();
        let q = PutQueue::new(&db, "ss");
        let a = q.pv("a", "C.A");
        let started = std::time::Instant::now();
        let _ = a.put_sync(EpicsValue::String("nope".into())).await;
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "answered {:?} into C's delay",
            started.elapsed()
        );
    }

    #[epics_macros_rs::epics_test]
    async fn a_sync_put_times_out_and_its_late_completion_is_ignored() {
        let (db, mut q) = circuit().await;
        let b = q.pv("b", "B");
        assert_eq!(
            b.put_sync_tmo(EpicsValue::Short(1), Duration::from_millis(200))
                .await,
            Err(PvStat::Timeout)
        );
        assert!(b.put_complete(), "the timed-out request is cancelled");
        release(&db).await;
        crate::runtime::task::sleep(Duration::from_millis(100)).await;
        assert_eq!(q.try_completion(), None);
    }

    #[epics_macros_rs::epics_test]
    async fn an_async_put_is_refused_while_one_is_pending() {
        let (db, mut q) = circuit().await;
        let b = q.pv("b", "B");
        assert_eq!(b.put_async(EpicsValue::Short(1)), Ok(()));
        assert!(!b.put_complete());
        assert_eq!(b.put_async(EpicsValue::Short(1)), Err(PvStat::Error));
        release(&db).await;
        assert_eq!(&*q.completion().await, "B");
        assert!(b.put_complete());
        assert_eq!(b.put_async(EpicsValue::Short(1)), Ok(()));
    }

    /// `check_pending` names the program variable and the PV apart
    /// (`seq_if.c:89-136`).
    #[epics_macros_rs::epics_test]
    async fn the_refusals_print_the_variable_and_the_pv() {
        use crate::runtime::log::{errlog_add_listener, errlog_flush, errlog_remove_listener};
        let (db, q) = circuit().await;
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = lines.clone();
        let id = errlog_add_listener(move |m| sink.lock().unwrap().push(m.trim_end().to_owned()));
        let b = q.pv("motTTH", "B");
        b.put_async(EpicsValue::Short(1)).unwrap();
        let _ = b.put_async(EpicsValue::Short(1));
        let _ = b
            .put_sync_tmo(EpicsValue::Short(1), Duration::from_millis(50))
            .await;
        let _ = b.put_sync_tmo(EpicsValue::Short(1), Duration::ZERO).await;
        errlog_flush();
        errlog_remove_listener(id);
        release(&db).await;
        assert_eq!(
            *lines.lock().unwrap(),
            [
                "sevr=major pvPut(ss ss, var motTTH, pv B): user error (there is already a pvPut pending for this channel/state set combination)",
                "sevr=major pvPut(ss ss, var motTTH, pv B): failed (timeout waiting for other pvPut requests to finish)",
                "sevr=major pvPut(motTTH,SYNC,0.000000): user error (timeout must be positive)",
            ]
        );
    }

    #[epics_macros_rs::epics_test]
    async fn a_sync_put_waits_out_a_pending_async_put() {
        let (db, q) = circuit().await;
        let b = q.pv("b", "B");
        b.put_async(EpicsValue::Short(1)).unwrap();
        assert_eq!(
            b.put_sync_tmo(EpicsValue::Short(0), Duration::from_millis(200))
                .await,
            Err(PvStat::Timeout),
            "check_pending times out on the pending put"
        );
        q.flush().await;
        assert_eq!(f64_of(&db, "B"), 1.0, "the SYNC put was not issued");
        let resetter = db.clone();
        crate::runtime::task::spawn_background(
            crate::runtime::task::CallbackPriority::Medium,
            async move {
                crate::runtime::task::sleep(Duration::from_millis(200)).await;
                release(&resetter).await;
            },
        );
        assert_eq!(b.put_sync(EpicsValue::Short(0)).await, Ok(()));
        assert!(b.put_complete());
    }
}
