//! [`ChannelSource`] implementation backed by an epics-rs [`PvDatabase`].
//!
//! Replaces the spvirit-shaped `bridge.rs::PvDatabaseStore`. Builds NTScalar
//! and NTScalarArray `PvField` values directly from `Snapshot`s, with full
//! alarm/timeStamp/display metadata.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::client_native::context::PvGetResult; // not used; kept for re-export hygiene
use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use crate::server_native::ChannelSource;

use epics_base_rs::server::access_security::{AccessLevel, AccessSecurityConfig};
use epics_base_rs::server::database::{PvDatabase, PvEntry, parse_pv_name};
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::EpicsValue;

/// Native `ChannelSource` over a `PvDatabase`.
pub struct PvDatabaseSource {
    db: Arc<PvDatabase>,
    /// Optional Access Security configuration. When set, every PUT
    /// is gated through `check_access_method` using the connecting
    /// peer's host/account/method (PVA `ChannelContext`). When None,
    /// PUTs are unrestricted (legacy behaviour for callers that
    /// build `PvDatabaseSource::new` without an ACF).
    acf: Arc<Option<AccessSecurityConfig>>,
}

impl PvDatabaseSource {
    pub fn new(db: Arc<PvDatabase>) -> Self {
        Self {
            db,
            acf: Arc::new(None),
        }
    }

    /// Build with ACF enforcement. Mirrors what `PvaServer::run`
    /// installs from its builder-supplied ACF — every PUT goes
    /// through `check_access_method` against the record's `ASG`.
    pub fn new_with_acf(db: Arc<PvDatabase>, acf: Arc<Option<AccessSecurityConfig>>) -> Self {
        Self { db, acf }
    }

    pub fn database(&self) -> &Arc<PvDatabase> {
        &self.db
    }

    /// Resolve the ASG of the record backing `pv_name` (the part
    /// before the optional `.FIELD` suffix). Returns "DEFAULT" for
    /// simple PVs (no record-level ASG) or for unknown names; the
    /// ACF defaults its `DEFAULT` ASG when no rule matches.
    async fn resolve_asg(&self, pv_name: &str) -> String {
        let (base, _field) = parse_pv_name(pv_name);
        if let Some(rec) = self.db.get_record(base).await {
            let inst = rec.read().await;
            if !inst.common.asg.is_empty() {
                return inst.common.asg.clone();
            }
        }
        "DEFAULT".to_string()
    }
}

// ── EpicsValue → PvField (NTScalar / NTScalarArray) ─────────────────────

fn snapshot_to_pv_field(snap: &Snapshot) -> PvField {
    let value_field = match &snap.value {
        EpicsValue::Double(v) => PvField::Scalar(ScalarValue::Double(*v)),
        EpicsValue::Float(v) => PvField::Scalar(ScalarValue::Float(*v)),
        EpicsValue::Long(v) => PvField::Scalar(ScalarValue::Int(*v)),
        EpicsValue::Short(v) => PvField::Scalar(ScalarValue::Short(*v)),
        EpicsValue::Char(v) => PvField::Scalar(ScalarValue::UByte(*v)),
        EpicsValue::Enum(v) => PvField::Scalar(ScalarValue::Int(*v as i32)),
        EpicsValue::String(s) => PvField::Scalar(ScalarValue::String(s.clone())),
        EpicsValue::Int64(v) => PvField::Scalar(ScalarValue::Long(*v)),
        EpicsValue::DoubleArray(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::Double(*x)).collect())
        }
        EpicsValue::FloatArray(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::Float(*x)).collect())
        }
        EpicsValue::LongArray(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::Int(*x)).collect())
        }
        EpicsValue::ShortArray(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::Short(*x)).collect())
        }
        EpicsValue::CharArray(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::UByte(*x)).collect())
        }
        EpicsValue::EnumArray(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::Int(*x as i32)).collect())
        }
        EpicsValue::StringArray(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::String(x.clone())).collect())
        }
        EpicsValue::Int64Array(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::Long(*x)).collect())
        }
    };

    let is_array = matches!(value_field, PvField::ScalarArray(_));
    let struct_id = if is_array {
        "epics:nt/NTScalarArray:1.0"
    } else {
        "epics:nt/NTScalar:1.0"
    };

    let mut s = PvStructure::new(struct_id);
    s.fields.push(("value".into(), value_field));
    s.fields.push(("alarm".into(), build_alarm(snap)));
    s.fields.push(("timeStamp".into(), build_timestamp(snap)));
    s.fields.push(("display".into(), build_display(snap)));
    if !is_array {
        s.fields.push(("control".into(), build_control(snap)));
        s.fields
            .push(("valueAlarm".into(), build_value_alarm(snap)));
    }
    PvField::Structure(s)
}

