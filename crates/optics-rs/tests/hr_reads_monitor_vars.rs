//! hrCtl reads autoMode as C's monitored variable, written as the monitor
//! arrives (hrCtl.st:64-66, no efflag).

use epics_base_rs::runtime::task;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use epics_seq_rs::seq;
use optics_rs::snl::hr_ctl::hrCtl;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const UR2D: f64 = 180.0 / std::f64::consts::PI / 1e6;
const HR_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/hrSeq.db");
const MOTORS: &str = r#"
record(motor, "H:p1") { field(HLM, "1500000") field(LLM, "10000") }
record(motor, "H:p2") { field(HLM, "1500000") field(LLM, "10000") }
"#;

async fn f64_of(ioc: &PvDatabase, pv: &str) -> f64 {
    ioc.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn string_of(ioc: &PvDatabase, pv: &str) -> String {
    match ioc.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        v => format!("{v:?}"),
    }
}

async fn boot() -> (Arc<PvDatabase>, ScanOwner) {
    let macros: HashMap<String, String> =
        [("P", "H:"), ("N", "1"), ("M_PHI1", "p1"), ("M_PHI2", "p2")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    let ioc = IocBuilder::new()
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("motor", || Box::new(motor_rs::MotorRecord::new()))
        .db_string(MOTORS, &macros)
        .unwrap()
        .db_file(HR_DB, &macros)
        .unwrap()
        .build_served()
        .await
        .unwrap()
        .0;
    // iocInit's PINI pass, which the program's start waits for.
    let scan = ScanOwner::start(ioc.clone());
    ioc.wait_for_pini().await;
    let db = PvDatabase::clone(&ioc);
    for name in ["H:p1", "H:p2"] {
        db.get_record(name)
            .unwrap()
            .write()
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
            .unwrap()
            .pos
            .rbv = 200_000.0;
    }
    seq(&hrCtl, "P=H:, N=1, M_PHI1=p1, M_PHI2=p2", 0, &db).expect("hrCtl starts");
    // The boot phiLimits pass: theta1Hi = HLM in degrees.
    wait_near(&ioc, "H:HR1_Theta1AO.DRVH", 1.5e6 * UR2D).await;
    assert_eq!(
        string_of(&ioc, "H:HR1_SeqMsg1SI").await,
        "Motor Soft Limit Changed"
    );
    (ioc, scan)
}

/// Return with the sequencer idle after an enter. An ack posted during a
/// pass is lost to that pass's opAck = 0, as in C, so it is posted again
/// until one is taken (msg1 " ") and its enter has put opAck 0.
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

/// Run `posts` once the sequencer has passed waitForCmndEnter's `opAck = 0`:
/// OperAck sends it there. Its puts are queued, so no record lock can hold
/// it inside the enter.
async fn post_after_enter(db: &PvDatabase, posts: impl FnOnce(&PvDatabase)) {
    idle(db, "H:HR1_OperAckBO", "H:HR1_SeqMsg1SI").await;
    optics_rs::db_access::DbChannel::new(db, "H:HR1_OperAckBO")
        .put_f64_process(1.0)
        .await
        .unwrap();
    wait_near(db, "H:HR1_OperAckBO", 0.0).await;
    posts(db);
}

/// Post `v` to `pv`'s VAL with no processing.
fn post(db: &PvDatabase, pv: &str, v: EpicsValue) {
    let rec = db.get_record(pv).unwrap();
    let mut inst = rec.write();
    inst.record.put_field("VAL", v).unwrap();
    inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
}

/// An E put queued with Mode=Auto already sees auto mode and drives phi1,
/// though E is taken before Mode.
#[epics_base_rs::epics_test]
async fn an_e_put_queued_after_auto_mode_moves() {
    let (ioc, _scan) = boot().await;
    let db = PvDatabase::clone(&ioc);
    let e = f64_of(&ioc, "H:HR1_EAO").await;
    let before = f64_of(&ioc, "H:p1.VAL").await;
    post_after_enter(&db, |db| {
        post(db, "H:HR1_ModeBO", EpicsValue::Enum(1));
        post(db, "H:HR1_EAO", EpicsValue::Double(e * 1.01));
    })
    .await;
    for _ in 0..100 {
        let set = f64_of(&ioc, "H:HR1_phi1SetAO").await;
        if set != before && f64_of(&ioc, "H:p1.VAL").await == set {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "phi1 not driven: phi1SetAO {} p1.VAL {} (was {before})",
        f64_of(&ioc, "H:HR1_phi1SetAO").await,
        f64_of(&ioc, "H:p1.VAL").await
    );
}

async fn wait_near(ioc: &PvDatabase, pv: &str, want: f64) {
    for _ in 0..100 {
        if (f64_of(ioc, pv).await - want).abs() < 1e-4 {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!("{pv} = {}, want {want}", f64_of(ioc, pv).await);
}

/// A motor soft-limit change, UseSet=Set and UseSet=Use each run C's
/// phiLimits (hrCtl.st:700-706, :768-786): Set zeroes the drive limits,
/// Use derives them again from the motor HLM/LLM.
#[epics_base_rs::epics_test]
async fn use_set_zeroes_the_drive_limits_and_use_restores_them() {
    let (ioc, _scan) = boot().await;
    let db = PvDatabase::clone(&ioc);
    let put = |pv: &'static str, v: EpicsValue| {
        let db = db.clone();
        async move {
            let (rec, field) = pv.split_once('.').unwrap_or((pv, "VAL"));
            db.put_record_field_from_ca(rec, field, v).await.unwrap();
        }
    };
    // phi1Hi = phi1MotHiLim*uR2D + phi1Off + worldOff*uR2D (hrCtl.st:848).
    let want = 1e6 * UR2D
        + f64_of(&ioc, "H:HR1_phi1OffAO").await
        + f64_of(&ioc, "H:HR1_worldOffAO").await * UR2D;

    put("H:p1.HLM", EpicsValue::Double(1e6)).await;
    wait_near(&ioc, "H:HR1_Theta1AO.DRVH", want).await;
    assert_eq!(
        string_of(&ioc, "H:HR1_SeqMsg1SI").await,
        "Motor Soft Limit Changed"
    );
    assert_ne!(f64_of(&ioc, "H:HR1_EAO.DRVH").await, 0.0);

    post_after_enter(&db, |db| post(db, "H:HR1_UseSetBO", EpicsValue::Enum(1))).await;
    wait_near(&ioc, "H:HR1_Theta1AO.DRVH", 0.0).await;
    wait_near(&ioc, "H:HR1_EAO.DRVH", 0.0).await;
    wait_near(&ioc, "H:HR1_phi1AO.DRVH", 0.0).await;

    post_after_enter(&db, |db| post(db, "H:HR1_UseSetBO", EpicsValue::Enum(0))).await;
    wait_near(&ioc, "H:HR1_Theta1AO.DRVH", want).await;
}

/// C's E takes the sequencer's own posts: UseSet=Set zeroes EHi before
/// ELo, aoRecord clamps E to 0, and eChanged reads that 0 (C kohzuIoc
/// oracle: "Wavelength > 2d spacing of crystal 1.", E back from theta1).
#[epics_base_rs::epics_test]
async fn set_mode_takes_e_as_its_own_limit_put_clamped_it() {
    let (ioc, _scan) = boot().await;
    let db = PvDatabase::clone(&ioc);
    // The oracle's limits, which leave ELo negative.
    for (rec, field, v) in [
        ("H:p1", "HLM", 90.0),
        ("H:p1", "LLM", -10.0),
        ("H:p2", "HLM", 90.0),
        ("H:p2", "LLM", -10.0),
    ] {
        db.put_record_field_from_ca(rec, field, EpicsValue::Double(v))
            .await
            .unwrap();
    }
    wait_near(&ioc, "H:HR1_EAO.DRVL", -229.18127).await;
    let e = f64_of(&ioc, "H:HR1_EAO").await;
    post_after_enter(&db, |db| post(db, "H:HR1_UseSetBO", EpicsValue::Enum(1))).await;
    let mut msg1 = String::new();
    for _ in 0..100 {
        msg1 = string_of(&ioc, "H:HR1_SeqMsg1SI").await;
        if msg1 == "Wavelength > 2d spacing of crystal 1." {
            break;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(msg1, "Wavelength > 2d spacing of crystal 1.");
    wait_near(&ioc, "H:HR1_EAO", e).await;
}

/// updatePsuedo echoes each motor's VAL and DMOV as it is posted, with no
/// command pass (hrCtl.st:1518-1540).
#[epics_base_rs::epics_test]
async fn the_cmd_and_dmov_echoes_follow_the_motor_monitors() {
    let (ioc, _scan) = boot().await;
    let db = PvDatabase::clone(&ioc);
    post(&db, "H:p1", EpicsValue::Double(300_000.0));
    post(&db, "H:p2", EpicsValue::Double(400_000.0));
    wait_near(&ioc, "H:HR1_phi1CmdAO", 300_000.0).await;
    wait_near(&ioc, "H:HR1_phi2CmdAO", 400_000.0).await;
    for (motor, echo) in [("H:p1", "H:HR1_phi1DmovBI"), ("H:p2", "H:HR1_phi2DmovBI")] {
        for dmov in [0i16, 1] {
            let rec = db.get_record(motor).unwrap();
            {
                let mut inst = rec.write();
                inst.record
                    .as_any_mut()
                    .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
                    .unwrap()
                    .stat
                    .dmov = i16::from(dmov == 1);
                inst.notify_field("DMOV", EventMask::VALUE | EventMask::LOG);
            }
            wait_near(&ioc, echo, f64::from(dmov)).await;
        }
    }
}

async fn put_process(ioc: &PvDatabase, pv: &str, v: f64) {
    optics_rs::db_access::DbChannel::new(ioc, pv)
        .put_f64_process(v)
        .await
        .unwrap();
}

/// calcMovements' phi1MotDesired for `theta1`, with opMode 0
/// (hrCtl.st:1184, :1239).
async fn desired(ioc: &PvDatabase, theta1: f64) -> f64 {
    let off = f64_of(ioc, "H:HR1_phi1OffAO").await + f64_of(ioc, "H:HR1_worldOffAO").await * UR2D;
    (theta1 - off) / UR2D
}

/// Press Put, then find phi1's VAL at the move to `theta1`.
async fn moves_to(ioc: &PvDatabase, theta1: f64) {
    put_process(ioc, "H:HR1_PutBO", 1.0).await;
    moved_to(ioc, theta1).await;
}

async fn moved_to(ioc: &PvDatabase, theta1: f64) {
    wait_near(ioc, "H:HR1_PutBO", 0.0).await;
    let (got, want) = (f64_of(ioc, "H:p1.VAL").await, desired(ioc, theta1).await);
    assert!((got - want).abs() < 1e-4, "H:p1.VAL: {got}, want {want}");
}

/// Put goes where its own pass calculated, though the 20 pass's Desired
/// echo lands inside that pass (C defect, hrCtl.st:337-345).
#[epics_base_rs::epics_test]
async fn a_late_desired_echo_leaves_the_move_at_the_put_pass() {
    let (ioc, _scan) = boot().await;
    let db = PvDatabase::clone(&ioc);
    idle(&db, "H:HR1_OperAckBO", "H:HR1_SeqMsg1SI").await;
    // The boot passes' Desired echoes are in.
    task::sleep(Duration::from_millis(2500)).await;
    let t0 = std::time::Instant::now();
    put_process(&ioc, "H:HR1_Theta1AO", 20.0).await;
    wait_near(&ioc, "H:HR1_phi1SetAO", desired(&ioc, 20.0).await).await;
    task::sleep(Duration::from_secs(1).saturating_sub(t0.elapsed())).await;
    put_process(&ioc, "H:HR1_Theta1AO", 30.0).await;
    wait_near(&ioc, "H:HR1_phi1SetAO", desired(&ioc, 30.0).await).await;
    // Under a 2 s echo delay and a 0.8 s put delay, the 20 echo lands
    // between the Put pass's assignment and its copy, the 30 echo after.
    task::sleep(Duration::from_millis(2400).saturating_sub(t0.elapsed())).await;
    moves_to(&ioc, 30.0).await;
}

/// Put recalculates the Desired values, so an edit before it is not
/// moved to, in C as here.
#[epics_base_rs::epics_test]
async fn a_desired_edit_before_put_is_recalculated() {
    let (ioc, _scan) = boot().await;
    let db = PvDatabase::clone(&ioc);
    idle(&db, "H:HR1_OperAckBO", "H:HR1_SeqMsg1SI").await;
    put_process(&ioc, "H:HR1_Theta1AO", 20.0).await;
    wait_near(&ioc, "H:HR1_phi1SetAO", desired(&ioc, 20.0).await).await;
    put_process(&ioc, "H:HR1_phi1SetAO", desired(&ioc, 25.0).await).await;
    task::sleep(Duration::from_millis(500)).await;
    moves_to(&ioc, 20.0).await;
}

/// A Desired edit landing inside the Put pass is dropped: Put goes where
/// the pass calculated, where C took the edit (C defect, hrCtl.st:337-345).
#[epics_base_rs::epics_test]
async fn a_desired_edit_inside_the_put_pass_is_dropped() {
    let (ioc, _scan) = boot().await;
    let db = PvDatabase::clone(&ioc);
    idle(&db, "H:HR1_OperAckBO", "H:HR1_SeqMsg1SI").await;
    put_process(&ioc, "H:HR1_Theta1AO", 20.0).await;
    wait_near(&ioc, "H:HR1_phi1SetAO", desired(&ioc, 20.0).await).await;
    let edit = desired(&ioc, 25.0).await;
    put_process(&ioc, "H:HR1_PutBO", 1.0).await;
    // Under a 1 s put delay, the edit lands between the Put pass's
    // assignment and its put.
    task::sleep(Duration::from_millis(500)).await;
    put_process(&ioc, "H:HR1_phi1SetAO", edit).await;
    moved_to(&ioc, 20.0).await;
}

const CHILD: &str = "HR_CTL_DEBUG_CHILD";

/// `test` run again in a child whose stdout is not captured, and the
/// hrCtl lines it printed.
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
        .filter_map(|l| l.find("hrCtl:").map(|i| l[i..].to_string()))
        .collect()
}

/// With hrCtlDebug set, an opMode change prints C's lines: each state's
/// entry block and the clauses it takes (hrCtl.st:788-794, :1232-1319,
/// :602-698).
#[epics_base_rs::epics_test]
async fn debug_prints_c_lines() {
    if std::env::var_os(CHILD).is_none() {
        let trace = trace_of("debug_prints_c_lines");
        assert_eq!(
            trace,
            [
                "hrCtl:waitForCmnd: opMode event.",
                "hrCtl:calcMovements: entry",
                "hrCtl:calcMovements, USE",
                "hrCtl:moveHR entry",
                "hrCtl:moveHR: NOT putting to motors",
                "hrCtl:waitForCmndEnter: entry",
                "hrCtl:waitForCmdEnter: initSeqDone",
                "hrCtl:waitForCmndEnter: Moving=0; Done=0",
                "hrCtl:waitForCmnd: entry",
            ]
        );
        return;
    }
    let (ioc, _scan) = boot().await;
    let db = PvDatabase::clone(&ioc);
    idle(&db, "H:HR1_OperAckBO", "H:HR1_SeqMsg1SI").await;
    // The enter's action reads on past its opAck = 0 into waitForCmnd.
    task::sleep(Duration::from_millis(200)).await;
    post(&db, "H:HR1CtlDebug", EpicsValue::Long(1));
    task::sleep(Duration::from_millis(200)).await;
    post(&db, "H:HR1_Mode2MO", EpicsValue::Enum(1));
    task::sleep(Duration::from_millis(500)).await;
}

/// Run `posts` in waitForCmndEnter's delay: an OperAck pass sends the
/// sequencer there, its SeqMsg1 " " first, and the enter's action ends
/// with opAck = 0.
async fn post_in_enter(db: &PvDatabase, posts: impl FnOnce(&PvDatabase)) {
    let (ack, msg1) = ("H:HR1_OperAckBO", "H:HR1_SeqMsg1SI");
    idle(db, ack, msg1).await;
    db.get_record(msg1)
        .unwrap()
        .write()
        .record
        .put_field("VAL", EpicsValue::String("-".into()))
        .unwrap();
    put_process(db, ack, 1.0).await;
    for _ in 0..2000 {
        if string_of(db, msg1).await == " " {
            break;
        }
        task::sleep(Duration::from_millis(1)).await;
    }
    posts(db);
    assert_eq!(
        f64_of(db, ack).await,
        1.0,
        "the posts missed the enter's delay"
    );
}

async fn shown(ioc: &PvDatabase, msg1: &str) {
    for _ in 0..100 {
        if string_of(ioc, "H:HR1_SeqMsg1SI").await == msg1 {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "SeqMsg1 {:?}, want {msg1:?}",
        string_of(ioc, "H:HR1_SeqMsg1SI").await
    );
}

/// Finish `motor`'s move where it was sent, as its driver would, which
/// completes hrCtl's put to it.
async fn finish(db: &PvDatabase, motor: &str) {
    {
        let rec = db.get_record(motor).unwrap();
        let mut inst = rec.write();
        let m = inst
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
            .unwrap();
        let to = m.pos.dval;
        m.set_event(motor_rs::flags::MotorEvent::DeviceUpdate(
            asyn_rs::interfaces::motor::MotorStatus {
                done: true,
                moving: false,
                ..asyn_rs::interfaces::motor::MotorStatus::default()
                    .with_position(to)
                    .with_encoder_position(to)
            },
        ));
    }
    db.process_record(motor).await.unwrap();
}

/// A theta1 request landing in waitForCmndEnter's delay is taken. C's
/// enter cleared its event, and the request stood untaken.
#[epics_base_rs::epics_test]
async fn a_request_in_the_enter_is_taken() {
    let (ioc, _scan) = boot().await;
    let db = PvDatabase::clone(&ioc);
    post_in_enter(&db, |db| {
        post(db, "H:HR1_Theta1AO", EpicsValue::Double(21.0))
    })
    .await;
    wait_near(&ioc, "H:HR1_phi1SetAO", desired(&ioc, 21.0).await).await;
}

/// A geometry change landing in waitForCmndEnter's delay is taken. C's
/// enter cleared its event.
#[epics_base_rs::epics_test]
async fn a_geometry_change_in_the_enter_is_taken() {
    let (ioc, _scan) = boot().await;
    let db = PvDatabase::clone(&ioc);
    post_in_enter(&db, |db| post(db, "H:HR1_GeomMO", EpicsValue::Enum(1))).await;
    shown(&ioc, "New geometry.  Switch Phi 2 motor dir.").await;
}

/// A lattice change landing with an E request in waitForCmndEnter's
/// delay applies the request under the new 2d. C's enter cleared both,
/// and its thChanged recomputed E from theta1 over a pending request.
#[epics_base_rs::epics_test]
async fn a_lattice_change_takes_a_pending_e_request_under_the_new_2d() {
    let (ioc, _scan) = boot().await;
    let db = PvDatabase::clone(&ioc);
    let e = f64_of(&ioc, "H:HR1_EAO").await * 1.01;
    post_in_enter(&db, |db| {
        post(db, "H:HR1_H1AO", EpicsValue::Double(3.0));
        post(db, "H:HR1_EAO", EpicsValue::Double(e));
    })
    .await;
    // Si (311).
    let d1 = 2.0 * 5.43102 / 11f64.sqrt();
    wait_near(&ioc, "H:HR1_2d1AO", d1).await;
    wait_near(
        &ioc,
        "H:HR1_Theta1AO",
        (12.3984244 / e / d1).asin().to_degrees(),
    )
    .await;
    assert!((f64_of(&ioc, "H:HR1_EAO").await - e).abs() < 1e-6);
}

/// A theta1 request landing in a move is moved to after it, and Done waits
/// for that move. C's enter cleared its event and asserted Done for it.
#[epics_base_rs::epics_test]
async fn a_request_during_a_move_is_moved_to_before_done() {
    let (ioc, _scan) = boot().await;
    let db = PvDatabase::clone(&ioc);
    idle(&db, "H:HR1_OperAckBO", "H:HR1_SeqMsg1SI").await;
    post(&db, "H:HR1_ModeBO", EpicsValue::Enum(1));
    put_process(&ioc, "H:HR1_Theta1AO", 21.0).await;
    wait_near(&ioc, "H:p1.VAL", desired(&ioc, 21.0).await).await;
    put_process(&ioc, "H:HR1_Theta1AO", 23.0).await;
    let mut busy = DbSubscription::subscribe(&db, "H:HR1_Moving")
        .await
        .unwrap();
    finish(&db, "H:p1").await;
    wait_near(&ioc, "H:p1.VAL", desired(&ioc, 23.0).await).await;
    while let Ok(v) = task::timeout(Duration::from_millis(20), busy.recv_f64()).await {
        assert_ne!(v, Some(0.0), "Done before the move to 23");
    }
    finish(&db, "H:p1").await;
    wait_near(&ioc, "H:HR1_Moving", 0.0).await;
}

/// A drive limit put clamping theta1 is this program's own put, so theta1
/// is not moved for it, and a tweak starts from the clamped theta1 as from
/// C's monitored one.
#[epics_base_rs::epics_test]
async fn a_limit_clamp_is_not_a_request_and_a_tweak_starts_from_it() {
    let (ioc, _scan) = boot().await;
    let db = PvDatabase::clone(&ioc);
    idle(&db, "H:HR1_OperAckBO", "H:HR1_SeqMsg1SI").await;
    put_process(&ioc, "H:HR1_Theta1AO", 20.0).await;
    let at20 = desired(&ioc, 20.0).await;
    wait_near(&ioc, "H:HR1_phi1SetAO", at20).await;
    // theta1Hi = phi1MotHiLim*uR2D + off, here 15.
    let off = 20.0 - at20 * UR2D;
    db.put_record_field_from_ca("H:p1", "HLM", EpicsValue::Double((15.0 - off) / UR2D))
        .await
        .unwrap();
    wait_near(&ioc, "H:HR1_Theta1AO", 15.0).await;
    // Nothing to wait on for an absence: give a taken request time to show.
    task::sleep(Duration::from_millis(500)).await;
    wait_near(&ioc, "H:HR1_phi1SetAO", at20).await;
    idle(&db, "H:HR1_OperAckBO", "H:HR1_SeqMsg1SI").await;
    post(&db, "H:HR1_Theta1TweakAI", EpicsValue::Double(1.0));
    put_process(&ioc, "H:HR1_Theta1DecBO", 1.0).await;
    shown(&ioc, "Tweaked Theta 1").await;
    wait_near(&ioc, "H:HR1_Theta1AO", 14.0).await;
}

/// A theta1 event holding a value the PV no longer holds, as an own put's
/// echo landing after a later put's readback does, is not taken.
#[epics_base_rs::epics_test]
async fn a_stale_event_is_not_taken() {
    let (ioc, _scan) = boot().await;
    let db = PvDatabase::clone(&ioc);
    idle(&db, "H:HR1_OperAckBO", "H:HR1_SeqMsg1SI").await;
    put_process(&ioc, "H:HR1_Theta1AO", 20.0).await;
    let at20 = desired(&ioc, 20.0).await;
    wait_near(&ioc, "H:HR1_phi1SetAO", at20).await;
    {
        let rec = db.get_record("H:HR1_Theta1AO").unwrap();
        let mut inst = rec.write();
        inst.record
            .put_field("VAL", EpicsValue::Double(10.0))
            .unwrap();
        inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
        inst.record
            .put_field("VAL", EpicsValue::Double(20.0))
            .unwrap();
    }
    task::sleep(Duration::from_millis(500)).await;
    wait_near(&ioc, "H:HR1_phi1SetAO", at20).await;
}

/// A NaN request, which is not == itself, is taken once: the program goes
/// on to take an ack and the next request.
#[epics_base_rs::epics_test]
async fn a_nan_request_is_taken_once() {
    let (ioc, _scan) = boot().await;
    let db = PvDatabase::clone(&ioc);
    idle(&db, "H:HR1_OperAckBO", "H:HR1_SeqMsg1SI").await;
    put_process(&ioc, "H:HR1_Theta1AO", f64::NAN).await;
    let mut set = 0.0;
    for _ in 0..100 {
        set = f64_of(&ioc, "H:HR1_phi1SetAO").await;
        if set.is_nan() {
            break;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    assert!(set.is_nan(), "H:HR1_phi1SetAO {set}");
    idle(&db, "H:HR1_OperAckBO", "H:HR1_SeqMsg1SI").await;
    put_process(&ioc, "H:HR1_Theta1AO", 20.0).await;
    wait_near(&ioc, "H:HR1_phi1SetAO", desired(&ioc, 20.0).await).await;
}

/// The settings' connect-time events, which C's waitForCmndEnter cleared,
/// are not changes: no mode or geometry pass follows boot's limits pass.
#[epics_base_rs::epics_test]
async fn connect_time_setting_events_are_not_changes() {
    let (ioc, _scan) = boot().await;
    // Nothing to wait on for an absence: give a taken event time to show.
    task::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        string_of(&ioc, "H:HR1_SeqMsg1SI").await,
        "Motor Soft Limit Changed"
    );
    assert_eq!(
        string_of(&ioc, "H:HR1_SeqMsg2SI").await,
        "Theta2LoLim clipped at 0.500"
    );
}
