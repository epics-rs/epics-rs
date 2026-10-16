#![cfg(tokio_backend)]
//! kohzuCtl_soft's boot ends where C's does. The first waitForCmnd takes
//! the connect-time monitors its waitForCmndEnter leaves set, in its order:
//! thetaLimits, the theta readback (thetaMotStopped `!causedMove`), then
//! yOffset. Unlike kohzuCtl, a manual-mode pass that would violate a limit
//! is not rolled back (`kohzuCtl_soft.st:891`). A zero readback holds init
//! in chkMotorLimits.
//!
//! Expected values are a C kohzuIoc's (sequencer 2.2.9, optics, motorSim
//! axes with these limits) after the same boot.

use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::{CaServer, CaServerBuilder};
use optics_rs::snl::kohzu_ctl_soft::{self, KohzuSoftConfig};
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

const KOHZU_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/kohzuSeq_soft.db");
// kohzuSeq_soft.db has no SpeedCtrl record for the one kohzuCtl_soft assigns.
const MOTORS: &str = r#"
record(motor, "K:th") { field(HLM, "90") field(LLM, "-10") }
record(motor, "K:y") { field(HLM, "250") field(LLM, "-250") }
record(motor, "K:z") { field(HLM, "250") field(LLM, "-250") }
record(bo, "K:m:SpeedCtrl") {}
record(ao, "K:np") { field(OUT, "K:m:Moving.VAL NPP") }
"#;

/// Boot with these theta, Y and Z readbacks.
async fn boot(rbv: [f64; 3]) -> CaServer {
    boot_with(rbv, &[]).await
}

/// [`boot`], with these PVs put before the sequencer connects.
async fn boot_with(rbv: [f64; 3], presets: &[(&str, f64)]) -> CaServer {
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
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("motor", || Box::new(motor_rs::MotorRecord::new()))
        .db_string(MOTORS, &macros)
        .unwrap()
        .db_file(KOHZU_DB, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = (**server.database()).clone();
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
    for (pv, v) in presets {
        optics_rs::db_access::DbChannel::new(&db, pv)
            .put_f64_process(*v)
            .await
            .unwrap();
    }
    tokio::spawn(kohzu_ctl_soft::run(
        KohzuSoftConfig::new("K:", "m:", "th", "y", "z", 1),
        db,
    ));
    server
}

async fn f64_of(server: &CaServer, pv: &str) -> f64 {
    server.get(pv).await.unwrap().to_f64().unwrap()
}

async fn string_of(server: &CaServer, pv: &str) -> String {
    match server.get(pv).await.unwrap() {
        EpicsValue::String(s) => s.to_string(),
        other => panic!("{pv} is not a string: {other:?}"),
    }
}

/// Wait until SeqMsg1 reads `msg1`, then let the rest of the pass land.
async fn settled(server: &CaServer, msg1: &str) {
    for _ in 0..200 {
        if string_of(server, "K:m:SeqMsg1").await == msg1 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "SeqMsg1 never read {msg1:?}; last {:?}",
        string_of(server, "K:m:SeqMsg1").await
    );
}

async fn assert_near(server: &CaServer, pv: &str, want: f64) {
    let got = f64_of(server, pv).await;
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
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{last}");
}

async fn assert_state(server: &CaServer, msg2: &str, moving: f64, values: &[(&str, f64)]) {
    eventually(|| state_mismatch(server, msg2, moving, values)).await;
}

async fn state_mismatch(
    server: &CaServer,
    msg2: &str,
    moving: f64,
    values: &[(&str, f64)],
) -> Result<(), String> {
    let got = string_of(server, "K:m:SeqMsg2").await;
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
        let got = f64_of(server, pv).await;
        if !(got == want || (got - want).abs() <= 1e-4 * want.abs().max(1.0)) {
            return Err(format!("{pv}: {got}, C {want}"));
        }
    }
    Ok(())
}

