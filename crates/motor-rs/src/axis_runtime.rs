//! Unified per-axis runtime (Phase 6).
//!
//! Converges command, status polling, and delay scheduling into a single
//! tokio task per axis, eliminating the `SharedDeviceState` mutex.
//!
//! External interface: `AxisHandle` with typed async methods.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus};
use asyn_rs::user::AsynUser;
use tokio::sync::{mpsc, oneshot};

use crate::device_state::*;
use crate::flags::MotorCommand;

/// Commands sent to the AxisRuntime via the AxisHandle.
#[derive(Debug)]
pub(crate) enum AxisCommand {
    /// Execute motor commands and manage polling.
    Execute {
        actions: DeviceActions,
        reply: oneshot::Sender<()>,
    },
    /// Request current status.
    GetStatus {
        reply: oneshot::Sender<Option<MotorStatus>>,
    },
    /// Start active polling.
    StartPolling,
    /// Stop active polling.
    StopPolling,
    /// Schedule a delay.
    ScheduleDelay { duration: Duration },
    /// Shutdown the runtime.
    Shutdown,
}

/// Cloneable handle to an AxisRuntime.
#[derive(Clone)]
pub struct AxisHandle {
    tx: mpsc::Sender<AxisCommand>,
    io_intr_rx_take: Arc<Mutex<Option<mpsc::Receiver<()>>>>,
}

impl AxisHandle {
    /// Send device actions to the axis and wait for execution.
    pub async fn execute(&self, actions: DeviceActions) {
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

    /// Get the latest motor status.
    pub async fn get_status(&self) -> Option<MotorStatus> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self
            .tx
            .send(AxisCommand::GetStatus { reply: reply_tx })
            .await;
        reply_rx.await.ok().flatten()
    }

    /// Start polling.
    pub async fn start_polling(&self) {
        let _ = self.tx.send(AxisCommand::StartPolling).await;
    }

    /// Stop polling.
    pub async fn stop_polling(&self) {
        let _ = self.tx.send(AxisCommand::StopPolling).await;
    }

    /// Schedule a delay.
    pub async fn schedule_delay(&self, _id: u64, duration: Duration) {
        let _ = self.tx.send(AxisCommand::ScheduleDelay { duration }).await;
    }

    /// Take the I/O Intr receiver (can only be called once).
    pub fn take_io_intr_receiver(&self) -> Option<mpsc::Receiver<()>> {
        self.io_intr_rx_take.lock().ok()?.take()
    }

    /// Shutdown the runtime.
    pub async fn shutdown(&self) {
        let _ = self.tx.send(AxisCommand::Shutdown).await;
    }
}

/// Per-axis runtime that owns the motor driver handle exclusively.
pub struct AxisRuntime {
    motor: Box<dyn AsynMotor>,
    cmd_rx: mpsc::Receiver<AxisCommand>,
    io_intr_tx: mpsc::Sender<()>,
    poll_interval: Duration,
    latest_status: Option<MotorStatus>,
    active_polling: bool,
    status_seq: u64,
}

/// Create an AxisRuntime and its handle.
pub fn create_axis_runtime(
    motor: Box<dyn AsynMotor>,
    poll_interval: Duration,
) -> (AxisRuntime, AxisHandle) {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (io_intr_tx, io_intr_rx) = mpsc::channel(16);

    let runtime = AxisRuntime {
        motor,
        cmd_rx,
        io_intr_tx,
        poll_interval,
        latest_status: None,
        active_polling: false,
        status_seq: 0,
    };

    let handle = AxisHandle {
        tx: cmd_tx,
        io_intr_rx_take: Arc::new(Mutex::new(Some(io_intr_rx))),
    };

    (runtime, handle)
}

impl AxisRuntime {
    /// Run the axis runtime. This function runs indefinitely until shutdown.
    pub async fn run(mut self) {
        // Initial poll
        self.poll_motor().await;

        loop {
            if self.active_polling {
                tokio::select! {
                    cmd = self.cmd_rx.recv() => {
                        match cmd {
                            Some(cmd) => {
                                if self.handle_command(cmd).await {
                                    return;
                                }
                            }
                            None => return,
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
                            return;
                        }
                    }
                    None => return,
                }
            }
        }
    }

