//! [`ServerInfoSource`] — the built-in low-priority `__server` source.
//!
//! Mirrors pvxs `ServerSource` (`serversource.cpp`). pvxs registers a
//! single internal source at `(order = -1, "__server")` (the lowest
//! priority slot, see `server.cpp:667`) that exposes one special PV
//! named `server`. That PV answers:
//!
//! - **GET** — returns a structure describing the server: GUID,
//!   implementation language, version string, and live peer counts.
//! - **RPC** — accepts an NTURI request whose `query.op` field selects
//!   the response:
//!   - `op=channels` → `NTScalarArray` of currently-hosted channel
//!     names (the union of every user source's `list_pvs()`).
//!   - `op=info` → the same server-info structure GET returns.
//!
//! This is what `pvxlist` / `pvlist` query to enumerate the channels a
//! server hosts. pvxs's `ServerSource::onSearch` is intentionally empty
//! — the `server` PV is *not* UDP-search-advertised; clients reach it
//! by connecting directly to the known host:port. We mirror that by
//! keeping [`ServerInfoSource::list_pvs`] empty so `server` never
//! self-lists in `op=channels` output nor in beacon advertisements,
//! AND by [`ServerInfoSource::searchable`] returning `false` so a UDP
//! SEARCH for the literal name `server` is never answered (F6).
//! `has_pv("server")` still returns `true`, which keeps the direct
//! TCP-connect GET / RPC path working — matching pvxs exactly: the
//! `server` PV is reachable by direct connect but invisible to
//! broadcast discovery.
//!
//! Registered automatically by [`crate::server_native::PvaServer::start`]
//! at `order = i32::MAX` so every user source takes precedence on name
//! collisions (a user is free to host their own PV literally named
//! `server`).

use std::sync::Arc;

use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, TypedScalarArray};

use super::peers::PeerRegistry;
use super::source::ChannelSource;

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
    /// Server GUID — the same 12 bytes the UDP responder advertises in
    /// SEARCH_RESPONSE and beacons. Rendered as a 24-char hex string
    /// in the `guid` field of the server-info structure.
    guid: [u8; 12],
    /// Per-peer registry shared with the TCP accept loop. Snapshotted
    /// on each GET / `op=info` to report live connection counts.
    peers: Arc<PeerRegistry>,
    /// Channel-list provider: a closure returning every PV name hosted
    /// by the *user* sources. Boxed so `ServerInfoSource` doesn't have
    /// to be generic over the composite; the registration code in
    /// `runtime.rs` wires this to the `CompositeSource::list_pvs` of
    /// the user-source half of the registry.
    channel_lister: Arc<ChannelLister>,
}

/// Async closure type for [`ServerInfoSource`]'s channel enumeration.
type ChannelLister = dyn Fn() -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Vec<String>> + Send>,
    > + Send
    + Sync;

