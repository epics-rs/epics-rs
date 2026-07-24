//! Beacon anomaly generation.
//!
//! Corresponds to C++ ca-gateway's `gateServer::generateBeaconAnomaly`
//! (gateServer.cc:422-432). When the gateway adds a new PV to its cache
//! (because a downstream client searched for it for the first time), it
//! triggers a beacon anomaly so that other downstream clients re-search
//! and discover the gateway as a server for that name.
//!
//! Real beacon emission is handled by the underlying [`epics_ca_rs`]
//! `CaServer` beacon emitter (lives in `epics-ca-rs::server::beacon`).
//! This module just throttles "beacon anomaly" requests according to
//! the C++ `GATE_RECONNECT_INHIBIT` (5 minutes) so we don't spam.

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// Default reconnect inhibit window — matches C++ `GATE_RECONNECT_INHIBIT`.
const DEFAULT_INHIBIT: Duration = Duration::from_secs(60 * 5);

/// Beacon anomaly trigger throttle.
pub struct BeaconAnomaly {
    inhibit: Duration,
    last: Mutex<Option<Instant>>,
    /// Optional handle into the underlying `CaServer`'s beacon
    /// emitter — when set, [`Self::request`] pulses it on every
    /// honored request so an immediate beacon actually goes on the
    /// wire (mirrors C++ ca-gateway's
    /// `gateServer::generateBeaconAnomaly`). Without this, the
    /// throttle just tracked timestamps and the new-PV announce was
    /// silent — downstream clients had to wait the full periodic
    /// beacon interval before re-searching.
    pulse: Mutex<Option<Arc<Notify>>>,
}

impl BeaconAnomaly {
    /// Create with the default 5-minute inhibit window.
    pub fn new() -> Self {
        Self::with_inhibit(DEFAULT_INHIBIT)
    }

    /// Create with a custom inhibit window.
    pub fn with_inhibit(inhibit: Duration) -> Self {
        Self {
            inhibit,
            last: Mutex::new(None),
            pulse: Mutex::new(None),
        }
    }

    /// Wire up the underlying `CaServer`'s beacon-reset handle
    /// (`CaServer::beacon_anomaly_handle()`). Once set, every honored
    /// `request()` pulses it so the beacon emitter sends a beacon
    /// immediately. Safe to call after the server has started.
    pub fn install_pulse(&self, pulse: Arc<Notify>) {
        *self.pulse.lock().unwrap() = Some(pulse);
    }

    /// Request a beacon anomaly. Returns true if the request was honored
    /// (not within the inhibit window). When a pulse handle has been
    /// installed via [`Self::install_pulse`], an honored request also
    /// fires the pulse so a beacon goes out immediately.
    pub fn request(&self) -> bool {
        let now = Instant::now();
        let mut last = self.last.lock().unwrap();
        match *last {
            Some(t) if now.duration_since(t) < self.inhibit => false,
            _ => {
                *last = Some(now);
                drop(last);
                if let Some(notify) = self.pulse.lock().unwrap().as_ref() {
                    notify.notify_one();
                }
                true
            }
        }
    }

    /// Manually reset the inhibit timer.
    pub fn reset(&self) {
        *self.last.lock().unwrap() = None;
    }

    /// Time since the last honored request.
    pub fn elapsed(&self) -> Option<Duration> {
        self.last.lock().unwrap().map(|t| t.elapsed())
    }
}

impl Default for BeaconAnomaly {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_request_honored() {
        let b = BeaconAnomaly::with_inhibit(Duration::from_secs(60));
        assert!(b.request());
    }

    #[test]
    fn second_immediate_request_inhibited() {
        let b = BeaconAnomaly::with_inhibit(Duration::from_secs(60));
        assert!(b.request());
        assert!(!b.request());
    }

    #[test]
    fn request_after_inhibit_expires() {
        let b = BeaconAnomaly::with_inhibit(Duration::from_millis(10));
        assert!(b.request());
        std::thread::sleep(Duration::from_millis(20));
        assert!(b.request());
    }

    #[test]
    fn reset_clears_inhibit() {
        let b = BeaconAnomaly::with_inhibit(Duration::from_secs(60));
        assert!(b.request());
        assert!(!b.request());
        b.reset();
        assert!(b.request());
    }

    /// Honored requests must pulse the installed Notify so the
    /// downstream CaServer's beacon emitter actually fires a beacon.
    /// Without this, `generateBeaconAnomaly` was silent on the wire
    /// (review finding #2).
    #[tokio::test(flavor = "current_thread")]
    async fn install_pulse_fires_on_honored_request() {
        let b = BeaconAnomaly::with_inhibit(Duration::from_millis(10));
        let pulse = Arc::new(Notify::new());
        b.install_pulse(pulse.clone());

        // Honored request → notify_one → recv resolves immediately.
        assert!(b.request());
        let woken = tokio::time::timeout(Duration::from_millis(100), pulse.notified())
            .await
            .is_ok();
        assert!(woken, "honored request must pulse the installed Notify");
    }

    /// Inhibited (rate-limited) requests must NOT pulse the Notify —
    /// otherwise an upstream that flaps within the inhibit window
    /// would still drive the beacon emitter on every flap.
    #[tokio::test(flavor = "current_thread")]
    async fn install_pulse_skips_on_inhibited_request() {
        let b = BeaconAnomaly::with_inhibit(Duration::from_secs(60));
        let pulse = Arc::new(Notify::new());
        b.install_pulse(pulse.clone());

        // First request honored.
        assert!(b.request());
        // Drain the first pulse so the next one is a fresh signal.
        let _ = tokio::time::timeout(Duration::from_millis(50), pulse.notified()).await;

        // Second request within inhibit window → not honored, no pulse.
        assert!(!b.request());
        let woken = tokio::time::timeout(Duration::from_millis(100), pulse.notified())
            .await
            .is_ok();
        assert!(!woken, "inhibited request must NOT pulse the Notify");
    }
}
