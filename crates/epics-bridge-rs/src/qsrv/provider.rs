//! ChannelProvider trait and BridgeProvider implementation.
//!
//! Corresponds to C++ QSRV's `PDBProvider` (pdb.h/pdb.cpp).
//!
//! The trait definitions here are temporary — they will move to `epics-pva-rs`
//! once the PVA server is implemented by the spvirit maintainer.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_pva_rs::pvdata::{FieldDesc, PvStructure};

use epics_base_rs::types::DbFieldType;

use super::channel::BridgeChannel;
use super::group::GroupChannel;
use super::group_config::GroupPvDef;
use super::pvif::NtType;
use crate::error::{BridgeError, BridgeResult};

// ---------------------------------------------------------------------------
// Access control
// ---------------------------------------------------------------------------

/// Full client credential set for method/authority/roles-aware ACF checks.
///
/// Mirrors the pvxs `Credentials` shape built in `pvxs/ioc/credentials.cpp`:
/// account (or method-prefixed form), host, auth method, certificate
/// authority, and roles. `AccessControl::can_read_creds` /
/// `can_write_creds` receive this so `AcfAccessControl` can pass the
/// correct values to `check_access_method` instead of the former
/// hardcoded `0` / `"anonymous"` / `""`.
#[derive(Debug, Clone, Default)]
pub struct ClientCreds {
    /// Account name (e.g. "alice", "CN=alice" for x509).
    pub user: String,
    /// Client host name (reverse-resolved or empty).
    pub host: String,
    /// Auth method ("anonymous", "ca", "x509", …).
    /// pvxs `ClientCredentials::method`
    /// (pvxs/include/pvxs/srvcommon.h:43).
    pub method: String,
    /// Root CA subject CN for the x509 method. Empty for non-TLS methods.
    /// ACF `AUTHORITY(...)` rules match against this.
    pub authority: String,
    /// Group/role claims. ACF UAG entries of the form `role/name` match
    /// against this list. pvxs `ClientCredentials::roles()`
    /// (pvxs/include/pvxs/srvcommon.h:55).
    pub roles: Vec<String>,
}

/// Access control interface for PVA channels.
///
/// Corresponds to C++ QSRV's per-channel ASCLIENT checks.
/// Default implementation allows all access.
pub trait AccessControl: Send + Sync {
    /// Check if the client can read this channel.
    fn can_read(&self, _channel: &str, _user: &str, _host: &str) -> bool {
        true
    }

    /// Check if the client can write to this channel.
    fn can_write(&self, _channel: &str, _user: &str, _host: &str) -> bool {
        true
    }

    /// Method/authority/roles-aware read check.
    ///
    /// Default forwards to `can_read(channel, creds.user, creds.host)` so
    /// impls that do not need method/authority/roles need not override.
    /// `AcfAccessControl` overrides to pass the full credential set to
    /// `check_access_method`.
    fn can_read_creds(&self, channel: &str, creds: &ClientCreds) -> bool {
        self.can_read(channel, &creds.user, &creds.host)
    }

    /// Method/authority/roles-aware write check.
    fn can_write_creds(&self, channel: &str, creds: &ClientCreds) -> bool {
        self.can_write(channel, &creds.user, &creds.host)
    }
}

/// Default access control that allows all operations.
pub struct AllowAllAccess;
impl AccessControl for AllowAllAccess {}

/// AccessControl backed by an epics-base [`AccessSecurityConfig`].
///
/// Bridges qsrv's per-channel access checks to epics-base ACF
/// (UAG/HAG/RULE/METHOD/AUTHORITY) so a `BridgeProvider` configured
/// from an `.acf` file enforces the same policy as the CA / PVA
/// servers. Looks up each record's `ASG` field via the database;
/// simple PVs (no record-level ASG) and unknown names fall back to
/// the `DEFAULT` ASG, matching the CA server's behaviour
/// (tcp.rs:300).
pub struct AcfAccessControl {
    db: Arc<epics_base_rs::server::database::PvDatabase>,
    cfg: Arc<epics_base_rs::server::access_security::AccessSecurityConfig>,
}

impl AcfAccessControl {
    pub fn new(
        db: Arc<epics_base_rs::server::database::PvDatabase>,
        cfg: epics_base_rs::server::access_security::AccessSecurityConfig,
    ) -> Self {
        Self {
            db,
            cfg: Arc::new(cfg),
        }
    }

    /// Resolve (ASG name, field ASL) for a channel from the backing database.
    ///
    /// Mirrors pvxs `ioc/securityclient.cpp:25` — `asAddClient` is passed
    /// `dbChannelFldDes(ch)->as_level` as the ASL. Our Rust model stores a
    /// per-record `common.asl` (same approach as
    /// `epics-ca-rs/src/server/tcp.rs:459`).
    fn resolve_asg_and_asl_blocking(&self, channel: &str) -> (String, u8) {
        let (record_name, _field) = epics_base_rs::server::database::parse_pv_name(channel);
        let db = self.db.clone();
        let name = record_name.to_string();
        let lookup = async move {
            if let Some(rec) = db.get_record(&name).await {
                let inst = rec.read().await;
                let asg = if inst.common.asg.is_empty() {
                    "DEFAULT".to_string()
                } else {
                    inst.common.asg.clone()
                };
                return (asg, inst.common.asl);
            }
            ("DEFAULT".to_string(), 0u8)
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => match handle.runtime_flavor() {
                tokio::runtime::RuntimeFlavor::MultiThread => {
                    tokio::task::block_in_place(|| handle.block_on(lookup))
                }
                _ => ("DEFAULT".to_string(), 0u8),
            },
            Err(_) => ("DEFAULT".to_string(), 0u8),
        }
    }

    /// Build pvxs-style credential strings from `ClientCreds`.
    ///
    /// Mirrors `pvxs/ioc/credentials.cpp:31-45`:
    /// - "ca" method (or no method): plain account name, path-suffix stripped.
    /// - other methods: `"method/account"`.
    /// - each role: `"role/rolename"`.
    fn credential_strings(creds: &ClientCreds) -> Vec<String> {
        let mut v = Vec::new();
        let primary = if creds.method == "ca" || creds.method.is_empty() {
            let pos = creds.user.rfind('/').map(|p| p + 1).unwrap_or(0);
            creds.user[pos..].to_string()
        } else {
            format!("{}/{}", creds.method, creds.user)
        };
        v.push(primary);
        for role in &creds.roles {
            v.push(format!("role/{role}"));
        }
        v
    }

