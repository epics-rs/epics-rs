//! `dbPutField` processing-gate parity for the motor record.
//!
//! C `dbPutField` (`dbAccess.c:1263-1268`) processes a Passive record on
//! a put only when the put field is `pp(TRUE)` in `motorRecord.dbd`. The
//! motor entry in `epics-base-rs::server::record::process_passive` models
//! that set (plus the motor-rs special()-transport extensions); these
//! tests pin the gate end-to-end through the database put path.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use motor_rs::MotorRecord;
use motor_rs::flags::*;

fn make_record(state: motor_rs::device_state::SharedDeviceState) -> MotorRecord {
    let mut rec = MotorRecord::new();
    rec.set_device_state(state);
    rec.stat.msta = MstaFlags::DONE;
    rec
}

/// Anchor the record with a first idle status pulse (Startup pass). C's
/// init_record precedes any dbPutField pass, so every put test below runs
/// against an anchored record — an unanchored mailbox-wired record parks
/// its signals until the anchor (see do_process_inner's anchor-first gate).
async fn anchor(db: &PvDatabase, state: &motor_rs::device_state::SharedDeviceState) {
    use asyn_rs::interfaces::motor::MotorStatus;
    use motor_rs::device_state::StampedStatus;
    state.lock().unwrap().latest_status = Some(StampedStatus {
        seq: 1,
        status: MotorStatus {
            done: true,
            ..MotorStatus::default()
        },
    });
    let mut visited = std::collections::HashSet::new();
    db.process_record_readback("M1", &mut visited, 0)
        .await
        .unwrap();
    // Drop the anchor pass's mailbox batch so each test asserts only on
    // the actions its own put produces.
    state.lock().unwrap().pending_actions.take();
}

#[tokio::test]
async fn non_pp_put_stores_without_processing() {
    // VELO has no pp(TRUE) in motorRecord.dbd: C stores it via special()
    // and runs NO process pass — no do_work chain end, no implicit
    // GET_INFO, STUP stays OFF.
    let state = motor_rs::device_state::new_shared_state();
    let db = PvDatabase::new();
    db.add_record("M1", Box::new(make_record(state.clone())))
        .await
        .unwrap();

    db.put_record_field_from_ca_no_notify("M1", "VELO", EpicsValue::Double(2.0))
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("M1.VELO").unwrap(),
        EpicsValue::Double(2.0),
        "the store itself lands without a process pass"
    );
    assert_eq!(
        db.get_pv("M1.STUP").unwrap(),
        EpicsValue::Short(0),
        "no process pass -> no implicit GET_INFO, STUP stays OFF"
    );
    assert!(
        state.lock().unwrap().pending_actions.is_none(),
        "no process pass -> nothing written to the device mailbox"
    );
}

#[tokio::test]
async fn sync_zero_release_processes_with_implicit_get_info() {
    // SYNC is pp(TRUE): a SYNC=0 release runs the process pass even
    // though the motor put handler latches nothing for it. The pass
    // falls to the do_work chain end (C 2540-2557) — implicit GET_INFO,
    // STUP -> BUSY.
    let state = motor_rs::device_state::new_shared_state();
    let db = PvDatabase::new();
    db.add_record("M1", Box::new(make_record(state.clone())))
        .await
        .unwrap();
    anchor(&db, &state).await;

    db.put_record_field_from_ca_no_notify("M1", "SYNC", EpicsValue::Short(0))
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("M1.STUP").unwrap(),
        EpicsValue::Short(2),
        "pp(TRUE) pass ends at the chain end: implicit GET_INFO, STUP BUSY"
    );
    let actions = state.lock().unwrap().pending_actions.take();
    assert!(
        actions.is_some_and(|a| a.status_refresh),
        "the implicit GET_INFO requests a device status refresh"
    );
}

