//! motorUtil.cc and motorUtilAux.cc: `motorUtilInit`'s task, which keeps
//! motorUtil.db's `$(P)alldone`, `$(P)moving` and `$(P)movingDiff` on the
//! IOC's motors' `.DMOV`, and stops every moving motor when `$(P)allstop`
//! goes to `stop`; `listMovingMotors` and `printChIDlist`.
//!
//! The channels are [`CaClientContext`]'s, as C's `ca_context_create` in an
//! IOC gets `dbContext`. A subscription only forwards its value into the
//! queue, and the task thread runs C's handler when it takes the message.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use epics_base_rs::error::eca::ECA_TIMEOUT;
use epics_base_rs::runtime::log::errlog_vprintf;
use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::runtime::task::{
    BlockingBridge, StackSizeClass, ThreadPriority, block_on_sync, spawn_dedicated_thread, timeout,
};
use epics_base_rs::server::database::{
    CaClientContext, ClientChannel, ClientSubscription, Connection, PvDatabase,
};
use epics_base_rs::server::iocsh::registry::*;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{DBR_SHORT, DBR_STRING, EpicsValue};
use parking_lot::Mutex;

type Chan = Arc<dyn ClientChannel>;

/// `TIMEOUT`, `ca_pend_io`'s.
const TIMEOUT: Duration = Duration::from_secs(60);
/// `PVNAME_SZ`.
const PVNAME_SZ: usize = 60;
/// `ca_add_event`'s mask, `DBE_VALUE | DBE_ALARM`.
const DBE_DEFAULT: u16 = 1 | 4;

/// `Motor_pv_info`.
#[derive(Default)]
struct Motor {
    name: String,
    dmov: Option<Chan>,
    stop: Option<Chan>,
    in_motion: bool,
    index: usize,
}

/// motorUtil.cc's statics, which the task fills and the iocsh commands
/// read.
struct State {
    motors: Vec<Motor>,
    allstop: Option<Chan>,
    alldone: Option<Chan>,
    moving: Option<Chan>,
    movingdiff: Option<Chan>,
    old_num_motors_moving: i32,
    old_alldone_value: i16,
}

static STATE: Mutex<State> = Mutex::new(State {
    motors: Vec::new(),
    allstop: None,
    alldone: None,
    moving: None,
    movingdiff: None,
    old_num_motors_moving: 0,
    old_alldone_value: 1,
});

/// `motorUtilInit`'s `initialized`.
static INITIALIZED: AtomicBool = AtomicBool::new(false);

enum Msg {
    /// `dmov_handler`: the motor's index and its `.DMOV`.
    Dmov(usize, i16),
    /// `allstop_handler`: `$(P)allstop` as a string.
    Allstop(String),
}

/// The CA half of the task, run from its thread.
struct Ca {
    ctx: CaClientContext,
    bridge: BlockingBridge,
    tx: mpsc::Sender<Msg>,
}

impl Ca {
    fn run<F: Future>(&self, f: F) -> F::Output {
        self.bridge.block_on(f)
    }

    /// `getChID`: `ca_search_and_connect` and `ca_pend_io(TIMEOUT)`. A name
    /// no context can search for is the timeout at once.
    fn get_ch_id(&self, name: &str) -> Option<Chan> {
        let ch = self.run(async {
            let ch = self.ctx.create_channel(name).await?;
            let up = timeout(TIMEOUT, async {
                while let Some(c) = ch.next_connection().await {
                    if c == Connection::Connected {
                        return true;
                    }
                }
                false
            })
            .await
            .unwrap_or(false);
            up.then_some(ch)
        });
        if ch.is_none() {
            errlog_vprintf(format_args!(
                "motorUtil.cc: getChID({name}) error: {ECA_TIMEOUT}\n"
            ));
        }
        ch
    }

    /// `pvMonitor`: `ca_add_event` of one `dbr_type` element, each value
    /// queued as `f` makes it.
    fn monitor(
        &self,
        ch: &Chan,
        dbr_type: u16,
        f: impl Fn(Snapshot) -> Msg + Send + Sync + 'static,
    ) -> Option<ClientSubscription> {
        let tx = self.tx.clone();
        let callback = Box::new(move |s| {
            // The wait for room holds the delivering thread; a reactor
            // worker that may not block drops the value.
            if let Err(mpsc::error::TrySendError::Full(msg)) = tx.try_send(f(s)) {
                let _ = block_on_sync(tx.send(msg));
            }
        });
        // C answers a failed ca_add_event with ca_task_exit and then goes on
        // searching on the context it destroyed; only this monitor is lost.
        self.run(ch.subscribe(dbr_type, 1, DBE_DEFAULT, callback))
            .ok()
    }

    /// `ca_put`, issued and not waited on.
    fn put(&self, ch: Option<&Chan>, v: EpicsValue) {
        if let Some(ch) = ch {
            let _ = self.run(ch.put(v, false));
        }
    }
}

