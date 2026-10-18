//! `saveData_writeXDR.c`'s task: `saveData_Init`, the CA connections the
//! req file names, `saveDataTask`'s message loop and its `proc_*` handlers.
//!
//! One thread owns every piece of state. C's CA callbacks write some of it
//! from the CA thread (`dataMonitor`'s handshake and `nb_scan_running`, the
//! `pdest` strings); here a subscription only forwards its value into the
//! queue, and the task does that work when it takes the message, in the
//! order the messages came.
//!
//! The channels are [`CaClientContext`]'s: a name this IOC serves is an
//! in-process channel, as C's `dbServiceIOInit` gives saveData's
//! `ca_context_create`; any other goes to the network.

use std::future::Future;
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::runtime::task::{
    BlockingBridge, TaskHandle, ThreadPriority, apply_to_current_thread, timeout,
};
use epics_base_rs::server::builtin_devices::timestamp::epics_time_to_strftime;
use epics_base_rs::server::database::{CaClientContext, ClientChannel, Connection};
use epics_base_rs::server::iocsh::register_command;
use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome, CommandResult,
};
use epics_base_rs::server::iocsh::vars::{VarAccess, VarDef, register_variable};
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{
    DBR_CHAR, DBR_CTRL_CHAR, DBR_CTRL_DOUBLE, DBR_CTRL_FLOAT, DBR_CTRL_LONG, DBR_CTRL_SHORT,
    DBR_DOUBLE, DBR_FLOAT, DBR_LONG, DBR_SHORT, DBR_STRING, DBR_TIME_SHORT,
};
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString, WallTime};
use futures_util::future::join_all;
use parking_lot::Mutex;

use super::mda::{
    self, DEBUG_SAVE_DATA_MSG, Env, ExtraPv, ExtraPvState, ExtraValue, MAX_STRING_SIZE, SCAN_NBD,
    SCAN_NBP, SCAN_NBT, STATUS_ACTIVE_FS_ERROR, STATUS_ACTIVE_OK, STATUS_INACTIVE, Scan, XXNV_OK,
    debug, file_status, truncate_message,
};
use super::req_file::ReqFile;

/// `SAVE_DATA_VERSION`.
pub const SAVE_DATA_VERSION: &str = "1.32.1";

/// `saveData_MessagePolicy`: how `.CPT` events reach the queue — 0 wait
/// for room, 1 drop when full, 2 drop when full and at most one per
/// `cpt_wait_time`, 3 wait and at most one per `cpt_wait_time`.
pub static MESSAGE_POLICY: AtomicI32 = AtomicI32::new(0);

/// `maxAllowedRetries`.
static MAX_ALLOWED_RETRIES: AtomicI64 = AtomicI64::new(5);
/// `retryWaitInSecs`.
static RETRY_WAIT_IN_SECS: AtomicI64 = AtomicI64::new(20);
/// `cpt_wait_time` in seconds, as `f64` bits.
static CPT_WAIT_TIME: AtomicU64 = AtomicU64::new(0);

/// `MAX_MSG`: the queue's depth.
const MAX_MSG: usize = 1000;
/// `PVNAME_STRINGSZ`.
const PVNAME_STRINGSZ: usize = 61;
/// `PREFIX_SIZE`.
const PREFIX_SIZE: usize = PVNAME_STRINGSZ / 2;
/// `COMPONENT_STRING_SIZE`.
const COMPONENT_STRING_SIZE: usize = 256;
/// `FNAMELEN`.
const FNAMELEN: usize = 512;
/// `MAX_UNITS_SIZE`.
const MAX_UNITS_SIZE: usize = 8;
/// `HANDSHAKE_BUSY` / `HANDSHAKE_DONE`.
const HANDSHAKE_BUSY: i16 = 1;
const HANDSHAKE_DONE: i16 = 0;
const DBE_VALUE: u16 = 1;
const DBE_ALARM: u16 = 4;
/// `ca_add_event`'s mask.
const DBE_DEFAULT: u16 = DBE_VALUE | DBE_ALARM;
const STAMP_FORMAT: &str = "%b %d, %Y %H:%M:%S.%06f";

/// C `DebugMsg<n>(d, ...)`: printed when `d <= debug_saveDataMsg`.
macro_rules! debug_msg {
    ($d:expr, $($arg:tt)*) => {
        if $d <= DEBUG_SAVE_DATA_MSG.load(Ordering::Relaxed) {
            print!($($arg)*);
        }
    };
}

fn cpt_wait_time() -> f64 {
    f64::from_bits(CPT_WAIT_TIME.load(Ordering::Relaxed))
}

