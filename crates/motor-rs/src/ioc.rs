//! IOC integration for motor-rs: the `simMotorCreate` command and
//! asynMotorController.cpp's `setMovingPollPeriod`, `setIdlePollPeriod` and
//! `asynMotorEnableMoveToHome`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::iocsh::registry::*;

use crate::builder::MotorBuilder;
use crate::sim_motor::SimMotor;

/// The poll loops of the motors `simMotorCreate` made. Each motor is axis 0
/// of its port, reached by DTYP `asynMotor` and OUT `@asyn(port,0)`
/// ([`crate::devices`]).
pub struct SimMotorHolder {
    poll_senders: Mutex<Vec<tokio::sync::mpsc::Sender<crate::poll_loop::PollCommand>>>,
}

impl SimMotorHolder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            poll_senders: Mutex::new(Vec::new()),
        })
    }

    /// Force a status post on every registered motor.
    /// Call after PINI processing to avoid queue buildup.
    pub fn start_all_polling(&self) {
        for tx in self
            .poll_senders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            let _ = tx.try_send(crate::poll_loop::PollCommand::ForcePoll);
        }
    }

    /// Create a `simMotorCreate` iocsh command.
    ///
    /// Usage: `simMotorCreate("port", lowLimit, highLimit, pollMs)`
    ///
    /// Creates a SimMotor with the given limits, in steps as C
    /// `motorSimConfigAxis` takes them, and poll interval,
    /// spawns the poll loop on the tokio runtime, and registers the axis
    /// for the record whose OUT names it.
    pub fn sim_motor_create_command(self: &Arc<Self>) -> CommandDef {
        let holder = self.clone();
        CommandDef::new(
            "simMotorCreate",
            vec![
                ArgDesc {
                    name: "port",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "lowLimit",
                    arg_type: ArgType::Double,
                },
                ArgDesc {
                    name: "highLimit",
                    arg_type: ArgType::Double,
                },
                ArgDesc {
                    name: "pollMs",
                    arg_type: ArgType::Int,
                },
            ],
            "simMotorCreate(port, lowLimit, highLimit, [pollMs]) - Create a simulated motor (limits in steps)",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = match &args[0] {
                    ArgValue::String(s) => s.to_str()?.to_owned(),
                    _ => return Err("port must be a string".into()),
                };
                let low_limit = match &args[1] {
                    ArgValue::Double(v) => *v,
                    _ => return Err("lowLimit must be a number".into()),
                };
                let high_limit = match &args[2] {
                    ArgValue::Double(v) => *v,
                    _ => return Err("highLimit must be a number".into()),
                };
                let poll_ms = match &args[3] {
                    ArgValue::Int(v) if *v > 0 => *v as u64,
                    ArgValue::Int(_) => return Err("pollMs must be positive".into()),
                    ArgValue::Missing => 100,
                    _ => return Err("pollMs must be an integer".into()),
                };

                let motor: Arc<Mutex<dyn asyn_rs::interfaces::motor::AsynMotor>> = Arc::new(
                    Mutex::new(SimMotor::new().with_limits(low_limit, high_limit)),
                );

                let crate::builder::AxisSetup {
                    axis,
                    poll_loop,
                    poll_cmd_tx,
                } = MotorBuilder::new(motor)
                    .trace_user(crate::trace::TraceUser::Asyn {
                        port: port.clone(),
                        addr: 0,
                    })
                    .poll_interval(Duration::from_millis(poll_ms))
                    .build_axis();

                crate::devices::register_controller_port(
                    ctx.db().scan_control().interrupt_gate(),
                    &port,
                    1,
                    (),
                )?;
                crate::devices::register_axis(axis)?;

                // The motorPoller thread, as C startPoller creates it.
                poll_loop.start();

                holder
                    .poll_senders
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(poll_cmd_tx);
                println!(
                    "simMotorCreate: port={port} limits=[{low_limit}, {high_limit}] poll={poll_ms}ms"
                );
                Ok(CommandOutcome::Continue)
            },
        )
    }
}

