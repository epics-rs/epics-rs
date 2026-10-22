//! The compiled `flexCombinedMotion.st` on the records it uses from
//! `flexCombinedMotion.db`, with both axes on simulated motors and the cap
//! sensor an `ao` the test sets while the coarse axis moves. One case per
//! boundary of the state machine.
//!
//! Where the hand port these replace differed, the case holds C's
//! behaviour: C leaves busy alone at start and never sets it (the
//! setPoint record's FLNK does), drops the setPoint and stop events of
//! connect time, uses the limit, deadband and retry PVs as they are (no
//! substitute for a zero), limits a fine-mode move with if / else if, takes
//! a coarse DMOV only in waitCoarse and a setPoint only in idle, holds in
//! afterMove while the mode is fine, and lets a coarse move run to its end
//! when a stop comes during it.
//!
//! `PVPUT` is a SYNC put (`seqPVmacros.h:167-171`), so moveCoarse holds
//! until the coarse axis is done. Every case leaves the program idle before
//! `seq_stop`.

use epics_base_rs::runtime::task;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use epics_seq_rs::{SeqThreadId, seq, seq_stop};
use optics_rs::snl::flex_combined_motion::flexCombinedMotion;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const FLEX_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/flexCombinedMotion.db");

/// The fine axis completes a move within a sim step. The coarse axis takes
/// a second or more for the moves below, the window in which the test
/// moves the cap sensor.
const AXES: &str = r#"
record(motor, "X:fm") { field(DTYP, "asynMotor") field(OUT, "@asyn($(PORT)fm,0)") field(MRES, "0.0001") field(VELO, "1000") field(ACCL, "0.01") }
record(motor, "X:cm") { field(DTYP, "asynMotor") field(OUT, "@asyn($(PORT)cm,0)") field(MRES, "0.001") field(VELO, "20") field(ACCL, "1") }
record(ao, "X:cap:pos") { }
"#;

/// The template from its program records on: the combined soft motor ahead
/// of them needs devMotorSoft, which motor-rs does not have, and the
/// program does not use it.
fn program_records() -> String {
    let db = std::fs::read_to_string(FLEX_DB).unwrap();
    let (_, records) = db
        .split_once("### Records that are used by the sequence program")
        .unwrap();
    records.to_string()
}

fn sim_axes() -> String {
    use motor_rs::builder::{AxisSetup, MotorBuilder};
    use std::sync::Mutex;
    static BOOTS: AtomicUsize = AtomicUsize::new(0);
    let port = format!("FLEXSIM{}_", BOOTS.fetch_add(1, Ordering::Relaxed));
    for m in ["fm", "cm"] {
        let name = format!("{port}{m}");
        let motor: Arc<Mutex<dyn asyn_rs::interfaces::motor::AsynMotor>> = Arc::new(Mutex::new(
            motor_rs::sim_motor::SimMotor::new().with_limits(-1e9, 1e9),
        ));
        let AxisSetup {
            axis, poll_loop, ..
        } = MotorBuilder::new(motor)
            .trace_user(motor_rs::trace::TraceUser::Asyn {
                port: name.clone(),
                addr: 0,
            })
            .poll_interval(Duration::from_millis(10))
            .build_axis();
        motor_rs::devices::register_controller_port(&asyn_rs::registry::open_owner(), &name, 1, ())
            .unwrap();
        motor_rs::devices::register_axis(axis).unwrap();
        poll_loop.start();
    }
    port
}

struct Flex {
    ioc: Arc<PvDatabase>,
    tid: SeqThreadId,
    _scan: ScanOwner,
}

