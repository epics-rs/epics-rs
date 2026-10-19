//! A record that posts by hand from `process()` and resets its own alarm, as
//! C throttle's `valuePut` does (`throttleRecord.c:540-615`), against
//! oracleStdIoc (std R6-x, softIoc R7.0.10) with
//!
//! ```text
//! record(throttle, "TZ") { field(OUT, "DST2") }
//! ```
//!
//! under `camonitor -m v` / `-m a` of TZ.SENT, TZ.WAIT and TZ.STS:
//!
//! ```text
//! caput TZ 0   -m v WAIT True  <undefined> UDF INVALID
//!              -m v SENT 0     <undefined> UDF INVALID
//!              -m v WAIT False <undefined> UDF INVALID
//!              -m v STS Success <undefined> UDF INVALID
//! ```
//!
//! The put arm's posts carry the alarm the record held before the cycle.

use epics_base_rs::types::WallTime;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::recgbl::alarm_status::{LINK_ALARM, NO_ALARM, UDF_ALARM};
use epics_base_rs::server::record::{
    CycleHooks, CycleMarks, CyclePostMask, FieldDesc, InputLinkRequest, LinkRead, LinkReadAs,
    ProcessAction, ProcessActions, ProcessOutcome, Record,
};
use epics_base_rs::types::{DbFieldType, DbfCode, EpicsValue};

const STS_ERR: i16 = 1;
const STS_SUC: i16 = 2;

/// `valuePut`'s and `valueSync`'s `db_post_events` ahead of the forward link,
/// in C order.
static POSTS: [(epics_base_rs::server::record::FieldIx, CyclePostMask); 4] =
    epics_base_rs::server::record::declared_post_table(
        FIELDS,
        [
            ("SENT", CyclePostMask::Value),
            ("VAL", CyclePostMask::Value),
            ("STS", CyclePostMask::Value),
            ("WAIT", CyclePostMask::Value),
        ],
    );
const POST_SENT: u64 = 1;
const POST_SYNC_VAL: u64 = 1 << 1;
const POST_SYNC_STS: u64 = 1 << 2;
const POST_WAIT: u64 = 1 << 3;

/// `valuePut`'s posts after the forward link (`throttleRecord.c:592-612`).
static TAIL: [(epics_base_rs::server::record::FieldIx, CyclePostMask); 3] =
    epics_base_rs::server::record::declared_post_table(
        FIELDS,
        [
            ("STS", CyclePostMask::Value),
            ("SENT", CyclePostMask::ResetValueLog),
            ("SENT", CyclePostMask::ResetAlarm),
        ],
    );

/// `process()`'s WAIT=True post (`throttleRecord.c:287`).
static PROCESS_POSTS: [(epics_base_rs::server::record::FieldIx, CyclePostMask); 1] =
    epics_base_rs::server::record::declared_post_table(FIELDS, [("WAIT", CyclePostMask::Value)]);

/// C throttle's cycle with the drive limits reduced to a rejection.
#[derive(Default)]
struct Throttle {
    val: f64,
    oval: f64,
    sent: f64,
    osent: f64,
    wait: i16,
    sts: i16,
    dly: f64,
    drvlh: f64,
    drvll: f64,
    outl: String,
    sinp: String,
    pending: Option<f64>,
    delay_active: bool,
    sync_flag: bool,
    process_tail: bool,
    /// A `valuePut` ran whose tail is not yet taken.
    tail_due: bool,
    /// `valuePut` ran a wait arm: STS posts after the forward link (:592).
    tail_sts: bool,
    /// `valuePut` took its non-CONSTANT arm, the one `recGblFwdLink` is on.
    forward: bool,
    posts: u64,
    process_posts: u64,
    queued: Vec<ProcessAction>,
}