/// C `asynMotorEnableMoveToHome(portName, axis, distance)`
/// (asynMotorController.cpp:983-1007): the distance a driver's
/// `doMoveToHome` reads back as `getReferencingModeMove()`.
pub fn enable_move_to_home_command() -> CommandDef {
    CommandDef::new(
        "asynMotorEnableMoveToHome",
        vec![
            ArgDesc {
                name: "Controller port name",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "Axis number",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "Distance",
                arg_type: ArgType::Int,
            },
        ],
        "asynMotorEnableMoveToHome(port, axis, distance)",
        |args: &[ArgValue], ctx: &CommandContext| {
            let port = match &args[0] {
                ArgValue::String(s) => s.to_str()?.to_owned(),
                _ => String::new(),
            };
            let int = |arg: &ArgValue| match arg {
                ArgValue::Int(v) => *v as i32,
                _ => 0,
            };
            let (axis, distance) = (int(&args[1]), int(&args[2]));
            let name = "asynMotorController:asynMotorEnableMoveToHome";
            if asyn_rs::manager::PortManager::global()
                .find_port_handle(&port)
                .is_err()
            {
                ctx.println(&format!("{name}: Error port {port} not found"));
                return Ok(CommandOutcome::Continue);
            }
            let Some(found) = crate::devices::axis_at(&port, axis) else {
                ctx.println(&format!("{name}: Error axis {axis} not found"));
                return Ok(CommandOutcome::Continue);
            };
            if distance <= 0 {
                ctx.println(&format!(
                    "{name}: Error distance must be positive integer axis={axis}"
                ));
            } else {
                found.set_referencing_mode_move(distance);
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// C `asynMotorControllerRegister` (asynMotorController.cpp:1051-1056),
/// which motorSupport.dbd gives every motor IOC.
pub fn asyn_motor_controller_commands() -> [CommandDef; 3] {
    [
        set_moving_poll_period_command(),
        set_idle_poll_period_command(),
        enable_move_to_home_command(),
    ]
}

/// C `setMovingPollPeriod(portName, movingPollPeriod)`
/// (asynMotorController.cpp:918-965).
pub fn set_moving_poll_period_command() -> CommandDef {
    poll_period_command("setMovingPollPeriod", "moving", |poll, period| {
        poll.set_moving_period(period)
    })
}

/// C `setIdlePollPeriod(portName, idlePollPeriod)`
/// (asynMotorController.cpp:934-979).
pub fn set_idle_poll_period_command() -> CommandDef {
    poll_period_command("setIdlePollPeriod", "idle", |poll, period| {
        poll.set_idle_period(period)
    })
}

fn poll_period_command(
    name: &'static str,
    which: &'static str,
    set: fn(&crate::poll_loop::SharedControllerPoll, Duration),
) -> CommandDef {
    CommandDef::new(
        name,
        vec![
            ArgDesc {
                name: "Controller port name",
                arg_type: ArgType::String,
            },
            // C's label for the period.
            ArgDesc {
                name: "Axis number",
                arg_type: ArgType::Double,
            },
        ],
        format!("{name}(port, period)"),
        move |args: &[ArgValue], ctx: &CommandContext| {
            let port = match &args[0] {
                ArgValue::String(s) => s.to_str()?.to_owned(),
                _ => String::new(),
            };
            let secs = match &args[1] {
                ArgValue::Double(v) => *v,
                _ => 0.0,
            };
            let function = format!("asynMotorController:{name}");
            let Some(poll) = crate::devices::controller_poll(&port) else {
                ctx.println(&format!("{function}: Error port {port} not found"));
                return Ok(CommandOutcome::Continue);
            };
            asyn_rs::manager::PortManager::global()
                .trace_manager()
                .print(
                    Some(&port),
                    Some(0),
                    0,
                    asyn_rs::trace::TraceMask::FLOW,
                    format_args!("{function}: Setting {which} poll period to {secs:.6}\n"),
                );
            // A negative period would have C's poller spin; it waits on the
            // wakeup alone here, as a zero period does.
            // Before startPoller, whose periods replace C's setting.
            if let Some(poll) = poll {
                set(&poll, Duration::try_from_secs_f64(secs).unwrap_or_default());
                poll.wakeup();
            }
            Ok(CommandOutcome::Continue)
        },
    )
}
