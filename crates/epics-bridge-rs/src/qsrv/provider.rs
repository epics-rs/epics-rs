//! ChannelProvider trait and BridgeProvider implementation.
//!
//! Corresponds to C++ QSRV's `PDBProvider` (pdb.h/pdb.cpp).
//!
//! The trait definitions here are temporary — they will move to `epics-pva-rs`
//! once that crate's native PVA server exposes them directly.

// RTEMS-EXEC-MODEL-ALLOW(21): checked - these run and pass in the feature-ON suite.

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

/// Outcome of a QSRV write authorization.
///
/// Produced once by the access layer — the SINGLE source of both "may
/// this client write" (`allowed`) and "did the matched ACF/ASG rule
/// carry `TRAPWRITE`" (`rule_was_trap`). The QSRV PUT path emits the
/// `asTrapWrite` put-log event iff `rule_was_trap`, and never re-derives
/// the trap flag at the emission site (see `super::trap_write`).
///
/// Mirrors C `asComputePvt` tracking `access` and `trapMask` together
/// (`asLibRoutines.c:983-1048`) and pvxs `SecurityClient` exposing both
/// `canWrite()` and the trap mask its `SecurityLogger` consults
/// (`pvxs/ioc/securityclient.cpp`, `pvxs/ioc/securitylogger.h:44`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WriteGrant {
    /// True iff the matched ACF/ASG rule grants write access.
    pub allowed: bool,
    /// True iff the matched (granting) rule carried the `TRAPWRITE`
    /// option. Always `false` when `!allowed` (C `asComputePvt` copies
    /// `trapMask` only on a rule that raises access, `asLibRoutines.c:
    /// 1041-1048`).
    pub rule_was_trap: bool,
}

/// Access control interface for PVA channels.
///
/// Corresponds to C++ QSRV's per-channel ASCLIENT checks.
/// Default implementation allows all access.
///
/// The methods are **async** because an ACF answer depends on the record's
/// `ASG`/`ASL` fields, which live behind the database's async locks
/// ([`AcfAccessControl`]). A sync trait here would force each impl to invent a
/// blocking bridge to reach that state, and a blocking bridge is only sound on
/// some runtime flavors — which made the access decision a function of how the
/// server's runtime happened to be built. Every caller of these methods is
/// already an `async fn`, so awaiting the answer costs nothing and removes the
/// bridge (and the wrong-answer fallback it needed) entirely.
#[async_trait::async_trait]
pub trait AccessControl: Send + Sync {
    /// Check if the client can read this channel.
    async fn can_read(&self, _channel: &str, _user: &str, _host: &str) -> bool {
        true
    }

    /// Check if the client can write to this channel.
    async fn can_write(&self, _channel: &str, _user: &str, _host: &str) -> bool {
        true
    }

    /// Method/authority/roles-aware read check.
    ///
    /// Default forwards to `can_read(channel, creds.user, creds.host)` so
    /// impls that do not need method/authority/roles need not override.
    /// `AcfAccessControl` overrides to pass the full credential set to
    /// `check_access_method`.
    async fn can_read_creds(&self, channel: &str, creds: &ClientCreds) -> bool {
        self.can_read(channel, &creds.user, &creds.host).await
    }

    /// Method/authority/roles-aware write check.
    async fn can_write_creds(&self, channel: &str, creds: &ClientCreds) -> bool {
        self.can_write(channel, &creds.user, &creds.host).await
    }

    /// Authorize a write and surface the matched ACF/ASG rule's
    /// `TRAPWRITE` flag in one result — the single source the QSRV PUT
    /// path uses both to gate the write and to decide `asTrapWrite`
    /// put-logging (see [`WriteGrant`] and `super::trap_write`).
    ///
    /// Default: forward to [`Self::can_write_creds`] with
    /// `rule_was_trap = false` — an impl with no ACF rule has no trap
    /// mask, matching C `asComputePvt` leaving `trapMask = 0`.
    /// [`AcfAccessControl`] overrides this to read the granting rule's
    /// trap flag via `check_access_method_trap`.
    async fn write_grant(&self, channel: &str, creds: &ClientCreds) -> WriteGrant {
        WriteGrant {
            allowed: self.can_write_creds(channel, creds).await,
            rule_was_trap: false,
        }
    }
}

/// Default access control that allows all operations.
pub struct AllowAllAccess;
impl AccessControl for AllowAllAccess {}

/// AccessControl backed by an epics-base [`epics_base_rs::server::access_security::AccessSecurityConfig`].
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
    /// C `asAddClient` keeps one computed ASGCLIENT per channel and every
    /// put is a bit test; pvxs caches the client's computed access on the
    /// channel at the first PUT (pvxs 3c0154b, issue #176). The Rust QSRV
    /// channel object is rebuilt per operation, so the computed
    /// (level, trap) is cached here on the policy owner instead, keyed by
    /// (channel, full credential identity). Invalidation is by
    /// construction: an ACF reload swaps in a whole new
    /// `AcfAccessControl` (`set_access_control`), and a `dbPut
    /// record.ASG` moves
    /// [`asg_change_generation`](epics_base_rs::server::access_security::asg_change_generation)
    /// which every entry
    /// snapshots. The rule walk here never evaluates CALC against live
    /// INP* values (`check_access_method_trap` fails those closed), so an
    /// entry cannot go stale through a DB value change.
    grant_cache: parking_lot::RwLock<HashMap<GrantKey, CachedAccess>>,
}

/// Full identity a cached access computation depends on. `roles` is part
/// of the key — a re-auth that only changes role claims must miss.
#[derive(Clone, PartialEq, Eq, Hash)]
struct GrantKey {
    channel: String,
    user: String,
    host: String,
    method: String,
    authority: String,
    roles: Vec<String>,
}

impl GrantKey {
    fn new(channel: &str, creds: &ClientCreds) -> Self {
        Self {
            channel: channel.to_string(),
            user: creds.user.clone(),
            host: creds.host.clone(),
            method: creds.method.clone(),
            authority: creds.authority.clone(),
            roles: creds.roles.clone(),
        }
    }
}

#[derive(Clone, Copy)]
struct CachedAccess {
    /// [`asg_change_generation`](epics_base_rs::server::access_security::asg_change_generation)
    /// snapshot taken BEFORE the (ASG, ASL)
    /// resolve, so a change racing the compute invalidates the entry on
    /// its next read instead of being lost.
    asg_generation: u64,
    level: AccessLevelLite,
    /// `TRAPWRITE` flag of the first credential's granting rule; only
    /// meaningful when `level` is `ReadWrite`.
    write_trap: bool,
}

/// Bound on distinct (channel × credential) cache entries. C's ASGCLIENT
/// list is bounded by live channels; this cache is name-keyed, so a
/// client cycling names/identities could otherwise grow it without
/// limit. Overflow flushes the whole map — crude, but a full re-walk per
/// entry is exactly the pre-cache behaviour.
const GRANT_CACHE_CAP: usize = 4096;