/// `motorUtilInit(vme_name)`. motorUtil.cc does not include `epicsStdio.h`,
/// so its two lines stay on the console under `motorUtilInit(...) > f`.
pub fn motor_util_init(vme_name: &str, ctx: &CommandContext) {
    if INITIALIZED.load(Ordering::Acquire) {
        ctx.console_println("motorUtil already initialized. Exiting");
        return;
    }
    // C's `temp[PVNAME_STRINGSZ+5]` holds the prefix and `movingDiff.VAL`,
    // which overruns it for a prefix of 52 or 53 characters; a String
    // cannot.
    if vme_name.len() > PVNAME_SZ - 7 {
        ctx.console_println(&format!(
            "motorUtilInit: Prefix {vme_name} has more than {} characters. Exiting",
            PVNAME_SZ - 7
        ));
        return;
    }
    INITIALIZED.store(true, Ordering::Release);
    let (tx, rx) = mpsc::channel(64);
    let ca = Ca {
        ctx: ctx.db().ca_client(),
        bridge: ctx.bridge().clone(),
        tx,
    };
    let db = ctx.db().clone();
    let vme = vme_name.to_owned();
    let _ = spawn_dedicated_thread(
        "motorUtil".into(),
        ThreadPriority::Medium,
        StackSizeClass::Medium,
        move || motor_util_task(&ca, &db, &vme, rx),
    );
}

/// `getMotorList`: the motor records, aliases left out, in their load
/// order.
fn get_motor_list(db: &PvDatabase) -> Vec<String> {
    db.all_record_names()
        .into_iter()
        .filter(|name| {
            db.get_record(name)
                .is_some_and(|rec| rec.read().record.record_type() == "motor")
        })
        .map(|name| name.to_string())
        .collect()
}

/// `motorUtil_task`.
fn motor_util_task(ca: &Ca, db: &PvDatabase, vme: &str, mut rx: mpsc::Receiver<Msg>) {
    let motorlist = get_motor_list(db);
    if motorlist.is_empty() {
        // C waits on an event nobody signals.
        while rx.blocking_recv().is_some() {}
        return;
    }
    // callocMustSucceed: printChIDlist shows these blank until filled.
    STATE.lock().motors = motorlist.iter().map(|_| Motor::default()).collect();
    let chid_moving = ca.get_ch_id(&format!("{vme}moving.VAL"));
    STATE.lock().moving = chid_moving.clone();
    let chid_alldone = ca.get_ch_id(&format!("{vme}alldone.VAL"));
    STATE.lock().alldone = chid_alldone.clone();
    let chid_movingdiff = ca.get_ch_id(&format!("{vme}movingDiff.VAL"));
    STATE.lock().movingdiff = chid_movingdiff.clone();
    if chid_moving.is_none() || chid_alldone.is_none() || chid_movingdiff.is_none() {
        errlog_vprintf(format_args!(
            "Failed to connect to {vme}moving or {vme}alldone or {vme}movingDiff.\n\
             Check prefix matches Db\n"
        ));
        return;
    }
    // The task's for its life, as C's are its context's.
    let mut subs = Vec::new();
    for (i, name) in motorlist.iter().enumerate() {
        {
            let mut st = STATE.lock();
            st.motors[i].index = i;
            st.motors[i].name = name.clone();
        }
        let dmov = ca.get_ch_id(&format!("{name}.DMOV"));
        // C hands the null chid of a .DMOV that did not connect to
        // ca_add_event; a motor's own field always connects here.
        if let Some(ch) = &dmov {
            subs.extend(ca.monitor(ch, DBR_SHORT, move |s| {
                Msg::Dmov(i, s.value.to_f64().unwrap_or(0.0) as i16)
            }));
        }
        let stop = ca.get_ch_id(&format!("{name}.STOP"));
        let mut st = STATE.lock();
        (st.motors[i].dmov, st.motors[i].stop) = (dmov, stop);
    }
    let allstop = ca.get_ch_id(&format!("{vme}allstop.VAL"));
    match &allstop {
        None => errlog_vprintf(format_args!("Failed to connect to {vme}allstop\n")),
        Some(ch) => subs.extend(ca.monitor(ch, DBR_STRING, |s| {
            Msg::Allstop(match s.value {
                EpicsValue::String(v) => v.as_str_lossy().into_owned(),
                _ => String::new(),
            })
        })),
    }
    STATE.lock().allstop = allstop;
    while let Some(msg) = rx.blocking_recv() {
        match msg {
            Msg::Dmov(i, dmov) => moving(ca, i, dmov),
            Msg::Allstop(v) => stop_all(ca, &v),
        }
    }
}

