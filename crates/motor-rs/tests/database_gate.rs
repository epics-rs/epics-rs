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

/// The status `motor` writes polled from the default.
fn polled(
    motor: &mut (impl asyn_rs::interfaces::motor::AsynMotor + ?Sized),
    user: &asyn_rs::user::AsynUser,
) -> asyn_rs::interfaces::motor::MotorStatus {
    let mut status = asyn_rs::interfaces::motor::MotorStatus::default();
    motor.poll(user, &mut status).unwrap();
    status
}

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
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_readback("M1", &mut visited)
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
        actions.is_some_and(|a| a.commands.contains(&motor_rs::flags::MotorCommand::Poll)),
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
async fn a_pid_gain_put_reaches_the_driver_at_put_time() {
    // PCOF is not pp(TRUE): C special() sends SET_PGAIN at put time.
    let mut rec = make_record(motor_rs::device_state::new_shared_state());
    let (port, mut sent) = motor_rs::device_support::CommandPort::capture();
    rec.set_command_port(port);
    rec.stat.msta = MstaFlags::DONE | MstaFlags::GAIN_SUPPORT;
    let db = PvDatabase::new();
    db.add_record("M1", Box::new(rec)).await.unwrap();

    db.put_record_field_from_ca_no_notify("M1", "PCOF", EpicsValue::Double(0.5))
        .await
        .unwrap();

    let Ok(motor_rs::poll_loop::ToAxis::Batch(batch)) = sent.try_recv() else {
        panic!("no batch");
    };
    assert!(matches!(
        batch.commands.as_slice(),
        [q] if matches!(q, MotorCommand::SetPidGain { .. })
    ));
}