fn snapshot_to_field_desc(snap: &Snapshot) -> FieldDesc {
    let (value_desc, is_array) = match &snap.value {
        EpicsValue::Double(_) => (FieldDesc::Scalar(ScalarType::Double), false),
        EpicsValue::Float(_) => (FieldDesc::Scalar(ScalarType::Float), false),
        EpicsValue::Long(_) => (FieldDesc::Scalar(ScalarType::Int), false),
        EpicsValue::Short(_) => (FieldDesc::Scalar(ScalarType::Short), false),
        EpicsValue::Char(_) => (FieldDesc::Scalar(ScalarType::UByte), false),
        EpicsValue::Enum(_) => (FieldDesc::Scalar(ScalarType::Int), false),
        EpicsValue::String(_) => (FieldDesc::Scalar(ScalarType::String), false),
        EpicsValue::Int64(_) => (FieldDesc::Scalar(ScalarType::Long), false),
        EpicsValue::DoubleArray(_) => (FieldDesc::ScalarArray(ScalarType::Double), true),
        EpicsValue::FloatArray(_) => (FieldDesc::ScalarArray(ScalarType::Float), true),
        EpicsValue::LongArray(_) => (FieldDesc::ScalarArray(ScalarType::Int), true),
        EpicsValue::ShortArray(_) => (FieldDesc::ScalarArray(ScalarType::Short), true),
        EpicsValue::CharArray(_) => (FieldDesc::ScalarArray(ScalarType::UByte), true),
        EpicsValue::EnumArray(_) => (FieldDesc::ScalarArray(ScalarType::Int), true),
        EpicsValue::StringArray(_) => (FieldDesc::ScalarArray(ScalarType::String), true),
        EpicsValue::Int64Array(_) => (FieldDesc::ScalarArray(ScalarType::Long), true),
    };
    let struct_id = if is_array {
        "epics:nt/NTScalarArray:1.0"
    } else {
        "epics:nt/NTScalar:1.0"
    };
    let mut fields = vec![
        ("value".to_string(), value_desc),
        ("alarm".into(), alarm_desc()),
        ("timeStamp".into(), timestamp_desc()),
        ("display".into(), display_desc()),
    ];
    if !is_array {
        fields.push(("control".into(), control_desc()));
        fields.push(("valueAlarm".into(), value_alarm_desc()));
    }
    FieldDesc::Structure {
        struct_id: struct_id.into(),
        fields,
    }
}

fn alarm_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "alarm_t".into(),
        fields: vec![
            ("severity".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("status".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("message".into(), FieldDesc::Scalar(ScalarType::String)),
        ],
    }
}

fn timestamp_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "time_t".into(),
        fields: vec![
            (
                "secondsPastEpoch".into(),
                FieldDesc::Scalar(ScalarType::Long),
            ),
            ("nanoseconds".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("userTag".into(), FieldDesc::Scalar(ScalarType::Int)),
        ],
    }
}

fn display_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "display_t".into(),
        fields: vec![
            ("limitLow".into(), FieldDesc::Scalar(ScalarType::Double)),
            ("limitHigh".into(), FieldDesc::Scalar(ScalarType::Double)),
            ("description".into(), FieldDesc::Scalar(ScalarType::String)),
            ("units".into(), FieldDesc::Scalar(ScalarType::String)),
            ("precision".into(), FieldDesc::Scalar(ScalarType::Int)),
        ],
    }
}

