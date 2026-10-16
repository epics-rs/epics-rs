#![cfg(tokio_backend)]
//! kohzuCtl's boot ends where C's does. initSequence's two theta passes
//! (`kohzuCtl.st:581`, `:595-603`) clamp against C's unset thetaHi/thetaLo
//! and are rolled back. The first waitForCmnd then takes the connect-time
//! monitors its waitForCmndEnter leaves set, in its order: thetaLimits, the
//! theta readback (thetaMotStopped `!causedMove`), then yOffset. A zero
//! readback holds init in chkMotorLimits.
//!
//! Expected values are a C kohzuIoc's (sequencer 2.2.9, optics, motorSim
//! axes with these limits) after the same boot.

use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::{CaServer, CaServerBuilder};
use optics_rs::snl::kohzu_ctl::{self, KohzuConfig};
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

const KOHZU_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/kohzuSeq.db");
const MOTORS: &str = r#"
record(motor, "K:th") { field(HLM, "90") field(LLM, "-10") }
record(motor, "K:y") { field(HLM, "250") field(LLM, "-250") }
record(motor, "K:z") { field(HLM, "250") field(LLM, "-250") }
"#;

/// Boot with these theta, Y and Z readbacks.
async fn boot(rbv: [f64; 3]) -> CaServer {
    boot_with(rbv, &[]).await
}

/// [`boot`], with these PVs put before the sequencer connects.
async fn boot_with(rbv: [f64; 3], presets: &[(&str, f64)]) -> CaServer {
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
    for (pv, v) in presets {
        optics_rs::db_access::DbChannel::new(&db, pv)
            .put_f64_process(*v)
            .await
            .unwrap();
    }
    tokio::spawn(kohzu_ctl::run(
        KohzuConfig::new("K:", "th", "y", "z", 0),
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
        if string_of(server, "K:KohzuSeqMsg1SI").await == msg1 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "SeqMsg1 never read {msg1:?}; last {:?}",
        string_of(server, "K:KohzuSeqMsg1SI").await
    );
}

fn near(got: f64, want: f64) -> bool {
    (got - want).abs() <= 1e-4 * want.abs().max(1.0)
}

async fn assert_near(server: &CaServer, pv: &str, want: f64) {
    let got = f64_of(server, pv).await;
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
    let got = string_of(server, "K:KohzuSeqMsg2SI").await;
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
        let got = f64_of(server, pv).await;
        if !near(got, want) {
            return Err(format!("{pv}: {got}, C {want}"));
        }
    }
    Ok(())
}

