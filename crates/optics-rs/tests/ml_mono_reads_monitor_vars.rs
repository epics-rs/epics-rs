//! ml_monoCtl reads autoMode and ccMode as C's monitored variables, written
//! as each monitor arrives (ml_monoCtl.st:55-64, no efflag), and its
//! updateSet state set ties UseSet to the motor .SET fields (:1354-1419).

use epics_base_rs::runtime::task;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use epics_seq_rs::seq;
use optics_rs::snl::ml_mono_ctl::ml_monoCtl;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const ML_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/ml_monoSeq.db");
const MOTORS: &str = r#"
record(motor, "M:th") { field(HLM, "90") field(LLM, "-10") }
record(motor, "M:th2") { field(HLM, "90") field(LLM, "-10") }
record(motor, "M:y") { field(HLM, "250") field(LLM, "-250") }
record(motor, "M:z") { field(HLM, "500") field(LLM, "-500") }
record(motor, "M:m12") { field(HLM, "90") field(LLM, "-10") }
"#;
const MACROS: &str = "P=M:, M_THETA=th, M_THETA2=th2, M_Y=y, M_Z=z, Y_OFF=35";
/// initSequence's hc and radConv (ml_monoCtl.st:551-552).
const HC: f64 = 12.3984244;
const RAD_CONV: f64 = 57.2958;

async fn f64_of(ioc: &PvDatabase, pv: &str) -> f64 {
    ioc.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn string_of(ioc: &PvDatabase, pv: &str) -> String {
    match ioc.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        v => format!("{v:?}"),
    }
}

async fn boot(rbv: [f64; 4]) -> (Arc<PvDatabase>, ScanOwner) {
    boot_with(rbv, MACROS).await
}

async fn boot_with(rbv: [f64; 4], macro_def: &str) -> (Arc<PvDatabase>, ScanOwner) {
    boot_seq(rbv, &[], macro_def).await
}

/// One start per test process: the program is `-r`, so a second start
/// would share the first one's variables, as in C. `presets` are put
/// before it starts.
async fn boot_seq(
    rbv: [f64; 4],
    presets: &[(&str, f64)],
    macro_def: &str,
) -> (Arc<PvDatabase>, ScanOwner) {
    let (ioc, scan) = ioc_with(rbv, presets).await;
    seq(&ml_monoCtl, macro_def, 0, &ioc).expect("ml_monoCtl starts");
    (ioc, scan)
}

