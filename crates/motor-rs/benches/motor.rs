use asyn_rs::interfaces::motor::MotorStatus;
use criterion::{Criterion, criterion_group, criterion_main};
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;
use motor_rs::MotorRecord;
use motor_rs::flags::{CommandSource, MstaFlags};

/// Full record-level move cycle: issue a VAL write, plan the motion, apply a
/// driver "done" status, and run completion. Exercises the core record logic
/// (coordinate cascade, command planning, state machine) without any async
/// runtime or driver I/O.
fn bench_motor_move_to_done(c: &mut Criterion) {
    c.bench_function("motor_move_to_done", |b| {
        b.iter(|| {
            let mut rec = MotorRecord::new();
            rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
            rec.plan_motion(CommandSource::Val);

            let status = MotorStatus {
                position: 10.0,
                encoder_position: 10.0,
                done: true,
                moving: false,
                ..MotorStatus::default()
            };
            rec.process_motor_info(&status);
            rec.stat.msta = MstaFlags::DONE;
            rec.check_completion();
        });
    });
}

criterion_group!(benches, bench_motor_move_to_done);
criterion_main!(benches);