#[tokio::test]
async fn user_limit_put_processes_with_implicit_get_info() {
    // HLM is pp(TRUE) with no last_write on the Rust side: the pass it
    // triggers is the src-less housekeeping pass, ending at the chain
    // end with the implicit GET_INFO like C.
    let state = motor_rs::device_state::new_shared_state();
    let db = PvDatabase::new();
    db.add_record("M1", Box::new(make_record(state.clone())))
        .await
        .unwrap();
    anchor(&db, &state).await;

    db.put_record_field_from_ca_no_notify("M1", "HLM", EpicsValue::Double(50.0))
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("M1.STUP").unwrap(),
        EpicsValue::Short(2),
        "pp(TRUE) limit put processes and fires the implicit GET_INFO"
    );
}

#[tokio::test]
async fn pid_gain_put_rides_its_pass_to_the_driver() {
    // PCOF is NOT pp(TRUE) in C (special() forwards SET_PGAIN with no
    // process pass), but motor-rs has no put-time driver channel: the
    // SetPidGain command rides the process pass that follows the put.
    // The motor pp entry therefore keeps PCOF as a special()-transport
    // extension — this pins that the forward still reaches the device
    // mailbox under the modeled gate.
    let state = motor_rs::device_state::new_shared_state();
    let mut rec = make_record(state.clone());
    rec.stat.msta = MstaFlags::DONE | MstaFlags::GAIN_SUPPORT;
    let db = PvDatabase::new();
    db.add_record("M1", Box::new(rec)).await.unwrap();

    db.put_record_field_from_ca_no_notify("M1", "PCOF", EpicsValue::Double(0.5))
        .await
        .unwrap();

    let actions = state
        .lock()
        .unwrap()
        .pending_actions
        .take()
        .expect("the transport pass writes the device mailbox");
    assert!(
        actions
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPidGain { .. })),
        "SetPidGain forwarded through the put's process pass"
    );
}