static FIELDS: &[FieldDesc] = &[
    FieldDesc::new("VAL", DbFieldType::Double, false),
    FieldDesc::new("SENT", DbFieldType::Double, false),
    FieldDesc::new("WAIT", DbFieldType::Short, false),
    FieldDesc::new("STS", DbFieldType::Short, false),
    FieldDesc::new("SYNC", DbFieldType::Short, false),
    FieldDesc::new("DLY", DbFieldType::Double, false),
    FieldDesc::new("DRVLH", DbFieldType::Double, false),
    FieldDesc::new("DRVLL", DbFieldType::Double, false),
    FieldDesc {
        declared_dbf: DbfCode::Outlink,
        ..FieldDesc::new("OUTL", DbFieldType::String, false)
    },
    FieldDesc {
        declared_dbf: DbfCode::Inlink,
        ..FieldDesc::new("SINP", DbFieldType::String, false)
    },
];

impl Throttle {
    /// C `valuePut` up to its tail.
    fn value_put(&mut self, actions: &mut Vec<ProcessAction>) {
        let Some(value) = self.pending.take() else {
            self.delay_active = false;
            self.value_put_tail();
            return;
        };
        if self.outl.is_empty() {
            self.sts = STS_ERR;
            self.value_put_done();
        } else {
            self.forward = true;
            actions.push(ProcessAction::WriteDbLink {
                link_field: "OUTL",
                value: EpicsValue::Double(value),
            });
        }
        self.delay_active = self.dly > 0.0;
        if self.delay_active {
            actions.push(ProcessAction::ValuePutAfter(Duration::from_secs_f64(
                self.dly,
            )));
        }
    }

    fn value_put_done(&mut self) {
        self.wait = 0;
        self.posts |= POST_WAIT;
        self.tail_sts = true;
        self.value_put_tail();
    }

    /// `throttleRecord.c:602-614`, taken by `take_value_put_tail`.
    fn value_put_tail(&mut self) {
        self.tail_due = true;
    }
}