async fn ioc_with(rbv: [f64; 4], presets: &[(&str, f64)]) -> (Arc<PvDatabase>, ScanOwner) {
    let macros: HashMap<String, String> = [("P", "M:")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let ioc = IocBuilder::new()
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("motor", || Box::new(motor_rs::MotorRecord::new()))
        .db_string(MOTORS, &macros)
        .unwrap()
        .db_file(ML_DB, &macros)
        .unwrap()
        .build_served()
        .await
        .unwrap()
        .0;
    // iocInit's PINI pass, which the program's start waits for.
    let scan = ScanOwner::start(ioc.clone());
    ioc.wait_for_pini().await;
    let db = PvDatabase::clone(&ioc);
    for (name, rbv) in ["M:th", "M:th2", "M:y", "M:z"].into_iter().zip(rbv) {
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
    for (pv, v) in presets {
        optics_rs::db_access::DbChannel::new(&db, pv)
            .put_f64_process(*v)
            .await
            .unwrap();
    }
    (ioc, scan)
}

/// Poll until SeqMsg1 reads `msg1`.
async fn settled(ioc: &PvDatabase, msg1: &str) {
    for _ in 0..200 {
        if string_of(ioc, "M:ml_monoSeqMsg1").await == msg1 {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!("SeqMsg1 never read {msg1:?}");
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

/// Run `posts` once the sequencer has passed waitForCmndEnter
/// (ml_monoCtl.st:652-712): OperAck sends it there, and the enter puts
/// `opAck = 0` before its done gate.
/// Its puts are queued, so no record lock can hold it inside the enter.
async fn post_after_enter(db: &PvDatabase, posts: impl FnOnce(&PvDatabase)) {
    idle(db, "M:ml_monoOperAck", "M:ml_monoSeqMsg1").await;
    optics_rs::db_access::DbChannel::new(db, "M:ml_monoOperAck")
        .put_f64_process(1.0)
        .await
        .unwrap();
    wait_near(db, "M:ml_monoOperAck", 0.0).await;
    posts(db);
}

async fn wait_near(ioc: &PvDatabase, pv: &str, want: f64) {
    for _ in 0..100 {
        if (f64_of(ioc, pv).await - want).abs() < 1e-6 {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!("{pv} = {}, want {want}", f64_of(ioc, pv).await);
}

/// Write `pv` and post it as a C client's put would.
fn post(ioc: &PvDatabase, pv: &str, v: EpicsValue) {
    let rec = ioc.get_record(pv).unwrap();
    let mut inst = rec.write();
    inst.record.put_field("VAL", v).unwrap();
    inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
}

/// An E put queued with Mode=Auto already sees auto mode and drives theta,
/// though E is taken before Mode. The boot's own yOffset put wakes the
/// yOffset arm last (C kohzuIoc oracle, y at 35: "y offset changed to
/// 35.000000", then E x1.01 drives th to 9.89998).
#[epics_base_rs::epics_test]
async fn an_e_put_queued_after_auto_mode_moves() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    let e = f64_of(&ioc, "M:ml_monoE").await;
    post_after_enter(&ioc, |db| {
        post(db, "M:ml_monoMode", EpicsValue::Enum(1));
        post(db, "M:ml_monoE", EpicsValue::Double(e * 1.01));
    })
    .await;
    for _ in 0..100 {
        let theta = f64_of(&ioc, "M:ml_monoTheta").await;
        if (theta - 9.89998).abs() < 1e-5 && f64_of(&ioc, "M:th.VAL").await == theta {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "theta {} not driven: th.VAL {}",
        f64_of(&ioc, "M:ml_monoTheta").await,
        f64_of(&ioc, "M:th.VAL").await
    );
}

async fn theta_driven_to(ioc: &PvDatabase, want: f64) {
    for _ in 0..100 {
        let theta = f64_of(ioc, "M:ml_monoTheta").await;
        if (theta - want).abs() < 1e-5 && f64_of(ioc, "M:th.VAL").await == theta {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "theta {} not driven: th.VAL {}",
        f64_of(ioc, "M:ml_monoTheta").await,
        f64_of(ioc, "M:th.VAL").await
    );
}

/// OperAck 0 makes no waitForCmnd clause true (ml_monoCtl.st:800), so the
/// state set stays put and the E put that follows drives theta as in C.
#[epics_base_rs::epics_test]
async fn an_oper_ack_0_leaves_the_e_put_after_it() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    let e = f64_of(&ioc, "M:ml_monoE").await;
    post_after_enter(&ioc, |db| {
        post(db, "M:ml_monoMode", EpicsValue::Enum(1));
        post(db, "M:ml_monoOperAck", EpicsValue::Enum(0));
    })
    .await;
    task::sleep(Duration::from_millis(30)).await;
    post(&ioc, "M:ml_monoE", EpicsValue::Double(e * 1.01));
    theta_driven_to(&ioc, 9.89998).await;
}

/// C kohzuIoc oracle, th at 0: chkMotorLimits holds the init sequence,
/// "Theta Readback of 0 not reasonable" every 2 s until the readback moves; then "All motors
/// within limits" and the sequence runs on to its yOffset pass.
#[epics_base_rs::epics_test]
async fn a_theta_readback_of_0_holds_init_in_chk_motor_limits() {
    let (ioc, _scan) = boot([0.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "Theta Readback of 0 not reasonable").await;
    // Two chkMotorLimits rounds: msg2 reads "Modify Motor Positions/Limits"
    // only until the next round puts "Set to Manual Mode" right after it.
    task::sleep(Duration::from_millis(4500)).await;
    assert_eq!(
        string_of(&ioc, "M:ml_monoSeqMsg1").await,
        "Theta Readback of 0 not reasonable"
    );
    assert_eq!(f64_of(&ioc, "M:ml_monoAlert").await, 1.0);

    let db = PvDatabase::clone(&ioc);
    {
        let rec = db.get_record("M:th").unwrap();
        let mut inst = rec.write();
        inst.record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
            .unwrap()
            .pos
            .rbv = 10.0;
        inst.notify_field("RBV", EventMask::VALUE | EventMask::LOG);
    }
    settled(&ioc, "y offset changed to 35.000000").await;
}

/// The yOffset initSequence puts, `atof(Y_OFF)` or 35 outside [1, 60]
/// (ml_monoCtl.st:604-614). updatePsuedo puts the y readback on its first
/// monitor (:1295-1298), so y reads 20, which no case gives, and of the two
/// posts the one that is not 20 is initSequence's; MDEL -1 posts it when it
/// equals the DOL's 1. updatePsuedo writing yOffset between initSequence's
/// assignment and its put, as C's threads can, no longer turns that put
/// into a second 20. th at 0 then holds the sequence in chkMotorLimits.
async fn y_offset_is(y_off: &str, want: f64) {
    let mdel = [("M:ml_mono_yOffset.MDEL", -1.0)];
    let (ioc, _scan) = ioc_with([0.0, 10.0, 20.0, 100.0], &mdel).await;
    let mut posts = DbSubscription::subscribe(&ioc, "M:ml_mono_yOffset")
        .await
        .unwrap();
    let macro_def = format!("P=M:, M_THETA=th, M_THETA2=th2, M_Y=y, M_Z=z, Y_OFF={y_off}");
    seq(&ml_monoCtl, &macro_def, 0, &ioc).expect("ml_monoCtl starts");
    settled(&ioc, "Theta Readback of 0 not reasonable").await;
    let mut seen = Vec::new();
    while let Ok(Some(v)) = task::timeout(Duration::from_millis(500), posts.recv_f64()).await {
        seen.push(v);
    }
    assert!(
        seen.len() == 2 && seen.contains(&20.0) && seen.contains(&want),
        "Y_OFF={y_off:?} posted {seen:?}, want {want} and 20"
    );
}

#[epics_base_rs::epics_test]
async fn a_y_off_below_1_is_35() {
    y_offset_is("0.5", 35.0).await;
}

#[epics_base_rs::epics_test]
async fn a_y_off_of_1_is_kept() {
    y_offset_is("1", 1.0).await;
}

#[epics_base_rs::epics_test]
async fn a_y_off_of_60_is_kept() {
    y_offset_is("60", 60.0).await;
}

#[epics_base_rs::epics_test]
async fn a_y_off_above_60_is_35() {
    y_offset_is("100", 35.0).await;
}

#[epics_base_rs::epics_test]
async fn y_off_is_read_by_atof() {
    y_offset_is("40mm", 40.0).await;
}

#[epics_base_rs::epics_test]
async fn y_off_atof_skips_a_leading_space() {
    y_offset_is(" 4e1", 40.0).await;
}

#[epics_base_rs::epics_test]
async fn a_y_off_with_no_number_is_35() {
    y_offset_is("mm", 35.0).await;
}

/// Store `rbv` as M:y's readback and post it, as the driver would.
fn post_y_rbv(ioc: &PvDatabase, rbv: f64) {
    let rec = ioc.get_record("M:y").unwrap();
    let mut inst = rec.write();
    inst.record
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
        .unwrap()
        .pos
        .rbv = rbv;
    inst.notify_field("RBV", EventMask::VALUE | EventMask::LOG);
}

/// updatePsuedo puts each y readback as yOffset (ml_monoCtl.st:1372-1375).
/// The previous readback's yOffset event landing between a pass's
/// assignment and its put must not be put in the new readback's place. th
/// at 0 holds the main state set in chkMotorLimits, whose waitForCmndEnter
/// would put the readback again.
#[epics_base_rs::epics_test]
async fn a_y_readback_is_put_over_the_previous_ones_event() {
    let (ioc, _scan) = boot([0.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "Theta Readback of 0 not reasonable").await;
    // The boot's own yOffset puts and their events land first.
    task::sleep(Duration::from_secs(1)).await;
    post_y_rbv(&ioc, 40.0);
    wait_near(&ioc, "M:ml_mono_yOffset", 40.0).await;
    post_y_rbv(&ioc, 45.0);
    wait_near(&ioc, "M:ml_mono_yOffset", 45.0).await;
}

/// waitForCmndEnter puts the y readback as yOffset (ml_monoCtl.st:660-662),
/// 0.1 s after the opAck arm. A yOffset post landing between that
/// assignment and its put must not be put in the readback's place; the
/// next enter, after the post's yOffset arm, would put the readback again.
#[epics_base_rs::epics_test]
async fn a_y_offset_post_during_the_enter_leaves_the_readback() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    idle(&ioc, "M:ml_monoOperAck", "M:ml_monoSeqMsg1").await;
    // A post before the opAck arm would wake the yOffset arm ahead of it.
    ioc.get_record("M:ml_monoSeqMsg1")
        .unwrap()
        .write()
        .record
        .put_field("VAL", EpicsValue::String("-".into()))
        .unwrap();
    optics_rs::db_access::DbChannel::new(&ioc, "M:ml_monoOperAck")
        .put_f64_process(1.0)
        .await
        .unwrap();
    settled(&ioc, " ").await;
    post(&ioc, "M:ml_mono_yOffset", EpicsValue::Double(50.0));
    wait_near(&ioc, "M:ml_monoOperAck", 0.0).await;
    assert_eq!(f64_of(&ioc, "M:ml_mono_yOffset").await, 35.0);
}

/// C's TwoD from the standing D and Order (ml_monoCtl.st:1510).
async fn two_d(ioc: &PvDatabase) -> f64 {
    (2.0 * f64_of(ioc, "M:ml_monoD").await) / f64_of(ioc, "M:ml_monoOrder").await
}

/// A D whose 2d is under the standing lambda: dInputChanged's DRVH and
/// DRVL puts clamp ml_monoLambda and ml_monoE, which post the clamped
/// values while thChanged recomputes both for the standing theta.
#[epics_base_rs::epics_test]
async fn a_lattice_change_past_the_drive_limits_recomputes_e_and_lambda_for_theta() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    idle(&ioc, "M:ml_monoOperAck", "M:ml_monoSeqMsg1").await;
    // 2d 8, under lambda 8.42, and E 1.47 under the new DRVL 1.55.
    optics_rs::db_access::DbChannel::new(&ioc, "M:ml_monoD")
        .put_f64_process(4.0)
        .await
        .unwrap();
    settled(&ioc, "New effective d spacing").await;
    let lambda = two_d(&ioc).await * (10.0 / RAD_CONV).sin();
    wait_near(&ioc, "M:ml_monoLambda", lambda).await;
    wait_near(&ioc, "M:ml_monoE", HC / lambda).await;
    wait_near(&ioc, "M:ml_monoThetaSet", 10.0).await;
}

/// A lambda request landing while an E pass puts lambda is taken by its own
/// pass or dropped, never put as the E pass's lambda: E stays the one for
/// ThetaSet.
#[epics_base_rs::epics_test]
async fn a_lambda_request_during_an_e_pass_leaves_e_for_theta_set() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    idle(&ioc, "M:ml_monoOperAck", "M:ml_monoSeqMsg1").await;
    let d = two_d(&ioc).await;
    post(&ioc, "M:ml_monoE", EpicsValue::Double(12.0));
    wait_near(&ioc, "M:ml_monoTheta", RAD_CONV * (HC / 12.0 / d).asin()).await;
    post(&ioc, "M:ml_monoLambda", EpicsValue::Double(0.9));
    task::sleep(Duration::from_secs(2)).await;
    let theta = f64_of(&ioc, "M:ml_monoThetaSet").await;
    wait_near(&ioc, "M:ml_monoE", HC / (d * (theta / RAD_CONV).sin())).await;
}

/// A theta request landing between thChanged and calcMovements is taken by
/// its own pass or dropped, never moved to as the pass's ThetaSet: E stays
/// the one for ThetaSet.
#[epics_base_rs::epics_test]
async fn a_theta_request_during_a_theta_pass_leaves_e_for_theta_set() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    idle(&ioc, "M:ml_monoOperAck", "M:ml_monoSeqMsg1").await;
    let d = two_d(&ioc).await;
    post(&ioc, "M:ml_monoTheta", EpicsValue::Double(20.0));
    wait_near(&ioc, "M:ml_monoLambda", d * (20.0 / RAD_CONV).sin()).await;
    post(&ioc, "M:ml_monoTheta", EpicsValue::Double(30.0));
    task::sleep(Duration::from_secs(2)).await;
    let theta = f64_of(&ioc, "M:ml_monoThetaSet").await;
    wait_near(&ioc, "M:ml_monoE", HC / (d * (theta / RAD_CONV).sin())).await;
}

/// A motor macro the command line leaves out takes the program's default
/// (ml_monoCtl.st:1), which initSequence names in `{P}ml_monoTheta2Pv`.
#[epics_base_rs::epics_test]
async fn the_theta2_motor_defaults_to_m12() {
    let (ioc, _scan) = boot_with([10.0, 10.0, 35.0, 100.0], "P=M:, M_THETA=th, M_Y=y, M_Z=z").await;
    for _ in 0..100 {
        if string_of(&ioc, "M:ml_monoTheta2Pv").await == "M:m12" {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "theta2MotName is {:?}",
        string_of(&ioc, "M:ml_monoTheta2Pv").await
    );
}

/// E x1.01 in auto mode from th at 10 and z at 100 with these motor
/// speeds: th, th2 and z .VELO each post what `want` gives for theta's and
/// Z's travel. Watched by monitor, since thetaMotStopped puts the old
/// speeds back once the motors are done.
async fn e_move_speeds(presets: &[(&str, f64)], want: impl Fn(f64, f64) -> [f64; 3]) {
    let (ioc, _scan) = boot_seq([10.0, 10.0, 35.0, 100.0], presets, MACROS).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    let pvs = ["M:th.VELO", "M:th2.VELO", "M:z.VELO"];
    let mut velo = Vec::new();
    for pv in pvs {
        velo.push(DbSubscription::subscribe(&ioc, pv).await.unwrap());
    }
    let e = f64_of(&ioc, "M:ml_monoE").await;
    post_after_enter(&ioc, |db| {
        post(db, "M:ml_monoMode", EpicsValue::Enum(1));
        post(db, "M:ml_monoE", EpicsValue::Double(e * 1.01));
    })
    .await;
    theta_driven_to(&ioc, 9.89998).await;
    let want = want(
        (f64_of(&ioc, "M:ml_monoTheta").await - 10.0).abs(),
        (f64_of(&ioc, "M:ml_monoZSet").await - 100.0).abs(),
    );
    for ((pv, mut velo), want) in pvs.into_iter().zip(velo).zip(want) {
        let mut posts = Vec::new();
        while !posts.iter().any(|v: &f64| (v - want).abs() < 1e-9) {
            match task::timeout(Duration::from_secs(5), velo.recv_f64()).await {
                Ok(Some(v)) => posts.push(v),
                _ => panic!("{pv} posted {posts:?}, want {want}"),
            }
        }
    }
}

/// Theta at VELO 0, as speed control leaves it after a move without theta
/// travel, takes zero time (ml_monoCtl.st:1065-1068): Z is the slower, so it
/// keeps its speed and theta gets its travel over Z's time. C took an
/// infinite thTime and put 0 into every VELO.
#[epics_base_rs::epics_test]
async fn a_zero_theta_speed_takes_no_time() {
    let speeds = [("M:th.VELO", 0.0), ("M:th2.VELO", 0.0), ("M:z.VELO", 3.0)];
    e_move_speeds(&speeds, |th, z| {
        let th = th / (z / 3.0);
        [th, th, 3.0]
    })
    .await;
}

/// Z at VELO 0 takes zero time the same way: theta is the slower, so it
/// keeps its speed and Z gets its travel over theta's time. C took an
/// infinite zTime and put 0 into theta's VELO.
#[epics_base_rs::epics_test]
async fn a_zero_z_speed_takes_no_time() {
    let speeds = [("M:th.VELO", 0.5), ("M:th2.VELO", 0.5), ("M:z.VELO", 0.0)];
    e_move_speeds(&speeds, |th, z| [0.5, 0.5, z / (th / 0.5)]).await;
}

/// Readbacks at the yOffset pass's own theta and Z leave both motors
/// nothing to do, so thTime is 0: Put keeps Z's speed, where C put 0/0, a
/// NaN, into its VELO (ml_monoCtl.st:1075-1084). th.VELO stays only if the
/// pass took that thTime >= zTime branch.
#[epics_base_rs::epics_test]
async fn a_put_with_nothing_to_do_keeps_the_motor_speeds() {
    let z = 35.0 / (2.0 * 10.0 / 57.2958_f64).tan();
    let speeds = [("M:th.VELO", 0.5), ("M:th2.VELO", 0.5), ("M:z.VELO", 3.0)];
    let (ioc, _scan) = boot_seq([10.0, 10.0, 35.0, z], &speeds, MACROS).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    let mut velo = Vec::new();
    for (pv, _) in speeds {
        velo.push(DbSubscription::subscribe(&ioc, pv).await.unwrap());
    }
    put_process(&ioc, "M:ml_monoPut", 1.0).await;
    wait_near(&ioc, "M:ml_monoPut", 0.0).await;
    for ((pv, speed), mut velo) in speeds.into_iter().zip(velo) {
        let mut posts = Vec::new();
        while let Ok(Some(v)) = task::timeout(Duration::from_millis(500), velo.recv_f64()).await {
            posts.push(v);
        }
        assert!(
            posts.iter().all(|v| *v == speed),
            "{pv} posted {posts:?}, want {speed}"
        );
    }
}

async fn set_flags(ioc: &PvDatabase) -> [f64; 4] {
    let mut flags = [0.0; 4];
    for (flag, pv) in flags
        .iter_mut()
        .zip(["M:th.SET", "M:th2.SET", "M:z.SET", "M:y.SET"])
    {
        *flag = f64_of(ioc, pv).await;
    }
    flags
}

async fn put_process(ioc: &PvDatabase, pv: &str, v: f64) {
    optics_rs::db_access::DbChannel::new(ioc, pv)
        .put_f64_process(v)
        .await
        .unwrap();
}

/// C kohzuIoc oracle: UseSet 1 sets th/th2/z .SET and leaves y alone; a
/// th2.SET 0 then takes UseSet, and through it th and z, back to Use.
#[epics_base_rs::epics_test]
async fn use_set_and_the_motor_set_fields_follow_each_other() {
    let (ioc, _scan) = boot([10.0, 10.0, 0.0, 100.0]).await;
    put_process(&ioc, "M:ml_monoUseSet", 1.0).await;
    let mut flags = [0.0; 4];
    for _ in 0..100 {
        flags = set_flags(&ioc).await;
        if flags == [1.0, 1.0, 1.0, 0.0] {
            break;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        flags,
        [1.0, 1.0, 1.0, 0.0],
        "th, th2, z, y .SET after UseSet 1"
    );

    put_process(&ioc, "M:th2.SET", 0.0).await;
    for _ in 0..100 {
        flags = set_flags(&ioc).await;
        if f64_of(&ioc, "M:ml_monoUseSet").await == 0.0 && flags == [0.0; 4] {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "UseSet {} .SET {flags:?} after th2.SET 0",
        f64_of(&ioc, "M:ml_monoUseSet").await
    );
}

/// Post DMOV on the three moved motors as their driver would: they have
/// none.
fn post_dmov(db: &PvDatabase, done: bool) {
    for motor in ["M:th", "M:th2", "M:z"] {
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

const SPEEDS: [(&str, f64); 3] = [("M:th.VELO", 1.0), ("M:th2.VELO", 1.0), ("M:z.VELO", 3.0)];

/// Z for `theta` at the boot's yOffset 35.
fn z_of(theta: f64) -> f64 {
    35.0 / (2.0 * theta / RAD_CONV).tan()
}

/// Theta's speed for a move from its readback 10 to `theta`: Z, at
/// [`SPEEDS`] from 100, is the slower.
fn th_speed(theta: f64) -> f64 {
    (theta - 10.0) / ((z_of(theta) - 100.0).abs() / 3.0)
}

/// Boot at [`SPEEDS`], the motors not done, and move theta to 20 in auto
/// mode. thSpeed holds th's speed once ThetaVel, its monitor's mirror,
/// shows it.
async fn move_at_speeds() -> (Arc<PvDatabase>, ScanOwner) {
    let (ioc, scan) = boot_seq([10.0, 10.0, 35.0, 100.0], &SPEEDS, MACROS).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    wait_near(&ioc, "M:ml_monoThetaVel", 1.0).await;
    post_after_enter(&ioc, |db| {
        post_dmov(db, false);
        post(db, "M:ml_monoMode", EpicsValue::Enum(1));
        post(db, "M:ml_monoTheta", EpicsValue::Double(20.0));
    })
    .await;
    (ioc, scan)
}

/// The motors done before the echo of move_ml_mono's th.VELO put is in get
/// their old speeds back (C defect, ml_monoCtl.st:382-392).
#[epics_base_rs::epics_test]
async fn a_move_done_before_its_speed_echo_puts_the_old_speeds_back() {
    let (ioc, _scan) = move_at_speeds().await;
    // Z's VAL is the move's last motor put.
    wait_near(&ioc, "M:z.VAL", z_of(20.0)).await;
    post_dmov(&ioc, true);
    for (pv, speed) in SPEEDS {
        wait_near(&ioc, pv, speed).await;
    }
}

/// A move started before the echo of the previous move's speed restore is
/// in gets its own speeds (C defect, ml_monoCtl.st:382-392).
#[epics_base_rs::epics_test]
async fn a_move_before_the_restore_echo_puts_its_own_speeds() {
    let (ioc, _scan) = move_at_speeds().await;
    wait_near(&ioc, "M:th.VELO", th_speed(20.0)).await;
    // Done once the move's speed echo is in, as a real move would be.
    wait_near(&ioc, "M:ml_monoThetaVel", th_speed(20.0)).await;
    post_dmov(&ioc, true);
    wait_near(&ioc, "M:th.VELO", 1.0).await;
    // DMOV 0 during the restore's readback pass would hold it in updateRdbk.
    post_after_enter(&ioc, |db| {
        post_dmov(db, false);
        post(db, "M:ml_monoTheta", EpicsValue::Double(30.0));
    })
    .await;
    wait_near(&ioc, "M:th.VELO", th_speed(30.0)).await;
}

/// Wait for an event of `sub` at `want`.
async fn shows(sub: &mut DbSubscription, want: f64) {
    loop {
        let got = task::timeout(Duration::from_secs(10), sub.recv_f64())
            .await
            .unwrap_or_else(|_| panic!("no event at {want}"))
            .unwrap();
        if (got - want).abs() <= 1e-4 * want.abs().max(1.0) {
            return;
        }
    }
}

/// The next move puts back the speeds VELO holds, though this move's speed
/// echo is in only after its restore (C defect, ml_monoCtl.st:1053-1059).
#[epics_base_rs::epics_test]
async fn a_speed_echo_after_its_restore_leaves_the_next_move_the_old_speeds() {
    let (ioc, _scan) = move_at_speeds().await;
    let mut vel = DbSubscription::subscribe(&ioc, "M:ml_monoThetaVel")
        .await
        .unwrap();
    // The echo may be in before the subscription.
    let echoed = (f64_of(&ioc, "M:ml_monoThetaVel").await - th_speed(20.0)).abs() < 1e-6;
    wait_near(&ioc, "M:th.VELO", th_speed(20.0)).await;
    // The move takes its time.
    task::sleep(Duration::from_millis(1500)).await;
    post_dmov(&ioc, true);
    wait_near(&ioc, "M:th.VELO", 1.0).await;
    // The next move starts once the speed echo is in.
    if !echoed {
        shows(&mut vel, th_speed(20.0)).await;
    }
    post_after_enter(&ioc, |db| {
        post_dmov(db, false);
        post(db, "M:ml_monoTheta", EpicsValue::Double(30.0));
    })
    .await;
    // Z's VAL is the move's last motor put.
    wait_near(&ioc, "M:z.VAL", z_of(30.0)).await;
    post_dmov(&ioc, true);
    for (pv, speed) in SPEEDS {
        wait_near(&ioc, pv, speed).await;
    }
}

async fn use_set_and_flags_reach(ioc: &PvDatabase, use_set: f64, flags: [f64; 4]) {
    for _ in 0..200 {
        if f64_of(ioc, "M:ml_monoUseSet").await == use_set && set_flags(ioc).await == flags {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "UseSet {} .SET {:?}, want {use_set} {flags:?}",
        f64_of(ioc, "M:ml_monoUseSet").await,
        set_flags(ioc).await
    );
}

/// UseSet put twice leaves th, th2 and z .SET at the second value, though
/// the first pass's .SET echo is in only during the second pass's put
/// (C defect, ml_monoCtl.st:445-459).
#[epics_base_rs::epics_test]
async fn use_set_put_twice_leaves_the_motor_sets_at_the_second() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    use_set_and_flags_reach(&ioc, 0.0, [0.0; 4]).await;
    put_process(&ioc, "M:ml_monoUseSet", 1.0).await;
    // The second lands while the first one's pass runs.
    task::sleep(Duration::from_millis(50)).await;
    put_process(&ioc, "M:ml_monoUseSet", 0.0).await;
    // Nothing to wait on for the mirrors to settle: give them time.
    task::sleep(Duration::from_secs(2)).await;
    use_set_and_flags_reach(&ioc, 0.0, [0.0; 4]).await;
}

/// The same for a motor .SET put twice and the UseSet echo
/// (C defect, ml_monoCtl.st:445-459).
#[epics_base_rs::epics_test]
async fn a_motor_set_put_twice_leaves_use_set_at_the_second() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    use_set_and_flags_reach(&ioc, 0.0, [0.0; 4]).await;
    put_process(&ioc, "M:th.SET", 1.0).await;
    task::sleep(Duration::from_millis(50)).await;
    put_process(&ioc, "M:th.SET", 0.0).await;
    task::sleep(Duration::from_secs(2)).await;
    use_set_and_flags_reach(&ioc, 0.0, [0.0; 4]).await;
}

/// UseSet off again before the first pass's .SET echoes are in stays off:
/// the late echoes are not mirrored back (C defect, ml_monoCtl.st:1441-1445).
#[epics_base_rs::epics_test]
async fn a_late_motor_set_echo_is_not_mirrored_back_to_use_set() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    use_set_and_flags_reach(&ioc, 0.0, [0.0; 4]).await;
    put_process(&ioc, "M:ml_monoUseSet", 1.0).await;
    use_set_and_flags_reach(&ioc, 1.0, [1.0, 1.0, 1.0, 0.0]).await;
    put_process(&ioc, "M:ml_monoUseSet", 0.0).await;
    let mut sub = DbSubscription::subscribe(&ioc, "M:ml_monoUseSet")
        .await
        .unwrap();
    stays(&mut sub, 0.0).await;
    use_set_and_flags_reach(&ioc, 0.0, [0.0; 4]).await;
}

/// The same for th.SET off again before the UseSet echo is in
/// (C defect, ml_monoCtl.st:1441-1445).
#[epics_base_rs::epics_test]
async fn a_late_use_set_echo_is_not_mirrored_back_to_the_motor_set() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    use_set_and_flags_reach(&ioc, 0.0, [0.0; 4]).await;
    put_process(&ioc, "M:th.SET", 1.0).await;
    wait_near(&ioc, "M:ml_monoUseSet", 1.0).await;
    put_process(&ioc, "M:th.SET", 0.0).await;
    let mut sub = DbSubscription::subscribe(&ioc, "M:th.SET").await.unwrap();
    stays(&mut sub, 0.0).await;
    use_set_and_flags_reach(&ioc, 0.0, [0.0; 4]).await;
}

/// Fail on an event of `sub` other than `want` for three seconds.
async fn stays(sub: &mut DbSubscription, want: f64) {
    let end = std::time::Instant::now() + Duration::from_secs(3);
    while let Ok(got) = task::timeout(
        end.saturating_duration_since(std::time::Instant::now()),
        sub.recv_f64(),
    )
    .await
    {
        assert_eq!(got.unwrap(), want, "a late echo was mirrored back");
    }
}

/// Put goes where the last pass calculated, though the previous pass's
/// Desired echoes are in only after it (C defect, ml_monoCtl.st:278-282).
#[epics_base_rs::epics_test]
async fn a_late_desired_echo_leaves_the_move_at_the_last_pass() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    idle(&ioc, "M:ml_monoOperAck", "M:ml_monoSeqMsg1").await;
    put_process(&ioc, "M:ml_monoTheta", 20.0).await;
    wait_near(&ioc, "M:ml_monoThetaSet", 20.0).await;
    // The 20 pass's echoes land between the 30 pass and Put.
    task::sleep(Duration::from_secs(1)).await;
    put_process(&ioc, "M:ml_monoTheta", 30.0).await;
    wait_near(&ioc, "M:ml_monoThetaSet", 30.0).await;
    task::sleep(Duration::from_millis(1500)).await;
    moves_to(&ioc, 30.0).await;
}

/// A Desired edit before Put is dropped: Put goes where the pass
/// calculated, where C took the edit (C defect, ml_monoCtl.st:278-282).
#[epics_base_rs::epics_test]
async fn a_desired_edit_before_put_is_dropped() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    idle(&ioc, "M:ml_monoOperAck", "M:ml_monoSeqMsg1").await;
    put_process(&ioc, "M:ml_monoTheta", 20.0).await;
    wait_near(&ioc, "M:ml_monoThetaSet", 20.0).await;
    for (pv, v) in [
        ("M:ml_monoThetaSet", 25.0),
        ("M:ml_monoTheta2Set", 26.0),
        ("M:ml_monoZSet", 40.0),
    ] {
        put_process(&ioc, pv, v).await;
    }
    task::sleep(Duration::from_millis(500)).await;
    moves_to(&ioc, 20.0).await;
}

/// Press Put, then find each motor's VAL at the move to `theta`.
async fn moves_to(ioc: &PvDatabase, theta: f64) {
    put_process(ioc, "M:ml_monoPut", 1.0).await;
    wait_near(ioc, "M:ml_monoPut", 0.0).await;
    for (pv, want) in [
        ("M:th.VAL", theta),
        ("M:th2.VAL", theta),
        ("M:z.VAL", 35.0 / (2.0 * theta / RAD_CONV).tan()),
    ] {
        let got = f64_of(ioc, pv).await;
        assert!((got - want).abs() < 1e-6, "{pv}: {got}, want {want}");
    }
}

/// A theta request landing while the move to 20 holds the program in
/// updateRdbk is moved to after that move, so the next tweak starts from
/// it. C's waitForCmndEnter took it into its monitored theta without a move.
#[epics_base_rs::epics_test]
async fn a_theta_request_during_a_move_is_moved_to_after_it() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    post_after_enter(&ioc, |db| {
        post_dmov(db, false);
        post(db, "M:ml_monoMode", EpicsValue::Enum(1));
        post(db, "M:ml_monoTheta", EpicsValue::Double(20.0));
    })
    .await;
    // Z's VAL is the move's last motor put.
    wait_near(&ioc, "M:z.VAL", z_of(20.0)).await;
    post(&ioc, "M:ml_monoTheta", EpicsValue::Double(30.0));
    task::sleep(Duration::from_millis(300)).await;
    post_dmov(&ioc, true);
    wait_near(&ioc, "M:z.VAL", z_of(30.0)).await;
    theta_driven_to(&ioc, 30.0).await;
    post_dmov(&ioc, true);
    idle(&ioc, "M:ml_monoOperAck", "M:ml_monoSeqMsg1").await;
    put_process(&ioc, "M:ml_monoThetaTweak", 1.0).await;
    put_process(&ioc, "M:ml_monoThetaInc", 1.0).await;
    wait_near(&ioc, "M:ml_monoTheta", 31.0).await;
}

/// `puts` processed while the program waits out waitForCmndEnter's
/// delay(.1), which C's clears dropped them in.
async fn put_in_enter(ioc: &PvDatabase, puts: &[(&str, f64)]) {
    idle(ioc, "M:ml_monoOperAck", "M:ml_monoSeqMsg1").await;
    // The opAck arm's SeqMsg2 is its last put before the enter's delay(.1);
    // it posts no event, as SeqMsg2 was " " already.
    post(ioc, "M:ml_monoSeqMsg2", EpicsValue::String("-".into()));
    put_process(ioc, "M:ml_monoOperAck", 1.0).await;
    let mut polls = 0;
    while string_of(ioc, "M:ml_monoSeqMsg2").await != " " {
        polls += 1;
        assert!(polls < 5000, "opAck arm never ran");
        task::sleep(Duration::from_millis(2)).await;
    }
    for (pv, v) in puts {
        put_process(ioc, pv, *v).await;
    }
    assert_eq!(
        f64_of(ioc, "M:ml_monoOperAck").await,
        1.0,
        "{puts:?} missed the enter's delay"
    );
}

/// A theta request landing in waitForCmndEnter's delay is moved to in auto
/// mode. C's clears took it into theta with no pass and no move.
#[epics_base_rs::epics_test]
async fn a_theta_request_in_the_enter_is_moved_to() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    post(&ioc, "M:ml_monoMode", EpicsValue::Enum(1));
    put_in_enter(&ioc, &[("M:ml_monoTheta", 20.0)]).await;
    wait_near(&ioc, "M:z.VAL", z_of(20.0)).await;
    theta_driven_to(&ioc, 20.0).await;
}

/// An E request and a D change landing in waitForCmndEnter's delay are
/// both taken: the layer spacing pass applies the request under the new
/// 2d. C's clears dropped the D change and took E with no pass.
#[epics_base_rs::epics_test]
async fn an_e_request_pending_at_a_layer_spacing_change_takes_the_new_2d() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    put_in_enter(&ioc, &[("M:ml_monoE", 1.5), ("M:ml_monoD", 20.0)]).await;
    settled(&ioc, "New effective d spacing").await;
    let theta = RAD_CONV * (HC / 1.5 / two_d(&ioc).await).asin();
    wait_near(&ioc, "M:ml_monoThetaSet", theta).await;
    wait_near(&ioc, "M:ml_monoTheta", theta).await;
    wait_near(&ioc, "M:ml_monoE", 1.5).await;
}

/// A theta LLM put above the standing theta clamps ml_monoTheta to the new
/// DRVL. The clamp is this program's own: no pass is computed for it, so
/// nothing moves and no limit refusal replaces the limits message, and Done
/// is written for the Moving the DRVL put raised.
#[epics_base_rs::epics_test]
async fn a_drive_limit_clamp_is_not_a_request() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    idle(&ioc, "M:ml_monoOperAck", "M:ml_monoSeqMsg1").await;
    post(&ioc, "M:ml_monoMode", EpicsValue::Enum(1));
    let th = f64_of(&ioc, "M:th.VAL").await;
    put_process(&ioc, "M:th.LLM", 15.0).await;
    wait_near(&ioc, "M:ml_monoTheta.DRVL", 15.0).await;
    wait_near(&ioc, "M:ml_monoTheta", 15.0).await;
    wait_near(&ioc, "M:ml_monoMoving", 0.0).await;
    task::sleep(Duration::from_millis(500)).await;
    assert_eq!(f64_of(&ioc, "M:th.VAL").await, th);
    assert_eq!(f64_of(&ioc, "M:ml_monoMoving").await, 0.0);
    assert_eq!(
        string_of(&ioc, "M:ml_monoSeqMsg1").await,
        "Theta Motor Soft Limits Changed"
    );
}

/// A tweak after the theta LLM put clamps `ao` to `limit` starts from the
/// clamp, as from C's monitored value.
async fn a_tweak_after_a_drive_limit_clamp_starts_from_it(
    [ao, limit, step_pv, button]: [&str; 4],
    step: f64,
) {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    idle(&ioc, "M:ml_monoOperAck", "M:ml_monoSeqMsg1").await;
    post(&ioc, "M:ml_monoMode", EpicsValue::Enum(1));
    put_process(&ioc, "M:th.LLM", 15.0).await;
    wait_near(&ioc, "M:ml_monoTheta", 15.0).await;
    wait_near(&ioc, "M:ml_monoMoving", 0.0).await;
    let clamp = f64_of(&ioc, limit).await;
    assert_eq!(f64_of(&ioc, ao).await, clamp, "{ao}");
    put_process(&ioc, step_pv, step.abs()).await;
    put_process(&ioc, button, 1.0).await;
    wait_near(&ioc, ao, clamp + step).await;
}

#[epics_base_rs::epics_test]
async fn an_e_tweak_after_a_drive_limit_clamp_starts_from_it() {
    a_tweak_after_a_drive_limit_clamp_starts_from_it(
        [
            "M:ml_monoE",
            "M:ml_monoE.DRVH",
            "M:ml_monoETweak",
            "M:ml_monoEDec",
        ],
        -0.1,
    )
    .await;
}

#[epics_base_rs::epics_test]
async fn a_lambda_tweak_after_a_drive_limit_clamp_starts_from_it() {
    a_tweak_after_a_drive_limit_clamp_starts_from_it(
        [
            "M:ml_monoLambda",
            "M:ml_monoLambda.DRVL",
            "M:ml_monoLambdaTweak",
            "M:ml_monoLambdaInc",
        ],
        0.01,
    )
    .await;
}

#[epics_base_rs::epics_test]
async fn a_theta_tweak_after_a_drive_limit_clamp_starts_from_it() {
    a_tweak_after_a_drive_limit_clamp_starts_from_it(
        [
            "M:ml_monoTheta",
            "M:ml_monoTheta.DRVL",
            "M:ml_monoThetaTweak",
            "M:ml_monoThetaInc",
        ],
        1.0,
    )
    .await;
}

/// Post `stale` on `pv`'s VAL and leave VAL at `now` with no event.
fn post_stale(db: &PvDatabase, pv: &str, stale: f64, now: f64) {
    let rec = db.get_record(pv).unwrap();
    let mut inst = rec.write();
    inst.record
        .put_field("VAL", EpicsValue::Double(stale))
        .unwrap();
    inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
    inst.record
        .put_field("VAL", EpicsValue::Double(now))
        .unwrap();
}

/// A stale theta event pending at a lattice change is not taken: the
/// lattice pass keeps the standing theta.
#[epics_base_rs::epics_test]
async fn a_stale_event_at_a_lattice_change_is_not_taken() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    put_process(&ioc, "M:ml_monoTheta", 20.0).await;
    wait_near(&ioc, "M:ml_monoThetaSet", 20.0).await;
    let mut set = DbSubscription::subscribe(&ioc, "M:ml_monoThetaSet")
        .await
        .unwrap();
    put_process(&ioc, "M:ml_monoD", 20.0).await;
    post_stale(&ioc, "M:ml_monoTheta", 10.0, 20.0);
    settled(&ioc, "New effective d spacing").await;
    stays(&mut set, 20.0).await;
}

