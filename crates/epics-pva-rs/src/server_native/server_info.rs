//! [`ServerInfoSource`] — the built-in `__server` diagnostic source.
//!
//! Mirrors pvxs `ServerSource` (`serversource.cpp`). pvxs registers a
//! single internal source at `(order = -1, "__server")` (src/server.cpp:542-547),
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
//! Registered automatically by `PvaServer::start`
//! at `order = -1`, BEFORE default-order (0) user sources, so the
//! reserved `server` name reaches diagnostics (pvxs parity). It claims
//! only `server`, so all other names still fall through to user sources;
//! a user that wants to own `server` must register at an explicit order
//! `< -1`.

use std::sync::Arc;

use epics_base_rs::types::PvString;

use crate::nt::NTScalar;
use crate::pvdata::{
    FieldDesc, PvField, PvStructure, RpcReply, ScalarType, ScalarValue, TypedScalarArray,
};

use super::source::{ChannelSource, OpError};

/// Canonical PV name the built-in source answers. pvxs `ServerSource`
/// hardcodes the same string.
pub const SERVER_PV_NAME: &str = "server";

/// `serversource.cpp:93` — every op the `server` RPC does not match falls
/// through to this one text; pvxs never echoes the op back.
const NOT_IMPLEMENTED: &str = "Not implemented";

/// `pvxs::NoField::what()` (data.cpp:17-19) — what a client sees when the
/// `server` RPC reads a `query.op` that is not there: `Value::copyOut` throws
/// (data.cpp:419-422) and the EXEC catch forwards `e.what()`
/// (serverget.cpp:504-508).
const NO_SUCH_FIELD: &str = "No such field";

/// Source name under which the built-in source is registered in the
/// [`super::CompositeSource`]. pvxs uses `"__server"`; the leading
/// `__` marks it internal (pvxs convention, see `composite.rs`).
pub const SERVER_SOURCE_NAME: &str = "__server";

/// Source name under which the hand-in user source is registered.
///
/// NOT pvxs's `"__builtin"`. That key belongs to `Server::Pvt::builtinsrc`
/// (`serverconn.h:265`), a source the *server* owns and only
/// `Server::addPV` writes into (`src/server.cpp:174-181`); an application never
/// hands one in. `PvaServer::start` takes the application's own
/// source, which is pvxs's `Server::addSource(name, src, order)` shape, and
/// that defaults to order 0 (`pvxs/server.h:116-118`) — behind both
/// internals. epics-rs has no `addPV` equivalent, so nothing occupies
/// `(-1, "__builtin")` and claiming that key for a hand-in source would put
/// an application source in the band pvxs reserves for its own.
pub const USER_SOURCE_NAME: &str = "__user";