fn control_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "control_t".into(),
        fields: vec![
            ("limitLow".into(), FieldDesc::Scalar(ScalarType::Double)),
            ("limitHigh".into(), FieldDesc::Scalar(ScalarType::Double)),
            ("minStep".into(), FieldDesc::Scalar(ScalarType::Double)),
        ],
    }
}

fn value_alarm_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "valueAlarm_t".into(),
        fields: vec![
            ("active".into(), FieldDesc::Scalar(ScalarType::Boolean)),
            (
                "lowAlarmLimit".into(),
                FieldDesc::Scalar(ScalarType::Double),
            ),
            (
                "lowWarningLimit".into(),
                FieldDesc::Scalar(ScalarType::Double),
            ),
            (
                "highWarningLimit".into(),
                FieldDesc::Scalar(ScalarType::Double),
            ),
            (
                "highAlarmLimit".into(),
                FieldDesc::Scalar(ScalarType::Double),
            ),
            (
                "lowAlarmSeverity".into(),
                FieldDesc::Scalar(ScalarType::Int),
            ),
            (
                "lowWarningSeverity".into(),
                FieldDesc::Scalar(ScalarType::Int),
            ),
            (
                "highWarningSeverity".into(),
                FieldDesc::Scalar(ScalarType::Int),
            ),
            (
                "highAlarmSeverity".into(),
                FieldDesc::Scalar(ScalarType::Int),
            ),
            ("hysteresis".into(), FieldDesc::Scalar(ScalarType::UByte)),
        ],
    }
}

fn build_alarm(snap: &Snapshot) -> PvField {
    let mut a = PvStructure::new("alarm_t");
    a.fields.push((
        "severity".into(),
        PvField::Scalar(ScalarValue::Int(snap.alarm.severity as i32)),
    ));
    a.fields.push((
        "status".into(),
        PvField::Scalar(ScalarValue::Int(snap.alarm.status as i32)),
    ));
    a.fields.push((
        "message".into(),
        PvField::Scalar(ScalarValue::String(String::new())),
    ));
    PvField::Structure(a)
}

fn build_timestamp(_snap: &Snapshot) -> PvField {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut t = PvStructure::new("time_t");
    t.fields.push((
        "secondsPastEpoch".into(),
        PvField::Scalar(ScalarValue::Long(now.as_secs() as i64)),
    ));
    t.fields.push((
        "nanoseconds".into(),
        PvField::Scalar(ScalarValue::Int(now.subsec_nanos() as i32)),
    ));
    t.fields
        .push(("userTag".into(), PvField::Scalar(ScalarValue::Int(0))));
    PvField::Structure(t)
}

fn build_display(snap: &Snapshot) -> PvField {
    let mut d = PvStructure::new("display_t");
    let (lo, hi, desc, units, prec) = if let Some(disp) = &snap.display {
        (
            disp.lower_disp_limit,
            disp.upper_disp_limit,
            String::new(),
            disp.units.clone(),
            disp.precision as i32,
        )
    } else {
        (0.0, 0.0, String::new(), String::new(), 0)
    };
    d.fields
        .push(("limitLow".into(), PvField::Scalar(ScalarValue::Double(lo))));
    d.fields
        .push(("limitHigh".into(), PvField::Scalar(ScalarValue::Double(hi))));
    d.fields.push((
        "description".into(),
        PvField::Scalar(ScalarValue::String(desc)),
    ));
    d.fields
        .push(("units".into(), PvField::Scalar(ScalarValue::String(units))));
    d.fields
        .push(("precision".into(), PvField::Scalar(ScalarValue::Int(prec))));
    PvField::Structure(d)
}

fn build_control(snap: &Snapshot) -> PvField {
    let mut c = PvStructure::new("control_t");
    let (lo, hi) = if let Some(ctrl) = &snap.control {
        (ctrl.lower_ctrl_limit, ctrl.upper_ctrl_limit)
    } else {
        (0.0, 0.0)
    };
    c.fields
        .push(("limitLow".into(), PvField::Scalar(ScalarValue::Double(lo))));
    c.fields
        .push(("limitHigh".into(), PvField::Scalar(ScalarValue::Double(hi))));
    c.fields
        .push(("minStep".into(), PvField::Scalar(ScalarValue::Double(0.0))));
    PvField::Structure(c)
}

