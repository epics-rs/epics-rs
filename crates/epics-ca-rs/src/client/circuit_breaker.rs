//! Circuit breaker for repeatedly-failing CA servers.
//!
//! Sits on top of the existing per-search penalty box (`search.rs`).
//! The penalty box reacts to a single TCP connect failure with a
//! 30-second cooldown; the circuit breaker tracks **patterns** of
//! repeated failures and escalates to a longer cooldown so we don't
//! waste cycles trying to reach a flapping server.
//!
//! Three states, classic Hystrix model:
//!
//! ```text
//!                       failures > threshold
//!   ┌──────────┐      ──────────────────────▶  ┌──────────┐
//!   │  CLOSED  │                                │   OPEN   │
//!   │ (normal) │   ◀──────────────────────      │ (cooldown │
//!   └──────────┘     success in HALF_OPEN       │  active) │
//!         ▲                                     └─────┬────┘
//!         │                                           │ cooldown elapsed
//!         │                                           ▼
//!         │                                     ┌──────────┐
//!         └─────── failure ───────────          │ HALF_OPEN│
//!                                               │ (probe)  │
//!                                               └──────────┘
//! ```
//!
//! - CLOSED: normal operation. Failures are counted in a sliding window.
//! - OPEN: cooldown period (default 60s). All traffic to this server is
//!   suppressed; search responses ignored.
//! - HALF_OPEN: a single probe attempt allowed. Success → CLOSED.
//!   Failure → back to OPEN with a longer cooldown.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Per-server failure-pattern tracker and state machine.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    state: BreakerState,
    /// When the current HalfOpen probe started. The probe is normally
    /// resolved by record_success/record_failure, but if the probe
    /// future is dropped without firing either (cancellation, panic
    /// in caller), the breaker would be stuck in HalfOpen forever
    /// (allow() returns false) without this. We treat probes older
    /// than `probe_timeout` as failed and allow a fresh attempt.
    probe_started_at: Option<Instant>,
    /// Recent failure timestamps within the rolling window.
    failures: Vec<Instant>,
    /// When the current OPEN cooldown ends. Only meaningful in OPEN state.
    cooldown_until: Option<Instant>,
    /// How long the current cooldown is, doubled on consecutive trips.
    current_cooldown: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug, Clone, Copy)]
pub struct BreakerConfig {
    /// Rolling window over which failures are counted.
    pub window: Duration,
    /// Threshold of failures within the window that trips the breaker.
    pub failure_threshold: usize,
    /// Initial OPEN cooldown duration. Doubled on each consecutive trip
    /// up to `max_cooldown`.
    pub initial_cooldown: Duration,
    /// Cap on the doubled cooldown.
    pub max_cooldown: Duration,
    /// Maximum time a HalfOpen probe is allowed to run without
    /// firing record_success/record_failure. After this, allow()
    /// treats the probe as failed and admits a fresh attempt — without
    /// it a dropped probe future strands the breaker in HalfOpen.
    pub probe_timeout: Duration,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(60),
            failure_threshold: 5,
            initial_cooldown: Duration::from_secs(60),
            max_cooldown: Duration::from_secs(600),
            probe_timeout: Duration::from_secs(30),
        }
    }
}

impl CircuitBreaker {
    fn new(initial_cooldown: Duration) -> Self {
        Self {
            state: BreakerState::Closed,
            failures: Vec::new(),
            cooldown_until: None,
            current_cooldown: initial_cooldown,
            probe_started_at: None,
        }
    }

    #[allow(dead_code)]
    pub fn state(&self) -> BreakerState {
        self.state
    }
}

/// Per-server registry. Use one instance per `CaClient`.
#[derive(Debug, Default)]
pub struct CircuitBreakerRegistry {
    config: BreakerConfig,
    breakers: HashMap<SocketAddr, CircuitBreaker>,
}

impl CircuitBreakerRegistry {
    pub fn new() -> Self {
        Self::with_config(BreakerConfig::default())
    }

    pub fn with_config(config: BreakerConfig) -> Self {
        Self {
            config,
            breakers: HashMap::new(),
        }
    }

    /// Soft cap on the number of per-address breaker entries. A
    /// long-lived client probing many transient servers (k8s pods,
    /// dev hosts) would otherwise grow `breakers` without bound at
    /// ~80 B/entry. When the map exceeds this, opportunistically
    /// drop entries that are Closed and have no recent failures —
    /// they hold no state worth preserving.
    const MAX_BREAKERS: usize = 4096;

