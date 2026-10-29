//! One motor record wired to a driver that lands exactly on every commanded
//! target, driven one process pass at a time.
//!
//! The two halves of a move are separate calls because what these tests
//! measure happens BETWEEN them: the put drives the move-start
//! pass (DMOV 1→0), and the readback pass drives the
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

/// The status `motor` writes polled from the default.
fn polled(motor: &mut (impl AsynMotor + ?Sized), user: &AsynUser) -> MotorStatus {
    let mut status = MotorStatus::default();
    motor.poll(user, &mut status).unwrap();
    status
}
use motor_rs::device_support::MotorDeviceSupport;
use motor_rs::flags::MstaFlags;

/// Reports every commanded position as reached, with the status bits a test
/// raises.
pub struct ExactMotor {
    pub position: f64,
    pub powered: bool,
    pub gain_support: bool,
    pub comms_error: bool,
    /// Reported as not done, e.g. while a jog runs.
    pub moving: bool,
    /// MSTA RA_DIRECTION, which TDIR follows.
    pub direction: bool,
}

impl AsynMotor for ExactMotor {
    fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
        asyn_rs::interfaces::motor::PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _user: &AsynUser, status: &mut MotorStatus) -> Result<(), AsynError> {
        *status = MotorStatus {
            done: !self.moving,
            powered: self.powered,
            gain_support: self.gain_support,
            comms_error: self.comms_error,
            direction: self.direction,
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
    fn move_velocity(
        &mut self,
        _user: &AsynUser,
        _min_vel: f64,
        _velocity: f64,
        _accel: f64,
    ) -> Result<(), AsynError> {
        self.moving = true;
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
    motor: Arc<Mutex<ExactMotor>>,
    pub(crate) state: SharedDeviceState,
    /// The driver's status sequence number; a pass consumes one.
    seq: u64,
    /// Held so the record's `PollCommand` sends succeed.
    pub(crate) _poll_cmd_rx: tokio::sync::mpsc::Receiver<motor_rs::poll_loop::PollCommand>,
    /// The driver commands the record queued for the poller thread, which
    /// this fixture stands in for.
    motor_cmd_rx: tokio::sync::mpsc::UnboundedReceiver<motor_rs::poll_loop::ToAxis>,
}

impl MotorFixture {
    /// Build `M1` at position 0 and run its startup readback pass. `configure`
    /// sets the per-test fields (deadbands, limits) before device init.
    pub async fn new(configure: impl FnOnce(&mut MotorRecord)) -> Self {
        let motor = Arc::new(Mutex::new(ExactMotor {
            position: 0.0,
            powered: false,
            gain_support: false,
            comms_error: false,
            moving: false,
            direction: false,
        }));
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
        let (motor_cmd_tx, motor_cmd_rx) = tokio::sync::mpsc::unbounded_channel();
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
        {
            let arc = db.get_record("M1").unwrap();
            let mut inst = arc.write();
            inst.common.dtyp = "simMotor".into();
            inst.device = Some(Box::new(dev));
        }

        // init took the cached status (seq 1); the startup pass reads the
        // poller's first post.
        state.lock().unwrap().latest_status = Some(StampedStatus {
            seq: 2,
            status: polled(&mut *motor.lock().unwrap(), &AsynUser::new(0)),
        });
        db.process_record_readback("M1", &mut ProcStack::new())
            .await
            .unwrap();

        Self {
            db,
            motor,
            state,
            seq: 2,
            _poll_cmd_rx,
            motor_cmd_rx,
        }
    }

    /// The put pass: dispatches the move and posts DMOV 1→0.
    pub async fn start_move(&self, target: f64) {
        self.db
            .put_record_field_from_ca_no_notify("M1", "VAL", EpicsValue::Double(target))
            .await
            .unwrap();
    }

    /// The completion pass: hands the record the driver's "done" status and
    /// processes the readback, which posts DMOV 0→1.
    pub async fn finish_move(&mut self) {
        self.poll_pass(|_| {}).await;
        assert_eq!(
            self.db.get_pv("M1.DMOV").unwrap(),
            EpicsValue::Short(1),
            "the readback pass must finish the move"
        );
    }

    /// One driver poll, after `edit` changed what the driver reports, and the
    /// readback pass that consumes it.
    pub async fn poll_pass(&mut self, edit: impl FnOnce(&mut ExactMotor)) {
        let user = AsynUser::new(0);
        let status = {
            let mut motor = self.motor.lock().unwrap();
            while let Ok(motor_rs::poll_loop::ToAxis::Batch(batch)) = self.motor_cmd_rx.try_recv() {
                motor_rs::poll_loop::execute_commands(
                    &mut *motor,
                    &batch.trace,
                    &batch.commands,
                    &mut |_| {},
                );
            }
            edit(&mut motor);
            polled(&mut *motor, &user)
        };
        self.seq += 1;
        self.state.lock().unwrap().latest_status = Some(StampedStatus {
            seq: self.seq,
            status,
        });
        self.db
            .process_record_readback("M1", &mut ProcStack::new())
            .await
            .unwrap();
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
        out.push(event.value().as_value().cloned().unwrap());
    }
    out
}
