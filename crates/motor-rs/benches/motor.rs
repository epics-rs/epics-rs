use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus};
use asyn_rs::user::AsynUser;
use motor_rs::axis_runtime::create_axis_runtime;
use motor_rs::device_state::{DeviceActions, PollDirective};
use motor_rs::flags::MotorCommand;

struct BenchMotor {
    position: f64,
    target: f64,
    moving: bool,
}

impl BenchMotor {
    fn new() -> Self {
        Self {
            position: 0.0,
            target: 0.0,
            moving: false,
        }
    }
}

impl AsynMotor for BenchMotor {
    fn move_absolute(
        &mut self,
        _user: &AsynUser,
        pos: f64,
        _vel: f64,
        _acc: f64,
    ) -> AsynResult<()> {
        self.target = pos;
        self.moving = true;
        Ok(())
    }

    fn home(&mut self, _user: &AsynUser, _vel: f64, _forward: bool) -> AsynResult<()> {
        self.target = 0.0;
        self.moving = true;
        Ok(())
    }

    fn stop(&mut self, _user: &AsynUser, _acc: f64) -> AsynResult<()> {
        self.target = self.position;
        self.moving = false;
        Ok(())
    }

    fn set_position(&mut self, _user: &AsynUser, pos: f64) -> AsynResult<()> {
        self.position = pos;
        self.target = pos;
        Ok(())
    }

    fn poll(&mut self, _user: &AsynUser) -> AsynResult<MotorStatus> {
        if self.moving {
            self.position = self.target;
            self.moving = false;
        }
        Ok(MotorStatus {
            position: self.position,
            encoder_position: self.position,
            done: !self.moving,
            moving: self.moving,
            ..MotorStatus::default()
        })
    }
}

fn bench_motor_move_to_done(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    c.bench_function("motor_move_to_done", |b| {
        b.iter(|| {
            rt.block_on(async {
                let (runtime, handle) =
                    create_axis_runtime(Box::new(BenchMotor::new()), Duration::from_millis(10));
                let rt_handle = tokio::spawn(runtime.run());

                // Wait for initial poll
                tokio::time::sleep(Duration::from_millis(5)).await;

                // Issue move
                let actions = DeviceActions {
                    commands: vec![MotorCommand::MoveAbsolute {
                        position: 10.0,
                        velocity: 1.0,
                        acceleration: 1.0,
                    }],
                    poll: PollDirective::Start,
                    ..Default::default()
                };
                handle.execute(actions).await;

                // Wait for poll to pick up done
                tokio::time::sleep(Duration::from_millis(20)).await;

                let status = handle.get_status().await.unwrap();
                assert!(status.done);

                handle.shutdown().await;
                let _ = rt_handle.await;
            });
        });
    });
}

criterion_group!(benches, bench_motor_move_to_done);
criterion_main!(benches);
