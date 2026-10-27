//! kohzuCtl_soft's boot ends where C's does. The first waitForCmnd takes
//! the connect-time monitors its waitForCmndEnter leaves set, in its order:
//! thetaLimits, the theta readback (thetaMotStopped `!causedMove`), then
//! yOffset. Unlike kohzuCtl, a manual-mode pass that would violate a limit
//! is not rolled back (`kohzuCtl_soft.st:962`). A zero readback holds init
//! in chkMotorLimits.
//!
//! Expected values are a C kohzuIoc's (sequencer 2.2.9, optics, motorSim
//! axes with these limits) after the same boot.
//!
//! kohzuCtl_soft is built `-r`, so every start shares one static block, as
//! C's globals: each case needs a process of its own, which nextest gives it.

use epics_base_rs::runtime::task;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, register_device_row};
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use optics_rs::snl::kohzu_ctl_soft::kohzuCtl_soft;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

const KOHZU_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/kohzuSeq_soft.db");
/// GEOM is left to the program's own `GEOM=1` (`kohzuCtl_soft.st:1`).
const SEQ_MACROS: &str = "P=K:,MONO=m:,M_THETA=th,M_Y=y,M_Z=z";
/// initSequence's radConv (`kohzuCtl_soft.st:527`).
const RAD_CONV: f64 = 57.2958;
// kohzuSeq_soft.db has no SpeedCtrl record for the one kohzuCtl_soft assigns.
const MOTORS: &str = r#"
record(motor, "K:th") { field(HLM, "90") field(LLM, "-10") }
record(motor, "K:y") { field(HLM, "250") field(LLM, "-250") }
record(motor, "K:z") { field(HLM, "250") field(LLM, "-250") }
record(bo, "K:m:SpeedCtrl") {}
record(ao, "K:np") { field(OUT, "K:m:Moving.VAL NPP") }
"#;

/// The booted IOC, scanning for as long as it lives.
struct Kohzu {
    ioc: Arc<PvDatabase>,
    _scan: ScanOwner,
}

impl std::ops::Deref for Kohzu {
    type Target = PvDatabase;

    fn deref(&self) -> &PvDatabase {
        &self.ioc
    }
}

/// Boot with these theta, Y and Z readbacks.
async fn boot(rbv: [f64; 3]) -> Kohzu {
    boot_with(rbv, &[]).await
}