/// thetaLimits sets 1..89, the readback pass moves theta to it, and the
/// yOffset pass recomputes Y/Z for it: nothing is rolled back.
#[tokio::test(flavor = "multi_thread")]
async fn a_readback_inside_the_limits_stands_after_the_connect_time_passes() {
    let server = boot([10.0, -17.77, 100.78]).await;
    settled(&server, "y offset changed to 17.500000").await;
    assert_state(
        &server,
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
#[tokio::test(flavor = "multi_thread")]
async fn a_readback_below_theta_lo_is_rolled_back_to_the_drive_limits() {
    let server = boot([0.5, -17.501, 200.0]).await;
    settled(&server, "Z will exceed soft limits").await;
    // The readback pass rolls back first; wait out the yOffset pass.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_state(
        &server,
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
#[tokio::test(flavor = "multi_thread")]
async fn a_zero_readback_holds_init_in_chk_motor_limits() {
    let server = boot([0.0, 0.0, 0.0]).await;
    settled(&server, "Theta Readback of 0 not reasonable").await;
    // Past two of chkMotorLimits' 2 s rounds and initSeq=3's 1 s delay.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        string_of(&server, "K:KohzuSeqMsg1SI").await,
        "Theta Readback of 0 not reasonable"
    );
    let msg2 = string_of(&server, "K:KohzuSeqMsg2SI").await;
    assert!(
        msg2 == "Set to Manual Mode" || msg2 == "Modify Motor Positions/Limits",
        "SeqMsg2 {msg2:?}"
    );
    assert_state(
        &server,
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
    assert_eq!(f64_of(&server, "K:BraggEAO.DRVL").await, f64::INFINITY);
}

/// A lattice change runs dInputChanged into thChanged: theta stands, E and
/// lambda follow the new 2d, and the manual-mode pass leaves KohzuMoving
/// raised by the setpoint puts.
#[tokio::test(flavor = "multi_thread")]
async fn a_lattice_change_recomputes_e_and_lambda_for_the_standing_theta() {
    let server = boot([10.0, -17.77, 100.78]).await;
    settled(&server, "y offset changed to 17.500000").await;
    let db = (**server.database()).clone();
    optics_rs::db_access::DbChannel::new(&db, "K:BraggHAO")
        .put_f64_process(3.0)
        .await
        .unwrap();
    settled(&server, "New d spacing").await;
    assert_eq!(
        string_of(&server, "K:KohzuSeqMsg2SI").await,
        "Set to Manual Mode"
    );
    assert_eq!(f64_of(&server, "K:KohzuMoving").await, 1.0, "KohzuMoving");
    assert_eq!(f64_of(&server, "K:KohzuAlertBO").await, 0.0, "KohzuAlertBO");
    assert_eq!(f64_of(&server, "K:KohzuModeBO").await, 0.0, "KohzuModeBO");
    for (pv, want) in [
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
    ] {
        assert_near(&server, pv, want).await;
    }
}

/// updatePsuedo echoes each motor field monitor on its own, so the echo
/// PVs follow the motors while init holds in chkMotorLimits.
#[tokio::test(flavor = "multi_thread")]
async fn the_echo_pvs_follow_the_motors_while_init_holds() {
    let server = boot([0.0, 0.0, 0.0]).await;
    settled(&server, "Theta Readback of 0 not reasonable").await;
    let db = (**server.database()).clone();
    optics_rs::db_access::DbChannel::new(&db, "K:z.VELO")
        .put_f64_process(7.0)
        .await
        .unwrap();
    eventually_near(&server, "K:KohzuZVelAI", 7.0).await;
    assert_eq!(f64_of(&server, "K:KohzuThetaDmovBI").await, 1.0);
}

/// A theta move made outside kohzuCtl posts many readbacks; updateRdbk's
/// efClear(thetaMotRdbk_mon) drops the ones queued while it waited, so C
/// runs one "Only Theta Motor Moved" pass per move (C kohzuIoc oracle).
#[tokio::test(flavor = "multi_thread")]
async fn a_direct_theta_move_runs_one_only_theta_motor_moved_pass() {
    let server = boot([10.0, -17.77, 100.78]).await;
    settled(&server, "y offset changed to 17.500000").await;
    let db = (**server.database()).clone();
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
            _ => motor.stat.dmov = v != 0.0,
        }
        inst.notify_field(field, EventMask::VALUE | EventMask::LOG);
    };
    post("DMOV", 0.0);
    post("RBV", 50.0);
    // updateRdbk's readback pass shows the first RBV was taken.
    eventually_near(&server, "K:BraggThetaRdbkAO", 50.0).await;
    post("RBV", 70.0);
    post("RBV", 89.5);
    post("DMOV", 1.0);
    let mut passes = 0;
    while passes == 0 {
        let v = tokio::time::timeout(Duration::from_secs(10), theta.recv_f64())
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
    eventually_near(&server, "K:BraggThetaAO.DRVH", 85.0).await;
    while let Ok(event) = theta.try_recv_event() {
        let v = event.value().as_value().and_then(EpicsValue::to_f64);
        passes += usize::from(v == Some(89.0));
    }
    assert_eq!(passes, 1, "Only Theta Motor Moved passes");
}

async fn use_set_and_motor_sets(server: &CaServer) -> [f64; 4] {
    [
        f64_of(server, "K:KohzuUseSetBO").await,
        f64_of(server, "K:th.SET").await,
        f64_of(server, "K:y.SET").await,
        f64_of(server, "K:z.SET").await,
    ]
}

/// updateSet (kohzuCtl.st:1387-1430) runs on the connect-time monitors with UseSet first,
/// so UseSet's value wins: a preset motor .SET is put back (C kohzuIoc
/// oracle: th.SET=Set before seq gives all Use; UseSet=Set gives all Set).
#[tokio::test(flavor = "multi_thread")]
async fn use_set_drives_the_motor_set_fields_at_connect() {
    let server = boot_with([10.0, -17.77, 100.78], &[("K:th.SET", 1.0)]).await;
    settled(&server, "y offset changed to 17.500000").await;
    assert_eq!(use_set_and_motor_sets(&server).await, [0.0; 4]);
    let server = boot_with([10.0, -17.77, 100.78], &[("K:KohzuUseSetBO", 1.0)]).await;
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

/// A tweak past its limit leaves the setpoint but still runs C's
/// eChanged/lChanged/thChanged chain (kohzuCtl.st:747-808), so in auto
/// mode it moves Y back from where it was driven (C kohzuIoc oracle: y=5,
/// E tweak 1000 → y.VAL -17.77; the same for lambda and theta).
#[tokio::test(flavor = "multi_thread")]
async fn a_tweak_past_the_limit_moves_to_the_standing_setpoint() {
    for (step, inc) in [
        ("K:BraggETweakAI", "K:BraggEIncBO"),
        ("K:BraggLambdaTweakAI", "K:BraggLambdaIncBO"),
        ("K:BraggThetaTweakAI", "K:BraggThetaIncBO"),
    ] {
        let server = boot([10.0, -17.77, 100.78]).await;
        settled(&server, "y offset changed to 17.500000").await;
        let db = (**server.database()).clone();
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
            let msg1 = string_of(&server, "K:KohzuSeqMsg1SI").await;
            match msg1.ends_with("tweak exceeds limit") {
                true => Ok(()),
                false => Err(format!("{inc}: SeqMsg1 {msg1:?}")),
            }
        })
        .await;
        eventually_near(&server, "K:y.VAL", -17.77).await;
    }
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
        let rec = db.get_record("K:BraggEAO").unwrap();
        let mut inst = rec.write();
        for e in [12.0, 14.0] {
            inst.record.put_field("VAL", EpicsValue::Double(e)).unwrap();
            inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
        }
    }
    let two_d = f64_of(&server, "K:Bragg2dSpacingAO").await;
    let theta = optics_rs::snl::kohzu_ctl::lambda_to_theta(
        optics_rs::snl::kohzu_ctl::energy_to_lambda(14.0),
        two_d,
    )
    .unwrap();
    eventually_near(&server, "K:BraggThetaAO", theta).await;
    assert_near(&server, "K:BraggEAO", 14.0).await;
}

/// C writes autoMode as its monitor arrives (kohzuCtl.st:78-80, no efflag),
/// so an E put queued with Mode=Auto already sees auto mode and moves Y
/// back from where it was driven, though E is taken before Mode.
#[tokio::test(flavor = "current_thread")]
async fn an_e_put_queued_after_auto_mode_moves() {
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
    let e = f64_of(&server, "K:BraggEAO").await;
    // No await between the posts: on this one-thread runtime the sequencer
    // finds both queued.
    for (pv, v) in [
        ("K:KohzuModeBO", EpicsValue::Enum(1)),
        ("K:BraggEAO", EpicsValue::Double(e + 1e-6)),
    ] {
        let rec = db.get_record(pv).unwrap();
        let mut inst = rec.write();
        inst.record.put_field("VAL", v).unwrap();
        inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
    }
    eventually_near(&server, "K:y.VAL", -17.77).await;
}