/// A retry dispatch is emitted on a driver-callback (CALLBACK_DATA) pass —
/// C `maybeRetry` arms MIP_RETRY and the same `dbProcess` re-enters
/// `do_work`, whose move block re-fires through devMotorAsyn's write. The
/// framework's driver-callback entry (`process_record_readback`) must
/// therefore run the motor's output stage: the regression suppressed it
/// (asyn `newOutputCallbackValue` semantics applied to every output
/// record), so the retry command rotted in the mailbox and the record
/// stuck at DMOV=0, MIP=RETRY|MOVE, with every later put timing out.
#[tokio::test]
async fn retry_dispatched_on_callback_pass_reaches_the_driver() {
    use asyn_rs::error::AsynError;
    use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus};
    use asyn_rs::user::AsynUser;
    use motor_rs::device_state::StampedStatus;
    use motor_rs::device_support::MotorDeviceSupport;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Lands 0.05 EGU short of the first commanded target, exactly on
    /// target afterwards — so the first completion needs one retry.
    struct UndershootMotor {
        position: f64,
        moves: Arc<AtomicU32>,
    }
    impl AsynMotor for UndershootMotor {
        fn poll(&mut self, _user: &AsynUser) -> Result<MotorStatus, AsynError> {
            Ok(MotorStatus {
                position: self.position,
                done: true,
                ..MotorStatus::default()
            })
        }
        fn move_absolute(
            &mut self,
            _user: &AsynUser,
            position: f64,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
        ) -> Result<(), AsynError> {
            let n = self.moves.fetch_add(1, Ordering::SeqCst);
            self.position = if n == 0 { position - 0.05 } else { position };
            Ok(())
        }
        fn stop(&mut self, _user: &AsynUser, _accel: f64) -> Result<(), AsynError> {
            Ok(())
        }
        fn home(
            &mut self,
            _user: &AsynUser,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
            _forwards: bool,
        ) -> Result<(), AsynError> {
            Ok(())
        }
        fn set_position(&mut self, _user: &AsynUser, position: f64) -> Result<(), AsynError> {
            self.position = position;
            Ok(())
        }
    }

    let moves = Arc::new(AtomicU32::new(0));
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(UndershootMotor {
        position: 0.0,
        moves: moves.clone(),
    }));
    let user = AsynUser::new(0);

    let state = motor_rs::device_state::new_shared_state();
    let mut rec = MotorRecord::new();
    rec.set_device_state(state.clone());
    rec.conv.mres = 0.001;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.limits.hlm = 100.0;
    rec.limits.llm = -100.0;
    rec.limits.lvio = false;
    rec.vel.velo = 100000.0;
    rec.vel.accl = 0.5;
    rec.stat.msta = MstaFlags::DONE;

    // poll_cmd_rx is held alive so PollDirective sends succeed; the test
    // drives statuses by hand instead of running the poll loop.
    let (poll_cmd_tx, _poll_cmd_rx) = tokio::sync::mpsc::channel(16);
    let mut dev = MotorDeviceSupport::new(
        motor.clone(),
        0,
        Duration::from_secs(1),
        poll_cmd_tx,
        state.clone(),
    );
    {
        use epics_base_rs::server::device_support::DeviceSupport;
        use epics_base_rs::server::record::Record;
        dev.init(&mut rec as &mut dyn Record).unwrap();
    }

    let db = PvDatabase::new();
    db.add_record("M1", Box::new(rec)).await.unwrap();
    if let Some(arc) = db.get_record("M1") {
        let mut inst = arc.write();
        inst.common.dtyp = "simMotor".to_string();
        inst.device = Some(Box::new(dev));
    }

    // Startup pass consumes the init-seeded status (seq 1).
    let mut visited = HashSet::new();
    db.process_record_readback("M1", &mut visited, 0)
        .await
        .unwrap();

    // Move to 1.0 — the put pass dispatches, the driver undershoots.
    db.put_record_field_from_ca_no_notify("M1", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(moves.load(Ordering::SeqCst), 1, "put pass dispatched");

    // Completion callback pass: diff = 0.05 >= RDBD -> maybeRetry arms and
    // re-dispatches ON THIS PASS. The write must reach the driver.
    let stamp = |seq: u64| {
        let status = motor.lock().unwrap().poll(&user).unwrap();
        state.lock().unwrap().latest_status = Some(StampedStatus { seq, status });
    };
    stamp(2);
    let mut visited = HashSet::new();
    db.process_record_readback("M1", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        moves.load(Ordering::SeqCst),
        2,
        "the retry emitted on the callback pass must reach the driver"
    );

    // Retry landed on target: the next callback pass finalizes.
    stamp(3);
    let mut visited = HashSet::new();
    db.process_record_readback("M1", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("M1.DMOV").unwrap(),
        EpicsValue::Short(1),
        "retry completion finalizes the motion"
    );
    assert_eq!(
        db.get_pv("M1.MIP").unwrap(),
        EpicsValue::Short(0),
        "MIP collapses to DONE after the retry converges"
    );
}

