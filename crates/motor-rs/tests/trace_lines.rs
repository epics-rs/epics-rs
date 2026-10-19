//! The asyn trace lines a motor prints, in C's words, on the axis's port.

use std::sync::{Arc, Mutex};

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus};
use asyn_rs::manager::PortManager;
use asyn_rs::trace::TraceFile;
use asyn_rs::user::AsynUser;
use motor_rs::flags::MotorCommand;
use motor_rs::poll_loop::execute_commands;
use motor_rs::trace::{MotorTrace, TraceUser};

/// An axis whose controller dropped the connection: every write fails the way
/// the XPS move socket does while asyn holds off its reconnect.
struct Disconnected;

fn refused() -> AsynResult<()> {
    Err(AsynError::Status {
        status: AsynStatus::Disconnected,
        message: "port XPS_M not connected".into(),
    })
}

impl AsynMotor for Disconnected {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        refused()
    }
    fn move_relative(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        refused()
    }
    fn move_velocity(&mut self, _: &AsynUser, _: f64, _: f64, _: f64) -> AsynResult<()> {
        refused()
    }
    fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
        refused()
    }
    fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        refused()
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        refused()
    }
    fn set_deferred_moves(&mut self, _: &AsynUser, _: bool) -> AsynResult<()> {
        refused()
    }
    fn set_encoder_ratio(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        refused()
    }
    fn set_high_limit(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        refused()
    }
    fn set_low_limit(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        refused()
    }
    fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
        asyn_rs::interfaces::motor::PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
        refused().map(|()| MotorStatus::default())
    }
}

/// Run `body` with `port`'s trace file a fresh file; return its lines.
fn traced_by(port: &str, body: impl FnOnce()) -> Vec<String> {
    let path = std::env::temp_dir().join(format!("motor-trace-{port}-{}", std::process::id()));
    let file = std::fs::File::create(&path).unwrap();
    PortManager::global()
        .trace_manager()
        .set_trace_file(Some(port), TraceFile::File(Arc::new(Mutex::new(file))));
    body();
    let text = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    text.lines().map(str::to_owned).collect()
}

/// Run `commands` queued at MRES 1 on a port whose trace file is a fresh
/// file; return its lines.
fn traced(port: &str, commands: &[MotorCommand]) -> Vec<String> {
    traced_by(port, || {
        let trace = MotorTrace::new(port, 0);
        trace.set_record("XPS:m1");
        execute_commands(&mut Disconnected, &trace, commands, &mut |_| {});
    })
}

#[test]
fn a_move_refused_by_a_disconnected_port_prints_the_controller_then_the_device_line() {
    let lines = traced(
        "traceLinesMove",
        &[MotorCommand::MoveAbsolute {
            position: 5.0,
            min_velocity: 0.1,
            velocity: 1.0,
            acceleration: 0.2,
        }],
    );
    let tails: Vec<&str> = lines
        .iter()
        .map(|l| {
            &l[l.find("asynMotorController")
                .or(l.find("devMotorAsyn"))
                .unwrap()..]
        })
        .collect();
    assert_eq!(
        tails,
        [
            "asynMotorController:writeFloat64 error, status=4 axis=0, function=1, value=5.000000",
            // The driver's "port XPS_M not connected" is not the record's
            // errorMessage; C nPoint LC400 prints this line with an empty tail.
            "devMotorAsyn::asynCallback: XPS:m1 pasyn{Float64,Int32}->write returned ",
        ]
    );
}

#[test]
fn a_refused_deferred_move_prints_only_the_controller_line() {
    let lines = traced(
        "traceLinesDefer",
        &[MotorCommand::DeferMoves { defer: true }],
    );
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(
        lines[0].ends_with(
            "asynMotorController:writeInt32 error, status=4 axis=0, function=11, value=1"
        ),
        "{lines:#?}"
    );
}

