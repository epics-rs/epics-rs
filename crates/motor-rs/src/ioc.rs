//! IOC integration for motor-rs: the `simMotorCreate` command.

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

                // Spawn poll loop on the tokio runtime (starts idle)
                ctx.bridge().spawn(poll_loop.run());

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
