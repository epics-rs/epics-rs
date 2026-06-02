//! [`ServerInfoSource`] — the built-in `__server` diagnostic source.
//!
//! Mirrors pvxs `ServerSource` (`serversource.cpp`). pvxs registers a
//! single internal source at `(order = -1, "__server")` (server.cpp:542-547),
//! consulted BEFORE default-order user sources since the lowest order is
//! called first (server.h:108-118), that exposes one special PV
//! named `server`. That PV answers RPC only — pvxs `ServerSource::onCreate`
//! installs *only* `handle->onRPC(...)` and never an `onOp` (GET/PUT)
//! handler (serversource.cpp:30-94). A GET against `server` therefore fails
//! the same way any PV with no operation handler fails, and we mirror that
//! by returning `None` from [`ServerInfoSource::get_introspection`] /
//! [`ServerInfoSource::get_value`] — there is no GET surface on `server`.
//!
//! The RPC handler unwraps an NTURI `query` structure, then:
//!
//! - a request carrying a `help` field replies with an `NTScalar` string
//!   (serversource.cpp:46-51), before any `op` is consulted;
//! - `op=channels` → `NTScalarArray` of currently-hosted channel names
//!   (the union of every user source's `list_pvs()`);
//! - `op=info` → a bare structure with exactly `implLang` and `version`
//!   (serversource.cpp:19-22, :83-90), no extra fields.
//!
//! This is what `pvxlist` / `pvlist` query to enumerate the channels a
//! server hosts. pvxs's `ServerSource::onSearch` is intentionally empty
//! — the `server` PV is *not* UDP-search-advertised; clients reach it
//! by connecting directly to the known host:port. We mirror that by
//! keeping [`ServerInfoSource::list_pvs`] empty so `server` never
//! self-lists in `op=channels` output nor in beacon advertisements,
//! AND by [`ServerInfoSource::searchable`] returning `false` so a UDP
//! SEARCH for the literal name `server` is never answered.
//! `has_pv("server")` still returns `true`, which keeps the direct
//! TCP-connect RPC path working — matching pvxs exactly: the
//! `server` PV is reachable by direct connect but invisible to
//! broadcast discovery.
//!
//! Registered automatically by [`crate::server_native::PvaServer::start`]
//! at `order = -1`, BEFORE default-order (0) user sources, so the
//! reserved `server` name reaches diagnostics (pvxs parity). It claims
//! only `server`, so all other names still fall through to user sources;
//! a user that wants to own `server` must register at an explicit order
//! `< -1`.

use std::sync::Arc;

use epics_base_rs::types::PvString;

use crate::nt::NTScalar;
use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, TypedScalarArray};

use super::source::{ChannelSource, OpError};

/// Canonical PV name the built-in source answers. pvxs `ServerSource`
/// hardcodes the same string.
pub const SERVER_PV_NAME: &str = "server";

/// Source name under which the built-in source is registered in the
/// [`super::CompositeSource`]. pvxs uses `"__server"`; the leading
/// `__` marks it internal (pvxs convention, see `composite.rs`).
pub const SERVER_SOURCE_NAME: &str = "__server";

/// Built-in source exposing the `server` PV. Cheap to clone — every
/// field is `Arc`-shared or `Copy`.
#[derive(Clone)]
pub struct ServerInfoSource {
    /// Channel-list provider: a closure returning every PV name hosted
    /// by the *user* sources. Boxed so `ServerInfoSource` doesn't have
    /// to be generic over the composite; the registration code in
    /// `runtime.rs` wires this to the `CompositeSource::list_pvs` of
    /// the user-source half of the registry.
    channel_lister: Arc<ChannelLister>,
}

/// Async closure type for [`ServerInfoSource`]'s channel enumeration.
type ChannelLister = dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<String>> + Send>>
    + Send
    + Sync;