/// The bytes before `max - 1`, as C's `strncpy` into a `max` buffer with
/// the last byte forced to NUL leaves them.
fn bounded(s: &str, max: usize) -> String {
    let mut end = s.len().min(max - 1);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// A C string out of a CA value: a `DBR_STRING`, or a `DBR_CHAR` array
/// (a `.$` long string) up to its NUL.
fn value_string(v: &EpicsValue) -> String {
    match v {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        EpicsValue::CharArray(a) => {
            let b = a.as_slice();
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf8_lossy(&b[..end]).into_owned()
        }
        EpicsValue::Char(c) if *c == 0 => String::new(),
        other => other.to_string(),
    }
}

/// Every element of a CA value as `f64`.
fn elements(v: &EpicsValue) -> Vec<f64> {
    fn all<T: Copy + Into<f64>>(a: &[T]) -> Vec<f64> {
        a.iter().map(|&x| x.into()).collect()
    }
    match v {
        EpicsValue::DoubleArray(a) => a.as_slice().to_vec(),
        EpicsValue::FloatArray(a) => all(a.as_slice()),
        EpicsValue::LongArray(a) => all(a.as_slice()),
        EpicsValue::ShortArray(a) => all(a.as_slice()),
        EpicsValue::EnumArray(a) => all(a.as_slice()),
        EpicsValue::CharArray(a) => all(a.as_slice()),
        EpicsValue::Int64Array(a) => a.as_slice().iter().map(|&x| x as f64).collect(),
        other => other.to_f64().into_iter().collect(),
    }
}

fn units(s: &Snapshot) -> String {
    s.units()
        .map(|u| bounded(&u.as_str_lossy(), MAX_UNITS_SIZE))
        .unwrap_or_default()
}

/// `ca_field_type` → the type `connectPV` gets the value as. CA reports
/// the unsigned and 64-bit fields as the old type that holds them
/// (`dbDBRnewToDBRold`), so every field type has one.
fn extra_dbr_type(native: DbFieldType) -> u16 {
    match native {
        DbFieldType::String | DbFieldType::Enum => DBR_STRING,
        DbFieldType::Char | DbFieldType::UChar => DBR_CTRL_CHAR,
        DbFieldType::Short => DBR_CTRL_SHORT,
        DbFieldType::Long | DbFieldType::UShort => DBR_CTRL_LONG,
        DbFieldType::Float => DBR_CTRL_FLOAT,
        DbFieldType::Double | DbFieldType::Int64 | DbFieldType::UInt64 | DbFieldType::ULong => {
            DBR_CTRL_DOUBLE
        }
    }
}

/// `extraValCallback`: the value a get of `dbr_type` delivered.
fn extra_value(dbr_type: u16, s: &Snapshot) -> ExtraValue {
    let units = units(s);
    let v = elements(&s.value);
    match dbr_type {
        DBR_STRING => ExtraValue::String(bounded(&value_string(&s.value), MAX_STRING_SIZE)),
        DBR_CTRL_CHAR => ExtraValue::Char {
            units,
            value: v.iter().map(|&x| x as i64 as u8).collect(),
        },
        DBR_CTRL_SHORT => ExtraValue::Short {
            units,
            value: v.iter().map(|&x| x as i16).collect(),
        },
        DBR_CTRL_LONG => ExtraValue::Long {
            units,
            value: v.iter().map(|&x| x as i32).collect(),
        },
        DBR_CTRL_FLOAT => ExtraValue::Float {
            units,
            value: v.iter().map(|&x| x as f32).collect(),
        },
        _ => ExtraValue::Double { units, value: v },
    }
}

type Chan = Arc<dyn ClientChannel>;

/// One `evid`: the task forwarding a subscription's values, cancelled on
/// drop (`ca_clear_subscription`, or the `ca_clear_channel` that ends it).
struct Sub(TaskHandle<()>);

impl Drop for Sub {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A channel and the subscription on it (C's `cpxds`/`crxds`/`cdxds` with
/// their `descMonitor`).
struct Watched {
    _ch: Chan,
    _sub: Option<Sub>,
}

/// Where a `descMonitor` string lands (C's `pdest`).
#[derive(Debug, Clone, Copy)]
enum Dest {
    Pxds(usize, usize),
    Rxds(usize, usize),
    Dxds(usize, usize),
    Pxsm(usize, usize),
}

/// One `epicsMessageQueue` message.
enum Msg {
    Data {
        scan: usize,
        stamp: WallTime,
        val: i16,
    },
    Npts {
        scan: usize,
        val: i32,
    },
    Cpt {
        scan: usize,
        val: i32,
    },
    Pxnv {
        scan: usize,
        i: usize,
        val: i16,
    },
    Rxnv {
        scan: usize,
        i: usize,
        val: i16,
    },
    Dxnv {
        scan: usize,
        i: usize,
        val: i16,
    },
    Txnv {
        scan: usize,
        i: usize,
        val: i16,
    },
    Txcd {
        scan: usize,
        i: usize,
        val: f32,
    },
    /// `MSG_DESC` and `MSG_SCAN_PXSM`.
    Text {
        dest: Dest,
        val: String,
    },
    FileSystem(String),
    FileSubdir(String),
    FileBasename(String),
    RealTime1D(i32),
    /// `saveData_Info` / `saveData_PrintScanInfo`: the text, sent back.
    Info(Option<String>, std::sync::mpsc::Sender<String>),
    /// `saveData_Priority`.
    Priority(u32),
}

/// The CA half: `ca_search`, `ca_pend_io`, gets, puts and subscriptions,
/// run from the task thread.
struct Ca {
    ctx: CaClientContext,
    bridge: BlockingBridge,
    tx: mpsc::Sender<Msg>,
}

impl Ca {
    fn run<F: Future>(&self, f: F) -> F::Output {
        self.bridge.block_on(f)
    }

    /// `ca_search`: `None` for an empty name (`ECA_EMPTYSTR`) or no context.
    fn search(&self, name: &str) -> Option<Chan> {
        if name.is_empty() {
            return None;
        }
        self.run(self.ctx.create_channel(name))
    }

    /// `ca_pend_io` over channels just searched: whether each connected
    /// within `secs`. A channel is pended once, on its first state.
    fn pend(&self, chans: &[Option<&Chan>], secs: f64) -> Vec<bool> {
        let d = Duration::from_secs_f64(secs);
        self.run(join_all(chans.iter().map(|ch| async move {
            let Some(ch) = ch else { return false };
            timeout(d, async {
                while let Some(c) = ch.next_connection().await {
                    if c == Connection::Connected {
                        return true;
                    }
                }
                false
            })
            .await
            .unwrap_or(false)
        })))
    }

    /// `ca_search` + `ca_pend_io(secs)`: the channel, connected or not, and
    /// whether it connected.
    fn connect(&self, name: &str, secs: f64) -> (Option<Chan>, bool) {
        let ch = self.search(name);
        let up = self.pend(&[ch.as_ref()], secs)[0];
        (ch, up)
    }

    /// `ca_array_get`s completed by one `ca_pend_io(secs)`. `None` in the
    /// outer slot is the pend timing out; an inner `None` is a get that
    /// failed at once (a disconnected channel), which leaves C's buffer as
    /// it was without failing the pend.
    fn gets(&self, reqs: &[(&Chan, u16, u32)], secs: f64) -> Option<Vec<Option<Snapshot>>> {
        let d = Duration::from_secs_f64(secs);
        let out = self.run(join_all(reqs.iter().map(|&(ch, t, n)| async move {
            match timeout(d, ch.get(t, n)).await {
                Ok(Ok(s)) => Ok(Some(s)),
                Ok(Err(_)) => Ok(None),
                Err(_) => Err(()),
            }
        })));
        out.into_iter().collect::<Result<_, _>>().ok()
    }

    fn get(&self, ch: &Chan, dbr_type: u16, count: u32, secs: f64) -> Option<Snapshot> {
        self.gets(&[(ch, dbr_type, count)], secs)?.pop().flatten()
    }

    /// `ca_array_put`, issued and not waited on.
    fn put(&self, ch: Option<&Chan>, v: EpicsValue) {
        if let Some(ch) = ch {
            let _ = self.run(ch.put(v, false));
        }
    }

    /// `ca_add_masked_array_event`: `f` turns each value into the message
    /// to queue and whether to wait for room (`epicsMessageQueueSend`) or
    /// drop it when the queue is full (`epicsMessageQueueTrySend`).
    fn monitor(
        &self,
        ch: &Chan,
        dbr_type: u16,
        count: u32,
        mask: u16,
        mut f: impl FnMut(Snapshot) -> Option<(Msg, bool)> + Send + 'static,
    ) -> Option<Sub> {
        let mut m = self.run(ch.subscribe(dbr_type, count, mask)).ok()?;
        let tx = self.tx.clone();
        let task = self.ctx.spawn(async move {
            while let Some(s) = m.next().await {
                match f(s) {
                    Some((msg, true)) => {
                        if tx.send(msg).await.is_err() {
                            return;
                        }
                    }
                    Some((msg, false)) => {
                        let _ = tx.try_send(msg);
                    }
                    None => {}
                }
            }
        })?;
        Some(Sub(task))
    }
}

/// A `SCAN`'s channels (its `chid` and `evid` fields).
struct ScanChans {
    hand_shake: Option<Chan>,
    auto_hand_shake: Option<Chan>,
    data: Option<Chan>,
    mpts: Option<Chan>,
    npts: Option<Chan>,
    cpt: Option<Chan>,
    bcpt: Option<Chan>,
    pxnv: [Option<Chan>; SCAN_NBP],
    pxpv: [Option<Chan>; SCAN_NBP],
    pxsm: [Option<Chan>; SCAN_NBP],
    rxnv: [Option<Chan>; SCAN_NBP],
    rxpv: [Option<Chan>; SCAN_NBP],
    pxra: [Option<Chan>; SCAN_NBP],
    rxcv: [Option<Chan>; SCAN_NBP],
    pxds: [Option<Watched>; SCAN_NBP],
    pxeu: [Option<Chan>; SCAN_NBP],
    rxds: [Option<Watched>; SCAN_NBP],
    rxeu: [Option<Chan>; SCAN_NBP],
    dxnv: Vec<Option<Chan>>,
    dxpv: Vec<Option<Chan>>,
    dxda: Vec<Option<Chan>>,
    dxcv: Vec<Option<Chan>>,
    dxds: Vec<Option<Watched>>,
    dxeu: Vec<Option<Chan>>,
    txnv: [Option<Chan>; SCAN_NBT],
    txpv: [Option<Chan>; SCAN_NBT],
    txcd: [Option<Chan>; SCAN_NBT],
    /// `cpt_evid` (`cpt_monitored` is its presence).
    cpt_sub: Option<Sub>,
    /// `cpt_time`, read by the `.CPT` forwarder.
    cpt_time: Arc<Mutex<Instant>>,
    subs: Vec<Sub>,
}

impl ScanChans {
    fn new() -> Self {
        Self {
            hand_shake: None,
            auto_hand_shake: None,
            data: None,
            mpts: None,
            npts: None,
            cpt: None,
            bcpt: None,
            pxnv: Default::default(),
            pxpv: Default::default(),
            pxsm: Default::default(),
            rxnv: Default::default(),
            rxpv: Default::default(),
            pxra: Default::default(),
            rxcv: Default::default(),
            pxds: Default::default(),
            pxeu: Default::default(),
            rxds: Default::default(),
            rxeu: Default::default(),
            dxnv: vec![None; SCAN_NBD],
            dxpv: vec![None; SCAN_NBD],
            dxda: vec![None; SCAN_NBD],
            dxcv: vec![None; SCAN_NBD],
            dxds: (0..SCAN_NBD).map(|_| None).collect(),
            dxeu: vec![None; SCAN_NBD],
            txnv: Default::default(),
            txpv: Default::default(),
            txcd: Default::default(),
            cpt_sub: None,
            cpt_time: Arc::new(Mutex::new(Instant::now())),
            subs: Vec::new(),
        }
    }
}

/// One extra PV's channel (`PV_NODE.channel`, `dbr_type`).
struct ExtraChan {
    ch: Chan,
    dbr_type: u16,
    pv: Arc<ExtraPv>,
}

/// What the MDA writes reach beyond the scan.
struct Io<'a> {
    ca: &'a Ca,
    save_status: &'a mut i16,
    status_ch: Option<&'a Chan>,
    message_ch: Option<&'a Chan>,
    extra: &'a [Arc<ExtraPv>],
}

impl Env for Io<'_> {
    fn save_status(&self) -> i16 {
        *self.save_status
    }

    fn set_save_status(&mut self, status: i16) {
        *self.save_status = status;
        self.ca.put(self.status_ch, EpicsValue::Short(status));
    }

    fn user_message(&mut self, msg: &str) {
        let mut msg = msg.to_string();
        truncate_message(&mut msg);
        self.ca
            .put(self.message_ch, EpicsValue::String(PvString::from(msg)));
    }

    fn extra_pvs(&self) -> &[Arc<ExtraPv>] {
        self.extra
    }
}

struct Task {
    ca: Ca,
    req_file: String,
    req_macros: String,
    scans: Vec<Scan>,
    chans: Vec<ScanChans>,
    extra: Vec<Arc<ExtraPv>>,
    extra_chans: Vec<ExtraChan>,
    save_status: i16,
    save_status_ch: Option<Chan>,
    message_ch: Option<Chan>,
    filename_ch: Option<Chan>,
    full_pathname_ch: Option<Chan>,
    counter_ch: Option<Chan>,
    file_subdir_ch: Option<Chan>,
    curr_retries_ch: Option<Chan>,
    total_retries_ch: Option<Chan>,
    abandoned_writes_ch: Option<Chan>,
    /// `file_system_state == FS_MOUNTED`.
    mounted: bool,
    /// `server_pathname`, and where its subdirectory starts
    /// (`server_subdir`).
    server_pathname: String,
    server_subdir: usize,
    /// `local_subdir`: the subdirectory as last set.
    local_subdir: String,
    real_time_1d: i32,
    ioc_prefix: String,
    basename: String,
    nb_scan_running: i32,
    curr_retries: i64,
    total_retries: i64,
    abandoned_writes: i64,
    subs: Vec<Sub>,
}

/// `msg_queue` and `threadId` once `saveData_Init` ran.
static TASK: OnceLock<mpsc::Sender<Msg>> = OnceLock::new();

/// `saveData_Init`: start the task on `fname` with `macros`, and return
/// once it has configured itself or given up (C suspends the caller until
/// then).
pub fn save_data_init(fname: &str, macros: &str, ctx: CaClientContext, bridge: BlockingBridge) {
    let (tx, rx) = mpsc::channel(MAX_MSG);
    if TASK.set(tx.clone()).is_err() {
        println!("saveData already initialized");
        return;
    }
    println!("saveData: message queue created");
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let ca = Ca { ctx, bridge, tx };
    let (fname, macros) = (bounded(fname, 40), bounded(macros, 40));
    let spawned = std::thread::Builder::new()
        .name("saveDataTask".into())
        .spawn(move || {
            // C `PRIORITY`.
            apply_to_current_thread(ThreadPriority::Custom(
                ThreadPriority::CaServerHigh.value() + 4,
            ));
            save_data_task(ca, fname, macros, rx, done_tx);
        });
    if spawned.is_err() {
        debug!(1, "Unable to create saveDataTask\n");
        return;
    }
    let _ = done_rx.recv();
}

/// Ask the task for text and wait for it.
fn ask(name: Option<&str>) -> Option<String> {
    let tx = TASK.get()?;
    let (reply, rx) = std::sync::mpsc::channel();
    tx.blocking_send(Msg::Info(name.map(str::to_string), reply))
        .ok()?;
    rx.recv().ok()
}

/// `saveData_PrintScanInfo`.
pub fn save_data_print_scan_info(name: &str) -> String {
    ask(Some(name)).unwrap_or_default()
}

/// `saveData_Info`.
pub fn save_data_info() -> String {
    ask(None).unwrap_or_else(|| "saveData: scan info:\n".to_string())
}

/// `saveData_Priority`: the task applies it when it takes the request.
pub fn save_data_priority(p: i32) {
    if let Some(tx) = TASK.get() {
        let _ = tx.blocking_send(Msg::Priority(p.max(0) as u32));
    }
}

/// `saveData_SetCptWait_ms`. C computes `(double)(ms/1000)`, an integer
/// division that turns every wait under a second into none; the
/// millisecond value is kept here.
pub fn save_data_set_cpt_wait_ms(ms: i32) {
    CPT_WAIT_TIME.store((f64::from(ms) / 1000.0).to_bits(), Ordering::Relaxed);
}

/// `saveDataTask`.
fn save_data_task(
    ca: Ca,
    req_file: String,
    req_macros: String,
    mut rx: mpsc::Receiver<Msg>,
    done: std::sync::mpsc::Sender<()>,
) {
    debug!(1, "Task saveDataTask running...\n");
    CPT_WAIT_TIME.store(0.1f64.to_bits(), Ordering::Relaxed);
    let mut task = Task {
        ca,
        req_file,
        req_macros,
        scans: Vec::new(),
        chans: Vec::new(),
        extra: Vec::new(),
        extra_chans: Vec::new(),
        save_status: STATUS_INACTIVE,
        save_status_ch: None,
        message_ch: None,
        filename_ch: None,
        full_pathname_ch: None,
        counter_ch: None,
        file_subdir_ch: None,
        curr_retries_ch: None,
        total_retries_ch: None,
        abandoned_writes_ch: None,
        mounted: false,
        server_pathname: String::new(),
        server_subdir: 0,
        local_subdir: String::new(),
        real_time_1d: 1,
        ioc_prefix: String::new(),
        basename: String::new(),
        nb_scan_running: 0,
        curr_retries: 0,
        total_retries: 0,
        abandoned_writes: 0,
        subs: Vec::new(),
    };
    if task.init().is_err() {
        println!("saveData: Unable to configure saveDataTask");
        let _ = done.send(());
        return;
    }
    debug!(1, "saveDataTask waiting for messages\n");
    let _ = done.send(());
    while let Some(msg) = rx.blocking_recv() {
        task.dispatch(msg);
    }
    debug!(
        1,
        "saveDataTask: epicsMessageQueueReceive returned neg. number\n"
    );
}

/// `req_readMacId` as C's callers see it: the length, -1 for an undefined
/// macro, and the name (empty then).
fn mac_id(rf: &mut ReqFile, max: usize) -> (i32, String) {
    match rf.read_mac_id(max) {
        Some(s) => (s.len() as i32, s),
        None => (-1, String::new()),
    }
}

/// `strcspn(s, ".")`.
fn record_part(s: &str) -> &str {
    &s[..s.find('.').unwrap_or(s.len())]
}

impl Task {
    fn io(&mut self) -> (&mut Vec<Scan>, &Vec<ScanChans>, Io<'_>) {
        (
            &mut self.scans,
            &self.chans,
            Io {
                ca: &self.ca,
                save_status: &mut self.save_status,
                status_ch: self.save_status_ch.as_ref(),
                message_ch: self.message_ch.as_ref(),
                extra: &self.extra,
            },
        )
    }

    fn send_user_message(&self, msg: &str) {
        self.ca.put(
            self.message_ch.as_ref(),
            EpicsValue::String(PvString::from(msg)),
        );
    }

    fn put_status(&self) {
        self.ca.put(
            self.save_status_ch.as_ref(),
            EpicsValue::Short(self.save_status),
        );
    }

    fn put_long(&self, ch: Option<&Chan>, v: i64) {
        self.ca.put(ch, EpicsValue::Long(v as i32));
    }

    fn put_full_pathname(&self) {
        let mut bytes = self.server_pathname.as_bytes().to_vec();
        bytes.push(0);
        self.ca.put(
            self.full_pathname_ch.as_ref(),
            EpicsValue::CharArray(bytes.into()),
        );
    }

    fn search_scan(&self, name: &str) -> Option<usize> {
        debug!(2, "searchScan '{name}'\n");
        self.scans.iter().position(|s| s.name == name)
    }

    /// `initSaveDataTask`.
    fn init(&mut self) -> Result<(), ()> {
        let Some(mut rf) = ReqFile::open(&self.req_file, Some(&self.req_macros)) else {
            println!("Unable to open \"{}\". saveDataTask aborted", self.req_file);
            return Err(());
        };

        // the IOC prefix
        if rf.goto_section("prefix") {
            self.ioc_prefix = mac_id(&mut rf, PREFIX_SIZE).1;
        }
        self.connect_retry_pvs();
        // punctuation to '_', so the prefix can name a file
        self.ioc_prefix = self
            .ioc_prefix
            .chars()
            .map(|c| if c.is_ascii_punctuation() { '_' } else { c })
            .collect();

        if !rf.goto_section("status") {
            println!("saveData: section [status] not found");
            return Err(());
        }
        let (n, name) = mac_id(&mut rf, PVNAME_STRINGSZ);
        if n == 0 {
            println!("saveData: status pv name not defined");
            return Err(());
        }
        let (ch, up) = self.ca.connect(&name, 0.5);
        self.save_status_ch = ch;
        if !up {
            println!("saveData: Unable to connect {name}");
            return Err(());
        }

        self.message_ch = self.optional_pv(&mut rf, "message", "message", "");
        self.filename_ch = self.optional_pv(&mut rf, "filename", "filename", "");
        self.full_pathname_ch = self.optional_pv(&mut rf, "fullPathName", "fullPathName", ".$");

        if !rf.goto_section("counter") {
            println!("saveData: section [counter] not found");
            return Err(());
        }
        let (n, name) = mac_id(&mut rf, PVNAME_STRINGSZ);
        if n == 0 {
            println!("saveData: counter pv name not defined");
            return Err(());
        }
        // connectCounter
        let (ch, up) = self.ca.connect(&name, 0.5);
        self.counter_ch = ch;
        if !up {
            println!("Can't connect counter {name}");
            println!("saveData: connectCounter({name}) failed ");
            return Err(());
        }

        if !rf.goto_section("fileSystem") {
            println!("saveData: section [fileSystem] not found");
            return Err(());
        }
        let (n, name) = mac_id(&mut rf, PVNAME_STRINGSZ);
        if n == 0 {
            println!("saveData: fileSystem pv name not defined");
            return Err(());
        }
        let name = format!("{name}.$");
        if self
            .connect_long_string(&name, true, Msg::FileSystem)
            .is_none()
        {
            println!("saveData: connectFileSystem({name}) failed ");
            return Err(());
        }

        if !rf.goto_section("subdir") {
            println!("saveData: section [subdir] not found");
            return Err(());
        }
        let (n, name) = mac_id(&mut rf, PVNAME_STRINGSZ);
        if n == 0 {
            println!("saveData: subDir pv name not defined");
            return Err(());
        }
        let name = format!("{name}.$");
        match self.connect_long_string(&name, true, Msg::FileSubdir) {
            Some(ch) => self.file_subdir_ch = Some(ch),
            None => {
                println!("saveData: connectSubdir({name}) failed ");
                return Err(());
            }
        }

        // We can run without the basename PV.
        if !rf.goto_section("basename") {
            println!("saveData: section [basename] not found");
        } else {
            let (n, name) = mac_id(&mut rf, PVNAME_STRINGSZ);
            if n == 0 {
                println!("saveData: baseName pv name not defined");
            } else {
                self.connect_long_string(&format!("{name}.$"), false, Msg::FileBasename);
            }
        }

        if rf.goto_section("scanRecord") {
            while !rf.eos() {
                let (n, name) = mac_id(&mut rf, PVNAME_STRINGSZ);
                if n == 0 && rf.current() != Some(b',') {
                    // C loops here forever on a character that is neither
                    // an id nor a comment; step past it.
                    rf.read_char();
                    rf.skip_comment();
                    continue;
                }
                if rf.current() == Some(b',') {
                    rf.read_char();
                    let (_, hs) = mac_id(&mut rf, PVNAME_STRINGSZ);
                    println!("saveData: handshake PV '{hs}' ignored.  Using '{name}.AWAIT'");
                }
                let hand_shake = bounded(&format!("{name}.AWAIT"), PVNAME_STRINGSZ);
                let auto = bounded(&format!("{name}.AAWAIT"), PVNAME_STRINGSZ);
                debug!(2, "saveData: call connectScan({name},{hand_shake})\n");
                self.connect_scan(&name, &hand_shake, &auto);
            }
        }

        if rf.goto_section("extraPV") {
            while !rf.eos() {
                let (n, name) = mac_id(&mut rf, PVNAME_STRINGSZ);
                let desc = if rf.current() == Some(b'"') {
                    rf.read_string(PVNAME_STRINGSZ)
                } else {
                    String::new()
                };
                if n == 0 && desc.is_empty() {
                    // The same endless loop as [scanRecord] in C.
                    rf.read_char();
                    rf.skip_comment();
                    continue;
                }
                self.connect_pv(&name, &desc);
            }
        }

        if !rf.goto_section("realTime1D") {
            println!("saveData: section [realTime1D] not found");
        } else {
            let (n, name) = mac_id(&mut rf, PVNAME_STRINGSZ);
            if n == 0 {
                println!("saveData: realTime1D pv name not defined");
            } else {
                self.connect_real_time_1d(&name);
            }
        }

        self.monitor_scans();
        Ok(())
    }

    /// The optional `[message]` / `[filename]` / `[fullPathName]` PVs.
    fn optional_pv(
        &self,
        rf: &mut ReqFile,
        section: &str,
        what: &str,
        suffix: &str,
    ) -> Option<Chan> {
        if !rf.goto_section(section) {
            println!("saveData: section [{section}] not found");
            return None;
        }
        let (n, name) = mac_id(rf, PVNAME_STRINGSZ);
        if n == 0 {
            println!("saveData: {what} pv name not defined");
            return None;
        }
        self.ca
            .connect(&bounded(&format!("{name}{suffix}"), PVNAME_STRINGSZ), 0.5)
            .0
    }

    /// `connectFileSystem` / `connectSubdir` / `connectBasename`: a `.$`
    /// long string monitored as `DBR_CHAR[256]`. `None` when it did not
    /// connect or cannot be monitored; `required` picks C's messages.
    fn connect_long_string(
        &mut self,
        name: &str,
        required: bool,
        msg: fn(String) -> Msg,
    ) -> Option<Chan> {
        let (ch, up) = self.ca.connect(name, 0.5);
        if !up {
            if required {
                println!("saveData: Unable to connect {name}\nsaveDataTask not initialized");
            } else {
                println!("saveData: Unable to connect {name}");
            }
            return None;
        }
        let ch = ch?;
        let sub = self.ca.monitor(
            &ch,
            DBR_CHAR,
            COMPONENT_STRING_SIZE as u32,
            DBE_VALUE,
            move |s| {
                Some((
                    msg(bounded(&value_string(&s.value), COMPONENT_STRING_SIZE)),
                    true,
                ))
            },
        );
        match sub {
            Some(sub) => self.subs.push(sub),
            None => {
                if required {
                    println!("saveData: Can't monitor {name}\nsaveDataTask not initialized");
                } else {
                    println!("saveData: Can't monitor {name}");
                }
                return None;
            }
        }
        Some(ch)
    }

    /// `connectRealTime1D`.
    fn connect_real_time_1d(&mut self, name: &str) {
        let (ch, up) = self.ca.connect(name, 0.5);
        let Some(ch) = ch.filter(|_| up) else {
            println!("saveData: Unable to connect {name}");
            return;
        };
        match self.ca.monitor(&ch, DBR_LONG, 1, DBE_DEFAULT, |s| {
            Some((
                Msg::RealTime1D(s.value.to_f64().unwrap_or(0.0) as i32),
                true,
            ))
        }) {
            Some(sub) => self.subs.push(sub),
            None => println!("saveData: Can't monitor {name}"),
        }
    }

    /// `connectRetryPVs`.
    fn connect_retry_pvs(&mut self) {
        let prefix = self.ioc_prefix.clone();
        let names = [
            "saveData_currRetries",
            "saveData_maxAllowedRetries",
            "saveData_totalRetries",
            "saveData_retryWaitInSecs",
            "saveData_abandonedWrites",
        ];
        let chans: Vec<Option<Chan>> = names
            .iter()
            .map(|n| {
                self.ca
                    .search(&bounded(&format!("{prefix}{n}"), PVNAME_STRINGSZ))
            })
            .collect();
        let up = self
            .ca
            .pend(&chans.iter().map(Option::as_ref).collect::<Vec<_>>(), 0.5);
        if up.iter().any(|&u| !u) {
            println!("saveData: Can't connect to some or all retry PVs");
        }
        let [curr, max, total, wait, abandoned] =
            <[Option<Chan>; 5]>::try_from(chans).unwrap_or_else(|_| unreachable!("five names"));
        self.curr_retries_ch = curr;
        self.total_retries_ch = total;
        self.abandoned_writes_ch = abandoned;
        if let Some(ch) = max {
            let sub = self.ca.monitor(&ch, DBR_LONG, 1, DBE_DEFAULT, |s| {
                // maxAllowedRetriesMonitor
                let i = s.value.to_f64().unwrap_or(-1.0) as i64;
                if i >= 0 {
                    MAX_ALLOWED_RETRIES.store(i, Ordering::Relaxed);
                }
                println!(
                    "saveData:maxAllowedRetries = {}",
                    MAX_ALLOWED_RETRIES.load(Ordering::Relaxed)
                );
                None
            });
            match sub {
                Some(sub) => self.subs.push(sub),
                None => println!(
                    "saveData: Can't monitor {prefix}saveData_maxAllowedRetries.  Using default of {}",
                    MAX_ALLOWED_RETRIES.load(Ordering::Relaxed)
                ),
            }
        }
        if let Some(ch) = wait {
            let sub = self.ca.monitor(&ch, DBR_LONG, 1, DBE_DEFAULT, |s| {
                // retryWaitInSecsMonitor
                let i = s.value.to_f64().unwrap_or(-1.0) as i64;
                if i >= 0 {
                    RETRY_WAIT_IN_SECS.store(i, Ordering::Relaxed);
                }
                println!(
                    "saveData:retryWaitInSecs = {}",
                    RETRY_WAIT_IN_SECS.load(Ordering::Relaxed)
                );
                None
            });
            match sub {
                Some(sub) => self.subs.push(sub),
                None => println!(
                    "saveData: Can't monitor {prefix}saveData_retryWaitInSecs.  Using default of {}",
                    RETRY_WAIT_IN_SECS.load(Ordering::Relaxed)
                ),
            }
        }
    }

    /// `connectPV`.
    fn connect_pv(&mut self, pv: &str, desc: &str) {
        let (ch, up) = self.ca.connect(pv, 10.0);
        let Some(ch) = ch.filter(|_| up) else {
            println!("saveData: Unable to connect to {pv}");
            return;
        };
        let Some(dbr_type) = ch.native_type().map(extra_dbr_type) else {
            println!("saveData: {pv} has an unsupported type");
            return;
        };
        let count = if dbr_type == DBR_STRING {
            1
        } else {
            ch.element_count().unwrap_or(1) as usize
        };
        // `pval` zeroed at `count` elements until the first get lands.
        let value = match dbr_type {
            DBR_STRING => ExtraValue::String(String::new()),
            DBR_CTRL_CHAR => ExtraValue::Char {
                units: String::new(),
                value: vec![0; count],
            },
            DBR_CTRL_SHORT => ExtraValue::Short {
                units: String::new(),
                value: vec![0; count],
            },
            DBR_CTRL_LONG => ExtraValue::Long {
                units: String::new(),
                value: vec![0; count],
            },
            DBR_CTRL_FLOAT => ExtraValue::Float {
                units: String::new(),
                value: vec![0.0; count],
            },
            _ => ExtraValue::Double {
                units: String::new(),
                value: vec![0.0; count],
            },
        };
        let node = Arc::new(ExtraPv {
            name: bounded(pv, PVNAME_STRINGSZ),
            state: Mutex::new(ExtraPvState {
                desc: bounded(desc, MAX_STRING_SIZE),
                value,
            }),
        });
        let extra = ExtraChan {
            ch,
            dbr_type,
            pv: node.clone(),
        };
        // a first image of the value
        self.get_extra(&extra);

        if desc.is_empty() {
            // The req file gave no description: the record's DESC.
            let name = bounded(&format!("{}.DESC", record_part(pv)), PVNAME_STRINGSZ);
            let (dch, up) = self.ca.connect(&name, 10.0);
            match dch.filter(|_| up) {
                None => println!("saveData: Unable to connect to {name}"),
                Some(dch) => {
                    // extraDescCallback
                    let node = node.clone();
                    self.ca.ctx.spawn(async move {
                        if let Ok(s) = dch.get(DBR_STRING, 1).await {
                            node.state.lock().desc =
                                bounded(&value_string(&s.value), MAX_STRING_SIZE);
                        }
                    });
                }
            }
        }
        self.extra.push(node);
        self.extra_chans.push(extra);
    }

    /// `ca_array_get_callback(.., extraValCallback)` of one extra PV.
    fn get_extra(&self, extra: &ExtraChan) {
        let (ch, dbr_type, node) = (extra.ch.clone(), extra.dbr_type, extra.pv.clone());
        let count = ch.element_count().unwrap_or(1);
        self.ca.ctx.spawn(async move {
            if let Ok(s) = ch.get(dbr_type, count).await {
                node.state.lock().value = extra_value(dbr_type, &s);
            }
        });
    }

    /// `getExtraPV`.
    fn get_extra_pvs(&self) {
        for extra in &self.extra_chans {
            self.get_extra(extra);
        }
    }

    /// `connectScan`.
    fn connect_scan(&mut self, name: &str, hand_shake: &str, auto_hand_shake: &str) {
        debug!(1, "connectScan({name})...\n");
        let ca = &self.ca;
        let mut scan = Scan::new(&bounded(name, PVNAME_STRINGSZ));
        let mut c = ScanChans::new();

        c.hand_shake = ca.search(hand_shake);
        ca.pend(&[c.hand_shake.as_ref()], 1.0);
        c.auto_hand_shake = ca.search(auto_hand_shake);
        ca.pend(&[c.auto_hand_shake.as_ref()], 1.0);
        ca.put(c.auto_hand_shake.as_ref(), EpicsValue::Short(1));

        let field = |f: &str| ca.search(&format!("{}.{f}", scan.name));
        c.data = field("DATA");
        c.mpts = field("MPTS");
        c.npts = field("NPTS");
        c.cpt = field("CPT");
        c.bcpt = field("BCPT");
        let up = ca.pend(
            &[&c.data, &c.mpts, &c.npts, &c.cpt, &c.bcpt].map(Option::as_ref),
            5.0,
        );
        if up.iter().any(|&u| !u) {
            println!("saveData: Unable to connect to {}", scan.name);
            self.disconnect_scan(c);
            return;
        }

        for i in 0..SCAN_NBP {
            let n = i + 1;
            c.pxnv[i] = field(&format!("P{n}NV"));
            c.pxpv[i] = field(&format!("P{n}PV"));
            c.pxsm[i] = field(&format!("P{n}SM"));
            c.rxnv[i] = field(&format!("R{n}NV"));
            c.rxpv[i] = field(&format!("R{n}PV"));
            c.pxra[i] = field(&format!("P{n}RA"));
            c.rxcv[i] = field(&format!("R{n}CV"));
        }
        for i in 0..SCAN_NBD {
            let n = i + 1;
            c.dxnv[i] = field(&format!("D{n:02}NV"));
            c.dxpv[i] = field(&format!("D{n:02}PV"));
            c.dxda[i] = field(&format!("D{n:02}DA"));
            c.dxcv[i] = field(&format!("D{n:02}CV"));
        }
        for i in 0..SCAN_NBT {
            let n = i + 1;
            c.txnv[i] = field(&format!("T{n}NV"));
            c.txpv[i] = field(&format!("T{n}PV"));
            c.txcd[i] = field(&format!("T{n}CD"));
        }
        let mut all: Vec<Option<&Chan>> = Vec::new();
        for i in 0..SCAN_NBP {
            all.extend(
                [
                    &c.pxnv[i], &c.pxpv[i], &c.pxsm[i], &c.rxnv[i], &c.rxpv[i], &c.pxra[i],
                    &c.rxcv[i],
                ]
                .map(Option::as_ref),
            );
        }
        for i in 0..SCAN_NBD {
            all.extend([&c.dxnv[i], &c.dxpv[i], &c.dxda[i], &c.dxcv[i]].map(Option::as_ref));
        }
        for i in 0..SCAN_NBT {
            all.extend([&c.txnv[i], &c.txpv[i], &c.txcd[i]].map(Option::as_ref));
        }
        let up = ca.pend(&all, 5.0);
        let mut up = up.into_iter();
        let mut take = |n: usize| (&mut up).take(n).all(|u| u);
        let pos_up: Vec<bool> = (0..SCAN_NBP).map(|_| take(7)).collect();
        let det_up: Vec<bool> = (0..SCAN_NBD).map(|_| take(4)).collect();
        let trg_up: Vec<bool> = (0..SCAN_NBT).map(|_| take(3)).collect();
        let nc = pos_up.iter().filter(|&&u| !u).count();
        for (i, _) in pos_up.iter().enumerate().filter(|(_, u)| !**u) {
            for ch in [
                &mut c.pxnv[i],
                &mut c.pxpv[i],
                &mut c.pxsm[i],
                &mut c.rxnv[i],
                &mut c.rxpv[i],
                &mut c.pxra[i],
                &mut c.rxcv[i],
            ] {
                *ch = None;
            }
            scan.pos[i].pxpv.clear();
            scan.pos[i].pxsm.clear();
            scan.pos[i].rxpv.clear();
        }
        if nc > 0 {
            println!(
                "saveDataTask warning: {}: {nc} positioner(s) not connected",
                scan.name
            );
        }
        let nc = det_up.iter().filter(|&&u| !u).count();
        for (i, _) in det_up.iter().enumerate().filter(|(_, u)| !**u) {
            for ch in [
                &mut c.dxnv[i],
                &mut c.dxpv[i],
                &mut c.dxda[i],
                &mut c.dxcv[i],
            ] {
                *ch = None;
            }
            scan.det[i].dxpv.clear();
        }
        if nc > 0 {
            println!(
                "saveDataTask warning: {}: {nc} detector(s) not connected",
                scan.name
            );
        }
        let nc = trg_up.iter().filter(|&&u| !u).count();
        for (i, _) in trg_up.iter().enumerate().filter(|(_, u)| !**u) {
            for ch in [&mut c.txnv[i], &mut c.txpv[i], &mut c.txcd[i]] {
                *ch = None;
            }
            scan.trg[i].txpv.clear();
            scan.trg[i].txpv_rec.clear();
        }
        if nc > 0 {
            println!(
                "saveDataTask warning: {}: {nc} trigger(s) not connected",
                scan.name
            );
        }

        // MPTS sizes the buffers.
        let mpts = c.mpts.as_ref().and_then(|ch| ca.get(ch, DBR_LONG, 1, 5.0));
        let Some(mpts) = mpts else {
            print!(
                "saveData: {}: Unable to read MPTS. Aborting connection",
                scan.name
            );
            self.disconnect_scan(c);
            return;
        };
        scan.mpts = mpts.value.to_f64().unwrap_or(0.0) as i64;
        debug!(
            1,
            "connectScan({name}): allocating {} pts for positioners\n", scan.mpts
        );
        for i in 0..SCAN_NBP {
            if c.pxra[i].is_some() {
                scan.pos[i].pxra = vec![0.0; scan.mpts.max(0) as usize];
            }
        }
        // Detector arrays wait for a valid D<n>NV (proc_scan_dxnv).
        *c.cpt_time.lock() = Instant::now();
        self.scans.push(scan);
        self.chans.push(c);
        debug!(1, "connectScan({name}) OK\n");
    }

    /// `disconnectScan`: dropping the channels clears them.
    fn disconnect_scan(&self, c: ScanChans) {
        if c.auto_hand_shake.is_some() {
            self.ca
                .put(c.auto_hand_shake.as_ref(), EpicsValue::Short(0));
        }
    }

    /// `monitorScans`: every DATA first, so their events keep their order.
    fn monitor_scans(&mut self) {
        for i in 0..self.scans.len() {
            self.monitor_scan(i, 0);
        }
        for i in 0..self.scans.len() {
            self.monitor_scan(i, 1);
        }
        self.update_scans();
    }

    /// `monitorScan`.
    fn monitor_scan(&mut self, scan: usize, pass: u8) {
        let ca = &self.ca;
        let name = self.scans[scan].name.clone();
        debug!(1, "monitorScan({name})...\n");
        let c = &mut self.chans[scan];
        let mut sub = |ch: &Option<Chan>,
                       t: u16,
                       f: Box<dyn FnMut(Snapshot) -> Option<(Msg, bool)> + Send>|
         -> bool {
            let Some(ch) = ch else { return false };
            match ca.monitor(ch, t, 1, DBE_DEFAULT, f) {
                Some(s) => {
                    c.subs.push(s);
                    true
                }
                None => {
                    println!("Unable to monitor {name}");
                    false
                }
            }
        };
        let short = |s: &Snapshot| s.value.to_f64().unwrap_or(0.0) as i16;
        if pass == 0 {
            sub(
                &c.data.clone(),
                DBR_TIME_SHORT,
                Box::new(move |s| {
                    Some((
                        Msg::Data {
                            scan,
                            stamp: s.timestamp,
                            val: short(&s),
                        },
                        true,
                    ))
                }),
            );
            return;
        }
        if c.npts.is_none() {
            println!("Unable to monitor {name} npts field");
            return;
        }
        if !sub(
            &c.npts.clone(),
            DBR_LONG,
            Box::new(move |s| {
                Some((
                    Msg::Npts {
                        scan,
                        val: s.value.to_f64().unwrap_or(0.0) as i32,
                    },
                    true,
                ))
            }),
        ) {
            return;
        }
        for i in 0..SCAN_NBP {
            let (nv, sm, rnv) = (c.pxnv[i].clone(), c.pxsm[i].clone(), c.rxnv[i].clone());
            if nv.is_none() || sm.is_none() || rnv.is_none() {
                debug!(2, "Unable to monitor {name} positioner {i}\n");
                return;
            }
            let ok = sub(
                &nv,
                DBR_SHORT,
                Box::new(move |s| {
                    Some((
                        Msg::Pxnv {
                            scan,
                            i,
                            val: short(&s),
                        },
                        true,
                    ))
                }),
            ) && sub(
                &sm,
                DBR_STRING,
                Box::new(move |s| {
                    Some((
                        Msg::Text {
                            dest: Dest::Pxsm(scan, i),
                            val: value_string(&s.value),
                        },
                        true,
                    ))
                }),
            ) && sub(
                &rnv,
                DBR_SHORT,
                Box::new(move |s| {
                    Some((
                        Msg::Rxnv {
                            scan,
                            i,
                            val: short(&s),
                        },
                        true,
                    ))
                }),
            );
            if !ok {
                return;
            }
        }
        for i in 0..SCAN_NBD {
            let nv = c.dxnv[i].clone();
            if nv.is_none() {
                debug!(2, "Unable to monitor {name} detector {i}\n");
                return;
            }
            if !sub(
                &nv,
                DBR_SHORT,
                Box::new(move |s| {
                    Some((
                        Msg::Dxnv {
                            scan,
                            i,
                            val: short(&s),
                        },
                        true,
                    ))
                }),
            ) {
                return;
            }
        }
        for i in 0..SCAN_NBT {
            let (nv, cd) = (c.txnv[i].clone(), c.txcd[i].clone());
            if nv.is_none() || cd.is_none() {
                debug!(2, "Unable to monitor {name} trigger {i}\n");
                return;
            }
            let ok = sub(
                &nv,
                DBR_SHORT,
                Box::new(move |s| {
                    Some((
                        Msg::Txnv {
                            scan,
                            i,
                            val: short(&s),
                        },
                        true,
                    ))
                }),
            ) && sub(
                &cd,
                DBR_FLOAT,
                Box::new(move |s| {
                    Some((
                        Msg::Txcd {
                            scan,
                            i,
                            val: s.value.to_f64().unwrap_or(0.0) as f32,
                        },
                        true,
                    ))
                }),
            );
            if !ok {
                return;
            }
        }
        debug!(1, "monitorScan({name}) OK\n");
    }

    /// `updateScan`: find the inner scan this one's triggers start, and
    /// keep `.CPT` monitored while there is one or data is written point
    /// by point.
    fn update_scan(&mut self, scan: usize) {
        debug!(2, "updateScan:entry for '{}'\n", self.scans[scan].name);
        let mut nxt = None;
        for t in &self.scans[scan].trg {
            if t.txsc == 0 && t.txcd != 0.0 {
                debug!(
                    2,
                    "updateScan:{}: calling searchScan({})\n", self.scans[scan].name, t.txpv_rec
                );
                nxt = self.search_scan(&t.txpv_rec);
                if nxt.is_some() {
                    break;
                }
            }
        }
        self.scans[scan].nxt = nxt;
        if nxt.is_none() && self.real_time_1d == 0 {
            if self.chans[scan].cpt_sub.take().is_some() {
                debug!(
                    2,
                    "updateScan:{}: clear .CPT subscription\n", self.scans[scan].name
                );
            }
        } else if self.chans[scan].cpt_sub.is_none() {
            debug!(
                2,
                "updateScan:{}: subscribe to .CPT\n", self.scans[scan].name
            );
            let cpt_time = self.chans[scan].cpt_time.clone();
            let sub = self.chans[scan].cpt.clone().and_then(|ch| {
                self.ca.monitor(&ch, DBR_LONG, 1, DBE_VALUE, move |s| {
                    // cptMonitor
                    debug!(2, "cptMonitor:entry\n");
                    let msg = Msg::Cpt {
                        scan,
                        val: s.value.to_f64().unwrap_or(0.0) as i32,
                    };
                    let policy = MESSAGE_POLICY.load(Ordering::Relaxed);
                    if policy == 2 || policy == 3 {
                        let now = Instant::now();
                        let mut last = cpt_time.lock();
                        if now.duration_since(*last).as_secs_f64() < cpt_wait_time() {
                            return None;
                        }
                        *last = now;
                    }
                    match policy {
                        0 | 3 => Some((msg, true)),
                        1 | 2 => Some((msg, false)),
                        _ => None,
                    }
                })
            });
            match sub {
                Some(sub) => self.chans[scan].cpt_sub = Some(sub),
                None => debug!(2, "Unable to monitor {}.CPT\n", self.scans[scan].name),
            }
        }
        debug!(
            2,
            "updateScan:exit cpt_monitored = {}\n",
            i32::from(self.chans[scan].cpt_sub.is_some())
        );
    }

    fn update_scans(&mut self) {
        for i in 0..self.scans.len() {
            self.update_scan(i);
        }
    }

    fn dispatch(&mut self, msg: Msg) {
        match msg {
            Msg::Data { scan, stamp, val } => {
                debug!(2, "saveDataTask: MSG_SCAN_DATA, val={val}\n");
                self.data_monitor(scan, val);
                self.proc_scan_data(scan, stamp, val);
            }
            Msg::Npts { scan, val } => {
                debug!(2, "saveDataTask: MSG_SCAN_NPTS, val={val}\n");
                self.scans[scan].npts = i64::from(val);
                debug_msg!(
                    2,
                    "{} MSG_SCAN_NPTS({})\n",
                    self.scans[scan].name,
                    self.scans[scan].npts
                );
            }
            Msg::Cpt { scan, val } => {
                debug!(2, "saveDataTask: MSG_SCAN_CPT, val={val}\n");
                self.proc_scan_cpt(scan, i64::from(val));
            }
            Msg::Pxnv { scan, i, val } => self.proc_scan_pxnv(scan, i, val),
            Msg::Rxnv { scan, i, val } => self.proc_scan_rxnv(scan, i, val),
            Msg::Dxnv { scan, i, val } => self.proc_scan_dxnv(scan, i, val),
            Msg::Txnv { scan, i, val } => self.proc_scan_txnv(scan, i, val),
            Msg::Txcd { scan, i, val } => {
                self.scans[scan].trg[i].txcd = val;
                self.update_scan(scan);
            }
            Msg::Text { dest, val } => {
                // proc_desc / proc_scan_pxsm
                let val = bounded(&val, MAX_STRING_SIZE);
                match dest {
                    Dest::Pxds(s, i) => self.scans[s].pos[i].pxds = val,
                    Dest::Rxds(s, i) => self.scans[s].pos[i].rxds = val,
                    Dest::Dxds(s, i) => self.scans[s].det[i].dxds = val,
                    Dest::Pxsm(s, i) => self.scans[s].pos[i].pxsm = val,
                }
            }
            Msg::FileSystem(fs) => {
                let fs = bounded(&fs, MAX_STRING_SIZE);
                debug_msg!(2, "MSG_FILE_SYSTEM({fs})\n");
                self.remount_file_system(&fs);
            }
            Msg::FileSubdir(sd) => self.proc_file_subdir(&sd),
            Msg::FileBasename(bn) => {
                self.basename = bounded(&bn, COMPONENT_STRING_SIZE);
                debug_msg!(2, "MSG_FILE_BASENAME({bn})\n");
            }
            Msg::RealTime1D(val) => {
                if self.real_time_1d != val {
                    self.real_time_1d = val;
                    self.update_scans();
                }
            }
            Msg::Info(name, reply) => {
                let text = match name {
                    Some(name) => self
                        .search_scan(&name)
                        .map(|i| self.info_scan(i))
                        .unwrap_or_default(),
                    None => self.info(),
                };
                let _ = reply.send(text);
            }
            Msg::Priority(p) => {
                apply_to_current_thread(ThreadPriority::Custom(p));
            }
        }
    }

    /// `dataMonitor`'s bookkeeping, done as the task takes the event.
    fn data_monitor(&mut self, scan: usize, val: i16) {
        if let Some(n) = self.scans[scan].nxt {
            self.scans[n].first_scan = false;
            self.scans[n].scan_dim = self.scans[scan].scan_dim - 1;
        }
        debug!(5, "dataMonitor({}): (DATA={val})\n", self.scans[scan].name);
        if self.scans[scan].data != -1 {
            if val == 1 {
                self.ca.put(
                    self.chans[scan].hand_shake.as_ref(),
                    EpicsValue::Short(HANDSHAKE_BUSY),
                );
            }
            if val == 0 {
                let was = self.nb_scan_running;
                self.nb_scan_running += 1;
                if was == 0 {
                    // C's file_system/subdir/basename `.DISP` puts are
                    // commented out there, so only the message clears.
                    self.send_user_message("");
                }
            }
            debug!(2, "\n nb_scan_running={}\n", self.nb_scan_running);
        }
    }

    /// The retry loop around one file write.
    fn with_retries(&mut self, scan: usize, mut write: impl FnMut(&mut Self, bool) -> bool) {
        self.scans[scan].saved_seek_pos = 0;
        self.curr_retries = 0;
        self.put_long(self.curr_retries_ch.as_ref(), self.curr_retries);
        loop {
            if write(self, self.curr_retries != 0) {
                return;
            }
            self.curr_retries += 1;
            if self.curr_retries <= MAX_ALLOWED_RETRIES.load(Ordering::Relaxed) {
                let wait = RETRY_WAIT_IN_SECS.load(Ordering::Relaxed);
                println!("saveData: ...will retry in {wait} seconds");
                self.total_retries += 1;
                self.put_long(self.total_retries_ch.as_ref(), self.total_retries);
                self.put_long(self.curr_retries_ch.as_ref(), self.curr_retries);
                std::thread::sleep(Duration::from_secs(wait.max(0) as u64));
            } else {
                println!("saveData: *******************************************");
                println!(
                    "saveData: too many retries; abandoning data from scan '{}'",
                    self.scans[scan].name
                );
                println!("saveData: *******************************************");
                self.abandoned_writes += 1;
                self.put_long(self.abandoned_writes_ch.as_ref(), self.abandoned_writes);
                return;
            }
        }
    }

    /// `proc_scan_data`.
    fn proc_scan_data(&mut self, scan: usize, stamp: WallTime, val: i16) {
        debug!(
            5,
            "proc_scan_data({}):entry:pmsg->val={val}\n", self.scans[scan].name
        );
        if self.scans[scan].data == -1 {
            // only once, at init
            self.scans[scan].data = val;
            return;
        }
        if self.save_status == STATUS_INACTIVE || self.save_status == STATUS_ACTIVE_FS_ERROR {
            if val == 0 {
                self.send_user_message("Scan not being saved !!!!!");
            } else {
                self.nb_scan_running -= 1;
                debug!(
                    2,
                    "(save_status inactive) nb_scan_running={}\n", self.nb_scan_running
                );
                if self.nb_scan_running < 0 {
                    println!(
                        "proc_scan_data({}): nb_scan_running was {}.  Set to zero.",
                        self.scans[scan].name, self.nb_scan_running
                    );
                    self.nb_scan_running = 0;
                }
            }
            self.scans[scan].data = val;
            return;
        }

        if val == 0 {
            self.scan_started(scan, stamp);
        } else if self.scans[scan].data == 0 && val == 1 {
            self.scan_ended(scan);
        }
        debug!(
            5,
            "proc_scan_data({}): exit({val})\n", self.scans[scan].name
        );
    }

    fn scan_started(&mut self, scan: usize, stamp: WallTime) {
        debug!(2, "scan started: {}\n", self.scans[scan].name);
        let ca = &self.ca;
        let c = &self.chans[scan];
        let s = &mut self.scans[scan];
        s.data = 0;

        // Count what the scan saves and get its units.
        let mut reqs: Vec<(&Chan, u16, u32)> = Vec::new();
        let mut slots = Vec::new();
        s.nb_pos = 0;
        for i in 0..SCAN_NBP {
            if s.pos[i].valid() {
                s.nb_pos += 1;
                if let Some(ch) = &c.pxeu[i] {
                    reqs.push((ch, DBR_CTRL_DOUBLE, 1));
                    slots.push((0, i));
                }
                if let Some(ch) = &c.rxeu[i] {
                    reqs.push((ch, DBR_CTRL_DOUBLE, 1));
                    slots.push((1, i));
                }
            }
        }
        s.nb_det = 0;
        for i in 0..SCAN_NBD {
            if s.det[i].dxnv == XXNV_OK {
                s.nb_det += 1;
                if let Some(ch) = &c.dxeu[i] {
                    reqs.push((ch, DBR_CTRL_FLOAT, 1));
                    slots.push((2, i));
                }
            }
        }
        s.nb_trg = s.trg.iter().filter(|t| t.txnv == XXNV_OK).count() as i32;
        s.cpt = 0;
        match ca.gets(&reqs, 2.0) {
            None => println!("saveData: Unable to get all pos/rdb/det units"),
            Some(got) => {
                for ((kind, i), snap) in slots.into_iter().zip(got) {
                    let Some(snap) = snap else { continue };
                    let u = units(&snap);
                    match kind {
                        0 => s.pos[i].pxeu = u,
                        1 => s.pos[i].rxeu = u,
                        _ => s.det[i].dxeu = u,
                    }
                }
            }
        }

        if self.scans[scan].first_scan {
            debug!(3, "Outermost scan\n");
            match self.counter_ch.as_ref() {
                None => println!("saveData: unable to get scan number !!!"),
                Some(ch) => match self.ca.get(ch, DBR_LONG, 1, 0.5) {
                    None => println!("saveData: unable to get scan number !!!"),
                    Some(v) => self.scans[scan].counter = v.value.to_f64().unwrap_or(0.0) as i64,
                },
            }
            if self.basename.is_empty() {
                self.basename = bounded(&self.ioc_prefix, COMPONENT_STRING_SIZE);
            }
            let counter = self.scans[scan].counter;
            let base = &self.basename;
            let mut fname = bounded(&format!("{base}{counter:04}.mda"), FNAMELEN);
            let mut ffname = bounded(&format!("{}{fname}", self.server_pathname), FNAMELEN);
            // An existing file gets '_nn' inserted.
            let mut duplicate = 0;
            while file_status(&ffname).is_ok() && duplicate < 99 {
                duplicate += 1;
                ffname = bounded(
                    &format!(
                        "{}{base}{counter:04}_{duplicate:02}.mda",
                        self.server_pathname
                    ),
                    FNAMELEN,
                );
            }
            if duplicate != 0 {
                fname = bounded(&format!("{base}{counter:04}_{duplicate:02}.mda"), FNAMELEN);
            }
            let mut msg = format!("Writing to {fname}");
            truncate_message(&mut msg);
            self.send_user_message(&msg);
            self.ca.put(
                self.filename_ch.as_ref(),
                EpicsValue::String(PvString::from(fname.as_str())),
            );
            self.put_long(self.counter_ch.as_ref(), counter + 1);
            let s = &mut self.scans[scan];
            s.fname = fname;
            s.ffname = ffname;

            // scan_getDim and reset_old_npts
            let mut dim = 0;
            let mut cur = Some(scan);
            while let Some(c) = cur {
                dim += 1;
                self.scans[c].old_npts = -1;
                cur = self.scans[c].nxt;
            }
            self.scans[scan].scan_dim = dim;
            self.get_extra_pvs();
        }

        if let Some(n) = self.scans[scan].nxt {
            let (dim, fname, ffname) = (
                self.scans[scan].scan_dim,
                self.scans[scan].fname.clone(),
                self.scans[scan].ffname.clone(),
            );
            let inner = &mut self.scans[n];
            inner.first_scan = false;
            inner.scan_dim = dim - 1;
            inner.fname = fname;
            inner.ffname = ffname;
        }

        let stamp = epics_time_to_strftime(STAMP_FORMAT, stamp);
        self.with_retries(scan, |t, retry| {
            let (scans, _, mut io) = t.io();
            mda::write_scan_rec_in_progress(scans, scan, &stamp, retry, &mut io)
        });
    }

    fn scan_ended(&mut self, scan: usize) {
        self.scans[scan].data = 1;
        // the buffered cpt that goes with the arrays
        if let Some(ch) = self.chans[scan].bcpt.clone()
            && let Some(v) = self.ca.get(&ch, DBR_LONG, 1, 1.0)
        {
            self.scans[scan].bcpt = v.value.to_f64().unwrap_or(0.0) as i64;
        }
        self.with_retries(scan, |t, retry| {
            let (scans, chans, mut io) = t.io();
            let ca = io.ca;
            let c = &chans[scan];
            mda::write_scan_rec_completed(&mut scans[scan], retry, &mut io, |s| {
                fetch_arrays(ca, c, s)
            })
        });
        if self.scans[scan].first_scan {
            let mut cur = self.scans[scan].nxt;
            while let Some(c) = cur {
                self.scans[c].first_scan = true;
                cur = self.scans[c].nxt;
            }
        }
        // hand shaking notify
        if self.chans[scan].hand_shake.is_some() {
            debug!(
                5,
                "proc_scan_data({}): done writing file {}; putting {HANDSHAKE_DONE} to .AWAIT\n",
                self.scans[scan].name,
                self.scans[scan].fname
            );
            self.ca.put(
                self.chans[scan].hand_shake.as_ref(),
                EpicsValue::Short(HANDSHAKE_DONE),
            );
        }
        self.nb_scan_running -= 1;
        debug!(
            2,
            "(save_status active) nb_scan_running={}\n", self.nb_scan_running
        );
    }

    /// `proc_scan_cpt`.
    fn proc_scan_cpt(&mut self, scan: usize, val: i64) {
        if self.save_status == STATUS_INACTIVE || self.save_status == STATUS_ACTIVE_FS_ERROR {
            return;
        }
        self.scans[scan].cpt = val;
        let s = &self.scans[scan];
        if s.data != 0 || s.cpt == 0 {
            return;
        }
        debug!(
            2,
            "saveData:proc_scan_cpt: saving {}[{}] to {}\n",
            s.name,
            s.cpt - 1,
            s.fname
        );
        let c = &self.chans[scan];
        let mut reqs: Vec<(&Chan, u16, u32)> = Vec::new();
        let mut slots = Vec::new();
        for i in 0..SCAN_NBP {
            if let (true, Some(ch)) = (s.pos[i].valid(), &c.rxcv[i]) {
                reqs.push((ch, DBR_DOUBLE, 1));
                slots.push((true, i));
            }
        }
        for i in 0..SCAN_NBD {
            if let (true, Some(ch)) = (s.det[i].dxnv == XXNV_OK, &c.dxcv[i]) {
                reqs.push((ch, DBR_FLOAT, 1));
                slots.push((false, i));
            }
        }
        let Some(got) = self.ca.gets(&reqs, 0.5) else {
            println!("saveData:proc_scan_cpt: unable to get current detector values !!!");
            return;
        };
        let s = &mut self.scans[scan];
        for ((pos, i), snap) in slots.into_iter().zip(got) {
            let Some(v) = snap.and_then(|snap| snap.value.to_f64()) else {
                continue;
            };
            if pos {
                s.pos[i].rxcv = v;
            } else {
                s.det[i].dxcv = v as f32;
            }
        }
        let opened = Instant::now();
        let (scans, _, mut io) = self.io();
        mda::write_cpt(&scans[scan], &mut io);
        debug!(
            1,
            "saveData:proc_scan_cpt:{} data point written ({:.3}s)\n",
            self.scans[scan].name,
            opened.elapsed().as_secs_f32()
        );
    }

    /// `ca_search` + `ca_pend_io(2.0)` of `<record>.DESC`, monitored into
    /// `dest`.
    fn watch_desc(&self, pv: &str, dest: Dest) -> Option<Watched> {
        let name = bounded(&format!("{}.DESC", record_part(pv)), PVNAME_STRINGSZ);
        let (ch, up) = self.ca.connect(&name, 2.0);
        let Some(ch) = ch.filter(|_| up) else {
            debug!(2, "Unable to connect {name}\n");
            return None;
        };
        let sub = self.ca.monitor(&ch, DBR_STRING, 1, DBE_DEFAULT, move |s| {
            Some((
                Msg::Text {
                    dest,
                    val: value_string(&s.value),
                },
                true,
            ))
        });
        Some(Watched { _ch: ch, _sub: sub })
    }

    /// `ca_search` + `ca_pend_io(2.0)` of the PV itself, and a
    /// `DBR_CTRL_*` get for its units.
    fn connect_eu(&self, pv: &str, dbr_type: u16) -> (Option<Chan>, Option<String>) {
        let (ch, up) = self.ca.connect(pv, 2.0);
        let Some(ch) = ch.filter(|_| up) else {
            debug!(2, "Unable to connect {pv}\n");
            return (None, None);
        };
        let eu = self.ca.get(&ch, dbr_type, 1, 2.0).map(|s| units(&s));
        (Some(ch), eu)
    }

    /// A `.xxPV` name: the string, or `ERROR` when it cannot be read.
    fn get_name(&self, ch: &Option<Chan>, secs: f64) -> Option<String> {
        let ch = ch.as_ref()?;
        self.ca
            .get(ch, DBR_STRING, 1, secs)
            .map(|s| bounded(&value_string(&s.value), PVNAME_STRINGSZ))
    }

    /// `proc_scan_pxnv`.
    fn proc_scan_pxnv(&mut self, scan: usize, i: usize, val: i16) {
        let p = &mut self.scans[scan].pos[i];
        p.pxnv = val;
        p.pxpv.clear();
        p.pxds.clear();
        p.pxeu.clear();
        self.chans[scan].pxds[i] = None;
        self.chans[scan].pxeu[i] = None;
        if val == XXNV_OK {
            match self.get_name(&self.chans[scan].pxpv[i], 2.0) {
                None => {
                    debug!(2, "Unable to get {}.P{}PV\n", self.scans[scan].name, i + 1);
                    self.scans[scan].pos[i].pxpv = "ERROR".into();
                }
                Some(pv) => {
                    self.chans[scan].pxds[i] = self.watch_desc(&pv, Dest::Pxds(scan, i));
                    let (ch, eu) = self.connect_eu(&pv, DBR_CTRL_DOUBLE);
                    self.chans[scan].pxeu[i] = ch;
                    let p = &mut self.scans[scan].pos[i];
                    p.pxpv = pv;
                    if let Some(eu) = eu {
                        p.pxeu = eu;
                    }
                }
            }
        }
        debug_msg!(2, "{} MSG_SCAN_PXNV({val})\n", self.scans[scan].name);
    }

    /// `proc_scan_rxnv`: also takes the pseudo names `time` / `TIME`.
    fn proc_scan_rxnv(&mut self, scan: usize, i: usize, val: i16) {
        let p = &mut self.scans[scan].pos[i];
        p.rxnv = val;
        p.rxpv.clear();
        p.rxds.clear();
        p.rxeu.clear();
        self.chans[scan].rxds[i] = None;
        self.chans[scan].rxeu[i] = None;
        match self.get_name(&self.chans[scan].rxpv[i], 0.5) {
            None => {
                debug!(2, "Unable to get {}.R{}PV\n", self.scans[scan].name, i + 1);
                self.scans[scan].pos[i].rxpv = "ERROR".into();
            }
            Some(pv) if val == XXNV_OK => {
                self.chans[scan].rxds[i] = self.watch_desc(&pv, Dest::Rxds(scan, i));
                let (ch, eu) = self.connect_eu(&pv, DBR_CTRL_DOUBLE);
                self.chans[scan].rxeu[i] = ch;
                let p = &mut self.scans[scan].pos[i];
                p.rxpv = pv;
                if let Some(eu) = eu {
                    p.rxeu = eu;
                }
            }
            Some(pv) => {
                let p = &mut self.scans[scan].pos[i];
                if pv == "time" || pv == "TIME" {
                    p.rxnv = XXNV_OK;
                    p.rxeu = "second".into();
                    p.rxpv = pv;
                }
            }
        }
        debug_msg!(2, "{} MSG_SCAN_RXNV({val})\n", self.scans[scan].name);
    }

    /// `proc_scan_dxnv`.
    fn proc_scan_dxnv(&mut self, scan: usize, i: usize, val: i16) {
        let mpts = self.scans[scan].mpts.max(0) as usize;
        let d = &mut self.scans[scan].det[i];
        d.dxnv = val;
        d.dxpv.clear();
        d.dxds.clear();
        d.dxeu.clear();
        self.chans[scan].dxds[i] = None;
        self.chans[scan].dxeu[i] = None;
        if val == XXNV_OK {
            if d.dxda.is_none() {
                d.dxda = Some(vec![0.0; mpts]);
            }
            match self.get_name(&self.chans[scan].dxpv[i], 1.0) {
                None => {
                    debug!(
                        2,
                        "Unable to get {}.D{:02}PV\n",
                        self.scans[scan].name,
                        i + 1
                    );
                    self.scans[scan].det[i].dxpv = "ERROR".into();
                }
                Some(pv) => {
                    self.chans[scan].dxds[i] = self.watch_desc(&pv, Dest::Dxds(scan, i));
                    let (ch, eu) = self.connect_eu(&pv, DBR_CTRL_FLOAT);
                    self.chans[scan].dxeu[i] = ch;
                    let d = &mut self.scans[scan].det[i];
                    d.dxpv = pv;
                    if let Some(eu) = eu {
                        d.dxeu = eu;
                    }
                }
            }
        }
        debug_msg!(2, "{} MSG_SCAN_DXNV({val})\n", self.scans[scan].name);
    }

    /// `proc_scan_txnv`.
    fn proc_scan_txnv(&mut self, scan: usize, i: usize, val: i16) {
        let t = &mut self.scans[scan].trg[i];
        // presume NOT linked to another sscan record
        t.txsc = 1;
        t.txnv = val;
        t.txpv.clear();
        t.txpv_rec.clear();
        if val == XXNV_OK {
            match self.get_name(&self.chans[scan].txpv[i], 2.0) {
                None => debug!(2, "Unable to get {}.T{}PV\n", self.scans[scan].name, i + 1),
                Some(pv) => {
                    let t = &mut self.scans[scan].trg[i];
                    let rec = record_part(&pv).to_string();
                    t.txsc = i16::from(&pv[rec.len()..] != ".EXSC");
                    t.txpv_rec = rec;
                    t.txpv = pv;
                }
            }
        }
        self.update_scan(scan);
        debug_msg!(2, "{} MSG_SCAN_TXNV({val})\n", self.scans[scan].name);
    }

    /// `remount_file_system` (the non-vxWorks branch: nothing to mount).
    fn remount_file_system(&mut self, filesystem: &str) {
        self.mounted = false;
        self.save_status = STATUS_ACTIVE_FS_ERROR;
        // reset the subdirectory to ""
        if !self.local_subdir.is_empty() {
            self.local_subdir.clear();
            self.ca.put(
                self.file_subdir_ch.as_ref(),
                EpicsValue::String(PvString::from("")),
            );
        }
        self.mounted = true;
        self.server_pathname = bounded(&format!("{filesystem}/"), COMPONENT_STRING_SIZE);
        self.server_subdir = self.server_pathname.len();
        let msg = if !check_rw_permission(&self.server_pathname) {
            "RW permission denied !!!"
        } else {
            self.save_status = STATUS_ACTIVE_OK;
            "saveData OK"
        };
        self.put_full_pathname();
        self.send_user_message(msg);
        self.put_status();
    }

    /// `proc_file_subdir`.
    fn proc_file_subdir(&mut self, subdir: &str) {
        if self.mounted {
            // the new directory differs from the previous one
            if subdir == self.local_subdir {
                return;
            }
            // failure until a file can be created
            self.save_status = STATUS_ACTIVE_FS_ERROR;
            self.server_pathname.truncate(self.server_subdir);
            self.local_subdir.clear();
            for part in subdir.split('/').filter(|p| !p.is_empty()) {
                let part = part.replace(' ', "_");
                self.server_pathname.push_str(&part);
                self.local_subdir.push_str(&part);
                let _ = mkdir_0775(&self.server_pathname);
                self.server_pathname.push('/');
                self.local_subdir.push('/');
            }
            let path = self.server_pathname.clone();
            let msg = if file_status(&path).is_err() {
                self.server_pathname.truncate(self.server_subdir);
                self.local_subdir.clear();
                "Invalid directory !!!"
            } else if !check_rw_permission(&path) {
                self.server_pathname.truncate(self.server_subdir);
                self.local_subdir.clear();
                "RW permission denied !!!"
            } else {
                self.save_status = STATUS_ACTIVE_OK;
                "saveData OK"
            };
            self.put_full_pathname();
            self.send_user_message(msg);
            self.put_status();
        }
        debug_msg!(2, "MSG_FILE_SUBDIR({subdir})\n");
    }

    /// `saveData_Info`.
    fn info(&self) -> String {
        let mut out = String::from("saveData: scan info:\n");
        for (i, s) in self.scans.iter().enumerate() {
            let mut chain = Vec::new();
            let mut cur = Some(i);
            while let Some(c) = cur {
                chain.push(self.scans[c].name.as_str());
                cur = self.scans[c].nxt;
            }
            out += &format!("scan   : {}\n", s.name);
            out += &format!("  rank : {}\n", chain.len());
            out += &format!("  links:{}\n", chain.join("->"));
        }
        out
    }

    /// `infoScan`.
    fn info_scan(&self, i: usize) -> String {
        let s = &self.scans[i];
        let c = &self.chans[i];
        let n = &s.name;
        let mut o = format!("scan name: {n}\n");
        let mut line = |l: String| {
            o.push_str(&l);
            o.push('\n');
        };
        line(match &c.hand_shake {
            Some(_) => format!("hand shake: {n}.AWAIT"),
            None => " No hand shake".into(),
        });
        line(match &c.auto_hand_shake {
            Some(_) => format!("auto hand shake: {n}.AAWAIT"),
            None => " No auto hand shake".into(),
        });
        line(format!("{n}.DATA[{}]= {}", cs(&c.data), s.data));
        line(format!("{n}.MPTS[{}]= {}", cs(&c.mpts), s.mpts));
        line(format!("{n}.NPTS[{}]= {}", cs(&c.npts), s.npts));
        line(format!("{n}.CPT [{}]= {}", cs(&c.cpt), s.cpt));
        line(format!("{n}.BCPT [{}]= {}", cs(&c.bcpt), s.bcpt));
        for k in 0..SCAN_NBP {
            if c.pxnv[k].is_some() {
                line(format!(
                    "{n}.P{}NV[{}]= {}",
                    k + 1,
                    cs(&c.pxnv[k]),
                    s.pos[k].pxnv
                ));
            }
        }
        for k in 0..SCAN_NBP {
            if c.pxpv[k].is_some() {
                line(format!(
                    "{n}.P{}PV[{}]= {}",
                    k + 1,
                    cs(&c.pxpv[k]),
                    s.pos[k].pxpv
                ));
                line(format!("  DESC: {}", s.pos[k].pxds));
                line(format!("  EGU : {}", s.pos[k].pxeu));
            }
        }
        for k in 0..SCAN_NBP {
            if c.pxsm[k].is_some() {
                line(format!(
                    "{n}.P{}SM[{}]= {}",
                    k + 1,
                    cs(&c.pxsm[k]),
                    s.pos[k].pxsm
                ));
            }
        }
        for k in 0..SCAN_NBP {
            if c.rxnv[k].is_some() {
                line(format!(
                    "{n}.R{}NV[{}]= {}",
                    k + 1,
                    cs(&c.rxnv[k]),
                    s.pos[k].rxnv
                ));
            }
        }
        for k in 0..SCAN_NBP {
            if c.rxpv[k].is_some() {
                line(format!(
                    "{n}.R{}PV[{}]= {}",
                    k + 1,
                    cs(&c.rxpv[k]),
                    s.pos[k].rxpv
                ));
                line(format!("  DESC: {}", s.pos[k].rxds));
                line(format!("  EGU : {}", s.pos[k].rxeu));
            }
        }
        for k in 0..SCAN_NBP {
            if c.pxra[k].is_some() {
                let p = &s.pos[k].pxra;
                let addr = if p.is_empty() {
                    "(nil)".to_string()
                } else {
                    format!("{:p}", p.as_ptr())
                };
                line(format!("{n}.P{}RA[{}]= {addr}", k + 1, cs(&c.pxra[k])));
            }
        }
        for k in 0..SCAN_NBP {
            if c.rxcv[k].is_some() {
                line(format!(
                    "{n}.R{}CV[{}]= {:.6}",
                    k + 1,
                    cs(&c.rxcv[k]),
                    s.pos[k].rxcv
                ));
            }
        }
        for k in 0..SCAN_NBD {
            if c.dxnv[k].is_some() {
                line(format!(
                    "{n}.D{:02}NV[{}]= {}",
                    k + 1,
                    cs(&c.dxnv[k]),
                    s.det[k].dxnv
                ));
            }
        }
        for k in 0..SCAN_NBD {
            if c.dxpv[k].is_some() {
                line(format!(
                    "{n}.D{:02}PV[{}]= {}",
                    k + 1,
                    cs(&c.dxpv[k]),
                    s.det[k].dxpv
                ));
                line(format!("  DESC: {}", s.det[k].dxds));
                line(format!("  EGU : {}", s.det[k].dxeu));
            }
        }
        for k in 0..SCAN_NBD {
            if c.dxda[k].is_some() {
                let a = if s.det[k].dxda.is_some() {
                    "allocated"
                } else {
                    "not allocated"
                };
                line(format!("{n}.D{:02}DA[{}] ({a})", k + 1, cs(&c.dxda[k])));
            }
        }
        for k in 0..SCAN_NBD {
            if c.dxcv[k].is_some() {
                line(format!(
                    "{n}.D{:02}CV[{}]= {:.6}",
                    k + 1,
                    cs(&c.dxcv[k]),
                    s.det[k].dxcv
                ));
            }
        }
        for k in 0..SCAN_NBT {
            if c.txnv[k].is_some() {
                line(format!(
                    "{n}.T{}NV[{}]= {}",
                    k + 1,
                    cs(&c.txnv[k]),
                    s.trg[k].txnv
                ));
            }
        }
        for k in 0..SCAN_NBT {
            if c.txpv[k].is_some() {
                line(format!(
                    "{n}.T{}PV[{}]= {}",
                    k + 1,
                    cs(&c.txpv[k]),
                    s.trg[k].txpv
                ));
            }
        }
        for k in 0..SCAN_NBT {
            if c.txcd[k].is_some() {
                line(format!(
                    "{n}.T{}CD[{}]= {:.6}",
                    k + 1,
                    cs(&c.txcd[k]),
                    s.trg[k].txcd
                ));
            }
        }
        o
    }
}

/// `cs[ca_state(chid)]`. The channel trait reports no live state: a channel
/// that never connected is "not found", one with read access is
/// "still available", any other "unavailable".
fn cs(ch: &Option<Chan>) -> &'static str {
    match ch {
        None => "invalid chid                              ",
        Some(ch) if ch.native_type().is_none() => "valid chid, IOC not found                 ",
        Some(ch) if ch.access().0 => "valid chid, IOC was found, still available",
        Some(_) => "valid chid, IOC was found but unavailable ",
    }
}

