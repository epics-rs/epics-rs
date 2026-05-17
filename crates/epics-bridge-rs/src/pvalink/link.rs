//! `PvaLink` — a single live PVA link bound to a remote PV.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};

use super::config::{LinkDirection, PvaLinkConfig};

#[derive(Debug, thiserror::Error)]
pub enum PvaLinkError {
    #[error("PVA error: {0}")]
    Pva(#[from] epics_pva_rs::error::PvaError),
    #[error("link is INP-only, write requested")]
    NotWritable,
    #[error("link is OUT-only, read requested")]
    NotReadable,
    #[error("field {0:?} not found in remote NT structure")]
    FieldNotFound(String),
    #[error("field {0:?} is not a scalar")]
    NotScalar(String),
    #[error("link config parse error: {0}")]
    Config(#[from] super::config::PvaLinkParseError),
    #[error("retry queue full ({0} pending puts)")]
    RetryQueueFull(usize),
    #[error("local-only link {0:?} has no matching local record")]
    NotLocal(String),
}

pub type PvaLinkResult<T> = Result<T, PvaLinkError>;

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
    /// Receiver half of the INP-monitor record-notification channel.
    ///
    /// B3: every monitor event for an INP+monitor link pushes the new
    /// [`PvField`] onto this channel (the sender lives inside the
    /// spawned monitor task). [`Self::take_notify_rx`] hands the
    /// receiver to the resolver, which forwards events into
    /// `scan_on_update` / CP processing of the owning record. Wrapped
    /// in a `Mutex<Option<..>>` because the receiver is single-consumer
    /// and is moved out exactly once.
    notify_rx: Mutex<Option<mpsc::Receiver<PvField>>>,
    /// Deferred / retry Put queue (OUT links, B4 `defer` / `retry`).
    ///
    /// `defer=true` causes `write` to enqueue here instead of issuing
    /// the Put immediately; `flush_deferred` drains it. `retry=true`
    /// causes a Put that fails because the upstream is unreachable to
    /// be enqueued and replayed by `flush_deferred` on reconnect.
    /// Mirrors pvxs `pvaLink::put_queue` (pvalink_channel.cpp:147).
    put_queue: Mutex<Vec<PvField>>,
}

/// Upper bound on the OUT-side retry/defer queue. pvxs bounds the
/// retry queue implicitly by the monitor queue depth; we use a fixed
/// generous cap so a permanently-disconnected link cannot grow memory
/// without bound.
const MAX_PUT_QUEUE: usize = 1024;

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

        if matches!(config.direction, LinkDirection::Inp) && config.monitor {
            // B3 / B4-Q: the channel buffer is sized to the link's
            // `Q` (monitor queue depth) so a slow record-side
            // consumer back-pressures rather than silently dropping
            // events. `try_send` below still tolerates a full
            // channel (the `latest` cache is authoritative for the
            // value itself; the channel only drives scan-on-update).
            let (tx, rx) = mpsc::channel::<PvField>(config.queue_size.max(1));
            notify_rx = Some(rx);

            let pv_name = config.pv_name.clone();
            let latest_clone = latest.clone();
            let client_clone = client.clone();
            // B4-pipeline / B4-Q: when the link asks for pipeline
            // flow-control or a non-default queue depth, build a
            // pvRequest carrying `record[pipeline=...,queueSize=N]`
            // so the negotiation reaches the server. Otherwise use
            // the plain monitor (lower overhead, matches prior
            // behaviour).
            let request = monitor_request(&config);
            let join = tokio::spawn(async move {
                match request {
                    Some(req) => {
                        let _ = client_clone
                            .pvmonitor_with_request(&pv_name, &req, |value| {
                                *latest_clone.lock() = Some(value.clone());
                                let _ = tx.try_send(value.clone());
                            })
                            .await;
                    }
                    None => {
                        let _ = client_clone
                            .pvmonitor(&pv_name, |value| {
                                *latest_clone.lock() = Some(value.clone());
                                let _ = tx.try_send(value.clone());
                            })
                            .await;
                    }
                }
            });
            monitor_abort = Some(MonitorAbort(join.abort_handle()));
        }