    /// Full credential/method/ASL-aware access level check.
    ///
    /// Resolves (ASG, ASL) from the database, builds pvxs-style credential
    /// strings, then calls `check_access_method` for each. Access is the
    /// maximum level across all credentials — mirrors `SecurityClient::canWrite`
    /// `any_of` semantics (`pvxs/ioc/securityclient.cpp:42-45`).
    fn level_for_creds(&self, channel: &str, creds: &ClientCreds) -> AccessLevelLite {
        use epics_base_rs::server::access_security::AccessLevel;
        let (asg, asl) = self.resolve_asg_and_asl_blocking(channel);
        let cred_strings = Self::credential_strings(creds);
        // a QSRV access context built through the legacy
        // no-method constructors (`AccessContext::anonymous`,
        // `with_identity`, `create_channel`/`create_channel_for`, or
        // the 3-arg `AccessControl::can_read`/`can_write`) carries an
        // empty `method`. pvxs never produces an empty method — an
        // unauthenticated client gets `method = "anonymous"`
        // (`serverconn.cpp:78`, `:230`). An empty method cannot match
        // a `METHOD("anonymous")` rule (`check_access_method` matches
        // METHOD lists by literal comparison), so an ACF that scopes
        // anonymous read access through such a rule would silently
        // deny the legacy path. Normalize an empty method to
        // `"anonymous"` for the rule check, matching the
        // `check_access_method(.., "anonymous", ..)` call origin/main
        // used for the legacy path. The account string passed for
        // UAG matching keeps `credential_strings`' empty-method
        // (plain-account) form, so UAG entries listing a bare
        // username still match.
        let method = if creds.method.is_empty() {
            "anonymous"
        } else {
            creds.method.as_str()
        };
        let mut best = AccessLevelLite::None;
        for cred_user in &cred_strings {
            let lvl = self.cfg.check_access_method(
                &asg,
                &creds.host,
                cred_user,
                asl,
                method,
                &creds.authority,
            );
            let lit = match lvl {
                AccessLevel::ReadWrite => AccessLevelLite::ReadWrite,
                AccessLevel::Read => AccessLevelLite::Read,
                _ => AccessLevelLite::None,
            };
            if lit == AccessLevelLite::ReadWrite {
                return lit;
            }
            if lit == AccessLevelLite::Read && best == AccessLevelLite::None {
                best = lit;
            }
        }
        best
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum AccessLevelLite {
    None,
    Read,
    ReadWrite,
}

impl AccessControl for AcfAccessControl {
    fn can_read(&self, channel: &str, user: &str, host: &str) -> bool {
        self.can_read_creds(
            channel,
            &ClientCreds {
                user: user.to_string(),
                host: host.to_string(),
                ..Default::default()
            },
        )
    }

    fn can_write(&self, channel: &str, user: &str, host: &str) -> bool {
        self.can_write_creds(
            channel,
            &ClientCreds {
                user: user.to_string(),
                host: host.to_string(),
                ..Default::default()
            },
        )
    }

    fn can_read_creds(&self, channel: &str, creds: &ClientCreds) -> bool {
        self.level_for_creds(channel, creds) != AccessLevelLite::None
    }

    fn can_write_creds(&self, channel: &str, creds: &ClientCreds) -> bool {
        self.level_for_creds(channel, creds) == AccessLevelLite::ReadWrite
    }
}

/// Per-channel client identity used for access enforcement.
///
/// Carries the access control policy plus the full credential set of
/// whichever downstream client opened this channel. The PVA server fills
/// in all fields from the connection's authentication context; when no
/// PVA server is wired (tests, in-process), all credential fields are
/// empty and `AccessControl` implementations fall back to their defaults.
#[derive(Clone)]
pub struct AccessContext {
    pub access: Arc<dyn AccessControl>,
    pub user: String,
    pub host: String,
    /// Auth method. pvxs `ClientCredentials::method`
    /// (pvxs/include/pvxs/srvcommon.h:43).
    pub method: String,
    /// Root CA subject CN for the x509 method; empty for others.
    pub authority: String,
    /// Role claims for UAG `role/…` entries.
    /// pvxs `ClientCredentials::roles()` (pvxs/include/pvxs/srvcommon.h:55).
    pub roles: Vec<String>,
}

impl AccessContext {
    /// Construct a context for an unauthenticated request (empty credentials).
    pub fn anonymous(access: Arc<dyn AccessControl>) -> Self {
        Self {
            access,
            user: String::new(),
            host: String::new(),
            method: String::new(),
            authority: String::new(),
            roles: Vec::new(),
        }
    }

    /// Construct a context with explicit user/host (method defaults to empty).
    pub fn with_identity(access: Arc<dyn AccessControl>, user: String, host: String) -> Self {
        Self {
            access,
            user,
            host,
            method: String::new(),
            authority: String::new(),
            roles: Vec::new(),
        }
    }

    /// Construct a context with the full [`ClientCreds`] set.
    pub fn with_creds(access: Arc<dyn AccessControl>, creds: ClientCreds) -> Self {
        Self {
            access,
            user: creds.user,
            host: creds.host,
            method: creds.method,
            authority: creds.authority,
            roles: creds.roles,
        }
    }

    /// Allow-all context (used by tests and the default `BridgeProvider`).
    pub fn allow_all() -> Self {
        Self::anonymous(Arc::new(AllowAllAccess))
    }

    pub fn can_read(&self, channel: &str) -> bool {
        self.access.can_read_creds(channel, &self.to_client_creds())
    }

    pub fn can_write(&self, channel: &str) -> bool {
        self.access
            .can_write_creds(channel, &self.to_client_creds())
    }

    fn to_client_creds(&self) -> ClientCreds {
        ClientCreds {
            user: self.user.clone(),
            host: self.host.clone(),
            method: self.method.clone(),
            authority: self.authority.clone(),
            roles: self.roles.clone(),
        }
    }
}

impl Default for AccessContext {
    fn default() -> Self {
        Self::allow_all()
    }
}

// ---------------------------------------------------------------------------
// Trait definitions (to be moved to epics-pva-rs)
// ---------------------------------------------------------------------------

/// PVA ChannelProvider interface.
///
/// Corresponds to C++ `pva::ChannelProvider`. A PVA server calls into this
/// trait to resolve channel names and create channel instances.
pub trait ChannelProvider: Send + Sync {
    /// Provider name (e.g., "BRIDGE").
    fn provider_name(&self) -> &str;

    /// Check if a channel name exists (for UDP search responses).
    fn channel_find(&self, name: &str) -> impl std::future::Future<Output = bool> + Send;

    /// List all available channel names.
    fn channel_list(&self) -> impl std::future::Future<Output = Vec<String>> + Send;

    /// Create a channel for the given name.
    fn create_channel(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = BridgeResult<AnyChannel>> + Send;
}

/// PVA Channel interface.
///
/// Corresponds to C++ `pva::Channel`. Each instance is bound to a single
/// PV (record or group).
pub trait Channel: Send + Sync {
    /// The channel (PV) name.
    fn channel_name(&self) -> &str;

    /// Get: read current value + metadata as a PvStructure.
    fn get(
        &self,
        request: &PvStructure,
    ) -> impl std::future::Future<Output = BridgeResult<PvStructure>> + Send;

    /// Put: write a PvStructure value into the record.
    fn put(
        &self,
        value: &PvStructure,
    ) -> impl std::future::Future<Output = BridgeResult<()>> + Send;

    /// GetField: return the type description (FieldDesc) for introspection.
    fn get_field(&self) -> impl std::future::Future<Output = BridgeResult<FieldDesc>> + Send;

    /// Create a monitor for this channel.
    fn create_monitor(
        &self,
    ) -> impl std::future::Future<Output = BridgeResult<super::group::AnyMonitor>> + Send;
}

/// the result of a [`PvaMonitor::poll`] — the snapshot
/// plus an optional explicit marked-leaf set.
///
/// `marked == None`: the server derives the wire changed-bitset itself
/// — the full request mask, or the value-diff for a pure
/// self-trigger group / single-record monitor. Single-record monitors
/// and the group priming snapshot use this.
///
/// `marked == Some(paths)`: the dot-separated group field paths the
/// resolved `+trigger` graph marks for this event. pvxs
/// `groupsource.cpp:283-300` iterates `field.triggers` on a member
/// event and marks each target field assigned-not-changed; the PVA
/// layer turns the paths into the wire changed-bitset
/// (`build_monitor_payload_marked`). Carrying the set here — rather
/// than re-deriving it by diffing snapshots — is what makes a
/// `"+trigger": "otherField"` member mark only its named targets
/// instead of behaving like `+trigger:"*"`.
#[derive(Debug, Clone)]
pub struct MonitorPoll {
    /// The (full) snapshot value for this event.
    pub value: PvStructure,
    /// Explicit changed field paths, or `None` to derive the bitset.
    pub marked: Option<Vec<String>>,
}

impl MonitorPoll {
    /// A snapshot with no explicit marked set — the server derives the
    /// changed-bitset (full mask, or diff for a partial source).
    pub fn derive(value: PvStructure) -> Self {
        Self {
            value,
            marked: None,
        }
    }
}

/// PVA Monitor interface.
///
/// Corresponds to C++ `pva::Monitor` / `BaseMonitor`.
pub trait PvaMonitor: Send + Sync {
    /// Wait for the next update. Returns `None` when the monitor is closed.
    fn poll(&mut self) -> impl std::future::Future<Output = Option<MonitorPoll>> + Send;

    /// Start the monitor (begin receiving events).
    fn start(&mut self) -> impl std::future::Future<Output = BridgeResult<()>> + Send;

    /// Stop the monitor.
    fn stop(&mut self) -> impl std::future::Future<Output = ()> + Send;
}

// ---------------------------------------------------------------------------
// AnyChannel — enum dispatch for Channel trait
// ---------------------------------------------------------------------------

/// Concrete channel type returned by BridgeProvider.
///
/// Uses enum dispatch instead of `dyn Channel` because async trait methods
/// with `impl Future` return types are not dyn-compatible.
pub enum AnyChannel {
    Single(BridgeChannel),
    Group(GroupChannel),
}

impl Channel for AnyChannel {
    fn channel_name(&self) -> &str {
        match self {
            Self::Single(ch) => ch.channel_name(),
            Self::Group(ch) => ch.channel_name(),
        }
    }

    async fn get(&self, request: &PvStructure) -> BridgeResult<PvStructure> {
        match self {
            Self::Single(ch) => ch.get(request).await,
            Self::Group(ch) => ch.get(request).await,
        }
    }

    async fn put(&self, value: &PvStructure) -> BridgeResult<()> {
        match self {
            Self::Single(ch) => ch.put(value).await,
            Self::Group(ch) => ch.put(value).await,
        }
    }

    async fn get_field(&self) -> BridgeResult<FieldDesc> {
        match self {
            Self::Single(ch) => ch.get_field().await,
            Self::Group(ch) => ch.get_field().await,
        }
    }

    async fn create_monitor(&self) -> BridgeResult<super::group::AnyMonitor> {
        match self {
            Self::Single(ch) => ch.create_monitor().await,
            Self::Group(ch) => ch.create_monitor().await,
        }
    }
}

// ---------------------------------------------------------------------------
// BridgeProvider
// ---------------------------------------------------------------------------

/// Bridge ChannelProvider that exposes EPICS database records as PVA channels.
///
/// Corresponds to C++ `PDBProvider`. Includes channel caching for reuse
/// and pluggable access control.
pub struct BridgeProvider {
    db: Arc<PvDatabase>,
    /// Group PV registry. Wrapped in [`parking_lot::RwLock`] so iocsh
    /// commands (`dbLoadGroup`, `processGroups`) can mutate the
    /// registry through a shared `Arc<BridgeProvider>` after the
    /// provider has been handed to the PVA server. The lock is taken
    /// only at config-load time and once per channel-find / list, so
    /// the contention cost is negligible.
    groups: parking_lot::RwLock<HashMap<String, GroupPvDef>>,
    /// Cumulative channel-creation counter. Tagged onto the provider
    /// so `qsrvStats` can report total throughput. Mirrors pvxs
    /// `qStats` (singlesourcehooks.cpp:88) total-channels metric.
    /// Counters never decrement; restart the IOC for a clean slate.
    channels_created: std::sync::atomic::AtomicU64,
    /// Cumulative GET / PUT / SUBSCRIBE counters. Same caveats.
    ops_get: std::sync::atomic::AtomicU64,
    ops_put: std::sync::atomic::AtomicU64,
    ops_subscribe: std::sync::atomic::AtomicU64,
    /// Metadata cache for single-record channels: (NtType, DbFieldType).
    /// Avoids repeated record introspection on every create_channel() call.
    /// Corresponds to C++ PDBProvider's transient_pv_map.
    record_cache: tokio::sync::RwLock<HashMap<String, (NtType, DbFieldType)>>,
    /// Live access-control cell. Channels and AccessContexts hold an
    /// `Arc<LiveAccessProxy>` that points at this cell, so
    /// `set_access_control` is observed by all existing channels on
    /// their *next* check (matches C++ QSRV — ACF reload takes effect
    /// without recreating channels).
    access_cell: Arc<parking_lot::RwLock<Arc<dyn AccessControl>>>,
    /// Source registry for file-loaded group definitions:
    /// `(filename, raw-macros)` → the group names that load placed into
    /// `groups`. pvxs keeps a `groupConfigFiles` list so
    /// `dbLoadGroup("-file.json")` can remove a previously added file
    /// and `dbLoadGroup("-*")` can clear them all
    /// (ioc/groupsourcehooks.cpp:133-183). The Rust port parses eagerly
    /// into `groups` (the daemon serves without `processGroups`), so the
    /// source identity is retained here to make the same add / replace /
    /// remove contract expressible without a deferred-parse rearchitecture.
    group_files: parking_lot::RwLock<Vec<GroupFileEntry>>,
    /// QSRV2 serving gate. pvxs adds the single-record and group sources
    /// to the PVA server only when `enable2()` returns true
    /// (`ioc/iochooks.cpp:461-496`, gated by `PVXS_QSRV_ENABLE` /
    /// `EPICS_IOC_IGNORE_SERVERS=qsrv2`). When `false`, every database /
    /// group resolution entry point answers "absent" so no DB-backed PVA
    /// channel is served — matching a pvxs IOC that loaded QSRV2 but had
    /// it disabled. Native PVA PVs (NDPluginPva) are owned by
    /// `QsrvPvStore`, not here, so they remain served (pvxs serves those
    /// through a separate `SharedPV`, ungated by `enable2()`).
    enabled: bool,
}

/// One `dbLoadGroup(filename, macros)` registration: its source
/// identity plus the group names that load placed into the live
/// registry, so the same file can later be removed.
struct GroupFileEntry {
    filename: String,
    macros: String,
    names: Vec<String>,
}

/// Proxy that re-reads the live access-control policy on every check.
/// Wraps an `Arc<RwLock<Arc<dyn AccessControl>>>` shared with the
/// owning [`BridgeProvider`] — `set_access_control` swaps the inner
/// `Arc` and existing AccessContexts pick up the new policy on their
/// next [`can_read`] / [`can_write`] call.
struct LiveAccessProxy {
    cell: Arc<parking_lot::RwLock<Arc<dyn AccessControl>>>,
}

impl AccessControl for LiveAccessProxy {
    fn can_read(&self, channel: &str, user: &str, host: &str) -> bool {
        self.cell.read().can_read(channel, user, host)
    }
    fn can_write(&self, channel: &str, user: &str, host: &str) -> bool {
        self.cell.read().can_write(channel, user, host)
    }
    fn can_read_creds(&self, channel: &str, creds: &ClientCreds) -> bool {
        self.cell.read().can_read_creds(channel, creds)
    }
    fn can_write_creds(&self, channel: &str, creds: &ClientCreds) -> bool {
        self.cell.read().can_write_creds(channel, creds)
    }
}

impl BridgeProvider {
    pub fn new(db: Arc<PvDatabase>) -> Self {
        Self::new_with_serving(db, true)
    }

    /// Construct a provider whose QSRV2 database/group serving is gated by
    /// `enabled`. The runner passes the result of the pvxs-compatible
    /// `enable2()` decision; `new` defaults to `true` (serving on) for the
    /// common in-process and test paths.
    pub fn new_with_serving(db: Arc<PvDatabase>, enabled: bool) -> Self {
        Self {
            db,
            groups: parking_lot::RwLock::new(HashMap::new()),
            record_cache: tokio::sync::RwLock::new(HashMap::new()),
            access_cell: Arc::new(parking_lot::RwLock::new(Arc::new(AllowAllAccess))),
            channels_created: std::sync::atomic::AtomicU64::new(0),
            ops_get: std::sync::atomic::AtomicU64::new(0),
            ops_put: std::sync::atomic::AtomicU64::new(0),
            ops_subscribe: std::sync::atomic::AtomicU64::new(0),
            group_files: parking_lot::RwLock::new(Vec::new()),
            enabled,
        }
    }

    /// Whether `name` is writable: a record exists, the `.DISP`
    /// field is not 1, and the field referenced by the PV name (if
    /// any sub-field is given) is mutable. Conservative defaults to
    /// `false` for unknown PVs and group PVs (writability isn't
    /// modelled per-group yet).
    pub async fn is_writable(&self, name: &str) -> bool {
        // Group channel: writability per-member is complex; for now
        // assume true if the group is registered.
        if self.groups.read().contains_key(name) {
            return true;
        }
        let (record, _field) = epics_base_rs::server::database::parse_pv_name(name);
        let Some(rec_arc) = self.db.get_record(record).await else {
            // PVA-plugin PVs (NTNDArray) aren't records — caller
            // (qsrv pva_adapter) should consult its own pva_pvs map.
            // Default false here so unknown names refuse PUT upfront.
            return false;
        };
        let inst = rec_arc.read().await;
        !inst.common.disp
    }

    /// Snapshot of cumulative QSRV throughput counters (channels
    /// created, GET / PUT / SUBSCRIBE issued). Mirrors pvxs's
    /// `qStats` aggregate output. Per-channel breakdown is not
    /// currently tracked — pvxs's per-channel counters require a
    /// channel-registry that we can add in a follow-up; for now
    /// callers get the IOC-wide totals.
    pub fn op_stats(&self) -> ProviderOpStats {
        use std::sync::atomic::Ordering::Relaxed;
        ProviderOpStats {
            channels_created: self.channels_created.load(Relaxed),
            gets: self.ops_get.load(Relaxed),
            puts: self.ops_put.load(Relaxed),
            subscribes: self.ops_subscribe.load(Relaxed),
        }
    }

    /// Bump the channel-creation counter. Called from `create_channel_for`.
    pub fn note_channel_created(&self) {
        self.channels_created
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Increment the cumulative GET counter. Channel implementations
    /// call this once per successful get. Held public so external
    /// `Channel` impls (outside this crate) can participate in
    /// `qsrvStats` totals.
    pub fn note_get(&self) {
        self.ops_get
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Increment the cumulative PUT counter.
    pub fn note_put(&self) {
        self.ops_put
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Increment the cumulative SUBSCRIBE counter.
    pub fn note_subscribe(&self) {
        self.ops_subscribe
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Snapshot returned by [`BridgeProvider::op_stats`].
#[derive(Debug, Clone, Default)]
pub struct ProviderOpStats {
    pub channels_created: u64,
    pub gets: u64,
    pub puts: u64,
    pub subscribes: u64,
}

impl BridgeProvider {
    /// Replace the current access-control policy. All AccessContexts
    /// vended from this provider — including those already attached to
    /// existing channels — observe the swap on their next access check.
    pub fn set_access_control(&self, access: Arc<dyn AccessControl>) {
        // Storage is `Arc<RwLock<Arc<dyn AccessControl>>>` so an
        // immutable receiver is correct — `&mut self` blocked
        // hot-reload through `Arc<BridgeProvider>`.
        *self.access_cell.write() = access;
    }

    /// Get a clone of the current access control policy.
    pub fn access_control(&self) -> Arc<dyn AccessControl> {
        self.access_cell.read().clone()
    }

    /// Hand out a live-tracking access wrapper. Use when constructing
    /// an [`AccessContext`] for a new channel so it observes future
    /// `set_access_control` swaps.
    pub fn live_access(&self) -> Arc<dyn AccessControl> {
        Arc::new(LiveAccessProxy {
            cell: self.access_cell.clone(),
        })
    }

    /// Check if a client can write to a channel.
    pub fn can_write(&self, channel: &str, user: &str, host: &str) -> bool {
        self.access_cell.read().can_write(channel, user, host)
    }

    /// Check if a client can read from a channel.
    pub fn can_read(&self, channel: &str, user: &str, host: &str) -> bool {
        self.access_cell.read().can_read(channel, user, host)
    }

    /// Load group PV definitions from a JSON config string. Takes
    /// `&self` (interior mutability) so iocsh commands can call this
    /// against a shared `Arc<BridgeProvider>`.
    pub fn load_group_config(&self, json: &str) -> BridgeResult<()> {
        let defs = super::group_config::parse_group_config(json)?;
        let mut g = self.groups.write();
        // Accumulate field-by-field, not replace. pvxs loads every DB
        // `info(Q:group)` entry and every queued `dbLoadGroup` file into
        // one `GroupConfigProcessor` (groupsourcehooks.cpp:192-207) whose
        // `fieldConfigMap` (groupprocessorcontext.cpp:25-42) collects
        // distinct fields for the same group name across all sources. A
        // bare `insert` here dropped a same-named group loaded earlier
        // (whether from another file or a record `info(Q:group)` tag),
        // turning a valid modular config into an order-dependent partial
        // group. `merge_group_defs` is the same field-keyed path
        // `load_info_group` already uses, so cross-source duplicate field
        // names collapse first-wins (groupconfigprocessor.cpp:221-225).
        super::group_config::merge_group_defs(&mut g, defs);
        Ok(())
    }

    /// Load group PV definitions from a JSON file.
    pub fn load_group_file(&self, path: &str) -> BridgeResult<()> {
        let content = std::fs::read_to_string(path)?;
        self.load_group_config(&content)
    }

    /// Load a group config under its `dbLoadGroup` source identity
    /// `(filename, macros)`, recording which group names this load
    /// places so the same file can later be removed. Mirrors pvxs
    /// `dbLoadGroup(file, macros)` (ioc/groupsourcehooks.cpp:147-183):
    /// re-loading the same `(filename, macros)` first removes the groups
    /// the previous load of that identity placed (pvxs erases the
    /// matching entry before appending), then installs the freshly
    /// parsed definitions. Parsing happens before any mutation so a
    /// parse error leaves the existing registry untouched.
    pub fn load_group_file_tracked(
        &self,
        filename: &str,
        macros: &str,
        json: &str,
    ) -> BridgeResult<()> {
        let defs = super::group_config::parse_group_config(json)?;
        // Drop a prior load of the same source identity (pvxs erases
        // the matching entry before appending the new one).
        self.remove_group_file(filename, macros);
        let names: Vec<String> = defs.iter().map(|d| d.name.clone()).collect();
        {
            let mut g = self.groups.write();
            // Accumulate across files rather than replace — a second file
            // contributing distinct fields to the same group name must add
            // them, not drop the first file's group (finding 40 /
            // groupsourcehooks.cpp:192-207, fieldConfigMap accumulation).
            // Same field-keyed merge as `load_group_config` /
            // `load_info_group`.
            super::group_config::merge_group_defs(&mut g, defs);
        }
        self.group_files.write().push(GroupFileEntry {
            filename: filename.to_string(),
            macros: macros.to_string(),
            names,
        });
        Ok(())
    }

    /// Remove every group placed by a prior `dbLoadGroup(filename,
    /// macros)` with the matching source identity. Mirrors pvxs
    /// `dbLoadGroup("-file.json", "MAC")` (groupsourcehooks.cpp:174-179),
    /// which compares the raw filename and raw macros string. Returns the
    /// number of group definitions removed.
    pub fn remove_group_file(&self, filename: &str, macros: &str) -> usize {
        let mut files = self.group_files.write();
        let mut groups = self.groups.write();
        let mut removed = 0;
        files.retain(|e| {
            if e.filename == filename && e.macros == macros {
                for n in &e.names {
                    if groups.remove(n).is_some() {
                        removed += 1;
                    }
                }
                false
            } else {
                true
            }
        });
        removed
    }

    /// Remove every file-loaded group. Mirrors pvxs `dbLoadGroup("-*")`
    /// (groupsourcehooks.cpp:140-143), which clears the registered file
    /// list. `info(Q:group)` groups are not file-sourced and are left
    /// intact; use [`Self::reset_groups`] to clear everything. Returns
    /// the number of group definitions removed.
    pub fn clear_group_files(&self) -> usize {
        let mut files = self.group_files.write();
        let mut groups = self.groups.write();
        let mut removed = 0;
        for e in files.drain(..) {
            for n in &e.names {
                if groups.remove(n).is_some() {
                    removed += 1;
                }
            }
        }
        removed
    }

    /// Load group definitions from a record's info(Q:group, ...) tag.
    pub fn load_info_group(&self, record_name: &str, json: &str) -> BridgeResult<()> {
        let defs = super::group_config::parse_info_group(record_name, json)?;
        let mut g = self.groups.write();
        super::group_config::merge_group_defs(&mut g, defs);
        Ok(())
    }

    /// Finalize loaded group definitions: validate trigger references
    /// (every `+trigger` field name must exist in the group) and
    /// populate `+all` triggers into explicit field lists. Mirrors
    /// pvxs `GroupConfigProcessor::resolveGroupTriggerReferences` /
    /// `createGroups`. Idempotent — safe to call after every
    /// `dbLoadGroup`. Returns the count of groups finalized; logs
    /// validation warnings via `tracing::warn`.
    pub fn process_groups(&self) -> usize {
        let g = self.groups.read();
        let names: Vec<String> = g.keys().cloned().collect();
        let mut finalized = 0;
        for name in names {
            let def = g.get(&name).cloned().unwrap();
            let field_names: std::collections::HashSet<String> =
                def.members.iter().map(|m| m.field_name.clone()).collect();
            for member in &def.members {
                if let super::group_config::TriggerDef::Fields(refs) = &member.triggers {
                    for r in refs {
                        if !field_names.contains(r) {
                            tracing::warn!(
                                group = %name,
                                member = %member.field_name,
                                trigger = %r,
                                "group trigger references unknown field"
                            );
                        }
                    }
                }
            }
            finalized += 1;
        }
        finalized
    }

    /// Access the underlying database.
    pub fn database(&self) -> &Arc<PvDatabase> {
        &self.db
    }

    /// Snapshot of the current group definitions. Cloned so callers
    /// don't hold the read lock across awaits.
    pub fn groups(&self) -> HashMap<String, GroupPvDef> {
        self.groups.read().clone()
    }

    /// Number of registered group PVs.
    pub fn group_count(&self) -> usize {
        self.groups.read().len()
    }

    /// Whether `name` is registered as a QSRV group composite PV.
    ///
    /// Synchronous, lock-cheap (one `parking_lot::RwLock` read). Used
    /// by the pvalink `local=true` locality check so a link to a
    /// QSRV group PV hosted by this IOC is accepted as local rather
    /// than wrongly rejected with `NotLocal`. Mirrors the group arm
    /// of [`ChannelProvider::channel_find`].
    pub fn has_group_pv(&self, name: &str) -> bool {
        self.groups.read().contains_key(name)
    }

    /// true iff `name` is a registered group PV whose every
    /// member uses the default self-trigger (`+trigger` absent →
    /// [`crate::qsrv::group_config::TriggerDef::SelfOnly`]) or
    /// explicit silence. Such a group's monitor events are *partial*
    /// — each event re-reads only the member that processed — so the
    /// PVA server narrows the wire changed-bitset by diffing
    /// consecutive snapshots. Returns `false` for non-groups and for
    /// groups carrying any explicit `+trigger` member (see
    /// [`GroupPvDef::is_pure_self_trigger`]).
    pub fn group_is_pure_self_trigger(&self, name: &str) -> bool {
        self.groups
            .read()
            .get(name)
            .map(|g| g.is_pure_self_trigger())
            .unwrap_or(false)
    }

    /// Whether this provider hosts `name` as any channel — a QSRV
    /// group composite PV *or* a single-record / simple PV in the
    /// backing database. This is the same name set
    /// [`ChannelProvider::channel_find`] resolves, exposed as an
    /// inherent method so the pvalink `local=true` locality check can
    /// query it without depending on the `ChannelProvider` trait.
    pub async fn hosts_pv(&self, name: &str) -> bool {
        self.channel_find(name).await
    }

    /// True iff a backing record, alias, or simple PV named `name`
    /// exists locally — the resolver-free existence test used to
    /// detect group/record name collisions. Mirrors pvxs
    /// `dbChannelTest(name)` (ioc/groupconfigprocessor.cpp:177):
    /// records + aliases via [`PvDatabase::get_record`], simple PVs
    /// via [`PvDatabase::find_pv`]. Deliberately does NOT invoke the
    /// search resolver (no upstream gateway subscription) — a name
    /// only conflicts when it is *locally* a record, exactly as
    /// `dbChannelTest` tests the local database.
    async fn record_exists(&self, name: &str) -> bool {
        self.db.get_record(name).await.is_some() || self.db.find_pv(name).await.is_some()
    }

    /// Resolve `name` to a group definition only when no backing
    /// record shadows it. pvxs ignores a configured group whose name
    /// collides with a real record name — the record wins and the
    /// group is dropped at `defineGroups`
    /// (ioc/groupconfigprocessor.cpp:170-181), so the surviving
    /// group map (ioc/groupsource.cpp:75-89) never contains a
    /// shadowed name. The Rust daemon (`bin/qsrv_rs.rs`) never runs
    /// the `processGroups` finalize, so the same invariant — a name
    /// is a record OR a group, never both — is enforced here at the
    /// single serving owner. Every group lookup on the serve path
    /// (`channel_find`, `create_channel*`, `channel_list`) goes
    /// through this gate, so the dual meaning cannot reach a client.
    async fn servable_group(&self, name: &str) -> Option<GroupPvDef> {
        let def = self.groups.read().get(name).cloned()?;
        if self.record_exists(name).await {
            return None;
        }
        Some(def)
    }

    /// Drop every registered group definition. Mirrors pvxs
    /// `resetGroups` (groupsourcehooks.cpp:222) — used between
    /// `iocInit` cycles in tests so the second run starts clean. The
    /// underlying records are unaffected.
    pub fn reset_groups(&self) -> usize {
        let mut g = self.groups.write();
        let n = g.len();
        g.clear();
        // Drop the file-source registry too, so a later
        // remove/clear or re-load starts from a clean slate.
        self.group_files.write().clear();
        n
    }

    /// Resolve a single member of a group by `(group_name, field)`.
    /// Returns the backing record name (`record.field`) and the
    /// member's [`super::group_config::FieldMapping`] so callers can
    /// route a get/put through the existing single-record path.
    /// Mirrors pvxs `getGroupField` / `putGroupField`
    /// (groupsource.cpp:408/497) at the lookup level — the actual
    /// db_get / db_put is delegated to the caller.
    pub fn group_member(
        &self,
        group: &str,
        field: &str,
    ) -> Option<(String, super::pvif::FieldMapping)> {
        let g = self.groups.read();
        let def = g.get(group)?;
        let m = def.members.iter().find(|m| m.field_name == field)?;
        Some((m.channel.clone(), m.mapping))
    }

    /// Read a single field of a group as an [`EpicsValue`]. Mirrors
    /// pvxs `getGroupField`. Returns `None` when the group/field
    /// pair is unknown or the backing record can't be read.
    pub async fn get_group_field(
        &self,
        group: &str,
        field: &str,
    ) -> Option<epics_base_rs::types::EpicsValue> {
        let (channel, mapping) = self.group_member(group, field)?;
        if matches!(
            mapping,
            super::pvif::FieldMapping::Structure | super::pvif::FieldMapping::Const
        ) {
            return None;
        }
        self.db.get_pv(&channel).await.ok()
    }

    /// Write a single field of a group. Mirrors pvxs `putGroupField`.
    /// Honors the BridgeProvider's live access policy at
    /// `group_name` granularity (matching whole-group put semantics).
    pub async fn put_group_field(
        &self,
        group: &str,
        field: &str,
        value: epics_base_rs::types::EpicsValue,
        user: &str,
        host: &str,
    ) -> BridgeResult<()> {
        if !self.can_write(group, user, host) {
            return Err(crate::error::BridgeError::PutRejected(format!(
                "write denied for group {group} (user='{user}' host='{host}')"
            )));
        }
        let (channel, mapping) = self
            .group_member(group, field)
            .ok_or_else(|| crate::error::BridgeError::RecordNotFound(format!("{group}.{field}")))?;
        if matches!(
            mapping,
            super::pvif::FieldMapping::Structure | super::pvif::FieldMapping::Const
        ) {
            return Err(crate::error::BridgeError::PutRejected(format!(
                "{group}.{field}: Structure/Const members are not writable"
            )));
        }
        self.db
            .put_pv(&channel, value)
            .await
            .map_err(|e| crate::error::BridgeError::PutRejected(e.to_string()))
    }

    /// Clear the record metadata cache.
    pub async fn clear_cache(&self) {
        self.record_cache.write().await.clear();
    }
}

impl ChannelProvider for BridgeProvider {
    fn provider_name(&self) -> &str {
        "BRIDGE"
    }

    async fn channel_find(&self, name: &str) -> bool {
        // QSRV2 disabled: no DB-backed or group channel is served.
        if !self.enabled {
            return false;
        }
        // A servable group answers as a group; a group name shadowed
        // by a record falls through to the database, so the record is
        // what answers (and resolves on create). See `servable_group`.
        if self.servable_group(name).await.is_some() {
            return true;
        }
        self.db.has_name(name).await
    }

    async fn channel_list(&self) -> Vec<String> {
        // QSRV2 disabled: advertise no DB record / alias / group names.
        if !self.enabled {
            return Vec::new();
        }
        let mut names = self.db.all_record_names().await;
        // PR #336 aliases are independently addressable channel
        // names — a PVA client running channelList expects them so
        // it can connect by alias. `channel_find` / `create_channel`
        // already resolve aliases via has_name/get_record.
        names.extend(self.db.all_alias_names().await);
        // A group name that collides with a record/alias/simple PV is
        // ignored (the record wins), so list it once — as the record.
        // Mirrors pvxs: a shadowed name never enters groupMap at
        // `defineGroups` (ioc/groupconfigprocessor.cpp:177), so
        // groupsource.cpp:75-89 lists only the surviving groups.
        let existing: std::collections::HashSet<String> = names.iter().cloned().collect();
        let group_keys: Vec<String> = self.groups.read().keys().cloned().collect();
        for k in group_keys {
            if !existing.contains(&k) && self.db.find_pv(&k).await.is_none() {
                names.push(k);
            }
        }
        names.sort();
        names
    }

    async fn create_channel(&self, name: &str) -> BridgeResult<AnyChannel> {
        // Default: create with allow-all (anonymous) access. PVA server
        // implementations should call create_channel_for to inject the
        // real client identity.
        self.create_channel_for(name, "", "").await
    }
}

impl BridgeProvider {
    /// Create a channel with explicit user/host (method defaults to empty).
    ///
    /// Used by the PVA server when it knows the connecting client's
    /// authenticated user/host. The trait method [`ChannelProvider::create_channel`]
    /// delegates to this with empty identity (anonymous mode). For full
    /// credential pass-through (method/authority/roles) use
    /// [`create_channel_with_creds`].
    pub async fn create_channel_for(
        &self,
        name: &str,
        user: &str,
        host: &str,
    ) -> BridgeResult<AnyChannel> {
        self.create_channel_with_creds(
            name,
            ClientCreds {
                user: user.to_string(),
                host: host.to_string(),
                ..Default::default()
            },
        )
        .await
    }

    /// Create a channel with the full [`ClientCreds`] set.
    ///
    /// Carries method, authority, and roles into the channel's
    /// [`AccessContext`] so `AcfAccessControl` can evaluate
    /// METHOD/AUTHORITY rules and role-based UAG entries.
    pub async fn create_channel_with_creds(
        &self,
        name: &str,
        creds: ClientCreds,
    ) -> BridgeResult<AnyChannel> {
        // QSRV2 disabled: refuse to construct any DB-backed / group
        // channel. Mirrors pvxs not registering the single/group sources
        // (iochooks.cpp:461-496); the not-counted early return keeps the
        // `qsrvStats` channel tally honest about what was actually served.
        if !self.enabled {
            return Err(BridgeError::ChannelNotFound(name.to_string()));
        }
        self.note_channel_created();
        let access_ctx = AccessContext::with_creds(self.live_access(), creds);

        // Check group PVs first — but a group name that collides with
        // a real record is ignored (the record wins), matching pvxs
        // `defineGroups` dbChannelTest (ioc/groupconfigprocessor.cpp:177).
        if let Some(def) = self.servable_group(name).await {
            return Ok(AnyChannel::Group(
                GroupChannel::new(self.db.clone(), def).with_access(access_ctx),
            ));
        } else if self.groups.read().contains_key(name) {
            tracing::warn!(
                group = %name,
                "QSRV group name conflicts with record name; serving the record and ignoring the group"
            );
        }

        // Single record (or `record.FIELD`) channel.
        // Cache by the full requested name so field PVs do not
        // poison the record's VAL-shaped cache entry.
        //
        // peel off any pvxs `PV.VAL{...}` channel-filter
        // JSON suffix before record/field resolution. The filter
        // chain stays on `BridgeChannel`; resolution and cache
        // lookup use the record-path-only form.
        let parsed = epics_base_rs::server::database::filters::split_channel_name(name);
        let resolution_name = parsed.record_path.as_str();
        let (record_name, field) = epics_base_rs::server::database::parse_pv_name(resolution_name);
        let field_upper = field.to_ascii_uppercase();

        // Cache hit only when the requested name has no filter
        // suffix — a filtered subscription must take a fresh
        // construction path through `BridgeChannel::new` so the
        // per-channel filter chain is parsed.
        if parsed.json_suffix.is_none() {
            let cache = self.record_cache.read().await;
            if let Some(&(nt_type, value_dbf)) = cache.get(name) {
                return Ok(AnyChannel::Single(
                    BridgeChannel::from_cached(
                        self.db.clone(),
                        name.to_string(),
                        record_name.to_string(),
                        field_upper,
                        nt_type,
                        value_dbf,
                    )
                    .with_access(access_ctx),
                ));
            }
        }

        // Cache miss — introspect and create. Use the
        // record-path-only form so `has_name` resolves the
        // unfiltered record/field; the filter suffix is consumed
        // by `BridgeChannel::new` itself.
        if self.db.has_name(resolution_name).await {
            let channel = BridgeChannel::new(self.db.clone(), name).await?;

            // Populate cache keyed by the full PV identity so a
            // subsequent `record.FIELD` hit isn't served from a
            // `record`-only entry (or vice versa). Filtered names
            // are NOT cached — each filtered subscription parses
            // its own chain.
            if parsed.json_suffix.is_none() {
                let mut cache = self.record_cache.write().await;
                cache.insert(name.to_string(), (channel.nt_type(), channel.value_dbf()));
            }

            return Ok(AnyChannel::Single(channel.with_access(access_ctx)));
        }

        Err(BridgeError::ChannelNotFound(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A QSRV group whose name collides with a real record must be
    /// ignored — the record wins, mirroring pvxs `defineGroups`
    /// dbChannelTest (ioc/groupconfigprocessor.cpp:177). A non-colliding
    /// group stays servable.
    #[tokio::test]
    async fn group_name_conflicting_with_record_is_ignored() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("REC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        let provider = BridgeProvider::new(db);
        // "REC" shadows the record; "GRP:ONLY" has no backing record.
        provider
            .load_group_config(
                r#"{
                    "REC":      { "value": { "+channel": "REC.VAL",   "+type": "plain" } },
                    "GRP:ONLY": { "value": { "+channel": "OTHER.VAL", "+type": "plain" } }
                }"#,
            )
            .unwrap();

        // create_channel resolves the record, not the group.
        match provider.create_channel("REC").await.unwrap() {
            AnyChannel::Single(_) => {}
            AnyChannel::Group(_) => panic!("REC must resolve to the record, not the group"),
        }
        // The non-colliding group is still served as a group.
        match provider.create_channel("GRP:ONLY").await.unwrap() {
            AnyChannel::Group(_) => {}
            AnyChannel::Single(_) => panic!("GRP:ONLY has no backing record; must be a group"),
        }

        assert!(provider.channel_find("REC").await);

        // channel_list emits exactly one "REC" (the record), not a
        // duplicate from the shadowed group; the live group survives.
        let list = provider.channel_list().await;
        assert_eq!(
            list.iter().filter(|n| n.as_str() == "REC").count(),
            1,
            "REC must appear once (record), not duplicated by the group"
        );
        assert!(
            list.iter().any(|n| n == "GRP:ONLY"),
            "the non-colliding group must still be listed"
        );
    }

    /// `dbLoadGroup` source identity: a tracked file load can later be
    /// removed by its `(filename, macros)` identity, re-loading the same
    /// identity replaces rather than duplicates, and `-*` clears all
    /// file-loaded groups — mirroring pvxs groupsourcehooks.cpp:133-183.
    #[tokio::test]
    async fn group_file_load_is_removable_by_source_identity() {
        use epics_base_rs::server::database::PvDatabase;

        let provider = BridgeProvider::new(Arc::new(PvDatabase::new()));
        let ga = r#"{ "GA": { "v": { "+channel": "X.VAL", "+type": "plain" } } }"#;
        let gb = r#"{ "GB": { "v": { "+channel": "Y.VAL", "+type": "plain" } } }"#;

        provider.load_group_file_tracked("a.json", "", ga).unwrap();
        provider
            .load_group_file_tracked("b.json", "M=1", gb)
            .unwrap();
        assert_eq!(provider.group_count(), 2);

        // Removal is identity-based: wrong macros string does not match.
        assert_eq!(provider.remove_group_file("b.json", ""), 0);
        assert_eq!(provider.group_count(), 2);

        // Exact (filename, macros) removes only that file's groups.
        assert_eq!(provider.remove_group_file("b.json", "M=1"), 1);
        assert!(provider.has_group_pv("GA"));
        assert!(!provider.has_group_pv("GB"));

        // Re-loading the same identity replaces (no duplicate entry).
        provider.load_group_file_tracked("a.json", "", ga).unwrap();
        assert_eq!(provider.group_count(), 1);

        // `-*` clears every file-loaded group.
        assert_eq!(provider.clear_group_files(), 1);
        assert_eq!(provider.group_count(), 0);
    }

    /// Two `dbLoadGroup` files each contribute a distinct field to the
    /// SAME group name; pvxs accumulates both into one runtime group
    /// (all files loaded into one GroupConfigProcessor,
    /// groupsourcehooks.cpp:192-207). Before the fix the second file's
    /// load did a bare `insert` that replaced the first, dropping file
    /// A's field and making the group order-dependent and partial.
    #[tokio::test]
    async fn group_files_accumulate_distinct_fields_for_same_group() {
        use epics_base_rs::server::database::PvDatabase;

        let provider = BridgeProvider::new(Arc::new(PvDatabase::new()));
        let file_a = r#"{ "GRP": { "a": { "+channel": "RA.VAL", "+type": "plain" } } }"#;
        let file_b = r#"{ "GRP": { "b": { "+channel": "RB.VAL", "+type": "plain" } } }"#;

        provider
            .load_group_file_tracked("a.json", "", file_a)
            .unwrap();
        provider
            .load_group_file_tracked("b.json", "", file_b)
            .unwrap();

        let def = provider.groups().get("GRP").cloned().expect("GRP exists");
        let names: Vec<&str> = def.members.iter().map(|m| m.field_name.as_str()).collect();
        assert_eq!(
            def.members.len(),
            2,
            "both fields must be present: {names:?}"
        );
        assert!(names.contains(&"a"), "file A field must survive: {names:?}");
        assert!(
            names.contains(&"b"),
            "file B field must be added: {names:?}"
        );
    }

    /// Access control that denies all writes, allows all reads.
    struct ReadOnly;
    impl AccessControl for ReadOnly {
        fn can_write(&self, _: &str, _: &str, _: &str) -> bool {
            false
        }
    }

    /// Access control that denies a specific channel name.
    struct DenySpecific(String);
    impl AccessControl for DenySpecific {
        fn can_read(&self, channel: &str, _: &str, _: &str) -> bool {
            channel != self.0
        }
        fn can_write(&self, channel: &str, _: &str, _: &str) -> bool {
            channel != self.0
        }
    }

    /// Regression: AcfAccessControl bridges qsrv's
    /// `AccessControl` trait to epics-base ACF so a BridgeProvider
    /// configured from a parsed `.acf` file enforces the same
    /// policy as the CA / PVA servers. Pre-fix, qsrv's AccessControl
    /// was independent of ACF: a site that loaded an .acf file at
    /// CA-server level still had to write a custom AccessControl
    /// impl for qsrv channels, otherwise PVA-side QSRV was
    /// effectively allow-all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn acf_access_control_gates_qsrv_channels() {
        use epics_base_rs::server::access_security::parse_acf;
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;

        // Only `admin` may WRITE; everyone may READ.
        let acf_text = r#"
UAG(admins) { admin }
ASG(SECURE) {
    RULE(1, READ)
    RULE(1, WRITE) { UAG(admins) }
}
"#;
        let cfg = parse_acf(acf_text).unwrap();

        let db = Arc::new(PvDatabase::new());
        db.add_record("AI:SEC", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let rec = db.get_record("AI:SEC").await.unwrap();
        rec.write().await.common.asg = "SECURE".to_string();

        let acl = AcfAccessControl::new(db.clone(), cfg);
        // Anyone can read.
        assert!(acl.can_read("AI:SEC", "guest", "anywhere"));
        assert!(acl.can_read("AI:SEC", "admin", "anywhere"));
        // Only admin can write.
        assert!(acl.can_write("AI:SEC", "admin", "anywhere"));
        assert!(!acl.can_write("AI:SEC", "guest", "anywhere"));
    }

    /// Regression: AcfAccessControl must honor method, authority,
    /// roles, and field ASL on all four axes independently.
    ///
    /// Upstream parity:
    /// - credential building: pvxs/ioc/credentials.cpp:31-45
    /// - field ASL: pvxs/ioc/securityclient.cpp:25 (`dbChannelFldDes(ch)->as_level`)
    /// - any_of semantics: pvxs/ioc/securityclient.cpp:42-45
    /// - ASL rule comparison: epics-base/modules/libcom/src/as/asLibRoutines.c:1006
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn br_r4_acf_method_authority_roles_field_asl() {
        use epics_base_rs::server::access_security::parse_acf;
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;

        let acf_text = r#"
UAG(writers)   { alice }
UAG(ops_role)  { "role/ops" }
ASG(ASL_GATED) {
    RULE(1, READ)
    RULE(0, WRITE) { UAG(writers) }
}
ASG(METHOD_GATED) {
    RULE(1, READ)
    RULE(1, WRITE) { METHOD("x509") }
}
ASG(AUTHORITY_GATED) {
    RULE(1, READ)
    RULE(1, WRITE) { AUTHORITY("Trusted Root") }
}
ASG(ROLE_GATED) {
    RULE(1, READ)
    RULE(1, WRITE) { UAG(ops_role) }
}
"#;
        let cfg = parse_acf(acf_text).unwrap();
        let db = Arc::new(PvDatabase::new());

        for name in &["AI:ASL0", "AI:ASL1", "AI:METH", "AI:AUTH", "AI:ROLE"] {
            db.add_record(name, Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
        }

        // Axis 4 — field ASL:
        //   AI:ASL0 has asl=0; RULE(0, WRITE) applies → alice can write.
        //   AI:ASL1 has asl=1; RULE(0, WRITE) is skipped (1 > 0) → alice CANNOT write.
        //   Pre-fix: hardcoded asl=0 caused AI:ASL1 write to return true (WRONG).
        {
            let rec = db.get_record("AI:ASL0").await.unwrap();
            let mut w = rec.write().await;
            w.common.asg = "ASL_GATED".to_string();
            w.common.asl = 0;
        }
        {
            let rec = db.get_record("AI:ASL1").await.unwrap();
            let mut w = rec.write().await;
            w.common.asg = "ASL_GATED".to_string();
            w.common.asl = 1;
        }
        {
            let rec = db.get_record("AI:METH").await.unwrap();
            rec.write().await.common.asg = "METHOD_GATED".to_string();
        }
        {
            let rec = db.get_record("AI:AUTH").await.unwrap();
            rec.write().await.common.asg = "AUTHORITY_GATED".to_string();
        }
        {
            let rec = db.get_record("AI:ROLE").await.unwrap();
            rec.write().await.common.asg = "ROLE_GATED".to_string();
        }

        let acl = AcfAccessControl::new(db.clone(), cfg);

        // ── Axis 4: field ASL ────────────────────────────────────────────
        // ASL=0 record: RULE(0, WRITE) applies.
        assert!(
            acl.can_write("AI:ASL0", "alice", "h"),
            "ASL=0: alice should be allowed to write"
        );
        // ASL=1 record: RULE(0, WRITE) is skipped (epics-base asLibRoutines.c:1006:
        // `if(pasgclient->level > pasgrule->level) goto next_rule`).
        assert!(
            !acl.can_write("AI:ASL1", "alice", "h"),
            "ASL=1: RULE(0,WRITE) must be skipped → write denied"
        );

        // ── Axis 1: method ───────────────────────────────────────────────
        // x509 client: METHOD("x509") rule matches → write allowed.
        // Pre-fix: hardcoded method="anonymous" caused x509 clients to be denied.
        let x509_creds = ClientCreds {
            user: "alice".to_string(),
            host: "h".to_string(),
            method: "x509".to_string(),
            authority: String::new(),
            roles: Vec::new(),
        };
        assert!(
            acl.can_write_creds("AI:METH", &x509_creds),
            "x509 client must match METHOD(\"x509\") rule"
        );
        let ca_creds = ClientCreds {
            user: "alice".to_string(),
            host: "h".to_string(),
            method: "ca".to_string(),
            authority: String::new(),
            roles: Vec::new(),
        };
        assert!(
            !acl.can_write_creds("AI:METH", &ca_creds),
            "ca client must NOT match METHOD(\"x509\")-only rule"
        );

        // ── Axis 2: authority ────────────────────────────────────────────
        let trusted_creds = ClientCreds {
            user: "alice".to_string(),
            host: "h".to_string(),
            method: "x509".to_string(),
            authority: "Trusted Root".to_string(),
            roles: Vec::new(),
        };
        assert!(
            acl.can_write_creds("AI:AUTH", &trusted_creds),
            "correct authority must match AUTHORITY(\"Trusted Root\")"
        );
        let other_ca_creds = ClientCreds {
            user: "alice".to_string(),
            host: "h".to_string(),
            method: "x509".to_string(),
            authority: "Other CA".to_string(),
            roles: Vec::new(),
        };
        assert!(
            !acl.can_write_creds("AI:AUTH", &other_ca_creds),
            "wrong authority must NOT match"
        );

        // ── Axis 3: roles ────────────────────────────────────────────────
        // "role/ops" credential string matches UAG(ops_role) { role/ops }.
        // Pre-fix: roles were not built into credential strings at all.
        let ops_creds = ClientCreds {
            user: "bob".to_string(),
            host: "h".to_string(),
            method: "ca".to_string(),
            authority: String::new(),
            roles: vec!["ops".to_string()],
        };
        assert!(
            acl.can_write_creds("AI:ROLE", &ops_creds),
            "client with role 'ops' must match UAG entry 'role/ops'"
        );
        let no_role_creds = ClientCreds {
            user: "bob".to_string(),
            host: "h".to_string(),
            method: "ca".to_string(),
            authority: String::new(),
            roles: Vec::new(),
        };
        assert!(
            !acl.can_write_creds("AI:ROLE", &no_role_creds),
            "client without 'ops' role must NOT write to ROLE_GATED"
        );
    }

    #[test]
    fn access_context_allow_all() {
        let ctx = AccessContext::allow_all();
        assert!(ctx.can_read("ANY"));
        assert!(ctx.can_write("ANY"));
    }

    #[test]
    fn access_context_read_only() {
        let ctx = AccessContext::anonymous(Arc::new(ReadOnly));
        assert!(ctx.can_read("X"));
        assert!(!ctx.can_write("X"));
    }

    #[test]
    fn access_context_with_identity() {
        let ctx =
            AccessContext::with_identity(Arc::new(AllowAllAccess), "alice".into(), "host1".into());
        assert_eq!(ctx.user, "alice");
        assert_eq!(ctx.host, "host1");
    }

    #[test]
    fn access_context_deny_specific() {
        let ctx = AccessContext::anonymous(Arc::new(DenySpecific("SECRET".to_string())));
        assert!(ctx.can_read("PUBLIC"));
        assert!(!ctx.can_read("SECRET"));
        assert!(ctx.can_write("PUBLIC"));
        assert!(!ctx.can_write("SECRET"));
    }

    #[test]
    fn provider_set_access_control() {
        let db = Arc::new(PvDatabase::new());
        let provider = BridgeProvider::new(db);
        // Default policy
        assert!(provider.can_read("X", "u", "h"));
        assert!(provider.can_write("X", "u", "h"));

        // Swap to read-only
        provider.set_access_control(Arc::new(ReadOnly));
        assert!(provider.can_read("X", "u", "h"));
        assert!(!provider.can_write("X", "u", "h"));
    }

    #[tokio::test]
    async fn read_only_channel_blocks_writes() {
        // Construct a channel directly with from_cached + with_access(ReadOnly).
        // We bypass create_channel here because BridgeChannel::new() requires
        // a real record in the database (which is non-trivial test setup);
        // the access enforcement path is identical regardless.
        let db = Arc::new(PvDatabase::new());
        let access = AccessContext::anonymous(Arc::new(ReadOnly));
        let ch = BridgeChannel::from_cached(
            db,
            "PROT".to_string(),
            "PROT".to_string(),
            "VAL".to_string(),
            super::super::pvif::NtType::Scalar,
            epics_base_rs::types::DbFieldType::Double,
        )
        .with_access(access);

        let mut put_struct = PvStructure::new("epics:nt/NTScalar:1.0");
        put_struct.fields.push((
            "value".into(),
            epics_pva_rs::pvdata::PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Double(2.0)),
        ));
        let result = ch.put(&put_struct).await;
        assert!(result.is_err(), "expected access denied");
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("denied"),
            "expected denial message, got: {err}"
        );
    }

    #[tokio::test]
    async fn deny_specific_channel_blocks_named() {
        let db = Arc::new(PvDatabase::new());
        let access = AccessContext::anonymous(Arc::new(DenySpecific("BLOCKED".to_string())));
        let ch = BridgeChannel::from_cached(
            db.clone(),
            "BLOCKED".to_string(),
            "BLOCKED".to_string(),
            "VAL".to_string(),
            super::super::pvif::NtType::Scalar,
            epics_base_rs::types::DbFieldType::Double,
        )
        .with_access(access);

        let req = PvStructure::new("");
        let result = ch.get(&req).await;
        assert!(result.is_err(), "expected read denied for BLOCKED");

        // A different channel name with the same policy should NOT be blocked
        let ok_access = AccessContext::anonymous(Arc::new(DenySpecific("BLOCKED".to_string())));
        let ch2 = BridgeChannel::from_cached(
            db,
            "ALLOWED".to_string(),
            "ALLOWED".to_string(),
            "VAL".to_string(),
            super::super::pvif::NtType::Scalar,
            epics_base_rs::types::DbFieldType::Double,
        )
        .with_access(ok_access);
        // Get will fail because no record exists, but it should fail with
        // RecordNotFound, not access denied.
        let result = ch2.get(&req).await;
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            !err.contains("denied"),
            "ALLOWED channel should pass access check, got: {err}"
        );
    }

    /// Read-deny access control: blocks all reads, allows all writes.
    /// Used to verify monitor enforcement (which is read).
    struct WriteOnly;
    impl AccessControl for WriteOnly {
        fn can_read(&self, _: &str, _: &str, _: &str) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn create_monitor_blocks_when_read_denied() {
        let db = Arc::new(PvDatabase::new());
        let access = AccessContext::anonymous(Arc::new(WriteOnly));
        let ch = BridgeChannel::from_cached(
            db,
            "PROT".to_string(),
            "PROT".to_string(),
            "VAL".to_string(),
            super::super::pvif::NtType::Scalar,
            epics_base_rs::types::DbFieldType::Double,
        )
        .with_access(access);

        // create_monitor must reject before even constructing the BridgeMonitor.
        // AnyMonitor doesn't implement Debug so we destructure manually.
        let result = ch.create_monitor().await;
        match result {
            Ok(_) => panic!("expected monitor create denied, got Ok"),
            Err(e) => {
                let err = format!("{e}");
                assert!(
                    err.contains("monitor create denied"),
                    "expected monitor denial message, got: {err}"
                );
            }
        }
    }

    /// LiveAccessProxy regression: an AccessContext vended from
    /// `BridgeProvider::live_access()` must observe `set_access_control`
    /// on its very next can_read / can_write call, without channel
    /// recreation. The earlier `Arc<dyn AccessControl>` direct-clone
    /// pattern pinned each channel to the policy at creation time.
    #[test]
    fn live_access_proxy_observes_policy_swap() {
        let db = Arc::new(PvDatabase::new());
        let provider = BridgeProvider::new(db);

        // Hand out an AccessContext bound to the LIVE proxy. Default is
        // AllowAllAccess.
        let ctx =
            AccessContext::with_identity(provider.live_access(), "alice".into(), "host1".into());
        assert!(ctx.can_read("ANY"));
        assert!(ctx.can_write("ANY"));

        // Swap to a deny-specific policy AFTER the context was created.
        provider.set_access_control(Arc::new(DenySpecific("SECRET".into())));
        assert!(ctx.can_read("ALLOWED"));
        assert!(!ctx.can_read("SECRET"), "swap must be observed live");
        assert!(!ctx.can_write("SECRET"));

        // Swap to read-only — same context, fresh decision.
        provider.set_access_control(Arc::new(ReadOnly));
        assert!(ctx.can_read("X"));
        assert!(
            !ctx.can_write("X"),
            "policy swap must take effect immediately"
        );

        // Swap back to allow-all — proxy still tracks.
        provider.set_access_control(Arc::new(AllowAllAccess));
        assert!(ctx.can_write("X"));
    }

    #[tokio::test]
    async fn bridge_monitor_start_blocks_when_read_denied() {
        // Defense-in-depth: even if a monitor is constructed via with_access
        // bypassing create_monitor, start() must still enforce.
        let db = Arc::new(PvDatabase::new());
        let access = AccessContext::anonymous(Arc::new(WriteOnly));
        let mut monitor = super::super::monitor::BridgeMonitor::new(
            db,
            "PROT".to_string(),
            "VAL".to_string(),
            super::super::pvif::NtType::Scalar,
        )
        .with_access(access);

        let result = monitor.start().await;
        assert!(result.is_err(), "expected monitor start denied");
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("monitor read denied"),
            "expected start denial, got: {err}"
        );
    }

    /// a QSRV access context built through the legacy
    /// no-method path (`AccessContext::anonymous`, `with_identity`,
    /// the 3-arg `AccessControl::can_read`/`can_write`) carries an
    /// empty `method`. pvxs sets an unauthenticated client's method
    /// to `"anonymous"` (`serverconn.cpp:78`), so an ACF that grants
    /// anonymous access through a `METHOD("anonymous")`-scoped rule
    /// must still apply on that path. The branch passed an empty
    /// method, which `check_access_method` cannot match against a
    /// `METHOD("anonymous")` list — silently denying the legacy path.
    ///
    /// Fails before the fix (empty method → no rule match → denied),
    /// passes after (`level_for_creds` normalizes empty → `anonymous`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mr_r12_legacy_path_matches_method_anonymous_rule() {
        use epics_base_rs::server::access_security::parse_acf;
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;

        // The ASG grants READ *only* through a METHOD("anonymous")
        // rule. A legacy/anonymous QSRV context must match it.
        let acf_text = r#"
ASG(ANON_GATED) {
    RULE(1, READ) { METHOD("anonymous") }
}
"#;
        let cfg = parse_acf(acf_text).unwrap();
        let db = Arc::new(PvDatabase::new());
        db.add_record("AI:ANON", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        {
            let rec = db.get_record("AI:ANON").await.unwrap();
            rec.write().await.common.asg = "ANON_GATED".to_string();
        }
        let acl = AcfAccessControl::new(db.clone(), cfg);

        // Legacy 3-arg path (create_channel / create_channel_for
        // funnel through these). Empty method on the branch.
        assert!(
            acl.can_read("AI:ANON", "alice", "h"),
            "legacy can_read must match METHOD(\"anonymous\") rule"
        );

        // Explicit-identity context: also a no-method legacy path.
        let id_creds = ClientCreds {
            user: "alice".to_string(),
            host: "h".to_string(),
            method: String::new(),
            authority: String::new(),
            roles: Vec::new(),
        };
        assert!(
            acl.can_read_creds("AI:ANON", &id_creds),
            "empty-method ClientCreds must match METHOD(\"anonymous\") rule"
        );

        // A real authenticated method (e.g. ca) must NOT be coerced
        // into anonymous — the normalization only fills an *empty*
        // method, so a ca client still fails the anonymous-only rule.
        let ca_creds = ClientCreds {
            user: "alice".to_string(),
            host: "h".to_string(),
            method: "ca".to_string(),
            authority: String::new(),
            roles: Vec::new(),
        };
        assert!(
            !acl.can_read_creds("AI:ANON", &ca_creds),
            "an explicit 'ca' method must NOT match a METHOD(\"anonymous\")-only rule"
        );
    }
}