/// A theta event holding a value the PV no longer holds, as an own put's
/// echo landing after a later put's readback does, is not taken.
#[epics_base_rs::epics_test]
async fn a_stale_event_is_not_taken() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    put_process(&ioc, "M:ml_monoTheta", 20.0).await;
    wait_near(&ioc, "M:ml_monoThetaSet", 20.0).await;
    let mut set = DbSubscription::subscribe(&ioc, "M:ml_monoThetaSet")
        .await
        .unwrap();
    post_stale(&ioc, "M:ml_monoTheta", 10.0, 20.0);
    stays(&mut set, 20.0).await;
}

/// A NaN request is done after its move though NaN != NaN, and its own
/// echo is no request.
#[epics_base_rs::epics_test]
async fn a_nan_request_is_taken_once() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    put_process(&ioc, "M:ml_monoTheta", f64::NAN).await;
    let mut set = 0.0;
    for _ in 0..100 {
        set = f64_of(&ioc, "M:ml_monoThetaSet").await;
        if set.is_nan() {
            break;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    assert!(set.is_nan(), "M:ml_monoThetaSet {set}");
    assert_eq!(f64_of(&ioc, "M:ml_monoMoving").await, 1.0);
    put_process(&ioc, "M:ml_monoPut", 1.0).await;
    task::sleep(Duration::from_millis(300)).await;
    post_dmov(&ioc, true);
    wait_near(&ioc, "M:ml_monoMoving", 0.0).await;
    idle(&ioc, "M:ml_monoOperAck", "M:ml_monoSeqMsg1").await;
    put_process(&ioc, "M:ml_monoTheta", 20.0).await;
    wait_near(&ioc, "M:ml_monoThetaSet", 20.0).await;
}

/// An E put equal to the standing E posts no E event, only the Moving
/// rise: busyChanged's pass moves to the standing setpoint, and Done waits
/// for that move. C left ml_monoMoving raised.
#[epics_base_rs::epics_test]
async fn an_equal_e_request_is_done_after_its_move() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    idle(&ioc, "M:ml_monoOperAck", "M:ml_monoSeqMsg1").await;
    post(&ioc, "M:ml_monoMode", EpicsValue::Enum(1));
    post_dmov(&ioc, false);
    let e = f64_of(&ioc, "M:ml_monoE").await;
    put_process(&ioc, "M:ml_monoE", e).await;
    wait_near(&ioc, "M:ml_monoMoving", 1.0).await;
    task::sleep(Duration::from_millis(500)).await;
    assert_eq!(f64_of(&ioc, "M:ml_monoMoving").await, 1.0);
    post_dmov(&ioc, true);
    wait_near(&ioc, "M:ml_monoMoving", 0.0).await;
    assert_eq!(f64_of(&ioc, "M:ml_monoE").await, e);
}