/// thetaLimits sets 1..89, the readback pass moves theta to it, and the
/// yOffset pass recomputes Y/Z for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_readback_inside_the_limits_stands_after_the_connect_time_passes() {
    let server = boot([10.0, -17.77, 100.78]).await;
    settled(&server, "y offset changed to 17.500000").await;
    assert_state(
        &server,
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
#[tokio::test(flavor = "multi_thread")]
async fn a_readback_below_theta_lo_keeps_the_clamped_setpoints_in_manual_mode() {
    let server = boot([0.5, -17.501, 200.0]).await;
    settled(&server, "Z will exceed soft limits").await;
    // The readback pass also reports Z; wait out the yOffset pass.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_state(
        &server,
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
    let e = f64_of(&server, "K:m:E").await;
    assert!(
        [113.282, 226.555].iter().any(|c| (e - c).abs() <= 1e-3),
        "K:m:E: {e}"
    );
}

/// A zero readback holds init in chkMotorLimits: the limits are never set
/// and Moving stays raised.
#[tokio::test(flavor = "multi_thread")]
async fn a_zero_readback_holds_init_in_chk_motor_limits() {
    let server = boot([0.0, 0.0, 0.0]).await;
    settled(&server, "Theta Readback of 0 not reasonable").await;
    // Past two of chkMotorLimits' 2 s rounds and initSeq=3's 1 s delay.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        string_of(&server, "K:m:SeqMsg1").await,
        "Theta Readback of 0 not reasonable"
    );
    let msg2 = string_of(&server, "K:m:SeqMsg2").await;
    assert!(
        msg2 == "Set to Manual Mode" || msg2 == "Modify Motor Positions/Limits",
        "SeqMsg2 {msg2:?}"
    );
    assert_state(
        &server,
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
#[tokio::test(flavor = "multi_thread")]
async fn the_echo_pvs_follow_the_motors_while_init_holds() {
    let server = boot([0.0, 0.0, 0.0]).await;
    settled(&server, "Theta Readback of 0 not reasonable").await;
    let db = (**server.database()).clone();
    optics_rs::db_access::DbChannel::new(&db, "K:z.VELO")
        .put_f64_process(7.0)
        .await
        .unwrap();
    eventually_near(&server, "K:m:ZVel", 7.0).await;
    assert_eq!(f64_of(&server, "K:m:ThetaDmov").await, 1.0);
}

/// A theta move made outside kohzuCtl_soft posts many readbacks; updateRdbk's
/// efClear(thetaMotRdbk_mon) drops the ones queued while it waited, so C
/// runs one "Only Theta Motor Moved" pass per move (C kohzuIoc oracle).
#[tokio::test(flavor = "multi_thread")]
async fn a_direct_theta_move_runs_one_only_theta_motor_moved_pass() {
    let server = boot([10.0, -17.77, 100.78]).await;
    settled(&server, "y offset changed to 17.500000").await;
    let db = (**server.database()).clone();
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
            _ => motor.stat.dmov = v != 0.0,
        }
        inst.notify_field(field, EventMask::VALUE | EventMask::LOG);
    };
    post("DMOV", 0.0);
    post("RBV", 50.0);
    // updateRdbk's readback pass shows the first RBV was taken.
    eventually_near(&server, "K:m:ThetaRdbk", 50.0).await;
    post("RBV", 70.0);
    post("RBV", 89.5);
    post("DMOV", 1.0);
    tokio::time::timeout(Duration::from_secs(10), theta.recv_f64())
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
    eventually_near(&server, "K:m:Theta.HLM", 85.0).await;
    while theta.try_recv_event().is_ok() {
        passes += 1;
    }
    assert_eq!(passes, 1, "Only Theta Motor Moved passes");
}

async fn use_set_and_motor_sets(server: &CaServer) -> [f64; 4] {
    [
        f64_of(server, "K:m:UseSet").await,
        f64_of(server, "K:th.SET").await,
        f64_of(server, "K:y.SET").await,
        f64_of(server, "K:z.SET").await,
    ]
}

/// updateSet (kohzuCtl_soft.st:1243-1286) runs on the connect-time monitors with UseSet first,
/// so UseSet's value wins: a preset motor .SET is put back (C kohzuIoc
/// oracle: th.SET=Set before seq gives all Use; UseSet=Set gives all Set).
#[tokio::test(flavor = "multi_thread")]
async fn use_set_drives_the_motor_set_fields_at_connect() {
    let server = boot_with([10.0, -17.77, 100.78], &[("K:th.SET", 1.0)]).await;
    settled(&server, "y offset changed to 17.500000").await;
    assert_eq!(use_set_and_motor_sets(&server).await, [0.0; 4]);
    let server = boot_with([10.0, -17.77, 100.78], &[("K:m:UseSet", 1.0)]).await;
    settled(&server, "y offset changed to 17.500000").await;
    assert_eq!(use_set_and_motor_sets(&server).await, [1.0; 4]);
}