    /// Should we currently allow traffic to this server?
    /// Called before scheduling a search/connect attempt. The HALF_OPEN
    /// state allows exactly **one** probe — once the probe is in flight,
    /// further calls return false until the probe resolves.
    pub fn allow(&mut self, server: SocketAddr) -> bool {
        if self.breakers.len() >= Self::MAX_BREAKERS && !self.breakers.contains_key(&server) {
            self.evict_idle_closed();
        }
        let now = Instant::now();
        let breaker = self
            .breakers
            .entry(server)
            .or_insert_with(|| CircuitBreaker::new(self.config.initial_cooldown));
        match breaker.state {
            BreakerState::Closed => true,
            BreakerState::Open => {
                if let Some(until) = breaker.cooldown_until {
                    if now >= until {
                        // Cooldown elapsed → transition to HALF_OPEN and
                        // permit the probe.
                        breaker.state = BreakerState::HalfOpen;
                        breaker.cooldown_until = None;
                        breaker.probe_started_at = Some(now);
                        true
                    } else {
                        false
                    }
                } else {
                    breaker.state = BreakerState::HalfOpen;
                    breaker.probe_started_at = Some(now);
                    true
                }
            }
            BreakerState::HalfOpen => {
                // Probe already in flight, deny additional traffic until
                // we hear back via record_success / record_failure.
                // Exception: if the probe is older than `probe_timeout`,
                // assume the future was dropped without firing either
                // outcome (caller cancellation, panic) and admit a fresh
                // probe rather than locking the breaker forever.
                if let Some(started) = breaker.probe_started_at
                    && now.duration_since(started) >= self.config.probe_timeout
                {
                    breaker.probe_started_at = Some(now);
                    return true;
                }
                false
            }
        }
    }

    /// Notify that a recent attempt against `server` succeeded.
    pub fn record_success(&mut self, server: SocketAddr) {
        if let Some(breaker) = self.breakers.get_mut(&server) {
            breaker.state = BreakerState::Closed;
            breaker.failures.clear();
            breaker.cooldown_until = None;
            breaker.current_cooldown = self.config.initial_cooldown;
            breaker.probe_started_at = None;
        }
    }

