//! A driver's steps on its own controller port, C's `setXxxParam(addr)` and
//! `callParamCallbacks(addr)` under the controller lock: the axis's owner
//! applies them, on the port's thread or through the port from the poller,
//! and never lets the driver reach the port from under the axis lock.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure, PortSets, StatusWrite};
use asyn_rs::param::{ParamType, ParamValue};
use asyn_rs::port::PortDriverBase;
use asyn_rs::registry::open_owner;
use asyn_rs::user::AsynUser;
use motor_rs::builder::{ControllerBuilder, MotorBuilder};
use motor_rs::controller_port::{BASE_PARAMS, ControllerParams, index};
use motor_rs::devices::{register_axis, register_controller_port};
use motor_rs::trace::TraceUser;

/// A controller parameter of the driver's own, at address 0.
#[derive(Default)]
struct Own;

impl ControllerParams for Own {
    fn create_params(&mut self, base: &mut PortDriverBase) -> AsynResult<()> {
        base.create_param("OWN", ParamType::Int32)?;
        Ok(())
    }
}

fn own() -> usize {
    BASE_PARAMS.len()
}

/// An axis that sets OWN at address 0 from its moves or its polls.
#[derive(Default)]
struct Posting {
    on_move: bool,
    on_poll: bool,
    polls: i32,
    sets: PortSets,
}

impl Posting {
    fn post(&mut self, value: i32) {
        self.sets.set(0, own(), ParamValue::Int32(value));
        self.sets.call_param_callbacks(0);
    }
}

impl AsynMotor for Posting {
    fn move_absolute(&mut self, _: &AsynUser, p: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        if self.on_move {
            self.post(p as i32);
        }
        Ok(())
    }
    fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
        Ok(())
    }
    fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
        self.polls += 1;
        if self.on_poll {
            self.post(100 + self.polls);
        }
        Ok(MotorStatus::default())
    }
    fn take_port_sets(&mut self) -> PortSets {
        std::mem::take(&mut self.sets)
    }
}

fn axis(port: &str, motor: Posting) -> motor_rs::builder::AxisSetup {
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(motor));
    MotorBuilder::new(motor)
        .trace_user(TraceUser::Asyn {
            port: port.into(),
            addr: 1,
        })
        .poll_interval(Duration::from_secs(60))
        .build_axis()
}

#[test]
fn a_port_write_applies_the_axis_s_own_port_sets_on_the_port() {
    let port = register_controller_port(&open_owner(), "PSWRITE", 2, Own).unwrap();
    let setup = axis(
        "PSWRITE",
        Posting {
            on_move: true,
            ..Posting::default()
        },
    );
    register_axis(setup.axis).unwrap();
    port.write_float64_blocking(index::MOVE_ABS, 1, 7.0)
        .unwrap();
    assert_eq!(port.read_int32_blocking(own(), 0).unwrap(), 7);
}

#[test]
fn a_poll_s_own_port_sets_reach_the_port() {
    let port = register_controller_port(&open_owner(), "PSPOLL", 2, Own).unwrap();
    // The startup poll.
    let _setup = axis(
        "PSPOLL",
        Posting {
            on_poll: true,
            ..Posting::default()
        },
    );
    assert_eq!(port.read_int32_blocking(own(), 0).unwrap(), 101);
}

#[test]
fn a_controller_poll_s_sets_before_any_axis_reach_the_port_with_the_first() {
    let port = register_controller_port(&open_owner(), "PSCTRL", 2, Own).unwrap();
    let builder = ControllerBuilder::new().controller_poll(|| {
        let mut sets = PortSets::default();
        sets.set(0, own(), ParamValue::Int32(5));
        sets
    });
    let axes = builder.axes();
    // Its startup pass, with no axis to name the port.
    drop(builder.build(Duration::from_secs(60), Duration::from_secs(60), 0));
    assert!(port.read_int32_blocking(own(), 0).is_err());
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(Posting::default()));
    let user = TraceUser::Asyn {
        port: "PSCTRL".into(),
        addr: 1,
    };
    // No poller takes the axis, but adding it names the port.
    assert!(axes.add(motor, user).is_err());
    assert_eq!(port.read_int32_blocking(own(), 0).unwrap(), 5);
}

/// A driver that reads its own port from inside a poll.
struct Reentrant(asyn_rs::port_handle::PortHandle);

impl AsynMotor for Reentrant {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
        Ok(())
    }
    fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
        let _ = self.0.read_int32_blocking(own(), 0);
        Ok(MotorStatus::default())
    }
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "ForbidBlocking")]
fn a_driver_call_to_its_own_port_from_under_the_axis_lock_panics() {
    let port = register_controller_port(&open_owner(), "PSGUARD", 2, Own).unwrap();
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(Reentrant(port)));
    MotorBuilder::new(motor)
        .trace_user(TraceUser::Asyn {
            port: "PSGUARD".into(),
            addr: 1,
        })
        .build_axis();
}

