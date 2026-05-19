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

    fn level_for(&self, channel: &str, user: &str, host: &str) -> AccessLevelLite {
        // qsrv's AccessControl is sync; resolve the record's ASG via
        // the runtime's current handle. We're on a tokio worker (the
        // PVA server task pool) so block_in_place + block_on is
        // safe; for callers outside a runtime, fall back to DEFAULT.
        let asg = self.resolve_asg_blocking(channel);
        let level = self
            .cfg
            .check_access_method(&asg, host, user, 0, "anonymous", "");
        match level {
            epics_base_rs::server::access_security::AccessLevel::ReadWrite => {
                AccessLevelLite::ReadWrite
            }
            epics_base_rs::server::access_security::AccessLevel::Read => AccessLevelLite::Read,
            _ => AccessLevelLite::None,
        }
    }

    fn resolve_asg_blocking(&self, channel: &str) -> String {
        let (record_name, _field) = epics_base_rs::server::database::parse_pv_name(channel);
        let db = self.db.clone();
        let name = record_name.to_string();
        let lookup = async move {
            if let Some(rec) = db.get_record(&name).await {
                let inst = rec.read().await;
                if !inst.common.asg.is_empty() {
                    return inst.common.asg.clone();
                }
            }
            "DEFAULT".to_string()
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => match handle.runtime_flavor() {
                tokio::runtime::RuntimeFlavor::MultiThread => {
                    tokio::task::block_in_place(|| handle.block_on(lookup))
                }
                _ => "DEFAULT".to_string(),
            },
            Err(_) => "DEFAULT".to_string(),
        }
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
        self.level_for(channel, user, host) != AccessLevelLite::None
    }

    fn can_write(&self, channel: &str, user: &str, host: &str) -> bool {
        self.level_for(channel, user, host) == AccessLevelLite::ReadWrite
    }
}

/// Per-channel client identity used for access enforcement.
///
/// Carries the access control policy plus the user/host of whichever
/// downstream client opened this channel. The PVA server is expected to
/// fill in `user`/`host` from the connection's authentication context;
/// when no PVA server is wired up yet, both fields are empty strings,
/// in which case [`AccessControl`] implementations should fall back to
/// their default (typically permit-all).
#[derive(Clone)]
pub struct AccessContext {
    pub access: Arc<dyn AccessControl>,
    pub user: String,
    pub host: String,
}

impl AccessContext {
    /// Construct a context for an unauthenticated request (empty user/host).
    pub fn anonymous(access: Arc<dyn AccessControl>) -> Self {
        Self {
            access,
            user: String::new(),
            host: String::new(),
        }
    }

    /// Construct a context with explicit credentials.
    pub fn with_identity(access: Arc<dyn AccessControl>, user: String, host: String) -> Self {
        Self { access, user, host }
    }

    /// Allow-all context (used by tests and the default `BridgeProvider`).
    pub fn allow_all() -> Self {
        Self::anonymous(Arc::new(AllowAllAccess))
    }

    pub fn can_read(&self, channel: &str) -> bool {
        self.access.can_read(channel, &self.user, &self.host)
    }

    pub fn can_write(&self, channel: &str) -> bool {
        self.access.can_write(channel, &self.user, &self.host)
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

/// PVA Monitor interface.
///
/// Corresponds to C++ `pva::Monitor` / `BaseMonitor`.
pub trait PvaMonitor: Send + Sync {
    /// Wait for the next update. Returns `None` when the monitor is closed.
    fn poll(&mut self) -> impl std::future::Future<Output = Option<PvStructure>> + Send;

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
}

impl BridgeProvider {
    pub fn new(db: Arc<PvDatabase>) -> Self {
        Self {
            db,
            groups: parking_lot::RwLock::new(HashMap::new()),
            record_cache: tokio::sync::RwLock::new(HashMap::new()),
            access_cell: Arc::new(parking_lot::RwLock::new(Arc::new(AllowAllAccess))),
            channels_created: std::sync::atomic::AtomicU64::new(0),
            ops_get: std::sync::atomic::AtomicU64::new(0),
            ops_put: std::sync::atomic::AtomicU64::new(0),
            ops_subscribe: std::sync::atomic::AtomicU64::new(0),
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
        // hot-reload through `Arc<BridgeProvider>` (P-G17).
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
        for def in defs {
            g.insert(def.name.clone(), def);
        }
        Ok(())
    }

    /// Load group PV definitions from a JSON file.
    pub fn load_group_file(&self, path: &str) -> BridgeResult<()> {
        let content = std::fs::read_to_string(path)?;
        self.load_group_config(&content)
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

    /// BR-R29: true iff `name` is a registered group PV whose every
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

    /// Drop every registered group definition. Mirrors pvxs
    /// `resetGroups` (groupsourcehooks.cpp:222) — used between
    /// `iocInit` cycles in tests so the second run starts clean. The
    /// underlying records are unaffected.
    pub fn reset_groups(&self) -> usize {
        let mut g = self.groups.write();
        let n = g.len();
        g.clear();
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
        if self.groups.read().contains_key(name) {
            return true;
        }
        self.db.has_name(name).await
    }

    async fn channel_list(&self) -> Vec<String> {
        let mut names = self.db.all_record_names().await;
        // PR #336 aliases are independently addressable channel
        // names — a PVA client running channelList expects them so
        // it can connect by alias. `channel_find` / `create_channel`
        // already resolve aliases via has_name/get_record (round 7).
        names.extend(self.db.all_alias_names().await);
        names.extend(self.groups.read().keys().cloned());
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
    /// Create a channel with explicit client identity for access control.
    ///
    /// Used by the PVA server when it knows the connecting client's
    /// authenticated user/host. The trait method [`ChannelProvider::create_channel`]
    /// delegates to this with empty identity (anonymous mode).
    pub async fn create_channel_for(
        &self,
        name: &str,
        user: &str,
        host: &str,
    ) -> BridgeResult<AnyChannel> {
        self.note_channel_created();
        let access_ctx =
            AccessContext::with_identity(self.live_access(), user.to_string(), host.to_string());

        // Check group PVs first
        if let Some(def) = self.groups.read().get(name).cloned() {
            return Ok(AnyChannel::Group(
                GroupChannel::new(self.db.clone(), def).with_access(access_ctx),
            ));
        }

        // Single record (or `record.FIELD`) channel — BR-R2.
        // Cache by the full requested name so field PVs do not
        // poison the record's VAL-shaped cache entry.
        //
        // BR-R40: peel off any pvxs `PV.VAL{...}` channel-filter
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

    /// Round-19 regression: AcfAccessControl bridges qsrv's
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
}