    /// Notify that a recent attempt against `server` failed. May trip
    /// the breaker into OPEN.
    pub fn record_failure(&mut self, server: SocketAddr) {
        // G4: mirror the cap-then-evict in `allow()` so a flapping
        // workload that hits record_failure on many transient
        // addresses (without ever calling allow on those addrs)
        // can't grow the map past MAX_BREAKERS.
        if self.breakers.len() >= Self::MAX_BREAKERS && !self.breakers.contains_key(&server) {
            self.evict_idle_closed();
        }
        let now = Instant::now();
        let breaker = self
            .breakers
            .entry(server)
            .or_insert_with(|| CircuitBreaker::new(self.config.initial_cooldown));

        match breaker.state {
            BreakerState::HalfOpen => {
                // Probe failed → open with double the previous cooldown.
                breaker.current_cooldown =
                    (breaker.current_cooldown * 2).min(self.config.max_cooldown);
                breaker.cooldown_until = Some(now + breaker.current_cooldown);
                breaker.state = BreakerState::Open;
                breaker.failures.clear();
            }
            BreakerState::Open => {
                // Already open — failures while OPEN are external noise.
            }
            BreakerState::Closed => {
                // Drop entries older than the rolling window.
                let cutoff = now - self.config.window;
                breaker.failures.retain(|t| *t >= cutoff);
                breaker.failures.push(now);
                if breaker.failures.len() >= self.config.failure_threshold {
                    breaker.cooldown_until = Some(now + breaker.current_cooldown);
                    breaker.state = BreakerState::Open;
                    breaker.failures.clear();
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn states(&self) -> impl Iterator<Item = (SocketAddr, BreakerState)> + '_ {
        self.breakers.iter().map(|(addr, b)| (*addr, b.state))
    }

    pub fn is_open(&self, server: SocketAddr) -> bool {
        self.breakers
            .get(&server)
            .map(|b| matches!(b.state, BreakerState::Open))
            .unwrap_or(false)
    }

    /// Read-only check: is this server *hard-blocked right now*, i.e.
    /// OPEN with a cooldown that has not yet elapsed?
    ///
    /// Distinct from [`is_open`](Self::is_open): an OPEN breaker whose
    /// cooldown has already elapsed is *probe-ready*, not blocking — it
    /// returns `false` here so a caller gating on this can fall through
    /// to [`allow`](Self::allow), which performs the OPEN→HALF_OPEN
    /// transition and consumes the probe slot. HALF_OPEN and CLOSED
    /// breakers also return `false`. Using `is_open` instead of this for
    /// an early reject would strand probe-ready breakers forever, since
    /// `allow` (the only code that leaves OPEN) would never be reached.
    pub fn is_blocking(&self, server: SocketAddr) -> bool {
        self.breakers
            .get(&server)
            .map(|b| {
                matches!(b.state, BreakerState::Open)
                    && b.cooldown_until
                        .map(|until| Instant::now() < until)
                        .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    /// Drop entries that are Closed AND have no recent failures and
    /// no probe in flight — they hold no information that subsequent
    /// `allow()` calls would need. Called on insert when the map
    /// crosses MAX_BREAKERS to keep growth bounded against
    /// long-running clients that probe many transient servers.
    fn evict_idle_closed(&mut self) {
        self.breakers.retain(|_, b| {
            !(matches!(b.state, BreakerState::Closed)
                && b.failures.is_empty()
                && b.probe_started_at.is_none())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast_config() -> BreakerConfig {
        BreakerConfig {
            window: Duration::from_secs(1),
            failure_threshold: 3,
            initial_cooldown: Duration::from_millis(50),
            max_cooldown: Duration::from_millis(400),
            probe_timeout: Duration::from_millis(500),
        }
    }

    fn addr() -> SocketAddr {
        "127.0.0.1:5064".parse().unwrap()
    }

    #[test]
    fn closed_allows_traffic_by_default() {
        let mut reg = CircuitBreakerRegistry::with_config(fast_config());
        assert!(reg.allow(addr()));
    }

    #[test]
    fn trips_after_threshold_failures() {
        let mut reg = CircuitBreakerRegistry::with_config(fast_config());
        for _ in 0..3 {
            reg.record_failure(addr());
        }
        assert!(!reg.allow(addr()));
        assert!(reg.is_open(addr()));
    }

    #[test]
    fn half_open_after_cooldown() {
        let mut reg = CircuitBreakerRegistry::with_config(fast_config());
        for _ in 0..3 {
            reg.record_failure(addr());
        }
        std::thread::sleep(Duration::from_millis(60));
        assert!(reg.allow(addr())); // half-open probe permitted
        assert!(!reg.allow(addr())); // second call denied (probe in flight)
    }

    #[test]
    fn success_in_half_open_returns_to_closed() {
        let mut reg = CircuitBreakerRegistry::with_config(fast_config());
        for _ in 0..3 {
            reg.record_failure(addr());
        }
        std::thread::sleep(Duration::from_millis(60));
        let _ = reg.allow(addr());
        reg.record_success(addr());
        assert!(reg.allow(addr()));
    }

    #[test]
    fn failure_in_half_open_doubles_cooldown() {
        let mut reg = CircuitBreakerRegistry::with_config(fast_config());
        for _ in 0..3 {
            reg.record_failure(addr());
        }
        std::thread::sleep(Duration::from_millis(60));
        let _ = reg.allow(addr()); // probe permitted
        reg.record_failure(addr()); // probe failed
        assert!(!reg.allow(addr()));
        std::thread::sleep(Duration::from_millis(60));
        // Doubled cooldown is now 100ms; original 50ms wouldn't have helped.
        assert!(!reg.allow(addr()));
    }

    #[test]
    fn is_blocking_distinguishes_probe_ready_from_hard_blocked() {
        let mut reg = CircuitBreakerRegistry::with_config(fast_config());
        for _ in 0..3 {
            reg.record_failure(addr());
        }
        // Just tripped OPEN: hard-blocked, cooldown not elapsed.
        assert!(reg.is_open(addr()));
        assert!(reg.is_blocking(addr()));
        std::thread::sleep(Duration::from_millis(60));
        // Cooldown elapsed: still OPEN, but now probe-ready, not blocking.
        assert!(reg.is_open(addr()));
        assert!(!reg.is_blocking(addr()));
    }

    #[test]
    fn open_breaker_recovers_via_is_blocking_then_allow() {
        // Regression: the search.rs reply path gates on `is_blocking()`
        // then calls `allow()`. A probe-ready OPEN breaker must pass the
        // gate so `allow()` can perform the OPEN→HALF_OPEN transition;
        // otherwise the breaker is stranded OPEN forever.
        let mut reg = CircuitBreakerRegistry::with_config(fast_config());
        for _ in 0..3 {
            reg.record_failure(addr());
        }
        std::thread::sleep(Duration::from_millis(60));
        assert!(
            !reg.is_blocking(addr()),
            "probe-ready breaker must not block"
        );
        assert!(reg.allow(addr()), "allow() must admit the probe");
        reg.record_success(addr());
        assert!(reg.allow(addr()), "breaker recovered to CLOSED");
        assert!(!reg.is_open(addr()));
    }

    #[test]
    fn stale_half_open_probe_self_heals_after_probe_timeout() {
        // If a HALF_OPEN probe is admitted but neither record_success
        // nor record_failure ever fires (the probe future was dropped —
        // caller cancellation/panic), `allow()` must treat a probe
        // older than `probe_timeout` as failed and admit a fresh one,
        // rather than stranding the breaker in HALF_OPEN forever.
        let mut reg = CircuitBreakerRegistry::with_config(fast_config());
        for _ in 0..3 {
            reg.record_failure(addr());
        }
        std::thread::sleep(Duration::from_millis(60)); // cooldown elapses
        assert!(reg.allow(addr()), "first probe admitted (now HALF_OPEN)");
        assert!(
            !reg.allow(addr()),
            "probe in flight — further traffic denied"
        );
        // Do NOT resolve the probe; let it age past probe_timeout (500ms).
        std::thread::sleep(Duration::from_millis(550));
        assert!(
            reg.allow(addr()),
            "stale probe treated as failed — a fresh probe is admitted"
        );
    }

    #[test]
    fn old_failures_drop_out_of_window() {
        let mut reg = CircuitBreakerRegistry::with_config(BreakerConfig {
            window: Duration::from_millis(100),
            failure_threshold: 3,
            ..fast_config()
        });
        reg.record_failure(addr());
        reg.record_failure(addr());
        std::thread::sleep(Duration::from_millis(150));
        reg.record_failure(addr()); // first two are stale, this alone shouldn't trip
        assert!(reg.allow(addr()));
    }
}