impl Record for Throttle {
    fn record_type(&self) -> &'static str {
        "hand_posted_throttle"
    }
    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::PROCESS_POSTED.union(CycleHooks::HAND_POSTED_OWN_STAMP)
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let mut actions = Vec::new();
        self.forward = false;
        self.process_tail = true;
        if self.drvlh > self.drvll && (self.val < self.drvll || self.val > self.drvlh) {
            self.val = self.oval;
            return Ok(ProcessOutcome::complete());
        }
        self.wait = 1;
        self.process_posts = 1;
        self.pending = Some(self.val);
        if !self.delay_active {
            self.value_put(&mut actions);
        }
        Ok(ProcessOutcome::complete_with(actions))
    }
    fn special(
        &mut self,
        field: &str,
        after: bool,
        _common: &epics_base_rs::server::record::CommonFields,
    ) -> CaResult<()> {
        if after && field == "SYNC" {
            self.sync_flag = true;
            if self.pending.is_none() {
                self.sync_flag = false;
                self.queued
                    .push(ProcessAction::GetLink { link_field: "SINP" });
            }
        }
        Ok(())
    }
    /// `process()` keeps `recGblFwdLink` commented out (`throttleRecord.c:308`).
    fn should_fire_forward_link(&self) -> bool {
        self.forward
    }
    fn take_special_actions(&mut self) -> Vec<ProcessAction> {
        std::mem::take(&mut self.queued)
    }
    fn set_out_link_write_status(
        &mut self,
        _link_field: &'static str,
        value: &EpicsValue,
        failed: bool,
    ) -> ProcessActions {
        let mut actions = ProcessActions::new();
        if failed {
            self.sts = STS_ERR;
        } else {
            self.sts = STS_SUC;
            self.sent = value.to_f64().unwrap();
            self.posts |= POST_SENT;
            if std::mem::take(&mut self.sync_flag) {
                actions.push(ProcessAction::GetLink { link_field: "SINP" });
            }
        }
        self.value_put_done();
        actions
    }
    fn link_read_result(&mut self, _link_field: &'static str, read: LinkRead) -> ProcessActions {
        self.posts |= POST_SYNC_STS;
        match read {
            LinkRead::Value(v) => {
                self.val = v.to_f64().unwrap();
                self.sts = STS_SUC;
                self.posts |= POST_SYNC_VAL;
            }
            LinkRead::Succeeded | LinkRead::Failed => self.sts = STS_ERR,
        }
        ProcessActions::new()
    }
    fn input_link_request(&self, _link_field: &str) -> InputLinkRequest {
        InputLinkRequest::As(LinkReadAs::Double)
    }
    fn take_cycle_posted_fields(&mut self) -> CycleMarks {
        CycleMarks::of(&POSTS, u128::from(std::mem::take(&mut self.posts)))
    }
    fn take_value_put_tail(&mut self) -> CycleMarks {
        if !std::mem::take(&mut self.tail_due) {
            return CycleMarks::NONE;
        }
        let moved = self.osent != self.sent;
        self.osent = self.sent;
        let sts = std::mem::take(&mut self.tail_sts) as u128;
        CycleMarks::of(&TAIL, sts | if moved { 1 << 1 } else { 1 << 2 })
    }
    fn take_process_posted_fields(&mut self) -> CycleMarks {
        CycleMarks::of(
            &PROCESS_POSTS,
            u128::from(std::mem::take(&mut self.process_posts)),
        )
    }
    /// throttle.dbd's `pp(TRUE)` fields.
    fn process_passive_fields(&self) -> &'static [&'static str] {
        &["VAL"]
    }
    fn process_posted_fields(&self) -> Option<epics_base_rs::server::record::DeclaredFields> {
        Some(
            &const {
                epics_base_rs::server::record::declared_set(FIELDS, ["VAL", "WAIT", "SENT", "STS"])
            },
        )
    }
    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &["WAIT", "SENT", "STS"]
    }
    /// C `process()`'s closing VAL post (`throttleRecord.c:296-302`).
    fn monitor_value_changed(&mut self) -> Option<bool> {
        if !std::mem::take(&mut self.process_tail) {
            return Some(false);
        }
        let changed = self.oval != self.val;
        self.oval = self.val;
        Some(changed)
    }
    /// `delayFuncCallback` (`throttleRecord.c:530-538`).
    fn delayed_value_put(&mut self) -> ProcessActions {
        let mut actions = Vec::new();
        self.forward = false;
        self.value_put(&mut actions);
        actions.into()
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        Some(match name {
            "VAL" => EpicsValue::Double(self.val),
            "SENT" => EpicsValue::Double(self.sent),
            "WAIT" => EpicsValue::Short(self.wait),
            "STS" => EpicsValue::Short(self.sts),
            "SYNC" => EpicsValue::Short(0),
            "DLY" => EpicsValue::Double(self.dly),
            "DRVLH" => EpicsValue::Double(self.drvlh),
            "DRVLL" => EpicsValue::Double(self.drvll),
            "OUTL" => EpicsValue::String(self.outl.clone().into()),
            "SINP" => EpicsValue::String(self.sinp.clone().into()),
            _ => return None,
        })
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        let f = || value.to_f64().unwrap_or(0.0);
        match name {
            "VAL" => self.val = f(),
            "SYNC" => {}
            "DLY" => self.dly = f(),
            "DRVLH" => self.drvlh = f(),
            "DRVLL" => self.drvll = f(),
            "OUTL" => self.outl = value.to_string(),
            "SINP" => self.sinp = value.to_string(),
            _ => return Err(CaError::FieldNotFound(name.into())),
        }
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

const DB: &str = r#"
record(ao, "DST2") {}
record(stringin, "SS") { field(VAL, "abc") }
record(hand_posted_throttle, "TF") { field(SINP, "SS") field(OUTL, "DST2") }
record(hand_posted_throttle, "TN") {}
record(hand_posted_throttle, "TZ") { field(OUTL, "DST2") }
record(hand_posted_throttle, "TW") { field(OUTL, "DST2") field(DRVLH, "10") field(DRVLL, "5") }
record(hand_posted_throttle, "TG") { field(OUTL, "DST2") field(DLY, "1") }
record(ao, "NUM") { field(VAL, "7") }
record(hand_posted_throttle, "TS") { field(OUTL, "DST2") field(DLY, "1") field(SINP, "NUM") }
record(hand_posted_throttle, "TC") { field(SINP, "SS") field(OUTL, "DST2") field(DLY, "1") }
record(hand_posted_throttle, "TD") { field(OUTL, "DST2") field(DLY, "1") }
record(calc, "CNT") { field(CALC, "VAL+1") }
record(calc, "CNT2") { field(CALC, "VAL+1") }
record(hand_posted_throttle, "TL") { field(OUTL, "DST2") field(DLY, "1") field(FLNK, "CNT") }
record(hand_posted_throttle, "TM") { field(FLNK, "CNT2") }
record(hand_posted_throttle, "TK") { field(SINP, "SS") field(OUTL, "DST2") field(DLY, "1") field(FLNK, "K") }
record(calc, "K") { field(CALC, "C+1") field(INPC, "K") field(INPA, "TK.SEVR NPP") }
record(hand_posted_throttle, "TU") { field(OUTL, "DST2") field(DLY, "1") }
record(ao, "WU") { field(OUT, "TU.UDF NPP") }
record(hand_posted_throttle, "TT") { field(OUTL, "DST2") field(DLY, "1") field(TPRO, "1") }
"#;

async fn ioc() -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("hand_posted_throttle", || Box::new(Throttle::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// One post as camonitor prints it: value, STAT, SEVR.
type Post = (f64, u16, u16);

fn subscribe(db: &PvDatabase, rec: &str, field: &str, mask: EventMask) -> EventReader {
    let inst = db.get_record(rec).unwrap();
    let mut g = inst.write();
    g.add_subscriber(field, 1, DbFieldType::Double, mask.bits())
        .unwrap()
}

fn drain(rx: &mut EventReader) -> Vec<Post> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push((
            e.value().as_value().and_then(EpicsValue::to_f64).unwrap(),
            e.stat(),
            e.sevr(),
        ));
    }
    out
}

async fn put(db: &PvDatabase, rec: &str, field: &str, v: f64) {
    let _ = db
        .put_record_field_from_ca_no_notify(rec, field, EpicsValue::Double(v))
        .await;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
}

const INVALID: u16 = 3;

#[epics_macros_rs::epics_test]
async fn the_put_arm_posts_under_the_alarm_held_before_the_cycle() {
    let db = ioc().await;
    let mut sent = subscribe(&db, "TZ", "SENT", EventMask::VALUE);
    let mut wait = subscribe(&db, "TZ", "WAIT", EventMask::VALUE);
    let mut sts = subscribe(&db, "TZ", "STS", EventMask::VALUE);
    for rx in [&mut sent, &mut wait, &mut sts] {
        drain(rx);
    }
    put(&db, "TZ", "VAL", 0.0).await;
    let udf = |v: f64| (v, UDF_ALARM, INVALID);
    assert_eq!(drain(&mut sent), [udf(0.0)]);
    assert_eq!(drain(&mut wait), [udf(1.0), udf(0.0)]);
    assert_eq!(drain(&mut sts), [udf(f64::from(STS_SUC))]);
}

/// `caput TZ 0` under `camonitor -m a TZ.SENT`: `0 NO_ALARM`, stamped. SENT
/// did not move, so `valuePut` posts its reset's `DBE_ALARM` alone.
#[epics_macros_rs::epics_test]
async fn an_unmoved_sent_posts_the_reset_alarm_alone() {
    let db = ioc().await;
    let mut sent = subscribe(&db, "TZ", "SENT", EventMask::ALARM);
    drain(&mut sent);
    put(&db, "TZ", "VAL", 0.0).await;
    assert_eq!(drain(&mut sent), [(0.0, NO_ALARM, 0)]);
}

/// `record(throttle, "TN") {}`, `caput TN 4`:
///
/// ```text
/// -m v STS Error  <undefined> UDF INVALID
/// -m a SENT 0     stamped
/// -m v VAL 4      stamped
/// ```
///
/// No OUT, so `valuePut` runs inside `process()` and resets the alarm there.
#[epics_macros_rs::epics_test]
async fn a_cycle_without_an_out_link_resets_inside_process() {
    let db = ioc().await;
    let mut sts = subscribe(&db, "TN", "STS", EventMask::VALUE);
    let mut sent_a = subscribe(&db, "TN", "SENT", EventMask::ALARM);
    let mut sent_v = subscribe(&db, "TN", "SENT", EventMask::VALUE);
    let mut val_v = subscribe(&db, "TN", "VAL", EventMask::VALUE);
    let mut val_a = subscribe(&db, "TN", "VAL", EventMask::ALARM);
    for rx in [&mut sts, &mut sent_a, &mut sent_v, &mut val_v, &mut val_a] {
        drain(rx);
    }
    put(&db, "TN", "VAL", 4.0).await;
    let udf = |v: f64| (v, UDF_ALARM, INVALID);
    assert_eq!(drain(&mut sts), [udf(f64::from(STS_ERR))]);
    assert_eq!(drain(&mut sent_a), [(0.0, NO_ALARM, 0)]);
    assert_eq!(drain(&mut sent_v), []);
    assert_eq!(drain(&mut val_v), [(4.0, NO_ALARM, 0)]);
    assert_eq!(drain(&mut val_a), []);
}

/// `caput TF.SYNC 1` (SINP reads stringin "abc" as DBR_DOUBLE and fails),
/// then `caput TF 1`:
///
/// ```text
/// -m v STS Error  <undefined> UDF INVALID     (the SYNC put)
/// -m a SENT 1     stamped LINK INVALID
/// -m a VAL 1      stamped NO_ALARM
/// ```
///
/// `valuePut`'s reset commits the LINK alarm the put left pending, and
/// `process()`'s commits NO_ALARM after it.
#[epics_macros_rs::epics_test]
async fn a_pending_link_alarm_rides_sents_tail_and_clears_on_val() {
    let db = ioc().await;
    let mut sts = subscribe(&db, "TF", "STS", EventMask::VALUE);
    let mut sent = subscribe(&db, "TF", "SENT", EventMask::ALARM);
    let mut val = subscribe(&db, "TF", "VAL", EventMask::ALARM);
    for rx in [&mut sts, &mut sent, &mut val] {
        drain(rx);
    }
    put(&db, "TF", "SYNC", 1.0).await;
    assert_eq!(drain(&mut sts), [(f64::from(STS_ERR), UDF_ALARM, INVALID)]);
    put(&db, "TF", "VAL", 1.0).await;
    assert_eq!(drain(&mut sent), [(1.0, LINK_ALARM, INVALID)]);
    assert_eq!(drain(&mut val), [(1.0, NO_ALARM, 0)]);
    let inst = db.get_record("TF").unwrap();
    assert_eq!(inst.read().common.stat, NO_ALARM);
}

fn drain_stamps(rx: &mut EventReader) -> Vec<(f64, WallTime)> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push((
            e.value().as_value().and_then(EpicsValue::to_f64).unwrap(),
            e.time(),
        ));
    }
    out
}

