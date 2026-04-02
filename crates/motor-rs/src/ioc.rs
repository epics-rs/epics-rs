//! IOC integration for motor-rs: `simMotorCreate` command and dynamic device support.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::iocsh::registry::*;

use crate::builder::MotorBuilder;
use crate::device_support::MotorDeviceSupport;
use crate::sim_motor::SimMotor;

/// Holds sim motor device support instances created by `simMotorCreate`.
///
/// Each motor is stored under the key `"simMotor_{port}"` and consumed
/// once by the dynamic device support factory during iocInit.
pub struct SimMotorHolder {
    motors: Mutex<HashMap<String, Option<MotorDeviceSupport>>>,
}

impl SimMotorHolder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            motors: Mutex::new(HashMap::new()),
        })
    }

    /// Create a `simMotorCreate` iocsh command.
    ///
    /// Usage: `simMotorCreate("port", lowLimit, highLimit, pollMs)`
    ///
    /// Creates a SimMotor with the given limits and poll interval,
    /// spawns the poll loop on the tokio runtime, and stores the
    /// device support for later binding via `dbLoadRecords`.
    pub fn sim_motor_create_command(self: &Arc<Self>) -> CommandDef {
        let holder = self.clone();
        CommandDef::new(
            "simMotorCreate",
            vec![
                ArgDesc {
                    name: "port",
                    arg_type: ArgType::String,
                    optional: false,
                },
                ArgDesc {
                    name: "lowLimit",
                    arg_type: ArgType::Double,
                    optional: false,
                },
                ArgDesc {
                    name: "highLimit",
                    arg_type: ArgType::Double,
                    optional: false,
                },
                ArgDesc {
                    name: "pollMs",
                    arg_type: ArgType::Int,
                    optional: true,
                },
            ],
            "simMotorCreate(port, lowLimit, highLimit, [pollMs]) - Create a simulated motor",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = match &args[0] {
                    ArgValue::String(s) => s.clone(),
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
                    ArgValue::Int(v) => *v as u64,
                    ArgValue::Missing => 100,
                    _ => return Err("pollMs must be an integer".into()),
                };

                let dtyp_key = format!("simMotor_{port}");

                let motor: Arc<Mutex<dyn asyn_rs::interfaces::motor::AsynMotor>> = Arc::new(
                    Mutex::new(SimMotor::new().with_limits(low_limit, high_limit)),
                );

                let setup = MotorBuilder::new(motor)
                    .poll_interval(Duration::from_millis(poll_ms))
                    .build();

                let crate::builder::MotorSetup {
                    record: _,
                    device_support,
                    poll_loop,
                    poll_cmd_tx: _,
                } = setup;

                let device_support = device_support.with_dtyp_name(dtyp_key.clone());

                // Spawn poll loop on the tokio runtime
                ctx.runtime_handle().spawn(poll_loop.run());

                holder
                    .motors
                    .lock()
                    .unwrap()
                    .insert(dtyp_key.clone(), Some(device_support));
                println!(
                    "simMotorCreate: port={port} limits=[{low_limit}, {high_limit}] poll={poll_ms}ms (DTYP={dtyp_key})"
                );
                Ok(CommandOutcome::Continue)
            },
        )
    }

    /// Return a dynamic device support factory that dispatches by DTYP name.
    ///
    /// Each device support is consumed once (take semantics).
    pub fn device_support_factory(
        self: &Arc<Self>,
    ) -> impl Fn(
        &epics_ca_rs::server::ioc_app::DeviceSupportContext,
    ) -> Option<Box<dyn epics_base_rs::server::device_support::DeviceSupport>>
    + Send
    + Sync
    + 'static {
        let holder = self.clone();
        move |ctx: &epics_ca_rs::server::ioc_app::DeviceSupportContext| {
            let mut motors = holder.motors.lock().unwrap();
            if let Some(slot) = motors.get_mut(ctx.dtyp) {
                if let Some(ds) = slot.take() {
                    return Some(Box::new(ds)
                        as Box<dyn epics_base_rs::server::device_support::DeviceSupport>);
                }
            }
            None
        }
    }
}
