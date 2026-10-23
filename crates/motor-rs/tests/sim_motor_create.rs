//! `simMotorCreate` does what C's `motorSimCreateController` and
//! `motorSimConfigAxis` do together, and prints only what they print.
//!
//! libtest captures `println!` inside the test process, so the command runs
//! in a child copy of this binary and the parent reads the child's stdout.

use std::sync::Arc;

use epics_base_rs::runtime::task::BlockingBridge;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandContext};
use motor_rs::ioc::SimMotorHolder;

const CHILD: &str = "SIM_MOTOR_CREATE_CHILD";

/// The child's entry: runs only when the parent sets [`CHILD`].
#[test]
fn create_child() {
    if std::env::var(CHILD).is_err() {
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _rt = rt.enter();
    let ctx = CommandContext::new(Arc::new(PvDatabase::new()), BlockingBridge::capture());
    let args = [
        ArgValue::String("SIMCREATE".into()),
        ArgValue::Double(-100.0),
        ArgValue::Double(100.0),
        ArgValue::Missing,
    ];
    let cmd = SimMotorHolder::new().sim_motor_create_command();
    cmd.handler.call(&args, &ctx).unwrap();
}

/// motorSimCreateController prints nothing and motorSimConfigAxis prints
/// its `configuring` line (motorSimDriver.cpp:482-486, :502-503).
#[test]
fn creating_a_sim_motor_prints_the_config_axis_line() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "create_child", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "{stdout}");
    let lines: Vec<_> = stdout
        .lines()
        .map(|l| l.trim_start_matches("test create_child ... "))
        .filter(|l| l.contains("SIMCREATE"))
        .collect();
    assert_eq!(
        lines,
        ["motorSimDriver:motorSimConfigAxis: configuring controller SIMCREATE axis 0"]
    );
}