fn time_of(db: &PvDatabase, rec: &str) -> WallTime {
    db.get_record(rec).unwrap().read().common.time
}

/// `caput TZ 0; caput TZ 1` under `camonitor -m v TZ.SENT TZ.VAL`: the put
/// arm's `SENT 1` carries the TIME the record held, the tail's `SENT 1` and
/// `VAL 1` the one `valuePut` stamps after it (`throttleRecord.c:602`).
#[epics_macros_rs::epics_test]
async fn the_put_arm_posts_under_the_previous_stamp() {
    let db = ioc().await;
    put(&db, "TZ", "VAL", 0.0).await;
    let mut sent = subscribe(&db, "TZ", "SENT", EventMask::VALUE);
    let mut val = subscribe(&db, "TZ", "VAL", EventMask::VALUE);
    drain(&mut sent);
    drain(&mut val);
    let before = time_of(&db, "TZ");
    put(&db, "TZ", "VAL", 1.0).await;
    let after = time_of(&db, "TZ");
    assert!(after > before);
    assert_eq!(drain_stamps(&mut sent), [(1.0, before), (1.0, after)]);
    assert_eq!(drain_stamps(&mut val), [(1.0, after)]);
}

/// `field(DRVLH, "10") field(DRVLL, "5")`, `caput TW 3`: `-m a VAL 0
/// <undefined>`. The rejected cycle never reaches `valuePut`, so nothing
/// stamps it.
#[epics_macros_rs::epics_test]
async fn a_rejected_cycle_keeps_the_previous_stamp() {
    let db = ioc().await;
    let mut val = subscribe(&db, "TW", "VAL", EventMask::ALARM);
    drain(&mut val);
    let before = time_of(&db, "TW");
    put(&db, "TW", "VAL", 3.0).await;
    assert_eq!(drain_stamps(&mut val), [(0.0, before)]);
    assert_eq!(time_of(&db, "TW"), before);
}