impl ServerInfoSource {
    /// Build the built-in source.
    ///
    /// `guid` is the server's identity (must equal what the UDP
    /// responder advertises). `peers` is the live peer registry.
    /// `channel_lister` returns the union of every user source's PV
    /// names — `runtime.rs` passes a closure over the user-source
    /// `CompositeSource`.
    pub fn new<F, Fut>(guid: [u8; 12], peers: Arc<PeerRegistry>, channel_lister: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Vec<String>> + Send + 'static,
    {
        Self {
            guid,
            peers,
            channel_lister: Arc::new(move || Box::pin(channel_lister())),
        }
    }

    /// Lower-cased 24-char hex rendering of the GUID. pvxs prints the
    /// GUID upper-cased in `pvxlist`; the wire field value is just an
    /// opaque identifier so the case is cosmetic — we use lower-case
    /// to match the rest of epics-pva-rs's hex rendering. Distinct
    /// servers still compare unequal.
    fn guid_hex(&self) -> String {
        use std::fmt::Write;
        let mut s = String::with_capacity(24);
        for b in &self.guid {
            write!(&mut s, "{b:02x}").expect("write to String never fails");
        }
        s
    }

    /// FieldDesc for the server-info structure returned by GET and by
    /// `op=info`. A plain (non-NT) structure, matching pvxs
    /// `ServerSource::info` which also uses a bare `Struct`. We add
    /// `guid` and the peer counters on top of pvxs's `implLang` /
    /// `version` pair — extra fields are backward-compatible (a client
    /// only reads the fields it knows).
    pub fn info_descriptor() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![
                ("guid".into(), FieldDesc::Scalar(ScalarType::String)),
                ("implLang".into(), FieldDesc::Scalar(ScalarType::String)),
                ("version".into(), FieldDesc::Scalar(ScalarType::String)),
                ("peerCount".into(), FieldDesc::Scalar(ScalarType::UInt)),
                ("channelCount".into(), FieldDesc::Scalar(ScalarType::UInt)),
            ],
        }
    }

    /// Build the server-info value. `channel_count` is the number of
    /// currently-hosted PV names; `peer_count` the live connection
    /// count.
    fn info_value(&self, peer_count: u32, channel_count: u32) -> PvField {
        let mut s = PvStructure::new("");
        s.fields.push((
            "guid".into(),
            PvField::Scalar(ScalarValue::String(self.guid_hex())),
        ));
        s.fields.push((
            "implLang".into(),
            PvField::Scalar(ScalarValue::String("rust".into())),
        ));
        s.fields.push((
            "version".into(),
            PvField::Scalar(ScalarValue::String(crate::VERSION.to_string())),
        ));
        s.fields.push((
            "peerCount".into(),
            PvField::Scalar(ScalarValue::UInt(peer_count)),
        ));
        s.fields.push((
            "channelCount".into(),
            PvField::Scalar(ScalarValue::UInt(channel_count)),
        ));
        PvField::Structure(s)
    }

    /// FieldDesc for the `op=channels` response — an `NTScalarArray`
    /// of strings, the same NT type pvxs's `ServerSource` replies with
    /// (`nt::NTScalar{TypeCode::StringA}`).
    pub fn channels_descriptor() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalarArray:1.0".into(),
            fields: vec![("value".into(), FieldDesc::ScalarArray(ScalarType::String))],
        }
    }

    /// Build the `op=channels` response value from a sorted, de-duped
    /// list of channel names.
    fn channels_value(names: Vec<String>) -> PvField {
        let mut s = PvStructure::new("epics:nt/NTScalarArray:1.0");
        s.fields.push((
            "value".into(),
            PvField::ScalarArrayTyped(TypedScalarArray::String(Arc::from(names))),
        ));
        PvField::Structure(s)
    }

    /// Snapshot live counts: `(peer_count, channel_count)`.
    async fn live_counts(&self) -> (u32, u32) {
        let peer_count = self.peers.len() as u32;
        let channels = (self.channel_lister)().await;
        (peer_count, channels.len() as u32)
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
        Some(PvField::Scalar(ScalarValue::String(s))) => Some(s.clone()),
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
    fn searchable(&self, _name: &str) -> impl std::future::Future<Output = bool> + Send {
        async { false }
    }

    fn get_introspection(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        let matches = name == SERVER_PV_NAME;
        async move {
            if matches {
                Some(Self::info_descriptor())
            } else {
                None
            }
        }
    }

    fn get_value(&self, name: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let this = self.clone();
        let matches = name == SERVER_PV_NAME;
        async move {
            if !matches {
                return None;
            }
            let (peers, channels) = this.live_counts().await;
            Some(this.info_value(peers, channels))
        }
    }

    /// The `server` PV is read-only — like a pvxs readonly SharedPV.
    fn put_value(
        &self,
        name: &str,
        _value: PvField,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let name = name.to_string();
        async move { Err(format!("'{name}' is read-only (built-in server source)")) }
    }

    async fn is_writable(&self, _name: &str) -> bool {
        false
    }

    /// The `server` PV is queried with one-shot GET/RPC, not MONITOR
    /// — pvxs's `ServerSource` installs no `onSubscribe`. Returning
    /// `None` makes a MONITOR INIT against `server` fail cleanly.
    async fn subscribe(
        &self,
        _name: &str,
    ) -> Option<tokio::sync::mpsc::Receiver<PvField>> {
        None
    }

    /// RPC dispatch — the heart of the `pvlist` facility.
    fn rpc(
        &self,
        name: &str,
        _request_desc: FieldDesc,
        request_value: PvField,
    ) -> impl std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send {
        let this = self.clone();
        let name = name.to_string();
        async move {
            if name != SERVER_PV_NAME {
                return Err(format!("no such PV: {name}"));
            }
            let op = Self::extract_op(&request_value)
                .ok_or_else(|| "missing 'op' query argument".to_string())?;
            match op.as_str() {
                "channels" => {
                    let mut names = (this.channel_lister)().await;
                    names.sort();
                    names.dedup();
                    Ok((
                        Self::channels_descriptor(),
                        Self::channels_value(names),
                    ))
                }
                "info" => {
                    let (peers, channels) = this.live_counts().await;
                    Ok((
                        Self::info_descriptor(),
                        this.info_value(peers, channels),
                    ))
                }
                other => Err(format!(
                    "unknown op '{other}' (expected 'channels' or 'info')"
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nturi_op(op: &str) -> (FieldDesc, PvField) {
        let mut query = PvStructure::new("");
        query.fields.push((
            "op".into(),
            PvField::Scalar(ScalarValue::String(op.into())),
        ));
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
        let peers = PeerRegistry::new();
        ServerInfoSource::new([0xAB; 12], peers, move || {
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
        // Descriptor is NTScalarArray<string>.
        match resp_desc {
            FieldDesc::Structure { struct_id, .. } => {
                assert_eq!(struct_id, "epics:nt/NTScalarArray:1.0");
            }
            other => panic!("unexpected channels descriptor: {other:?}"),
        }
        let names = match resp_value {
            PvField::Structure(s) => match s.get_field("value") {
                Some(PvField::ScalarArrayTyped(TypedScalarArray::String(a))) => a.to_vec(),
                other => panic!("unexpected channels value shape: {other:?}"),
            },
            other => panic!("unexpected channels wrapper: {other:?}"),
        };
        assert_eq!(
            names,
            vec![
                "a:pv".to_string(),
                "m:pv".to_string(),
                "z:pv".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn rpc_info_returns_guid_and_version() {
        let src = source_with(vec!["one".into(), "two".into()]);
        let (desc, value) = nturi_op("info");
        let (_, resp) = src.rpc("server", desc, value).await.expect("rpc ok");
        let s = match resp {
            PvField::Structure(s) => s,
            other => panic!("unexpected info wrapper: {other:?}"),
        };
        match s.get_field("guid") {
            Some(PvField::Scalar(ScalarValue::String(g))) => {
                assert_eq!(g, &"ab".repeat(12));
            }
            other => panic!("unexpected guid field: {other:?}"),
        }
        match s.get_field("version") {
            Some(PvField::Scalar(ScalarValue::String(v))) => {
                assert_eq!(v, crate::VERSION);
            }
            other => panic!("unexpected version field: {other:?}"),
        }
        match s.get_field("channelCount") {
            Some(PvField::Scalar(ScalarValue::UInt(n))) => assert_eq!(*n, 2),
            other => panic!("unexpected channelCount field: {other:?}"),
        }
        match s.get_field("implLang") {
            Some(PvField::Scalar(ScalarValue::String(l))) => assert_eq!(l, "rust"),
            other => panic!("unexpected implLang field: {other:?}"),
        }
    }

    #[tokio::test]
    async fn rpc_unknown_op_errors() {
        let src = source_with(vec![]);
        let (desc, value) = nturi_op("frobnicate");
        let err = src.rpc("server", desc, value).await.unwrap_err();
        assert!(err.contains("frobnicate"), "error names the bad op: {err}");
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
        assert!(err.contains("op"), "error mentions missing op: {err}");
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
    async fn get_value_returns_info_structure() {
        let src = source_with(vec!["a".into()]);
        let v = src.get_value("server").await.expect("get value");
        match v {
            PvField::Structure(s) => {
                assert!(s.get_field("guid").is_some());
                assert!(s.get_field("version").is_some());
            }
            other => panic!("unexpected get value: {other:?}"),
        }
        assert!(src.get_value("not:server").await.is_none());
    }

    #[tokio::test]
    async fn put_is_rejected_read_only() {
        let src = source_with(vec![]);
        let err = src
            .put_value("server", PvField::Null)
            .await
            .unwrap_err();
        assert!(err.contains("read-only"), "put rejected: {err}");
    }
}