/// A manual-mode request's pass holds ml_monoMoving for Put, and a second
/// put of the same value joins that hold: Done follows Put's move.
#[epics_base_rs::epics_test]
async fn an_equal_request_during_a_manual_hold_joins_it() {
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    idle(&ioc, "M:ml_monoOperAck", "M:ml_monoSeqMsg1").await;
    let th = f64_of(&ioc, "M:th.VAL").await;
    put_process(&ioc, "M:ml_monoTheta", 20.0).await;
    wait_near(&ioc, "M:ml_monoThetaSet", 20.0).await;
    put_process(&ioc, "M:ml_monoTheta", 20.0).await;
    task::sleep(Duration::from_millis(500)).await;
    assert_eq!(f64_of(&ioc, "M:ml_monoMoving").await, 1.0);
    assert_eq!(f64_of(&ioc, "M:th.VAL").await, th);
    post_dmov(&ioc, false);
    put_process(&ioc, "M:ml_monoPut", 1.0).await;
    wait_near(&ioc, "M:z.VAL", z_of(20.0)).await;
    task::sleep(Duration::from_millis(300)).await;
    assert_eq!(f64_of(&ioc, "M:ml_monoMoving").await, 1.0);
    post_dmov(&ioc, true);
    wait_near(&ioc, "M:ml_monoMoving", 0.0).await;
    assert_eq!(f64_of(&ioc, "M:th.VAL").await, 20.0);
}