fn build_value_alarm(_snap: &Snapshot) -> PvField {
    let mut v = PvStructure::new("valueAlarm_t");
    v.fields.push((
        "active".into(),
        PvField::Scalar(ScalarValue::Boolean(false)),
    ));
    for name in [
        "lowAlarmLimit",
        "lowWarningLimit",
        "highWarningLimit",
        "highAlarmLimit",
    ] {
        v.fields
            .push((name.into(), PvField::Scalar(ScalarValue::Double(0.0))));
    }
    for name in [
        "lowAlarmSeverity",
        "lowWarningSeverity",
        "highWarningSeverity",
        "highAlarmSeverity",
    ] {
        v.fields
            .push((name.into(), PvField::Scalar(ScalarValue::Int(0))));
    }
    v.fields
        .push(("hysteresis".into(), PvField::Scalar(ScalarValue::UByte(0))));
    PvField::Structure(v)
}

// ── ChannelSource impl ────────────────────────────────────────────────────

async fn snapshot_for(db: &PvDatabase, name: &str) -> Option<Snapshot> {
    let (_base, field) = parse_pv_name(name);
    match db.find_entry(name).await? {
        PvEntry::Simple(pv) => Some(pv.snapshot().await),
        PvEntry::Record(rec) => {
            let inst = rec.read().await;
            inst.snapshot_for_field(field)
        }
    }
}

