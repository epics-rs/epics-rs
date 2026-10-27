use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::services::PortServices;

/// C `pasynBase->autoConnectTimeout` (asynManager.c:110), initialised to
/// `DEFAULT_AUTOCONNECT_TIMEOUT` = 0.5 s (:49, :509) and rewritten by
/// `asynSetAutoConnectTimeout` (:2375-2391).
///
/// Process-global on purpose: C has exactly one `pasynBase`. Held as the raw
/// `double`, sign included, because its two readers disagree on a negative
/// value: the registration wait (:2137) does not wait, the connect poll
/// (drvAsynIPPort.c:546-555) waits forever.
static AUTO_CONNECT_TIMEOUT_SECS: AtomicU64 = AtomicU64::new(0.5f64.to_bits());

fn auto_connect_timeout_secs() -> f64 {
    f64::from_bits(AUTO_CONNECT_TIMEOUT_SECS.load(Ordering::Relaxed))
}

/// The registration connect-wait every new port gets — C's
/// `waitConnect(..., pasynBase->autoConnectTimeout)` at :2137, whose
/// `epicsEventWaitWithTimeout` does not wait on a negative or NaN value.
pub fn auto_connect_timeout() -> Duration {
    let secs = auto_connect_timeout_secs();
    if secs.is_finite() && secs > 0.0 {
        Duration::from_secs_f64(secs)
    } else {
        Duration::ZERO
    }
}

/// The bound on an IP port's TCP connect — C `connectIt`'s `connectTimeout`
/// handed to `poll` (drvAsynIPPort.c:546-555), read at every connect. Not
/// rounded: `poll_ready` owns the whole-millisecond interval, and polls a
/// sub-millisecond value 1 ms where C's `(int)(1000 * connectTimeout)` polls
/// 0. A negative value is `poll`'s wait-forever, the longest wait an `int` of
/// milliseconds spells.
pub fn connect_poll_timeout() -> Duration {
    let longest = Duration::from_millis(i32::MAX as u64);
    let secs = auto_connect_timeout_secs();
    if ((1000.0 * secs) as i32) < 0 {
        longest
    } else if secs > 0.0 {
        Duration::try_from_secs_f64(secs).map_or(longest, |t| t.min(longest))
    } else {
        Duration::ZERO
    }
}

/// C `pasynManager->setAutoConnectTimeout(double)` — the iocsh
/// `asynSetAutoConnectTimeout`. Stored as given; ports registered after this
/// call and every later IP connect read the new value, exactly as in C.
pub fn set_auto_connect_timeout(secs: f64) {
    AUTO_CONNECT_TIMEOUT_SECS.store(secs.to_bits(), Ordering::Relaxed);
}

/// Configuration for a port runtime.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Auto-connect on startup.
    pub auto_connect: bool,
    /// Backoff configuration for auto-connect retries.
    pub connect_backoff: BackoffConfig,
    /// Supervision policy.
    pub supervision: SupervisionPolicy,
    /// How long port creation waits for an `auto_connect` port to come up
    /// before returning — C `waitConnect(pport->pconnectUser,
    /// pasynBase->autoConnectTimeout)` at the end of registration
    /// (asynManager.c:2137), with `DEFAULT_AUTOCONNECT_TIMEOUT` = 0.5 s (:49,
    /// :509). It bounds the wait; it does not decide the outcome — a port that
    /// has not connected by then keeps retrying on its own timer, exactly as C's
    /// does, and registration still succeeds.
    pub auto_connect_timeout: Duration,
    /// The trace configuration and exception list the port is created with —
    /// C's `pasynBase`. Defaults to the process-wide
    /// [`PortServices::global()`], so a port built with a default config is
    /// still traceable and still announces exceptions; a
    /// [`crate::manager::PortManager`] carrying its own `TraceManager`
    /// substitutes its own here.
    pub services: PortServices,
    /// C `registerPort`'s `priority`, the band of the port's thread; 0 is
    /// `epicsThreadPriorityMedium` (asynManager.c:2077).
    pub priority: u32,
    /// C `registerPort`'s `stackSize` for the port's thread, as C reads it: 0
    /// is `epicsThreadStackMedium` (asynManager.c:2078-2080), then 1/2 are the
    /// Medium/Big classes and the rest bytes (`StackSpec::from_c`).
    pub stack_size: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            auto_connect: true,
            connect_backoff: BackoffConfig::default(),
            supervision: SupervisionPolicy::default(),
            auto_connect_timeout: auto_connect_timeout(),
            priority: 0,
            stack_size: 0,
            services: PortServices::global(),
        }
    }
}

/// Backoff configuration for retries.
#[derive(Debug, Clone)]
pub struct BackoffConfig {
    pub initial: Duration,
    pub max: Duration,
    pub multiplier: f64,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(100),
            max: Duration::from_secs(30),
            multiplier: 2.0,
        }
    }
}

/// Supervision policy for runtime actors.
#[derive(Debug, Clone)]
pub struct SupervisionPolicy {
    /// Maximum restart attempts before giving up.
    pub max_restarts: usize,
    /// Window in which max_restarts is counted.
    pub restart_window: Duration,
}

impl Default for SupervisionPolicy {
    fn default() -> Self {
        Self {
            max_restarts: 5,
            restart_window: Duration::from_secs(60),
        }
    }
}

#[cfg(test)]
mod auto_connect_timeout_tests {
    use super::*;

    /// One C double, two readers that part on its sign: the registration
    /// wait does not wait on a negative value (asynManager.c:2137), the
    /// connect poll waits forever (drvAsynIPPort.c:547-555).
    #[test]
    fn the_auto_connect_timeout_readers_part_on_the_sign() {
        let longest = Duration::from_millis(i32::MAX as u64);
        for (secs, wait, poll) in [
            (0.5, Duration::from_millis(500), Duration::from_millis(500)),
            (
                0.0004,
                Duration::from_micros(400),
                Duration::from_micros(400),
            ),
            (0.0, Duration::ZERO, Duration::ZERO),
            (f64::NAN, Duration::ZERO, Duration::ZERO),
            (f64::INFINITY, Duration::ZERO, longest),
            // C's `(int)` makes this a 0 ms poll, not a wait-forever.
            (-0.0004, Duration::ZERO, Duration::ZERO),
            (-1.0, Duration::ZERO, longest),
        ] {
            set_auto_connect_timeout(secs);
            assert_eq!(auto_connect_timeout(), wait, "registration wait at {secs}");
            assert_eq!(connect_poll_timeout(), poll, "connect poll at {secs}");
        }
        set_auto_connect_timeout(0.5);
    }
}
