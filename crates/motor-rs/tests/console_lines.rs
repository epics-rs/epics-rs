//! The lines motorRecord.cc printf's, on stdout, under the record's name.
//!
//! libtest captures `println!` inside the test process, so each scenario runs
//! in a child copy of this binary and the parent reads the child's stdout.

use std::time::Duration;

use asyn_rs::interfaces::motor::MotorStatus;
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::types::EpicsValue;
use motor_rs::device_support::MotorDeviceSupport;
use motor_rs::flags::*;
use motor_rs::record::MotorRecord;

const SCENARIO: &str = "MOTOR_CONSOLE_SCENARIO";

/// `m1`, named the way an IOC names it: through its device support.
fn named_record() -> MotorRecord {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.limits.hlm = 100.0;
    rec.limits.llm = -100.0;
    rec.vel.velo = 10.0;
    rec.vel.accl = 0.5;
    rec.stat.msta = MstaFlags::DONE;
    rec.timing.ntm = true;

    let (poll_cmd_tx, poll_cmd_rx) = tokio::sync::mpsc::channel(16);
    let (motor_cmd_tx, motor_cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    std::mem::forget((poll_cmd_rx, motor_cmd_rx));
    let mut dev = MotorDeviceSupport::new(
        Default::default(),
        Duration::from_secs(1),
        poll_cmd_tx,
        motor_cmd_tx,
        motor_rs::device_state::new_shared_state(),
    );
    dev.set_record_info("m1", ScanType::Passive);
    dev.init(&mut rec as &mut dyn Record).unwrap();
    rec
}

/// A moving status at dial `position`, reported in steps.
fn moving(rec: &mut MotorRecord, position: f64, direction: bool) {
    let steps = position / rec.conv.mres;
    rec.process_motor_info(&MotorStatus {
        position: steps,
        encoder_position: steps,
        done: false,
        moving: true,
        direction,
        ..Default::default()
    });
}

fn run(scenario: &str) {
    match scenario {
        "ntm-write" => {
            let mut rec = named_record();
            rec.pos.dval = 50.0;
            rec.pos.val = 50.0;
            rec.plan_motion(CommandSource::Val);
            moving(&mut rec, 25.0, true);
            rec.pos.dval = -10.0;
            rec.pos.val = -10.0;
            let effects = rec.plan_motion(CommandSource::Val);
            assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
        }
        "ntm-poll" => {
            let mut rec = named_record();
            rec.pos.dval = 50.0;
            rec.pos.val = 50.0;
            rec.plan_motion(CommandSource::Val);
            // Overshot to 80 while still moving: the error now points back.
            moving(&mut rec, 80.0, false);
            let effects = rec.check_completion();
            assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
        }
        "sync-rdbl-error" | "sync-rdbl-ok" => {
            let mut rec = named_record();
            rec.conv.urip = true;
            rec.set_rdbl_error(scenario == "sync-rdbl-error");
            rec.put_field("SYNC", EpicsValue::Short(1)).unwrap();
            rec.plan_motion(CommandSource::Sync);
            assert_eq!(rec.get_field("SYNC"), Some(EpicsValue::Short(0)));
        }
        "stop-rdbl-error" => {
            let mut rec = named_record();
            rec.conv.urip = true;
            rec.set_rdbl_error(true);
            rec.stat.mip = MipFlags::JOG_REQ;
            rec.plan_motion(CommandSource::Stop);
        }
        other => panic!("unknown scenario {other}"),
    }
}

/// The child's entry: runs only when the parent names a scenario.
#[test]
fn scenario_child() {
    if let Ok(scenario) = std::env::var(SCENARIO) {
        run(&scenario);
    }
}

/// The record's own lines from `scenario`, libtest's chatter dropped.
fn printed(scenario: &str) -> Vec<String> {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "scenario_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(SCENARIO, scenario)
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "{stdout}");
    stdout
        .lines()
        .map(|l| l.trim_start_matches("test scenario_child ... "))
        .filter(|l| l.starts_with("m1"))
        .map(str::to_owned)
        .collect()
}

#[test]
fn an_ntm_stop_on_a_target_write_prints_tdir() {
    assert_eq!(printed("ntm-write"), ["m1:tdir = 1"]);
}

#[test]
fn an_ntm_stop_at_poll_prints_tdir() {
    assert_eq!(printed("ntm-poll"), ["m1:tdir = 0"]);
}

#[test]
fn a_sync_with_the_readback_link_down_prints_the_rdbl_error() {
    assert_eq!(
        printed("sync-rdbl-error"),
        ["m1: syncTargetPosition: error reading RDBL link."]
    );
}

#[test]
fn a_sync_with_the_readback_link_up_prints_nothing() {
    assert!(printed("sync-rdbl-ok").is_empty());
}

#[test]
fn a_stop_with_nothing_moving_and_the_readback_link_down_prints_the_rdbl_error() {
    assert_eq!(
        printed("stop-rdbl-error"),
        ["m1: syncTargetPosition: error reading RDBL link."]
    );
}