#[test]
fn init_reads_the_cached_status_and_a_lost_controller_prints_nothing() {
    use asyn_rs::interfaces::motor::MotorStatus;
    use epics_base_rs::server::device_support::DeviceSupport;
    use epics_base_rs::server::record::{Record, ScanType};
    use motor_rs::device_state::StampedStatus;
    use motor_rs::device_support::MotorDeviceSupport;

    // C readGenericPointer copies the parameter cache and fails only for a
    // bad axis (asynMotorController.cpp:505-525), so init_record prints
    // nothing for a controller that is down: the poller's last status, here
    // its COMM_ERR one, is what init sees.
    let port = "traceLinesInit";
    let state = motor_rs::device_state::new_shared_state();
    state.lock().unwrap().latest_status = Some(StampedStatus {
        seq: 1,
        status: MotorStatus {
            position: 250.0,
            comms_error: true,
            problem: true,
            ..MotorStatus::default()
        },
    });
    let mut rec = motor_rs::MotorRecord::new();
    rec.conv.mres = 0.01;
    let lines = traced_by(port, || {
        let (poll_cmd_tx, _poll_cmd_rx) = tokio::sync::mpsc::channel(16);
        let (motor_cmd_tx, _motor_cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut dev = MotorDeviceSupport::new(
            Arc::new(MotorTrace::new(port, 0)),
            std::time::Duration::from_secs(1),
            poll_cmd_tx,
            motor_cmd_tx,
            state.clone(),
        );
        dev.set_record_info("XPS:m1", ScanType::Passive);
        dev.init(&mut rec as &mut dyn Record).unwrap();
    });
    assert!(lines.is_empty(), "{lines:#?}");
    assert_eq!(rec.pos.rrbv, 250);
    assert!(rec.stat.msta.contains(motor_rs::flags::MstaFlags::COMM_ERR));
}

#[test]
fn a_refused_init_position_load_prints_init_controller_in_raw_steps() {
    let lines = traced(
        "traceLinesInitPos",
        &[MotorCommand::InitPosition { position: 500.0 }],
    );
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert!(
        lines[0].ends_with(
            "asynMotorController:writeFloat64 error, status=4 axis=0, function=9, value=500.000000"
        ),
        "{lines:#?}"
    );
    assert!(
        lines[1].ends_with(
            "devMotorAsyn::init_controller, XPS:m1 failed to set position to 500.000000"
        ),
        "{lines:#?}"
    );
}

#[test]
fn limits_print_on_the_parameter_they_write() {
    use asyn_rs::trace::TraceMask;

    // What the record sends for DHLM 2, DLLM -1 at MRES -0.01
    // (motorRecord.cc:4243-4249).
    let port = "traceLinesNegLimits";
    PortManager::global()
        .trace_manager()
        .set_trace_mask(Some(port), TraceMask::ERROR | TraceMask::FLOW);
    let lines = traced(
        port,
        &[
            MotorCommand::SetLowLimit { position: -200.0 },
            MotorCommand::SetHighLimit { position: 100.0 },
        ],
    );
    let tails: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.find("asynMotorController").map(|i| &l[i..]))
        .collect();
    assert_eq!(
        tails,
        [
            "asynMotorController:writeFloat64: Set driver traceLinesNegLimits, axis 0 low limit=-200.000000",
            "asynMotorController:writeFloat64 error, status=4 axis=0, function=19, value=-200.000000",
            "asynMotorController:writeFloat64: Set driver traceLinesNegLimits, axis 0 high limit=100.000000",
            "asynMotorController:writeFloat64 error, status=4 axis=0, function=18, value=100.000000",
        ]
    );
}

#[test]
fn a_move_prints_the_raw_values_it_carries() {
    use asyn_rs::trace::TraceMask;

    let port = "traceLinesMoveSteps";
    PortManager::global()
        .trace_manager()
        .set_trace_mask(Some(port), TraceMask::FLOW);
    let lines = traced(
        port,
        &[MotorCommand::MoveAbsolute {
            position: -500.0,
            min_velocity: 10.0,
            velocity: 100.0,
            acceleration: 200.0,
        }],
    );
    assert_eq!(
        lines
            .iter()
            .map(|l| &l[l.find("asynMotorController").unwrap()..])
            .collect::<Vec<_>>(),
        [
            "asynMotorController:writeFloat64: Set driver traceLinesMoveSteps, axis 0 move absolute to -500.000000, base velocity=10.000000, velocity=100.000000, acceleration=200.000000"
        ]
    );
}

#[test]
fn a_refused_init_encoder_ratio_prints_init_controller() {
    let lines = traced(
        "traceLinesInitRatio",
        &[MotorCommand::InitEncoderRatio { ratio: 0.5 }],
    );
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert!(
        lines[0].ends_with(
            "asynMotorController:writeFloat64 error, status=4 axis=0, function=14, value=0.500000"
        ),
        "{lines:#?}"
    );
    assert!(
        lines[1].ends_with(
            "devMotorAsyn::init_controller, XPS:m1 failed to set encoder ratio to 0.500000"
        ),
        "{lines:#?}"
    );
}