/// `writeScanRecCompleted`'s `ca_array_get`s of `P<n>RA` (`DBR_DOUBLE`) and
/// `D<nn>DA` (`DBR_FLOAT`), `bcpt` long, completed by `ca_pend_io(1.0)`.
fn fetch_arrays(ca: &Ca, c: &ScanChans, s: &mut Scan) -> bool {
    let count = s.bcpt.max(0) as u32;
    let mut reqs: Vec<(&Chan, u16, u32)> = Vec::new();
    let mut slots = Vec::new();
    for i in 0..SCAN_NBP {
        if s.pos[i].valid() {
            match &c.pxra[i] {
                None => println!(
                    "saveData:writeScanRecCompleted: Can't get {} positioner array {i}",
                    s.name
                ),
                Some(ch) if count > 0 => {
                    reqs.push((ch, DBR_DOUBLE, count));
                    slots.push((true, i));
                }
                Some(_) => {}
            }
        }
    }
    for i in 0..SCAN_NBD {
        if s.det[i].dxnv == XXNV_OK && s.det[i].dxda.is_some() {
            match &c.dxda[i] {
                None => println!(
                    "saveData:writeScanRecCompleted: Can't get {} detector array {i}",
                    s.name
                ),
                Some(ch) if count > 0 => {
                    reqs.push((ch, DBR_FLOAT, count));
                    slots.push((false, i));
                }
                Some(_) => {}
            }
        }
    }
    let Some(got) = ca.gets(&reqs, 1.0) else {
        return false;
    };
    for ((pos, i), snap) in slots.into_iter().zip(got) {
        let Some(snap) = snap else {
            println!(
                "saveData:writeScanRecCompleted: ca_array_get() ({} pts) failed for scan {}, {}{i}",
                s.bcpt,
                s.name,
                if pos { 'p' } else { 'd' }
            );
            continue;
        };
        let v = elements(&snap.value);
        if pos {
            let dst = &mut s.pos[i].pxra;
            for (d, x) in dst.iter_mut().zip(v.iter().take(count as usize)) {
                *d = *x;
            }
        } else if let Some(dst) = s.det[i].dxda.as_mut() {
            for (d, x) in dst.iter_mut().zip(v.iter().take(count as usize)) {
                *d = *x as f32;
            }
        }
    }
    true
}

