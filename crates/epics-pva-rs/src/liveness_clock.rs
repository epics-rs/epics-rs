//! Time for the peer-silence watchdogs of both connection sides: the server's
//! inactivity deadline (`server_native::tcp`) and the client's heartbeat
//! (`client_native::server_conn`). A test drives `ManualClock` so a deadline
//! assertion does not race the scheduler: on the wall clock, a sleep that load
//! stretches past the deadline is real silence.

#[cfg(test)]
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[derive(Clone, Default)]
pub(crate) enum LivenessClock {
    #[default]
    System,
    #[cfg(test)]
    Manual(Arc<ManualClock>),
}

impl LivenessClock {
    /// Nanoseconds on a monotonic timeline: a wall-clock step must not
    /// read a live peer as silent, as pvxs's bufferevent timeouts do not.
    pub(crate) fn now_nanos(&self) -> u64 {
        match self {
            Self::System => {
                static ORIGIN: OnceLock<Instant> = OnceLock::new();
                ORIGIN.get_or_init(Instant::now).elapsed().as_nanos() as u64
            }
            #[cfg(test)]
            Self::Manual(clock) => clock.now_nanos(),
        }
    }

    pub(crate) async fn sleep(&self, period: Duration) {
        match self {
            Self::System => epics_base_rs::runtime::task::sleep(period).await,
            #[cfg(test)]
            Self::Manual(clock) => clock.sleep(period).await,
        }
    }

    /// A periodic ticker with `runtime::task::interval`'s semantics: the
    /// first tick is immediate and missed ticks burst.
    pub(crate) fn interval(&self, period: Duration) -> LivenessTicker {
        match self {
            Self::System => LivenessTicker::System(epics_base_rs::runtime::task::interval(period)),
            #[cfg(test)]
            Self::Manual(clock) => LivenessTicker::Manual {
                next: clock.now_nanos(),
                period: period.as_nanos() as u64,
                clock: clock.clone(),
            },
        }
    }
}

pub(crate) enum LivenessTicker {
    System(epics_base_rs::runtime::task::Interval),
    #[cfg(test)]
    Manual {
        clock: Arc<ManualClock>,
        next: u64,
        period: u64,
    },
}

impl LivenessTicker {
    pub(crate) async fn tick(&mut self) {
        match self {
            Self::System(interval) => interval.tick().await,
            #[cfg(test)]
            Self::Manual {
                clock,
                next,
                period,
            } => {
                clock.sleep_until(*next).await;
                *next += *period;
            }
        }
    }
}

/// A clock that moves only on [`ManualClock::advance`], and records every
/// deadline armed against it so a test can assert on the re-arm itself:
/// one-shot sleeps and ticker deadlines apart, since a connection runs both.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct ManualClock {
    state: std::sync::Mutex<ManualClockState>,
}

#[cfg(test)]
#[derive(Default)]
struct ManualClockState {
    now: u64,
    armed: Vec<u64>,
    ticks: Vec<u64>,
    sleepers: Vec<std::task::Waker>,
}

#[cfg(test)]
impl ManualClock {
    pub(crate) fn advance(&self, by: Duration) {
        let sleepers = {
            let mut state = self.state.lock().unwrap();
            state.now += by.as_nanos() as u64;
            std::mem::take(&mut state.sleepers)
        };
        sleepers.into_iter().for_each(std::task::Waker::wake);
    }

    /// Every [`LivenessClock::sleep`] deadline armed so far, as time since
    /// the clock's zero.
    pub(crate) fn armed(&self) -> Vec<Duration> {
        let state = self.state.lock().unwrap();
        state
            .armed
            .iter()
            .map(|&at| Duration::from_nanos(at))
            .collect()
    }

    /// Every [`LivenessTicker::tick`] deadline armed so far, as time since
    /// the clock's zero.
    pub(crate) fn ticks(&self) -> Vec<Duration> {
        let state = self.state.lock().unwrap();
        state
            .ticks
            .iter()
            .map(|&at| Duration::from_nanos(at))
            .collect()
    }

    fn now_nanos(&self) -> u64 {
        self.state.lock().unwrap().now
    }

    async fn sleep(&self, period: Duration) {
        let deadline = {
            let mut state = self.state.lock().unwrap();
            let deadline = state.now + period.as_nanos() as u64;
            state.armed.push(deadline);
            deadline
        };
        self.wait_until(deadline).await
    }

    async fn sleep_until(&self, deadline: u64) {
        self.state.lock().unwrap().ticks.push(deadline);
        self.wait_until(deadline).await
    }

    async fn wait_until(&self, deadline: u64) {
        std::future::poll_fn(|cx| {
            let mut state = self.state.lock().unwrap();
            if state.now >= deadline {
                return std::task::Poll::Ready(());
            }
            if !state.sleepers.iter().any(|w| w.will_wake(cx.waker())) {
                state.sleepers.push(cx.waker().clone());
            }
            std::task::Poll::Pending
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CLOCK_REALTIME step cannot be made from a test, so pin the
    /// timeline: the system clock must not read the wall clock.
    #[test]
    fn system_clock_is_not_the_wall_clock() {
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let now = LivenessClock::System.now_nanos();
        assert!(
            wall.abs_diff(now) > Duration::from_secs(86_400).as_nanos() as u64,
            "liveness clock {now} tracks the wall clock {wall}"
        );
    }
}