/// `field(DLY, "1")`, `caput TG 1; caput TG 2`: the queued `VAL 2` carries
/// TG 1's stamp.
#[epics_macros_rs::epics_test]
async fn a_queued_cycle_keeps_the_previous_stamp() {
    let db = ioc().await;
    put(&db, "TG", "VAL", 1.0).await;
    let mut val = subscribe(&db, "TG", "VAL", EventMask::VALUE);
    drain(&mut val);
    let before = time_of(&db, "TG");
    put(&db, "TG", "VAL", 2.0).await;
    assert_eq!(drain_stamps(&mut val), [(2.0, before)]);
    assert_eq!(time_of(&db, "TG"), before);
}

/// `field(DLY, "1") field(SINP, "NUM")`, NUM 7, `caput TS 1; caput TS 2;
/// caput TS.SYNC 1` under `camonitor -m va` of TS.VAL, TS.SENT, TS.STS:
///
/// ```text
/// TS.SENT  T1 2
/// TS.VAL   T1 7
/// TS.STS   T1 Success
/// TS.STS   T1 Success
/// TS.SENT  T2 2
/// ```
///
/// The delay's `valuePut` writes the queued 2, `valueSync` reads SINP after the
/// landed write (`throttleRecord.c:571-572`), and everything up to the tail
/// posts under the previous stamp T1; only the tail's SENT carries the stamp
/// `valuePut` takes at `:602`.
#[epics_macros_rs::epics_test]
async fn a_sync_after_the_write_posts_under_the_previous_stamp() {
    let db = ioc().await;
    let mut sent = subscribe(&db, "TS", "SENT", EventMask::VALUE);
    let mut val = subscribe(&db, "TS", "VAL", EventMask::VALUE);
    let mut sts = subscribe(&db, "TS", "STS", EventMask::VALUE);
    put(&db, "TS", "VAL", 1.0).await;
    put(&db, "TS", "VAL", 2.0).await;
    put(&db, "TS", "SYNC", 1.0).await;
    for rx in [&mut sent, &mut val, &mut sts] {
        drain(rx);
    }
    let before = time_of(&db, "TS");
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1200)).await;
    let after = time_of(&db, "TS");
    assert!(after > before);
    assert_eq!(drain_stamps(&mut sent), [(2.0, before), (2.0, after)]);
    assert_eq!(drain_stamps(&mut val), [(7.0, before)]);
    let suc = f64::from(STS_SUC);
    assert_eq!(drain_stamps(&mut sts), [(suc, before), (suc, before)]);
}