#[test]
fn a_refused_runtime_encoder_ratio_prints_the_async_callback_line() {
    let lines = traced(
        "traceLinesRatio",
        &[MotorCommand::SetEncoderRatio { pulses: [0.5, 1.0] }],
    );
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert!(
        lines[1]
            .ends_with("devMotorAsyn::asynCallback: XPS:m1 pasyn{Float64,Int32}->write returned "),
        "{lines:#?}"
    );
}

#[test]
fn an_init_that_loads_no_position_says_so_under_flow() {
    use asyn_rs::interfaces::motor::MotorStatus;
    use asyn_rs::trace::TraceMask;

    let port = "traceLinesNoLoad";
    PortManager::global()
        .trace_manager()
        .set_trace_mask(Some(port), TraceMask::ERROR | TraceMask::FLOW);
    let lines = traced_by(port, || {
        let trace = Arc::new(MotorTrace::new(port, 0));
        trace.set_record("XPS:m1");
        let mut rec = motor_rs::MotorRecord::new();
        rec.set_trace(trace);
        rec.conv.mres = 0.01;
        rec.conv.rstm = motor_rs::flags::RestoreMode::Never;
        rec.pos.dval = 3.0;
        rec.initial_readback(&MotorStatus {
            position: 100.0,
            ..MotorStatus::default()
        });
    });
    assert_eq!(
        lines
            .iter()
            .map(|l| &l[l.find("devMotorAsyn").unwrap()..])
            .collect::<Vec<_>>(),
        [
            "devMotorAsyn::init_controller, XPS:m1 setting of position not required, position=100.000000, mres=0.010000, dval=3.000000, rdbd=0.010000"
        ]
    );
}

/// `user`'s lines for a refused stop, from the port file and the default one.
fn stop_traced(user: TraceUser) -> Vec<String> {
    let path = std::env::temp_dir().join(format!("motor-trace-default-{}", std::process::id()));
    let file = std::fs::File::create(&path).unwrap();
    PortManager::global()
        .trace_manager()
        .set_trace_file(None, TraceFile::File(Arc::new(Mutex::new(file))));
    let mut lines = traced_by("traceLinesModel1", || {
        let trace = MotorTrace::with_user(user);
        trace.set_record("PIC844:m1");
        execute_commands(
            &mut Disconnected,
            &trace,
            &[MotorCommand::Stop { acceleration: 1.0 }],
            &mut |_| {},
        );
    });
    lines.extend(
        std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_owned),
    );
    let _ = std::fs::remove_file(&path);
    lines
}

#[test]
fn a_model1_axis_prints_nothing_for_a_refused_command() {
    assert!(
        !stop_traced(TraceUser::Asyn {
            port: "traceLinesModel1".into(),
            addr: 0
        })
        .is_empty()
    );
    assert!(
        stop_traced(TraceUser::Model1 {
            device: "traceLinesModel1",
            card: 0,
            signal: 0,
        })
        .is_empty()
    );
}

/// `user`'s lines, under ERROR|FLOW|IO_DRIVER, for a refused move then stop.
fn move_and_stop_traced(port: &str, user: TraceUser) -> Vec<String> {
    use asyn_rs::trace::TraceMask;

    PortManager::global().trace_manager().set_trace_mask(
        Some(port),
        TraceMask::ERROR | TraceMask::FLOW | TraceMask::IO_DRIVER,
    );
    let lines = traced_by(port, || {
        let trace = MotorTrace::with_user(user);
        trace.set_record("MM4000:m1");
        let queued = [
            MotorCommand::MoveAbsolute {
                position: 5.0,
                min_velocity: 0.1,
                velocity: 1.0,
                acceleration: 0.2,
            },
            MotorCommand::Stop { acceleration: 1.0 },
        ];
        execute_commands(&mut Disconnected, &trace, &queued, &mut |_| {});
    });
    lines
        .iter()
        .map(|l| {
            let at = ["drvMotorAsyn", "asynMotorController", "devMotorAsyn"]
                .iter()
                .filter_map(|w| l.find(w))
                .min()
                .unwrap();
            l[at..].to_owned()
        })
        .collect()
}