    /// Handle a command. Returns true if runtime should shut down.
    async fn handle_command(&mut self, cmd: AxisCommand) -> bool {
        match cmd {
            AxisCommand::Execute { actions, reply } => {
                self.execute_actions(&actions);
                self.apply_poll_directive(&actions);
                if let Some(ref delay) = actions.schedule_delay {
                    let dur = delay.duration;
                    let tx = self.io_intr_tx.clone();
                    // Spawn a delay task - when done, it just triggers io_intr
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
            AxisCommand::ScheduleDelay { duration } => {
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

    fn execute_actions(&mut self, actions: &DeviceActions) {
        let user = AsynUser::new(0);
        for cmd in &actions.commands {
            let result = match cmd {
                MotorCommand::MoveAbsolute {
                    position,
                    velocity,
                    acceleration,
                } => self
                    .motor
                    .move_absolute(&user, *position, *velocity, *acceleration),
                MotorCommand::MoveVelocity {
                    direction,
                    velocity,
                    acceleration,
                } => {
                    let target = if *direction { 1e9 } else { -1e9 };
                    self.motor
                        .move_absolute(&user, target, *velocity, *acceleration)
                }
                MotorCommand::Home {
                    forward,
                    velocity,
                    acceleration: _,
                } => self.motor.home(&user, *velocity, *forward),
                MotorCommand::Stop { acceleration } => self.motor.stop(&user, *acceleration),
                MotorCommand::SetPosition { position } => self.motor.set_position(&user, *position),
                MotorCommand::SetClosedLoop { enable } => {
                    self.motor.set_closed_loop(&user, *enable)
                }
                MotorCommand::Poll => Ok(()),
            };
            if let Err(e) = result {
                tracing::error!("motor command error: {e}");
            }
        }
    }

    fn apply_poll_directive(&mut self, actions: &DeviceActions) {
        match actions.poll {
            PollDirective::Start => self.active_polling = true,
            PollDirective::Stop => self.active_polling = false,
            PollDirective::None => {}
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
                tracing::error!("motor poll error: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asyn_rs::error::AsynResult;

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
            // Simple sim: instantly reach target
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
            create_axis_runtime(Box::new(SimMotor::new()), Duration::from_millis(50));

        let runtime_handle = tokio::spawn(runtime.run());

        // Get initial status (from initial poll)
        tokio::time::sleep(Duration::from_millis(10)).await;
        let status = handle.get_status().await.unwrap();
        assert!(status.done);
        assert!((status.position - 0.0).abs() < 1e-10);

        // Execute a move
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

        // Wait for poll to pick up the move completion
        tokio::time::sleep(Duration::from_millis(100)).await;

        let status = handle.get_status().await.unwrap();
        assert!((status.position - 10.0).abs() < 1e-10);
        assert!(status.done);

        handle.shutdown().await;
        let _ = runtime_handle.await;
    }

    #[tokio::test]
    async fn axis_runtime_polling_start_stop() {
        let (runtime, handle) =
            create_axis_runtime(Box::new(SimMotor::new()), Duration::from_millis(20));

        let runtime_handle = tokio::spawn(runtime.run());

        handle.start_polling().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.stop_polling().await;

        handle.shutdown().await;
        let _ = runtime_handle.await;
    }

    #[tokio::test]
    async fn axis_runtime_set_position() {
        let (runtime, handle) =
            create_axis_runtime(Box::new(SimMotor::new()), Duration::from_millis(50));

        let runtime_handle = tokio::spawn(runtime.run());

        let actions = DeviceActions {
            commands: vec![MotorCommand::SetPosition { position: 5.0 }],
            poll: PollDirective::None,
            ..Default::default()
        };
        handle.execute(actions).await;

        // Force a poll to update status
        handle.start_polling().await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let status = handle.get_status().await.unwrap();
        assert!((status.position - 5.0).abs() < 1e-10);

        handle.shutdown().await;
        let _ = runtime_handle.await;
    }

    #[tokio::test]
    async fn axis_runtime_io_intr() {
        let (runtime, handle) =
            create_axis_runtime(Box::new(SimMotor::new()), Duration::from_millis(50));

        let mut io_intr_rx = handle.take_io_intr_receiver().unwrap();

        let runtime_handle = tokio::spawn(runtime.run());

        // Initial poll should trigger io_intr
        let result = tokio::time::timeout(Duration::from_millis(100), io_intr_rx.recv()).await;
        assert!(result.is_ok());

        handle.shutdown().await;
        let _ = runtime_handle.await;
    }

    #[tokio::test]
    async fn axis_handle_clone() {
        let (runtime, handle) =
            create_axis_runtime(Box::new(SimMotor::new()), Duration::from_millis(50));

        let handle2 = handle.clone();

        let runtime_handle = tokio::spawn(runtime.run());

        tokio::time::sleep(Duration::from_millis(10)).await;
        let status = handle2.get_status().await.unwrap();
        assert!((status.position - 0.0).abs() < 1e-10);

        handle.shutdown().await;
        let _ = runtime_handle.await;
    }
}