/// Hold a manual theta request to 20, then switch to auto mode.
async fn held_at_20() -> (Arc<PvDatabase>, ScanOwner) {
    let (ioc, scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    idle(&ioc, "M:ml_monoOperAck", "M:ml_monoSeqMsg1").await;
    put_process(&ioc, "M:ml_monoTheta", 20.0).await;
    wait_near(&ioc, "M:ml_monoThetaSet", 20.0).await;
    task::sleep(Duration::from_millis(300)).await;
    post(&ioc, "M:ml_monoMode", EpicsValue::Enum(1));
    (ioc, scan)
}

/// An equal theta request in auto mode, put once the hold is lowered, is
/// moved to and Done.
async fn an_equal_request_after_the_hold_is_done(ioc: &PvDatabase) {
    wait_near(ioc, "M:ml_monoMoving", 0.0).await;
    // A move's limit monitors run thetaLimits, whose own theta put raises
    // Moving and whose checkDone lowers it.
    task::sleep(Duration::from_secs(1)).await;
    let mut moving = DbSubscription::subscribe(ioc, "M:ml_monoMoving")
        .await
        .unwrap();
    put_process(ioc, "M:ml_monoTheta", 20.0).await;
    shows(&mut moving, 1.0).await;
    task::sleep(Duration::from_millis(300)).await;
    post_dmov(ioc, true);
    shows(&mut moving, 0.0).await;
}

/// An equal request after a hold's move is done is not taken for that
/// hold.
#[epics_base_rs::epics_test]
async fn a_hold_ends_at_its_done() {
    let (ioc, _scan) = held_at_20().await;
    post_dmov(&ioc, false);
    put_process(&ioc, "M:ml_monoPut", 1.0).await;
    wait_near(&ioc, "M:z.VAL", z_of(20.0)).await;
    post_dmov(&ioc, true);
    an_equal_request_after_the_hold_is_done(&ioc).await;
}

/// A hold lowered by hand ends with it.
#[epics_base_rs::epics_test]
async fn a_hold_ends_when_lowered_by_hand() {
    let (ioc, _scan) = held_at_20().await;
    put_process(&ioc, "M:ml_monoMoving", 0.0).await;
    an_equal_request_after_the_hold_is_done(&ioc).await;
}

const CHILD: &str = "ML_MONO_CTL_DEBUG_CHILD";

/// `test` run again in a child whose stdout is not captured, and the
/// ml_monoCtl lines it printed.
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
        .filter_map(|l| l.find("ml_monoCtl:").map(|i| l[i..].to_string()))
        .collect()
}