/// `field(SINP, "SS") field(DLY, "1")`, `caput TC 1; caput TC 2; caput
/// TC.SYNC 1`, then the cooldown fires twice:
///
/// ```text
/// timer 1   -m a SENT 2  LINK INVALID     -m v SEVR INVALID
/// timer 2   -m a SENT 2  NO_ALARM         -m v SEVR NO_ALARM
/// ```
///
/// `delayFuncCallback` runs `valuePut` alone, so the LINK alarm its deferred
/// sync raised is committed by `valuePut`'s reset and outlives the cycle.
#[epics_macros_rs::epics_test]
async fn the_cooldown_timer_runs_no_closing_reset() {
    let db = ioc().await;
    let mut sent = subscribe(&db, "TC", "SENT", EventMask::ALARM);
    let mut val = subscribe(&db, "TC", "VAL", EventMask::ALARM);
    put(&db, "TC", "VAL", 1.0).await;
    put(&db, "TC", "VAL", 2.0).await;
    put(&db, "TC", "SYNC", 1.0).await;
    drain(&mut sent);
    drain(&mut val);
    let stat = || db.get_record("TC").unwrap().read().common.stat;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1100)).await;
    assert_eq!(drain(&mut sent), [(2.0, LINK_ALARM, INVALID)]);
    assert_eq!(drain(&mut val), []);
    assert_eq!(stat(), LINK_ALARM);
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1000)).await;
    assert_eq!(drain(&mut sent), [(2.0, NO_ALARM, 0)]);
    assert_eq!(stat(), NO_ALARM);
}

