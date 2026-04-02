//! AxisRuntime: per-axis actor for motor control.
//!
//! Promoted from motor-rs/src/axis_runtime.rs with added event emission
//! and shutdown support.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::interfaces::motor::{AsynMotor, MotorStatus};
use crate::user::AsynUser;

use super::event::RuntimeEvent;

/// Commands sent to the AxisRuntime.
#[derive(Debug)]
pub enum AxisCommand {
    Execute {
        actions: AxisActions,
        reply: oneshot::Sender<()>,
    },
    GetStatus {
        reply: oneshot::Sender<Option<MotorStatus>>,
    },
    StartPolling,
    StopPolling,
    ScheduleDelay {
        id: u64,
        duration: Duration,
    },
    Shutdown,
}

/// Actions to execute on an axis.
#[derive(Debug, Default)]
pub struct AxisActions {
    pub commands: Vec<AxisMotorCommand>,
    pub poll: AxisPollDirective,
    pub schedule_delay: Option<AxisDelayRequest>,
    pub status_refresh: bool,
}

/// Motor commands for the axis runtime.
#[derive(Debug, Clone)]
pub enum AxisMotorCommand {
    MoveAbsolute {
        position: f64,
        velocity: f64,
        acceleration: f64,
    },
    MoveVelocity {
        direction: bool,
        velocity: f64,
        acceleration: f64,
    },
    Home {
        forward: bool,
        velocity: f64,
        acceleration: f64,
    },
    Stop {
        acceleration: f64,
    },
    SetPosition {
        position: f64,
    },
    SetClosedLoop {
        enable: bool,
    },
    Poll,
}

/// Poll control directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AxisPollDirective {
    #[default]
    None,
    Start,
    Stop,
}

/// Delay request.
#[derive(Debug, Clone)]
pub struct AxisDelayRequest {
    pub id: u64,
    pub duration: Duration,
}

/// Cloneable handle to an AxisRuntime.
#[derive(Clone)]
pub struct AxisRuntimeHandle {
    tx: mpsc::Sender<AxisCommand>,
    io_intr_rx_take: Arc<Mutex<Option<mpsc::Receiver<()>>>>,
    event_tx: broadcast::Sender<RuntimeEvent>,
    axis_id: i32,
}

impl AxisRuntimeHandle {
    pub async fn execute(&self, actions: AxisActions) {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self
            .tx
            .send(AxisCommand::Execute {
                actions,
                reply: reply_tx,
            })
            .await;
        let _ = reply_rx.await;
    }

    pub async fn get_status(&self) -> Option<MotorStatus> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self
            .tx
            .send(AxisCommand::GetStatus { reply: reply_tx })
            .await;
        reply_rx.await.ok().flatten()
    }

    pub async fn start_polling(&self) {
        let _ = self.tx.send(AxisCommand::StartPolling).await;
    }

    pub async fn stop_polling(&self) {
        let _ = self.tx.send(AxisCommand::StopPolling).await;
    }

    pub async fn schedule_delay(&self, id: u64, duration: Duration) {
        let _ = self
            .tx
            .send(AxisCommand::ScheduleDelay { id, duration })
            .await;
    }

    pub fn take_io_intr_receiver(&self) -> Option<mpsc::Receiver<()>> {
        self.io_intr_rx_take.lock().ok()?.take()
    }

    pub async fn shutdown(&self) {
        let _ = self.tx.send(AxisCommand::Shutdown).await;
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.event_tx.subscribe()
    }

    pub fn axis_id(&self) -> i32 {
        self.axis_id
    }
}

/// Per-axis runtime that owns the motor driver exclusively.
pub struct AxisRuntime {
    motor: Box<dyn AsynMotor>,
    cmd_rx: mpsc::Receiver<AxisCommand>,
    io_intr_tx: mpsc::Sender<()>,
    event_tx: broadcast::Sender<RuntimeEvent>,
    poll_interval: Duration,
    latest_status: Option<MotorStatus>,
    active_polling: bool,
    status_seq: u64,
    axis_id: i32,
}

