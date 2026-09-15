//! A move finished with no `.DMOV` monitor must leave DMOV's published value
//! at the completion's 1, so a client that subscribes afterwards receives the
//! next move's DMOV 1→0→1. ophyd `EpicsMotor` completes a move only on that
//! transition; with the move-start 0 dropped, its first `mv` never returned.
//! C posts both halves on an empty `mlis` too (`motorRecord.cc:2603-2606`,
//! `:3628-3629`), so a later subscriber sees them.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynError;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use motor_rs::MotorRecord;
use motor_rs::device_state::StampedStatus;
use motor_rs::device_support::MotorDeviceSupport;
use motor_rs::flags::MstaFlags;

/// Lands exactly on every commanded target.
struct ExactMotor {
    position: f64,
}

impl AsynMotor for ExactMotor {
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
        self.position = position;
        Ok(())
    }
}

#[tokio::test]
async fn subscriber_after_unwatched_move_sees_dmov_zero_then_one() {
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(ExactMotor { position: 0.0 }));
    let state = motor_rs::device_state::new_shared_state();

    let mut rec = MotorRecord::new();
    rec.set_device_state(state.clone());
    rec.conv.mres = 0.001;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.limits.hlm = 100.0;
    rec.limits.llm = -100.0;
    rec.vel.velo = 100000.0;
    rec.vel.accl = 0.5;
    rec.stat.msta = MstaFlags::DONE;

    // Held alive so PollDirective sends succeed; statuses are driven by hand.
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
    {
        let arc = db.get_record("M1").unwrap();
        let mut inst = arc.write();
        inst.common.dtyp = "simMotor".to_string();
        inst.device = Some(Box::new(dev));
    }

    // Startup pass consumes the init-seeded status (seq 1).
    db.process_record_readback("M1", &mut HashSet::new())
        .await
        .unwrap();

    // One move: the put pass dispatches (DMOV 1→0), the completion callback
    // pass finalizes (DMOV 0→1).
    let move_to = |target: f64, seq: u64| {
        let (db, motor, state) = (db.clone(), motor.clone(), state.clone());
        async move {
            let user = AsynUser::new(0);
            db.put_record_field_from_ca_no_notify("M1", "VAL", EpicsValue::Double(target))
                .await
                .unwrap();
            let status = motor.lock().unwrap().poll(&user).unwrap();
            state.lock().unwrap().latest_status = Some(StampedStatus { seq, status });
            db.process_record_readback("M1", &mut HashSet::new())
                .await
                .unwrap();
            assert_eq!(db.get_pv("M1.DMOV").unwrap(), EpicsValue::Short(1));
        }
    };

    // Client A: moves with no DMOV monitor.
    move_to(1.0, 2).await;

    // Client B: subscribes, then moves.
    let mut rx = db
        .get_record("M1")
        .unwrap()
        .write()
        .add_subscriber(
            "DMOV",
            1,
            DbFieldType::Short,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .unwrap();
    move_to(2.0, 3).await;

    let mut dmov = Vec::new();
    while let Ok(event) = rx.try_recv() {
        dmov.push(event.snapshot.value.clone());
    }
    assert_eq!(
        dmov,
        [EpicsValue::Short(0), EpicsValue::Short(1)],
        "the move after an unwatched move must post DMOV 0 at its start"
    );
}