/// Boot with limits ±10, home 0, and `presets` put before the program
/// starts. retries reads -1 until the program takes a setPoint.
async fn boot(presets: &[(&str, f64)]) -> Flex {
    let port = sim_axes();
    let macros: HashMap<String, String> = [
        ("P", "X:"),
        ("M", "flex"),
        ("C", "cap"),
        ("HLM", "10"),
        ("LLM", "-10"),
        ("HOME", "0"),
        ("PORT", &port),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let (name, factory) = motor_rs::motor_record_factory();
    let ioc = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(
            &motor_rs::devices::devices(),
            motor_rs::devices::device_support_factory(),
        )
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("sseq", || {
            Box::new(epics_base_rs::server::records::sseq::SseqRecord::default())
        })
        .db_string(AXES, &macros)
        .unwrap()
        .db_string(&program_records(), &macros)
        .unwrap()
        .build_served()
        .await
        .unwrap()
        .0;
    put(&ioc, "X:flex:retries", -1.0).await;
    for (pv, v) in presets {
        put(&ioc, pv, *v).await;
    }
    // The program waits for PINI, which the scan owner's pass publishes.
    let scan = ScanOwner::start(ioc.clone());
    let tid = seq(
        &flexCombinedMotion,
        "P=X:,M=flex,C=cap,FM=fm,CM=cm",
        0,
        &ioc,
    )
    .expect("flexCombinedMotion starts");
    Flex {
        ioc,
        tid,
        _scan: scan,
    }
}

/// A CA put, which processes the record as a client's would.
async fn put(ioc: &PvDatabase, pv: &str, v: f64) {
    let (record, field) = pv.split_once('.').unwrap_or((pv, "VAL"));
    ioc.put_record_field_from_ca(record, field, EpicsValue::Double(v))
        .await
        .unwrap();
}

fn f64_of(ioc: &PvDatabase, pv: &str) -> f64 {
    ioc.get_pv(pv).unwrap().to_f64().unwrap()
}

fn near(got: f64, want: f64) -> bool {
    (got - want).abs() <= 1e-9
}

/// Wait until `pv` reads `want`; panic after 10 s.
async fn reaches(ioc: &PvDatabase, pv: &str, want: f64) {
    for _ in 0..1000 {
        if near(f64_of(ioc, pv), want) {
            return;
        }
        task::sleep(Duration::from_millis(10)).await;
    }
    panic!("{pv} = {}, C {want}", f64_of(ioc, pv));
}

/// `pv` reads `want` now and for the next half second.
async fn holds(ioc: &PvDatabase, pv: &str, want: f64) {
    for _ in 0..50 {
        let got = f64_of(ioc, pv);
        assert!(near(got, want), "{pv} = {got}, C {want}");
        task::sleep(Duration::from_millis(10)).await;
    }
}

/// Put setPoint until idle takes it, which its `PVPUT(retries, 0)` shows.
/// A put landing before init's efClear is dropped, so it is repeated.
async fn command(f: &Flex, sp: f64) {
    put(&f.ioc, "X:flex:retries", -1.0).await;
    for _ in 0..10 {
        put(&f.ioc, "X:flex:setPoint", sp).await;
        for _ in 0..100 {
            if f64_of(&f.ioc, "X:flex:retries") != -1.0 {
                return;
            }
            task::sleep(Duration::from_millis(10)).await;
        }
    }
    panic!("setPoint {sp} was never taken");
}

/// Wait until the coarse axis is moving.
async fn coarse_moving(f: &Flex) {
    reaches(&f.ioc, "X:cm.DMOV", 0.0).await;
}

/// The move ends in resetBusy.
async fn done(f: &Flex) {
    reaches(&f.ioc, "X:flex:busy", 0.0).await;
}

/// init's efClears (`flexCombinedMotion.st:81-82`) drop the setPoint and
/// stop events of connect time, and nothing touches busy, which the
/// setPoint record's FLNK left at 1.
#[epics_base_rs::epics_test]
async fn connect_time_set_point_and_stop_are_dropped() {
    let f = boot(&[("X:flex:setPoint", 5.0), ("X:flex:stop", 1.0)]).await;
    let mut coarse_stop = DbSubscription::subscribe(&f.ioc, "X:cm.STOP")
        .await
        .unwrap();
    task::sleep(Duration::from_millis(500)).await;
    holds(&f.ioc, "X:flex:retries", -1.0).await;
    holds(&f.ioc, "X:flex:busy", 1.0).await;
    holds(&f.ioc, "X:fm.VAL", 0.0).await;
    while let Ok(event) = coarse_stop.try_recv_event() {
        let v = event.value().as_value().and_then(EpicsValue::to_f64);
        assert_ne!(v, Some(1.0), "the connect-time stop was taken");
    }
    // The program is running: the same set point put again is taken.
    command(&f, 5.0).await;
    done(&f).await;
    reaches(&f.ioc, "X:fm.VAL", 5.0).await;
    seq_stop(f.tid);
}

/// Fine mode moves the fine axis by setPoint - pos + fine RBV (`:127`,
/// `:131-145`, `:314-323`) and never the coarse one.
#[epics_base_rs::epics_test]
async fn fine_mode_moves_only_the_fine_axis() {
    let f = boot(&[("X:flex:mode", 3.0), ("X:cap:pos", 5.0)]).await;
    command(&f, 8.0).await;
    done(&f).await;
    reaches(&f.ioc, "X:fm.VAL", 3.0).await;
    holds(&f.ioc, "X:cm.VAL", 0.0).await;
    assert_eq!(f64_of(&f.ioc, "X:flex:retries"), 0.0);
    seq_stop(f.tid);
}

/// Fine mode limits the move to the fine range (`:134-143`).
#[epics_base_rs::epics_test]
async fn fine_mode_limits_the_move_to_the_fine_range() {
    let f = boot(&[
        ("X:flex:mode", 3.0),
        ("X:flex:upperLimit", 3.0),
        ("X:flex:lowerLimit", -3.0),
    ])
    .await;
    command(&f, 10.0).await;
    done(&f).await;
    reaches(&f.ioc, "X:fm.VAL", 3.0).await;
    reaches(&f.ioc, "X:fm.RBV", 3.0).await;
    // -10 - 0 + 3 is under the lower limit.
    command(&f, -10.0).await;
    done(&f).await;
    reaches(&f.ioc, "X:fm.VAL", -3.0).await;
    seq_stop(f.tid);
}

/// The limit test is if / else if, upper first (`:134-143`): with the limits crossed, a
/// move over the upper one takes it and one under it takes the lower.
#[epics_base_rs::epics_test]
async fn crossed_fine_limits_take_the_upper_test_first() {
    let f = boot(&[
        ("X:flex:mode", 3.0),
        ("X:flex:upperLimit", -1.0),
        ("X:flex:lowerLimit", 1.0),
    ])
    .await;
    command(&f, 5.0).await;
    done(&f).await;
    reaches(&f.ioc, "X:fm.VAL", -1.0).await;
    reaches(&f.ioc, "X:fm.RBV", -1.0).await;
    // -5 - 0 - 1 is not over -1, and is under 1.
    command(&f, -5.0).await;
    done(&f).await;
    reaches(&f.ioc, "X:fm.VAL", 1.0).await;
    seq_stop(f.tid);
}

/// The same set point again still runs calcDistance (`:105-112`), which
/// re-locks the feedback after the sensor drifted.
#[epics_base_rs::epics_test]
async fn the_same_set_point_again_relocks_the_feedback() {
    let f = boot(&[("X:flex:mode", 3.0), ("X:cap:pos", 5.0)]).await;
    command(&f, 8.0).await;
    done(&f).await;
    reaches(&f.ioc, "X:fm.RBV", 3.0).await;
    put(&f.ioc, "X:cap:pos", 6.0).await;
    command(&f, 8.0).await;
    done(&f).await;
    reaches(&f.ioc, "X:fm.VAL", 5.0).await;
    seq_stop(f.tid);
}

/// Fast mode inside the open fine range moves only the fine axis
/// (`:159-164`).
#[epics_base_rs::epics_test]
async fn fast_mode_inside_the_fine_range_moves_only_the_fine_axis() {
    let f = boot(&[]).await;
    command(&f, 5.0).await;
    done(&f).await;
    reaches(&f.ioc, "X:fm.VAL", 5.0).await;
    holds(&f.ioc, "X:cm.VAL", 0.0).await;
    seq_stop(f.tid);
}

/// A move landing on a limit is out of the fine range (`:153`): the fine
/// axis goes home, the coarse one moves the rest, and fast mode then
/// finishes on the fine axis from the sensor (`:250-256`, `:306-309`).
#[epics_base_rs::epics_test]
async fn fast_mode_at_a_limit_moves_coarse_then_finishes_fine() {
    let f = boot(&[]).await;
    command(&f, 10.0).await;
    coarse_moving(&f).await;
    assert_eq!(f64_of(&f.ioc, "X:cm.VAL"), 10.0);
    put(&f.ioc, "X:cap:pos", 9.5).await;
    done(&f).await;
    reaches(&f.ioc, "X:fm.VAL", 0.5).await;
    assert_eq!(f64_of(&f.ioc, "X:cm.VAL"), 10.0);
    seq_stop(f.tid);
}

/// homeFine puts a home outside the limits on the nearer limit
/// (`:172-185`), and the coarse move is what is left (`:196`). One boot
/// per test: a second program in the process attaches to the first one's
/// pv system (`create_or_attach_pv_system`), which resolves names in the
/// first database.
async fn home_outside_the_limits(home: f64, fine: f64, coarse: f64) {
    let f = boot(&[("X:flex:mode", 2.0), ("X:flex:homePos", home)]).await;
    command(&f, 25.0).await;
    coarse_moving(&f).await;
    reaches(&f.ioc, "X:fm.VAL", fine).await;
    assert_eq!(f64_of(&f.ioc, "X:cm.VAL"), coarse);
    put(&f.ioc, "X:cap:pos", 25.0).await;
    done(&f).await;
    seq_stop(f.tid);
}

#[epics_base_rs::epics_test]
async fn a_home_over_the_high_limit_is_put_on_it() {
    home_outside_the_limits(20.0, 10.0, 15.0).await;
}

#[epics_base_rs::epics_test]
async fn a_home_under_the_low_limit_is_put_on_it() {
    home_outside_the_limits(-20.0, -10.0, 35.0).await;
}

/// Coarse mode parks the fine axis at home even for a move inside the fine
/// range (`:147-151`) and ends on the coarse axis once the error is inside
/// the deadband (`:296-300`).
#[epics_base_rs::epics_test]
async fn coarse_mode_parks_the_fine_axis_at_home() {
    let f = boot(&[("X:flex:mode", 2.0), ("X:flex:homePos", -8.0)]).await;
    command(&f, 1.0).await;
    coarse_moving(&f).await;
    reaches(&f.ioc, "X:fm.VAL", -8.0).await;
    assert_eq!(f64_of(&f.ioc, "X:cm.VAL"), 9.0);
    put(&f.ioc, "X:cap:pos", 1.0).await;
    done(&f).await;
    holds(&f.ioc, "X:fm.VAL", -8.0).await;
    assert_eq!(f64_of(&f.ioc, "X:flex:retries"), 0.0);
    seq_stop(f.tid);
}

/// Precise mode moves the coarse axis again by the error while it is over
/// the deadband, counting retries (`:281-288`), then finishes fine.
#[epics_base_rs::epics_test]
async fn precise_mode_retries_until_inside_the_deadband() {
    let f = boot(&[("X:flex:mode", 1.0)]).await;
    command(&f, 20.0).await;
    coarse_moving(&f).await;
    put(&f.ioc, "X:cap:pos", 15.0).await;
    reaches(&f.ioc, "X:flex:retries", 1.0).await;
    coarse_moving(&f).await;
    assert_eq!(f64_of(&f.ioc, "X:cm.VAL"), 25.0);
    put(&f.ioc, "X:cap:pos", 19.99).await;
    done(&f).await;
    reaches(&f.ioc, "X:fm.VAL", 20.0 - 19.99).await;
    assert_eq!(f64_of(&f.ioc, "X:flex:retries"), 1.0);
    seq_stop(f.tid);
}

/// A deadband under the coarse resolution is the resolution (`:268-271`):
/// an error between the two is not retried.
#[epics_base_rs::epics_test]
async fn a_deadband_under_the_coarse_resolution_is_the_resolution() {
    let f = boot(&[("X:flex:mode", 1.0), ("X:flex:deadband", 0.0001)]).await;
    command(&f, 20.0).await;
    coarse_moving(&f).await;
    put(&f.ioc, "X:cap:pos", 19.9995).await;
    done(&f).await;
    reaches(&f.ioc, "X:fm.VAL", 20.0 - 19.9995).await;
    assert_eq!(f64_of(&f.ioc, "X:flex:retries"), 0.0);
    seq_stop(f.tid);
}

/// maxRetries 0 gives up after the first coarse move, writing nothing to
/// the fine axis (`:275-279`).
#[epics_base_rs::epics_test]
async fn max_retries_zero_gives_up_after_one_coarse_move() {
    let f = boot(&[("X:flex:mode", 1.0), ("X:flex:maxRetries", 0.0)]).await;
    command(&f, 20.0).await;
    coarse_moving(&f).await;
    put(&f.ioc, "X:cap:pos", 19.0).await;
    done(&f).await;
    holds(&f.ioc, "X:fm.VAL", 0.0).await;
    assert_eq!(f64_of(&f.ioc, "X:cm.VAL"), 20.0);
    assert_eq!(f64_of(&f.ioc, "X:flex:retries"), 0.0);
    seq_stop(f.tid);
}

/// abort stops the coarse axis 0.15 s after stop, then resetStop resets
/// stop and resetBusy busy (`:226-245`, `:335`).
async fn stops_the_coarse_axis(f: &Flex, coarse_stop: &mut DbSubscription) {
    loop {
        let v = task::timeout(Duration::from_secs(10), coarse_stop.recv_f64())
            .await
            .expect("no coarse STOP put")
            .unwrap();
        if v == 1.0 {
            break;
        }
    }
    done(f).await;
    reaches(&f.ioc, "X:flex:stop", 0.0).await;
}

/// A stop during a coarse move does not cut it short: moveCoarse holds in
/// its SYNC put until the axis is done, waitCoarse then takes the DMOV
/// ahead of the stop (`:206`, `:214-223`), and idle takes the stop once the move
/// ends (`:97-100`).
#[epics_base_rs::epics_test]
async fn a_stop_during_a_coarse_move_is_taken_after_it() {
    let f = boot(&[("X:flex:mode", 2.0)]).await;
    let mut coarse_stop = DbSubscription::subscribe(&f.ioc, "X:cm.STOP")
        .await
        .unwrap();
    command(&f, 20.0).await;
    coarse_moving(&f).await;
    put(&f.ioc, "X:flex:stop", 1.0).await;
    put(&f.ioc, "X:cap:pos", 20.0).await;
    reaches(&f.ioc, "X:cm.RBV", 20.0).await;
    stops_the_coarse_axis(&f, &mut coarse_stop).await;
    seq_stop(f.tid);
}

/// idle takes a stop by efTest (`:97-100`): with nothing moving, it still
/// stops the coarse axis and resets busy.
#[epics_base_rs::epics_test]
async fn a_stop_in_idle_stops_the_coarse_axis() {
    let f = boot(&[]).await;
    let mut coarse_stop = DbSubscription::subscribe(&f.ioc, "X:cm.STOP")
        .await
        .unwrap();
    // A command first, so the stop below lands after init's efClear.
    command(&f, 0.0).await;
    done(&f).await;
    put(&f.ioc, "X:flex:busy", 1.0).await;
    put(&f.ioc, "X:flex:stop", 1.0).await;
    stops_the_coarse_axis(&f, &mut coarse_stop).await;
    seq_stop(f.tid);
}

/// Only waitCoarse tests the coarse DMOV (`:214`): a coarse move made
/// outside the program, while it idles, moves nothing and leaves busy.
#[epics_base_rs::epics_test]
async fn a_coarse_move_outside_the_program_is_ignored() {
    let f = boot(&[]).await;
    command(&f, 0.0).await;
    done(&f).await;
    put(&f.ioc, "X:flex:busy", 1.0).await;
    put(&f.ioc, "X:cm", 1.0).await;
    coarse_moving(&f).await;
    reaches(&f.ioc, "X:cm.DMOV", 1.0).await;
    holds(&f.ioc, "X:flex:busy", 1.0).await;
    holds(&f.ioc, "X:fm.VAL", 0.0).await;
    seq_stop(f.tid);
}

/// Only idle takes a setPoint (`:88-95`): one put during a coarse move
/// waits for the move to finish, then starts the next.
#[epics_base_rs::epics_test]
async fn a_set_point_during_a_coarse_move_waits_for_idle() {
    let f = boot(&[]).await;
    let mut fine = DbSubscription::subscribe(&f.ioc, "X:fm.VAL").await.unwrap();
    command(&f, 20.0).await;
    coarse_moving(&f).await;
    put(&f.ioc, "X:flex:retries", -1.0).await;
    put(&f.ioc, "X:flex:setPoint", 30.0).await;
    put(&f.ioc, "X:cap:pos", 19.5).await;
    holds(&f.ioc, "X:flex:retries", -1.0).await;
    // The first move finishes fine at 0.5; 30 - 19.5 + 0.5 is then over
    // the limit, so the next homes the fine axis and moves coarse by 11.
    reaches(&f.ioc, "X:flex:retries", 0.0).await;
    // The first move's resetBusy cleared busy; set it to see the second end.
    put(&f.ioc, "X:flex:busy", 1.0).await;
    reaches(&f.ioc, "X:cm.VAL", 31.0).await;
    put(&f.ioc, "X:cap:pos", 30.0).await;
    done(&f).await;
    let mut seen = Vec::new();
    while let Ok(event) = fine.try_recv_event() {
        seen.extend(event.value().as_value().and_then(EpicsValue::to_f64));
    }
    let finish = seen.iter().position(|&v| near(v, 0.5));
    assert!(
        finish.is_some_and(|i| seen[i..].contains(&0.0)),
        "fine VAL posts {seen:?}"
    );
    seq_stop(f.tid);
}

/// afterMove has no clause for fine mode (`:248-261`): switched to fine
/// during a coarse move, the program holds there, busy, until the mode
/// leaves fine.
#[epics_base_rs::epics_test]
async fn fine_mode_after_a_coarse_move_holds_in_after_move() {
    let f = boot(&[]).await;
    command(&f, 20.0).await;
    coarse_moving(&f).await;
    put(&f.ioc, "X:flex:mode", 3.0).await;
    put(&f.ioc, "X:cap:pos", 19.5).await;
    reaches(&f.ioc, "X:cm.DMOV", 1.0).await;
    holds(&f.ioc, "X:flex:busy", 1.0).await;
    holds(&f.ioc, "X:fm.VAL", 0.0).await;
    put(&f.ioc, "X:flex:mode", 0.0).await;
    done(&f).await;
    reaches(&f.ioc, "X:fm.VAL", 0.5).await;
    seq_stop(f.tid);
}

const CHILD: &str = "FLEX_COMBINED_MOTION_DEBUG_CHILD";

/// `DEBUG_PRINT`'s header names the program (`seqPVmacros.h:228-231`).
///
/// C defect fixed: C never assigned `SNLtaskName` and printed it, NULL,
/// with `%s`.
#[epics_base_rs::epics_test]
async fn debug_lines_name_the_program() {
    if std::env::var_os(CHILD).is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "debug_lines_name_the_program",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        let line = stdout
            .lines()
            .find(|l| l.ends_with("idle -> checkSetPoint"))
            .expect("no idle -> checkSetPoint line");
        assert!(line.contains(",flexCombinedMotion,3> "), "{line}");
        return;
    }
    let f = boot(&[
        ("X:flex:debug", 3.0),
        ("X:flex:mode", 3.0),
        ("X:cap:pos", 5.0),
    ])
    .await;
    command(&f, 8.0).await;
    done(&f).await;
    seq_stop(f.tid);
}
