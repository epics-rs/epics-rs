//! Live server telemetry counters, shared by the async server front-end and
//! the `std::net` blocking driver's monitor path.
//!
//! [`ServerStats`] is pure atomics (no runtime dependency), so it lives in its
//! own module rather than the host-only `super::ca_server` orchestrator — the
//! RTEMS blocking driver (`server::blocking`) threads the same counters through
//! the shared `tcp`/`monitor` delivery logic.

/// Lightweight live-connection counters surfaced by `CaServer::stats`
/// and the `casr` iocsh command. Mirrors RSRV's `casr` output at the
/// summary level — total connects / disconnects since startup, plus
/// the running active count derived from their delta.
#[derive(Debug, Default)]
pub struct ServerStats {
    pub connects_total: std::sync::atomic::AtomicU64,
    pub disconnects_total: std::sync::atomic::AtomicU64,
    pub started_at: std::sync::OnceLock<std::time::Instant>,
    /// Total bytes received from clients since startup. Incremented
    /// by `handle_client` on every TCP read. Mirrors the
    /// `caServerBytes_in` counter from PR #592's `dbServerStats`.
    pub bytes_in: std::sync::atomic::AtomicU64,
    /// Total bytes sent to clients since startup. Mirrors
    /// `caServerBytes_out`. Updated when the per-client BufWriter
    /// reports successful flushes; CA over TLS counts post-decrypt
    /// plaintext (the rustls handshake bytes are not surfaced).
    pub bytes_out: std::sync::atomic::AtomicU64,
    /// Total CREATE_CHAN successes across the server lifetime.
    /// PR #592's `caServerChannelCount` minus the closes (which we
    /// track separately so the open-channel count is computable).
    pub channels_opened_total: std::sync::atomic::AtomicU64,
    /// Total CLEAR_CHANNEL successes. Subtract from
    /// `channels_opened_total` for the live channel count.
    pub channels_closed_total: std::sync::atomic::AtomicU64,
    /// Total successful EVENT_ADD setups. Mirrors
    /// `caServerSubscriptionCount`.
    pub subscriptions_opened_total: std::sync::atomic::AtomicU64,
    /// Total successful EVENT_CANCEL / channel-close subscription
    /// teardowns. Subtract from opened for the live subscription
    /// count.
    pub subscriptions_closed_total: std::sync::atomic::AtomicU64,
    /// Cumulative monitor events posted to client subscriptions since
    /// startup — counted once per subscription update the server
    /// dequeues for delivery (the initial value post plus every later
    /// monitor event). The PCAS `caServer::subscriptionEventsPosted()`
    /// counter; the CA gateway derives `serverPostRate` from its delta
    /// (ca-gateway `gateServer.cc:2147-2148`). RSRV has no equivalent —
    /// this is a portable-CA-server (gateway) concept.
    pub subscription_events_posted: std::sync::atomic::AtomicU64,
    /// Cumulative monitor events processed (successfully written to a
    /// client) since startup. Trails `subscription_events_posted` when a
    /// dequeued event is suppressed before the wire (read access denied)
    /// or the client write fails mid-delivery. The PCAS
    /// `caServer::subscriptionEventsProcessed()` counter; the CA gateway
    /// derives `serverEventRate` from its delta (same C site).
    pub subscription_events_processed: std::sync::atomic::AtomicU64,
}

impl ServerStats {
    pub fn active_clients(&self) -> u64 {
        let c = self
            .connects_total
            .load(std::sync::atomic::Ordering::Relaxed);
        let d = self
            .disconnects_total
            .load(std::sync::atomic::Ordering::Relaxed);
        c.saturating_sub(d)
    }

    /// Number of channels currently open across all clients.
    /// Mirrors PR #592's `caServerChannelCount`.
    pub fn active_channels(&self) -> u64 {
        let o = self
            .channels_opened_total
            .load(std::sync::atomic::Ordering::Relaxed);
        let c = self
            .channels_closed_total
            .load(std::sync::atomic::Ordering::Relaxed);
        o.saturating_sub(c)
    }

    /// Number of subscriptions currently active across all clients.
    /// Mirrors PR #592's `caServerSubscriptionCount`.
    pub fn active_subscriptions(&self) -> u64 {
        let o = self
            .subscriptions_opened_total
            .load(std::sync::atomic::Ordering::Relaxed);
        let c = self
            .subscriptions_closed_total
            .load(std::sync::atomic::Ordering::Relaxed);
        o.saturating_sub(c)
    }

    pub fn uptime(&self) -> std::time::Duration {
        self.started_at
            .get()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    }
}