        Ok(Self {
            config,
            client,
            latest,
            notify_rx: Mutex::new(notify_rx),
            put_queue: Mutex::new(Vec::new()),
            _monitor_abort: monitor_abort,
        })
    }

    /// Take the INP-monitor notification receiver (B3). Returns the
    /// channel exactly once; subsequent calls return `None`. The
    /// resolver calls this right after `open` to spawn the
    /// scan-on-update forwarder. `None` for OUT / non-monitor links
    /// (they never created a channel) or after the receiver has
    /// already been claimed.
    pub fn take_notify_rx(&self) -> Option<mpsc::Receiver<PvField>> {
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
        if matches!(self.config.direction, LinkDirection::Out) {
            return Err(PvaLinkError::NotReadable);
        }
        if self.config.monitor
            && let Some(v) = self.latest.lock().clone()
        {
            return Ok(extract_field(&v, &self.config.field));
        }
        let result = self.client.pvget_full(&self.config.pv_name).await?;
        Ok(extract_field(&result.value, &self.config.field))
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
        if matches!(self.config.direction, LinkDirection::Out) || !self.config.monitor {
            return None;
        }
        let v = self.latest.lock().clone()?;
        Some(extract_field(&v, &self.config.field))
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
    pub async fn write(&self, value_str: &str) -> PvaLinkResult<()> {
        if matches!(self.config.direction, LinkDirection::Inp) {
            return Err(PvaLinkError::NotWritable);
        }
        // String form: parse into a typed PvField scalar so the
        // defer / retry queue is value-typed and uniform with
        // `write_pv_field`. A bare scalar is the common case for the
        // string path.
        let field = PvField::Scalar(ScalarValue::String(value_str.to_string()));
        if self.config.defer {
            return self.enqueue_put(field);
        }
        match self.client.pvput(&self.config.pv_name, value_str).await {
            Ok(()) => Ok(()),
            Err(e) if self.config.retry && is_disconnect(&e) => self.enqueue_put(field),
            Err(e) => Err(PvaLinkError::Pva(e)),
        }
    }

    /// Write a typed `PvField` directly (no string round-trip). For
    /// large arrays this avoids the O(N) `Display` allocation +
    /// O(N) pvput parse-back that `write(value_str)` triggers.
    /// Used by the pvalink OUT path on EpicsValue array variants.
    ///
    /// B4: same `defer` / `retry` semantics as [`Self::write`].
    pub async fn write_pv_field(&self, value: &PvField) -> PvaLinkResult<()> {
        if matches!(self.config.direction, LinkDirection::Inp) {
            return Err(PvaLinkError::NotWritable);
        }
        if self.config.defer {
            return self.enqueue_put(value.clone());
        }
        match self
            .client
            .pvput_pv_field(&self.config.pv_name, value)
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if self.config.retry && is_disconnect(&e) => self.enqueue_put(value.clone()),
            Err(e) => Err(PvaLinkError::Pva(e)),
        }
    }

    /// Push `value` onto the deferred / retry Put queue (B4). Returns
    /// `RetryQueueFull` once the queue hits [`MAX_PUT_QUEUE`].
    fn enqueue_put(&self, value: PvField) -> PvaLinkResult<()> {
        let mut q = self.put_queue.lock();
        if q.len() >= MAX_PUT_QUEUE {
            return Err(PvaLinkError::RetryQueueFull(q.len()));
        }
        q.push(value);
        Ok(())
    }

    /// Number of Puts currently held in the defer / retry queue (B4).
    pub fn pending_put_count(&self) -> usize {
        self.put_queue.lock().len()
    }

    /// Flush every queued Put to the upstream PV in FIFO order (B4).
    ///
    /// Called for `defer` links to issue the queued value, and for
    /// `retry` links once the upstream reconnects. On a disconnect
    /// error for a `retry` link the still-unsent values are restored
    /// to the front of the queue so a later flush retries them;
    /// non-disconnect errors are surfaced and the offending value is
    /// dropped (it would fail identically on every retry). Returns
    /// the number of Puts successfully issued. Mirrors pvxs
    /// `pvaLinkChannel::run` draining `put_queue` (pvalink_channel.cpp).
    pub async fn flush_deferred(&self) -> PvaLinkResult<usize> {
        if matches!(self.config.direction, LinkDirection::Inp) {
            return Err(PvaLinkError::NotWritable);
        }
        let queued: Vec<PvField> = std::mem::take(&mut *self.put_queue.lock());
        let mut sent = 0usize;
        for (idx, value) in queued.iter().enumerate() {
            match self
                .client
                .pvput_pv_field(&self.config.pv_name, value)
                .await
            {
                Ok(()) => sent += 1,
                Err(e) if self.config.retry && is_disconnect(&e) => {
                    // Still disconnected — restore the unsent tail
                    // (including the current value) to the front so
                    // a later flush picks up where we left off.
                    let mut q = self.put_queue.lock();
                    let mut tail: Vec<PvField> = queued[idx..].to_vec();
                    tail.append(&mut q);
                    *q = tail;
                    return Err(PvaLinkError::Pva(e));
                }
                Err(e) => {
                    // Non-retry hard error: the offending value (idx)
                    // would fail identically on every retry, so drop
                    // only it — restore the still-unsent tail
                    // (`idx+1..`) so a later flush replays the values
                    // queued behind the failure. The queue was already
                    // `mem::take`-emptied, so without this the entire
                    // tail is silently lost.
                    if idx + 1 < queued.len() {
                        let mut q = self.put_queue.lock();
                        let mut tail: Vec<PvField> = queued[idx + 1..].to_vec();
                        tail.append(&mut q);
                        *q = tail;
                    }
                    return Err(PvaLinkError::Pva(e));
                }
            }
        }
        Ok(sent)
    }

    /// True when the link's monitor has received at least one update
    /// (i.e., the upstream PV is reachable and has emitted a value).
    /// Mirrors pvxs `pvaIsConnected` (pvalink_lset.cpp:186).
    pub fn is_connected(&self) -> bool {
        self.latest.lock().is_some()
    }

    /// Raw remote NT `alarm.severity` of the latest cached value, in
    /// EPICS severity numbering (`0 = NO_ALARM` … `3 = INVALID`).
    /// `None` when no value is cached or the structure carries no
    /// alarm sub-field.
    fn remote_alarm_severity(&self) -> Option<i32> {
        let v = self.latest.lock().clone()?;
        let PvField::Structure(s) = v else {
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
        let sev = self.remote_alarm_severity()?;
        if self.config.sevr.propagates(sev) {
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
        // Severity gate first — NMS / sub-threshold links report
        // nothing.
        let sev = self.link_alarm_severity()?;
        let v = self.latest.lock().clone()?;
        let PvField::Structure(s) = v else {
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

    /// Latest `(seconds, nanoseconds)` from the NT timeStamp slot, if
    /// the cached value carries one. Mirrors pvxs
    /// `pvaGetTimeStampTag`.
    pub fn time_stamp(&self) -> Option<(i64, i32)> {
        let v = self.latest.lock().clone()?;
        let PvField::Structure(s) = v else {
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
        Some((secs, nsec))
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
            notify_rx: Mutex::new(None),
            put_queue: Mutex::new(Vec::new()),
        }
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
        | PvaError::ConnectionRefused => true,
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
    }
}

/// Build the pvRequest for an INP+monitor link when its options
/// require server-side negotiation (B4 `Q` / `pipeline`).
///
/// Returns `None` for the default monitor (no pipeline, default
/// queue depth) so the plain `pvmonitor` fast path is kept. When a
/// request is built it carries `record[pipeline=...,queueSize=N]`,
/// which `epics-pva-rs` re-sends on every reconnect, mirroring pvxs
/// `pvaLink::makeRequest` (pvalink_link.cpp:47).
fn monitor_request(config: &PvaLinkConfig) -> Option<epics_pva_rs::pv_request::PvRequestExpr> {
    use super::config::DEFAULT_QUEUE_SIZE;
    let needs_pipeline = config.pipeline;
    let needs_queue = config.queue_size != DEFAULT_QUEUE_SIZE;
    if !needs_pipeline && !needs_queue {
        return None;
    }
    let mut req = epics_pva_rs::pv_request::PvRequestExpr::default();
    if needs_pipeline {
        req.record_options
            .push(("pipeline".to_string(), "true".to_string()));
    }
    // Always carry queueSize alongside pipeline (pvxs sends both in
    // `makeRequest`); also carry it on its own when a non-default Q
    // was requested.
    req.record_options.push((
        "queueSize".to_string(),
        config.queue_size.max(1).to_string(),
    ));
    Some(req)
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

    #[test]
    fn b4_monitor_request_none_for_defaults() {
        let cfg = PvaLinkConfig::defaults_for("X", LinkDirection::Inp);
        assert!(monitor_request(&cfg).is_none());
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
    async fn b4_defer_queues_instead_of_putting() {
        let link = PvaLink::for_test(out_cfg(true, false), None);
        assert_eq!(link.pending_put_count(), 0);
        // defer=true: write enqueues, returns Ok without a server.
        link.write("42").await.expect("deferred write is Ok");
        assert_eq!(link.pending_put_count(), 1);
        link.write_pv_field(&PvField::Scalar(ScalarValue::Double(1.0)))
            .await
            .expect("deferred typed write is Ok");
        assert_eq!(link.pending_put_count(), 2);
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
    async fn b4_retry_queue_full_rejects() {
        let link = PvaLink::for_test(out_cfg(true, false), None);
        for _ in 0..MAX_PUT_QUEUE {
            link.write("1").await.expect("within capacity");
        }
        assert_eq!(link.pending_put_count(), MAX_PUT_QUEUE);
        let overflow = link.write("1").await;
        assert!(matches!(overflow, Err(PvaLinkError::RetryQueueFull(_))));
    }

    #[tokio::test]
    async fn b4_flush_deferred_replays_when_still_disconnected() {
        // defer link, retry=false; flush against no server. The first
        // value's Put fails with a hard error → that one value is
        // dropped, but the still-unsent tail (`idx+1..`) is restored so
        // a later flush can replay it. Without the tail restore the
        // whole queue would be silently lost to the `mem::take`.
        let link = PvaLink::for_test(out_cfg(true, false), None);
        link.write("1").await.unwrap();
        link.write("2").await.unwrap();
        assert_eq!(link.pending_put_count(), 2);
        let r = link.flush_deferred().await;
        assert!(r.is_err());
        // Only the failing entry ("1") was dropped; "2" stays queued.
        assert_eq!(link.pending_put_count(), 1);
    }

    #[tokio::test]
    async fn b4_flush_deferred_retry_restores_unsent_tail() {
        // defer + retry: flush against no server → all values are
        // restored to the queue for a later retry.
        let link = PvaLink::for_test(out_cfg(true, true), None);
        link.write("1").await.unwrap();
        link.write("2").await.unwrap();
        let r = link.flush_deferred().await;
        assert!(r.is_err(), "still disconnected");
        // retry restores the unsent tail (both values).
        assert_eq!(link.pending_put_count(), 2);
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
}