/// `mkdir(path, 0775)`.
fn mkdir_0775(path: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o775).create(path)
    }
    #[cfg(not(unix))]
    std::fs::DirBuilder::new().create(path)
}

/// `checkRWpermission`: create and remove a `rix_` file in `path`.
fn check_rw_permission(path: &str) -> bool {
    let mut tmp = bounded(&format!("{path}/rix_"), 100);
    while file_status(&tmp).is_ok() && tmp.len() < 100 {
        tmp.push('_');
    }
    if file_status(&tmp).is_ok() {
        return false;
    }
    let created = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&tmp);
    if created.is_err() || file_status(&tmp).is_err() {
        return false;
    }
    let _ = std::fs::remove_file(&tmp);
    true
}

/// The `[scanRecord]` and `[extraPV]` entries of a req file, as
/// `initSaveDataTask` reads them.
#[cfg(test)]
fn req_entries(path: &str, macros: &str) -> (Vec<String>, Vec<(String, String)>) {
    let mut rf = ReqFile::open(path, Some(macros)).unwrap();
    let mut scans = Vec::new();
    let mut extra = Vec::new();
    if rf.goto_section("scanRecord") {
        while !rf.eos() {
            let (n, name) = mac_id(&mut rf, PVNAME_STRINGSZ);
            if n == 0 && rf.current() != Some(b',') {
                rf.read_char();
                rf.skip_comment();
                continue;
            }
            if rf.current() == Some(b',') {
                rf.read_char();
                mac_id(&mut rf, PVNAME_STRINGSZ);
            }
            scans.push(name);
        }
    }
    if rf.goto_section("extraPV") {
        while !rf.eos() {
            let (n, name) = mac_id(&mut rf, PVNAME_STRINGSZ);
            let desc = if rf.current() == Some(b'"') {
                rf.read_string(PVNAME_STRINGSZ)
            } else {
                String::new()
            };
            if n == 0 && desc.is_empty() {
                rf.read_char();
                rf.skip_comment();
                continue;
            }
            extra.push((name, desc));
        }
    }
    (scans, extra)
}