/// `caput TC 1; caput TC 2; caput TC.SYNC 1`, the cooldown fires once, then
/// `caput TC 3` while the delay runs: `-m a VAL 3 NO_ALARM`, AMSG cleared.
/// The timer cycle left `AMSG "field SINP"` committed, and the queued cycle's
/// `process()` reset (`throttleRecord.c:300`) is the one that clears it.
#[epics_macros_rs::epics_test]
async fn a_queued_put_after_the_cooldown_commits_its_closing_reset() {
    let db = ioc().await;
    let mut val = subscribe(&db, "TC", "VAL", EventMask::ALARM);
    put(&db, "TC", "VAL", 1.0).await;
    put(&db, "TC", "VAL", 2.0).await;
    put(&db, "TC", "SYNC", 1.0).await;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1000)).await;
    let common = |db: &PvDatabase| {
        let c = db.get_record("TC").unwrap().read().common.clone();
        (c.stat, c.amsg.to_string())
    };
    assert_eq!(common(&db), (LINK_ALARM, "field SINP".to_string()));
    drain(&mut val);
    put(&db, "TC", "VAL", 3.0).await;
    assert_eq!(drain(&mut val), [(3.0, NO_ALARM, 0)]);
    assert_eq!(common(&db), (NO_ALARM, String::new()));
}

/// `field(OUT, "DST2") field(DLY, "1")`, `caput TD 1; caput TD 2; caput
/// TD.DISA 1`, then 1.5 s: DST2 2, SENT 2. `delayFuncCallback` calls
/// `valuePut` directly (`throttleRecord.c:530-538`), never `dbProcess`, so
/// the DISA put during the delay does not stop the queued write.
#[epics_macros_rs::epics_test]
async fn a_disabled_record_still_writes_its_queued_value_when_the_delay_ends() {
    let db = ioc().await;
    put(&db, "TD", "VAL", 1.0).await;
    put(&db, "TD", "VAL", 2.0).await;
    put(&db, "TD", "DISA", 1.0).await;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1500)).await;
    assert_eq!(db.get_pv("DST2").unwrap(), EpicsValue::Double(2.0));
    assert_eq!(db.get_pv("TD.SENT").unwrap(), EpicsValue::Double(2.0));
}

/// `field(OUT, "DST2") field(DLY, "1") field(FLNK, "CNT")`, `dbpf TL 1;
/// dbpf TL 2`, the cooldown, then the empty fire, and a CONSTANT-OUT `TM`
/// with `FLNK CNT2`, `dbpf TM 4`: softIoc reads CNT 1, 1, 2, 2 and CNT2 0.
/// `recGblFwdLink` is on `valuePut`'s non-CONSTANT arm alone (`:578`).
#[epics_macros_rs::epics_test]
async fn the_cooldown_write_fires_the_forward_link() {
    let db = ioc().await;
    let cnt = |db: &PvDatabase, r: &str| db.get_pv(r).unwrap().to_f64().unwrap();
    put(&db, "TL", "VAL", 1.0).await;
    assert_eq!(cnt(&db, "CNT"), 1.0);
    put(&db, "TL", "VAL", 2.0).await;
    assert_eq!(cnt(&db, "CNT"), 1.0);
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1000)).await;
    assert_eq!(cnt(&db, "CNT"), 2.0);
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1100)).await;
    assert_eq!(cnt(&db, "CNT"), 2.0);
    put(&db, "TM", "VAL", 4.0).await;
    assert_eq!(cnt(&db, "CNT2"), 0.0);
}