/// PI's axis: its `setPosition` wakes the poller (PIasynAxis.cpp:357), and
/// releasing deferred moves posts DONE=0 to each deferred axis
/// (PIasynController.cpp:251-252); its wakeup (:268) is left out, as the
/// pass it makes would read this hardware as done again.
struct Pi {
    polls: Arc<AtomicI32>,
    done: bool,
    deferred: Vec<i32>,
    sets: PortSets,
}

impl Pi {
    fn new(polls: Arc<AtomicI32>, deferred: Vec<i32>) -> Self {
        Self {
            polls,
            done: true,
            deferred,
            sets: PortSets::default(),
        }
    }
}

impl AsynMotor for Pi {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
        Ok(())
    }
    fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        self.sets.wakeup_poller();
        Ok(())
    }
    fn set_deferred_moves(&mut self, _: &AsynUser, defer: bool) -> AsynResult<()> {
        if !defer {
            for &addr in &self.deferred {
                self.sets.status(addr, StatusWrite::Done(false));
            }
        }
        Ok(())
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Ok(MotorStatus {
            done: self.done,
            ..MotorStatus::default()
        })
    }
    fn take_port_sets(&mut self) -> PortSets {
        std::mem::take(&mut self.sets)
    }
}

/// Axis `addr` of `port`, registered, its poller running on `rt`.
fn pi_axis(rt: &tokio::runtime::Runtime, port: &str, addr: i32, motor: Pi) {
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(motor));
    let setup = MotorBuilder::new(motor)
        .trace_user(TraceUser::Asyn {
            port: port.into(),
            addr,
        })
        .poll_interval(Duration::from_secs(60))
        .build_axis();
    register_axis(setup.axis).unwrap();
    rt.spawn(setup.poll_loop.run());
}

fn eventually(mut f: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn a_set_position_s_wakeup_makes_the_poller_pass() {
    let port = register_controller_port(&open_owner(), "PSWAKE", 2, Own).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let polls = Arc::new(AtomicI32::new(0));
    pi_axis(&rt, "PSWAKE", 1, Pi::new(polls.clone(), Vec::new()));
    // The startup poll.
    assert!(eventually(|| polls.load(Ordering::SeqCst) >= 1));
    let before = polls.load(Ordering::SeqCst);
    port.write_float64_blocking(index::POSITION, 1, 3.0)
        .unwrap();
    assert!(eventually(|| polls.load(Ordering::SeqCst) > before));
}

#[test]
fn a_deferred_move_release_posts_done_0_to_each_deferred_axis() {
    let port = register_controller_port(&open_owner(), "PSDEFER", 4, Own).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    pi_axis(&rt, "PSDEFER", 1, Pi::new(Arc::default(), vec![2, 3]));
    for addr in [2, 3] {
        pi_axis(&rt, "PSDEFER", addr, Pi::new(Arc::default(), Vec::new()));
    }
    let done = |addr| {
        port.read_int32_blocking(index::STATUS, addr)
            .is_ok_and(|word| word & 0x2 != 0)
    };
    // The install posts nothing to the port; an UPDATE_STATUS poll does.
    for addr in [2, 3] {
        port.write_int32_blocking(index::UPDATE_STATUS, addr, 1)
            .unwrap();
    }
    assert!(eventually(|| done(2) && done(3)));
    port.write_int32_blocking(index::DEFER_MOVES, 1, 0).unwrap();
    assert!(eventually(|| !done(2) && !done(3)));
}

/// One poll that sets OWN on its own address twice, first as a port set
/// and then as an axis param, as a C poll's `setIntegerParam(addr, …)`
/// then `pAxis->setIntegerParam(…)` leave the later value.
#[derive(Default)]
struct Ordered;

impl AsynMotor for Ordered {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
        Ok(())
    }
    fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
        Ok(MotorStatus::default())
    }
    fn take_port_sets(&mut self) -> PortSets {
        let mut sets = PortSets::default();
        sets.set(1, own(), ParamValue::Int32(1));
        sets.axis(own(), ParamValue::Int32(2));
        sets
    }
}

fn ordered_axis(port: &str) {
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(Ordered));
    let setup = MotorBuilder::new(motor)
        .trace_user(TraceUser::Asyn {
            port: port.into(),
            addr: 1,
        })
        .poll_interval(Duration::from_secs(60))
        .build_axis();
    register_axis(setup.axis).unwrap();
}

#[test]
fn a_poll_s_port_set_then_axis_param_land_in_that_order_on_the_poller() {
    let port = register_controller_port(&open_owner(), "PSORDP", 2, Own).unwrap();
    // The startup poll runs on the poller.
    ordered_axis("PSORDP");
    assert_eq!(port.read_int32_blocking(own(), 1).unwrap(), 2);
}

#[test]
fn a_poll_s_port_set_then_axis_param_land_in_that_order_on_the_port() {
    let port = register_controller_port(&open_owner(), "PSORDW", 2, Own).unwrap();
    ordered_axis("PSORDW");
    port.write_int32_blocking(own(), 1, 0).unwrap();
    port.write_int32_blocking(index::UPDATE_STATUS, 1, 1)
        .unwrap();
    assert_eq!(port.read_int32_blocking(own(), 1).unwrap(), 2);
}