/// A motor .SET put reaches UseSet, and UseSet the other two motors
/// (C kohzuIoc oracle: y.SET=Set leaves all four Set).
#[tokio::test(flavor = "multi_thread")]
async fn a_motor_set_put_drives_use_set_and_the_other_motors() {
    let server = boot([10.0, -17.77, 100.78]).await;
    settled(&server, "y offset changed to 17.500000").await;
    let db = (**server.database()).clone();
    optics_rs::db_access::DbChannel::new(&db, "K:y.SET")
        .put_f64_process(1.0)
        .await
        .unwrap();
    eventually(|| async {
        match use_set_and_motor_sets(&server).await {
            [1.0, 1.0, 1.0, 1.0] => Ok(()),
            sets => Err(format!("UseSet and motor SETs {sets:?}")),
        }
    })
    .await;
}

/// Poll `pv` until it is within [`assert_near`]'s tolerance of `want`, then
/// assert it.
async fn eventually_near(server: &CaServer, pv: &str, want: f64) {
    for _ in 0..100 {
        if (f64_of(server, pv).await - want).abs() <= 1e-4 * want.abs().max(1.0) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_near(server, pv, want).await;
}

/// Two E puts queued before the sequencer looks raise E_mon once, and C's
/// sync'd variable holds the later value, so theta follows the second put.
#[tokio::test(flavor = "current_thread")]
async fn two_queued_e_puts_take_the_later_value() {
    let server = boot([10.0, -17.77, 100.78]).await;
    settled(&server, "y offset changed to 17.500000").await;
    let db = (**server.database()).clone();
    // Both posts land in one step with no await, so the sequencer, on this
    // one-thread runtime, cannot take the first before the second is queued.
    {
        let rec = db.get_record("K:m:E").unwrap();
        let mut inst = rec.write();
        for e in [12.0, 14.0] {
            inst.record.put_field("VAL", EpicsValue::Double(e)).unwrap();
            inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
        }
    }
    let two_d = f64_of(&server, "K:m:2dSpacing").await;
    let theta = optics_rs::snl::kohzu_ctl::lambda_to_theta(
        optics_rs::snl::kohzu_ctl::energy_to_lambda(14.0),
        two_d,
    )
    .unwrap();
    eventually_near(&server, "K:m:Theta", theta).await;
    assert_near(&server, "K:m:E", 14.0).await;
}

/// E_mon enters eChanged on any change, however small, and C writes
/// autoMode as its monitor arrives (kohzuCtl_soft.st:74-76, no efflag): an
/// E put 5e-13 off its standing value, queued with Mode=Auto, moves Y back
/// from where it was driven (C kohzuIoc oracle: y=5, E+5e-13 → y.VAL
/// -17.77), though E is taken before Mode.
#[tokio::test(flavor = "current_thread")]
async fn an_e_put_below_1e_12_off_still_moves() {
    let server = boot([10.0, -17.77, 100.78]).await;
    settled(&server, "y offset changed to 17.500000").await;
    let db = (**server.database()).clone();
    db.get_record("K:y")
        .unwrap()
        .write()
        .record
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
        .unwrap()
        .pos
        .val = 5.0;
    let e = f64_of(&server, "K:m:E").await;
    // No await between the posts: on this one-thread runtime the sequencer
    // finds both queued.
    for (pv, v) in [
        ("K:m:Mode", EpicsValue::Enum(1)),
        ("K:m:E", EpicsValue::Double(e + 5e-13)),
    ] {
        let rec = db.get_record(pv).unwrap();
        let mut inst = rec.write();
        inst.record.put_field("VAL", v).unwrap();
        inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
    }
    eventually_near(&server, "K:y.VAL", -17.77).await;
}

/// kohzuMoving is a monitor variable (kohzuCtl_soft.st:83-85): a Moving
/// write that posts nothing (dbPut through an NPP link, dbAccess.c:1416)
/// leaves it 0, so the next waitForCmndEnter does not clear Moving (C
/// kohzuIoc oracle: Moving stays Busy, "kohzuMoving=0; kohzuDone=1").
#[tokio::test(flavor = "current_thread")]
async fn a_moving_write_with_no_monitor_is_not_cleared() {
    let server = boot([10.0, -17.77, 100.78]).await;
    settled(&server, "y offset changed to 17.500000").await;
    let db = (**server.database()).clone();
    db.put_record_field_from_ca("K:np", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(f64_of(&server, "K:m:Moving").await, 1.0);
    db.put_record_field_from_ca("K:th", "HLM", EpicsValue::Double(80.0))
        .await
        .unwrap();
    settled(&server, "Theta Motor Soft Limits Changed").await;
    assert_eq!(f64_of(&server, "K:m:Moving").await, 1.0);
}
