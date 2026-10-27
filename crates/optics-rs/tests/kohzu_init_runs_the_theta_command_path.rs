//! kohzuCtl's boot ends where C's does. initSequence's two theta passes
//! (`kohzuCtl.st:587`, `:601-609`) clamp against C's unset thetaHi/thetaLo
//! and are rolled back. The first waitForCmnd then takes the connect-time
//! monitors its waitForCmndEnter leaves set, in its order: thetaLimits, the
//! theta readback (thetaMotStopped `!causedMove`), then yOffset. A zero
//! readback holds init in chkMotorLimits.
//!
//! Expected values are a C kohzuIoc's (sequencer 2.2.9, optics, motorSim
//! axes with these limits) after the same boot.
//!
//! kohzuCtl is built `-r`, so every start shares one static block, as C's
//! globals: each case needs a process of its own, which nextest gives it.

use epics_base_rs::runtime::task;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use optics_rs::snl::kohzu_ctl::kohzuCtl;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

const KOHZU_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/kohzuSeq.db");
/// GEOM is left to the program's own `GEOM=1` (`kohzuCtl.st:1`).
const SEQ_MACROS: &str = "P=K:,M_THETA=th,M_Y=y,M_Z=z";
/// initSequence's radConv (`kohzuCtl.st:528`).
const RAD_CONV: f64 = 57.2958;
const MOTORS: &str = r#"
record(motor, "K:th") { field(HLM, "90") field(LLM, "-10") }
record(motor, "K:y") { field(HLM, "250") field(LLM, "-250") }
record(motor, "K:z") { field(HLM, "250") field(LLM, "-250") }
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
    boot_seq(rbv, presets, SEQ_MACROS).await
}