/// [`boot`], with these PVs put before the sequencer connects.
async fn boot_with(rbv: [f64; 3], presets: &[(&str, f64)]) -> Kohzu {
    let macros: HashMap<String, String> = [
        ("P", "K:"),
        ("MONO", "m:"),
        ("M_THETA", "th"),
        ("M_Y", "y"),
        ("M_Z", "z"),
        ("yOffHi", "50"),
        ("yOffLo", "-50"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    // devSoftMotor.dbd's line, which kohzuSeq_soft.db's motors name. The port
    // has no devMotorSoft, so these motors have no device support.
    static SOFT_MOTOR: DevSup = DevSup {
        name: "devMotorSoft",
        choice: "Soft Channel",
        link_type: DbLinkType::Constant,
        dset: Dset::new(8, &[2, 4, 5, 6, 7], false),
    };
    register_device_row("motor", &SOFT_MOTOR);
    let ioc = IocBuilder::new()
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("motor", || Box::new(motor_rs::MotorRecord::new()))
        .db_string(MOTORS, &macros)
        .unwrap()
        .db_file(KOHZU_DB, &macros)
        .unwrap()
        .build_served()
        .await
        .unwrap()
        .0;
    let db = PvDatabase::clone(&ioc);
    // RBV is a readback the loader refuses; no driver moves these motors,
    // so the one set here is the one kohzuCtl_soft reads.
    for (name, rbv) in ["K:th", "K:y", "K:z"].into_iter().zip(rbv) {
        db.get_record(name)
            .unwrap()
            .write()
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
            .unwrap()
            .pos
            .rbv = rbv;
    }
    // iocInit's PINI pass, which seq waits for and st.cmd's presets follow.
    let scan = ScanOwner::start(ioc.clone());
    db.wait_for_pini().await;
    for (pv, v) in presets {
        optics_rs::db_access::DbChannel::new(&db, pv)
            .put_f64_process(*v)
            .await
            .unwrap();
    }
    epics_seq_rs::seq(&kohzuCtl_soft, SEQ_MACROS, 0, &db).expect("kohzuCtl_soft starts");
    Kohzu { ioc, _scan: scan }
}

async fn f64_of(ioc: &PvDatabase, pv: &str) -> f64 {
    ioc.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn string_of(ioc: &PvDatabase, pv: &str) -> String {
    match ioc.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        other => panic!("{pv} is not a string: {other:?}"),
    }
}

/// Wait until SeqMsg1 reads `msg1`, then let the rest of the pass land.
async fn settled(ioc: &PvDatabase, msg1: &str) {
    shown(ioc, msg1).await;
    task::sleep(Duration::from_millis(500)).await;
}

/// Wait until SeqMsg1 reads `msg1`, then until the sequencer waits for a
/// command past its efClears, where a command put is taken.
async fn ready(ioc: &PvDatabase, msg1: &str) {
    shown(ioc, msg1).await;
    idle(ioc, "K:m:OperAck", "K:m:SeqMsg1").await;
}

/// Wait until SeqMsg1 reads `msg1`.
async fn shown(ioc: &PvDatabase, msg1: &str) {
    for _ in 0..200 {
        if string_of(ioc, "K:m:SeqMsg1").await == msg1 {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "SeqMsg1 never read {msg1:?}; last {:?}",
        string_of(ioc, "K:m:SeqMsg1").await
    );
}

async fn assert_near(ioc: &PvDatabase, pv: &str, want: f64) {
    let got = f64_of(ioc, pv).await;
    assert!(
        (got - want).abs() <= 1e-4 * want.abs().max(1.0),
        "{pv}: {got}, C {want}"
    );
}

/// Poll `check` until it passes; panic with its last failure after 10 s.
async fn eventually<F: Future<Output = Result<(), String>>>(mut check: impl FnMut() -> F) {
    let mut last = String::new();
    for _ in 0..200 {
        match check().await {
            Ok(()) => return,
            Err(e) => last = e,
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!("{last}");
}

async fn assert_state(ioc: &PvDatabase, msg2: &str, moving: f64, values: &[(&str, f64)]) {
    eventually(|| state_mismatch(ioc, msg2, moving, values)).await;
}

async fn state_mismatch(
    ioc: &PvDatabase,
    msg2: &str,
    moving: f64,
    values: &[(&str, f64)],
) -> Result<(), String> {
    let got = string_of(ioc, "K:m:SeqMsg2").await;
    if got != msg2 {
        return Err(format!("SeqMsg2 {got:?}, C {msg2:?}"));
    }
    for (pv, want) in [
        ("K:m:Moving", moving),
        ("K:m:Alert", 1.0),
        ("K:m:Mode", 0.0),
    ]
    .into_iter()
    .chain(values.iter().copied())
    {
        let got = f64_of(ioc, pv).await;
        if !(got == want || (got - want).abs() <= 1e-4 * want.abs().max(1.0)) {
            return Err(format!("{pv}: {got}, C {want}"));
        }
    }
    Ok(())
}

/// thetaLimits sets 1..89, the readback pass moves theta to it, and the
/// yOffset pass recomputes Y/Z for it.
#[epics_base_rs::epics_test]
async fn a_readback_inside_the_limits_stands_after_the_connect_time_passes() {
    let ioc = boot([10.0, -17.77, 100.78]).await;
    settled(&ioc, "y offset changed to 17.500000").await;
    assert_state(
        &ioc,
        "Set to Manual Mode",
        0.0,
        &[
            ("K:m:Theta", 10.0),
            ("K:m:Theta.HLM", 89.0),
            ("K:m:Theta.LLM", 1.0),
            ("K:m:E", 11.3853),
            ("K:m:Lambda", 1.08898),
            ("K:m:ThetaSet", 10.0),
            ("K:m:YSet", -17.77),
            ("K:m:ZSet", 100.779),
            ("K:m:ThetaRdbk", 10.0),
            ("K:m:ERdbk", 11.3853),
        ],
    )
    .await;
}

/// Below thetaLo the readback pass clamps the setpoints to 1 deg and Z
/// exceeds its limit; in manual mode nothing is rolled back.
#[epics_base_rs::epics_test]
#[ignore = "needs devMotorSoft (devSoft.cc), reviewer6"]
async fn a_readback_below_theta_lo_keeps_the_clamped_setpoints_in_manual_mode() {
    let ioc = boot([0.5, -17.501, 200.0]).await;
    settled(&ioc, "Z will exceed soft limits").await;
    // The readback pass also reports Z; wait out the yOffset pass.
    task::sleep(Duration::from_millis(500)).await;
    assert_state(
        &ioc,
        "Set to Manual Mode",
        0.0,
        &[
            ("K:m:Theta", 0.5),
            ("K:m:Theta.HLM", 89.0),
            ("K:m:Theta.LLM", 1.0),
            ("K:m:Lambda", 0.109448),
            ("K:m:ThetaSet", 1.0),
            ("K:m:YSet", -17.5027),
            ("K:m:ZSet", 1002.73),
            ("K:m:ThetaRdbk", 0.5),
            ("K:m:ERdbk", 226.555),
        ],
    )
    .await;
    // C's E ends on the thChanged put (113.282) in one run and on the
    // thetaMotStopped one (226.555) in another.
    let e = f64_of(&ioc, "K:m:E").await;
    assert!(
        [113.282, 226.555].iter().any(|c| (e - c).abs() <= 1e-3),
        "K:m:E: {e}"
    );
}

/// A zero readback holds init in chkMotorLimits: the limits are never set
/// and Moving stays raised.
#[epics_base_rs::epics_test]
async fn a_zero_readback_holds_init_in_chk_motor_limits() {
    let ioc = boot([0.0, 0.0, 0.0]).await;
    settled(&ioc, "Theta Readback of 0 not reasonable").await;
    // Past two of chkMotorLimits' 2 s rounds and initSeq=3's 1 s delay.
    task::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        string_of(&ioc, "K:m:SeqMsg1").await,
        "Theta Readback of 0 not reasonable"
    );
    let msg2 = string_of(&ioc, "K:m:SeqMsg2").await;
    assert!(
        msg2 == "Set to Manual Mode" || msg2 == "Modify Motor Positions/Limits",
        "SeqMsg2 {msg2:?}"
    );
    assert_state(
        &ioc,
        &msg2,
        1.0,
        &[
            ("K:m:Theta", 0.0),
            ("K:m:Theta.HLM", 0.0),
            ("K:m:Theta.LLM", 0.0),
            ("K:m:E", f64::INFINITY),
            ("K:m:Lambda", 0.0),
            ("K:m:ThetaSet", 0.0),
            ("K:m:YSet", -17.5),
            ("K:m:ZSet", f64::INFINITY),
            ("K:m:ThetaRdbk", 0.0),
            ("K:m:ERdbk", f64::INFINITY),
        ],
    )
    .await;
}

/// updatePsuedo_soft echoes each motor field monitor on its own, so the
/// echo PVs follow the motors while init holds in chkMotorLimits.
#[epics_base_rs::epics_test]
async fn the_echo_pvs_follow_the_motors_while_init_holds() {
    let ioc = boot([0.0, 0.0, 0.0]).await;
    shown(&ioc, "Theta Readback of 0 not reasonable").await;
    let db = PvDatabase::clone(&ioc);
    optics_rs::db_access::DbChannel::new(&db, "K:z.VELO")
        .put_f64_process(7.0)
        .await
        .unwrap();
    eventually_near(&ioc, "K:m:ZVel", 7.0).await;
    eventually_near(&ioc, "K:m:ThetaDmov", 1.0).await;
}

/// A theta move made outside kohzuCtl_soft posts many readbacks; updateRdbk's
/// efClear(thetaMotRdbk_mon) drops the ones queued while it waited, so C
/// runs one "Only Theta Motor Moved" pass per move (C kohzuIoc oracle).
#[epics_base_rs::epics_test]
async fn a_direct_theta_move_runs_one_only_theta_motor_moved_pass() {
    let ioc = boot([10.0, -17.77, 100.78]).await;
    ready(&ioc, "y offset changed to 17.500000").await;
    let db = PvDatabase::clone(&ioc);
    // Each pass puts the readback to the theta motor, whose HLM refuses it,
    // so theta posts once per pass. SeqMsg1 cannot count passes: a string
    // monitor is by-reference, so a slow reader loses a message the next one
    // overwrites; a double monitor queues each.
    let mut theta = DbSubscription::subscribe(&db, "K:m:Theta").await.unwrap();
    // RBV and DMOV are readbacks a put refuses: store them as the driver
    // would and post the monitor kohzuCtl_soft holds.
    let post = |field: &str, v: f64| {
        let rec = db.get_record("K:th").unwrap();
        let mut inst = rec.write();
        let motor = inst
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
            .unwrap();
        match field {
            "RBV" => motor.pos.rbv = v,
            _ => motor.stat.dmov = i16::from(v != 0.0),
        }
        inst.notify_field(field, EventMask::VALUE | EventMask::LOG);
    };
    post("DMOV", 0.0);
    post("RBV", 50.0);
    // updateRdbk's readback pass shows the first RBV was taken.
    eventually_near(&ioc, "K:m:ThetaRdbk", 50.0).await;
    post("RBV", 70.0);
    post("RBV", 89.5);
    post("DMOV", 1.0);
    task::timeout(Duration::from_secs(10), theta.recv_f64())
        .await
        .expect("no Only Theta Motor Moved pass")
        .unwrap();
    let mut passes = 1;
    // The theta HLM is monitored after its RBV, so its pass runs only once
    // no RBV event is left queued, and every theta post before it is queued.
    optics_rs::db_access::DbChannel::new(&db, "K:th.HLM")
        .put_f64_process(85.0)
        .await
        .unwrap();
    eventually_near(&ioc, "K:m:Theta.HLM", 85.0).await;
    while theta.try_recv_event().is_ok() {
        passes += 1;
    }
    assert_eq!(passes, 1, "Only Theta Motor Moved passes");
}

async fn use_set_and_motor_sets(ioc: &PvDatabase) -> [f64; 4] {
    [
        f64_of(ioc, "K:m:UseSet").await,
        f64_of(ioc, "K:th.SET").await,
        f64_of(ioc, "K:y.SET").await,
        f64_of(ioc, "K:z.SET").await,
    ]
}

/// updateSet (kohzuCtl_soft.st:1320-1369) runs on the connect-time monitors with UseSet first,
/// so UseSet's value wins: a preset motor .SET is put back (C kohzuIoc
/// oracle: th.SET=Set before seq gives all Use).
#[epics_base_rs::epics_test]
async fn use_set_puts_back_a_preset_motor_set_at_connect() {
    let ioc = boot_with([10.0, -17.77, 100.78], &[("K:th.SET", 1.0)]).await;
    sets_reach(&ioc, [0.0; 4]).await;
}

/// The same pass from a preset UseSet (C kohzuIoc oracle: UseSet=Set gives
/// all Set).
#[epics_base_rs::epics_test]
async fn a_preset_use_set_drives_the_motor_sets_at_connect() {
    let ioc = boot_with([10.0, -17.77, 100.78], &[("K:m:UseSet", 1.0)]).await;
    sets_reach(&ioc, [1.0; 4]).await;
}

async fn sets_reach(ioc: &PvDatabase, want: [f64; 4]) {
    eventually(|| async {
        match use_set_and_motor_sets(ioc).await {
            sets if sets == want => Ok(()),
            sets => Err(format!("UseSet and motor SETs {sets:?}, C {want:?}")),
        }
    })
    .await;
}

/// A motor .SET put reaches UseSet, and UseSet the other two motors
/// (C kohzuIoc oracle: y.SET=Set leaves all four Set).
#[epics_base_rs::epics_test]
async fn a_motor_set_put_drives_use_set_and_the_other_motors() {
    // updateSet's connect-time UseSet pass puts back a motor SET put before
    // it: the preset th.SET it resets shows that pass has run.
    let ioc = boot_with([10.0, -17.77, 100.78], &[("K:th.SET", 1.0)]).await;
    sets_reach(&ioc, [0.0; 4]).await;
    let db = PvDatabase::clone(&ioc);
    optics_rs::db_access::DbChannel::new(&db, "K:y.SET")
        .put_f64_process(1.0)
        .await
        .unwrap();
    sets_reach(&ioc, [1.0; 4]).await;
}

/// Poll `pv` until it is within [`assert_near`]'s tolerance of `want`, then
/// assert it.
async fn eventually_near(ioc: &PvDatabase, pv: &str, want: f64) {
    for _ in 0..100 {
        if (f64_of(ioc, pv).await - want).abs() <= 1e-4 * want.abs().max(1.0) {
            break;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    assert_near(ioc, pv, want).await;
}

/// Return with the sequencer idle after an enter. An ack posted during a
/// pass is lost to that pass's opAck = 0, as in C, so it is posted again
/// until one is taken (msg1 " ") and its enter has ended (opAck 0).
async fn idle(db: &PvDatabase, ack: &str, msg1: &str) {
    let ack_ch = optics_rs::db_access::DbChannel::new(db, ack);
    for _ in 0..50 {
        db.get_record(msg1)
            .unwrap()
            .write()
            .record
            .put_field("VAL", EpicsValue::String("-".into()))
            .unwrap();
        ack_ch.put_f64_process(1.0).await.unwrap();
        for _ in 0..20 {
            task::sleep(Duration::from_millis(50)).await;
            if string_of(db, msg1).await == " " && f64_of(db, ack).await == 0.0 {
                return;
            }
        }
    }
    panic!("{ack} never taken");
}

/// Run `posts` once the sequencer has passed waitForCmndEnter's efClears
/// (kohzuCtl_soft.st:612-655): `ack` sends it there, and the enter's `opAck = 0` follows them.
/// Its puts are queued, so no record lock can hold it inside the enter.
async fn post_after_enter(db: &PvDatabase, ack: &str, posts: impl FnOnce(&PvDatabase)) {
    idle(db, ack, "K:m:SeqMsg1").await;
    optics_rs::db_access::DbChannel::new(db, ack)
        .put_f64_process(1.0)
        .await
        .unwrap();
    eventually_near(db, ack, 0.0).await;
    posts(db);
}

/// Post `v` to `pv`'s VAL with no processing.
fn post(db: &PvDatabase, pv: &str, v: EpicsValue) {
    let rec = db.get_record(pv).unwrap();
    let mut inst = rec.write();
    inst.record.put_field("VAL", v).unwrap();
    inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
}

/// E_mon enters eChanged on any change, however small, and C writes
/// autoMode as its monitor arrives (kohzuCtl_soft.st:74-76, no efflag): an
/// E put 5e-13 off its standing value, queued with Mode=Auto, moves Y back
/// from where it was driven (C kohzuIoc oracle: y=5, E+5e-13 → y.VAL
/// -17.77), though E is taken before Mode.
#[epics_base_rs::epics_test]
async fn an_e_put_below_1e_12_off_still_moves() {
    let ioc = boot([10.0, -17.77, 100.78]).await;
    shown(&ioc, "y offset changed to 17.500000").await;
    let db = PvDatabase::clone(&ioc);
    db.get_record("K:y")
        .unwrap()
        .write()
        .record
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
        .unwrap()
        .pos
        .val = 5.0;
    let e = f64_of(&ioc, "K:m:E").await;
    post_after_enter(&db, "K:m:OperAck", |db| {
        post(db, "K:m:Mode", EpicsValue::Enum(1));
        post(db, "K:m:E", EpicsValue::Double(e + 5e-13));
    })
    .await;
    eventually_near(&ioc, "K:y.VAL", -17.77).await;
}

/// A lambda request landing while an E pass puts lambda is taken by its own
/// pass or dropped, never put as the E pass's lambda: E stays the one for
/// ThetaSet.
#[epics_base_rs::epics_test]
async fn a_lambda_request_during_an_e_pass_leaves_e_for_theta_set() {
    let ioc = boot([10.0, -17.77, 100.78]).await;
    ready(&ioc, "y offset changed to 17.500000").await;
    let d = f64_of(&ioc, "K:m:2dSpacing").await;
    post(&ioc, "K:m:E", EpicsValue::Double(12.0));
    eventually_near(&ioc, "K:m:Theta", RAD_CONV * (12.3984244 / 12.0 / d).asin()).await;
    post(&ioc, "K:m:Lambda", EpicsValue::Double(0.9));
    task::sleep(Duration::from_secs(2)).await;
    let theta = f64_of(&ioc, "K:m:ThetaSet").await;
    assert_near(&ioc, "K:m:E", 12.3984244 / (d * (theta / RAD_CONV).sin())).await;
}

/// A theta request landing between thChanged and calcMovements is taken by
/// its own pass or dropped, never moved to as the pass's ThetaSet: E stays
/// the one for ThetaSet.
#[epics_base_rs::epics_test]
async fn a_theta_request_during_a_theta_pass_leaves_e_for_theta_set() {
    let ioc = boot([10.0, -17.77, 100.78]).await;
    ready(&ioc, "y offset changed to 17.500000").await;
    let d = f64_of(&ioc, "K:m:2dSpacing").await;
    post(&ioc, "K:m:Theta", EpicsValue::Double(20.0));
    eventually_near(&ioc, "K:m:Lambda", d * (20.0 / RAD_CONV).sin()).await;
    post(&ioc, "K:m:Theta", EpicsValue::Double(30.0));
    task::sleep(Duration::from_secs(2)).await;
    let theta = f64_of(&ioc, "K:m:ThetaSet").await;
    assert_near(&ioc, "K:m:E", 12.3984244 / (d * (theta / RAD_CONV).sin())).await;
}

/// kohzuMoving is a monitor variable (kohzuCtl_soft.st:83-85): a Moving
/// write that posts nothing (dbPut through an NPP link, dbAccess.c:1416)
/// leaves it 0, so the next waitForCmndEnter does not clear Moving (C
/// kohzuIoc oracle: Moving stays Busy, "kohzuMoving=0; kohzuDone=1").
#[epics_base_rs::epics_test]
async fn a_moving_write_with_no_monitor_is_not_cleared() {
    let ioc = boot([10.0, -17.77, 100.78]).await;
    ready(&ioc, "y offset changed to 17.500000").await;
    let db = PvDatabase::clone(&ioc);
    db.put_record_field_from_ca("K:np", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(f64_of(&ioc, "K:m:Moving").await, 1.0);
    db.put_record_field_from_ca("K:th", "HLM", EpicsValue::Double(80.0))
        .await
        .unwrap();
    // The idle step ends with waitForCmndEnter past the limits pass's own.
    ready(&ioc, "Theta Motor Soft Limits Changed").await;
    assert_eq!(f64_of(&ioc, "K:m:Moving").await, 1.0);
}

/// Readbacks at the yOffset pass's own Y and Z for theta 10 leave every
/// motor nothing to do, so thTime is 0: Put keeps each speed, where C put
/// 0/0, a NaN, into Y's and Z's VELO (`kohzuCtl_soft.st:937-947`).
/// th.VELO stays only if the pass took that thTime = 0 branch.
#[epics_base_rs::epics_test]
async fn a_move_with_nothing_to_do_keeps_the_motor_speeds() {
    let theta = 10.0 / RAD_CONV;
    let rbv = [10.0, -17.5 / theta.cos(), 17.5 / theta.sin()];
    let speeds = [("K:th.VELO", 1.0), ("K:y.VELO", 2.0), ("K:z.VELO", 3.0)];
    let presets: Vec<_> = speeds.into_iter().chain([("K:m:SpeedCtrl", 1.0)]).collect();
    let ioc = boot_with(rbv, &presets).await;
    ready(&ioc, "y offset changed to 17.500000").await;
    optics_rs::db_access::DbChannel::new(&ioc, "K:m:Put")
        .put_f64_process(1.0)
        .await
        .unwrap();
    eventually_near(&ioc, "K:m:Put", 0.0).await;
    for (pv, speed) in speeds {
        assert_eq!(f64_of(&ioc, pv).await, speed, "{pv}");
    }
}

/// Theta at VELO 0, as speed control leaves it after a move without theta
/// travel, takes zero time (`kohzuCtl_soft.st:913-923`): Z is the slowest
/// to theta 20, so it keeps its speed and theta and Y get their travel over
/// Z's time. C took an infinite thTime and put 0 into every VELO.
#[epics_base_rs::epics_test]
async fn a_zero_theta_speed_takes_no_time() {
    let speeds = [("K:th.VELO", 0.0), ("K:y.VELO", 2.0), ("K:z.VELO", 3.0)];
    let presets: Vec<_> = speeds.into_iter().chain([("K:m:SpeedCtrl", 1.0)]).collect();
    let ioc = boot_with([10.0, -17.77, 100.78], &presets).await;
    ready(&ioc, "y offset changed to 17.500000").await;
    let theta = 20.0 / RAD_CONV;
    let (y, z) = (-17.5 / theta.cos(), 17.5 / theta.sin());
    let z_time = (z - 100.78).abs() / 3.0;
    let want = [
        ("K:th.VELO", 10.0 / z_time),
        ("K:y.VELO", (y + 17.77).abs() / z_time),
        ("K:z.VELO", 3.0),
    ];
    for (pv, v) in [("K:m:Mode", 1.0), ("K:m:Theta", 20.0)] {
        optics_rs::db_access::DbChannel::new(&ioc, pv)
            .put_f64_process(v)
            .await
            .unwrap();
    }
    eventually(|| async {
        for (pv, want) in want {
            let got = f64_of(&ioc, pv).await;
            if (got - want).abs() > 1e-4 * want.abs().max(1.0) {
                return Err(format!("{pv}: {got}, want {want}"));
            }
        }
        Ok(())
    })
    .await;
}

async fn put_process(ioc: &PvDatabase, pv: &str, v: f64) {
    optics_rs::db_access::DbChannel::new(ioc, pv)
        .put_f64_process(v)
        .await
        .unwrap();
}

/// Post DMOV on the three motors as their driver would: they have none.
fn post_dmov(db: &PvDatabase, done: bool) {
    for motor in ["K:th", "K:y", "K:z"] {
        let rec = db.get_record(motor).unwrap();
        let mut inst = rec.write();
        inst.record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
            .unwrap()
            .stat
            .dmov = i16::from(done);
        inst.notify_field("DMOV", EventMask::VALUE | EventMask::LOG);
    }
}

const SPEEDS: [(&str, f64); 3] = [("K:th.VELO", 1.0), ("K:y.VELO", 2.0), ("K:z.VELO", 3.0)];

/// Theta's speed for a move from its readback 10 to `theta`: Z, at
/// [`SPEEDS`], is the slowest.
fn th_speed(theta: f64) -> f64 {
    let z = 17.5 / (theta / RAD_CONV).sin();
    (theta - 10.0) / ((z - 100.78).abs() / 3.0)
}

/// Boot at [`SPEEDS`] with speed control into auto mode, the motors not
/// done. thSpeed holds th's speed once ThetaVel, its monitor's mirror,
/// shows it.
async fn boot_speeds() -> Kohzu {
    let presets: Vec<_> = SPEEDS.into_iter().chain([("K:m:SpeedCtrl", 1.0)]).collect();
    let ioc = boot_with([10.0, -17.77, 100.78], &presets).await;
    ready(&ioc, "y offset changed to 17.500000").await;
    eventually_near(&ioc, "K:m:ThetaVel", 1.0).await;
    post_dmov(&ioc, false);
    put_process(&ioc, "K:m:Mode", 1.0).await;
    ioc
}

/// The motors done before the echo of Move's th.VELO put is in get their
/// old speeds back (C defect, kohzuCtl_soft.st:398-408).
#[epics_base_rs::epics_test]
async fn a_move_done_before_its_speed_echo_puts_the_old_speeds_back() {
    let ioc = boot_speeds().await;
    put_process(&ioc, "K:m:Theta", 20.0).await;
    // Z's VAL is Move's last motor put.
    eventually_near(&ioc, "K:z.VAL", 17.5 / (20.0 / RAD_CONV).sin()).await;
    post_dmov(&ioc, true);
    for (pv, speed) in SPEEDS {
        eventually_near(&ioc, pv, speed).await;
    }
}

/// A move started before the echo of the previous move's speed restore is
/// in gets its own speeds (C defect, kohzuCtl_soft.st:398-408).
#[epics_base_rs::epics_test]
async fn a_move_before_the_restore_echo_puts_its_own_speeds() {
    let ioc = boot_speeds().await;
    put_process(&ioc, "K:m:Theta", 20.0).await;
    eventually_near(&ioc, "K:th.VELO", th_speed(20.0)).await;
    // Done once Move's speed echo is in, as a real move would be.
    eventually_near(&ioc, "K:m:ThetaVel", th_speed(20.0)).await;
    post_dmov(&ioc, true);
    eventually_near(&ioc, "K:th.VELO", 1.0).await;
    // DMOV 0 during the restore's readback pass would hold it in updateRdbk.
    post_after_enter(&ioc, "K:m:OperAck", |db| {
        post_dmov(db, false);
        post(db, "K:m:Theta", EpicsValue::Double(30.0));
    })
    .await;
    eventually_near(&ioc, "K:th.VELO", th_speed(30.0)).await;
}

/// UseSet put twice leaves every motor .SET at the second value, though
/// the first pass's .SET echo is in only during the second pass's put
/// (C defect, kohzuCtl_soft.st:458-471).
#[epics_base_rs::epics_test]
async fn use_set_put_twice_leaves_the_motor_sets_at_the_second() {
    let ioc = boot([10.0, -17.77, 100.78]).await;
    ready(&ioc, "y offset changed to 17.500000").await;
    sets_reach(&ioc, [0.0; 4]).await;
    put_process(&ioc, "K:m:UseSet", 1.0).await;
    // The second lands while the first one's pass runs.
    task::sleep(Duration::from_millis(50)).await;
    put_process(&ioc, "K:m:UseSet", 0.0).await;
    // Nothing to wait on for the mirrors to settle: give them time.
    task::sleep(Duration::from_secs(2)).await;
    sets_reach(&ioc, [0.0; 4]).await;
}

/// The same for a motor .SET put twice and the UseSet echo
/// (C defect, kohzuCtl_soft.st:458-471).
#[epics_base_rs::epics_test]
async fn a_motor_set_put_twice_leaves_use_set_at_the_second() {
    let ioc = boot([10.0, -17.77, 100.78]).await;
    ready(&ioc, "y offset changed to 17.500000").await;
    sets_reach(&ioc, [0.0; 4]).await;
    put_process(&ioc, "K:th.SET", 1.0).await;
    task::sleep(Duration::from_millis(50)).await;
    put_process(&ioc, "K:th.SET", 0.0).await;
    task::sleep(Duration::from_secs(2)).await;
    sets_reach(&ioc, [0.0; 4]).await;
}

/// initSequence's escaped block names each motor `{P}{M_x}`
/// (`kohzuCtl_soft.st:540-558`) and publishes the names.
#[epics_base_rs::epics_test]
async fn init_names_the_motors_from_p_and_their_macros() {
    let ioc = boot([10.0, -17.77, 100.78]).await;
    eventually(|| async {
        let mut got = Vec::new();
        for pv in ["K:m:ThetaPv", "K:m:YPv", "K:m:ZPv"] {
            got.push(string_of(&ioc, pv).await);
        }
        if got == ["K:th", "K:y", "K:z"] {
            Ok(())
        } else {
            Err(format!("motor names {got:?}"))
        }
    })
    .await;
}