/// Stands in for the poller thread: runs the driver commands the record
/// passes queued.
fn run_queued(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<motor_rs::poll_loop::ToAxis>,
    motor: &std::sync::Mutex<dyn asyn_rs::interfaces::motor::AsynMotor>,
) {
    while let Ok(motor_rs::poll_loop::ToAxis::Batch(batch)) = rx.try_recv() {
        motor_rs::poll_loop::execute_commands(
            &mut *motor.lock().unwrap(),
            &batch.trace,
            &batch.commands,
            &mut |_| {},
        );
    }
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
        fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
            asyn_rs::interfaces::motor::PollFailure::PROBLEM_AND_COMMS_ERROR
        }
        fn poll(&mut self, _user: &AsynUser, status: &mut MotorStatus) -> Result<(), AsynError> {
            *status = MotorStatus {
                done: true,
                ..MotorStatus::default().with_position(self.position)
            };
            Ok(())
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
            // 0.05 EGU short at MRES 0.001.
            self.position = if n == 0 { position - 50.0 } else { position };
            Ok(())
        }
        fn move_relative(
            &mut self,
            _user: &AsynUser,
            _distance: f64,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
        ) -> Result<(), AsynError> {
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
    rec.limits.lvio = 0;
    rec.vel.velo = 100000.0;
    rec.vel.accl = 0.5;
    rec.stat.msta = MstaFlags::DONE;

    // poll_cmd_rx is held alive so PollDirective sends succeed; the test
    // drives statuses by hand instead of running the poll loop.
    let (poll_cmd_tx, _poll_cmd_rx) = tokio::sync::mpsc::channel(16);
    let (motor_cmd_tx, mut motor_cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    // The poller's startup poll fills the cache init reads.
    state.lock().unwrap().latest_status = Some(motor_rs::device_state::StampedStatus {
        seq: 1,
        status: polled(
            &mut *motor.lock().unwrap(),
            &asyn_rs::user::AsynUser::new(0),
        ),
    });
    let mut dev = MotorDeviceSupport::new(
        Default::default(),
        Duration::from_secs(1),
        poll_cmd_tx,
        motor_cmd_tx,
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
        inst.common.dtyp = "simMotor".into();
        inst.device = Some(Box::new(dev));
    }

    // init_record took the cached status (seq 1); the startup pass reads
    // the poller's first post.
    state.lock().unwrap().latest_status = Some(StampedStatus {
        seq: 2,
        status: polled(&mut *motor.lock().unwrap(), &user),
    });
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_readback("M1", &mut visited)
        .await
        .unwrap();
    run_queued(&mut motor_cmd_rx, &motor);

    // Move to 1.0 — the put pass dispatches, the driver undershoots.
    db.put_record_field_from_ca_no_notify("M1", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    run_queued(&mut motor_cmd_rx, &motor);
    assert_eq!(moves.load(Ordering::SeqCst), 1, "put pass dispatched");

    // Completion callback pass: diff = 0.05 >= RDBD -> maybeRetry arms and
    // re-dispatches ON THIS PASS. The write must reach the driver.
    let stamp = |seq: u64| {
        let status = polled(&mut *motor.lock().unwrap(), &user);
        state.lock().unwrap().latest_status = Some(StampedStatus { seq, status });
    };
    stamp(3);
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_readback("M1", &mut visited)
        .await
        .unwrap();
    run_queued(&mut motor_cmd_rx, &motor);
    assert_eq!(
        moves.load(Ordering::SeqCst),
        2,
        "the retry emitted on the callback pass must reach the driver"
    );

    // Retry landed on target: the next callback pass finalizes.
    stamp(4);
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_readback("M1", &mut visited)
        .await
        .unwrap();
    run_queued(&mut motor_cmd_rx, &motor);
    assert_eq!(
        db.get_pv("M1.DMOV").unwrap(),
        EpicsValue::Short(1),
        "retry completion finalizes the motion"
    );
    assert_eq!(
        db.get_pv("M1.MIP").unwrap(),
        EpicsValue::UShort(0),
        "MIP collapses to DONE after the retry converges"
    );
}

/// The device init anchors the record (C init_record), so a put before
/// the first status pulse is an ordinary put pass: C has no pre-anchor
/// window, as init_record precedes any dbPutField.
#[tokio::test]
async fn a_put_before_the_first_pulse_moves_at_once() {
    use asyn_rs::error::AsynError;
    use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus};
    use asyn_rs::user::AsynUser;
    use motor_rs::device_state::StampedStatus;
    use motor_rs::device_support::MotorDeviceSupport;
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
        fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
            asyn_rs::interfaces::motor::PollFailure::PROBLEM_AND_COMMS_ERROR
        }
        fn poll(&mut self, _user: &AsynUser, status: &mut MotorStatus) -> Result<(), AsynError> {
            *status = MotorStatus {
                done: true,
                ..MotorStatus::default().with_position(self.position)
            };
            Ok(())
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
        fn move_relative(
            &mut self,
            _user: &AsynUser,
            _distance: f64,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
        ) -> Result<(), AsynError> {
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
    rec.limits.lvio = 0;
    rec.vel.velo = 100000.0;
    rec.vel.accl = 0.5;
    rec.stat.msta = MstaFlags::DONE;

    let (poll_cmd_tx, _poll_cmd_rx) = tokio::sync::mpsc::channel(16);
    let (motor_cmd_tx, mut motor_cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    // The poller's startup poll fills the cache init reads.
    state.lock().unwrap().latest_status = Some(motor_rs::device_state::StampedStatus {
        seq: 1,
        status: polled(
            &mut *motor.lock().unwrap(),
            &asyn_rs::user::AsynUser::new(0),
        ),
    });
    let mut dev = MotorDeviceSupport::new(
        Default::default(),
        Duration::from_secs(1),
        poll_cmd_tx,
        motor_cmd_tx,
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
        inst.common.dtyp = "simMotor".into();
        inst.device = Some(Box::new(dev));
    }

    db.put_record_field_from_ca_no_notify("M1", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    run_queued(&mut motor_cmd_rx, &motor);
    assert_eq!(moves.load(Ordering::SeqCst), 1, "the put moves at once");
    assert_eq!(
        loads.load(Ordering::SeqCst),
        0,
        "the anchor kept the controller position"
    );

    let stamp = |seq: u64| {
        let status = polled(&mut *motor.lock().unwrap(), &user);
        state.lock().unwrap().latest_status = Some(StampedStatus { seq, status });
    };

    // Completion pass: the axis landed on target.
    stamp(2);
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_readback("M1", &mut visited)
        .await
        .unwrap();
    run_queued(&mut motor_cmd_rx, &motor);
    assert_eq!(db.get_pv("M1.DMOV").unwrap(), EpicsValue::Short(1));
    assert_eq!(db.get_pv("M1.MIP").unwrap(), EpicsValue::UShort(0));
    assert_eq!(db.get_pv("M1.RBV").unwrap(), EpicsValue::Double(1.0));
    assert_eq!(
        loads.load(Ordering::SeqCst),
        0,
        "no position redefine anywhere on the parked-put path"
    );
}

#[tokio::test]
async fn only_a_udf_put_of_one_raises_udf_alarm() {
    // UDF is pp(TRUE) in dbCommon.dbd, and alarm_sub tests `udf == TRUE`
    // (motorRecord.cc:3372): oracleMotorIoc reads NO_ALARM after
    // `dbpf M1.UDF 2` and `255`, UDF/INVALID only after `1`.
    use epics_base_rs::server::recgbl::alarm_status;
    let state = motor_rs::device_state::new_shared_state();
    let db = PvDatabase::new();
    db.add_record("M1", Box::new(make_record(state.clone())))
        .await
        .unwrap();
    anchor(&db, &state).await;

    for (udf, sevr, stat) in [
        ("2", 0, alarm_status::NO_ALARM),
        ("255", 0, alarm_status::NO_ALARM),
        ("1", 3, alarm_status::UDF_ALARM),
        ("0", 0, alarm_status::NO_ALARM),
    ] {
        db.put_record_field_from_ca_no_notify("M1", "UDF", EpicsValue::String(udf.into()))
            .await
            .unwrap();
        assert_eq!(
            (db.get_pv("M1.SEVR").unwrap(), db.get_pv("M1.STAT").unwrap()),
            (EpicsValue::Short(sevr), EpicsValue::Short(stat as i16)),
            "UDF {udf}"
        );
    }
}