/// drvMotorAsyn's own lines are its port's (drvMotorAsyn.c:576-752): a
/// model-2 axis prints devMotorAsyn's alone, and no asynMotorController line.
#[test]
fn a_model2_axis_prints_neither_drv_motor_asyn_nor_controller_lines() {
    let model3 = move_and_stop_traced(
        "traceLinesModel3",
        TraceUser::Asyn {
            port: "traceLinesModel3".into(),
            addr: 0,
        },
    );
    assert!(
        model3.iter().any(|l| l.starts_with("asynMotorController")),
        "{model3:#?}"
    );

    let lines = move_and_stop_traced(
        "traceLinesModel2",
        TraceUser::Model2 {
            port: "traceLinesModel2".into(),
            addr: 0,
        },
    );
    let refused = "devMotorAsyn::asynCallback: MM4000:m1 pasyn{Float64,Int32}->write returned ";
    assert_eq!(lines, [refused, refused]);
}

/// The device-support half of a move under ASYN_TRACEIO_DEVICE, as C
/// motorSim prints it: init's `update_values`, the put pass that reads no
/// status, the posts while the move is pending (`P`), and a callback pass
/// per post once it finished.
#[tokio::test(flavor = "multi_thread")]
async fn status_callback_and_update_values_print_under_traceio_device() {
    use std::collections::HashMap;
    use std::time::Duration;

    use asyn_rs::registry::open_owner;
    use asyn_rs::trace::TraceMask;
    use epics_base_rs::server::ioc_builder::IocBuilder;
    use epics_base_rs::types::EpicsValue;
    use motor_rs::builder::{AxisSetup, MotorBuilder};
    use motor_rs::devices::{
        device_support_factory, devices, register_axis, register_controller_port,
    };

    let port = "traceLinesIoDevice";
    // The controller's own user connects device 0 when the port is created
    // (asynPortDriver.cpp:4099), so the record's lines take device 0's
    // trace file: `asynSetTraceFile port 0`, not `-1`.
    register_controller_port(&open_owner(), port, 1, ()).unwrap();
    PortManager::global()
        .trace_manager()
        .set_trace_mask(Some(port), TraceMask::IO_DEVICE);
    let path = std::env::temp_dir().join(format!("motor-trace-{port}-{}", std::process::id()));
    PortManager::global().trace_manager().set_device_trace_file(
        port,
        0,
        TraceFile::File(Arc::new(Mutex::new(std::fs::File::create(&path).unwrap()))),
    );
    let motor: Arc<Mutex<dyn AsynMotor>> =
        Arc::new(Mutex::new(motor_rs::sim_motor::SimMotor::new()));
    let AxisSetup {
        axis, poll_loop, ..
    } = MotorBuilder::new(motor)
        .trace_user(TraceUser::Asyn {
            port: port.into(),
            addr: 0,
        })
        .poll_interval(Duration::from_millis(10))
        .build_axis();
    register_axis(axis).unwrap();
    tokio::spawn(poll_loop.start());
    let (name, factory) = motor_rs::motor_record_factory();
    let ioc = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(
            &format!(
                r#"record(motor, "TL:m") {{ field(DTYP, "asynMotor") field(OUT, "@asyn({port},0)") field(MRES, "0.01") field(VELO, "10") }}"#
            ),
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = Arc::clone(ioc.database());
    assert!(ioc.run().await);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let booted = std::fs::read_to_string(&path).unwrap().len();
    db.put_record_field_from_ca("TL:m", "VAL", EpicsValue::Double(0.1))
        .await
        .unwrap();
    for _ in 0..300 {
        if db.get_pv("TL:m.DMOV").unwrap() == EpicsValue::Short(1) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let text = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let tail = |l: &str| l[l.find("TL:m ").unwrap()..].to_owned();
    let boot: Vec<String> = text[..booted].lines().map(tail).collect();
    let moved: Vec<String> = text[booted..].lines().map(tail).collect();
    assert_eq!(boot, ["TL:m devMotorAsyn::update_values, needUpdate=1"]);
    assert_eq!(
        moved[..3],
        [
            "TL:m devMotorAsyn::update_values, needUpdate=0",
            "TL:m devMotorAsyn::statusCallback new value=[p:0.000000,e:0.000000,s:80]  P",
            "TL:m devMotorAsyn::update_values, needUpdate=1",
        ],
        "{moved:#?}"
    );
    assert_eq!(
        moved[moved.len() - 2..],
        [
            "TL:m devMotorAsyn::statusCallback new value=[p:10.000000,e:10.000000,s:2]   ",
            "TL:m devMotorAsyn::update_values, needUpdate=1",
        ],
        "{moved:#?}"
    );
}

/// An axis whose driver refuses a deferred move with a param-library status.
struct UndefinedParam;

impl AsynMotor for UndefinedParam {
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
    fn set_deferred_moves(&mut self, _: &AsynUser, _: bool) -> AsynResult<()> {
        Err(AsynError::ParamUndefined(11))
    }
    fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
        asyn_rs::interfaces::motor::PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
        Ok(MotorStatus::default())
    }
}

#[test]
fn a_param_library_refusal_prints_its_own_status_code() {
    let port = "traceLinesParam";
    let lines = traced_by(port, || {
        let trace = MotorTrace::new(port, 0);
        let commands = [MotorCommand::DeferMoves { defer: true }];
        execute_commands(&mut UndefinedParam, &trace, &commands, &mut |_| {});
    });
    assert_eq!(lines.len(), 1, "{lines:#?}");
    // asynParamUndefined is 10; AsynStatus has no such value.
    assert!(
        lines[0].ends_with(
            "asynMotorController:writeInt32 error, status=10 axis=0, function=11, value=1"
        ),
        "{lines:#?}"
    );
}

/// An axis whose every write succeeds.
struct Quiet;

impl AsynMotor for Quiet {
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
    fn set_closed_loop(&mut self, _: &AsynUser, _: bool) -> AsynResult<()> {
        Ok(())
    }
    fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
        asyn_rs::interfaces::motor::PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
        Ok(MotorStatus::default())
    }
}