impl ServerInfoSource {
    /// Build the built-in source.
    ///
    /// `channel_lister` returns the union of every user source's PV
    /// names — `runtime.rs` passes a closure over the user-source
    /// `CompositeSource`.
    pub fn new<F, Fut>(channel_lister: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Vec<String>> + Send + 'static,
    {
        Self {
            channel_lister: Arc::new(move || Box::pin(channel_lister())),
        }
    }

    /// FieldDesc for the `op=info` structure. A bare (non-NT) structure
    /// holding exactly `implLang` and `version`, byte-for-byte matching
    /// pvxs `ServerSource::info` (serversource.cpp:19-22). pvxs adds no
    /// other fields, so we add none either — a richer Rust-only
    /// structure would make any client that prints the whole returned
    /// value diverge from pvxs output.
    pub fn info_descriptor() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![
                ("implLang".into(), FieldDesc::Scalar(ScalarType::String)),
                ("version".into(), FieldDesc::Scalar(ScalarType::String)),
            ],
        }
    }

    /// Build the `op=info` value: `implLang` and `version` only. pvxs
    /// reports `implLang="cpp"`; this is a Rust server, so we report
    /// `"rust"` truthfully — the field name and structure shape match
    /// pvxs, only the honest language token differs.
    fn info_value() -> PvField {
        let mut s = PvStructure::new("");
        s.fields.push((
            "implLang".into(),
            PvField::Scalar(ScalarValue::String("rust".into())),
        ));
        s.fields.push((
            "version".into(),
            PvField::Scalar(ScalarValue::String(crate::VERSION.into())),
        ));
        PvField::Structure(s)
    }

    /// FieldDesc + value for a `help` reply — a full `NTScalar` string,
    /// the same NT type pvxs `ServerSource` replies with for a request
    /// that carries a `help` field (`nt::NTScalar{TypeCode::String}`,
    /// serversource.cpp:46-51). Routed through the shared [`NTScalar`]
    /// builder so the advertised `epics:nt/NTScalar:1.0` ID carries the
    /// mandatory `alarm` and `timeStamp` members (pvxs `NTScalar::build()`,
    /// nt.cpp:44-53) — a strict NT client selecting the layout by ID then
    /// finds every member it expects.
    fn help_response() -> (FieldDesc, PvField) {
        let desc = NTScalar::new(ScalarType::String).build();
        let mut value = NTScalar::new(ScalarType::String).create();
        if let PvField::Structure(s) = &mut value {
            s.set(
                "value",
                PvField::Scalar(ScalarValue::String(
                    "server PV: RPC op=channels lists hosted PVs, op=info reports \
                     implLang/version"
                        .into(),
                )),
            );
        }
        (desc, value)
    }

    /// FieldDesc for the `op=channels` response — a full `NTScalarArray`
    /// of strings, the same NT type pvxs's `ServerSource` replies with
    /// (`nt::NTScalar{TypeCode::StringA}.create()`, serversource.cpp:55-80).
    /// Routed through the shared [`NTScalar`] array builder so the
    /// advertised `epics:nt/NTScalarArray:1.0` ID carries the mandatory
    /// `alarm` and `timeStamp` members alongside `value`.
    pub fn channels_descriptor() -> FieldDesc {
        NTScalar::array(ScalarType::String).build()
    }

    /// Build the `op=channels` response value from a sorted, de-duped
    /// list of channel names. The full NT shape (value + alarm +
    /// timeStamp) comes from the shared builder; only `value` is
    /// overwritten with the channel-name array.
    fn channels_value(names: Vec<String>) -> PvField {
        let mut value = NTScalar::array(ScalarType::String).create();
        if let PvField::Structure(s) = &mut value {
            s.set(
                "value",
                PvField::ScalarArrayTyped(TypedScalarArray::String(
                    names.into_iter().map(PvString::from).collect(),
                )),
            );
        }
        value
    }

    /// Whether the request carries a `help` field (after NTURI `query`
    /// unwrapping), mirroring pvxs `args["help"].valid()`
    /// (serversource.cpp:46). Any present field named `help` triggers
    /// the help reply regardless of its value/type.
    fn has_help(request: &PvField) -> bool {
        let root = match request {
            PvField::Structure(s) => s,
            _ => return false,
        };
        if let Some(PvField::Structure(query)) = root.get_field("query")
            && query.get_field("help").is_some()
        {
            return true;
        }
        root.get_field("help").is_some()
    }

    /// Extract the `op` argument from an RPC request. Handles both the
    /// NTURI shape (`query.op`) and a flat-struct request (`op` at the
    /// top level) — pvxs `ServerSource::onRPC` unwraps `query` when
    /// present, then reads `op`.
    fn extract_op(request: &PvField) -> Option<String> {
        let root = match request {
            PvField::Structure(s) => s,
            _ => return None,
        };
        // NTURI: arguments live under `query`.
        if let Some(PvField::Structure(query)) = root.get_field("query") {
            if let Some(op) = scalar_string(query.get_field("op")) {
                return Some(op);
            }
        }
        // Flat-struct fallback: `op` directly at the top level.
        scalar_string(root.get_field("op"))
    }
}

