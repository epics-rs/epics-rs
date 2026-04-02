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
    poll_interval: Duration,
    status_seq: u64,
}

impl MotorPollLoop {
    pub fn new(
        cmd_rx: mpsc::Receiver<PollCommand>,
        io_intr_tx: mpsc::Sender<()>,
        motor: Arc<Mutex<dyn AsynMotor>>,
        device_state: SharedDeviceState,
        poll_interval: Duration,
    ) -> Self {
        Self {
            cmd_rx,
            io_intr_tx,
            motor,
            device_state,
            poll_interval,
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

    /// Run the poll loop. Call from a spawned task.
    pub async fn run(mut self) {
        // Start active: initial poll triggers I/O Intr so the record
        // picks up the first motor status (clears LVIO, sets MSTA, etc.)
        let mut active = true;

        loop {
            if active {
                // Poll mode: check for commands or poll on interval
                tokio::select! {
                    cmd = self.cmd_rx.recv() => {
                        match cmd {
                            Some(PollCommand::StartPolling) => {
                                active = true;
                                self.poll_and_notify().await;
                            }
                            Some(PollCommand::StopPolling) => {
                                active = false;
                            }
                            Some(PollCommand::ScheduleDelay(delay_id, dur)) => {
                                active = false;
                                tokio::time::sleep(dur).await;
                                match self.device_state.lock() {
                                    Ok(mut ds) => { ds.expired_delay_id = Some(delay_id); }
                                    Err(e) => {
                                        tracing::error!("device state lock poisoned in delay expiry: {e}");
                                        continue;
                                    }
                                }
                                let _ = self.io_intr_tx.send(()).await;
                            }
                            Some(PollCommand::Shutdown) => {
                                return;
                            }
                            None => {
                                return;
                            }
                        }
                    }
                    _ = tokio::time::sleep(self.poll_interval) => {
                        self.poll_and_notify().await;
                    }
                }
            } else {
                // Idle mode: wait for commands only
                match self.cmd_rx.recv().await {
                    Some(PollCommand::StartPolling) => {
                        active = true;
                        self.poll_and_notify().await;
                    }
                    Some(PollCommand::StopPolling) => {
                        active = false;
                    }
                    Some(PollCommand::ScheduleDelay(delay_id, dur)) => {
                        tokio::time::sleep(dur).await;
                        match self.device_state.lock() {
                            Ok(mut ds) => {
                                ds.expired_delay_id = Some(delay_id);
                            }
                            Err(e) => {
                                tracing::error!("device state lock poisoned in delay expiry: {e}");
                                continue;
                            }
                        }
                        let _ = self.io_intr_tx.send(()).await;
                    }
                    Some(PollCommand::Shutdown) | None => {
                        return;
                    }
                }
            }
        }
    }
}
