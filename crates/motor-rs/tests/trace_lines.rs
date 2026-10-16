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
            "devMotorAsyn::asynCallback: XPS:m1 pasyn{Float64,Int32}->write returned port XPS_M not connected",
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
        &[MotorCommand::SetEncoderRatio { ratio: 0.5 }],
    );
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert!(
        lines[1].ends_with(
            "devMotorAsyn::asynCallback: XPS:m1 pasyn{Float64,Int32}->write returned port XPS_M not connected"
        ),
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

#[test]
fn a_model2_axis_prints_drv_motor_asyn_write_lines_and_no_controller_line() {
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
    assert_eq!(lines.len(), 5, "{lines:#?}");
    assert!(
        lines[0].starts_with("drvMotorAsyn::writeFloat64, reason=104, pasynUser=0x"),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains(" pAxis=0x"), "{}", lines[0]);
    let refused = "devMotorAsyn::asynCallback: MM4000:m1 pasyn{Float64,Int32}->write returned port XPS_M not connected";
    assert_eq!(
        lines[1..],
        [
            "drvMotorAsyn::writeFloat64, reason=104, value=5.000000",
            refused,
            "drvMotorAsyn::writeInt32, reason=107, value=0",
            refused,
        ]
    );
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
    tokio::spawn(poll_loop.run());
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
