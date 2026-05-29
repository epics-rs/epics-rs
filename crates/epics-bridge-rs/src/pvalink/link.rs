//! `PvaLink` — a single live PVA link bound to a remote PV.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pv_request::PvRequestExpr;
use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};

use super::config::{LinkDirection, ProcMode, PvaLinkConfig};

#[derive(Debug, thiserror::Error)]
pub enum PvaLinkError {
    #[error("PVA error: {0}")]
    Pva(#[from] epics_pva_rs::error::PvaError),
    #[error("link is INP-only, write requested")]
    NotWritable,
    #[error("link is OUT-only, read requested")]
    NotReadable,
    #[error("pvalink monitor for {0:?} is disconnected (cached value is stale)")]
    Disconnected(String),
    #[error("field {0:?} not found in remote NT structure")]
    FieldNotFound(String),
    #[error("field {0:?} is not a scalar")]
    NotScalar(String),
    #[error("link config parse error: {0}")]
    Config(#[from] super::config::PvaLinkParseError),
    #[error("local-only link {0:?} has no matching local record")]
    NotLocal(String),
}

pub type PvaLinkResult<T> = Result<T, PvaLinkError>;

/// A lifecycle-aware event delivered to the scan-on-update forwarder
/// for an INP+monitor link.
///
/// pvxs `pvaLinkChannel::run()` scans every CP / eligible-CPP target
/// after BOTH the monitor value-update branch AND the
/// `catch(client::Disconnect&)` branch — the same
/// `atomic_records` / `nonatomic_records` scan loop runs with no
/// value comparison (`pvxs/ioc/pvalink_channel.cpp:359-373` sets
/// `connected=false` + `onDisconnect()`, then `:420-432` scans). A
/// value-only channel can only fire the forwarder on `Value`, so a
/// disconnect with no trailing value was silently missed and the CP
/// record never observed LINK_ALARM/INVALID. `Disconnected` carries
/// that lifecycle transition so the forwarder runs the scan path
/// even when no new `PvField` arrives.
///
/// Reconnect needs no separate variant: the re-subscribe loop's first
/// post-reconnect monitor callback delivers a `Value`, which already
/// drives the scan (matching pvxs's reconnect/`onTypeChange` scan at
/// `:342-352`).
#[derive(Debug, Clone)]
pub enum ScanEvent {
    /// A fresh monitor value arrived; drives per-field change detection.
    Value(PvField),
    /// The live monitor subscription ended (IOC restart / transient
    /// I/O). Carries no value; the forwarder scans CP/CPP targets
    /// unconditionally so the owning record processes the disconnect.
    Disconnected,
}

/// A live PVA link.
///
/// Constructed once per record-link instance. For INP links the optional
/// monitor task spawns automatically; for OUT links the link just owns the
/// PvaClient and writes synchronously.
pub struct PvaLink {
    /// Field declaration order matters: Rust drops fields in
    /// declaration order, so `_monitor_abort` MUST come BEFORE
    /// `client`. The spawned monitor task holds its own clone of the
    /// PvaClient; if the parent client field drops first and that
    /// shutdown is cancellation-token-driven (not refcount-only),
    /// the still-running task hits I/O errors before the abort
    /// lands. Order: abort first → task stops → client drops cleanly.
    _monitor_abort: Option<MonitorAbort>,
    config: PvaLinkConfig,
    client: PvaClient,
    /// Latest received value (INP only — None until first event).
    latest: Arc<Mutex<Option<PvField>>>,
    /// Live-connection flag for INP+monitor links (B-pvalink-restart).
    ///
    /// The INP monitor task is a re-subscribe loop: it sets this
    /// `true` while a subscription is live and `false` the moment
    /// `pvmonitor` returns (IOC restart / transient I/O), then
    /// re-subscribes with exponential backoff. `is_connected()`
    /// reads this flag so a downstream IOC restart is reflected as
    /// a disconnect instead of serving the stale cached value
    /// forever. `None` for OUT / non-monitor links.
    monitor_connected: Option<Arc<AtomicBool>>,
    /// Receiver half of the INP-monitor record-notification channel.
    ///
    /// B3: every monitor event for an INP+monitor link pushes the new
    /// [`PvField`] onto this channel (the sender lives inside the
    /// spawned monitor task). [`Self::take_notify_rx`] hands the
    /// receiver to the resolver, which forwards events into
    /// `scan_on_update` / CP processing of the owning record. Wrapped
    /// in a `Mutex<Option<..>>` because the receiver is single-consumer
    /// and is moved out exactly once.
    ///
    /// Carries [`ScanEvent`], not a bare `PvField`: the monitor task
    /// also pushes a `Disconnected` lifecycle event when a live
    /// subscription ends, so the forwarder can scan CP/CPP targets on
    /// the disconnect even though no value accompanies it.
    notify_rx: Mutex<Option<mpsc::Receiver<ScanEvent>>>,
    /// Single pending OUT Put (OUT links, B4 `defer` / `retry`).
    ///
    /// `defer=true` causes `write` to store the value here instead of
    /// issuing the Put immediately; `flush_deferred` sends it.
    /// `retry=true` causes a Put that fails because the upstream is
    /// unreachable to be stored and replayed by `flush_deferred` on
    /// reconnect.
    ///
    /// One slot, NOT a FIFO queue: pvxs keeps a single `put_scratch`
    /// per link and a later write to the same link overwrites it before
    /// the channel-level PUT starts, so retry replays only "the most
    /// recent incomplete PUT" and defer caches one current value
    /// (`pvxs/ioc/pvalink_lset.cpp:643-653`, `documentation/pvalink.rst:99-113`).
    /// A FIFO log would replay stale intermediate values after a newer
    /// value superseded them. (Combining sibling-field writes from
    /// different links into one channel-level PUT is a separate concern
    /// owned by the shared OUT channel, tracked under the OUT-coalescing
    /// work.)
    ///
    /// Stores [`QueuedPut`], not a bare `PvField`: a string-form
    /// `write` must replay through the string `pvput` path (which
    /// coerces the text against the channel's introspected type),
    /// not as a `PvField::Scalar(String)` — replaying a String to a
    /// numeric record was a type mismatch.
    pending_put: Mutex<Option<QueuedPut>>,
}

/// A queued OUT-link Put, preserving the caller's original value
/// form so the deferred replay uses the same type-correct path the
/// immediate Put would have used.
#[derive(Debug, Clone)]
enum QueuedPut {
    /// From [`PvaLink::write`] — replayed via the string `pvput`
    /// path so the text is coerced against the channel's native
    /// scalar type, not forced to a `String` field.
    Str(String),
    /// From [`PvaLink::write_pv_field`] — replayed verbatim via the
    /// typed `pvput_pv_field` path.
    Field(PvField),
}

struct MonitorAbort(tokio::task::AbortHandle);

impl Drop for MonitorAbort {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl PvaLink {
    /// Open a link against the configured PV.
    ///
    /// For INP+monitor links, this also spawns a background monitor task.
    pub async fn open(config: PvaLinkConfig) -> PvaLinkResult<Self> {
        let client = PvaClient::builder().timeout(Duration::from_secs(5)).build();

        let latest = Arc::new(Mutex::new(None));
        let mut notify_rx = None;
        let mut monitor_abort = None;
        let mut monitor_connected = None;

        if matches!(config.direction, LinkDirection::Inp) && config.monitor {
            // B3 / B4-Q: the channel buffer is sized to the link's
            // `Q` (monitor queue depth) so a slow record-side
            // consumer back-pressures rather than silently dropping
            // events. `try_send` below still tolerates a full
            // channel (the `latest` cache is authoritative for the
            // value itself; the channel only drives scan-on-update).
            let (tx, rx) = mpsc::channel::<ScanEvent>(config.queue_size.max(1));
            notify_rx = Some(rx);

            let pv_name = config.pv_name.clone();
            let latest_clone = latest.clone();
            let client_clone = client.clone();
            let connected = Arc::new(AtomicBool::new(false));
            let connected_for_task = connected.clone();
            monitor_connected = Some(connected);
            // B4-pipeline / B4-Q: when the link asks for pipeline
            // flow-control or a non-default queue depth, build a
            // pvRequest carrying `record[pipeline=...,queueSize=N]`
            // so the negotiation reaches the server. Otherwise use
            // the plain monitor (lower overhead, matches prior
            // behaviour).
            let request = monitor_request(&config);
            // B-pvalink-restart: the INP monitor is a re-subscribe
            // loop, mirroring `channel_cache.rs::spawn_upstream_monitor`.
            // `pvmonitor` blocks until the subscription ends (IOC
            // restart / transient I/O); when it returns we mark the
            // link disconnected and re-subscribe after an exponential
            // backoff. Pre-fix this ran `pvmonitor` exactly once, so
            // a single IOC restart froze the cached value forever and
            // `is_connected()` stayed true.
            let join = tokio::spawn(async move {
                let mut backoff = Duration::from_millis(250);
                let max_backoff = Duration::from_secs(30);
                loop {
                    let tx_inner = tx.clone();
                    let latest_inner = latest_clone.clone();
                    let connected_inner = connected_for_task.clone();
                    // Liveness is proven by a delivered event, not by
                    // entering `pvmonitor` (which may fail immediately
                    // against a down IOC). The callback flips the flag
                    // `true` on the first event of each subscription;
                    // it is reset `false` when `pvmonitor` returns.
                    let on_event = move |value: &PvField| {
                        connected_inner.store(true, Ordering::Release);
                        *latest_inner.lock() = Some(value.clone());
                        let _ = tx_inner.try_send(ScanEvent::Value(value.clone()));
                    };
                    let result = match &request {
                        Some(req) => {
                            client_clone
                                .pvmonitor_with_request(&pv_name, req, on_event)
                                .await
                        }
                        None => client_clone.pvmonitor(&pv_name, on_event).await,
                    };
                    // Subscription ended — reflect the disconnect so
                    // `is_connected()` goes false until re-subscribed.
                    // `swap` reports whether the subscription had been
                    // live (any event delivered): pvxs only runs the
                    // disconnect scan path for a channel that was
                    // connected (`pvalink_channel.cpp:342` gates the
                    // `Disconnect` handling on the prior `connected`
                    // state), so a never-connected subscription failure
                    // must not synthesize a spurious disconnect scan.
                    let was_connected = connected_for_task.swap(false, Ordering::AcqRel);
                    if was_connected {
                        // Notify the scan forwarder of the lifecycle
                        // transition so CP / passive-CPP targets process
                        // and expose LINK_ALARM/INVALID even though no
                        // value follows the disconnect. pvxs scans the
                        // atomic/non-atomic target lists after the
                        // `catch(client::Disconnect&)` branch
                        // (`pvalink_channel.cpp:359-373` + `:420-432`).
                        // `try_send` matches the value path: the channel
                        // is drained promptly and a single disconnect
                        // event sits alone, so a drop here is only
                        // possible under an already-saturated backlog.
                        let _ = tx.try_send(ScanEvent::Disconnected);
                    }
                    match &result {
                        Ok(()) => tracing::debug!(
                            pv = %pv_name,
                            "pvalink: INP monitor ended, re-subscribing"
                        ),
                        Err(e) => tracing::warn!(
                            pv = %pv_name,
                            error = %e,
                            backoff_ms = backoff.as_millis() as u64,
                            "pvalink: INP monitor failed, will retry"
                        ),
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, max_backoff);
                }
            });
            monitor_abort = Some(MonitorAbort(join.abort_handle()));
        }

        Ok(Self {
            config,
            client,
            latest,
            monitor_connected,
            notify_rx: Mutex::new(notify_rx),
            pending_put: Mutex::new(None),
            _monitor_abort: monitor_abort,
        })
    }

    /// Take the INP-monitor notification receiver (B3). Returns the
    /// channel exactly once; subsequent calls return `None`. The
    /// resolver calls this right after `open` to spawn the
    /// scan-on-update forwarder. `None` for OUT / non-monitor links
    /// (they never created a channel) or after the receiver has
    /// already been claimed.
    pub fn take_notify_rx(&self) -> Option<mpsc::Receiver<ScanEvent>> {
        self.notify_rx.lock().take()
    }

    pub fn config(&self) -> &PvaLinkConfig {
        &self.config
    }

    /// Read the current value of the linked field.
    ///
    /// In monitor mode this returns the cached latest value; otherwise it
    /// triggers a fresh GET.
    pub async fn read(&self) -> PvaLinkResult<PvField> {
        self.read_with_field(&self.config.field.clone()).await
    }

    /// Like [`Self::read`] but selects `field` instead of
    /// `self.config.field`. Lets the resolver pass a per-link field
    /// selector when multiple DB links share a cached upstream channel
    /// but differ in which sub-field they target (pvxs
    /// `pvalink_link.cpp:91` — `root = lchan->root[fieldName]`).
    pub async fn read_with_field(&self, field: &str) -> PvaLinkResult<PvField> {
        if matches!(self.config.direction, LinkDirection::Out) {
            return Err(PvaLinkError::NotReadable);
        }
        // a monitor link serves its cached value ONLY
        // while the subscription is live. When the upstream monitor is
        // down — an IOC restart / transient I/O, or before the first
        // event has ever arrived — the cache is not a valid current
        // read; surface a failed read so the lset's `get_value`
        // returns `None` and base processing raises LINK_ALARM/INVALID,
        // instead of handing back a stale value as if it were fresh.
        // The `latest` slot is still retained for the diagnostic /
        // timestamp accessors (`latest_value`, `time_stamp`). Mirrors
        // pvxs `pvaGetValue`'s `!self->valid()` gate
        // (pvalink_lset.cpp:259-272), which returns failure while
        // disconnected and does NOT fall back to a one-shot GET.
        // Non-monitor INP links keep the fresh-GET path: each read
        // proves connectivity by itself, so there is no stale window.
        if self.config.monitor {
            if !self.is_connected() {
                return Err(PvaLinkError::Disconnected(self.config.pv_name.clone()));
            }
            return match self.latest.lock().clone() {
                Some(v) => Ok(select_link_value(&v, field)),
                // Connected flag set but the first event has not yet
                // written `latest` (the callback stores the flag before
                // the value): not-yet-valid, surface a failed read.
                None => Err(PvaLinkError::Disconnected(self.config.pv_name.clone())),
            };
        }
        let result = self.client.pvget_full(&self.config.pv_name).await?;
        Ok(select_link_value(&result.value, field))
    }

    /// Synchronous fast-path read: return the cached field if the
    /// monitor has delivered at least one event, without ever
    /// awaiting. Returns `None` for OUT links, non-monitor INPs,
    /// or pre-first-event INPs.
    ///
    /// Lets the record-link resolver path skip `block_on` on every
    /// process — the typical hot case where a monitor has already
    /// populated the cache. Mirrors pvxs `pvalink_lset.cpp::pvaLoadValue`
    /// (sync read of cached `current` slot).
    pub fn try_read_cached(&self) -> Option<PvField> {
        self.try_read_cached_with_field(&self.config.field.clone())
    }

    /// Like [`Self::try_read_cached`] but selects `field` instead of
    /// `self.config.field`. Mirrors the per-link field override path
    /// (`pvxs pvalink_link.cpp:91`).
    pub fn try_read_cached_with_field(&self, field: &str) -> Option<PvField> {
        if matches!(self.config.direction, LinkDirection::Out) || !self.config.monitor {
            return None;
        }
        // same disconnect gate as `read_with_field` — the
        // sync fast path must never serve a stale cached value while the
        // monitor subscription is down. Returning `None` makes the lset
        // `get_value` fall through to the (also-gated) slow path and
        // ultimately report no value, so base raises LINK_ALARM/INVALID.
        if !self.is_connected() {
            return None;
        }
        let v = self.latest.lock().clone()?;
        Some(select_link_value(&v, field))
    }

    /// Convenience: read the value as f64.
    pub async fn read_scalar_f64(&self) -> PvaLinkResult<f64> {
        let pv = self.read().await?;
        scalar_as_f64(&pv).ok_or_else(|| PvaLinkError::NotScalar(self.config.field.clone()))
    }

    /// Write a value to the linked PV (OUT direction only).
    ///
    /// B4: honors the link's `defer` / `retry` options. With
    /// `defer=true` the value is queued and the Put is only issued by
    /// [`Self::flush_deferred`]. With `retry=true` a Put that fails
    /// because the upstream is unreachable is queued for replay
    /// instead of surfacing an error. Mirrors pvxs `pvaPutValue`
    /// (pvalink_lset.cpp:647 `if(!self->defer) lchan->put()`).
    ///
    /// delegates to [`Self::write_with_block`] with `block=false`.
    pub async fn write(&self, value_str: &str) -> PvaLinkResult<()> {
        self.write_with_block(value_str, false).await
    }

    /// Like [`Self::write`] but passes `block` through to the PUT
    /// pvRequest (`record._options.block`). Mirrors pvxs
    /// `pvaPutValueX(wait)` → `block = !after_put.empty()`
    /// (`pvalink_lset.cpp:647`, `pvalink_channel.cpp:223`).
    pub async fn write_with_block(&self, value_str: &str, block: bool) -> PvaLinkResult<()> {
        if matches!(self.config.direction, LinkDirection::Inp) {
            return Err(PvaLinkError::NotWritable);
        }
        // Keep the value in string form on the defer / retry queue so
        // the replay goes through the string `pvput` path — which
        // coerces the text against the channel's introspected scalar
        // type. Storing `PvField::Scalar(String)` instead would
        // replay a String field to a possibly-numeric record.
        if self.config.defer {
            self.set_pending(QueuedPut::Str(value_str.to_string()));
            return Ok(());
        }
        let req = build_put_request(self.config.proc, block);
        let result = if is_subfield(&self.config.field) {
            self.client
                .pvput_field_with_request(&self.config.pv_name, &self.config.field, &req, value_str)
                .await
        } else {
            self.client
                .pvput_with_request(&self.config.pv_name, &req, value_str)
                .await
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) if self.config.retry && is_disconnect(&e) => {
                self.set_pending(QueuedPut::Str(value_str.to_string()));
                Ok(())
            }
            Err(e) => Err(PvaLinkError::Pva(e)),
        }
    }

    /// Write a typed `PvField` directly (no string round-trip). For
    /// large arrays this avoids the O(N) `Display` allocation +
    /// O(N) pvput parse-back that `write(value_str)` triggers.
    /// Used by the pvalink OUT path on EpicsValue array variants.
    ///
    /// B4: same `defer` / `retry` semantics as [`Self::write`].
    ///
    /// delegates to [`Self::write_pv_field_with_block`] with
    /// `block=false`.
    pub async fn write_pv_field(&self, value: &PvField) -> PvaLinkResult<()> {
        self.write_pv_field_with_block(value, false).await
    }

    /// Like [`Self::write_pv_field`] but passes `block` through to the
    /// PUT pvRequest. Mirrors pvxs `pvaPutValueX` for typed values
    /// (`pvalink_channel.cpp:268`).
    pub async fn write_pv_field_with_block(
        &self,
        value: &PvField,
        block: bool,
    ) -> PvaLinkResult<()> {
        if matches!(self.config.direction, LinkDirection::Inp) {
            return Err(PvaLinkError::NotWritable);
        }
        if self.config.defer {
            self.set_pending(QueuedPut::Field(value.clone()));
            return Ok(());
        }
        let req = build_put_request(self.config.proc, block);
        // a typed write to a query-bearing OUT link
        // (`pva://PV?field=someArray`) must target the selected
        // sub-field, not the root `value`. Mirrors pvxs `linkBuildPut`
        // (`pvalink_channel.cpp:138`): `top[fieldName]` when
        // `fieldName` is non-empty.
        let result = if is_subfield(&self.config.field) {
            self.client
                .pvput_pv_field_field_with_request(
                    &self.config.pv_name,
                    &self.config.field,
                    &req,
                    value,
                )
                .await
        } else {
            self.client
                .pvput_pv_field_with_request(&self.config.pv_name, &req, value)
                .await
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) if self.config.retry && is_disconnect(&e) => {
                self.set_pending(QueuedPut::Field(value.clone()));
                Ok(())
            }
            Err(e) => Err(PvaLinkError::Pva(e)),
        }
    }

    /// Store `value` as the link's single pending Put (B4), overwriting
    /// any earlier pending value. pvxs keeps one `put_scratch` per link
    /// and a later write overwrites it before the channel PUT starts
    /// (`pvxs/ioc/pvalink_lset.cpp:643-653`), so defer/retry coalesce to
    /// the most recent value rather than queueing a stale FIFO log.
    fn set_pending(&self, value: QueuedPut) {
        *self.pending_put.lock() = Some(value);
    }

    /// Number of Puts currently pending (0 or 1) on the defer / retry
    /// slot (B4).
    pub fn pending_put_count(&self) -> usize {
        usize::from(self.pending_put.lock().is_some())
    }

    /// Flush the link's single pending Put to the upstream PV (B4).
    ///
    /// Called for `defer` links to issue the cached value, and for
    /// `retry` links once the upstream reconnects. Replays only the
    /// most recent pending value — pvxs keeps one `put_scratch` per
    /// link, so retry replays "the most recent incomplete PUT" rather
    /// than a FIFO log of superseded intermediate values
    /// (`documentation/pvalink.rst:99-113`,
    /// `pvxs/ioc/pvalink_lset.cpp:643-653`). Returns the number of Puts
    /// issued (0 or 1).
    ///
    /// On a disconnect error for a `retry` link the value is restored
    /// to the pending slot so a later flush retries it — but only if a
    /// newer write has not already taken the slot, so the most-recent
    /// value still wins. A non-disconnect (or non-retry) error drops
    /// the value: it would fail identically on every retry.
    pub async fn flush_deferred(&self) -> PvaLinkResult<usize> {
        if matches!(self.config.direction, LinkDirection::Inp) {
            return Err(PvaLinkError::NotWritable);
        }
        let Some(value) = self.pending_put.lock().take() else {
            return Ok(0);
        };
        // Replay through the same pvRequest path the immediate Put would
        // have used (process/block options, field targeting). block=false:
        // the caller did not request a blocking wait at queue time and
        // there is no caller to signal completion to.
        let req = build_put_request(self.config.proc, false);
        let put_result = match &value {
            QueuedPut::Str(s) => {
                if is_subfield(&self.config.field) {
                    self.client
                        .pvput_field_with_request(&self.config.pv_name, &self.config.field, &req, s)
                        .await
                } else {
                    self.client
                        .pvput_with_request(&self.config.pv_name, &req, s)
                        .await
                }
            }
            QueuedPut::Field(f) => {
                // replay typed field writes to the selected sub-field,
                // same targeting as the immediate path.
                if is_subfield(&self.config.field) {
                    self.client
                        .pvput_pv_field_field_with_request(
                            &self.config.pv_name,
                            &self.config.field,
                            &req,
                            f,
                        )
                        .await
                } else {
                    self.client
                        .pvput_pv_field_with_request(&self.config.pv_name, &req, f)
                        .await
                }
            }
        };
        match put_result {
            Ok(()) => Ok(1),
            Err(e) if self.config.retry && is_disconnect(&e) => {
                // Still disconnected — restore the value so a later
                // flush retries it, but only if a newer write has not
                // already replaced it (most-recent-PUT wins).
                let mut slot = self.pending_put.lock();
                if slot.is_none() {
                    *slot = Some(value);
                }
                Err(PvaLinkError::Pva(e))
            }
            Err(e) => Err(PvaLinkError::Pva(e)),
        }
    }

    /// True when the link currently has a live upstream connection.
    /// Mirrors pvxs `pvaIsConnected` (pvalink_lset.cpp:186).
    ///
    /// B-pvalink-restart: for INP+monitor links this reads the
    /// monitor task's live-connection flag — it goes `false` the
    /// moment the upstream subscription ends (IOC restart / transient
    /// I/O) and back `true` once the re-subscribe loop delivers a
    /// fresh event. Pre-fix this returned `latest.is_some()`, which
    /// stayed `true` forever once any value had been cached, so an
    /// IOC restart was never reflected. For non-monitor links (which
    /// never run the monitor task) it falls back to "a value has been
    /// cached".
    pub fn is_connected(&self) -> bool {
        match &self.monitor_connected {
            Some(flag) => flag.load(Ordering::Acquire),
            None => self.latest.lock().is_some(),
        }
    }

    /// Raw remote NT `alarm.severity` of the latest cached value, read
    /// relative to the link's selected field root, in EPICS severity
    /// numbering (`0 = NO_ALARM` … `3 = INVALID`). `None` when no value
    /// is cached or the selected root carries no alarm sub-field.
    ///
    /// `field` selects the metadata root exactly as pvxs does: a
    /// non-empty `fieldName` rebinds `root = lchan->root[fieldName]` and
    /// `fld_severity` is then resolved relative to that selected root
    /// (`pvxs/ioc/pvalink_link.cpp:90-110`,
    /// `pvxs/ioc/pvalink_lset.cpp:412-430`). `field=""` keeps the
    /// top-level root.
    fn remote_alarm_severity(&self, field: &str) -> Option<i32> {
        let v = self.latest.lock().clone()?;
        let PvField::Structure(s) = select_target(&v, field) else {
            return None;
        };
        let PvField::Structure(a) = s.get_field("alarm")? else {
            return None;
        };
        match a.get_field("severity")? {
            PvField::Scalar(sv) => Some(scalar_value_to_f64(sv) as i32),
            _ => None,
        }
    }

    /// true iff this monitor link previously delivered a
    /// value but its subscription is now down — the cached `latest` is
    /// stale. This is the precise "disconnected after connect" state
    /// that must contribute LINK_ALARM/INVALID through the alarm hooks.
    ///
    /// A link that NEVER connected (no cached value) is deliberately
    /// excluded here: the value path (`get_value` → `None` → base
    /// LINK_ALARM/INVALID, gated by the parsed link type) owns that
    /// case. Excluding it also keeps the base bare-name alarm loop
    /// (`external_link_alarm`, which tries every registered lset) from
    /// mis-claiming a name a sibling lset actually owns — a PVA lset
    /// asked about a `ca://` name lazily opens a never-connecting PVA
    /// link, and must report `None` (not INVALID) so the loop falls
    /// through to the CA lset. Mirrors pvxs `pvaLink::valid()` going
    /// false on disconnect while the snapshot is retained
    /// (pvalink_channel.cpp:370-376).
    fn monitor_disconnected_stale(&self) -> bool {
        self.config.monitor && !self.is_connected() && self.latest.lock().is_some()
    }

    /// Severity to fold into the owning record's `LINK_ALARM`, after
    /// applying the link's `MS`/`NMS`/`MSI` maximize-severity mode
    /// (B2). Returns `None` when no alarm should propagate — i.e.
    /// `NMS`, or the remote severity does not meet the mode's
    /// threshold, or no value is cached yet.
    ///
    /// Mirrors pvxs `pvalink_lset.cpp:418` — the `recGblSetSevrMsg`
    /// gate that propagates `snap_severity` into `LINK_ALARM` only
    /// when `(sevr==MS && sev!=NO_ALARM) || (sevr==MSI && sev==INVALID)`.
    pub fn link_alarm_severity(&self) -> Option<i32> {
        self.link_alarm_severity_with(&self.config.field, self.config.sevr)
    }

    /// Like [`Self::link_alarm_severity`] but gates on a
    /// caller-supplied `sevr` mode instead of this link's
    /// `self.config.sevr`.
    ///
    /// the registry can return a cached INP `PvaLink` whose
    /// `config.sevr` belongs to whichever caller opened it first.
    /// Two INP links to the same remote PV with different `sevr`
    /// options would otherwise share one link's `MS`/`NMS`/`MSI`
    /// gate. The resolver passes the caller's own parsed `sevr` so
    /// each link applies its own maximize-severity mode (pvxs
    /// `pvaLinkConfig::sevr` is per-link, `pvxs/ioc/pvalink.h:65`).
    pub fn link_alarm_severity_with(
        &self,
        field: &str,
        sevr: super::config::SevrMode,
    ) -> Option<i32> {
        // a disconnected monitor link is INVALID
        // regardless of the MS/NMS/MSI gate — a broken link is a local
        // failure, not a remote-severity propagation that NMS would
        // suppress. pvxs `pvaGetValue` sets LINK_ALARM/INVALID_ALARM
        // unconditionally while `!valid()` (pvalink_lset.cpp:259-272).
        // Routing INVALID(3) through this existing `alarm_severity`
        // hook means consumers reading the alarm path — not just the
        // value path — also observe the disconnect, via the same
        // single source of truth (`is_connected`).
        if self.monitor_disconnected_stale() {
            return Some(3); // INVALID
        }
        let sev = self.remote_alarm_severity(field)?;
        if sevr.propagates(sev) {
            Some(sev)
        } else {
            None
        }
    }

    /// Best-effort alarm message for the linked PV.
    ///
    /// B2: the message is gated by the link's maximize-severity mode
    /// (`MS`/`NMS`/`MSI`). It returns `Some(..)` only when the remote
    /// severity actually propagates per [`Self::link_alarm_severity`] —
    /// the database consults this hook to decide whether to raise
    /// `LINK_ALARM` on the owning record, so an `NMS` link (the
    /// default) must report no alarm even when the remote PV is in
    /// alarm. Mirrors pvxs `pvaGetAlarmMsg` (pvalink_lset.cpp:536),
    /// which reads the same `snap_*` slots that the `MS`/`MSI` gate
    /// at `pvalink_lset.cpp:418` populates.
    ///
    /// When the remote NT structure has no `alarm.message` string but
    /// the severity does propagate, a synthetic message is returned so
    /// the alarm is still observable.
    pub fn alarm_message(&self) -> Option<String> {
        self.alarm_message_with(&self.config.field, self.config.sevr)
    }

    /// Like [`Self::alarm_message`] but gates on a caller-supplied
    /// `sevr` mode instead of this link's `self.config.sevr`.
    ///
    /// same rationale as [`Self::link_alarm_severity_with`] —
    /// the alarm-message gate must use the caller's per-link `sevr`,
    /// not whichever cached link's mode happens to be shared.
    pub fn alarm_message_with(&self, field: &str, sevr: super::config::SevrMode) -> Option<String> {
        // disconnect dominates — report the link failure,
        // not the stale remote alarm message. Pairs with the INVALID
        // severity `link_alarm_severity_with` returns for the same
        // state, so the owning record's LINK_ALARM carries a disconnect
        // message rather than a misleading snapshot of the last remote
        // alarm string.
        if self.monitor_disconnected_stale() {
            return Some("pvalink monitor disconnected".to_string());
        }
        // Severity gate first — NMS / sub-threshold links report
        // nothing.
        let sev = self.link_alarm_severity_with(field, sevr)?;
        let v = self.latest.lock().clone()?;
        // Resolve the alarm string relative to the link's selected
        // field root, matching `remote_alarm_severity` — pvxs reads
        // `fld_message` from the same selected `root[fieldName]`
        // (`pvxs/ioc/pvalink_link.cpp:90-110`,
        // `pvxs/ioc/pvalink_lset.cpp:412-430`).
        let PvField::Structure(s) = select_target(&v, field) else {
            return None;
        };
        let msg = s.get_field("alarm").and_then(|alarm| {
            let PvField::Structure(a) = alarm else {
                return None;
            };
            match a.get_field("message") {
                Some(PvField::Scalar(ScalarValue::String(m))) if !m.is_empty() => Some(m.clone()),
                _ => None,
            }
        });
        Some(msg.unwrap_or_else(|| format!("remote severity {sev}")))
    }

    /// Latest cached NT value, if any. Returned as the raw [`PvField`]
    /// so callers can pull whichever sub-field they need (alarm,
    /// timeStamp, value, etc.). pvxs `pvaGetTimeStampTag`
    /// (pvalink_lset.cpp:571) lives on top of this.
    pub fn latest_value(&self) -> Option<PvField> {
        self.latest.lock().clone()
    }

    /// Latest `(seconds, nanoseconds, userTag)` from the NT timeStamp
    /// slot, if the cached value carries one. Mirrors pvxs
    /// `pvaGetTimeStampTag`. The `userTag` is the remote
    /// `timeStamp.userTag` widened to the 64-bit tag WITHOUT sign
    /// extension (see the read below); `0` when the field is absent.
    pub fn time_stamp(&self, field: &str) -> Option<(i64, i32, u64)> {
        // a disconnected monitor link must not serve its
        // stale latched timestamp — pvxs gates every lset getter
        // through `CHECK_VALID` (`valid() = connected && root`), so
        // `pvaGetTimeStampTag` returns no-data once the channel drops
        // even though the snapshot is retained. Reporting the frozen
        // timestamp would let the owning record adopt a remote time
        // that no longer reflects live data.
        if self.monitor_disconnected_stale() {
            return None;
        }
        let v = self.latest.lock().clone()?;
        // Resolve `timeStamp` relative to the selected field root —
        // pvxs reads `fld_seconds`/`fld_nanoseconds`/`fld_usertag` from
        // the same `root[fieldName]` selected by `onTypeChange`
        // (`pvxs/ioc/pvalink_link.cpp:90-110`,
        // `pvxs/ioc/pvalink_lset.cpp:399-409`).
        let PvField::Structure(s) = select_target(&v, field) else {
            return None;
        };
        let ts = s.get_field("timeStamp")?;
        let PvField::Structure(t) = ts else {
            return None;
        };
        let secs = match t.get_field("secondsPastEpoch")? {
            PvField::Scalar(ScalarValue::Long(v)) => *v,
            PvField::Scalar(ScalarValue::ULong(v)) => *v as i64,
            _ => return None,
        };
        let nsec = match t.get_field("nanoseconds")? {
            PvField::Scalar(ScalarValue::Int(v)) => *v,
            PvField::Scalar(ScalarValue::UInt(v)) => *v as i32,
            _ => return None,
        };
        // pvxs `pvalink_lset.cpp:406-409` reads `timeStamp.userTag` into
        // the snapshot tag, falling back to 0 when the field is absent
        // (`if(self->fld_usertag) snap_tag = ...; else snap_tag = 0;`).
        // The wire field is a signed int32; zero-extend (`as u32 as u64`)
        // rather than sign-extend so a bit-31 tag like 0x9000_0000 widens
        // to 0x0000_0000_9000_0000, not 0xFFFF_FFFF_9000_0000 (the
        // pvData `as<uint32_t>()` reinterpret, not `as<epicsUTag>()`).
        let utag = match t.get_field("userTag") {
            Some(PvField::Scalar(ScalarValue::Int(v))) => *v as u32 as u64,
            Some(PvField::Scalar(ScalarValue::UInt(v))) => *v as u64,
            _ => 0,
        };
        Some((secs, nsec, utag))
    }

    /// Remote display / control / valueAlarm metadata snapshot for
    /// this link's cached NT value.
    ///
    /// Mirrors the pvxs pvalink lset metadata getters
    /// (`pvxs/ioc/pvalink_lset.cpp:199`–`:534`):
    ///
    /// * `dbf_type` / `element_count` derive from the value at the
    ///   link's field path (`config.field`) — `pvaGetDBFtype` reads
    ///   `fld_value.type()`, `pvaGetElements` reads its array length
    ///   or `1` for a scalar.
    /// * `graphic_limits` / `control_limits` / `alarm_limits` /
    ///   `precision` / `units` / `description` are read from the
    ///   *top-level* NT `display` / `control` / `valueAlarm`
    ///   sub-structures — `pvaGetGraphicLimits` &c. read
    ///   `fld_meta["display.limitLow"]`, etc.
    ///
    /// Returns `None` when no value is cached (link not connected).
    /// Each field is `None` when the remote NT value did not carry
    /// that metadata — the caller then keeps its local default,
    /// exactly as the C getters leave the buffer untouched on a
    /// missing `Value::as`.
    pub fn link_metadata(&self) -> Option<epics_base_rs::server::database::LinkMetadata> {
        self.link_metadata_with(&self.config.field)
    }

    /// Like [`Self::link_metadata`] but derives DBF type and element
    /// count from a caller-supplied `field` path instead of this
    /// link's `self.config.field`.
    ///
    /// the registry can return a cached INP `PvaLink` whose
    /// `config.field` belongs to whichever caller opened it first, so
    /// two INP links to the same remote PV with different `field`
    /// options would otherwise report the same DBF type / element
    /// count. The resolver passes the caller's own parsed `field`
    /// (pvxs `pvaGetDBFtype` reads the per-link `fld_value`,
    /// `pvxs/ioc/pvalink_lset.cpp:199`).
    pub fn link_metadata_with(
        &self,
        field: &str,
    ) -> Option<epics_base_rs::server::database::LinkMetadata> {
        use epics_base_rs::server::database::LinkMetadata;

        // same gate as the value/timestamp getters — a
        // disconnected monitor link must not surface its stale latched
        // display/control/valueAlarm metadata, DBF type or element
        // count. pvxs's `CHECK_VALID` (`valid() = connected && root`)
        // makes every metadata getter (`pvaGetGraphicLimits`,
        // `pvaGetDBFtype`, …) a no-op while disconnected even though
        // the NT snapshot is retained.
        if self.monitor_disconnected_stale() {
            return None;
        }

        let root = self.latest.lock().clone()?;

        // DBF type and element count derive from the selected value.
        // pvxs `pvaGetDBFtype` reads `fld_value`, which is the
        // selected-root rule: empty `field` selects the top-level value
        // (its `.value` for a structure, else the root itself), a
        // non-empty `field` selects `root[field]` (then `.value` if that
        // is itself a structure). See [`select_link_value`]
        // (`pvalink_link.cpp:90-110`, `pvalink_lset.cpp:199-236`).
        let value_field = select_link_value(&root, field);

        let dbf_type = link_dbf_type(&value_field);
        let element_count = link_element_count(&value_field);

        // display / control / valueAlarm are read from the top-level
        // root here (the `field=`-aware `fld_meta` selection is a
        // separate concern tracked on its own).
        let graphic_limits = limit_pair(&root, "display.limitLow", "display.limitHigh");
        let control_limits = limit_pair(&root, "control.limitLow", "control.limitHigh");
        let alarm_limits = {
            let lolo = scalar_as_f64(&extract_field(&root, "valueAlarm.lowAlarmLimit"));
            let lo = scalar_as_f64(&extract_field(&root, "valueAlarm.lowWarningLimit"));
            let hi = scalar_as_f64(&extract_field(&root, "valueAlarm.highWarningLimit"));
            let hihi = scalar_as_f64(&extract_field(&root, "valueAlarm.highAlarmLimit"));
            match (lolo, lo, hi, hihi) {
                // pvxs writes each of the four buffers independently
                // and leaves a missing one untouched; we only surface
                // the alarm-limit set when at least one is present,
                // defaulting the absent ones to 0.0 to mirror a
                // record's zero-initialised limit fields.
                (None, None, None, None) => None,
                (a, b, c, d) => Some((
                    a.unwrap_or(0.0),
                    b.unwrap_or(0.0),
                    c.unwrap_or(0.0),
                    d.unwrap_or(0.0),
                )),
            }
        };
        let precision = scalar_as_f64(&extract_field(&root, "display.precision")).map(|p| p as i16);
        let units = string_field(&root, "display.units");
        let description = string_field(&root, "display.description");

        Some(LinkMetadata {
            dbf_type,
            element_count,
            graphic_limits,
            control_limits,
            alarm_limits,
            precision,
            units,
            description,
        })
    }

    /// Test-only constructor: build a [`PvaLink`] with a pre-seeded
    /// cached value and no live connection. Lets the unit tests
    /// exercise the cache-reading accessors (`link_alarm_severity`,
    /// `alarm_message`, `try_read_cached`) and the defer queue
    /// without standing up a PVA server.
    #[cfg(test)]
    pub(crate) fn for_test(config: PvaLinkConfig, cached: Option<PvField>) -> Self {
        let client = PvaClient::builder().timeout(Duration::from_secs(1)).build();
        Self {
            _monitor_abort: None,
            config,
            client,
            latest: Arc::new(Mutex::new(cached)),
            monitor_connected: None,
            notify_rx: Mutex::new(None),
            pending_put: Mutex::new(None),
        }
    }

    /// Test-only constructor: build a [`PvaLink`] around a
    /// caller-supplied [`PvaClient`] (typically one pinned at a test
    /// `PvaServer` address). Lets a server-backed test exercise the
    /// real `write` / `write_pv_field` wire paths without UDP
    /// discovery.
    #[cfg(test)]
    pub(crate) fn for_test_with_client(config: PvaLinkConfig, client: PvaClient) -> Self {
        Self {
            _monitor_abort: None,
            config,
            client,
            latest: Arc::new(Mutex::new(None)),
            monitor_connected: None,
            notify_rx: Mutex::new(None),
            pending_put: Mutex::new(None),
        }
    }

    /// Test-only constructor for an INP+monitor link whose
    /// live-connection flag is externally controllable. Returns the
    /// link plus the shared `AtomicBool` so a test can simulate the
    /// re-subscribe loop's connect / event / disconnect transitions
    /// (B-pvalink-restart) without standing up a PVA server.
    #[cfg(test)]
    pub(crate) fn for_test_with_monitor_flag(
        config: PvaLinkConfig,
        cached: Option<PvField>,
    ) -> (Self, Arc<AtomicBool>) {
        let client = PvaClient::builder().timeout(Duration::from_secs(1)).build();
        let flag = Arc::new(AtomicBool::new(false));
        let link = Self {
            _monitor_abort: None,
            config,
            client,
            latest: Arc::new(Mutex::new(cached)),
            monitor_connected: Some(flag.clone()),
            notify_rx: Mutex::new(None),
            pending_put: Mutex::new(None),
        };
        (link, flag)
    }
}

/// True iff a [`PvaError`] indicates the upstream is currently
/// unreachable (as opposed to a value-level rejection). Used to
/// decide whether a `retry` link should queue the Put (B4).
///
/// pvxs gates `retry` on `!pvaLink::valid()` — "the channel is not
/// connected" — so the classification here mirrors that: I/O errors,
/// timeouts, refused connections, an unresolved channel, and the
/// search-failure (`no servers found`) case all mean "not connected
/// yet", and a `retry` link queues the Put for replay on connect. A
/// genuine value rejection (`InvalidValue`, `Decode`) is not a
/// disconnect — retrying it would fail identically.
fn is_disconnect(e: &epics_pva_rs::error::PvaError) -> bool {
    use epics_pva_rs::error::PvaError;
    match e {
        PvaError::Io(_)
        | PvaError::Timeout
        | PvaError::ChannelNotFound(_)
        | PvaError::ConnectionRefused
        // The explicit disconnect variant: the virtual circuit dropped
        // after connecting, so a `retry` link queues the Put for replay.
        | PvaError::Disconnected => true,
        // The client reports a failed name search as a Protocol
        // error ("no servers found for PV ..."); that is a
        // not-connected condition, not a protocol violation.
        PvaError::Protocol(msg) => {
            let m = msg.to_ascii_lowercase();
            m.contains("no servers found")
                || m.contains("not connected")
                || m.contains("disconnect")
        }
        PvaError::InvalidValue(_) | PvaError::Decode(_) => false,
        // an interrupted *wait* is not a disconnect. The
        // channel stays connected and the underlying Put keeps running,
        // recoverable by a later `wait` (`epics-pva-rs` error.rs:11-16) —
        // it is neither a "not connected yet" condition nor a value
        // rejection. A `retry` link must NOT queue it: the original Put
        // is still in flight, so replaying on the next connect would
        // duplicate the write.
        PvaError::Interrupted => false,
        // `RemoteError` is a server-side rejection on a *connected*
        // channel (not transport-down); `Finished` / `Connected` are
        // lifecycle sentinels, never a "not connected yet" condition. A
        // `retry` link must not queue on any of them — the server has
        // already answered, or the channel is up.
        PvaError::RemoteError(_) | PvaError::Finished | PvaError::Connected => false,
    }
}

/// Build the pvRequest for an OUT PUT operation from the link's
/// [`ProcMode`] and the caller's `block` argument. Mirrors pvxs
/// `pvalink_channel.cpp:28-47` (putReq template) + `220-263` (runtime
/// process/block computation):
///   - `record._options.process` ← [`ProcMode::put_process_request`]
///     (`Default → "passive"`, `Npp → "false"`, `Pp`/`Cp`/`Cpp → "true"`)
///   - `block`                   → `"true"` / `"false"` (pvxs `wait`)
///
/// the process value is the three-way wire string the
/// `ProcMode` enum derives, not a two-way bool — `Default` and `Npp`
/// were previously indistinguishable (`"passive"` for both), and
/// `Cp`/`Cpp` could not request remote processing on PUT.
fn build_put_request(proc: ProcMode, block: bool) -> PvRequestExpr {
    PvRequestExpr {
        fields: vec![],
        record_options: vec![
            (
                "process".to_string(),
                proc.put_process_request().to_string(),
            ),
            (
                "block".to_string(),
                if block { "true" } else { "false" }.to_string(),
            ),
        ],
        field_options: vec![],
    }
}

/// True iff `field` names a non-default sub-field (not `""` or `"value"`).
/// When true, PUT must use `pvput_field_with_request` to target that
/// specific field in the DATA phase. Mirrors pvxs `linkBuildPut:138`:
/// `top[fieldName]` when `fieldName` is non-empty.
fn is_subfield(field: &str) -> bool {
    !field.is_empty() && field != "value"
}

/// build the pvRequest for an INP+monitor link.
///
/// pvxs `pvaLink::makeRequest` (`pvalink_link.cpp:47-65`) ALWAYS
/// emits three fields on every INP monitor request:
///
///   - `record._options.pipeline`  — boolean, honors `cfg.pipeline`
///   - `record._options.atomic`    — hard-coded `true` (forces the
///     remote QSRV/group to assemble atomic snapshots even when the
///     local pvalink isn't part of an atomic scan batch — these are
///     related but distinct concepts).
///   - `record._options.queueSize` — int, defaults to 4 even when
///     no other option requires negotiation.
///
/// The earlier Rust path returned `None` for the default monitor,
/// so a no-options INP link sent no pvRequest and the remote server
/// fell back to its own defaults — including possibly non-atomic
/// snapshots and a different queue depth. Match pvxs by always
/// returning a request with all three fields populated.
fn monitor_request(config: &PvaLinkConfig) -> Option<epics_pva_rs::pv_request::PvRequestExpr> {
    let mut req = epics_pva_rs::pv_request::PvRequestExpr::default();
    req.record_options.push((
        "pipeline".to_string(),
        if config.pipeline { "true" } else { "false" }.to_string(),
    ));
    // pvxs `pvalink_link.cpp:64`: forced true on the remote request,
    // independent of `cfg.atomic` (the local scan-batch flag).
    req.record_options
        .push(("atomic".to_string(), "true".to_string()));
    req.record_options.push((
        "queueSize".to_string(),
        config.queue_size.max(1).to_string(),
    ));
    Some(req)
}

/// Select the link's *target* from the remote root, following pvxs's
/// `pvaLink::onTypeChange()` rule (`pvxs/ioc/pvalink_link.cpp:90-110`):
/// an empty `field` selects the top-level root (`lchan->root`); a
/// non-empty `field` selects `root[field]` (a dotted path navigates
/// through nested structures). The target is the basis both for the
/// value (see [`select_link_value`]) and for the display / control /
/// valueAlarm metadata.
fn select_target(root: &PvField, field: &str) -> PvField {
    if field.is_empty() {
        root.clone()
    } else {
        extract_field(root, field)
    }
}

/// Select the link's *value* from the remote root, following pvxs's
/// `pvaGetDBFtype` / `onTypeChange` rule
/// (`pvxs/ioc/pvalink_lset.cpp:199-236`, `pvalink_link.cpp:90-110`):
/// after [`select_target`] picks the target, if that target is itself
/// a structure (an NTScalar/NTScalarArray) its `.value` child is the
/// value; otherwise the selected target *is* the value (a top-level
/// or sub-field scalar/array).
///
/// For the default empty `field` on an NTScalar this yields `root.value`
/// — identical to the former hard-coded `field="value"`; the new
/// behavior is that a top-level non-structure value (a bare scalar/array
/// PV) is selected directly instead of being searched for a non-existent
/// `value` child, and that an explicitly selected sub-structure drills
/// into its own `.value`.
fn select_link_value(root: &PvField, field: &str) -> PvField {
    match select_target(root, field) {
        PvField::Structure(s) => s.get_field("value").cloned().unwrap_or(PvField::Null),
        other => other,
    }
}

/// Walk a dotted field path through a [`PvField`] and return the leaf value.
fn extract_field(root: &PvField, path: &str) -> PvField {
    if path.is_empty() {
        return root.clone();
    }
    let mut cursor = root.clone();
    for segment in path.split('.') {
        cursor = match cursor {
            PvField::Structure(s) => s.get_field(segment).cloned().unwrap_or(PvField::Null),
            other => return other,
        };
    }
    cursor
}

fn scalar_as_f64(field: &PvField) -> Option<f64> {
    match field {
        PvField::Scalar(sv) => Some(scalar_value_to_f64(sv)),
        PvField::Structure(s) => s.get_value().map(scalar_value_to_f64),
        _ => None,
    }
}

fn scalar_value_to_f64(v: &ScalarValue) -> f64 {
    match v {
        ScalarValue::Boolean(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        ScalarValue::Byte(x) => *x as f64,
        ScalarValue::UByte(x) => *x as f64,
        ScalarValue::Short(x) => *x as f64,
        ScalarValue::UShort(x) => *x as f64,
        ScalarValue::Int(x) => *x as f64,
        ScalarValue::UInt(x) => *x as f64,
        ScalarValue::Long(x) => *x as f64,
        ScalarValue::ULong(x) => *x as f64,
        ScalarValue::Float(x) => *x as f64,
        ScalarValue::Double(x) => *x,
        ScalarValue::String(s) => s.parse().unwrap_or(0.0),
    }
}

/// Map the cached NT value at the link's field path to a DBF type,
/// mirroring pvxs `pvaGetDBFtype` (`pvxs/ioc/pvalink_lset.cpp:199`).
///
/// An NT `enum_t` structure (an `index` integer + `choices` string
/// array) maps to `Enum`; a scalar / scalar array maps by element
/// type; anything else has no DBF mapping and yields `None` (the
/// record then keeps its own field type).
fn link_dbf_type(value_field: &PvField) -> Option<epics_base_rs::server::database::LinkDbfType> {
    use epics_base_rs::server::database::LinkDbfType;

    let from_scalar = |sv: &ScalarValue| match sv {
        ScalarValue::Byte(_) => Some(LinkDbfType::Char),
        ScalarValue::UByte(_) => Some(LinkDbfType::UChar),
        ScalarValue::Short(_) => Some(LinkDbfType::Short),
        ScalarValue::UShort(_) => Some(LinkDbfType::UShort),
        ScalarValue::Int(_) => Some(LinkDbfType::Long),
        ScalarValue::UInt(_) => Some(LinkDbfType::ULong),
        ScalarValue::Long(_) => Some(LinkDbfType::Int64),
        ScalarValue::ULong(_) => Some(LinkDbfType::UInt64),
        ScalarValue::Float(_) => Some(LinkDbfType::Float),
        ScalarValue::Double(_) => Some(LinkDbfType::Double),
        ScalarValue::String(_) => Some(LinkDbfType::String),
        // pvxs maps a boolean value through `DBF_LONG` (the
        // `default:` arm of the `pvaGetDBFtype` switch — booleans are
        // not a DBF type).
        ScalarValue::Boolean(_) => Some(LinkDbfType::Long),
    };

    match value_field {
        PvField::Scalar(sv) => from_scalar(sv),
        PvField::ScalarArray(arr) => arr.first().and_then(from_scalar),
        PvField::ScalarArrayTyped(arr) => {
            use epics_pva_rs::pvdata::ScalarType;
            Some(match arr.scalar_type() {
                ScalarType::Byte => LinkDbfType::Char,
                ScalarType::UByte => LinkDbfType::UChar,
                ScalarType::Short => LinkDbfType::Short,
                ScalarType::UShort => LinkDbfType::UShort,
                ScalarType::Int => LinkDbfType::Long,
                ScalarType::UInt => LinkDbfType::ULong,
                ScalarType::Long => LinkDbfType::Int64,
                ScalarType::ULong => LinkDbfType::UInt64,
                ScalarType::Float => LinkDbfType::Float,
                ScalarType::Double => LinkDbfType::Double,
                ScalarType::String => LinkDbfType::String,
                ScalarType::Boolean => LinkDbfType::Long,
            })
        }
        PvField::Structure(s) => {
            // NTEnum: pvxs maps a struct with an integer `index` and a
            // `choices` string array to `DBF_ENUM`.
            let has_index = matches!(
                s.get_field("index"),
                Some(PvField::Scalar(
                    ScalarValue::Byte(_)
                        | ScalarValue::UByte(_)
                        | ScalarValue::Short(_)
                        | ScalarValue::UShort(_)
                        | ScalarValue::Int(_)
                        | ScalarValue::UInt(_)
                        | ScalarValue::Long(_)
                        | ScalarValue::ULong(_)
                ))
            );
            let has_choices = matches!(
                s.get_field("choices"),
                Some(PvField::ScalarArray(_) | PvField::ScalarArrayTyped(_))
            );
            if has_index && has_choices {
                Some(LinkDbfType::Enum)
            } else {
                // A struct carrying a `value` sub-field (an NT struct)
                // — recurse into `value` so a link with an empty
                // field path still resolves the DBF type, matching
                // pvxs's "if fieldName empty, use top struct value".
                s.get_field("value").and_then(link_dbf_type)
            }
        }
        _ => None,
    }
}

/// Element count for the cached NT value at the link's field path:
/// the array length, or `1` for a scalar. Mirrors pvxs
/// `pvaGetElements` (`pvxs/ioc/pvalink_lset.cpp:242`).
fn link_element_count(value_field: &PvField) -> Option<i64> {
    match value_field {
        PvField::Scalar(_) => Some(1),
        PvField::ScalarArray(arr) => Some(arr.len() as i64),
        PvField::ScalarArrayTyped(arr) => Some(arr.len() as i64),
        PvField::Structure(s) => {
            // NTEnum / NT struct: the meaningful count is the `value`
            // sub-field's count (scalar index → 1).
            match s.get_field("value") {
                Some(v) => link_element_count(v),
                None if s.get_field("index").is_some() => Some(1),
                None => None,
            }
        }
        _ => None,
    }
}

/// Read a `(lo, hi)` limit pair from two dotted NT paths. Returns
/// `None` only when *neither* path resolves to a scalar — a missing
/// half defaults to `0.0`, mirroring pvxs leaving an unwritten
/// `*lo`/`*hi` buffer untouched against a record's zero-initialised
/// limit field.
fn limit_pair(root: &PvField, lo_path: &str, hi_path: &str) -> Option<(f64, f64)> {
    let lo = scalar_as_f64(&extract_field(root, lo_path));
    let hi = scalar_as_f64(&extract_field(root, hi_path));
    match (lo, hi) {
        (None, None) => None,
        (l, h) => Some((l.unwrap_or(0.0), h.unwrap_or(0.0))),
    }
}

/// Read a dotted NT path that should hold a string scalar. Empty
/// strings are treated as "absent" so an NT value that carries an
/// empty `display.units` does not override a record's local EGU.
fn string_field(root: &PvField, path: &str) -> Option<String> {
    match extract_field(root, path) {
        PvField::Scalar(ScalarValue::String(s)) if !s.is_empty() => Some(s),
        _ => None,
    }
}

// Suppress unused warning for fields used only via accessors.
#[allow(dead_code)]
fn _suppress(_: &PvStructure) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_top_level_value() {
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.5))));
        let root = PvField::Structure(s);
        let v = extract_field(&root, "value");
        match v {
            PvField::Scalar(ScalarValue::Double(d)) => assert_eq!(d, 1.5),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn extract_nested_field() {
        let mut alarm = PvStructure::new("alarm_t");
        alarm
            .fields
            .push(("severity".into(), PvField::Scalar(ScalarValue::Int(2))));
        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields
            .push(("alarm".into(), PvField::Structure(alarm)));
        let value = extract_field(&PvField::Structure(root), "alarm.severity");
        assert!(matches!(value, PvField::Scalar(ScalarValue::Int(2))));
    }

    #[test]
    fn missing_field_returns_null() {
        let s = PvStructure::new("epics:nt/NTScalar:1.0");
        let v = extract_field(&PvField::Structure(s), "nope");
        assert!(matches!(v, PvField::Null));
    }

    /// pvxs selected-root rule (`select_link_value`,
    /// `pvalink_link.cpp:90-110`): empty field selects the top-level
    /// value (`.value` for a structure, else the root itself); a
    /// non-empty field selects `root[field]` then `.value` if that is a
    /// structure. Default `field=""` on an NTScalar still yields the
    /// scalar, but a bare top-level value is now selected directly
    /// instead of being searched for a missing `value` child.
    #[test]
    fn select_link_value_follows_pvxs_selected_root_rule() {
        // NTScalar, empty field → .value.
        let mut nt = PvStructure::new("epics:nt/NTScalar:1.0");
        nt.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(2.5))));
        let nt = PvField::Structure(nt);
        assert!(matches!(
            select_link_value(&nt, ""),
            PvField::Scalar(ScalarValue::Double(d)) if d == 2.5
        ));
        // explicit field="value" yields the same scalar.
        assert!(matches!(
            select_link_value(&nt, "value"),
            PvField::Scalar(ScalarValue::Double(d)) if d == 2.5
        ));

        // Bare top-level scalar (PV whose root is not an NT structure):
        // empty field selects the scalar itself, not a missing child.
        let bare = PvField::Scalar(ScalarValue::Long(7));
        assert!(matches!(
            select_link_value(&bare, ""),
            PvField::Scalar(ScalarValue::Long(7))
        ));

        // Non-empty field selecting a nested structure drills into its
        // own `.value`.
        let mut sub = PvStructure::new("epics:nt/NTScalar:1.0");
        sub.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Long(42))));
        let mut outer = PvStructure::new("some_t");
        outer.fields.push(("sub".into(), PvField::Structure(sub)));
        let outer = PvField::Structure(outer);
        assert!(matches!(
            select_link_value(&outer, "sub"),
            PvField::Scalar(ScalarValue::Long(42))
        ));
    }

    /// `link_metadata` derives the DBF type from the selected value. A
    /// link whose `field` selects a nested NTScalar-shaped structure
    /// must report that structure's `.value` DBF type. Pre-fix the
    /// metadata path read the selected target *as* the value, so a
    /// structure selection produced no DBF type at all.
    #[test]
    fn link_metadata_dbf_drills_selected_substructure_value() {
        use epics_base_rs::server::database::LinkDbfType;

        let mut sub = PvStructure::new("epics:nt/NTScalar:1.0");
        sub.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Long(42))));
        let mut outer = PvStructure::new("some_t");
        outer.fields.push(("sub".into(), PvField::Structure(sub)));

        let cfg = PvaLinkConfig {
            field: "sub".to_string(),
            ..PvaLinkConfig::defaults_for("META:PV", LinkDirection::Inp)
        };
        let link = PvaLink::for_test(cfg, Some(PvField::Structure(outer)));
        let meta = link.link_metadata().expect("metadata present");
        // PVA `long` → DBF Int64 (full 64-bit width); the point is that
        // a DBF type is derived at all, from the sub-structure's .value.
        assert_eq!(
            meta.dbf_type,
            Some(LinkDbfType::Int64),
            "DBF type must come from the selected sub-structure's .value"
        );
    }

    /// A link whose `field` selects a nested structure must read its
    /// alarm severity, alarm message AND timestamp from that selected
    /// root — not from the top-level NT alarm/timeStamp. pvxs rebinds
    /// `root = lchan->root[fieldName]` in `onTypeChange`
    /// (`pvxs/ioc/pvalink_link.cpp:90-110`) and then resolves
    /// `fld_severity`/`fld_message`/`fld_seconds`/`fld_nanoseconds`/
    /// `fld_usertag` relative to that root
    /// (`pvxs/ioc/pvalink_lset.cpp:399-430`). Pre-fix every one of
    /// these getters read the top-level alarm/timeStamp regardless of
    /// the selected field, so a link selecting a nested member adopted
    /// the wrong member's (or the container's) alarm and time.
    #[test]
    fn alarm_and_timestamp_resolve_at_selected_field_root() {
        // Top-level alarm is NO_ALARM and timeStamp is secs=1; the
        // selected `member` sub-structure carries MAJOR(2)/"member hot"
        // and a distinct timeStamp secs=999, ns=7, userTag=5.
        let mut top_alarm = PvStructure::new("alarm_t");
        top_alarm
            .fields
            .push(("severity".into(), PvField::Scalar(ScalarValue::Int(0))));
        let mut top_ts = PvStructure::new("time_t");
        top_ts.fields.push((
            "secondsPastEpoch".into(),
            PvField::Scalar(ScalarValue::Long(1)),
        ));
        top_ts
            .fields
            .push(("nanoseconds".into(), PvField::Scalar(ScalarValue::Int(0))));

        let mut mem_alarm = PvStructure::new("alarm_t");
        mem_alarm
            .fields
            .push(("severity".into(), PvField::Scalar(ScalarValue::Int(2))));
        mem_alarm.fields.push((
            "message".into(),
            PvField::Scalar(ScalarValue::String("member hot".into())),
        ));
        let mut mem_ts = PvStructure::new("time_t");
        mem_ts.fields.push((
            "secondsPastEpoch".into(),
            PvField::Scalar(ScalarValue::Long(999)),
        ));
        mem_ts
            .fields
            .push(("nanoseconds".into(), PvField::Scalar(ScalarValue::Int(7))));
        mem_ts
            .fields
            .push(("userTag".into(), PvField::Scalar(ScalarValue::Int(5))));
        let mut member = PvStructure::new("epics:nt/NTScalar:1.0");
        member
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(7.0))));
        member
            .fields
            .push(("alarm".into(), PvField::Structure(mem_alarm)));
        member
            .fields
            .push(("timeStamp".into(), PvField::Structure(mem_ts)));

        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        root.fields
            .push(("alarm".into(), PvField::Structure(top_alarm)));
        root.fields
            .push(("timeStamp".into(), PvField::Structure(top_ts)));
        root.fields
            .push(("member".into(), PvField::Structure(member)));
        let value = PvField::Structure(root);

        // field="member", MS: alarm/message/timestamp come from member.
        let cfg = PvaLinkConfig {
            field: "member".to_string(),
            monitor: true,
            sevr: SevrMode::Ms,
            ..PvaLinkConfig::defaults_for("SEL:PV", LinkDirection::Inp)
        };
        let link = PvaLink::for_test(cfg, Some(value.clone()));
        assert_eq!(
            link.link_alarm_severity(),
            Some(2),
            "severity must come from the selected member, not top-level NO_ALARM"
        );
        assert_eq!(
            link.alarm_message().as_deref(),
            Some("member hot"),
            "message must come from the selected member"
        );
        assert_eq!(
            link.time_stamp("member"),
            Some((999, 7, 5)),
            "timestamp/userTag must come from the selected member"
        );

        // field="" reads the top-level root: NO_ALARM does not
        // propagate (None), and the timestamp is the top-level secs=1.
        let cfg_top = PvaLinkConfig {
            monitor: true,
            sevr: SevrMode::Ms,
            ..PvaLinkConfig::defaults_for("SEL:PV", LinkDirection::Inp)
        };
        let link_top = PvaLink::for_test(cfg_top, Some(value));
        assert_eq!(
            link_top.link_alarm_severity(),
            None,
            "top-level NO_ALARM under MS propagates nothing"
        );
        assert_eq!(link_top.alarm_message(), None);
        assert_eq!(
            link_top.time_stamp(""),
            Some((1, 0, 0)),
            "empty field reads the top-level timeStamp"
        );
    }

    use super::super::config::LinkDirection;
    use super::super::config::{PvaLinkConfig, SevrMode};

    /// Build an NTScalar-shaped structure with an `alarm.severity`
    /// (and optional `alarm.message`).
    fn nt_with_alarm(severity: i32, message: Option<&str>) -> PvField {
        let mut alarm = PvStructure::new("alarm_t");
        alarm.fields.push((
            "severity".into(),
            PvField::Scalar(ScalarValue::Int(severity)),
        ));
        if let Some(m) = message {
            alarm.fields.push((
                "message".into(),
                PvField::Scalar(ScalarValue::String(m.to_string())),
            ));
        }
        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(7.0))));
        root.fields
            .push(("alarm".into(), PvField::Structure(alarm)));
        PvField::Structure(root)
    }

    fn inp_cfg(sevr: SevrMode) -> PvaLinkConfig {
        PvaLinkConfig {
            monitor: true,
            sevr,
            ..PvaLinkConfig::defaults_for("X", LinkDirection::Inp)
        }
    }

    // ---- B2: MS / NMS / MSI severity propagation on the read path ----

    #[test]
    fn b2_nms_drops_all_severities() {
        for sev in 1..=3 {
            let link = PvaLink::for_test(
                inp_cfg(SevrMode::Nms),
                Some(nt_with_alarm(sev, Some("bad"))),
            );
            assert_eq!(link.link_alarm_severity(), None, "sev={sev}");
            assert_eq!(link.alarm_message(), None, "sev={sev}");
        }
    }

    #[test]
    fn b2_ms_propagates_any_nonzero_severity() {
        // NO_ALARM does not propagate.
        let ok = PvaLink::for_test(inp_cfg(SevrMode::Ms), Some(nt_with_alarm(0, None)));
        assert_eq!(ok.link_alarm_severity(), None);
        assert_eq!(ok.alarm_message(), None);
        // MINOR / MAJOR / INVALID all propagate.
        for sev in 1..=3 {
            let link = PvaLink::for_test(
                inp_cfg(SevrMode::Ms),
                Some(nt_with_alarm(sev, Some("oops"))),
            );
            assert_eq!(link.link_alarm_severity(), Some(sev), "sev={sev}");
            assert_eq!(link.alarm_message(), Some("oops".to_string()), "sev={sev}");
        }
    }

    #[test]
    fn b2_msi_propagates_only_invalid() {
        let minor = PvaLink::for_test(inp_cfg(SevrMode::Msi), Some(nt_with_alarm(1, Some("m"))));
        assert_eq!(minor.link_alarm_severity(), None);
        let major = PvaLink::for_test(inp_cfg(SevrMode::Msi), Some(nt_with_alarm(2, Some("m"))));
        assert_eq!(major.link_alarm_severity(), None);
        let invalid =
            PvaLink::for_test(inp_cfg(SevrMode::Msi), Some(nt_with_alarm(3, Some("dead"))));
        assert_eq!(invalid.link_alarm_severity(), Some(3));
        assert_eq!(invalid.alarm_message(), Some("dead".to_string()));
    }

    #[test]
    fn b2_synthetic_message_when_no_alarm_message_field() {
        // MS link, severity propagates, but the NT struct has no
        // alarm.message — a synthetic message is returned.
        let link = PvaLink::for_test(inp_cfg(SevrMode::Ms), Some(nt_with_alarm(2, None)));
        assert_eq!(link.link_alarm_severity(), Some(2));
        assert_eq!(link.alarm_message(), Some("remote severity 2".to_string()));
    }

    #[test]
    fn b2_no_cached_value_means_no_alarm() {
        let link = PvaLink::for_test(inp_cfg(SevrMode::Ms), None);
        assert_eq!(link.link_alarm_severity(), None);
        assert_eq!(link.alarm_message(), None);
    }

    // ---- B4: monitor_request (Q / pipeline) ----

    /// pvxs `pvaLink::makeRequest` always emits pipeline +
    /// atomic + queueSize even on a defaults-only INP monitor.
    /// Regression for the prior `None`-for-defaults shortcut that
    /// silently let the remote server fall back to its own defaults.
    #[test]
    fn b4_monitor_request_always_carries_pvxs_options() {
        let cfg = PvaLinkConfig::defaults_for("X", LinkDirection::Inp);
        let req = monitor_request(&cfg).expect("defaults still yield a request");
        // pipeline = false on default config; queueSize = pvxs default 4;
        // atomic = forced true.
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "pipeline" && v == "false")
        );
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "atomic" && v == "true"),
            "atomic must be hard-coded true on remote pvalink monitor requests"
        );
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "queueSize" && v == "4"),
            "queueSize must default to pvxs's 4 on no-options links"
        );
    }

    #[test]
    fn b4_monitor_request_carries_queue_size() {
        let cfg = PvaLinkConfig {
            queue_size: 16,
            ..PvaLinkConfig::defaults_for("X", LinkDirection::Inp)
        };
        let req = monitor_request(&cfg).expect("non-default Q yields a request");
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "queueSize" && v == "16")
        );
    }

    #[test]
    fn b4_monitor_request_carries_pipeline() {
        let cfg = PvaLinkConfig {
            pipeline: true,
            ..PvaLinkConfig::defaults_for("X", LinkDirection::Inp)
        };
        let req = monitor_request(&cfg).expect("pipeline yields a request");
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "pipeline" && v == "true")
        );
        // pvxs `makeRequest` always sends queueSize alongside pipeline.
        assert!(req.record_options.iter().any(|(k, _)| k == "queueSize"));
    }

    // ---- B4: defer / retry Put queue ----

    fn out_cfg(defer: bool, retry: bool) -> PvaLinkConfig {
        PvaLinkConfig {
            defer,
            retry,
            ..PvaLinkConfig::defaults_for("X", LinkDirection::Out)
        }
    }

    #[tokio::test]
    async fn b4_defer_coalesces_to_most_recent() {
        let link = PvaLink::for_test(out_cfg(true, false), None);
        assert_eq!(link.pending_put_count(), 0);
        // defer=true: write caches, returns Ok without a server.
        link.write("42").await.expect("deferred write is Ok");
        assert_eq!(link.pending_put_count(), 1);
        // A second deferred write supersedes the first — the link keeps
        // one pending value (most-recent-PUT wins), it does not grow a
        // FIFO log of stale intermediate writes.
        link.write_pv_field(&PvField::Scalar(ScalarValue::Double(1.0)))
            .await
            .expect("deferred typed write is Ok");
        assert_eq!(link.pending_put_count(), 1);
        // The retained value is the most recent (the typed Double),
        // not the superseded "42".
        match link.pending_put.lock().as_ref() {
            Some(QueuedPut::Field(PvField::Scalar(ScalarValue::Double(d)))) => assert_eq!(*d, 1.0),
            other => panic!("most-recent write must be retained, got {other:?}"),
        }
    }

    /// MINOR (pvalink string-PUT): a deferred string `write` is cached
    /// as a `QueuedPut::Str`, NOT a `PvField::Scalar(String)`. The
    /// replay then goes through the string `pvput` path, which coerces
    /// the text against the channel's native scalar type — replaying a
    /// String field to a numeric record was the bug. A typed
    /// `write_pv_field` is cached as `QueuedPut::Field` verbatim.
    #[tokio::test]
    async fn minor_deferred_string_put_keeps_string_form() {
        let str_link = PvaLink::for_test(out_cfg(true, false), None);
        str_link.write("42").await.unwrap();
        match str_link.pending_put.lock().as_ref() {
            Some(QueuedPut::Str(s)) => assert_eq!(s, "42"),
            other => panic!("string write must cache QueuedPut::Str, got {other:?}"),
        }

        let field_link = PvaLink::for_test(out_cfg(true, false), None);
        field_link
            .write_pv_field(&PvField::Scalar(ScalarValue::Double(1.0)))
            .await
            .unwrap();
        match field_link.pending_put.lock().as_ref() {
            Some(QueuedPut::Field(PvField::Scalar(ScalarValue::Double(d)))) => assert_eq!(*d, 1.0),
            other => panic!("typed write must cache QueuedPut::Field, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn b4_retry_queues_on_disconnect() {
        // retry=true, no server reachable → write should queue rather
        // than error.
        let link = PvaLink::for_test(out_cfg(false, true), None);
        let r = link.write("7").await;
        assert!(r.is_ok(), "retry write should queue, got {r:?}");
        assert_eq!(link.pending_put_count(), 1);
    }

    #[tokio::test]
    async fn b4_no_retry_surfaces_disconnect_error() {
        // retry=false, no server → write must surface the error.
        let link = PvaLink::for_test(out_cfg(false, false), None);
        let r = link.write("7").await;
        assert!(r.is_err(), "non-retry write must error on disconnect");
        assert_eq!(link.pending_put_count(), 0);
    }

    #[tokio::test]
    async fn b4_flush_deferred_drops_value_on_hard_error() {
        // defer link, retry=false; flush against no server. The pending
        // value's Put fails with a hard error → the value is dropped
        // (it would fail identically on every replay), leaving the slot
        // empty.
        let link = PvaLink::for_test(out_cfg(true, false), None);
        link.write("1").await.unwrap();
        link.write("2").await.unwrap();
        // Coalesced to one pending value ("2").
        assert_eq!(link.pending_put_count(), 1);
        let r = link.flush_deferred().await;
        assert!(r.is_err());
        assert_eq!(link.pending_put_count(), 0);
    }

    #[tokio::test]
    async fn b4_flush_deferred_retry_restores_pending() {
        // defer + retry: flush against no server → the pending value is
        // restored to the slot for a later retry.
        let link = PvaLink::for_test(out_cfg(true, true), None);
        link.write("1").await.unwrap();
        link.write("2").await.unwrap();
        assert_eq!(link.pending_put_count(), 1);
        let r = link.flush_deferred().await;
        assert!(r.is_err(), "still disconnected");
        // retry restores the most-recent pending value ("2").
        assert_eq!(link.pending_put_count(), 1);
        match link.pending_put.lock().as_ref() {
            Some(QueuedPut::Str(s)) => assert_eq!(s, "2"),
            other => panic!("most-recent value must be restored, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn b4_flush_deferred_retry_keeps_newer_write() {
        // defer + retry: a fresh write that lands during a still-failing
        // flush must NOT be clobbered by the restore — most-recent-PUT
        // wins. Here the slot already holds "2"; simulate a newer write
        // by pre-seeding then confirming the restore is a no-op when the
        // slot is already occupied.
        let link = PvaLink::for_test(out_cfg(true, true), None);
        link.write("1").await.unwrap();
        // flush takes "1"; while the put is failing a newer "2" arrives.
        // Model it by writing "2" after the take would have happened —
        // since flush is synchronous here, instead assert the guard:
        // restore only when slot empty.
        let r = link.flush_deferred().await;
        assert!(r.is_err());
        link.write("2").await.unwrap();
        // A second flush restores "2" (not the older "1").
        let r2 = link.flush_deferred().await;
        assert!(r2.is_err());
        match link.pending_put.lock().as_ref() {
            Some(QueuedPut::Str(s)) => assert_eq!(s, "2"),
            other => panic!("newer write must survive, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn b4_flush_on_inp_link_rejected() {
        let link = PvaLink::for_test(inp_cfg(SevrMode::Nms), None);
        assert!(matches!(
            link.flush_deferred().await,
            Err(PvaLinkError::NotWritable)
        ));
    }

    #[test]
    fn b3_take_notify_rx_only_once() {
        // INP+monitor link built via for_test has no channel (no live
        // monitor), so take_notify_rx is None — exercised here for
        // the OUT / non-monitor branch. The live-channel path is
        // covered by the integration-side forwarder test.
        let link = PvaLink::for_test(inp_cfg(SevrMode::Nms), None);
        assert!(link.take_notify_rx().is_none());
    }

    // ---- B-pvalink-restart: INP monitor disconnect reflection ----

    /// BUG 2 regression: `is_connected()` for an INP+monitor link is
    /// driven by the monitor task's live-connection flag, so an
    /// upstream disconnect (IOC restart / transient I/O) is reflected
    /// even though a value is still cached. Pre-fix `is_connected()`
    /// returned `latest.is_some()`, which stayed `true` forever once
    /// any value had been cached.
    #[test]
    fn bug2_is_connected_reflects_monitor_disconnect() {
        // Link with a cached value (a prior event) but the monitor
        // flag still false — not yet (re)connected.
        let (link, flag) = PvaLink::for_test_with_monitor_flag(
            inp_cfg(SevrMode::Nms),
            Some(PvField::Scalar(ScalarValue::Double(1.0))),
        );
        assert!(
            !link.is_connected(),
            "cached value alone must NOT report connected"
        );

        // Monitor delivers an event → flag flips true.
        flag.store(true, Ordering::Release);
        assert!(link.is_connected(), "live subscription reports connected");

        // Upstream subscription ends (IOC restart) → flag false, even
        // though `latest` still holds the stale cached value.
        flag.store(false, Ordering::Release);
        assert!(
            link.latest_value().is_some(),
            "stale value is still cached after disconnect"
        );
        assert!(
            !link.is_connected(),
            "disconnect must be reflected despite the stale cached value"
        );

        // Re-subscribe loop delivers a fresh event → connected again.
        flag.store(true, Ordering::Release);
        assert!(link.is_connected(), "re-subscribe restores connected");
    }

    // ---- disconnected monitor must not serve stale ----
    // Tested by invariant boundary, not by narrative: the four
    // (cached, connected) corners each get one case.

    /// Boundary (cached=Some, connected=false): a monitor link that
    /// HAD a value but whose subscription is now down. Every read
    /// hook MUST refuse the stale cache; the alarm hooks MUST report
    /// LINK/INVALID regardless of the link's NMS mode (a disconnect is
    /// a link failure, not a remote-severity propagation NMS suppresses).
    /// The diagnostic `latest_value` slot survives.
    #[tokio::test]
    async fn fr13_disconnected_monitor_read_fails_and_reports_invalid() {
        let (link, flag) = PvaLink::for_test_with_monitor_flag(
            inp_cfg(SevrMode::Nms),
            Some(nt_with_alarm(0, None)),
        );
        // Connected once, then dropped.
        flag.store(true, Ordering::Release);
        flag.store(false, Ordering::Release);

        // Value reads refuse the stale cache.
        assert!(
            matches!(link.read().await, Err(PvaLinkError::Disconnected(_))),
            "disconnected monitor read must fail, not return the stale value"
        );
        assert!(
            link.try_read_cached().is_none(),
            "sync fast path must return None while disconnected"
        );

        // Alarm hooks report INVALID(3) + a disconnect message even
        // though the link is NMS (which suppresses remote severities).
        assert_eq!(
            link.link_alarm_severity_with("", SevrMode::Nms),
            Some(3),
            "disconnect must surface INVALID regardless of NMS"
        );
        assert_eq!(
            link.alarm_message_with("", SevrMode::Nms).as_deref(),
            Some("pvalink monitor disconnected"),
            "disconnect message, not the stale remote alarm string"
        );

        // The cached value itself is preserved for diagnostics/timestamp.
        assert!(
            link.latest_value().is_some(),
            "stale value retained for diagnostic accessors"
        );
    }

    /// Boundary (cached=Some, connected=true): a live monitor link
    /// serves its cached value and propagates the MS-gated remote
    /// severity exactly as before — the FR-13 gate is inert when
    /// connected.
    #[tokio::test]
    async fn fr13_connected_monitor_serves_cached_value_and_remote_alarm() {
        let (link, flag) = PvaLink::for_test_with_monitor_flag(
            inp_cfg(SevrMode::Ms),
            Some(nt_with_alarm(2, Some("hot"))),
        );
        flag.store(true, Ordering::Release);

        assert!(link.read().await.is_ok(), "live monitor read succeeds");
        assert!(
            link.try_read_cached().is_some(),
            "live monitor fast path returns the cached value"
        );
        // MS link, remote MAJOR(2) → propagates 2, not the disconnect INVALID.
        assert_eq!(link.link_alarm_severity_with("", SevrMode::Ms), Some(2));
        assert_eq!(
            link.alarm_message_with("", SevrMode::Ms).as_deref(),
            Some("hot")
        );
    }

    /// Boundary (cached=None, connected=false): a monitor link that
    /// NEVER connected. Reads fail (no GET-bootstrap for monitor
    /// links), but `alarm_severity` returns `None` — NOT INVALID — so
    /// the base `external_link_alarm` bare-name loop, which tries every
    /// registered lset, does not let a PVA lset mis-claim a name a
    /// sibling lset owns. The never-connected INVALID is owned by the
    /// value path (`get_value` → None → base LINK/INVALID, gated by the
    /// parsed link type).
    #[tokio::test]
    async fn fr13_never_connected_monitor_reports_no_alarm_but_read_fails() {
        let (link, _flag) = PvaLink::for_test_with_monitor_flag(inp_cfg(SevrMode::Ms), None);
        assert!(
            matches!(link.read().await, Err(PvaLinkError::Disconnected(_))),
            "never-connected monitor read fails (no stale, no GET-bootstrap)"
        );
        assert!(link.try_read_cached().is_none());
        assert_eq!(
            link.link_alarm_severity_with("", SevrMode::Ms),
            None,
            "never-connected link must not claim INVALID via the alarm hook"
        );
        assert_eq!(link.alarm_message_with("", SevrMode::Ms), None);
    }

    /// Boundary (cached=Some, connected=false): the FR-13 stale gate is
    /// a defect *family* — it must cover every lset getter that
    /// surfaces cached NT-derived state, not just value/alarm. pvxs
    /// gates each metadata getter through `CHECK_VALID`, so a
    /// disconnected monitor's `time_stamp` (`pvaGetTimeStampTag`) and
    /// `link_metadata` (`pvaGetGraphicLimits`/`pvaGetDBFtype`/…) must
    /// report no-data while the snapshot is retained — exactly like the
    /// value read. Pre-fix these two getters served the stale latched
    /// metadata/timestamp. The existing `for_test(None)` case
    /// only covers cached=None; this covers cached=Some+disconnected.
    #[tokio::test]
    async fn fr13_disconnected_monitor_metadata_and_timestamp_refuse_stale() {
        // NT value carrying a timeStamp slot and a display sub-structure
        // so both getters have something to (wrongly) serve if ungated.
        let mut ts = PvStructure::new("time_t");
        ts.fields.push((
            "secondsPastEpoch".into(),
            PvField::Scalar(ScalarValue::Long(1_700_000_000)),
        ));
        ts.fields
            .push(("nanoseconds".into(), PvField::Scalar(ScalarValue::Int(42))));
        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        root.fields
            .push(("display".into(), nt_display(-9.0, 9.0, "arb", "linked", 2)));
        root.fields
            .push(("timeStamp".into(), PvField::Structure(ts)));

        let (link, flag) = PvaLink::for_test_with_monitor_flag(
            inp_cfg(SevrMode::Nms),
            Some(PvField::Structure(root)),
        );

        // Connected: both getters serve the cached metadata/timestamp
        // (the FR-13 gate is inert while connected).
        flag.store(true, Ordering::Release);
        assert_eq!(
            link.time_stamp(""),
            Some((1_700_000_000, 42, 0)),
            "connected monitor surfaces the cached timestamp; no userTag \
             field in the fixture, so the tag defaults to 0"
        );
        assert!(
            link.link_metadata().is_some(),
            "connected monitor surfaces the cached metadata"
        );

        // Disconnected: both refuse the stale snapshot, even though it
        // is still cached for diagnostics.
        flag.store(false, Ordering::Release);
        assert!(
            link.time_stamp("").is_none(),
            "disconnected monitor must not serve the stale latched timestamp"
        );
        assert!(
            link.link_metadata().is_none(),
            "disconnected monitor must not serve the stale latched metadata"
        );
        assert!(
            link.latest_value().is_some(),
            "snapshot retained (pvxs keeps root; CHECK_VALID gates the getters)"
        );
    }

    /// The remote `timeStamp.userTag` is a signed int32 on the wire, but
    /// the adopted record tag is a 64-bit `epicsUTag`. Widening must
    /// zero-extend (`as u32 as u64`), never sign-extend: a bit-31 tag
    /// like 0x9000_0000 must widen to 0x0000_0000_9000_0000, not
    /// 0xFFFF_FFFF_9000_0000. This is the pvData `as<uint32_t>()` (not
    /// `as<epicsUTag>()`) read at `pvalink_lset.cpp:406-409`. Boundaries:
    /// absent tag, small positive, and the bit-31 sign boundary on both
    /// the signed `Int` and unsigned `UInt` carriers.
    #[tokio::test]
    async fn pvalink_usertag_widens_without_sign_extension() {
        // Build a connected monitor link whose cached NT value carries a
        // timeStamp with the given userTag field, then read the tag.
        let read_tag = |usertag: Option<PvField>| -> Option<u64> {
            let mut ts = PvStructure::new("time_t");
            ts.fields.push((
                "secondsPastEpoch".into(),
                PvField::Scalar(ScalarValue::Long(1_700_000_000)),
            ));
            ts.fields
                .push(("nanoseconds".into(), PvField::Scalar(ScalarValue::Int(42))));
            if let Some(f) = usertag {
                ts.fields.push(("userTag".into(), f));
            }
            let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
            root.fields
                .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
            root.fields
                .push(("timeStamp".into(), PvField::Structure(ts)));
            let (link, flag) = PvaLink::for_test_with_monitor_flag(
                inp_cfg(SevrMode::Nms),
                Some(PvField::Structure(root)),
            );
            flag.store(true, Ordering::Release);
            link.time_stamp("").map(|(_, _, utag)| utag)
        };

        // Absent userTag → 0 (pvxs `else snap_tag = 0`).
        assert_eq!(read_tag(None), Some(0), "absent userTag adopts tag 0");
        // Small positive signed tag → identity.
        assert_eq!(
            read_tag(Some(PvField::Scalar(ScalarValue::Int(5)))),
            Some(5),
            "small positive userTag widens to itself"
        );
        // bit-31 set on the signed `Int` carrier: 0x9000_0000 as i32 is
        // negative, so a naive `as u64` would sign-extend.
        assert_eq!(
            read_tag(Some(PvField::Scalar(ScalarValue::Int(
                0x9000_0000u32 as i32
            )))),
            Some(0x0000_0000_9000_0000),
            "bit-31 signed userTag must zero-extend, not sign-extend to \
             0xFFFF_FFFF_9000_0000"
        );
        // int32 -1 widens to 0xFFFF_FFFF, not u64::MAX.
        assert_eq!(
            read_tag(Some(PvField::Scalar(ScalarValue::Int(-1)))),
            Some(0x0000_0000_FFFF_FFFF),
            "int32 -1 userTag must widen to 0xFFFF_FFFF, not 0xFFFF_FFFF_FFFF_FFFF"
        );
        // Same boundary on the unsigned `UInt` carrier.
        assert_eq!(
            read_tag(Some(PvField::Scalar(ScalarValue::UInt(0x9000_0000)))),
            Some(0x0000_0000_9000_0000),
            "bit-31 unsigned userTag widens cleanly"
        );
    }

    /// BUG 2: a live INP+monitor link spawns the re-subscribe loop and
    /// installs the `monitor_connected` flag. Before the first event
    /// the link reports disconnected (no liveness proven yet); the
    /// monitor task is present so a later disconnect is observable.
    #[tokio::test]
    async fn bug2_inp_monitor_link_installs_connection_flag() {
        // INP + monitor against a PV with no server reachable. `open`
        // spawns the re-subscribe loop; `pvmonitor` fails fast and the
        // loop backs off — `is_connected()` stays false (no event
        // delivered) instead of being absent/true.
        let cfg = PvaLinkConfig {
            monitor: true,
            ..PvaLinkConfig::defaults_for("BUG2:NOPV", LinkDirection::Inp)
        };
        let link = PvaLink::open(cfg).await.expect("open INP monitor link");
        assert!(
            link.monitor_connected.is_some(),
            "INP+monitor link must install the live-connection flag"
        );
        assert!(
            !link.is_connected(),
            "no event delivered yet → not connected"
        );
    }

    #[test]
    fn b4_is_disconnect_classification() {
        use epics_pva_rs::error::PvaError;
        assert!(is_disconnect(&PvaError::Timeout));
        assert!(is_disconnect(&PvaError::ConnectionRefused));
        assert!(is_disconnect(&PvaError::ChannelNotFound("x".into())));
        // value / protocol rejections are NOT disconnects.
        assert!(!is_disconnect(&PvaError::InvalidValue("x".into())));
        assert!(!is_disconnect(&PvaError::Protocol("x".into())));
        assert!(!is_disconnect(&PvaError::Decode("x".into())));
    }

    // ---- pvalink DB-link metadata hooks ----

    /// Build a numeric `display` sub-structure with limitLow/limitHigh,
    /// units, description and precision.
    fn nt_display(lo: f64, hi: f64, units: &str, desc: &str, prec: i32) -> PvField {
        let mut d = PvStructure::new("");
        d.fields
            .push(("limitLow".into(), PvField::Scalar(ScalarValue::Double(lo))));
        d.fields
            .push(("limitHigh".into(), PvField::Scalar(ScalarValue::Double(hi))));
        d.fields.push((
            "units".into(),
            PvField::Scalar(ScalarValue::String(units.to_string())),
        ));
        d.fields.push((
            "description".into(),
            PvField::Scalar(ScalarValue::String(desc.to_string())),
        ));
        d.fields
            .push(("precision".into(), PvField::Scalar(ScalarValue::Int(prec))));
        PvField::Structure(d)
    }

    /// Build a numeric `control` sub-structure with limitLow/limitHigh.
    fn nt_control(lo: f64, hi: f64) -> PvField {
        let mut c = PvStructure::new("");
        c.fields
            .push(("limitLow".into(), PvField::Scalar(ScalarValue::Double(lo))));
        c.fields
            .push(("limitHigh".into(), PvField::Scalar(ScalarValue::Double(hi))));
        PvField::Structure(c)
    }

    /// Build a `valueAlarm` sub-structure with the four limit fields.
    fn nt_value_alarm(lolo: f64, lo: f64, hi: f64, hihi: f64) -> PvField {
        let mut v = PvStructure::new("");
        v.fields.push((
            "lowAlarmLimit".into(),
            PvField::Scalar(ScalarValue::Double(lolo)),
        ));
        v.fields.push((
            "lowWarningLimit".into(),
            PvField::Scalar(ScalarValue::Double(lo)),
        ));
        v.fields.push((
            "highWarningLimit".into(),
            PvField::Scalar(ScalarValue::Double(hi)),
        ));
        v.fields.push((
            "highAlarmLimit".into(),
            PvField::Scalar(ScalarValue::Double(hihi)),
        ));
        PvField::Structure(v)
    }

    /// a pvalink must surface the linked PV's remote
    /// display / control / valueAlarm metadata, DBF type and element
    /// count through the DB-link metadata hook — the Rust counterpart
    /// of the pvxs pvalink lset metadata getters installed at
    /// `pvxs/ioc/pvalink_lset.cpp:700` and exercised by
    /// `pvxs/test/testpvalink.cpp:416`.
    ///
    /// The cached NT value carries the same metadata shape and the
    /// same numbers `testpvalink.cpp:416` asserts (graphic -9/9,
    /// control -10/10, alarm -8/-7/7/8, precision 2, units "arb",
    /// scalar element count 1). Pre-fix `PvaLink` had no
    /// `link_metadata` accessor and `LinkSet` had no metadata hook, so
    /// every one of these was invisible to DB link callers.
    #[test]
    fn br_r24_link_metadata_surfaces_remote_display_control_valuealarm() {
        use epics_base_rs::server::database::LinkDbfType;

        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        root.fields
            .push(("display".into(), nt_display(-9.0, 9.0, "arb", "linked", 2)));
        root.fields
            .push(("control".into(), nt_control(-10.0, 10.0)));
        root.fields
            .push(("valueAlarm".into(), nt_value_alarm(-8.0, -7.0, 7.0, 8.0)));

        let link = PvaLink::for_test(inp_cfg(SevrMode::Nms), Some(PvField::Structure(root)));
        let meta = link
            .link_metadata()
            .expect("connected link must expose metadata");

        assert_eq!(meta.dbf_type, Some(LinkDbfType::Double), "DBF type");
        assert_eq!(meta.element_count, Some(1), "scalar element count");
        assert_eq!(meta.graphic_limits, Some((-9.0, 9.0)), "graphic limits");
        assert_eq!(meta.control_limits, Some((-10.0, 10.0)), "control limits");
        assert_eq!(
            meta.alarm_limits,
            Some((-8.0, -7.0, 7.0, 8.0)),
            "alarm limits (lolo, lo, hi, hihi)"
        );
        assert_eq!(meta.precision, Some(2), "display precision");
        assert_eq!(meta.units.as_deref(), Some("arb"), "display units");
        assert_eq!(
            meta.description.as_deref(),
            Some("linked"),
            "display description"
        );
    }

    /// a not-yet-connected link (no cached value) reports no
    /// metadata — the record then keeps its local defaults. And an
    /// NTEnum value maps to `DBF_ENUM` with element count 1.
    #[test]
    fn br_r24_link_metadata_none_when_disconnected_and_enum_maps_to_dbf_enum() {
        use epics_base_rs::server::database::LinkDbfType;

        let disconnected = PvaLink::for_test(inp_cfg(SevrMode::Nms), None);
        assert!(
            disconnected.link_metadata().is_none(),
            "no cached value → no metadata snapshot"
        );

        // NTEnum: `value` is a struct with an integer `index` and a
        // `choices` string array → DBF_ENUM.
        let mut enum_value = PvStructure::new("enum_t");
        enum_value
            .fields
            .push(("index".into(), PvField::Scalar(ScalarValue::Int(1))));
        enum_value.fields.push((
            "choices".into(),
            PvField::ScalarArray(vec![
                ScalarValue::String("OFF".into()),
                ScalarValue::String("ON".into()),
            ]),
        ));
        let mut root = PvStructure::new("epics:nt/NTEnum:1.0");
        root.fields
            .push(("value".into(), PvField::Structure(enum_value)));
        let link = PvaLink::for_test(inp_cfg(SevrMode::Nms), Some(PvField::Structure(root)));
        let meta = link.link_metadata().expect("connected");
        assert_eq!(meta.dbf_type, Some(LinkDbfType::Enum), "NTEnum → DBF_ENUM");
        assert_eq!(meta.element_count, Some(1), "enum index element count");
    }

    /// pvalink OUT writes must carry proc/block/field options in
    /// the PUT pvRequest.
    ///
    /// On main: `build_put_request` did not exist — no pvRequest was built
    /// and `record._options.process` / `block` never reached the server.
    /// After fix: `build_put_request` produces the correct pvRequest, and
    /// `is_subfield` gates field-targeted vs. value-targeted dispatch.
    ///
    /// pvxs parity:
    ///   pvalink_channel.cpp:31-38 (putReq template)
    ///   pvalink_channel.cpp:220-263 (runtime process/block computation)
    ///   pvalink_channel.cpp:138 (field targeting via top[fieldName])
    #[test]
    fn br_r11_pvalink_out_options_preserved() {
        // proc=PP → process="true"
        let req = build_put_request(ProcMode::Pp, false);
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "process" && v == "true"),
            "proc=PP must produce process=true in pvRequest"
        );

        // proc=Default → process="passive" (distinct from NPP)
        let req = build_put_request(ProcMode::Default, false);
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "process" && v == "passive"),
            "proc=Default must produce process=passive in pvRequest"
        );

        // block=true must appear in pvRequest
        let req = build_put_request(ProcMode::Pp, true);
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "block" && v == "true"),
            "block=true must appear in pvRequest"
        );

        // block=false must appear in pvRequest
        let req = build_put_request(ProcMode::Default, false);
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "block" && v == "false"),
            "block=false must appear in pvRequest"
        );

        // field="" and field="value" are NOT sub-fields (use pvput_with_request)
        assert!(!is_subfield(""), "empty field is not a sub-field");
        assert!(!is_subfield("value"), "\"value\" is not a sub-field");

        // field="DESC" IS a sub-field (use pvput_field_with_request)
        assert!(
            is_subfield("DESC"),
            "\"DESC\" must be treated as a sub-field"
        );
        assert!(is_subfield("alarm.severity"), "dotted path is a sub-field");

        // A deferred write with field="DESC" and proc=PP queues the
        // value; when flushed the replay uses build_put_request.
        // Verify the defer path still enqueues (does not bypass).
        let cfg = PvaLinkConfig {
            field: "DESC".to_string(),
            proc: ProcMode::Pp,
            defer: true,
            ..PvaLinkConfig::defaults_for("BR11:PV", LinkDirection::Out)
        };
        let link = PvaLink::for_test(cfg, None);
        // Deferred write queues without hitting the network.
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                link.write_with_block("hello", true)
                    .await
                    .expect("deferred write_with_block must enqueue");
            });
        assert_eq!(link.pending_put_count(), 1, "one entry queued");
    }

    /// a typed (`PvField`) OUT write to a query-bearing link
    /// `pva://PV?field=<subfield>` must land the typed value on the
    /// selected sub-field, not on the root `value`. Pre-fix
    /// `write_pv_field` always routed through `pvput_pv_field_*`
    /// (root-targeted), so a typed array write clobbered `value` and
    /// left the requested sub-field untouched. pvxs `linkBuildPut`
    /// (`pvalink_channel.cpp:138`) targets `top[fieldName]`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mr_r4_typed_field_put_targets_subfield() {
        use epics_pva_rs::pvdata::{FieldDesc, ScalarType};
        use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

        // Structure PV with a root `value` array and an `aux` array
        // sub-field. The default SharedPV has no on_put handler, so
        // an inbound PUT lands directly in `current()`.
        let desc = FieldDesc::Structure {
            struct_id: "structure".into(),
            fields: vec![
                ("value".into(), FieldDesc::ScalarArray(ScalarType::Long)),
                ("aux".into(), FieldDesc::ScalarArray(ScalarType::Long)),
            ],
        };
        let initial = PvField::Structure(PvStructure {
            struct_id: "structure".into(),
            fields: vec![
                (
                    "value".into(),
                    PvField::ScalarArray(vec![ScalarValue::Long(1), ScalarValue::Long(2)]),
                ),
                (
                    "aux".into(),
                    PvField::ScalarArray(vec![ScalarValue::Long(7), ScalarValue::Long(8)]),
                ),
            ],
        });
        let pv = SharedPV::new();
        pv.open(desc, initial);
        let source = SharedSource::new();
        source.add("MR_R4:PV", pv.clone());
        let server = PvaServer::isolated(Arc::new(source)).expect("test PVA server must start");
        let addr = server.tcp_addr();

        let client = PvaClient::builder()
            .server_addr(addr)
            .timeout(Duration::from_secs(3))
            .build();
        let cfg = PvaLinkConfig {
            field: "aux".to_string(),
            ..PvaLinkConfig::defaults_for("MR_R4:PV", LinkDirection::Out)
        };
        let link = PvaLink::for_test_with_client(cfg, client);

        // Typed OUT write of a Long array to the `aux` sub-field.
        link.write_pv_field(&PvField::ScalarArray(vec![
            ScalarValue::Long(100),
            ScalarValue::Long(200),
            ScalarValue::Long(300),
        ]))
        .await
        .expect("typed field-targeted write must succeed");

        tokio::time::sleep(Duration::from_millis(80)).await;

        let current = pv.current().expect("PV has a current value");
        let PvField::Structure(s) = current else {
            panic!("expected structure value");
        };
        // Normalize an array field to `Vec<i64>` — the wire
        // round-trip may yield `ScalarArrayTyped`, logically equal to
        // a `ScalarArray` of the same elements.
        fn longs(field: &PvField) -> Vec<i64> {
            let scalars = match field {
                PvField::ScalarArray(v) => v.clone(),
                PvField::ScalarArrayTyped(t) => t.to_scalar_values(),
                other => panic!("expected an array field, got {other:?}"),
            };
            scalars
                .into_iter()
                .map(|sv| match sv {
                    ScalarValue::Long(x) => x,
                    other => panic!("expected Long element, got {other:?}"),
                })
                .collect()
        }

        let aux = s.get_field("aux").expect("aux sub-field present");
        assert_eq!(
            longs(aux),
            vec![100, 200, 300],
            "typed write must update the `aux` sub-field"
        );
        let value = s.get_field("value").expect("value sub-field present");
        assert_eq!(
            longs(value),
            vec![1, 2],
            "root `value` must be untouched by a field-targeted write"
        );
    }
}