fn arg_str(args: &[ArgValue], i: usize) -> String {
    match args.get(i) {
        Some(ArgValue::String(s)) => s.to_str_lossy().into_owned(),
        _ => String::new(),
    }
}

fn arg_int(args: &[ArgValue], i: usize) -> i32 {
    match args.get(i) {
        Some(ArgValue::Int(v)) => *v as i32,
        _ => 0,
    }
}

/// `saveDataRegistrar`: the seven commands and three variables.
pub fn register_save_data() {
    let s = |name| ArgDesc {
        name,
        arg_type: ArgType::String,
    };
    let int = |name| ArgDesc {
        name,
        arg_type: ArgType::Int,
    };
    register_command(CommandDef::new(
        "saveData_Init",
        vec![s("fname"), s("macros")],
        "",
        |args: &[ArgValue], ctx: &CommandContext| -> CommandResult {
            save_data_init(
                &arg_str(args, 0),
                &arg_str(args, 1),
                ctx.db().ca_client(),
                ctx.bridge().clone(),
            );
            Ok(CommandOutcome::Continue)
        },
    ));
    register_command(CommandDef::new(
        "saveData_PrintScanInfo",
        vec![s("name")],
        "",
        |args: &[ArgValue], ctx: &CommandContext| -> CommandResult {
            ctx.print_fmt(format_args!(
                "{}",
                save_data_print_scan_info(&arg_str(args, 0))
            ));
            Ok(CommandOutcome::Continue)
        },
    ));
    register_command(CommandDef::new(
        "saveData_Priority",
        vec![int("priority")],
        "",
        |args: &[ArgValue], _: &CommandContext| -> CommandResult {
            save_data_priority(arg_int(args, 0));
            Ok(CommandOutcome::Continue)
        },
    ));
    register_command(CommandDef::new(
        "saveData_SetCptWait_ms",
        vec![int("ms")],
        "",
        |args: &[ArgValue], _: &CommandContext| -> CommandResult {
            save_data_set_cpt_wait_ms(arg_int(args, 0));
            Ok(CommandOutcome::Continue)
        },
    ));
    register_command(CommandDef::new(
        "saveData_Version",
        vec![],
        "",
        |_: &[ArgValue], ctx: &CommandContext| -> CommandResult {
            ctx.println(&format!("saveData Version: {SAVE_DATA_VERSION}"));
            Ok(CommandOutcome::Continue)
        },
    ));
    register_command(CommandDef::new(
        "saveData_CVS",
        vec![],
        "",
        |_: &[ArgValue], ctx: &CommandContext| -> CommandResult {
            ctx.println("saveData CVS: $Id: saveData.c,v 1.46 2009-10-15 19:23:02 mooney Exp $");
            Ok(CommandOutcome::Continue)
        },
    ));
    register_command(CommandDef::new(
        "saveData_Info",
        vec![],
        "",
        |_: &[ArgValue], ctx: &CommandContext| -> CommandResult {
            ctx.print_fmt(format_args!("{}", save_data_info()));
            Ok(CommandOutcome::Continue)
        },
    ));
    register_variable(VarDef {
        name: "debug_saveData",
        access: VarAccess::Int {
            get: || i64::from(mda::DEBUG_SAVE_DATA.load(Ordering::Relaxed)),
            set: |v| mda::DEBUG_SAVE_DATA.store(v as i32, Ordering::Relaxed),
        },
    });
    register_variable(VarDef {
        name: "debug_saveDataMsg",
        access: VarAccess::Int {
            get: || i64::from(DEBUG_SAVE_DATA_MSG.load(Ordering::Relaxed)),
            set: |v| DEBUG_SAVE_DATA_MSG.store(v as i32, Ordering::Relaxed),
        },
    });
    register_variable(VarDef {
        name: "saveData_MessagePolicy",
        access: VarAccess::Int {
            get: || i64::from(MESSAGE_POLICY.load(Ordering::Relaxed)),
            set: |v| MESSAGE_POLICY.store(v as i32, Ordering::Relaxed),
        },
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stray character in `[scanRecord]` is stepped over; C spins on it.
    #[test]
    fn stray_char_in_scan_record_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.req");
        std::fs::write(
            &path,
            "[scanRecord]\n$(P)scan1 ! $(P)scan2,$(P)scanH\n[extraPV]\n$(P)a \"desc a\" ! $(P)b\n",
        )
        .unwrap();
        let (scans, extra) = req_entries(path.to_str().unwrap(), "P=x:");
        assert_eq!(scans, ["x:scan1", "x:scan2"]);
        assert_eq!(
            extra,
            [
                ("x:a".to_string(), "desc a".to_string()),
                ("x:b".to_string(), String::new())
            ]
        );
    }

    /// `saveData_SetCptWait_ms(100)` waits 0.1 s; C's integer division
    /// made it 0.
    #[test]
    fn cpt_wait_keeps_milliseconds() {
        save_data_set_cpt_wait_ms(100);
        assert_eq!(cpt_wait_time(), 0.1);
    }

    /// `checkRWpermission` leaves nothing behind.
    #[test]
    fn rw_permission_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(check_rw_permission(dir.path().to_str().unwrap()));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(!check_rw_permission(
            dir.path().join("missing").to_str().unwrap()
        ));
    }

    #[test]
    fn extra_types_follow_connect_pv() {
        assert_eq!(extra_dbr_type(DbFieldType::Enum), DBR_STRING);
        assert_eq!(extra_dbr_type(DbFieldType::UShort), DBR_CTRL_LONG);
        assert_eq!(extra_dbr_type(DbFieldType::Int64), DBR_CTRL_DOUBLE);
    }
}