/// Read a `PvField` as a string scalar, if it is one.
fn scalar_string(field: Option<&PvField>) -> Option<String> {
    match field {
        Some(PvField::Scalar(ScalarValue::String(s))) => Some(s.as_str_lossy().into_owned()),
        _ => None,
    }
}

impl ChannelSource for ServerInfoSource {
    /// Empty — pvxs's `server` PV is not search-advertised and does
    /// not appear in its own channel list. Keeping this empty means
    /// `op=channels` reports only user PVs and beacons never advertise
    /// `server`.
    async fn list_pvs(&self) -> Vec<String> {
        Vec::new()
    }

    /// Only the literal name `server` resolves here.
    fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let matches = name == SERVER_PV_NAME;
        async move { matches }
    }

    /// Always `false` — pvxs's `ServerSource::onSearch` is empty, so
    /// the `server` PV is never advertised on UDP discovery. A client
    /// reaches it only by a direct TCP connect to a known host:port.
    /// Returning `false` here keeps a broadcast SEARCH for the literal
    /// name `server` unanswered while `has_pv("server") == true` still
    /// lets the direct-connect GET / RPC path resolve it.
    async fn searchable(&self, _name: &str) -> bool {
        false
    }

    /// `None` for every name. pvxs `ServerSource::onCreate` installs no
    /// `onOp` handler, so `server` has no GET surface — there is no
    /// introspection prototype to negotiate. Returning `None` makes a
    /// GET (or GET_FIELD) INIT against `server` fail rather than
    /// returning a Rust-only structure pvxs would never serve over GET.
    async fn get_introspection(&self, _name: &str) -> Option<FieldDesc> {
        None
    }

    /// `None` for every name — `server` answers RPC only, not GET
    /// (pvxs has no `onOp` for it). See [`Self::get_introspection`].
    async fn get_value(&self, _name: &str) -> Option<PvField> {
        None
    }

    /// The `server` PV is read-only — like a pvxs readonly SharedPV.
    fn put_value(
        &self,
        name: &str,
        _value: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let name = name.to_string();
        // The `server` PV simply does not accept writes from anyone — a
        // fixed property of the PV, not an authorization decision —
        // so this is Failed, not Denied.
        async move {
            Err(OpError::failed(format!(
                "'{name}' is read-only (built-in server source)"
            )))
        }
    }

    async fn is_writable(&self, _name: &str) -> bool {
        false
    }

    /// The `server` PV is queried with one-shot GET/RPC, not MONITOR
    /// — pvxs's `ServerSource` installs no `onSubscribe`. Returning
    /// `None` makes a MONITOR INIT against `server` fail cleanly.
    async fn subscribe(&self, _name: &str) -> Option<tokio::sync::mpsc::Receiver<PvField>> {
        None
    }

    /// RPC dispatch — the heart of the `pvlist` facility.
    fn rpc(
        &self,
        name: &str,
        _request_desc: FieldDesc,
        request_value: PvField,
    ) -> impl std::future::Future<Output = Result<(FieldDesc, PvField), OpError>> + Send {
        let this = self.clone();
        let name = name.to_string();
        async move {
            if name != SERVER_PV_NAME {
                return Err(OpError::failed(format!("no such PV: {name}")));
            }
            // pvxs `ServerSource::onRPC` answers a `help`-bearing request
            // FIRST, before reading `op` (serversource.cpp:46-51), so
            // `pvcall server help=true` returns a help string rather than
            // failing for a missing `op`.
            if Self::has_help(&request_value) {
                return Ok(Self::help_response());
            }
            let op = Self::extract_op(&request_value)
                .ok_or_else(|| OpError::failed("missing 'op' query argument"))?;
            match op.as_str() {
                "channels" => {
                    let mut names = (this.channel_lister)().await;
                    names.sort();
                    names.dedup();
                    Ok((Self::channels_descriptor(), Self::channels_value(names)))
                }
                "info" => Ok((Self::info_descriptor(), Self::info_value())),
                other => Err(OpError::failed(format!(
                    "unknown op '{other}' (expected 'channels' or 'info')"
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nturi_op(op: &str) -> (FieldDesc, PvField) {
        let mut query = PvStructure::new("");
        query
            .fields
            .push(("op".into(), PvField::Scalar(ScalarValue::String(op.into()))));
        let mut root = PvStructure::new("epics:nt/NTURI:1.0");
        root.fields.push((
            "scheme".into(),
            PvField::Scalar(ScalarValue::String("pva".into())),
        ));
        root.fields.push((
            "path".into(),
            PvField::Scalar(ScalarValue::String("server".into())),
        ));
        root.fields
            .push(("query".into(), PvField::Structure(query)));
        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTURI:1.0".into(),
            fields: vec![
                ("scheme".into(), FieldDesc::Scalar(ScalarType::String)),
                ("path".into(), FieldDesc::Scalar(ScalarType::String)),
                (
                    "query".into(),
                    FieldDesc::Structure {
                        struct_id: String::new(),
                        fields: vec![("op".into(), FieldDesc::Scalar(ScalarType::String))],
                    },
                ),
            ],
        };
        (desc, PvField::Structure(root))
    }

    fn source_with(channels: Vec<String>) -> ServerInfoSource {
        ServerInfoSource::new(move || {
            let c = channels.clone();
            async move { c }
        })
    }

    #[tokio::test]
    async fn has_pv_only_matches_server() {
        let src = source_with(vec![]);
        assert!(src.has_pv("server").await);
        assert!(!src.has_pv("anything:else").await);
    }

    #[tokio::test]
    async fn list_pvs_is_empty_so_server_never_self_lists() {
        let src = source_with(vec!["user:pv".into()]);
        assert!(src.list_pvs().await.is_empty());
    }

    #[tokio::test]
    async fn rpc_channels_returns_sorted_deduped_names() {
        let src = source_with(vec![
            "z:pv".into(),
            "a:pv".into(),
            "a:pv".into(),
            "m:pv".into(),
        ]);
        let (desc, value) = nturi_op("channels");
        let (resp_desc, resp_value) = src.rpc("server", desc, value).await.expect("rpc ok");
        // Descriptor is a full NTScalarArray<string>: pvxs replies with
        // `nt::NTScalar{TypeCode::StringA}.create()` (serversource.cpp:55-80),
        // whose `epics:nt/NTScalarArray:1.0` ID promises value + alarm +
        // timeStamp (nt.cpp:44-53). A strict NT client must find all three.
        match &resp_desc {
            FieldDesc::Structure { struct_id, fields } => {
                assert_eq!(struct_id, "epics:nt/NTScalarArray:1.0");
                let members: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
                assert!(members.contains(&"value"), "channels desc: {members:?}");
                assert!(
                    members.contains(&"alarm"),
                    "channels desc must carry alarm: {members:?}"
                );
                assert!(
                    members.contains(&"timeStamp"),
                    "channels desc must carry timeStamp: {members:?}"
                );
            }
            other => panic!("unexpected channels descriptor: {other:?}"),
        }
        let names = match &resp_value {
            PvField::Structure(s) => {
                // The served value matches the descriptor: alarm + timeStamp
                // sub-structures present, not just `value`.
                assert!(s.get_field("alarm").is_some(), "value must carry alarm");
                assert!(
                    s.get_field("timeStamp").is_some(),
                    "value must carry timeStamp"
                );
                match s.get_field("value") {
                    Some(PvField::ScalarArrayTyped(TypedScalarArray::String(a))) => a.to_vec(),
                    other => panic!("unexpected channels value shape: {other:?}"),
                }
            }
            other => panic!("unexpected channels wrapper: {other:?}"),
        };
        assert_eq!(
            names,
            vec!["a:pv".to_string(), "m:pv".to_string(), "z:pv".to_string()]
        );
    }

    #[tokio::test]
    async fn rpc_info_returns_only_impllang_and_version() {
        // pvxs `op=info` is a bare struct with exactly implLang+version
        // (serversource.cpp:19-22). No guid/peerCount/channelCount.
        let src = source_with(vec!["one".into(), "two".into()]);
        let (desc, value) = nturi_op("info");
        let (_, resp) = src.rpc("server", desc, value).await.expect("rpc ok");
        let s = match resp {
            PvField::Structure(s) => s,
            other => panic!("unexpected info wrapper: {other:?}"),
        };
        match s.get_field("version") {
            Some(PvField::Scalar(ScalarValue::String(v))) => {
                assert_eq!(v, crate::VERSION);
            }
            other => panic!("unexpected version field: {other:?}"),
        }
        match s.get_field("implLang") {
            Some(PvField::Scalar(ScalarValue::String(l))) => assert_eq!(l, "rust"),
            other => panic!("unexpected implLang field: {other:?}"),
        }
        // Rust-only fields are gone (pvxs parity).
        assert!(s.get_field("guid").is_none(), "guid must not be present");
        assert!(
            s.get_field("peerCount").is_none(),
            "peerCount must not be present"
        );
        assert!(
            s.get_field("channelCount").is_none(),
            "channelCount must not be present"
        );
        // Exactly two fields, in pvxs order.
        let names: Vec<&str> = s.fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["implLang", "version"]);
    }

    #[tokio::test]
    async fn rpc_help_returns_ntscalar_string() {
        // pvxs answers a help-bearing request with an NTScalar<string>
        // BEFORE reading `op` (serversource.cpp:46-51) — so a request
        // with `help` and no `op` succeeds rather than failing.
        let src = source_with(vec![]);
        let mut query = PvStructure::new("");
        query.fields.push((
            "help".into(),
            PvField::Scalar(ScalarValue::String("".into())),
        ));
        let mut root = PvStructure::new("epics:nt/NTURI:1.0");
        root.fields
            .push(("query".into(), PvField::Structure(query)));
        let (desc, resp) = src
            .rpc("server", FieldDesc::Variant, PvField::Structure(root))
            .await
            .expect("help rpc ok");
        match &desc {
            FieldDesc::Structure { struct_id, fields } => {
                assert_eq!(struct_id, "epics:nt/NTScalar:1.0");
                // Full NTScalar: value + alarm + timeStamp (nt.cpp:44-53).
                let members: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
                assert!(members.contains(&"value"), "help desc: {members:?}");
                assert!(
                    members.contains(&"alarm"),
                    "help desc must carry alarm: {members:?}"
                );
                assert!(
                    members.contains(&"timeStamp"),
                    "help desc must carry timeStamp: {members:?}"
                );
            }
            other => panic!("unexpected help descriptor: {other:?}"),
        }
        match resp {
            PvField::Structure(s) => {
                assert!(
                    s.get_field("alarm").is_some(),
                    "help value must carry alarm"
                );
                assert!(
                    s.get_field("timeStamp").is_some(),
                    "help value must carry timeStamp"
                );
                match s.get_field("value") {
                    Some(PvField::Scalar(ScalarValue::String(_))) => {}
                    other => panic!("unexpected help value: {other:?}"),
                }
            }
            other => panic!("unexpected help wrapper: {other:?}"),
        }
    }

    #[tokio::test]
    async fn rpc_unknown_op_errors() {
        let src = source_with(vec![]);
        let (desc, value) = nturi_op("frobnicate");
        let err = src.rpc("server", desc, value).await.unwrap_err();
        assert!(
            err.message.contains("frobnicate"),
            "error names the bad op: {err}"
        );
    }

    #[tokio::test]
    async fn rpc_missing_op_errors() {
        let src = source_with(vec![]);
        // NTURI with an empty query — no `op`.
        let mut query = PvStructure::new("");
        let _ = &mut query;
        let mut root = PvStructure::new("epics:nt/NTURI:1.0");
        root.fields
            .push(("query".into(), PvField::Structure(query)));
        let err = src
            .rpc("server", FieldDesc::Variant, PvField::Structure(root))
            .await
            .unwrap_err();
        assert!(
            err.message.contains("op"),
            "error mentions missing op: {err}"
        );
    }

    #[tokio::test]
    async fn rpc_flat_struct_request_op_at_top_level() {
        // pvxs custom services may send `op` at the top level rather
        // than inside `query`. `extract_op` handles both.
        let src = source_with(vec!["x".into()]);
        let mut root = PvStructure::new("");
        root.fields.push((
            "op".into(),
            PvField::Scalar(ScalarValue::String("channels".into())),
        ));
        let (_, resp) = src
            .rpc("server", FieldDesc::Variant, PvField::Structure(root))
            .await
            .expect("flat-struct rpc ok");
        match resp {
            PvField::Structure(s) => match s.get_field("value") {
                Some(PvField::ScalarArrayTyped(TypedScalarArray::String(a))) => {
                    assert_eq!(a.to_vec(), vec!["x".to_string()]);
                }
                other => panic!("unexpected value: {other:?}"),
            },
            other => panic!("unexpected wrapper: {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_has_no_surface_on_server() {
        // pvxs installs no `onOp` for `server`, so it has no GET surface
        // (serversource.cpp:30-94). Both the introspection prototype and
        // the value read return `None`, which makes a GET INIT against
        // `server` fail rather than returning a Rust-only structure.
        let src = source_with(vec!["a".into()]);
        assert!(src.get_introspection("server").await.is_none());
        assert!(src.get_value("server").await.is_none());
    }

    #[tokio::test]
    async fn put_is_rejected_read_only() {
        let src = source_with(vec![]);
        let err = src.put_value("server", PvField::Null).await.unwrap_err();
        assert!(err.message.contains("read-only"), "put rejected: {err}");
    }
}
