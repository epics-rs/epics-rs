//! C parity tests for UEIP/URIP readback.

use motor_rs::flags::*;
use motor_rs::record::MotorRecord;

use asyn_rs::interfaces::motor::MotorStatus;

fn make_record() -> MotorRecord {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.limits.hlm = 100.0;
    rec.limits.llm = -100.0;
    rec.limits.lvio = false;
    rec.stat.msta = MstaFlags::DONE;
    rec
}

fn make_status(pos: f64, enc: f64) -> MotorStatus {
    MotorStatus {
        position: pos,
        encoder_position: enc,
        done: true,
        moving: false,
        ..Default::default()
    }
}

#[test]
fn ueip_true_full_encoder_path() {
    let mut rec = make_record();
    rec.conv.mres = 0.001;
    rec.conv.eres = 0.0005; // encoder finer than motor
    rec.conv.ueip = true;

    let status = make_status(10.0, 10.005);
    rec.process_motor_info(&status);

    // REP = round(10.005 / 0.0005) = 20010
    assert_eq!(rec.pos.rep, 20010);
    // RMP = round(10.0 / 0.001) = 10000
    assert_eq!(rec.pos.rmp, 10000);
    // RRBV = REP (UEIP=true)
    assert_eq!(rec.pos.rrbv, 20010);
    // DRBV = RRBV * ERES = 20010 * 0.0005 = 10.005
    assert!((rec.pos.drbv - 10.005).abs() < 1e-10);
    // RBV = dial_to_user(DRBV) = 10.005
    assert!((rec.pos.rbv - 10.005).abs() < 1e-10);
}

#[test]
fn ueip_false_uses_motor_position_path() {
    let mut rec = make_record();
    rec.conv.mres = 0.001;
    rec.conv.eres = 0.0005;
    rec.conv.ueip = false;

    let status = make_status(10.0, 20.0);
    rec.process_motor_info(&status);

    // RMP = round(10.0 / 0.001) = 10000
    assert_eq!(rec.pos.rmp, 10000);
    // RRBV = RMP (UEIP=false)
    assert_eq!(rec.pos.rrbv, 10000);
    // DRBV = RRBV * MRES = 10000 * 0.001 = 10.0
    assert!((rec.pos.drbv - 10.0).abs() < 1e-10);
}

#[test]
fn ueip_true_invalid_eres_falls_back() {
    let mut rec = make_record();
    rec.conv.mres = 0.001;
    rec.conv.ueip = true;

    // Zero ERES
    rec.conv.eres = 0.0;
    let status = make_status(10.0, 10.0);
    rec.process_motor_info(&status);
    assert_eq!(rec.pos.rep, 10000); // fell back to MRES
    assert!((rec.pos.drbv - 10.0).abs() < 1e-10);

    // NaN ERES
    rec.conv.eres = f64::NAN;
    rec.process_motor_info(&status);
    assert_eq!(rec.pos.rep, 10000);
    assert!((rec.pos.drbv - 10.0).abs() < 1e-10);

    // Inf ERES
    rec.conv.eres = f64::INFINITY;
    rec.process_motor_info(&status);
    assert_eq!(rec.pos.rep, 10000);
    assert!((rec.pos.drbv - 10.0).abs() < 1e-10);
}

#[test]
fn rbv_drbv_rrbv_consistency_ueip() {
    let mut rec = make_record();
    rec.conv.mres = 0.002;
    rec.conv.eres = 0.001;
    rec.conv.ueip = true;
    rec.conv.dir = MotorDir::Pos;
    rec.pos.off = 5.0;

    let status = make_status(10.0, 10.0);
    rec.process_motor_info(&status);

    // REP = round(10.0 / 0.001) = 10000
    // RRBV = 10000
    // DRBV = 10000 * 0.001 = 10.0
    // RBV = 1*10.0 + 5.0 = 15.0
    assert_eq!(rec.pos.rep, 10000);
    assert_eq!(rec.pos.rrbv, 10000);
    assert!((rec.pos.drbv - 10.0).abs() < 1e-10);
    assert!((rec.pos.rbv - 15.0).abs() < 1e-10);
}

#[test]
fn rbv_with_dir_neg() {
    let mut rec = make_record();
    rec.conv.mres = 0.001;
    rec.conv.ueip = false;
    rec.conv.dir = MotorDir::Neg;
    rec.pos.off = 0.0;

    let status = make_status(10.0, 10.0);
    rec.process_motor_info(&status);

    // DRBV = 10.0
    // RBV = -1*10.0 + 0 = -10.0
    assert!((rec.pos.drbv - 10.0).abs() < 1e-10);
    assert!((rec.pos.rbv - (-10.0)).abs() < 1e-10);
}

#[test]
fn movn_reflects_phase_and_driver() {
    let mut rec = make_record();
    rec.conv.mres = 0.001;

    // Idle + not moving → MOVN=false
    rec.stat.phase = MotionPhase::Idle;
    let status = make_status(0.0, 0.0);
    rec.process_motor_info(&status);
    assert!(!rec.stat.movn);

    // Active phase + not moving → MOVN=true
    rec.stat.phase = MotionPhase::MainMove;
    rec.process_motor_info(&status);
    assert!(rec.stat.movn);

    // Idle + driver moving → MOVN=true
    rec.stat.phase = MotionPhase::Idle;
    let moving = MotorStatus {
        moving: true,
        done: false,
        ..Default::default()
    };
    rec.process_motor_info(&moving);
    assert!(rec.stat.movn);
}