/// Wrap a user source in the composite every PVA server must serve from.
///
/// # Why this is a function and not two call sites
///
/// It used to be written out in `runtime.rs` only, and
/// `blocking::BlockingPvaServer::bind` — the driver the whole RTEMS path uses
/// — bound the user source *directly*. The result was a server that came up,
/// served its user PVs and looked healthy while `pvxlist -i`,
/// `pvxlist <address>` and `pvlist-rs` all failed with "Refused to create
/// Channel":
/// the reserved [`SERVER_PV_NAME`] channel did not exist, so the server could
/// not be asked what it was. On a target with no shell that is the difference
/// between a diagnosable IOC and an opaque one.
///
/// The defect was not that one caller forgot a step; it was that the
/// composition rule lived in a caller at all, so every new server driver had
/// to rediscover it. Both drivers now call this, and a third cannot skip it
/// without deliberately not calling the only thing that returns a bindable
/// source.
///
/// # The rule
///
/// pvxs registers both of its internal sources at order **-1**
/// (`src/server.cpp:542-546`) and keys the registry by `(order, name)`
/// (`serverconn.h:268`, `src/server.cpp:91`), iterating it ascending at
/// CREATE_CHANNEL (`serverchan.cpp:304`). Application sources go in through
/// `Server::addSource`, whose order defaults to **0**
/// (`pvxs/server.h:116-118`) — QSRV is the worked example, `qsrvSingle` at 0
/// (`ioc/singlesourcehooks.cpp:158`) and `qsrvGroup` at 1
/// (`ioc/groupsourcehooks.cpp:219`). So every application source sits
/// strictly behind the internals, and that is the band the hand-in source
/// gets here.
///
/// The consequence is deliberate and is pvxs's, not ours: a database record
/// named literally `server` is SHADOWED. `ServerSource::onSearch` is empty
/// — "our `server` PV is not advertised" (`serversource.cpp:25-28`) — so the
/// SEARCH is answered only because the user source advertises the record,
/// but `onCreate` then claims any channel whose name is `server`
/// (`serversource.cpp:30-33`) before the order-0 source is consulted. Since
/// `pvxlist` reaches the facility by RPC-ing that same channel name
/// (`tools/list.cpp:159-161`), letting a user PV win the name is exactly
/// what takes `pvxlist` and `pvxinfo` off the air. Only pvxs's own
/// `addPV`-backed `builtinsrc` outranks the diagnostic, and epics-rs has no
/// `addPV`. Every name other than `server` falls through to the user
/// source, because the built-in source claims nothing else.
///
/// The channel lister enumerates the **user** source directly rather than the
/// composite. Going through the composite would also include the built-in
/// source; that is harmless because its `list_pvs` is empty, but naming the
/// user half keeps the intent explicit.
///
/// # Errors
///
/// Only if a `(name, order)` pair is already registered, which a freshly
/// created composite cannot have. Returned rather than asserted so the two
/// callers keep their own error prefixes.
pub fn compose_with_server_info(user_source: super::DynSource) -> Result<super::DynSource, String> {
    let server_info = Arc::new(ServerInfoSource::new({
        let user_source = user_source.clone();
        move || {
            let user_source = user_source.clone();
            async move { user_source.list_pvs().await }
        }
    }));

    let composite = super::CompositeSource::new();
    composite.add_source(USER_SOURCE_NAME, user_source, 0)?;
    composite.add_source(SERVER_SOURCE_NAME, server_info as super::DynSource, -1)?;
    Ok(composite as super::DynSource)
}

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

    /// The exact `help` reply payload pvxs `ServerSource::onRPC` sends:
    /// `ret["value"] = "Help, I really should write some help"`
    /// (serversource.cpp:47). A client or test comparing the built-in
    /// `server` help output against pvxs must see this byte-for-byte, so
    /// the Rust server emits the same literal rather than its own
    /// descriptive text.
    const HELP_TEXT: &'static str = "Help, I really should write some help";

    /// FieldDesc + value for a `help` reply — a full `NTScalar` string,
    /// the same NT type pvxs `ServerSource` replies with for a request
    /// that carries a `help` field (`nt::NTScalar{TypeCode::String}`,
    /// serversource.cpp:46-51). Routed through the shared [`NTScalar`]
    /// builder so the advertised `epics:nt/NTScalar:1.0` ID carries the
    /// mandatory `alarm` and `timeStamp` members (pvxs `NTScalar::build()`,
    /// nt.cpp:44-53) — a strict NT client selecting the layout by ID then
    /// finds every member it expects. The `value` is the pvxs literal
    /// (serversource.cpp:47) so the exact-output matches.
    fn help_response() -> (FieldDesc, PvField) {
        let desc = NTScalar::new(ScalarType::String).build();
        let mut value = NTScalar::new(ScalarType::String).create();
        if let PvField::Structure(s) = &mut value {
            s.set(
                "value",
                PvField::Scalar(ScalarValue::String(Self::HELP_TEXT.into())),
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

    /// The single argument structure pvxs `ServerSource::onRPC` inspects.
    /// pvxs does `args = raw; if(auto Q = args["query"]) args = Q;`
    /// (serversource.cpp:41-44) and then reads *both* `help` and `op` from
    /// that one selected value — so NTURI `query` unwrapping is terminal:
    /// once a `query` field is present it is the only argument view and
    /// there is no fall-back to root-level `help`/`op`.
    ///
    /// - `query` present and a structure → that structure is the args view.
    /// - `query` present but not a structure → no args (pvxs's `args = Q`
    ///   on a non-structure has neither `help` nor a readable `op`, so both
    ///   lookups fail and the RPC errors). Returned as `None`.
    /// - no `query` field → flat custom request; the root is the args view.
    fn rpc_args(request: &PvField) -> Option<&PvStructure> {
        let root = match request {
            PvField::Structure(s) => s,
            _ => return None,
        };
        match root.get_field("query") {
            Some(PvField::Structure(query)) => Some(query),
            // `query` present but not a structure → terminal, no args.
            Some(_) => None,
            // No `query` → inspect the root for a flat request.
            None => Some(root),
        }
    }

    /// Whether the selected RPC argument view carries a `help` field,
    /// mirroring pvxs `args["help"].valid()` (serversource.cpp:46). Any
    /// present field named `help` triggers the help reply regardless of
    /// its value/type.
    fn has_help(request: &PvField) -> bool {
        Self::rpc_args(request).is_some_and(|args| args.get_field("help").is_some())
    }

    /// Extract the `op` argument from the selected RPC argument view —
    /// the NTURI `query` structure when present, otherwise the flat root.
    /// No fall-back across the `query` boundary (see [`Self::rpc_args`]).
    fn extract_op(request: &PvField) -> Option<String> {
        scalar_string(Self::rpc_args(request)?.get_field("op"))
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
    async fn subscribe(
        &self,
        _name: &str,
    ) -> Option<crate::server_native::source::MonitorStream<PvField>> {
        None
    }

    /// RPC dispatch — the heart of the `pvlist` facility.
    fn rpc(
        &self,
        name: &str,
        _request_desc: FieldDesc,
        request_value: PvField,
    ) -> impl std::future::Future<Output = Result<RpcReply, OpError>> + Send {
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
                return Ok(Self::help_response().into());
            }
            // A missing `op` is not a hand-written diagnostic in pvxs: it reads
            // `args["op"].as<std::string>()` (serversource.cpp:53) on a field
            // that isn't there, `Value::copyOut` throws `NoField` ("No such
            // field", data.cpp:17-19,419-422), and the RPC EXEC catch forwards
            // `e.what()` to the client (serverget.cpp:504-508). That text is
            // the contract.
            let op =
                Self::extract_op(&request_value).ok_or_else(|| OpError::failed(NO_SUCH_FIELD))?;
            match op.as_str() {
                "channels" => {
                    let mut names = (this.channel_lister)().await;
                    names.sort();
                    names.dedup();
                    Ok((Self::channels_descriptor(), Self::channels_value(names)).into())
                }
                "info" => Ok((Self::info_descriptor(), Self::info_value()).into()),
                // pvxs falls off the op chain into `eop->error("Not
                // implemented")` (serversource.cpp:93) — it never echoes the
                // op back or lists the ones it knows.
                _ => Err(OpError::failed(NOT_IMPLEMENTED)),
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

    #[epics_macros_rs::epics_test]
    async fn has_pv_only_matches_server() {
        let src = source_with(vec![]);
        assert!(src.has_pv("server").await);
        assert!(!src.has_pv("anything:else").await);
    }

    #[epics_macros_rs::epics_test]
    async fn list_pvs_is_empty_so_server_never_self_lists() {
        let src = source_with(vec!["user:pv".into()]);
        assert!(src.list_pvs().await.is_empty());
    }

    #[epics_macros_rs::epics_test]
    async fn rpc_channels_returns_sorted_deduped_names() {
        let src = source_with(vec![
            "z:pv".into(),
            "a:pv".into(),
            "a:pv".into(),
            "m:pv".into(),
        ]);
        let (desc, value) = nturi_op("channels");
        let (resp_desc, resp_value) = src
            .rpc("server", desc, value)
            .await
            .expect("rpc ok")
            .into_value()
            .expect("value reply");
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

    #[epics_macros_rs::epics_test]
    async fn rpc_info_returns_only_impllang_and_version() {
        // pvxs `op=info` is a bare struct with exactly implLang+version
        // (serversource.cpp:19-22). No guid/peerCount/channelCount.
        let src = source_with(vec!["one".into(), "two".into()]);
        let (desc, value) = nturi_op("info");
        let (_, resp) = src
            .rpc("server", desc, value)
            .await
            .expect("rpc ok")
            .into_value()
            .expect("value reply");
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

    #[epics_macros_rs::epics_test]
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
            .expect("help rpc ok")
            .into_value()
            .expect("value reply");
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
                // The
                // `value` payload must be the exact pvxs literal
                // (serversource.cpp:47), not Rust-specific descriptive text.
                match s.get_field("value") {
                    Some(PvField::Scalar(ScalarValue::String(text))) => {
                        assert_eq!(
                            text.as_str_lossy().as_ref(),
                            "Help, I really should write some help",
                            "help value must match pvxs literal byte-for-byte"
                        );
                    }
                    other => panic!("unexpected help value: {other:?}"),
                }
            }
            other => panic!("unexpected help wrapper: {other:?}"),
        }
    }

    #[epics_macros_rs::epics_test]
    async fn rpc_unknown_op_errors() {
        let src = source_with(vec![]);
        let (desc, value) = nturi_op("frobnicate");
        let err = src.rpc("server", desc, value).await.unwrap_err();
        // pvxs answers every unmatched op with one bare string
        // (serversource.cpp:93); it never echoes the op back.
        assert_eq!(
            err.message, NOT_IMPLEMENTED,
            "an unmatched op must carry pvxs's contract text: {err}"
        );
    }

    #[epics_macros_rs::epics_test]
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
        assert_eq!(
            err.message, NO_SUCH_FIELD,
            "a missing `op` must surface pvxs's NoField text, not a hand-written diagnostic: {err}"
        );
    }

    #[epics_macros_rs::epics_test]
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
            .expect("flat-struct rpc ok")
            .into_value()
            .expect("value reply");
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

    #[epics_macros_rs::epics_test]
    async fn rpc_query_op_wins_over_root_help() {
        // pvxs selects `args = query` then reads BOTH help and op from
        // that single view (serversource.cpp:41-53) — a root-level `help`
        // beside `query.op` is never consulted, so this returns the
        // channel list, not the help text.
        let src = source_with(vec!["x".into()]);
        let mut query = PvStructure::new("");
        query.fields.push((
            "op".into(),
            PvField::Scalar(ScalarValue::String("channels".into())),
        ));
        let mut root = PvStructure::new("epics:nt/NTURI:1.0");
        root.fields.push((
            "help".into(),
            PvField::Scalar(ScalarValue::String("".into())),
        ));
        root.fields
            .push(("query".into(), PvField::Structure(query)));
        let (_, resp) = src
            .rpc("server", FieldDesc::Variant, PvField::Structure(root))
            .await
            .expect("rpc ok")
            .into_value()
            .expect("value reply");
        // A help reply is an NTScalar<string>; the channel list is a
        // string array. Asserting we got the array proves query.op won
        // over the root-level help.
        match resp {
            PvField::Structure(s) => match s.get_field("value") {
                Some(PvField::ScalarArrayTyped(TypedScalarArray::String(a))) => {
                    assert_eq!(a.to_vec(), vec!["x".to_string()]);
                }
                other => panic!("query.op=channels must win over root help: {other:?}"),
            },
            other => panic!("unexpected wrapper: {other:?}"),
        }
    }

    #[epics_macros_rs::epics_test]
    async fn rpc_present_query_does_not_fall_back_to_root_op() {
        // With a present (here empty) `query`, pvxs reads `op` ONLY from
        // the query view; a root-level `op` is not a fallback. So this
        // errors for a missing op rather than running root `op=channels`.
        let src = source_with(vec!["x".into()]);
        let query = PvStructure::new(""); // present but empty: no op
        let mut root = PvStructure::new("epics:nt/NTURI:1.0");
        root.fields.push((
            "op".into(),
            PvField::Scalar(ScalarValue::String("channels".into())),
        ));
        root.fields
            .push(("query".into(), PvField::Structure(query)));
        let err = src
            .rpc("server", FieldDesc::Variant, PvField::Structure(root))
            .await
            .unwrap_err();
        assert_eq!(
            err.message, NO_SUCH_FIELD,
            "present query without op must error, not fall back to root op: {err}"
        );
    }

    #[epics_macros_rs::epics_test]
    async fn get_has_no_surface_on_server() {
        // pvxs installs no `onOp` for `server`, so it has no GET surface
        // (serversource.cpp:30-94). Both the introspection prototype and
        // the value read return `None`, which makes a GET INIT against
        // `server` fail rather than returning a Rust-only structure.
        let src = source_with(vec!["a".into()]);
        assert!(src.get_introspection("server").await.is_none());
        assert!(src.get_value("server").await.is_none());
    }

    #[epics_macros_rs::epics_test]
    async fn put_is_rejected_read_only() {
        let src = source_with(vec![]);
        let err = src.put_value("server", PvField::Null).await.unwrap_err();
        assert!(err.message.contains("read-only"), "put rejected: {err}");
    }
}
