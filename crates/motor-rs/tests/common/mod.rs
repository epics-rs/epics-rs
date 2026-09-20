//! One motor record wired to a driver that lands exactly on every commanded
//! target, driven one process pass at a time.
//!
//! The two halves of a move are separate calls because what these tests
//! measure happens BETWEEN them: the put drives the move-start
//! `AsyncPendingNotify` pass (DMOV 1→0), and the readback pass drives the
//! completion (DMOV 0→1).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynError;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use motor_rs::MotorRecord;
use motor_rs::device_state::{SharedDeviceState, StampedStatus};
use motor_rs::device_support::MotorDeviceSupport;
use motor_rs::flags::MstaFlags;

/// Reports every commanded position as reached.
pub struct ExactMotor {
    pub position: f64,
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

/// The record (`M1`), its driver, and the status mailbox between them.
pub struct MotorFixture {
    pub db: PvDatabase,
    motor: Arc<Mutex<dyn AsynMotor>>,
    state: SharedDeviceState,
    /// The driver's status sequence number; a pass consumes one.
    seq: u64,
    /// Held so the record's `PollCommand` sends succeed.
    _poll_cmd_rx: tokio::sync::mpsc::Receiver<motor_rs::poll_loop::PollCommand>,
}

impl MotorFixture {
    /// Build `M1` at position 0 and run its startup readback pass. `configure`
    /// sets the per-test fields (deadbands, limits) before device init.
    pub async fn new(configure: impl FnOnce(&mut MotorRecord)) -> Self {
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
        configure(&mut rec);

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
            inst.common.dtyp = "simMotor".into();
            inst.device = Some(Box::new(dev));
        }

        // Startup pass consumes the status `init` seeded (seq 1).
        db.process_record_readback("M1", &mut ProcStack::new())
            .await
            .unwrap();

        Self {
            db,
            motor,
            state,
            seq: 1,
            _poll_cmd_rx,
        }
    }

    /// The put pass: dispatches the move and posts DMOV 1→0
    /// (`AsyncPendingNotify`).
    pub async fn start_move(&self, target: f64) {
        self.db
            .put_record_field_from_ca_no_notify("M1", "VAL", EpicsValue::Double(target))
            .await
            .unwrap();
    }

    /// The completion pass: hands the record the driver's "done" status and
    /// processes the readback, which posts DMOV 0→1.
    pub async fn finish_move(&mut self) {
        let user = AsynUser::new(0);
        let status = self.motor.lock().unwrap().poll(&user).unwrap();
        self.seq += 1;
        self.state.lock().unwrap().latest_status = Some(StampedStatus {
            seq: self.seq,
            status,
        });
        self.db
            .process_record_readback("M1", &mut ProcStack::new())
            .await
            .unwrap();
        assert_eq!(
            self.db.get_pv("M1.DMOV").unwrap(),
            EpicsValue::Short(1),
            "the readback pass must finish the move"
        );
    }

    /// Both halves of one move.
    pub async fn move_to(&mut self, target: f64) {
        self.start_move(target).await;
        self.finish_move().await;
    }

    pub fn subscribe(
        &self,
        field: &str,
        sid: u32,
        field_type: DbFieldType,
        mask: EventMask,
    ) -> EventReader {
        self.db
            .get_record("M1")
            .unwrap()
            .write()
            .add_subscriber(field, sid, field_type, mask.bits())
            .unwrap()
    }
}

pub fn drain(rx: &mut EventReader) -> Vec<EpicsValue> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event.snapshot.value.clone());
    }
    out
}
