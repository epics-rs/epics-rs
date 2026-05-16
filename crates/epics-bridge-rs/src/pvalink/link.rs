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
    /// Subscriber channel for record-side notification (INP monitor mode).
    #[allow(dead_code)]
    notify_tx: Option<mpsc::Sender<PvField>>,
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
        let mut notify_tx = None;
        let mut monitor_abort = None;

        if matches!(config.direction, LinkDirection::Inp) && config.monitor {
            let (tx, _rx) = mpsc::channel::<PvField>(64);
            notify_tx = Some(tx.clone());

            let pv_name = config.pv_name.clone();
            let latest_clone = latest.clone();
            let client_clone = client.clone();
            let join = tokio::spawn(async move {
                let _ = client_clone
                    .pvmonitor(&pv_name, |value| {
                        *latest_clone.lock() = Some(value.clone());
                        let _ = tx.try_send(value.clone());
                    })
                    .await;
            });
            monitor_abort = Some(MonitorAbort(join.abort_handle()));
        }

        Ok(Self {
            config,
            client,
            latest,
            notify_tx,
            _monitor_abort: monitor_abort,
        })
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
    pub async fn write(&self, value_str: &str) -> PvaLinkResult<()> {
        if matches!(self.config.direction, LinkDirection::Inp) {
            return Err(PvaLinkError::NotWritable);
        }
        self.client.pvput(&self.config.pv_name, value_str).await?;
        Ok(())
    }

    /// Write a typed `PvField` directly (no string round-trip). For
    /// large arrays this avoids the O(N) `Display` allocation +
    /// O(N) pvput parse-back that `write(value_str)` triggers.
    /// Used by the pvalink OUT path on EpicsValue array variants.
    pub async fn write_pv_field(&self, value: &PvField) -> PvaLinkResult<()> {
        if matches!(self.config.direction, LinkDirection::Inp) {
            return Err(PvaLinkError::NotWritable);
        }
        self.client
            .pvput_pv_field(&self.config.pv_name, value)
            .await?;
        Ok(())
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
    /// `alarm_message`, `try_read_cached`) without standing up a PVA
    /// server.
    #[cfg(test)]
    pub(crate) fn for_test(config: PvaLinkConfig, cached: Option<PvField>) -> Self {
        let client = PvaClient::builder().timeout(Duration::from_secs(1)).build();
        Self {
            _monitor_abort: None,
            config,
            client,
            latest: Arc::new(Mutex::new(cached)),
            notify_tx: None,
        }
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

    use super::super::config::{LinkDirection, PvaLinkConfig, SevrMode};

    /// Build an NTScalar-shaped structure with an `alarm.severity`
    /// (and optional `alarm.message`).
    fn nt_with_alarm(severity: i32, message: Option<&str>) -> PvField {
        let mut alarm = PvStructure::new("alarm_t");
        alarm
            .fields
            .push(("severity".into(), PvField::Scalar(ScalarValue::Int(severity))));
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
            let link = PvaLink::for_test(inp_cfg(SevrMode::Nms), Some(nt_with_alarm(sev, Some("bad"))));
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
            let link = PvaLink::for_test(inp_cfg(SevrMode::Ms), Some(nt_with_alarm(sev, Some("oops"))));
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
        let invalid = PvaLink::for_test(inp_cfg(SevrMode::Msi), Some(nt_with_alarm(3, Some("dead"))));
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
}