/// `stopAll`. The puts go out after the state is let go, as every put
/// below does: one may process a record.
fn stop_all(ca: &Ca, value: &str) {
    if value == "release" {
        return;
    }
    let mut puts: Vec<(Option<Chan>, EpicsValue)> = Vec::new();
    {
        let st = STATE.lock();
        // if at least one motor is moving, then continue with stop all
        if motor_moving_count(&st) > 0 {
            // Only a moving motor, so as not to stop one whose device and
            // driver support were not loaded.
            for m in st.motors.iter().filter(|m| m.in_motion) {
                puts.push((m.stop.clone(), EpicsValue::Short(1)));
            }
        }
        // reset allstop so that it may be called again
        puts.push((st.allstop.clone(), EpicsValue::Short(0)));
    }
    for (ch, v) in puts {
        ca.put(ch.as_ref(), v);
    }
}

/// `moving`.
fn moving(ca: &Ca, index: usize, dmov: i16) {
    let mut puts: Vec<(Option<Chan>, EpicsValue)> = Vec::new();
    {
        let mut st = STATE.lock();
        st.motors[index].in_motion = dmov == 0;
        let diff_char = if dmov != 0 { '-' } else { '+' };
        let num_motors_moving = motor_moving_count(&st);
        let new_alldone_value = i16::from(num_motors_moving == 0);
        if new_alldone_value != st.old_alldone_value {
            puts.push((st.alldone.clone(), EpicsValue::Short(new_alldone_value)));
            st.old_alldone_value = new_alldone_value;
        }
        if num_motors_moving != st.old_num_motors_moving {
            puts.push((st.moving.clone(), EpicsValue::Long(num_motors_moving)));
            // Tell which motor's dmov changed, with its NUL as C's strlen+1.
            let mut diff = format!("{diff_char}{}", st.motors[index].name).into_bytes();
            diff.push(0);
            puts.push((st.movingdiff.clone(), EpicsValue::CharArray(diff.into())));
            st.old_num_motors_moving = num_motors_moving;
        }
    }
    for (ch, v) in puts {
        ca.put(ch.as_ref(), v);
    }
}

/// `motorMovingCount`.
fn motor_moving_count(st: &State) -> i32 {
    st.motors.iter().filter(|m| m.in_motion).count() as i32
}

/// `listMovingMotors`.
pub fn list_moving_motors() {
    errlog_vprintf(format_args!("\nThe following motors are moving:\n"));
    for m in STATE.lock().motors.iter().filter(|m| m.in_motion) {
        errlog_vprintf(format_args!("{}, index = {}\n", m.name, m.index));
    }
}

/// A chid as C's `%p` prints it.
fn chid(ch: &Option<Chan>) -> String {
    match ch {
        Some(ch) => format!("{:p}", Arc::as_ptr(ch).cast::<()>()),
        None => "(nil)".into(),
    }
}

/// `printChIDlist`.
pub fn print_chid_list() {
    let st = STATE.lock();
    for (i, m) in st.motors.iter().enumerate() {
        // C's string continues across the line, its indent and all.
        errlog_vprintf(format_args!(
            "i = {i},\tname = {}\tchid_dmov = {}\tchid_stop =                {}\
             \tin_motion = {}\tindex = {}\n",
            m.name,
            chid(&m.dmov),
            chid(&m.stop),
            i32::from(m.in_motion),
            m.index,
        ));
    }
    errlog_vprintf(format_args!("chid_allstop = {}\n", chid(&st.allstop)));
    errlog_vprintf(format_args!("chid_alldone = {}\n", chid(&st.alldone)));
    errlog_vprintf(format_args!("chid_moving = {}\n", chid(&st.moving)));
}

/// C `motorUtilRegister` (motorUtil.cc:433-438), which motorSupport.dbd
/// gives every motor IOC.
pub fn motor_util_commands() -> [CommandDef; 3] {
    [
        CommandDef::new(
            "motorUtilInit",
            vec![ArgDesc {
                name: "IOC name",
                arg_type: ArgType::String,
            }],
            "motorUtilInit(prefix)",
            |args: &[ArgValue], ctx: &CommandContext| {
                let name = match &args[0] {
                    ArgValue::String(s) => s.to_str()?.to_owned(),
                    _ => String::new(),
                };
                motor_util_init(&name, ctx);
                Ok(CommandOutcome::Continue)
            },
        ),
        CommandDef::new(
            "printChIDlist",
            vec![ArgDesc {
                name: "Print motorUtil chid list",
                arg_type: ArgType::String,
            }],
            "printChIDlist",
            |_: &[ArgValue], _: &CommandContext| {
                print_chid_list();
                Ok(CommandOutcome::Continue)
            },
        ),
        CommandDef::new(
            "listMovingMotors",
            vec![ArgDesc {
                name: "List moving motors",
                arg_type: ArgType::String,
            }],
            "listMovingMotors",
            |_: &[ArgValue], _: &CommandContext| {
                list_moving_motors();
                Ok(CommandOutcome::Continue)
            },
        ),
    ]
}