/// Create an AxisRuntime and its handle.
pub fn create_axis_runtime(
    motor: Box<dyn AsynMotor>,
    poll_interval: Duration,
    axis_id: i32,
) -> (AxisRuntime, AxisRuntimeHandle) {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (io_intr_tx, io_intr_rx) = mpsc::channel(16);
    let (event_tx, _) = broadcast::channel(64);

    let runtime = AxisRuntime {
        motor,
        cmd_rx,
        io_intr_tx,
        event_tx: event_tx.clone(),
        poll_interval,
        latest_status: None,
        active_polling: false,
        status_seq: 0,
        axis_id,
    };

    let handle = AxisRuntimeHandle {
        tx: cmd_tx,
        io_intr_rx_take: Arc::new(Mutex::new(Some(io_intr_rx))),
        event_tx,
        axis_id,
    };

    (runtime, handle)
}

impl AxisRuntime {
    pub async fn run(mut self) {
        let _ = self.event_tx.send(RuntimeEvent::Started {
            port_name: format!("axis-{}", self.axis_id),
        });

        // Initial poll
        self.poll_motor().await;

        loop {
            if self.active_polling {
                tokio::select! {
                    cmd = self.cmd_rx.recv() => {
                        match cmd {
                            Some(cmd) => {
                                if self.handle_command(cmd).await {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    _ = tokio::time::sleep(self.poll_interval) => {
                        self.poll_motor().await;
                    }
                }
            } else {
                match self.cmd_rx.recv().await {
                    Some(cmd) => {
                        if self.handle_command(cmd).await {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }

        let _ = self.event_tx.send(RuntimeEvent::Stopped {
            port_name: format!("axis-{}", self.axis_id),
        });
    }

    async fn handle_command(&mut self, cmd: AxisCommand) -> bool {
        match cmd {
            AxisCommand::Execute { actions, reply } => {
                self.execute_actions(&actions);
                self.apply_poll_directive(&actions);
                if let Some(ref delay) = actions.schedule_delay {
                    let dur = delay.duration;
                    let tx = self.io_intr_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(dur).await;
                        let _ = tx.send(()).await;
                    });
                }
                let _ = reply.send(());
                false
            }
            AxisCommand::GetStatus { reply } => {
                let _ = reply.send(self.latest_status.clone());
                false
            }
            AxisCommand::StartPolling => {
                self.active_polling = true;
                false
            }
            AxisCommand::StopPolling => {
                self.active_polling = false;
                false
            }
            AxisCommand::ScheduleDelay { id: _, duration } => {
                self.active_polling = false;
                let tx = self.io_intr_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(duration).await;
                    let _ = tx.send(()).await;
                });
                false
            }
            AxisCommand::Shutdown => true,
        }
    }

    fn execute_actions(&mut self, actions: &AxisActions) {
        let user = AsynUser::new(0);
        for cmd in &actions.commands {
            let result = match cmd {
                AxisMotorCommand::MoveAbsolute {
                    position,
                    velocity,
                    acceleration,
                } => self
                    .motor
                    .move_absolute(&user, *position, *velocity, *acceleration),
                AxisMotorCommand::MoveVelocity {
                    direction,
                    velocity,
                    acceleration,
                } => {
                    let target = if *direction { 1e9 } else { -1e9 };
                    self.motor
                        .move_absolute(&user, target, *velocity, *acceleration)
                }
                AxisMotorCommand::Home {
                    forward,
                    velocity,
                    acceleration: _,
                } => self.motor.home(&user, *velocity, *forward),
                AxisMotorCommand::Stop { acceleration } => self.motor.stop(&user, *acceleration),
                AxisMotorCommand::SetPosition { position } => {
                    self.motor.set_position(&user, *position)
                }
                AxisMotorCommand::SetClosedLoop { enable } => {
                    self.motor.set_closed_loop(&user, *enable)
                }
                AxisMotorCommand::Poll => Ok(()),
            };
            if let Err(e) = result {
                let _ = self.event_tx.send(RuntimeEvent::Error {
                    port_name: format!("axis-{}", self.axis_id),
                    message: e.to_string(),
                });
            }
        }
    }

    fn apply_poll_directive(&mut self, actions: &AxisActions) {
        match actions.poll {
            AxisPollDirective::Start => self.active_polling = true,
            AxisPollDirective::Stop => self.active_polling = false,
            AxisPollDirective::None => {}
        }
    }

    async fn poll_motor(&mut self) {
        let user = AsynUser::new(0);
        match self.motor.poll(&user) {
            Ok(status) => {
                self.status_seq += 1;
                self.latest_status = Some(status);
                let _ = self.io_intr_tx.send(()).await;
            }
            Err(e) => {
                let _ = self.event_tx.send(RuntimeEvent::Error {
                    port_name: format!("axis-{}", self.axis_id),
                    message: e.to_string(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AsynResult;

    struct SimMotor {
        position: f64,
        target: f64,
        moving: bool,
    }

    impl SimMotor {
        fn new() -> Self {
            Self {
                position: 0.0,
                target: 0.0,
                moving: false,
            }
        }
    }

    impl AsynMotor for SimMotor {
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

    #[tokio::test]
    async fn axis_runtime_basic() {
        let (runtime, handle) =
            create_axis_runtime(Box::new(SimMotor::new()), Duration::from_millis(50), 0);
        let rt_handle = tokio::spawn(runtime.run());

        tokio::time::sleep(Duration::from_millis(10)).await;
        let status = handle.get_status().await.unwrap();
        assert!(status.done);

        let actions = AxisActions {
            commands: vec![AxisMotorCommand::MoveAbsolute {
                position: 10.0,
                velocity: 1.0,
                acceleration: 1.0,
            }],
            poll: AxisPollDirective::Start,
            ..Default::default()
        };
        handle.execute(actions).await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        let status = handle.get_status().await.unwrap();
        assert!((status.position - 10.0).abs() < 1e-10);
        assert!(status.done);

        handle.shutdown().await;
        let _ = rt_handle.await;
    }

    #[tokio::test]
    async fn axis_runtime_events() {
        let (runtime, handle) =
            create_axis_runtime(Box::new(SimMotor::new()), Duration::from_millis(50), 1);
        let mut event_rx = handle.subscribe_events();
        let rt_handle = tokio::spawn(runtime.run());

        // Should receive Started event
        let evt = tokio::time::timeout(Duration::from_millis(100), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match evt {
            RuntimeEvent::Started { port_name } => {
                assert_eq!(port_name, "axis-1");
            }
            _ => panic!("expected Started event"),
        }

        handle.shutdown().await;
        let _ = rt_handle.await;
    }

    #[tokio::test]
    async fn axis_runtime_io_intr() {
        let (runtime, handle) =
            create_axis_runtime(Box::new(SimMotor::new()), Duration::from_millis(50), 0);
        let mut io_intr_rx = handle.take_io_intr_receiver().unwrap();
        let rt_handle = tokio::spawn(runtime.run());

        // Initial poll should trigger io_intr
        let result = tokio::time::timeout(Duration::from_millis(100), io_intr_rx.recv()).await;
        assert!(result.is_ok());

        handle.shutdown().await;
        let _ = rt_handle.await;
    }

    #[tokio::test]
    async fn axis_handle_clone_works() {
        let (runtime, handle) =
            create_axis_runtime(Box::new(SimMotor::new()), Duration::from_millis(50), 0);
        let handle2 = handle.clone();
        let rt_handle = tokio::spawn(runtime.run());

        tokio::time::sleep(Duration::from_millis(10)).await;
        let status = handle2.get_status().await.unwrap();
        assert!((status.position - 0.0).abs() < 1e-10);

        handle.shutdown().await;
        let _ = rt_handle.await;
    }
}