/// The `asynCallback` lines, past `pmsg=<address>`, of a record's speed,
/// move, closed-loop and GET_INFO writes on a `user` axis of `port`.
fn async_callback_lines(port: &str, user: TraceUser) -> Vec<String> {
    use asyn_rs::trace::TraceMask;
    use motor_rs::device_state::AxisSubscribers;
    use motor_rs::poll_loop::{CommandBatch, MotorPollLoop, PollerInbox};
    use std::time::Duration;
    use tokio::sync::mpsc;

    PortManager::global()
        .trace_manager()
        .set_trace_mask(Some(port), TraceMask::FLOW);
    let lines = traced_by(port, || {
        let (_poll_tx, poll) = mpsc::channel(4);
        let (tx, commands) = mpsc::unbounded_channel();
        let mut poll_loop = MotorPollLoop::new(
            PollerInbox { poll, commands },
            Arc::new(Mutex::new(Quiet)),
            AxisSubscribers::new(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            0,
        );
        let trace = MotorTrace::with_user(user);
        trace.set_record("TL:m1");
        tx.send(motor_rs::poll_loop::ToAxis::from(CommandBatch {
            trace: Arc::new(trace),
            record: Some(0),
            commands: vec![
                MotorCommand::SetVelocity { velocity: 4.0 },
                MotorCommand::MoveAbsolute {
                    position: 5.0,
                    min_velocity: 0.0,
                    velocity: 4.0,
                    acceleration: 0.0,
                },
                MotorCommand::SetClosedLoop { enable: true },
                MotorCommand::Poll,
            ],
        }))
        .unwrap();
        poll_loop.run_queued_commands();
    });
    lines
        .iter()
        .filter_map(|l| {
            let at = l.find("devMotorAsyn::asynCallback")?;
            let size = l.find(", sizeof(*pmsg)").unwrap();
            assert!(l[..size].contains(" pmsg=0x"), "{l}");
            Some(format!(
                "{}{}",
                &l[at..l.find("pmsg=").unwrap()],
                &l[size..]
            ))
        })
        .collect()
}

#[test]
fn each_record_write_prints_async_callbacks_flow_line() {
    #[repr(C)]
    struct Message(i32, i32, i32, f64);
    let size = std::mem::size_of::<Message>();
    let line = |command, interface, ivalue, dvalue: f64, reason| {
        format!(
            "devMotorAsyn::asynCallback: TL:m1 , sizeof(*pmsg)={size}, pmsg->command={command},pmsg->interface={interface}, pmsg->ivalue={ivalue}, pmsg->dvalue={dvalue:.6}, pasynUser->reason={reason}"
        )
    };
    let model3 = async_callback_lines(
        "traceLinesCallback3",
        TraceUser::Asyn {
            port: "traceLinesCallback3".into(),
            addr: 0,
        },
    );
    assert_eq!(
        model3,
        [
            line(5, 1, 0, 4.0, 5),
            line(0, 1, 0, 5.0, 1),
            line(16, 0, 1, 0.0, 20),
            line(18, 0, 0, 0.0, 27)
        ]
    );
    let model2 = async_callback_lines(
        "traceLinesCallback2",
        TraceUser::Model2 {
            port: "traceLinesCallback2".into(),
            addr: 0,
        },
    );
    assert_eq!(
        model2,
        [
            line(5, 1, 0, 4.0, 100),
            line(0, 1, 0, 5.0, 104),
            line(16, 0, 1, 0.0, 8),
            line(18, 0, 0, 0.0, 109)
        ]
    );
}

/// The `build_trans` lines of a model-3 record `TL:m1` on `port` through
/// init_record (DHLM 100, DLLM -100) and one VAL=5 process pass, past each
/// `pmsg=<address>`.
fn build_trans_lines(port: &str) -> Vec<String> {
    use asyn_rs::trace::TraceMask;
    use epics_base_rs::server::device_support::DeviceSupport;
    use epics_base_rs::server::record::Record;
    use epics_base_rs::types::EpicsValue;
    use motor_rs::device_state::new_shared_state;
    use motor_rs::device_support::MotorDeviceSupport;
    use std::time::Duration;
    use tokio::sync::mpsc;

    PortManager::global()
        .trace_manager()
        .set_trace_mask(Some(port), TraceMask::FLOW);
    let lines = traced_by(port, || {
        let trace = MotorTrace::new(port, 0);
        trace.set_record("TL:m1");
        let (poll_tx, _polls) = mpsc::channel(4);
        let (motor_tx, _commands) = mpsc::unbounded_channel();
        let mut support = MotorDeviceSupport::new(
            Arc::new(trace),
            Duration::ZERO,
            poll_tx,
            motor_tx,
            new_shared_state(),
        );
        let mut rec = motor_rs::MotorRecord::new();
        rec.conv.mres = 1.0;
        rec.limits.dhlm = 100.0;
        rec.limits.dllm = -100.0;
        rec.limits.hlm = 100.0;
        rec.limits.llm = -100.0;
        rec.vel.velo = 2.0;
        rec.vel.vbas = 1.0;
        rec.vel.accl = 0.5;
        support.init(&mut rec).unwrap();
        rec.put_field("VAL", EpicsValue::Double(5.0)).unwrap();
        rec.process().unwrap();
        support.write(&mut rec).unwrap();
    });
    lines
        .iter()
        .filter(|l| l.contains("build_trans"))
        .map(|l| match (l.find("pmsg="), l.find(", sizeof")) {
            (Some(at), Some(size)) => format!("{}{}", &l[..at], &l[size..]),
            _ => l.clone(),
        })
        // init_controller's line has no newline (devMotorAsyn.c:235-237).
        .map(|l| {
            let at = l.find("devMotorAsyn::build_trans");
            l[at.or(l.find("devAsynMotor::build_trans")).unwrap()..].to_owned()
        })
        .collect()
}

#[test]
fn a_record_prints_build_trans_flow_lines_with_its_pact() {
    let lines = build_trans_lines("traceLinesBuild");
    let call = |cmnd, pact, value: f64| {
        format!("devMotorAsyn::build_trans: TL:m1 motor_cmnd={cmnd}, pact={pact}, value={value:.6}")
    };
    let queue = |command, interface, dvalue: f64| {
        format!(
            "devAsynMotor::build_trans: calling queueRequest, , sizeof(*pmsg)=24pmsg->command={command}, pmsg->interface={interface}, pmsg->dvalue={dvalue:.6}"
        )
    };
    assert_eq!(
        lines,
        [
            // init_record's dial limits (motorRecord.cc:717-718).
            call(19, 0, 100.0),
            queue(14, 1, 100.0),
            call(20, 0, -100.0),
            queue(15, 1, -100.0),
            // do_work's move (:2527-2535): MOVE_ABS queues only at GO.
            call(6, 1, 2.0),
            queue(5, 1, 2.0),
            call(5, 1, 1.0),
            queue(6, 1, 1.0),
            call(7, 1, 2.0),
            queue(7, 1, 2.0),
            call(0, 1, 5.0),
            call(8, 1, 0.0),
            queue(0, 1, 5.0),
        ]
    );
}

/// A subclass taking the proportional gain and the resolution itself, as
/// omsBaseController's writeFloat64 does without calling the base
/// (omsBaseController.cpp:358-372).
struct OwnGain;

impl motor_rs::controller_port::WriteHook for OwnGain {
    fn handles(&self, function: usize) -> bool {
        use motor_rs::controller_port::index;
        matches!(function, index::PGAIN | index::RESOLUTION)
    }
}

struct OwnGainParams;

impl motor_rs::controller_port::ControllerParams for OwnGainParams {
    fn write_hook(&self) -> Option<Arc<dyn motor_rs::controller_port::WriteHook>> {
        Some(Arc::new(OwnGain))
    }
}

/// The base prints nothing for a write the subclass takes itself, whether
/// an axis or the port finishes it; a write it passes on still prints the
/// base's lines.
#[test]
fn a_write_the_subclass_takes_itself_prints_no_base_line() {
    use asyn_rs::interfaces::motor::PidGainKind;
    use asyn_rs::registry::open_owner;
    use asyn_rs::trace::TraceMask;

    let port = "traceLinesOwnGain";
    let handle =
        motor_rs::devices::register_controller_port(&open_owner(), port, 1, OwnGainParams).unwrap();
    let (motor_tx, _commands) = tokio::sync::mpsc::unbounded_channel();
    let (poll_tx, _polls) = tokio::sync::mpsc::channel(1);
    motor_rs::devices::register_axis(motor_rs::device_support::MotorAxis::new(
        TraceUser::Asyn {
            port: port.into(),
            addr: 0,
        },
        std::sync::Arc::new(std::sync::Mutex::new(motor_rs::sim_motor::SimMotor::new())),
        std::time::Duration::from_secs(1),
        poll_tx,
        motor_tx,
        motor_rs::device_state::AxisSubscribers::new(),
    ))
    .unwrap();
    PortManager::global().trace_manager().set_trace_mask(
        Some(port),
        TraceMask::ERROR | TraceMask::FLOW | TraceMask::IO_DRIVER,
    );
    // The port's own user connected device 0, whose trace file is its own.
    let lines = |write: &dyn Fn()| {
        let path = std::env::temp_dir().join(format!("motor-trace-{port}-{}", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        PortManager::global().trace_manager().set_device_trace_file(
            port,
            0,
            TraceFile::File(Arc::new(Mutex::new(file))),
        );
        write();
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        text.lines()
            .filter(|l| l.contains("asynMotorController"))
            .count()
    };
    let base = |commands: &[MotorCommand]| {
        lines(&|| {
            let trace = MotorTrace::new(port, 0);
            execute_commands(&mut Disconnected, &trace, commands, &mut |_| {});
        })
    };
    let gain = MotorCommand::SetPidGain {
        kind: PidGainKind::Proportional,
        gain: 2.0,
    };
    assert_eq!(base(&[gain]), 0);
    assert_eq!(base(&[MotorCommand::SetHighLimit { position: 1.0 }]), 2);
    // Neither maps to an axis command, so the port finishes both.
    let on_port = |function| {
        lines(&|| {
            handle.write_float64_blocking(function, 0, 0.5).unwrap();
        })
    };
    use motor_rs::controller_port::index;
    assert_eq!(on_port(index::RESOLUTION), 0);
    assert_eq!(on_port(index::ACCEL), 1);
}

/// phytron's poll: a failed exchange raises the problem bit and returns
/// asynSuccess (phytronAxisMotor.cpp:1842-1872).
struct FailsQuietly;

impl AsynMotor for FailsQuietly {
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
    fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
        asyn_rs::interfaces::motor::PollFailure::PROBLEM.succeeding()
    }
    fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
        refused().map(|()| MotorStatus::default())
    }
}

#[test]
fn a_get_info_whose_poll_c_returns_success_from_prints_no_error() {
    let port = "traceLinesQuietPoll";
    let lines = traced_by(port, || {
        let trace = MotorTrace::new(port, 0);
        trace.set_record("PH:m1");
        execute_commands(
            &mut FailsQuietly,
            &trace,
            &[MotorCommand::Poll],
            &mut |_| {},
        );
    });
    assert!(lines.is_empty(), "{lines:#?}");
}
