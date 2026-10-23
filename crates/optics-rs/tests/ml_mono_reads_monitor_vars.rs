//! ml_monoCtl reads autoMode and ccMode as C's monitored variables, written
//! as each monitor arrives (ml_monoCtl.st:55-64, no efflag), and its
//! updateSet state set ties UseSet to the motor .SET fields (:1262-1305).

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

/// Run `posts` once the sequencer has passed waitForCmndEnter's efClears
/// (ml_monoCtl.st:564-607): OperAck sends it there, and the enter's `opAck = 0` follows them.
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

/// OperAck 0 makes no waitForCmnd clause true (ml_monoCtl.st:705), so the
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
/// (ml_monoCtl.st:521-532). updatePsuedo puts the y readback on its first
/// monitor (:1217-1219), so y reads 20, which no case gives, and of the two
/// posts the one that is not 20 is initSequence's; MDEL -1 posts it when it
/// equals the DOL's 1. Two 20s mean updatePsuedo wrote the shared variable
/// between initSequence's assignment and its put, as C's threads can, and
/// leave nothing to check. th at 0 then holds the sequence in
/// chkMotorLimits.
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
        seen.len() == 2 && seen.iter().all(|v| *v == 20.0 || *v == want),
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
/// travel, takes zero time (ml_monoCtl.st:913-916): Z is the slower, so it
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
/// NaN, into its VELO (ml_monoCtl.st:921-925). th.VELO stays only if the
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
/// (ml_monoCtl.st:669-672, :824-956, :564-618).
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