impl ChannelSource for PvDatabaseSource {
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        let db = self.db.clone();
        async move {
            let mut names = db.all_record_names().await;
            names.extend(db.all_simple_pv_names().await);
            // Aliases are independently addressable channel names —
            // a PVA client doing channelList must see them so it can
            // connect by alias. has_name and find_entry already
            // resolve aliases on the server side (round 7).
            names.extend(db.all_alias_names().await);
            names
        }
    }

    fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let db = self.db.clone();
        let name = name.to_string();
        async move { db.has_name(&name).await }
    }

    fn get_introspection(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        let db = self.db.clone();
        let name = name.to_string();
        async move {
            let snap = snapshot_for(&db, &name).await?;
            Some(snapshot_to_field_desc(&snap))
        }
    }

    fn get_value(&self, name: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let db = self.db.clone();
        let name = name.to_string();
        async move {
            let snap = snapshot_for(&db, &name).await?;
            Some(snapshot_to_pv_field(&snap))
        }
    }

    fn put_value(
        &self,
        name: &str,
        value: PvField,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let db = self.db.clone();
        let name = name.to_string();
        async move {
            // Extract the inner value field (NTScalar.value or top-level scalar).
            let scalar = match &value {
                PvField::Structure(s) => s.get_field("value").cloned(),
                _ => Some(value),
            };
            let scalar = scalar.ok_or_else(|| "PUT missing 'value' field".to_string())?;
            let epics = pv_field_to_epics(&scalar)
                .ok_or_else(|| "PUT value not representable as EpicsValue".to_string())?;
            db.put_pv(&name, epics).await.map_err(|e| e.to_string())
        }
    }

    fn put_value_ctx(
        &self,
        name: &str,
        value: PvField,
        ctx: crate::server_native::ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let db = self.db.clone();
        let acf = self.acf.clone();
        let name_s = name.to_string();
        // Look up the record's ASG eagerly so the async block below
        // doesn't have to borrow `&self`.
        let resolve_fut = self.resolve_asg(name);
        async move {
            // ACF gate. Skipped when no AccessSecurityConfig is
            // attached (legacy / opt-in behaviour for tests + simple
            // tools); otherwise the PUT is denied unless the peer's
            // (host, account, method) yields ReadWrite for the
            // record's ASG.
            if let Some(ref cfg) = *acf {
                let asg = resolve_fut.await;
                let level = cfg.check_access_method(
                    &asg,
                    &ctx.host,
                    &ctx.account,
                    0, // record-level ASL not threaded through PVA layer yet
                    &ctx.method,
                    "", // authority (cert issuer) not surfaced via PVA wire today
                );
                if level != AccessLevel::ReadWrite {
                    return Err(format!(
                        "PUT denied by access security: ASG '{asg}' from {host}/{account}/{method}",
                        host = ctx.host,
                        account = ctx.account,
                        method = ctx.method,
                    ));
                }
            }

            // Reuse the standard PUT path for the actual write —
            // identical to `put_value` so behaviour stays in sync
            // when ACF is disabled.
            let scalar = match &value {
                PvField::Structure(s) => s.get_field("value").cloned(),
                _ => Some(value),
            };
            let scalar = scalar.ok_or_else(|| "PUT missing 'value' field".to_string())?;
            let epics = pv_field_to_epics(&scalar)
                .ok_or_else(|| "PUT value not representable as EpicsValue".to_string())?;
            db.put_pv(&name_s, epics).await.map_err(|e| e.to_string())
        }
    }

    fn is_writable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let db = self.db.clone();
        let name = name.to_string();
        async move { db.has_name(&name).await }
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
        let db = self.db.clone();
        let name = name.to_string();
        async move {
            let (tx, rx) = mpsc::channel::<PvField>(64);
            let entry = db.find_entry(&name).await?;
            match entry {
                PvEntry::Simple(pv) => {
                    // Simple PVs don't expose change-streams; emit one
                    // initial snapshot. Subsequent puts won't be observed by
                    // the monitor — accept this limitation for v1.
                    let snap = pv.snapshot().await;
                    let pv = snapshot_to_pv_field(&snap);
                    let _ = tx.send(pv).await;
                }
                PvEntry::Record(_rec) => {
                    // Subscribe via the public DbSubscription API.
                    use epics_base_rs::server::database::db_access::DbSubscription;
                    let mut sub = match DbSubscription::subscribe(&db, &name).await {
                        Some(s) => s,
                        None => return None,
                    };
                    tokio::spawn(async move {
                        while let Some(snap) = sub.recv_snapshot().await {
                            let pv = snapshot_to_pv_field(&snap);
                            if tx.send(pv).await.is_err() {
                                break;
                            }
                        }
                    });
                }
            }
            Some(rx)
        }
    }
}

// ── PvField → EpicsValue (PUT path) ────────────────────────────────────

fn pv_field_to_epics(field: &PvField) -> Option<EpicsValue> {
    match field {
        PvField::Scalar(sv) => Some(scalar_to_epics(sv)),
        PvField::ScalarArray(items) if !items.is_empty() => match &items[0] {
            ScalarValue::Double(_) => Some(EpicsValue::DoubleArray(
                items
                    .iter()
                    .filter_map(|v| match v {
                        ScalarValue::Double(x) => Some(*x),
                        _ => None,
                    })
                    .collect(),
            )),
            ScalarValue::Int(_) => Some(EpicsValue::LongArray(
                items
                    .iter()
                    .filter_map(|v| match v {
                        ScalarValue::Int(x) => Some(*x),
                        _ => None,
                    })
                    .collect(),
            )),
            ScalarValue::Long(_) => Some(EpicsValue::Int64Array(
                items
                    .iter()
                    .filter_map(|v| match v {
                        ScalarValue::Long(x) => Some(*x),
                        _ => None,
                    })
                    .collect(),
            )),
            ScalarValue::Float(_) => Some(EpicsValue::FloatArray(
                items
                    .iter()
                    .filter_map(|v| match v {
                        ScalarValue::Float(x) => Some(*x),
                        _ => None,
                    })
                    .collect(),
            )),
            _ => None,
        },
        _ => None,
    }
}