impl AcfAccessControl {
    pub fn new(
        db: Arc<epics_base_rs::server::database::PvDatabase>,
        cfg: epics_base_rs::server::access_security::AccessSecurityConfig,
    ) -> Self {
        Self {
            db,
            cfg: Arc::new(cfg),
            grant_cache: parking_lot::RwLock::new(HashMap::new()),
        }
    }

    /// Resolve (ASG name, field ASL) for a channel from the backing database.
    ///
    /// Mirrors pvxs `ioc/securityclient.cpp:25` — `asAddClient` is passed
    /// `dbChannelFldDes(ch)->as_level` as the ASL. Our Rust model stores a
    /// per-record `common.asl` (same approach as
    /// `epics-ca-rs/src/server/tcp.rs:459`).
    /// The `DEFAULT` ASG is the fallback for a channel that genuinely has no
    /// record-level ASG — a simple PV, or a name the database does not know. It
    /// must never stand in for an ASG the code failed to look up: doing so
    /// evaluates the wrong access group, which for a record in a read-only
    /// group means granting a write the ACF denies.
    /// The third element is whether the channel resolved to a known
    /// record. Only a known-record resolution may be cached: an unknown
    /// name's `DEFAULT` fallback would otherwise keep serving `DEFAULT`
    /// after the record appears (post-init `dbLoadRecords`), with no
    /// ASG-field put to move the invalidation generation.
    async fn resolve_asg_and_asl(&self, channel: &str) -> (String, u8, bool) {
        let (record_name, _field) = epics_base_rs::server::database::parse_pv_name(channel);
        if let Some(rec) = self.db.get_record(record_name) {
            let inst = rec.read();
            return (
                inst.common.access_group().to_string(),
                inst.common.asl,
                true,
            );
        }
        ("DEFAULT".to_string(), 0u8, false)
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
    async fn level_for_creds(&self, channel: &str, creds: &ClientCreds) -> AccessLevelLite {
        self.computed_access(channel, creds).await.level
    }

    /// The single rule evaluation every check on this policy routes
    /// through, cached per (channel, credential identity) — see the
    /// [`Self::grant_cache`] field doc for the C/pvxs model and the
    /// invalidation reasoning.
    async fn computed_access(&self, channel: &str, creds: &ClientCreds) -> CachedAccess {
        use epics_base_rs::server::access_security::AccessLevel;
        let generation = epics_base_rs::server::access_security::asg_change_generation();
        let key = GrantKey::new(channel, creds);
        if let Some(hit) = self.grant_cache.read().get(&key)
            && hit.asg_generation == generation
        {
            return *hit;
        }
        let (asg, asl, record_known) = self.resolve_asg_and_asl(channel).await;
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
        // pvxs `SecurityClient::canWrite` is `any_of` over the credential
        // list (`pvxs/ioc/securityclient.cpp:42-45`): the first
        // credential that grants `ReadWrite` wins, and that rule's
        // `trapMask` is the grant's trap flag — C `asComputePvt` copies
        // `trapMask` from the rule that set the granted access
        // (`asLibRoutines.c:1041-1048`). A denied write carries
        // `rule_was_trap = false` (`asComputePvt` leaves `trapMask = 0`
        // on a `NoAccess` outcome).
        let mut entry = CachedAccess {
            asg_generation: generation,
            level: AccessLevelLite::None,
            write_trap: false,
        };
        for cred_user in &cred_strings {
            let (lvl, trap) = self.cfg.check_access_method_trap(
                &asg,
                &creds.host,
                cred_user,
                asl,
                method,
                &creds.authority,
            );
            match lvl {
                AccessLevel::ReadWrite => {
                    entry.level = AccessLevelLite::ReadWrite;
                    entry.write_trap = trap;
                    break;
                }
                AccessLevel::Read => entry.level = AccessLevelLite::Read,
                _ => {}
            }
        }
        if record_known {
            let mut cache = self.grant_cache.write();
            if cache.len() >= GRANT_CACHE_CAP {
                cache.clear();
            }
            cache.insert(key, entry);
        }
        entry
    }

    /// Write authorization plus the matched (granting) rule's
    /// `TRAPWRITE` flag, from the same cached evaluation every other
    /// check uses (see [`Self::computed_access`]).
    async fn grant_for_creds(&self, channel: &str, creds: &ClientCreds) -> WriteGrant {
        let computed = self.computed_access(channel, creds).await;
        WriteGrant {
            allowed: computed.level == AccessLevelLite::ReadWrite,
            rule_was_trap: computed.write_trap,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum AccessLevelLite {
    None,
    Read,
    ReadWrite,
}

#[async_trait::async_trait]
impl AccessControl for AcfAccessControl {
    async fn can_read(&self, channel: &str, user: &str, host: &str) -> bool {
        self.can_read_creds(
            channel,
            &ClientCreds {
                user: user.to_string(),
                host: host.to_string(),
                ..Default::default()
            },
        )
        .await
    }

    async fn can_write(&self, channel: &str, user: &str, host: &str) -> bool {
        self.can_write_creds(
            channel,
            &ClientCreds {
                user: user.to_string(),
                host: host.to_string(),
                ..Default::default()
            },
        )
        .await
    }

    async fn can_read_creds(&self, channel: &str, creds: &ClientCreds) -> bool {
        self.level_for_creds(channel, creds).await != AccessLevelLite::None
    }

    async fn can_write_creds(&self, channel: &str, creds: &ClientCreds) -> bool {
        // Route through `grant_for_creds` (the trap-carrying path) so the
        // allow/deny decision and the trap flag come from one rule
        // evaluation — `write_grant` cannot disagree with `can_write_creds`.
        self.grant_for_creds(channel, creds).await.allowed
    }

    async fn write_grant(&self, channel: &str, creds: &ClientCreds) -> WriteGrant {
        self.grant_for_creds(channel, creds).await
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
    /// Full credential set of the downstream client (pvxs
    /// `ClientCredentials`, srvcommon.h:36-56). Shared by refcount so
    /// per-op access checks hand `&self.creds` to the policy instead
    /// of re-cloning five strings per check.
    pub creds: Arc<ClientCreds>,
}

impl AccessContext {
    /// Construct a context for an unauthenticated request (empty credentials).
    pub fn anonymous(access: Arc<dyn AccessControl>) -> Self {
        Self {
            access,
            creds: Arc::new(ClientCreds::default()),
        }
    }

    /// Construct a context with explicit user/host (method defaults to empty).
    pub fn with_identity(access: Arc<dyn AccessControl>, user: String, host: String) -> Self {
        Self {
            access,
            creds: Arc::new(ClientCreds {
                user,
                host,
                ..Default::default()
            }),
        }
    }

    /// Construct a context with the full [`ClientCreds`] set.
    pub fn with_creds(access: Arc<dyn AccessControl>, creds: ClientCreds) -> Self {
        Self {
            access,
            creds: Arc::new(creds),
        }
    }

    /// Allow-all context (used by tests and the default `BridgeProvider`).
    pub fn allow_all() -> Self {
        Self::anonymous(Arc::new(AllowAllAccess))
    }

    pub async fn can_read(&self, channel: &str) -> bool {
        self.access.can_read_creds(channel, &self.creds).await
    }

    pub async fn can_write(&self, channel: &str) -> bool {
        self.access.can_write_creds(channel, &self.creds).await
    }

    /// Authorize a write to `channel` and surface the matched rule's
    /// `TRAPWRITE` flag (see [`WriteGrant`]). The QSRV PUT path calls
    /// this once and uses the result both to gate the write and to gate
    /// `asTrapWrite` put-logging — the grant is the single source of the
    /// trap decision.
    pub async fn write_grant(&self, channel: &str) -> WriteGrant {
        self.access.write_grant(channel, &self.creds).await
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
    /// The server-wide QSRV group drain — pvxs's one `qsrvGroup` event pump
    /// per `GroupSource` (`ioc/groupsource.cpp:96`). Every group channel
    /// this provider vends shares it, so ALL group subscriptions on the
    /// served IOC drain through ONE task (doc/qsrv-rtems-design.md §9.15).
    group_pump: Arc<super::group_pump::GroupPump>,
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
    ///
    /// [`parking_lot::RwLock`], like every other lock in this struct: the
    /// critical section is a `HashMap` get/insert with no I/O in it, so an
    /// async lock bought nothing and cost a PI-invisible wait on the
    /// GET/PUT hot path (doc/qsrv-rtems-design.md §5, L-A). The guard is
    /// `!Send`, so a future that held one across an `.await` would fail the
    /// `+ Send` bound the `QsrvPvStore` source methods impose on every
    /// caller of this provider — the rule is enforced by construction, not
    /// by review.
    record_cache: parking_lot::RwLock<HashMap<String, (NtType, DbFieldType)>>,
    /// Live access-control cell. Channels and AccessContexts hold an
    /// `Arc<LiveAccessProxy>` that points at this cell, so
    /// `set_access_control` is observed by all existing channels on
    /// their *next* check (matches C++ QSRV — ACF reload takes effect
    /// without recreating channels).
    access_cell: Arc<parking_lot::RwLock<Arc<dyn AccessControl>>>,
    /// "Base" group fragments that survive every `dbLoadGroup` file
    /// add/remove/clear: `info(Q:group)` record tags ([`Self::load_info_group`])
    /// and direct config loads ([`Self::load_group_config`]). pvxs rebuilds
    /// these first in `processGroups` (`loadConfigFromDb`,
    /// ioc/groupsourcehooks.cpp:198) before any file fragments, so they are
    /// kept separately from the removable file list and re-merged on every
    /// rebuild. Stored as the parsed fragments (not the merged result) so a
    /// rebuild can replay them through the field-keyed `merge_group_defs`.
    base_group_defs: parking_lot::RwLock<Vec<GroupPvDef>>,
    /// Source registry for file-loaded group definitions. pvxs keeps a
    /// `groupConfigFiles` list so `dbLoadGroup("-file.json")` removes one
    /// file's contribution and `dbLoadGroup("-*")` clears them all
    /// (ioc/groupsourcehooks.cpp:133-183), then `processGroups` rebuilds the
    /// live group map from the remaining files plus the DB-info fragments
    /// (groupsourcehooks.cpp:200-207). Each entry stores its source identity
    /// `(filename, raw-macros)` AND the parsed `GroupPvDef` fragments so the
    /// live [`Self::groups`] map can be rebuilt field-by-field from the
    /// surviving sources — removal is NOT a whole-group-name delete, which
    /// would drop fields contributed to the same group name by other files
    /// or `info(Q:group)` tags.
    group_files: parking_lot::RwLock<Vec<GroupFileEntry>>,
    /// Bumped on every rewrite of the live [`Self::groups`] map
    /// (`rebuild_groups` / `reset_groups`). Lets a caller that caches a
    /// resolved [`AnyChannel`] (the PVA adapter's per-peer channel
    /// cache) detect that group name→definition bindings may have
    /// changed and re-resolve, without re-checking the map per op.
    group_generation: std::sync::atomic::AtomicU64,
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

/// One `dbLoadGroup(filename, macros)` registration: its source identity
/// plus the parsed `GroupPvDef` fragments that load contributed. Keeping
/// the fragments (rather than only the group names) lets the live registry
/// be rebuilt field-by-field from the surviving sources when this file is
/// removed, so a file that contributed only some fields of a shared group
/// name does not delete the whole group.
struct GroupFileEntry {
    filename: String,
    macros: String,
    defs: Vec<GroupPvDef>,
}

/// Proxy that re-reads the live access-control policy on every check.
/// Wraps an `Arc<RwLock<Arc<dyn AccessControl>>>` shared with the
/// owning [`BridgeProvider`] — `set_access_control` swaps the inner
/// `Arc` and existing AccessContexts pick up the new policy on their
/// next [`AccessControl::can_read`] / [`AccessControl::can_write`] call.
struct LiveAccessProxy {
    cell: Arc<parking_lot::RwLock<Arc<dyn AccessControl>>>,
}

impl LiveAccessProxy {
    /// Snapshot the live policy and release the lock *before* awaiting it: the
    /// checks below are async now, and the sync `parking_lot` guard must not be
    /// held across an await point.
    fn policy(&self) -> Arc<dyn AccessControl> {
        self.cell.read().clone()
    }
}

#[async_trait::async_trait]
impl AccessControl for LiveAccessProxy {
    async fn can_read(&self, channel: &str, user: &str, host: &str) -> bool {
        self.policy().can_read(channel, user, host).await
    }
    async fn can_write(&self, channel: &str, user: &str, host: &str) -> bool {
        self.policy().can_write(channel, user, host).await
    }
    async fn can_read_creds(&self, channel: &str, creds: &ClientCreds) -> bool {
        self.policy().can_read_creds(channel, creds).await
    }
    async fn can_write_creds(&self, channel: &str, creds: &ClientCreds) -> bool {
        self.policy().can_write_creds(channel, creds).await
    }
    async fn write_grant(&self, channel: &str, creds: &ClientCreds) -> WriteGrant {
        // Forward to the live inner policy so a swapped-in
        // `AcfAccessControl`'s trap flag is observed; the default impl
        // would drop it to `rule_was_trap = false`.
        self.policy().write_grant(channel, creds).await
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
            group_pump: super::group_pump::GroupPump::new(),
            groups: parking_lot::RwLock::new(HashMap::new()),
            record_cache: parking_lot::RwLock::new(HashMap::new()),
            access_cell: Arc::new(parking_lot::RwLock::new(Arc::new(AllowAllAccess))),
            channels_created: std::sync::atomic::AtomicU64::new(0),
            ops_get: std::sync::atomic::AtomicU64::new(0),
            ops_put: std::sync::atomic::AtomicU64::new(0),
            ops_subscribe: std::sync::atomic::AtomicU64::new(0),
            base_group_defs: parking_lot::RwLock::new(Vec::new()),
            group_files: parking_lot::RwLock::new(Vec::new()),
            group_generation: std::sync::atomic::AtomicU64::new(0),
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
        // assume true if the group is *served* (registered and not
        // shadowed by a backing record). A shadowed name falls through to
        // the record path below so its `.DISP` governs writability — pvxs
        // serves such a name only as the record (defineGroups,
        // ioc/groupconfigprocessor.cpp:170-181), never the group.
        if self.is_servable_group(name).await {
            return true;
        }
        let (record, _field) = epics_base_rs::server::database::parse_pv_name(name);
        let Some(rec_arc) = self.db.get_record(record) else {
            // PVA-plugin PVs (NTNDArray) aren't records — caller
            // (qsrv pva_adapter) should consult its own pva_pvs map.
            // Default false here so unknown names refuse PUT upfront.
            return false;
        };
        let inst = rec_arc.read();
        inst.common.disp == 0
    }

    /// Which NT property leaves the channel `name` (`REC`, `REC.FIELD`, or a
    /// group member's channel) actually SUPPLIES — the record type's rset
    /// slots narrowed to the addressed field, as `dbChannelGet` narrows
    /// `getProperties`'s option mask (`dbAccess.c:336-430`).
    ///
    /// This is the same mask every [`Snapshot`](epics_base_rs::server::snapshot::Snapshot)
    /// carries; it is exposed separately for the paths that must decide which
    /// leaves they may MARK before (or without) reading a snapshot: a group's
    /// per-member masks, resolved once at monitor start, and the GET/seed
    /// mark set. A name that resolves to no record supplies nothing.
    pub async fn channel_property_support(
        &self,
        name: &str,
    ) -> epics_base_rs::server::snapshot::PropertySupport {
        channel_property_support(&self.db, name).await
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

    /// Current group-registry generation (see the field doc). A cached
    /// channel resolution is valid only while this value is unchanged.
    pub fn group_generation(&self) -> u64 {
        self.group_generation
            .load(std::sync::atomic::Ordering::Acquire)
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
    pub async fn can_write(&self, channel: &str, user: &str, host: &str) -> bool {
        let policy = self.access_cell.read().clone();
        policy.can_write(channel, user, host).await
    }

    /// Check if a client can read from a channel.
    pub async fn can_read(&self, channel: &str, user: &str, host: &str) -> bool {
        let policy = self.access_cell.read().clone();
        policy.can_read(channel, user, host).await
    }

    /// Load group PV definitions from a JSON config string. Takes
    /// `&self` (interior mutability) so iocsh commands can call this
    /// against a shared `Arc<BridgeProvider>`.
    pub fn load_group_config(&self, json: &str) -> BridgeResult<()> {
        let defs = super::group_config::parse_group_config(json)?;
        // A direct config load is a "base" source — it survives every
        // dbLoadGroup file add/remove/clear, like pvxs DB-info groups
        // (`loadConfigFromDb` runs before `loadConfigFiles` in
        // `processGroups`, groupsourcehooks.cpp:198-201). Record the parsed
        // fragments and rebuild the live map field-by-field from all
        // sources via `merge_group_defs`, which collapses cross-source
        // duplicate field names first-wins (groupconfigprocessor.cpp:221-225).
        self.base_group_defs.write().extend(defs);
        self.rebuild_groups();
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
        {
            let mut files = self.group_files.write();
            // Drop a prior load of the same source identity (pvxs erases
            // the matching entry before appending the new one,
            // groupsourcehooks.cpp:174-183), then register this file's
            // parsed fragments.
            files.retain(|e| !(e.filename == filename && e.macros == macros));
            files.push(GroupFileEntry {
                filename: filename.to_string(),
                macros: macros.to_string(),
                defs,
            });
        }
        // Rebuild the live map from all surviving sources, accumulating
        // field-by-field — a second file contributing distinct fields to
        // the same group name adds them rather than replacing the first
        // file's group (groupsourcehooks.cpp:192-207, fieldConfigMap).
        self.rebuild_groups();
        Ok(())
    }

    /// Remove every group placed by a prior `dbLoadGroup(filename,
    /// macros)` with the matching source identity. Mirrors pvxs
    /// `dbLoadGroup("-file.json", "MAC")` (groupsourcehooks.cpp:174-179),
    /// which compares the raw filename and raw macros string. Returns the
    /// number of group definitions removed.
    pub fn remove_group_file(&self, filename: &str, macros: &str) -> usize {
        let before = self.groups.read().len();
        {
            let mut files = self.group_files.write();
            files.retain(|e| !(e.filename == filename && e.macros == macros));
        }
        // Rebuild from the surviving sources rather than deleting the
        // removed file's group names: a group name shared with another file
        // or an `info(Q:group)` tag keeps the fields those sources
        // contributed (pvxs re-runs `processGroups` over the remaining file
        // list, groupsourcehooks.cpp:200-207). The reported count is the
        // number of group names that fully disappeared.
        self.rebuild_groups();
        let after = self.groups.read().len();
        before.saturating_sub(after)
    }

    /// Remove every file-loaded group. Mirrors pvxs `dbLoadGroup("-*")`
    /// (groupsourcehooks.cpp:140-143), which clears the registered file
    /// list. `info(Q:group)` groups are not file-sourced and are left
    /// intact; use [`Self::reset_groups`] to clear everything. Returns
    /// the number of group definitions removed.
    pub fn clear_group_files(&self) -> usize {
        let before = self.groups.read().len();
        self.group_files.write().clear();
        // `info(Q:group)` and direct-config base fragments are not
        // file-sourced and survive the rebuild; use `reset_groups` to clear
        // everything. The reported count is the number of group names that
        // fully disappeared once the file fragments are removed.
        self.rebuild_groups();
        let after = self.groups.read().len();
        before.saturating_sub(after)
    }

    /// Single owner of the live group map. Rebuilds [`Self::groups`] from
    /// the current sources via the field-keyed `merge_group_defs`: the base
    /// fragments (DB-info / direct-config) first — matching pvxs
    /// `loadConfigFromDb` before `loadConfigFiles` in `processGroups`
    /// (groupsourcehooks.cpp:198-207) — then each tracked `dbLoadGroup`
    /// file's fragments in load order. Because the map is a pure derivation
    /// of the sources, an add/remove/clear re-merges the survivors, so a
    /// file that contributed only some fields of a shared group name never
    /// deletes the fields other sources contributed.
    fn rebuild_groups(&self) {
        let mut merged: HashMap<String, GroupPvDef> = HashMap::new();
        {
            let base = self.base_group_defs.read();
            super::group_config::merge_group_defs(&mut merged, base.clone());
        }
        {
            let files = self.group_files.read();
            for entry in files.iter() {
                super::group_config::merge_group_defs(&mut merged, entry.defs.clone());
            }
        }
        *self.groups.write() = merged;
        self.group_generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// Load group definitions from a record's info(Q:group, ...) tag.
    pub fn load_info_group(&self, record_name: &str, json: &str) -> BridgeResult<()> {
        let defs = super::group_config::parse_info_group(record_name, json)?;
        // `info(Q:group)` tags are base sources (pvxs `loadConfigFromDb`);
        // record the fragments and rebuild from all sources.
        self.base_group_defs.write().extend(defs);
        self.rebuild_groups();
        Ok(())
    }

    /// Finalize loaded group definitions: validate trigger references
    /// (every `+trigger` field name must exist in the group) and
    /// populate `+all` triggers into explicit field lists. Mirrors
    /// pvxs `GroupConfigProcessor::resolveGroupTriggerReferences` /
    /// `createGroups`. Idempotent — safe to call after every
    /// `dbLoadGroup`. Returns the count of groups CREATED — a group whose
    /// `+channel` does not resolve is refused, exactly as pvxs
    /// `createGroups` catches the `Field`/`Channel` ctor throw and prints
    /// `"%s: Error Group not created: %s\n"` (groupconfigprocessor.cpp:429-444,
    /// ioc/field.cpp:23-25, ioc/channel.cpp:37); it is not counted and is
    /// never served. Trigger references to unknown fields stay a warning
    /// (pvxs `defineGroupTriggers` prints and continues, :393-398).
    pub async fn process_groups(&self) -> usize {
        let defs: Vec<GroupPvDef> = self.groups.read().values().cloned().collect();
        let mut created = 0;
        for def in defs {
            let field_names: std::collections::HashSet<&str> =
                def.members.iter().map(|m| m.field_name.as_str()).collect();
            for member in &def.members {
                if let super::group_config::TriggerDef::Fields(refs) = &member.triggers {
                    for r in refs {
                        if !field_names.contains(r.as_str()) {
                            tracing::warn!(
                                group = %def.name,
                                member = %member.field_name,
                                trigger = %r,
                                "group trigger references unknown field"
                            );
                        }
                    }
                }
            }
            // The same admission test the serve gate applies, so the count
            // this reports and the set `servable_group` will hand out are the
            // same set by construction.
            if let Some(err) = self.group_creation_error(&def).await {
                eprintln!("{}: Error Group not created: {err}", def.name);
                tracing::error!(group = %def.name, error = %err, "group not created");
                continue;
            }
            created += 1;
        }
        created
    }

    /// Why `def` cannot be created, or `None` when every member resolves.
    ///
    /// pvxs builds one `dbChannel` per `+channel` while constructing the
    /// group's fields; a `+channel` that `dbChannelCreate` refuses throws out
    /// of `Field::Field` (ioc/field.cpp:23-25 → ioc/channel.cpp:37) and
    /// `createGroups` drops the WHOLE group. This is the port's single
    /// creation gate: the same answer feeds the `processGroups` report and
    /// [`Self::servable_group`], so a group with an unresolvable member can
    /// neither be counted as created nor reach a client — the "half-created
    /// group that fails every operation" state is not representable behind
    /// the gate.
    async fn group_creation_error(&self, def: &GroupPvDef) -> Option<String> {
        for member in &def.members {
            if member.channel.is_empty() {
                // Channel-less members (Structure / Const / Proc mappings) never
                // bind a dbChannel — pvxs skips them (`if(!def.channel.empty())`).
                continue;
            }
            if let Err(e) = super::channel::resolve_db_channel(&self.db, &member.channel).await {
                return Some(e);
            }
        }
        None
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
        self.db.get_record(name).is_some() || self.db.find_pv(name).await.is_some()
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
    ///
    /// The gate also applies pvxs's CREATION test: a group whose `+channel`
    /// does not resolve to a dbChannel is never created by `createGroups`
    /// (groupconfigprocessor.cpp:429-444), so it must not answer a search
    /// either — the client gets a clean "PV not found" instead of a group
    /// that connects and then fails every operation. See
    /// [`Self::group_creation_error`].
    pub(crate) async fn servable_group(&self, name: &str) -> Option<GroupPvDef> {
        let def = self.groups.read().get(name).cloned()?;
        if self.record_exists(name).await {
            return None;
        }
        if self.group_creation_error(&def).await.is_some() {
            return None;
        }
        Some(def)
    }

    /// Lean bool form of [`Self::servable_group`]: `name` is served as a
    /// group iff it is registered, not shadowed by a backing record, AND
    /// creatable (every `+channel` resolves).
    ///
    /// The single shadow-aware predicate every group-specific helper
    /// consults so the "a name is a record XOR a group" invariant pvxs
    /// enforces in `defineGroups` (ioc/groupconfigprocessor.cpp:170-181,
    /// live `groupMap` ioc/groupsource.cpp:75-89) holds on every path, not
    /// only the find/list/create serve path. Avoids cloning the
    /// `GroupPvDef` when only the existence answer is needed (e.g. the
    /// `is_writable` / `PROCESS` admission checks).
    pub(crate) async fn is_servable_group(&self, name: &str) -> bool {
        // Defined in terms of the gate itself, so the bool answer and the
        // definition answer can never disagree about what is served.
        self.servable_group(name).await.is_some()
    }

    /// Drop every registered group definition. Mirrors pvxs
    /// `resetGroups` (groupsourcehooks.cpp:222) — used between
    /// `iocInit` cycles in tests so the second run starts clean. The
    /// underlying records are unaffected.
    pub fn reset_groups(&self) -> usize {
        let n = self.groups.read().len();
        // Drop every source — base (info / config) fragments and the
        // file-source registry — so a later remove/clear or re-load starts
        // from a clean slate, then clear the derived live map.
        self.base_group_defs.write().clear();
        self.group_files.write().clear();
        self.groups.write().clear();
        self.group_generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        n
    }

    /// Resolve a single member of a group by `(group_name, field)`.
    /// Returns the backing record name (`record.field`) and the
    /// member's [`super::pvif::FieldMapping`] so callers can
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

    /// Read a single field of a group as an [`epics_base_rs::types::EpicsValue`]. Mirrors
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
        self.db.get_pv(&channel).ok()
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
        if !self.can_write(group, user, host).await {
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
    ///
    /// Synchronous: the whole body is one `parking_lot` write guard over a
    /// `HashMap::clear`, with nothing to await.
    pub fn clear_cache(&self) {
        self.record_cache.write().clear();
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
        // Peel the EPICS `$` long-string modifier (C `dbChannel.c:486-505`)
        // before the existence check so a record-level `REC$` (default
        // `VAL`) answers the search; `split_channel_name` leaves the `$`
        // on the record path (the CA server detects it there too).
        // `has_name` strips any remaining `{json}` / `[range]` suffix
        // itself, so this only removes the trailing modifier.
        let parsed = epics_base_rs::server::database::filters::split_channel_name(name);
        let core = parsed
            .record_path
            .strip_suffix('$')
            .unwrap_or(&parsed.record_path);
        self.db.has_name(core).await
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
        names.extend(self.db.all_alias_names());
        // Only groups the serve gate hands out are listed: a name shadowed by
        // a record is listed once (as the record — the record wins, pvxs
        // `defineGroups` ioc/groupconfigprocessor.cpp:177), and a group that
        // `createGroups` would refuse never enters `groupMap`
        // (:429-444), so groupsource.cpp:75-89 cannot list it.
        let existing: std::collections::HashSet<String> = names.iter().cloned().collect();
        let group_keys: Vec<String> = self.groups.read().keys().cloned().collect();
        for k in group_keys {
            if !existing.contains(&k) && self.is_servable_group(&k).await {
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
    /// [`Self::create_channel_with_creds`].
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
                GroupChannel::new(self.db.clone(), def)
                    .with_access(access_ctx)
                    .with_pump(self.group_pump.clone()),
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
        // Peel the EPICS `$` long-string modifier (C `dbChannel.c:486-505`)
        // off the record path so the underlying record/field resolves at
        // the `has_name` gate below; `BridgeChannel::new` re-reads the
        // modifier from the full name to select the long-string view. The
        // `$` is left on the record path by `split_channel_name` (the CA
        // server detects it there as well).
        let resolution_name = parsed
            .record_path
            .strip_suffix('$')
            .unwrap_or(parsed.record_path.as_str());
        let (record_name, field) = epics_base_rs::server::database::parse_pv_name(resolution_name);
        let field_upper = field.to_ascii_uppercase();

        // Cache hit only when the requested name has no filter
        // suffix — a filtered subscription must take a fresh
        // construction path through `BridgeChannel::new` so the
        // per-channel filter chain is parsed.
        if parsed.json_suffix.is_none() {
            // Sync guard, scoped to this block: nothing inside it awaits, and
            // it is dropped before the `has_name` await below.
            let cache = self.record_cache.read();
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
                let mut cache = self.record_cache.write();
                cache.insert(name.to_string(), (channel.nt_type(), channel.value_dbf()));
            }

            return Ok(AnyChannel::Single(channel.with_access(access_ctx)));
        }

        Err(BridgeError::ChannelNotFound(name.to_string()))
    }
}

/// Which NT property leaves the channel `name` supplies — see
/// [`BridgeProvider::channel_property_support`]. The free form is the one
/// owner of the lookup; the group monitor holds a `PvDatabase` rather than a
/// provider and resolves its members' masks through this.
pub async fn channel_property_support(
    db: &PvDatabase,
    name: &str,
) -> epics_base_rs::server::snapshot::PropertySupport {
    let (record, field) = epics_base_rs::server::database::parse_pv_name(name);
    let Some(rec_arc) = db.get_record(record) else {
        return epics_base_rs::server::snapshot::PropertySupport::NONE;
    };
    let inst = rec_arc.read();
    inst.property_support_for_field(field)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `+channel` admission boundary (pvxs `createGroups`
    /// groupconfigprocessor.cpp:429-444 catching the `Field`/`Channel` ctor
    /// throw). One case per boundary of "would `dbChannelCreate` succeed?":
    ///
    /// - member record missing              → group NOT created
    /// - member field missing on a real rec → group NOT created
    /// - `$` on a `DBF_CHAR` waveform       → group NOT created
    ///   (`dbChannel.c:500-503` `S_dbLib_fieldNotFound`; the port used to
    ///   fabricate a `double` leaf for the unresolvable `VAL$` field)
    /// - `$` on a `DBF_STRING` field        → group NOT created (deviation,
    ///   see `channel::resolve_db_channel`: the group path has no `$` view,
    ///   so admitting it yields a group that answers `FieldNotFound` to every
    ///   GET — refused with a named reason instead)
    /// - every member resolves              → group created
    ///
    /// "Not created" is asserted at the client boundary, not just in the
    /// count: an uncreatable group must not answer a search, must not build
    /// a channel, and must not be listed.
    #[tokio::test]
    async fn a_group_whose_channel_does_not_resolve_is_not_created() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::server::records::stringin::StringinRecord;
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let db = Arc::new(PvDatabase::new());
        db.add_record("G:ai", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        db.add_record("G:si", Box::new(StringinRecord::new("hello")))
            .await
            .unwrap();
        db.add_record("G:wf", Box::new(WaveformRecord::new(40, DbFieldType::Char)))
            .await
            .unwrap();

        let provider = Arc::new(BridgeProvider::new(db));
        provider
            .load_group_config(
                r#"{
                    "G:missingrec":   { "value": { "+channel": "G:nosuch.VAL", "+type": "plain" } },
                    "G:missingfield": { "value": { "+channel": "G:ai.NOPE",    "+type": "plain" } },
                    "G:dollarchar":   { "value": { "+channel": "G:wf.VAL$",    "+type": "plain" } },
                    "G:dollarstr":    { "value": { "+channel": "G:si.VAL$",    "+type": "plain" } },
                    "G:ok":           { "value": { "+channel": "G:ai.VAL",     "+type": "plain" } }
                }"#,
            )
            .unwrap();

        // All five are configured; only the creatable ones are created.
        assert_eq!(provider.group_count(), 5, "all five parse into the config");
        assert_eq!(
            provider.process_groups().await,
            1,
            "only `G:ok` binds a channel this server can serve"
        );

        for refused in [
            "G:missingrec",
            "G:missingfield",
            "G:dollarchar",
            "G:dollarstr",
        ] {
            assert!(
                !provider.channel_find(refused).await,
                "{refused}: an uncreated group must not answer a search"
            );
            assert!(
                provider.create_channel(refused).await.is_err(),
                "{refused}: an uncreated group must not build a channel"
            );
        }
        assert!(
            provider.channel_find("G:ok").await,
            "G:ok: every member resolves; the group is served"
        );
        match provider.create_channel("G:ok").await.unwrap() {
            AnyChannel::Group(_) => {}
            AnyChannel::Single(_) => panic!("G:ok must build as a group"),
        }

        let listed = provider.channel_list().await;
        assert_eq!(
            listed.iter().filter(|n| n.starts_with("G:")).count(),
            4,
            "the three records and the one created group, nothing else: {listed:?}"
        );
        assert!(
            listed.contains(&"G:ok".to_string()),
            "the created group must be listed: {listed:?}"
        );
    }

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
        // The group's `+channel` must resolve, or the group is never
        // created (`group_creation_error`) and the shadow rule under test
        // would never be reached.
        db.add_record("OTHER", Box::new(AiRecord::new(2.0)))
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

    /// The shadow invariant must hold on the per-group *predicate* paths,
    /// not only find/list/create. A group whose name collides with a
    /// record is served only as the record (pvxs `defineGroups`,
    /// groupconfigprocessor.cpp:170-181), so `is_writable` and the shared
    /// `is_servable_group` gate must both resolve a shadowed name to the
    /// record. Before the fix these read the raw registry and leaked
    /// group-derived behavior.
    #[tokio::test]
    async fn shadowed_group_predicates_resolve_to_record() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("SH:rec", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        // `SH:grp`'s `+channel` must resolve, or the group is refused at
        // creation and the shadow rule under test is never exercised.
        db.add_record("OTHER", Box::new(AiRecord::new(2.0)))
            .await
            .unwrap();
        // DISP=1 so the served record is NOT writable; a leaked group
        // predicate would still advertise writable=true.
        {
            let rec = db.get_record("SH:rec").unwrap();
            rec.write().common.disp = 1;
        }

        let provider = BridgeProvider::new(db);
        // "SH:rec" shadows the record; "SH:grp" has no backing record.
        // Both are pure self-trigger groups (no explicit `+trigger`).
        provider
            .load_group_config(
                r#"{
                    "SH:rec": { "value": { "+channel": "SH:rec.VAL", "+type": "plain" } },
                    "SH:grp": { "value": { "+channel": "OTHER.VAL",  "+type": "plain" } }
                }"#,
            )
            .unwrap();

        // The single shadow gate.
        assert!(
            !provider.is_servable_group("SH:rec").await,
            "shadowed name is not a servable group"
        );
        assert!(
            provider.is_servable_group("SH:grp").await,
            "non-colliding group is servable"
        );

        // is_writable: the shadowed name falls to the record path, whose
        // DISP=1 makes it not writable (raw-registry leak returned true).
        assert!(
            !provider.is_writable("SH:rec").await,
            "shadowed name must respect record DISP, not the group blanket-true"
        );
        assert!(
            provider.is_writable("SH:grp").await,
            "a real servable group is writable"
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

    /// Removing one `dbLoadGroup` file must NOT delete the whole group name
    /// when another file (or an `info(Q:group)` tag) contributed fields to
    /// the same group. pvxs erases only the removed file entry and re-runs
    /// `processGroups` over the remaining sources
    /// (groupsourcehooks.cpp:174-207), so the surviving fields persist. The
    /// prior Rust path tracked only group *names* per file and removed the
    /// whole group on `-file`, losing the other sources' fields.
    #[tokio::test]
    async fn group_file_remove_keeps_other_sources_fields_for_shared_group() {
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

        // Removing a.json keeps GRP alive with b.json's surviving field;
        // no group name fully disappeared, so the reported count is 0.
        assert_eq!(provider.remove_group_file("a.json", ""), 0);
        let def = provider
            .groups()
            .get("GRP")
            .cloned()
            .expect("GRP must survive removal of a.json");
        let names: Vec<&str> = def.members.iter().map(|m| m.field_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["b"],
            "only file A's field is removed: {names:?}"
        );

        // Re-loading the SAME identity as b.json (replace) leaves GRP with
        // exactly b's field — no duplicate, no loss of the survivor.
        provider
            .load_group_file_tracked("b.json", "", file_b)
            .unwrap();
        let def = provider.groups().get("GRP").cloned().expect("GRP exists");
        assert_eq!(def.members.len(), 1, "re-load must not duplicate b");
    }

    /// `dbLoadGroup("-*")` clears file fragments but must preserve an
    /// `info(Q:group)` (base) fragment for the same group name — pvxs's
    /// `processGroups` rebuilds the DB-info groups after clearing files
    /// (groupsourcehooks.cpp:140-143 + 198-207). The base/file source split
    /// makes the file clear leave the info-contributed field intact.
    #[tokio::test]
    async fn group_file_clear_preserves_info_group_fragment() {
        use epics_base_rs::server::database::PvDatabase;

        let provider = BridgeProvider::new(Arc::new(PvDatabase::new()));
        // info(Q:group) on record REC contributes GRP.i (record-relative
        // channel is prefixed with "REC.").
        provider
            .load_info_group(
                "REC",
                r#"{ "GRP": { "i": { "+channel": "VAL", "+type": "plain" } } }"#,
            )
            .unwrap();
        // a file contributes GRP.f.
        provider
            .load_group_file_tracked(
                "f.json",
                "",
                r#"{ "GRP": { "f": { "+channel": "RF.VAL", "+type": "plain" } } }"#,
            )
            .unwrap();
        assert_eq!(
            provider.groups().get("GRP").unwrap().members.len(),
            2,
            "both info and file fields present before clear"
        );

        // -* clears the file fragment; the info(Q:group) fragment survives.
        provider.clear_group_files();
        let def = provider
            .groups()
            .get("GRP")
            .cloned()
            .expect("GRP must survive `-*` via its info(Q:group) fragment");
        let names: Vec<&str> = def.members.iter().map(|m| m.field_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["i"],
            "only the file field is cleared: {names:?}"
        );
    }

    /// Access control that denies all writes, allows all reads.
    struct ReadOnly;
    #[async_trait::async_trait]
    impl AccessControl for ReadOnly {
        async fn can_write(&self, _: &str, _: &str, _: &str) -> bool {
            false
        }
    }

    /// Access control that denies a specific channel name.
    struct DenySpecific(String);
    #[async_trait::async_trait]
    impl AccessControl for DenySpecific {
        async fn can_read(&self, channel: &str, _: &str, _: &str) -> bool {
            channel != self.0
        }
        async fn can_write(&self, channel: &str, _: &str, _: &str) -> bool {
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
        let rec = db.get_record("AI:SEC").unwrap();
        rec.write().common.asg = "SECURE".to_string();

        let acl = AcfAccessControl::new(db.clone(), cfg);
        // Anyone can read.
        assert!(acl.can_read("AI:SEC", "guest", "anywhere").await);
        assert!(acl.can_read("AI:SEC", "admin", "anywhere").await);
        // Only admin can write.
        assert!(acl.can_write("AI:SEC", "admin", "anywhere").await);
        assert!(!acl.can_write("AI:SEC", "guest", "anywhere").await);
    }

    /// The grant cache must repoint when the record's ASG changes
    /// (C `asChangeGroup` re-runs `asComputePvt` for every ASGCLIENT
    /// on `dbPut record.ASG`). The first check populates the cache
    /// under the record's initial group; moving the record into a
    /// deny-all group and firing the ASG-change notifier must flip
    /// the cached decision, not serve the stale grant.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn acf_grant_cache_invalidated_by_asg_field_change() {
        use epics_base_rs::server::access_security::{notify_asg_field_changed, parse_acf};
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;

        let acf_text = r#"
ASG(OPEN) {
    RULE(1, WRITE)
}
ASG(LOCKED) {
}
"#;
        let cfg = parse_acf(acf_text).unwrap();
        let db = Arc::new(PvDatabase::new());
        db.add_record("AI:MOVE", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let rec = db.get_record("AI:MOVE").unwrap();
        rec.write().common.asg = "OPEN".to_string();

        let acl = AcfAccessControl::new(db.clone(), cfg);
        // Twice: the second call is the cache-hit path.
        assert!(acl.can_write("AI:MOVE", "guest", "anywhere").await);
        assert!(acl.can_write("AI:MOVE", "guest", "anywhere").await);

        // The field-I/O layer's `dbPut record.ASG` sequence: mutate,
        // then notify.
        rec.write().common.asg = "LOCKED".to_string();
        notify_asg_field_changed();
        assert!(!acl.can_write("AI:MOVE", "guest", "anywhere").await);
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
            let rec = db.get_record("AI:ASL0").unwrap();
            let mut w = rec.write();
            w.common.asg = "ASL_GATED".to_string();
            w.common.asl = 0;
        }
        {
            let rec = db.get_record("AI:ASL1").unwrap();
            let mut w = rec.write();
            w.common.asg = "ASL_GATED".to_string();
            w.common.asl = 1;
        }
        {
            let rec = db.get_record("AI:METH").unwrap();
            rec.write().common.asg = "METHOD_GATED".to_string();
        }
        {
            let rec = db.get_record("AI:AUTH").unwrap();
            rec.write().common.asg = "AUTHORITY_GATED".to_string();
        }
        {
            let rec = db.get_record("AI:ROLE").unwrap();
            rec.write().common.asg = "ROLE_GATED".to_string();
        }

        let acl = AcfAccessControl::new(db.clone(), cfg);

        // ── Axis 4: field ASL ────────────────────────────────────────────
        // ASL=0 record: RULE(0, WRITE) applies.
        assert!(
            acl.can_write("AI:ASL0", "alice", "h").await,
            "ASL=0: alice should be allowed to write"
        );
        // ASL=1 record: RULE(0, WRITE) is skipped (epics-base asLibRoutines.c:1006:
        // `if(pasgclient->level > pasgrule->level) goto next_rule`).
        assert!(
            !acl.can_write("AI:ASL1", "alice", "h").await,
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
            acl.can_write_creds("AI:METH", &x509_creds).await,
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
            !acl.can_write_creds("AI:METH", &ca_creds).await,
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
            acl.can_write_creds("AI:AUTH", &trusted_creds).await,
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
            !acl.can_write_creds("AI:AUTH", &other_ca_creds).await,
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
            acl.can_write_creds("AI:ROLE", &ops_creds).await,
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
            !acl.can_write_creds("AI:ROLE", &no_role_creds).await,
            "client without 'ops' role must NOT write to ROLE_GATED"
        );
    }

    #[tokio::test]
    async fn access_context_allow_all() {
        let ctx = AccessContext::allow_all();
        assert!(ctx.can_read("ANY").await);
        assert!(ctx.can_write("ANY").await);
    }

    #[tokio::test]
    async fn access_context_read_only() {
        let ctx = AccessContext::anonymous(Arc::new(ReadOnly));
        assert!(ctx.can_read("X").await);
        assert!(!ctx.can_write("X").await);
    }

    #[tokio::test]
    async fn access_context_with_identity() {
        let ctx =
            AccessContext::with_identity(Arc::new(AllowAllAccess), "alice".into(), "host1".into());
        assert_eq!(ctx.creds.user, "alice");
        assert_eq!(ctx.creds.host, "host1");
    }

    #[tokio::test]
    async fn access_context_deny_specific() {
        let ctx = AccessContext::anonymous(Arc::new(DenySpecific("SECRET".to_string())));
        assert!(ctx.can_read("PUBLIC").await);
        assert!(!ctx.can_read("SECRET").await);
        assert!(ctx.can_write("PUBLIC").await);
        assert!(!ctx.can_write("SECRET").await);
    }

    #[tokio::test]
    async fn provider_set_access_control() {
        let db = Arc::new(PvDatabase::new());
        let provider = BridgeProvider::new(db);
        // Default policy
        assert!(provider.can_read("X", "u", "h").await);
        assert!(provider.can_write("X", "u", "h").await);

        // Swap to read-only
        provider.set_access_control(Arc::new(ReadOnly));
        assert!(provider.can_read("X", "u", "h").await);
        assert!(!provider.can_write("X", "u", "h").await);
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
        let err = result.expect_err("expected access denied");
        // pvxs `doFieldPreProcessing` (iocsource.cpp:385) throws exactly this
        // on a write-ACF denial; the wire carries no identity detail.
        assert_eq!(
            super::super::put_status::wire_message(&err),
            "Put not permitted",
            "denial must carry pvxs's contract text"
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
    #[async_trait::async_trait]
    impl AccessControl for WriteOnly {
        async fn can_read(&self, _: &str, _: &str, _: &str) -> bool {
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
    #[tokio::test]
    async fn live_access_proxy_observes_policy_swap() {
        let db = Arc::new(PvDatabase::new());
        let provider = BridgeProvider::new(db);

        // Hand out an AccessContext bound to the LIVE proxy. Default is
        // AllowAllAccess.
        let ctx =
            AccessContext::with_identity(provider.live_access(), "alice".into(), "host1".into());
        assert!(ctx.can_read("ANY").await);
        assert!(ctx.can_write("ANY").await);

        // Swap to a deny-specific policy AFTER the context was created.
        provider.set_access_control(Arc::new(DenySpecific("SECRET".into())));
        assert!(ctx.can_read("ALLOWED").await);
        assert!(!ctx.can_read("SECRET").await, "swap must be observed live");
        assert!(!ctx.can_write("SECRET").await);

        // Swap to read-only — same context, fresh decision.
        provider.set_access_control(Arc::new(ReadOnly));
        assert!(ctx.can_read("X").await);
        assert!(
            !ctx.can_write("X").await,
            "policy swap must take effect immediately"
        );

        // Swap back to allow-all — proxy still tracks.
        provider.set_access_control(Arc::new(AllowAllAccess));
        assert!(ctx.can_write("X").await);
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
            let rec = db.get_record("AI:ANON").unwrap();
            rec.write().common.asg = "ANON_GATED".to_string();
        }
        let acl = AcfAccessControl::new(db.clone(), cfg);

        // Legacy 3-arg path (create_channel / create_channel_for
        // funnel through these). Empty method on the branch.
        assert!(
            acl.can_read("AI:ANON", "alice", "h").await,
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
            acl.can_read_creds("AI:ANON", &id_creds).await,
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
            !acl.can_read_creds("AI:ANON", &ca_creds).await,
            "an explicit 'ca' method must NOT match a METHOD(\"anonymous\")-only rule"
        );
    }
}
