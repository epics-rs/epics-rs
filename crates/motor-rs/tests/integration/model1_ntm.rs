//! Seven C model-1 device supports force NTM to NO in init_record, as they
//! do their own backlash (devEMC18011.cc:127-129, devPIE816.cc:130,
//! devMDT695.cc:129, ...), before `motor_init_record_com` looks for the
//! card; a record's own NTM stands otherwise.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{Model1Motor, MotorStatus, PollFailure};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::DevSup;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::{AxisSetup, MotorBuilder};
use motor_rs::devices::{
    device_support_factory, devices, model1_device, register_axis, register_model1_device,
    register_model1_device_disabling_ntm,
};
use motor_rs::trace::TraceUser;

static OWN_BACKLASH: DevSup = model1_device("devTestNtmOff", "TestNtmOff");
static PLAIN: DevSup = model1_device("devTestNtm", "TestNtm");

struct Card;

impl Model1Motor for Card {
    fn set_velocity(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_vel_base(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_accel(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn move_absolute(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn move_relative(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn move_velocity(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn home(&mut self, _: &AsynUser, _: bool) -> AsynResult<()> {
        Ok(())
    }
    fn stop(&mut self, _: &AsynUser) -> AsynResult<()> {
        Ok(())
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    /// `motor_init` reads the card with the status function the task queries.
    fn update_delay(&self) -> Duration {
        Duration::ZERO
    }
    fn take_init_status(&mut self) -> MotorStatus {
        let mut status = MotorStatus::default();
        self.poll(&AsynUser::default(), &mut status)
            .expect("the card answers");
        status
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        *status = MotorStatus {
            done: true,
            ..MotorStatus::default()
        };
        Ok(())
    }
}

/// Card `card` of `device`, with signal 0 only.
fn configure(device: &'static DevSup, card: i32) {
    let AxisSetup { axis, .. } = MotorBuilder::model1(Arc::new(Mutex::new(Card)))
        .trace_user(TraceUser::Model1 {
            device: device.choice,
            card,
            signal: 0,
        })
        .poll_interval(Duration::from_millis(10))
        .build_axis();
    register_axis(axis).unwrap();
}

/// Each `(DTYP, OUT)` as a record with NTM YES, and its NTM past iocInit.
async fn ntm_after_init(records: &[(&str, &str)]) -> Vec<EpicsValue> {
    register_model1_device_disabling_ntm(&OWN_BACKLASH);
    register_model1_device(&PLAIN);
    let db: String = records
        .iter()
        .enumerate()
        .map(|(i, (dtyp, out))| {
            format!(
                r#"record(motor, "N:m{i}") {{ field(DTYP, "{dtyp}") field(OUT, "{out}") field(NTM, "YES") }}
"#
            )
        })
        .collect();
    let (name, factory) = motor_rs::motor_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(&db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    (0..records.len())
        .map(|i| db.get_pv(format!("N:m{i}.NTM")).unwrap())
        .collect()
}

#[tokio::test]
async fn a_device_doing_its_own_backlash_forces_ntm_off_at_init() {
    configure(&OWN_BACKLASH, 0);
    assert_eq!(
        ntm_after_init(&[("TestNtmOff", "#C0 S0 @")]).await,
        [EpicsValue::Short(0)]
    );
}

#[tokio::test]
async fn ntm_stands_for_any_other_device() {
    configure(&PLAIN, 1);
    assert_eq!(
        ntm_after_init(&[("TestNtm", "#C1 S0 @")]).await,
        [EpicsValue::Short(1)]
    );
}

/// The DTYP forces NTM off with no axis bound: no card, a signal the card
/// lacks, and an OUT that is no VME_IO link.
#[tokio::test]
async fn ntm_is_forced_off_for_a_record_with_no_axis() {
    configure(&OWN_BACKLASH, 2);
    configure(&PLAIN, 3);
    assert_eq!(
        ntm_after_init(&[
            ("TestNtmOff", "#C9 S0 @"),
            ("TestNtmOff", "#C2 S5 @"),
            ("TestNtmOff", "@bus"),
            ("TestNtm", "#C9 S0 @"),
            ("TestNtm", "#C3 S5 @"),
            ("TestNtm", "@bus"),
        ])
        .await,
        [0, 0, 0, 1, 1, 1].map(EpicsValue::Short)
    );
}
