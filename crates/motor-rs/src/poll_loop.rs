use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::motor::AsynMotor;
use asyn_rs::user::AsynUser;
use tokio::sync::mpsc;

use crate::device_state::*;

/// Commands sent to the poll loop.
#[derive(Debug)]
pub enum PollCommand {
    StartPolling,
    StopPolling,
    ScheduleDelay(u64, Duration),
    Shutdown,
}

/// Motor poll loop — one per record, stays alive for the record's lifetime.
pub struct MotorPollLoop {
    cmd_rx: mpsc::Receiver<PollCommand>,
    io_intr_tx: mpsc::Sender<()>,
    motor: Arc<Mutex<dyn AsynMotor>>,
    device_state: SharedDeviceState,
    moving_poll_interval: Duration,
    idle_poll_interval: Duration,
    forced_fast_polls_config: u32,
    forced_fast_polls_remaining: u32,
    last_moving: bool,
    status_seq: u64,
}

impl MotorPollLoop {
    pub fn new(
        cmd_rx: mpsc::Receiver<PollCommand>,
        io_intr_tx: mpsc::Sender<()>,
        motor: Arc<Mutex<dyn AsynMotor>>,
        device_state: SharedDeviceState,
        moving_poll_interval: Duration,
        idle_poll_interval: Duration,
        forced_fast_polls: u32,
    ) -> Self {
        Self {
            cmd_rx,
            io_intr_tx,
            motor,
            device_state,
            moving_poll_interval,
            idle_poll_interval,
            forced_fast_polls_config: forced_fast_polls,
            forced_fast_polls_remaining: 0,
            last_moving: false,
            status_seq: 1, // starts at 1 (init already wrote seq=1)
        }
    }

    /// Poll the motor and write stamped status to shared state.
    async fn poll_and_notify(&mut self) {
        let user = AsynUser::new(0);
        let status = {
            let mut motor = match self.motor.lock() {
                Ok(m) => m,
                Err(_) => return,
            };
            match motor.poll(&user) {
                Ok(s) => s,
                Err(_) => return,
            }
        };
        self.last_moving = status.moving;
        self.status_seq += 1;
        {
            match self.device_state.lock() {
                Ok(mut ds) => {
                    ds.latest_status = Some(StampedStatus {
                        seq: self.status_seq,
                        status,
                    });
                }
                Err(e) => {
                    tracing::error!("device state lock poisoned in poll_and_notify: {e}");
                    return;
                }
            }
        }
        let _ = self.io_intr_tx.send(()).await;
    }

    /// Spawn a settle-delay timer as an independent task so the `select!`
    /// loop keeps servicing commands (Shutdown/Stop/Start) while the delay
    /// runs. On expiry it records `expired_delay_id` and pulses io_intr.
    fn spawn_delay(&self, delay_id: u64, dur: Duration) {
        let device_state = self.device_state.clone();
        let io_intr_tx = self.io_intr_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(dur).await;
            match device_state.lock() {
                Ok(mut ds) => ds.expired_delay_id = Some(delay_id),
                Err(e) => {
                    tracing::error!("device state lock poisoned in delay expiry: {e}");
                    return;
                }
            }
            let _ = io_intr_tx.send(()).await;
        });
    }

    fn effective_poll_interval(&mut self) -> Duration {
        if self.forced_fast_polls_remaining > 0 {
            self.forced_fast_polls_remaining -= 1;
            self.moving_poll_interval
        } else if self.last_moving {
            self.moving_poll_interval
        } else {
            self.idle_poll_interval
        }
    }

    /// Run the poll loop. Call from a spawned task.
    pub async fn run(mut self) {
        // Start idle: device support init() sends StartPolling after
        // iocInit, matching C EPICS where the poller starts in init_record.
        let mut active = false;

        loop {
            if active {
                // Poll mode: check for commands or poll on interval
                let interval = self.effective_poll_interval();
                tokio::select! {
                    cmd = self.cmd_rx.recv() => {
                        match cmd {
                            Some(PollCommand::StartPolling) => {
                                active = true;
                                self.forced_fast_polls_remaining = self.forced_fast_polls_config;
                                self.poll_and_notify().await;
                            }
                            Some(PollCommand::StopPolling) => {
                                active = false;
                            }
                            Some(PollCommand::ScheduleDelay(delay_id, dur)) => {
                                active = false;
                                self.spawn_delay(delay_id, dur);
                            }
                            Some(PollCommand::Shutdown) => {
                                return;
                            }
                            None => {
                                return;
                            }
                        }
                    }
                    // C asynMotorController.cpp:633-634: idlePollPeriod_ == 0
                    // blocks on the event (no timed poll). A zero effective
                    // interval disables this timed arm so the loop is
                    // event-driven only, never busy-spinning on sleep(0).
                    _ = tokio::time::sleep(interval), if !interval.is_zero() => {
                        self.poll_and_notify().await;
                    }
                }
            } else {
                // Idle mode: wait for commands only
                match self.cmd_rx.recv().await {
                    Some(PollCommand::StartPolling) => {
                        active = true;
                        self.forced_fast_polls_remaining = self.forced_fast_polls_config;
                        self.poll_and_notify().await;
                    }
                    Some(PollCommand::StopPolling) => {
                        active = false;
                    }
                    Some(PollCommand::ScheduleDelay(delay_id, dur)) => {
                        self.spawn_delay(delay_id, dur);
                    }
                    Some(PollCommand::Shutdown) | None => {
                        return;
                    }
                }
            }
        }
    }
}