/// `throttle(TC){SINP SS, OUT DST2, DLY 1, FLNK K}`, `calc(K){CALC "C+1", INPC
/// K, INPA "TC.SEVR NPP"}`, `caput TC 1; caput TC 2; caput TC.SYNC 1`: softIoc
/// reads K 2, K.A 0 and TC.SEVR INVALID after the first cooldown, K 2 after the
/// second. K.A 0 is `recGblFwdLink` (`:578`) ahead of `valuePut`'s reset, which
/// commits the failed sync's LINK alarm; K 2 after the empty second fire is
/// FLNK on the write arm alone (an FLNK on every fire reads 3).
#[epics_macros_rs::epics_test]
async fn the_cooldown_forward_link_runs_ahead_of_the_reset() {
    let db = ioc().await;
    let get = |db: &PvDatabase, pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    put(&db, "TK", "VAL", 1.0).await;
    assert_eq!(get(&db, "K"), 1.0);
    assert_eq!(get(&db, "K.A"), 3.0);
    put(&db, "TK", "VAL", 2.0).await;
    put(&db, "TK", "SYNC", 1.0).await;
    assert_eq!(get(&db, "K"), 1.0);
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1200)).await;
    assert_eq!(get(&db, "K"), 2.0);
    assert_eq!(get(&db, "K.A"), 0.0);
    assert_eq!(
        db.get_record("TK").unwrap().read().common.sevr as u16,
        INVALID
    );
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1000)).await;
    assert_eq!(get(&db, "K"), 2.0);
}

/// `throttle(TU){OUT DST, DLY 1}`, `ao(W){OUT "TU.UDF NPP"}`, `dbpf TU 1;
/// dbpf TU 2; dbpf W 1`: softIoc reads TU INVALID/UDF after the write fire
/// and still after the empty second fire, `valuePut`'s `checkAlarms`
/// (`throttleRecord.c:604`) running on both arms ahead of the reset.
#[epics_macros_rs::epics_test]
async fn the_cooldown_raises_udf_ahead_of_the_reset() {
    let db = ioc().await;
    let alarm = |db: &PvDatabase| {
        let rec = db.get_record("TU").unwrap();
        let g = rec.read();
        (g.common.stat, g.common.sevr as u16)
    };
    put(&db, "TU", "VAL", 1.0).await;
    put(&db, "TU", "VAL", 2.0).await;
    put(&db, "WU", "VAL", 1.0).await;
    assert_eq!(alarm(&db), (NO_ALARM, NO_ALARM));
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1200)).await;
    assert_eq!(alarm(&db), (UDF_ALARM, INVALID));
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1000)).await;
    assert_eq!(alarm(&db), (UDF_ALARM, INVALID));
}

const CHILD: &str = "HAND_POSTED_TPRO_CHILD";

/// `field(OUT, "DST2") field(DLY, "1") field(TPRO, "1")`, `dbpf TT 1; dbpf TT
/// 2`, then the cooldown writes DST2 2: softIoc prints `dbProcess of 'TT'`
/// for the two puts alone, as `delayFuncCallback` never calls `dbProcess`.
#[test]
fn the_cooldown_timer_prints_no_tpro_line() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_tpro_cooldown",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines = stdout
        .lines()
        .filter(|l| l.ends_with(": dbProcess of 'TT'"))
        .count();
    assert_eq!(lines, 2, "{stdout}");
}

#[epics_macros_rs::epics_test]
async fn child_tpro_cooldown() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let db = ioc().await;
    put(&db, "TT", "VAL", 1.0).await;
    put(&db, "TT", "VAL", 2.0).await;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1500)).await;
    assert_eq!(db.get_pv("DST2").unwrap(), EpicsValue::Double(2.0));
}