fn scalar_to_epics(v: &ScalarValue) -> EpicsValue {
    match v {
        ScalarValue::Boolean(b) => EpicsValue::Enum(if *b { 1 } else { 0 }),
        ScalarValue::Byte(x) => EpicsValue::Char(*x as u8),
        ScalarValue::Short(x) => EpicsValue::Short(*x),
        ScalarValue::Int(x) => EpicsValue::Long(*x),
        ScalarValue::Long(x) => EpicsValue::Int64(*x),
        ScalarValue::UByte(x) => EpicsValue::Char(*x),
        ScalarValue::UShort(x) => EpicsValue::Enum(*x),
        ScalarValue::UInt(x) => EpicsValue::Long(*x as i32),
        ScalarValue::ULong(x) => EpicsValue::Long(*x as i32),
        ScalarValue::Float(x) => EpicsValue::Float(*x),
        ScalarValue::Double(x) => EpicsValue::Double(*x),
        ScalarValue::String(s) => EpicsValue::String(s.clone()),
    }
}

#[allow(unused_imports)]
use crate::error::PvaError;
#[allow(unused_imports)]
type _Pvr = PvGetResult;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_native::ChannelContext;
    use epics_base_rs::server::access_security::parse_acf;

    fn make_ctx(host: &str, account: &str, method: &str) -> ChannelContext {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        ChannelContext {
            peer: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0),
            account: account.to_string(),
            method: method.to_string(),
            host: host.to_string(),
        }
    }

    fn pv_double(v: f64) -> PvField {
        // Top-level scalar PvField — put_value / put_value_ctx accept
        // both NTScalar and bare scalar.
        PvField::Scalar(ScalarValue::Double(v))
    }

    /// Round-17 regression: PVA PUT must be gated through ACF when
    /// the source was built with ACF enforcement. Pre-fix the PVA
    /// server stored ACF only as `#[allow(dead_code)]` and never
    /// called `check_access*` — every client could PUT regardless
    /// of UAG/HAG/RULE configuration.
    #[tokio::test]
    async fn put_value_ctx_denies_when_acf_writeable_rule_unmet() {
        use epics_base_rs::server::records::ai::AiRecord;

        // ACF: only `admin` user on `host:lab` may WRITE.
        let acf_text = r#"
UAG(admins) { admin }
HAG(lab) { lab-pc1 }
ASG(SECURE) {
    RULE(1, READ)
    RULE(1, WRITE) { UAG(admins) HAG(lab) }
}
"#;
        let acf = parse_acf(acf_text).unwrap();

        let db = Arc::new(PvDatabase::new());
        db.add_record("AI:SECURE", Box::new(AiRecord::new(0.0))).await;
        // Mark the record as belonging to the SECURE ASG.
        let rec = db.get_record("AI:SECURE").await.unwrap();
        rec.write().await.common.asg = "SECURE".to_string();

        let source = PvDatabaseSource::new_with_acf(db.clone(), Arc::new(Some(acf)));

        // Allowed: admin from lab-pc1 — must succeed.
        source
            .put_value_ctx(
                "AI:SECURE",
                pv_double(1.0),
                make_ctx("lab-pc1", "admin", "anonymous"),
            )
            .await
            .expect("admin/lab-pc1 must be allowed to PUT");

        // Denied: regular user from a non-lab host — must fail with
        // a recognisable error.
        let err = source
            .put_value_ctx(
                "AI:SECURE",
                pv_double(2.0),
                make_ctx("intruder-pc", "guest", "anonymous"),
            )
            .await
            .expect_err("non-admin must be denied");
        assert!(
            err.contains("denied by access security"),
            "denial reason must be visible: {err:?}",
        );
    }

    /// Round-17: when the source is built without ACF, behaviour
    /// matches the old `put_value` path — every PUT succeeds.
    #[tokio::test]
    async fn put_value_ctx_allows_when_no_acf() {
        use epics_base_rs::server::records::ai::AiRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("AI:OPEN", Box::new(AiRecord::new(0.0))).await;

        let source = PvDatabaseSource::new(db.clone());
        source
            .put_value_ctx(
                "AI:OPEN",
                pv_double(7.0),
                make_ctx("anywhere", "anyone", "anonymous"),
            )
            .await
            .expect("PUT must succeed when no ACF is attached");
    }
}