/// A write parked before the first status pulse (autosave pass-1 restore,
/// or a CA put in the init→first-poll window) must not turn that pulse
/// into a command pass: the pulse anchors (Startup), the anchor leaves the
/// parked target untouched, and the write replays as a real move on the
/// pass the anchor's forced refresh queues. Pre-fix, the pulse was consumed
/// as a put pass against an unanchored baseline and the following Startup
/// synced the mid-flight readback into VAL/DVAL.
#[tokio::test]
async fn parked_pre_pulse_put_anchors_first_then_replays_as_a_move() {
    use asyn_rs::error::AsynError;
    use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus};
    use asyn_rs::user::AsynUser;
    use motor_rs::device_state::StampedStatus;
    use motor_rs::device_support::MotorDeviceSupport;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Lands exactly on target; counts moves and position redefines.
    struct SpyMotor {
        position: f64,
        moves: Arc<AtomicU32>,
        loads: Arc<AtomicU32>,
    }
    impl AsynMotor for SpyMotor {
        fn poll(&mut self, _user: &AsynUser) -> Result<MotorStatus, AsynError> {
            Ok(MotorStatus {
                position: self.position,
                done: true,
                ..MotorStatus::default()
            })
        }
        fn move_absolute(
            &mut self,
            _user: &AsynUser,
            position: f64,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
        ) -> Result<(), AsynError> {
            self.moves.fetch_add(1, Ordering::SeqCst);
            self.position = position;
            Ok(())
        }
        fn stop(&mut self, _user: &AsynUser, _accel: f64) -> Result<(), AsynError> {
            Ok(())
        }
        fn home(
            &mut self,
            _user: &AsynUser,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
            _forwards: bool,
        ) -> Result<(), AsynError> {
            Ok(())
        }
        fn set_position(&mut self, _user: &AsynUser, position: f64) -> Result<(), AsynError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            self.position = position;
            Ok(())
        }
    }

    let moves = Arc::new(AtomicU32::new(0));
    let loads = Arc::new(AtomicU32::new(0));
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SpyMotor {
        position: 0.0,
        moves: moves.clone(),
        loads: loads.clone(),
    }));
    let user = AsynUser::new(0);

    let state = motor_rs::device_state::new_shared_state();
    let mut rec = MotorRecord::new();
    rec.set_device_state(state.clone());
    rec.conv.mres = 0.001;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.limits.hlm = 100.0;
    rec.limits.llm = -100.0;
    rec.limits.lvio = false;
    rec.vel.velo = 100000.0;
    rec.vel.accl = 0.5;
    rec.stat.msta = MstaFlags::DONE;

    let (poll_cmd_tx, _poll_cmd_rx) = tokio::sync::mpsc::channel(16);
    let mut dev = MotorDeviceSupport::new(
        motor.clone(),
        0,
        Duration::from_secs(1),
        poll_cmd_tx,
        state.clone(),
    );
    {
        use epics_base_rs::server::device_support::DeviceSupport;
        use epics_base_rs::server::record::Record;
        dev.init(&mut rec as &mut dyn Record).unwrap();
    }

    let db = PvDatabase::new();
    db.add_record("M1", Box::new(rec)).await.unwrap();
    if let Some(arc) = db.get_record("M1") {
        let mut inst = arc.write();
        inst.common.dtyp = "simMotor".to_string();
        inst.device = Some(Box::new(dev));
    }

    // Put BEFORE any status pulse processed: init() cleared the pass0
    // latch, so this parks a fresh last_write — the pass-1/early-CA-put
    // window. The put's own process pass consumes the init-seeded status
    // (seq 1) as the ANCHOR, not as a put pass.
    db.put_record_field_from_ca_no_notify("M1", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();

    assert_eq!(
        moves.load(Ordering::SeqCst),
        0,
        "the anchor pass must not dispatch the parked put"
    );
    assert_eq!(
        loads.load(Ordering::SeqCst),
        0,
        "the anchor must not redefine the controller position"
    );
    assert_eq!(
        db.get_pv("M1.VAL").unwrap(),
        EpicsValue::Double(1.0),
        "the anchor must not sync the parked target away"
    );
    assert_eq!(
        db.get_pv("M1.DVAL").unwrap(),
        EpicsValue::Double(1.0),
        "the anchor must not sync the parked dial target away"
    );

    let stamp = |seq: u64| {
        let status = motor.lock().unwrap().poll(&user).unwrap();
        state.lock().unwrap().latest_status = Some(StampedStatus { seq, status });
    };

    // The anchor's forced refresh queues the replay pass: the parked put
    // now dispatches as an ordinary post-init move.
    stamp(2);
    let mut visited = HashSet::new();
    db.process_record_readback("M1", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        moves.load(Ordering::SeqCst),
        1,
        "the parked put replays as a real move after the anchor"
    );

    // Completion pass: the axis landed on target.
    stamp(3);
    let mut visited = HashSet::new();
    db.process_record_readback("M1", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(db.get_pv("M1.DMOV").unwrap(), EpicsValue::Short(1));
    assert_eq!(db.get_pv("M1.MIP").unwrap(), EpicsValue::Short(0));
    assert_eq!(db.get_pv("M1.RBV").unwrap(), EpicsValue::Double(1.0));
    assert_eq!(
        loads.load(Ordering::SeqCst),
        0,
        "no position redefine anywhere on the parked-put path"
    );
}