/// With ml_monoCtlDebug set, an E put in manual mode prints C's lines
/// (ml_monoCtl.st:759-764, :968-1121, :652-708), then busyChanged's for
/// the Moving rise its pass's puts raised, which the pass holds.
#[epics_base_rs::epics_test]
async fn debug_prints_c_lines() {
    if std::env::var_os(CHILD).is_none() {
        let trace = trace_of("debug_prints_c_lines");
        assert_eq!(
            trace,
            [
                "ml_monoCtl:waitForCmnd:E_mon",
                "ml_monoCtl:eChanged",
                "ml_monoCtl:lChanged",
                "ml_monoCtl:thChanged",
                "ml_monoCtl:calcMovements",
                "ml_monoCtl:move_ml_mono",
                "ml_monoCtl:waitForCmndEnter:initSeqDone",
                "ml_monoCtl:waitForCmndEnter: ml_monoMoving=1; ml_monoDone=0",
                "ml_monoCtl:waitForCmnd:mlMbusy",
                "ml_monoCtl:busyChanged",
                "ml_monoCtl:waitForCmndEnter:initSeqDone",
                "ml_monoCtl:waitForCmndEnter: ml_monoMoving=1; ml_monoDone=0",
            ]
        );
        return;
    }
    let (ioc, _scan) = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&ioc, "y offset changed to 35.000000").await;
    idle(&ioc, "M:ml_monoOperAck", "M:ml_monoSeqMsg1").await;
    post(&ioc, "M:ml_monoCtlDebug", EpicsValue::Long(1));
    task::sleep(Duration::from_millis(200)).await;
    let e = f64_of(&ioc, "M:ml_monoE").await;
    post(&ioc, "M:ml_monoE", EpicsValue::Double(e * 1.01));
    task::sleep(Duration::from_millis(500)).await;
}