/// [`boot_with`], with `seq` given this macro definition.
async fn boot_seq(rbv: [f64; 3], presets: &[(&str, f64)], macro_def: &str) -> Kohzu {
    let macros: HashMap<String, String> = [
        ("P", "K:"),
        ("M_THETA", "th"),
        ("M_Y", "y"),
        ("M_Z", "z"),
        ("yOffHi", "50"),
        ("yOffLo", "-50"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
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
    // so the one set here is the one kohzuCtl reads.
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
    epics_seq_rs::seq(&kohzuCtl, macro_def, 0, &db).expect("kohzuCtl starts");
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

/// Wait until SeqMsg1 reads `msg1`.
async fn shown(ioc: &PvDatabase, msg1: &str) {
    for _ in 0..200 {
        if string_of(ioc, "K:KohzuSeqMsg1SI").await == msg1 {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "SeqMsg1 never read {msg1:?}; last {:?}",
        string_of(ioc, "K:KohzuSeqMsg1SI").await
    );
}

/// Wait until SeqMsg1 reads `msg1`, then until the sequencer waits for a
/// command past its efClears, where a command put is taken.
async fn ready(ioc: &PvDatabase, msg1: &str) {
    shown(ioc, msg1).await;
    idle(ioc, "K:KohzuOperAckBO", "K:KohzuSeqMsg1SI").await;
}

fn near(got: f64, want: f64) -> bool {
    (got - want).abs() <= 1e-4 * want.abs().max(1.0)
}

async fn assert_near(ioc: &PvDatabase, pv: &str, want: f64) {
    let got = f64_of(ioc, pv).await;
    assert!(near(got, want), "{pv}: {got}, C {want}");
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
    let got = string_of(ioc, "K:KohzuSeqMsg2SI").await;
    if got != msg2 {
        return Err(format!("SeqMsg2 {got:?}, C {msg2:?}"));
    }
    for (pv, want) in [
        ("K:KohzuMoving", moving),
        ("K:KohzuAlertBO", 1.0),
        ("K:KohzuModeBO", 0.0),
    ]
    .into_iter()
    .chain(values.iter().copied())
    {
        let got = f64_of(ioc, pv).await;
        if !near(got, want) {
            return Err(format!("{pv}: {got}, C {want}"));
        }
    }
    Ok(())
}

/// thetaLimits sets 1..89, the readback pass moves theta to it, and the
/// yOffset pass recomputes Y/Z for it: nothing is rolled back.
#[epics_base_rs::epics_test]
async fn a_readback_inside_the_limits_stands_after_the_connect_time_passes() {
    let ioc = boot([10.0, -17.77, 100.78]).await;
    settled(&ioc, "y offset changed to 17.500000").await;
    assert_state(
        &ioc,
        "Set to Manual Mode",
        0.0,
        &[
            ("K:BraggThetaAO", 10.0),
            ("K:BraggThetaAO.DRVL", 1.0),
            ("K:BraggThetaAO.DRVH", 89.0),
            ("K:BraggEAO", 11.3853),
            ("K:BraggEAO.DRVL", 1.97734),
            ("K:BraggLambdaAO", 1.08898),
            ("K:KohzuThetaSetAO", 10.0),
            ("K:KohzuYSetAO", -17.77),
            ("K:KohzuZSetAO", 100.779),
            ("K:BraggThetaRdbkAO", 10.0),
        ],
    )
    .await;
}

/// Below thetaLo the readback pass is clamped to 1 deg and Z exceeds its
/// limit; the yOffset pass is rolled back the same way.
#[epics_base_rs::epics_test]
async fn a_readback_below_theta_lo_is_rolled_back_to_the_drive_limits() {
    let ioc = boot([0.5, -17.501, 200.0]).await;
    settled(&ioc, "Z will exceed soft limits").await;
    // The readback pass rolls back first; wait out the yOffset pass.
    task::sleep(Duration::from_millis(500)).await;
    assert_state(
        &ioc,
        "Command ignored",
        0.0,
        &[
            ("K:BraggThetaAO", 1.0),
            ("K:BraggThetaAO.DRVL", 1.0),
            ("K:BraggThetaAO.DRVH", 89.0),
            ("K:BraggEAO", 1.97734),
            ("K:BraggEAO.DRVL", 1.97734),
            ("K:BraggLambdaAO", 0.109448),
            ("K:KohzuThetaSetAO", 0.0),
            ("K:KohzuYSetAO", 0.0),
            ("K:KohzuZSetAO", 0.0),
            ("K:BraggThetaRdbkAO", 0.5),
        ],
    )
    .await;
}

/// A zero readback holds init in chkMotorLimits: the limits are never set
/// and the first pass's KohzuMoving stays raised.
#[epics_base_rs::epics_test]
async fn a_zero_readback_holds_init_in_chk_motor_limits() {
    let ioc = boot([0.0, 0.0, 0.0]).await;
    settled(&ioc, "Theta Readback of 0 not reasonable").await;
    // Past two of chkMotorLimits' 2 s rounds and initSeq=3's 1 s delay.
    task::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        string_of(&ioc, "K:KohzuSeqMsg1SI").await,
        "Theta Readback of 0 not reasonable"
    );
    let msg2 = string_of(&ioc, "K:KohzuSeqMsg2SI").await;
    assert!(
        msg2 == "Set to Manual Mode" || msg2 == "Modify Motor Positions/Limits",
        "SeqMsg2 {msg2:?}"
    );
    assert_state(
        &ioc,
        &msg2,
        1.0,
        &[
            ("K:BraggThetaAO", 0.0),
            ("K:BraggThetaAO.DRVL", 0.0),
            ("K:BraggThetaAO.DRVH", 0.0),
            ("K:BraggEAO", 0.0),
            ("K:BraggLambdaAO", 0.0),
            ("K:KohzuThetaSetAO", 0.0),
            ("K:KohzuZSetAO", 0.0),
            ("K:BraggThetaRdbkAO", 0.0),
        ],
    )
    .await;
    assert_eq!(f64_of(&ioc, "K:BraggEAO.DRVL").await, f64::INFINITY);
}

/// A lattice change runs dInputChanged into thChanged: theta stands, E and
/// lambda follow the new 2d, and the manual-mode pass leaves KohzuMoving
/// raised by the setpoint puts.
#[epics_base_rs::epics_test]
async fn a_lattice_change_recomputes_e_and_lambda_for_the_standing_theta() {
    let ioc = boot([10.0, -17.77, 100.78]).await;
    // The OperAck pass blanks SeqMsg2, so the pass's own is the one read.
    ready(&ioc, "y offset changed to 17.500000").await;
    let db = PvDatabase::clone(&ioc);
    optics_rs::db_access::DbChannel::new(&db, "K:BraggHAO")
        .put_f64_process(3.0)
        .await
        .unwrap();
    shown(&ioc, "New d spacing").await;
    let values = [
        ("K:Bragg2dSpacingAO", 3.27503),
        ("K:BraggThetaAO", 10.0),
        ("K:BraggEAO", 21.8012),
        ("K:BraggLambdaAO", 0.568702),
        ("K:BraggEAO.DRVH", 216.918),
        ("K:BraggEAO.DRVL", 3.78632),
        ("K:BraggLambdaAO.DRVH", 3.27453),
        ("K:BraggLambdaAO.DRVL", 0.0571571),
        ("K:KohzuThetaSetAO", 10.0),
        ("K:KohzuYSetAO", -17.77),
        ("K:KohzuZSetAO", 100.779),
        ("K:BraggThetaRdbkAO", 10.0),
        ("K:BraggERdbkAO", 21.8012),
        ("K:KohzuMoving", 1.0),
        ("K:KohzuAlertBO", 0.0),
        ("K:KohzuModeBO", 0.0),
    ];
    eventually(|| async {
        let msg2 = string_of(&ioc, "K:KohzuSeqMsg2SI").await;
        if msg2 != "Set to Manual Mode" {
            return Err(format!("SeqMsg2 {msg2:?}"));
        }
        for (pv, want) in values {
            let got = f64_of(&ioc, pv).await;
            if !near(got, want) {
                return Err(format!("{pv}: {got}, C {want}"));
            }
        }
        Ok(())
    })
    .await;
}

/// updatePsuedo echoes each motor field monitor on its own, so the echo
/// PVs follow the motors while init holds in chkMotorLimits.
#[epics_base_rs::epics_test]
async fn the_echo_pvs_follow_the_motors_while_init_holds() {
    let ioc = boot([0.0, 0.0, 0.0]).await;
    shown(&ioc, "Theta Readback of 0 not reasonable").await;
    let db = PvDatabase::clone(&ioc);
    optics_rs::db_access::DbChannel::new(&db, "K:z.VELO")
        .put_f64_process(7.0)
        .await
        .unwrap();
    eventually_near(&ioc, "K:KohzuZVelAI", 7.0).await;
    eventually_near(&ioc, "K:KohzuThetaDmovBI", 1.0).await;
}

/// A theta move made outside kohzuCtl posts many readbacks; updateRdbk's
/// efClear(thetaMotRdbk_mon) drops the ones queued while it waited, so C
/// runs one "Only Theta Motor Moved" pass per move (C kohzuIoc oracle).
#[epics_base_rs::epics_test]
async fn a_direct_theta_move_runs_one_only_theta_motor_moved_pass() {
    let ioc = boot([10.0, -17.77, 100.78]).await;
    ready(&ioc, "y offset changed to 17.500000").await;
    let db = PvDatabase::clone(&ioc);
    // Each pass sets theta to the readback, which DRVH clamps to 89. SeqMsg1
    // cannot count passes: a string monitor is by-reference, so a slow reader
    // loses a message the next one overwrites; a double monitor queues each.
    let mut theta = DbSubscription::subscribe(&db, "K:BraggThetaAO")
        .await
        .unwrap();
    // RBV and DMOV are readbacks a put refuses: store them as the driver
    // would and post the monitor kohzuCtl holds.
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
    eventually_near(&ioc, "K:BraggThetaRdbkAO", 50.0).await;
    post("RBV", 70.0);
    post("RBV", 89.5);
    post("DMOV", 1.0);
    let mut passes = 0;
    while passes == 0 {
        let v = task::timeout(Duration::from_secs(10), theta.recv_f64())
            .await
            .expect("no Only Theta Motor Moved pass")
            .unwrap();
        passes += usize::from(v == 89.0);
    }
    // The theta HLM is monitored after its RBV, so its pass runs only once
    // no RBV event is left queued, and every theta post before it is queued.
    optics_rs::db_access::DbChannel::new(&db, "K:th.HLM")
        .put_f64_process(85.0)
        .await
        .unwrap();
    eventually_near(&ioc, "K:BraggThetaAO.DRVH", 85.0).await;
    while let Ok(event) = theta.try_recv_event() {
        let v = event.value().as_value().and_then(EpicsValue::to_f64);
        passes += usize::from(v == Some(89.0));
    }
    assert_eq!(passes, 1, "Only Theta Motor Moved passes");
}

async fn use_set_and_motor_sets(ioc: &PvDatabase) -> [f64; 4] {
    [
        f64_of(ioc, "K:KohzuUseSetBO").await,
        f64_of(ioc, "K:th.SET").await,
        f64_of(ioc, "K:y.SET").await,
        f64_of(ioc, "K:z.SET").await,
    ]
}

/// updateSet (kohzuCtl.st:1410-1453) runs on the connect-time monitors with UseSet first,
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
    let ioc = boot_with([10.0, -17.77, 100.78], &[("K:KohzuUseSetBO", 1.0)]).await;
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

/// A tweak past its limit leaves the setpoint but still runs C's
/// eChanged/lChanged/thChanged chain (kohzuCtl.st:755-816), so in auto
/// mode it moves Y back from where it was driven (C kohzuIoc oracle: y=5,
/// E tweak 1000 → y.VAL -17.77; the same for lambda and theta). One case
/// per test process: kohzuCtl is not reentrant, so a second instance would
/// run on the first one's variables.
async fn a_tweak_past_the_limit_moves_to_the_standing_setpoint(
    step: &'static str,
    inc: &'static str,
) {
    let ioc = boot([10.0, -17.77, 100.78]).await;
    ready(&ioc, "y offset changed to 17.500000").await;
    let db = PvDatabase::clone(&ioc);
    let put = |pv: &'static str, v: f64| {
        let db = db.clone();
        async move {
            optics_rs::db_access::DbChannel::new(&db, pv)
                .put_f64_process(v)
                .await
                .unwrap();
        }
    };
    put("K:KohzuModeBO", 1.0).await;
    db.get_record("K:y")
        .unwrap()
        .write()
        .record
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
        .unwrap()
        .pos
        .val = 5.0;
    put(step, 1000.0).await;
    put(inc, 1.0).await;
    eventually(|| async {
        let msg1 = string_of(&ioc, "K:KohzuSeqMsg1SI").await;
        match msg1.ends_with("tweak exceeds limit") {
            true => Ok(()),
            false => Err(format!("{inc}: SeqMsg1 {msg1:?}")),
        }
    })
    .await;
    eventually_near(&ioc, "K:y.VAL", -17.77).await;
}

#[epics_base_rs::epics_test]
async fn an_e_tweak_past_the_limit_moves_to_the_standing_setpoint() {
    a_tweak_past_the_limit_moves_to_the_standing_setpoint("K:BraggETweakAI", "K:BraggEIncBO").await;
}

#[epics_base_rs::epics_test]
async fn a_lambda_tweak_past_the_limit_moves_to_the_standing_setpoint() {
    a_tweak_past_the_limit_moves_to_the_standing_setpoint(
        "K:BraggLambdaTweakAI",
        "K:BraggLambdaIncBO",
    )
    .await;
}

#[epics_base_rs::epics_test]
async fn a_theta_tweak_past_the_limit_moves_to_the_standing_setpoint() {
    a_tweak_past_the_limit_moves_to_the_standing_setpoint(
        "K:BraggThetaTweakAI",
        "K:BraggThetaIncBO",
    )
    .await;
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
/// (kohzuCtl.st:613-658): `ack` sends it there, and the enter's `opAck = 0` follows them.
/// Its puts are queued, so no record lock can hold it inside the enter.
async fn post_after_enter(db: &PvDatabase, ack: &str, posts: impl FnOnce(&PvDatabase)) {
    idle(db, ack, "K:KohzuSeqMsg1SI").await;
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

/// C writes autoMode as its monitor arrives (kohzuCtl.st:78-80, no efflag),
/// so an E put queued with Mode=Auto already sees auto mode and moves Y
/// back from where it was driven, though E is taken before Mode.
#[epics_base_rs::epics_test]
async fn an_e_put_queued_after_auto_mode_moves() {
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
    let e = f64_of(&ioc, "K:BraggEAO").await;
    post_after_enter(&db, "K:KohzuOperAckBO", |db| {
        post(db, "K:KohzuModeBO", EpicsValue::Enum(1));
        post(db, "K:BraggEAO", EpicsValue::Double(e + 1e-6));
    })
    .await;
    eventually_near(&ioc, "K:y.VAL", -17.77).await;
}

/// updateRdbk takes no command while the motors move: the next
/// waitForCmndEnter drops the theta setpoint raised meanwhile, and a theta
/// HLM put waits for the move (C kohzuIoc oracle: theta 20 and th.HLM 85
/// put during the move to 30 leave DRVH 89 until it stops, then th.VAL 30,
/// BraggThetaAO 20, DRVH 85, "Theta Motor Soft Limits Changed", Done).
#[epics_base_rs::epics_test]
async fn a_setpoint_put_during_a_move_is_dropped() {
    let ioc = boot([10.0, -17.77, 100.78]).await;
    ready(&ioc, "y offset changed to 17.500000").await;
    let db = PvDatabase::clone(&ioc);
    let put = |pv: &'static str, v: f64| {
        let db = db.clone();
        async move {
            optics_rs::db_access::DbChannel::new(&db, pv)
                .put_f64_process(v)
                .await
                .unwrap();
        }
    };
    // The test motors have no driver, so the test owns DMOV for all three,
    // posting it as the record would: kohzuCtl reads its monitor.
    let set_dmov = |done: bool| {
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
    };
    let drvh = f64_of(&ioc, "K:BraggThetaAO.DRVH").await;
    put("K:KohzuModeBO", 1.0).await;
    set_dmov(false);
    put("K:BraggThetaAO", 30.0).await;
    eventually_near(&ioc, "K:th.VAL", 30.0).await;
    put("K:BraggThetaAO", 20.0).await;
    put("K:th.HLM", 85.0).await;
    // Nothing to wait on for an absence: give a taken command time to show.
    task::sleep(Duration::from_millis(300)).await;
    assert_near(&ioc, "K:th.VAL", 30.0).await;
    assert_near(&ioc, "K:BraggThetaAO.DRVH", drvh).await;
    set_dmov(true);
    eventually(|| async {
        let msg1 = string_of(&ioc, "K:KohzuSeqMsg1SI").await;
        if msg1 == "Theta Motor Soft Limits Changed" {
            Ok(())
        } else {
            Err(format!("SeqMsg1 {msg1:?}"))
        }
    })
    .await;
    eventually_near(&ioc, "K:BraggThetaAO.DRVH", 85.0).await;
    eventually_near(&ioc, "K:KohzuMoving", 0.0).await;
    assert_near(&ioc, "K:th.VAL", 30.0).await;
    assert_near(&ioc, "K:BraggThetaAO", 20.0).await;
}

/// Readbacks at the yOffset pass's own Y and Z for theta 10 leave every
/// motor nothing to do, so thTime is 0: Move keeps each speed, where C put
/// 0/0, a NaN, into Y's and Z's VELO (`kohzuCtl.st:1032-1042`). th.VELO
/// stays only if the pass took that thTime = 0 branch.
#[epics_base_rs::epics_test]
async fn a_move_with_nothing_to_do_keeps_the_motor_speeds() {
    let theta = 10.0 / RAD_CONV;
    let rbv = [10.0, -17.5 / theta.cos(), 17.5 / theta.sin()];
    let speeds = [("K:th.VELO", 1.0), ("K:y.VELO", 2.0), ("K:z.VELO", 3.0)];
    let ioc = boot_with(rbv, &speeds).await;
    ready(&ioc, "y offset changed to 17.500000").await;
    optics_rs::db_access::DbChannel::new(&ioc, "K:KohzuPutBO")
        .put_f64_process(1.0)
        .await
        .unwrap();
    eventually_near(&ioc, "K:KohzuPutBO", 0.0).await;
    for (pv, speed) in speeds {
        assert_eq!(f64_of(&ioc, pv).await, speed, "{pv}");
    }
}

/// Theta at VELO 0, as speed control leaves it after a move without theta
/// travel, takes zero time (`kohzuCtl.st:1008-1018`): Z is the slowest to
/// theta 20, so it keeps its speed and theta and Y get their travel over
/// Z's time. C took an infinite thTime and put 0 into every VELO.
#[epics_base_rs::epics_test]
async fn a_zero_theta_speed_takes_no_time() {
    let speeds = [("K:th.VELO", 0.0), ("K:y.VELO", 2.0), ("K:z.VELO", 3.0)];
    let ioc = boot_with([10.0, -17.77, 100.78], &speeds).await;
    ready(&ioc, "y offset changed to 17.500000").await;
    let theta = 20.0 / RAD_CONV;
    let (y, z) = (-17.5 / theta.cos(), 17.5 / theta.sin());
    let z_time = (z - 100.78).abs() / 3.0;
    let want = [
        ("K:th.VELO", 10.0 / z_time),
        ("K:y.VELO", (y + 17.77).abs() / z_time),
        ("K:z.VELO", 3.0),
    ];
    for (pv, v) in [("K:KohzuModeBO", 1.0), ("K:BraggThetaAO", 20.0)] {
        optics_rs::db_access::DbChannel::new(&ioc, pv)
            .put_f64_process(v)
            .await
            .unwrap();
    }
    eventually(|| async {
        for (pv, want) in want {
            let got = f64_of(&ioc, pv).await;
            if !near(got, want) {
                return Err(format!("{pv}: {got}, want {want}"));
            }
        }
        Ok(())
    })
    .await;
}

const CHILD: &str = "KOHZU_CTL_DEBUG_CHILD";

/// `test` run again in a child whose stdout is not captured, and the
/// kohzuCtl lines it printed.
fn trace_of(test: &str) -> Vec<String> {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        // libtest's `test <name> ... ` heads the first line, unterminated.
        .filter_map(|l| l.find("kohzuCtl:").map(|i| l[i..].to_string()))
        .collect()
}

/// With kohzuCtlDebug set at boot, the boot passes print C's lines; the
/// rolled-back moveKohzu arm prints none (kohzuCtl.st:1067-1073).
#[epics_base_rs::epics_test]
async fn debug_prints_c_lines() {
    if std::env::var_os(CHILD).is_none() {
        let trace = trace_of("debug_prints_c_lines");
        // C's Linux run races the monitor of waitForCmndEnter's own
        // pvPut(kohzuMoving) (kohzuCtl.st:663) against waitForCmnd's whens.
        // This follows C's delivery on CAC-event above the state set
        // (dbContext.cpp), RTEMS's outcome: it has landed, so kMbusy (:719)
        // wins over the pending yOffset_mon (:731).
        assert_eq!(
            trace,
            [
                "kohzuCtl:initSequence:initSeq=0",
                "kohzuCtl:dInputChanged",
                "kohzuCtl:thChanged",
                "kohzuCtl:calcMovements",
                "kohzuCtl:initSequence:initSeq=1",
                "kohzuCtl:chkMotorLimits",
                "kohzuCtl:chkMotorLimitsExit: OK",
                "kohzuCtl:initSequence:initSeq=2",
                "kohzuCtl:initSequence:initSeq=3",
                "kohzuCtl:thChanged",
                "kohzuCtl:calcMovements",
                "kohzuCtl:waitForCmndEnter:initSeqDone",
                "kohzuCtl:waitForCmndEnter: kohzuMoving=1; kohzuDone=1",
                "kohzuCtl:waitForCmndEnter: asserting 'done'.",
                "kohzuCtl:waitForCmnd:thetaMotXxLim_mon",
                "kohzuCtl:thetaLimits",
                "kohzuCtl:checkDone: done",
                "kohzuCtl:waitForCmndEnter:initSeqDone",
                "kohzuCtl:waitForCmndEnter: kohzuMoving=1; kohzuDone=1",
                "kohzuCtl:waitForCmndEnter: asserting 'done'.",
                "kohzuCtl:waitForCmnd:thetaMotRdbk_mon",
                "kohzuCtl:updateRdbk:*Dmov (done=0,moving=0)",
                "kohzuCtl:updateRdbk: done",
                "kohzuCtl:thetaMotStopped:!causedMove (done=1,moving=0)",
                "kohzuCtl:thChanged",
                "kohzuCtl:calcMovements",
                "kohzuCtl:moveKohzu",
                "kohzuCtl:waitForCmndEnter:initSeqDone",
                "kohzuCtl:waitForCmndEnter: kohzuMoving=1; kohzuDone=1",
                "kohzuCtl:waitForCmndEnter: asserting 'done'.",
                "kohzuCtl:waitForCmnd:kMbusy",
                "kohzuCtl:busyChanged",
                "kohzuCtl:waitForCmndEnter:initSeqDone",
                "kohzuCtl:waitForCmndEnter: kohzuMoving=0; kohzuDone=0",
                "kohzuCtl:waitForCmnd:yOffset_mon",
                "kohzuCtl:calcMovements",
                "kohzuCtl:moveKohzu",
                "kohzuCtl:waitForCmndEnter:initSeqDone",
                "kohzuCtl:waitForCmndEnter: kohzuMoving=0; kohzuDone=0",
            ]
        );
        return;
    }
    let ioc = boot_with([10.0, -17.77, 100.78], &[("K:KohzuCtlDebug", 1.0)]).await;
    settled(&ioc, "y offset changed to 17.500000").await;
}

/// initSequence's escaped block names each motor `{P}{M_x}`
/// (`kohzuCtl.st:541-559`) and publishes the names.
#[epics_base_rs::epics_test]
async fn init_names_the_motors_from_p_and_their_macros() {
    let ioc = boot([10.0, -17.77, 100.78]).await;
    eventually(|| async {
        let mut got = Vec::new();
        for pv in ["K:KohzuThetaPvSI", "K:KohzuYPvSI", "K:KohzuZPvSI"] {
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

/// Wait for the Y drive limits initSequence puts for the geometry `seq`
/// was given in `macro_def`: 0/-35 for geom 1, 60/0 for geom 2
/// (`kohzuCtl.st:560-584`).
async fn y_drive_limits(macro_def: &str, geom: i16, rbv: [f64; 3]) -> Kohzu {
    let ioc = boot_seq(rbv, &[], macro_def).await;
    let want = if geom == 2 { [60.0, 0.0] } else { [0.0, -35.0] };
    eventually(|| async {
        let got = [
            f64_of(&ioc, "K:KohzuYSetAO.DRVH").await,
            f64_of(&ioc, "K:KohzuYSetAO.DRVL").await,
        ];
        if got == want {
            Ok(())
        } else {
            Err(format!("{macro_def}: Y DRVH/DRVL {got:?}, C {want:?}"))
        }
    })
    .await;
    ioc
}

/// geom is 2 only for a GEOM whose first character is '2'.
#[epics_base_rs::epics_test]
async fn no_geom_macro_is_the_programs_geometry_1() {
    y_drive_limits(SEQ_MACROS, 1, [10.0, -17.77, 100.78]).await;
}

#[epics_base_rs::epics_test]
async fn geom_02_is_geometry_1() {
    let macro_def = format!("{SEQ_MACROS},GEOM=02");
    y_drive_limits(&macro_def, 1, [10.0, -17.77, 100.78]).await;
}

/// seqMacParse stores an empty value, so the first character is the NUL.
#[epics_base_rs::epics_test]
async fn an_empty_geom_is_geometry_1() {
    let macro_def = format!("{SEQ_MACROS},GEOM=");
    y_drive_limits(&macro_def, 1, [10.0, -17.77, 100.78]).await;
}

/// Geometry 2 keeps the db's yOffset (`Kohzu_yOffsetAO` DOL 1) and puts
/// Y = yOffset/(2 cos theta), Z = yOffset/(2 sin theta)
/// (`kohzuCtl.st:963-964`; values from that formula, not a C IOC run).
#[epics_base_rs::epics_test]
async fn geom_2_0_moves_y_and_z_by_geometry_2() {
    let theta = 10.0 / RAD_CONV;
    let (y, z) = (1.0 / (2.0 * theta.cos()), 1.0 / (2.0 * theta.sin()));
    let macro_def = format!("{SEQ_MACROS},GEOM=2.0");
    let ioc = y_drive_limits(&macro_def, 2, [10.0, y, z]).await;
    settled(&ioc, "y offset changed to 1.000000").await;
    eventually_near(&ioc, "K:KohzuYSetAO", y).await;
    eventually_near(&ioc, "K:KohzuZSetAO", z).await;
}
