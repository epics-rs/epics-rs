use std::collections::HashMap;

use crate::error::{CaError, CaResult};

/// Access level for a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessLevel {
    NoAccess,
    Read,
    ReadWrite,
}

/// Opaque proof that an access check has been performed.
///
/// Type-state ACF gate: every `ChannelSource` op that
/// touches a PV by name now demands an `AccessChecked` instead of
/// raw `(name, ctx)`. The struct has only one public constructor —
/// [`AccessGate::check`] — so it is impossible to call a gated op
/// without first running the check. This is the structural fix for
/// the missed-path pattern that surfaced as ACF coverage grew
/// (ACF was first added on three ops, then later review
/// uncovered four more wire paths that skipped the check).
///
/// The private `_seal` field blocks external struct-literal
/// construction; the constructor is reachable only through
/// `AccessGate::check`.
#[derive(Debug, Clone)]
pub struct AccessChecked {
    pv_name: String,
    level: AccessLevel,
    /// Write-trap mask of the rule that resolved `level`. C
    /// `asComputePvt` stores this as `pasgclient->trapMask`
    /// (`asLibRoutines.c:1048`); put-logging listeners consult it
    /// to honour `TRAPWRITE` / `NOTRAPWRITE`.
    rule_was_trap: bool,
    // Private nominal type; external crates cannot construct
    // `AccessSeal` and therefore cannot fabricate `AccessChecked`
    // via struct literal.
    _seal: AccessSeal,
}

#[derive(Debug, Clone)]
struct AccessSeal;

impl AccessChecked {
    /// The PV name the check was performed against.
    pub fn pv_name(&self) -> &str {
        &self.pv_name
    }

    /// Resolved access level for `(peer, asg, asl)`.
    pub fn level(&self) -> AccessLevel {
        self.level
    }

    /// True iff the level grants at least READ.
    pub fn allows_read(&self) -> bool {
        !matches!(self.level, AccessLevel::NoAccess)
    }

    /// True iff the level grants WRITE.
    pub fn allows_write(&self) -> bool {
        matches!(self.level, AccessLevel::ReadWrite)
    }

    /// True iff the ACF rule that resolved this access level carried
    /// the `TRAPWRITE` option. Mirrors C `pasgclient->trapMask`
    /// (`asLibRoutines.c:1048`) — `false` for `NOTRAPWRITE`, for a
    /// rule with no trap option, and for a denied (`NoAccess`)
    /// resolution. CA put-logging dispatch sets
    /// [`TrapWriteMessage::rule_was_trap`] from this value.
    pub fn rule_was_trap(&self) -> bool {
        self.rule_was_trap
    }
}

/// Per-source access policy holder. Wraps an optional
/// [`AccessSecurityConfig`] cell plus the PV → ASG/ASL resolution
/// hooks the source provides. The wire dispatcher (tcp.rs) asks
/// the source for its `AccessGate`, calls
/// [`AccessGate::check`] once per op, and threads the resulting
/// [`AccessChecked`] into the source's typed op methods.
///
/// Two variants:
///
/// * `Required` — an ACF cell is attached. The check evaluates it
///   under the read lock; absent ACF still produces a permissive
///   token (matching the earlier behaviour for sources whose ACF
///   cell is `None`).
/// * `Open` — the source explicitly opts out of ACF entirely
///   (e.g. test fixtures, in-process sources that never touch the
///   network). All checks return a `ReadWrite` token.
#[derive(Clone)]
pub struct AccessGate {
    inner: AccessGateInner,
    /// Generation counter bumped whenever the
    /// gate's underlying ACF policy changes (reload / clear / hot
    /// swap). Long-lived consumers (PVA monitor tasks spawned at
    /// SUBSCRIBE time, gateway bridge tasks) capture the value at
    /// spawn and compare on each event; a mismatch forces a fresh
    /// `check()` so a peer that was allowed at subscribe time but
    /// is now `NoAccess` under the new policy sees its subscription
    /// torn down on the next event (matching the CA-side
    /// `reeval_access_rights` semantics).
    ///
    /// Two backing shapes —
    /// * `Atomic`: owned `AtomicU64` for terminal gates
    ///   (`Required`, `Open`). `bump_acl_version` `fetch_add`s.
    /// * `Aggregator`: a closure that returns a derived version
    ///   from sub-gates. `CompositeSource` uses this to expose a
    ///   gate whose `acl_version()` is the `wrapping_sum` of its
    ///   inner sources' versions (NOT `max`: max produced
    ///   false negatives when an
    ///   inner bumped to a value still under the existing peak),
    ///   so a bump on any inner (e.g. a
    ///   `GatewayChannelSource::set_acf` on a child) is visible at
    ///   the composite's top-level gate. Note: this gate is only a
    ///   **change signal** — the allow/deny authority remains the
    ///   matched inner source's gate; see
    ///   `ChannelSource::revalidate_read` for the owner path the
    ///   monitor reload loop uses. Pre-fix the composite
    ///   inherited the default `Open` gate (version=0 forever) and
    ///   tcp.rs's monitor loop compared against that stale value,
    ///   missing every inner reload.
    acl_version: AclVersionSource,
    /// optional `INP*`-link value resolver. When present,
    /// [`Self::check`] evaluates CALC-gated rules against live values;
    /// when absent, CALC rules fail closed (deny). Installed by the
    /// owning server via [`Self::with_inp_resolver`].
    inp_resolver: Option<InpResolver>,
    /// C `asAddClient` computes a client's access once per channel and
    /// every operation is a bit test; the Rust op layer called
    /// [`Self::check_with_roles`] — resolver + full rule walk — on every
    /// EXEC. The walk result is deterministic in (policy snapshot,
    /// pv name, credential identity) whenever no [`InpResolver`] is
    /// installed (CALC rules then fail closed, reading no live values),
    /// so those checks are cached here, shared across gate clones.
    /// An entry is valid only while all three of its stamps hold: the
    /// `acl_version` generation, the ASG-field-change generation
    /// ([`asg_change_generation`] — the resolver reads `record.ASG`),
    /// and the identity of the ACF config `Arc` it was computed against
    /// (so a cell swap that forgot to bump the version still misses).
    /// `Open` gates and unattached cells bypass the cache: their checks
    /// are already walk-free, and an unattached-cell result would
    /// otherwise survive the ACF being attached.
    check_cache: std::sync::Arc<parking_lot::RwLock<HashMap<CheckKey, CachedCheck>>>,
}

/// Full identity a cached [`AccessGate`] check depends on. `roles` is
/// part of the key — a re-auth that only changes role claims must miss.
#[derive(Clone, PartialEq, Eq, Hash)]
struct CheckKey {
    pv_name: String,
    host: String,
    user: String,
    method: String,
    authority: String,
    roles: Vec<String>,
}

#[derive(Clone, Copy)]
struct CachedCheck {
    acl_version: u64,
    asg_generation: u64,
    /// `Arc::as_ptr` of the ACF config the entry was computed against.
    cfg_ident: usize,
    level: AccessLevel,
    rule_was_trap: bool,
}

/// Bound on distinct (pv × credential) entries; overflow flushes the
/// map (a re-walk per entry is exactly the pre-cache behaviour).
const CHECK_CACHE_CAP: usize = 4096;

#[derive(Clone)]
enum AclVersionSource {
    Atomic(std::sync::Arc<std::sync::atomic::AtomicU64>),
    Aggregator(std::sync::Arc<dyn Fn() -> u64 + Send + Sync>),
}

/// Asynchronous closure that resolves `pv_name → (ASG, ASL)` for a
/// source. Sources install one when constructing an
/// [`AccessGate::required`].
pub type AsgAslResolver = std::sync::Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = (String, u8)> + Send>>
        + Send
        + Sync,
>;

/// resolves an ASG `INP*` link string (typically a
/// `record.field` PV name) to its current numeric value, or `None` when
/// the input is unresolvable / disconnected (bad input → the CALC-gated
/// rule denies). Installed on an [`AccessGate`] by the owning server so
/// `check` can evaluate `RULE(...) { CALC(...) }` against live values.
/// Async because the value typically lives behind the server's async
/// database lock; [`AccessGate::check_with_roles`] resolves the ASG's
/// links up front, then evaluates the (sync) expression.
pub type InpResolver = std::sync::Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<f64>> + Send>>
        + Send
        + Sync,
>;

/// The shared Access Security policy cell — one per server, cloned into every
/// [`AccessGate`] built from it.
///
/// Lock-free by construction: a reader takes an `Arc` snapshot of the policy
/// and an operator reload publishes a whole new one. This is
/// `doc/rtems-priority-locks-design.md` §3 row **L9**, and it is the ACF cell
/// `epics-pva-rs` and `epics-ca-rs` share. It used to be a
/// `tokio::sync::RwLock` whose read guard was held across the *whole* check —
/// including the async ASG resolve and every CALC `INP*` resolve — so a
/// preempted low-priority `CAS-client` / `PVAS-conn` thread could hold an
/// operator reload, and any higher-priority checker behind it, off for an
/// unbounded, kernel-invisible time.
///
/// The observable check semantics are unchanged: an in-flight check still
/// completes against the policy it started with, because it holds that `Arc`
/// for its whole body. Only the writer changes — it publishes instead of
/// waiting for in-flight readers.
///
/// A newtype (not an alias) so every post-construction swap goes through
/// [`AcfCell::store`], which fires [`notify_asg_field_changed`] — the one
/// change signal policy-derived caches ([`AccessGate`]'s check cache, the
/// QSRV grant cache) and the CA server's `reeval_access_rights` path key
/// on. A raw `ArcSwapOption` swap would update enforcement (checks load
/// per-op) but leave those caches and the wire ACCESS_RIGHTS stale.
#[derive(Clone)]
pub struct AcfCell(std::sync::Arc<arc_swap::ArcSwapOption<AccessSecurityConfig>>);

impl AcfCell {
    /// Snapshot the current policy (lock-free guard).
    pub fn load(&self) -> arc_swap::Guard<Option<std::sync::Arc<AccessSecurityConfig>>> {
        self.0.load()
    }

    /// Snapshot the current policy as an owned `Option<Arc<..>>`.
    pub fn load_full(&self) -> Option<std::sync::Arc<AccessSecurityConfig>> {
        self.0.load_full()
    }

    /// Publish a new policy (or `None` to clear it) and fire the
    /// process-wide access-policy change notification so live
    /// connections re-evaluate their rights and derived caches drop
    /// their entries.
    pub fn store(&self, value: Option<std::sync::Arc<AccessSecurityConfig>>) {
        self.0.store(value);
        notify_asg_field_changed();
    }
}

/// Build a shared [`AcfCell`] holding `initial`. The single construction
/// point, so no caller has to name `arc_swap` or get the `Arc` nesting right.
pub fn new_acf_cell(initial: Option<AccessSecurityConfig>) -> AcfCell {
    AcfCell(std::sync::Arc::new(arc_swap::ArcSwapOption::new(
        initial.map(std::sync::Arc::new),
    )))
}

/// Build a shared [`AcfCell`] that serves `db`, with its ASG `INP*` watcher
/// already running (C `asCa.c`, see [`spawn_asg_inp_watcher`]).
///
/// The constructor every server that enforces a policy over a record database
/// must use. Access levels are cached per channel, so a policy cell without
/// the watcher silently keeps a `CALC`-gated grant alive after the gate
/// closes; welding the watcher to construction is what stops the next serving
/// entry point from re-opening that hole. [`new_acf_cell`] stays for the cells
/// that gate no database — the gateways' proxied namespace, fixtures.
///
/// Must be called from within the runtime: it spawns.
pub fn new_acf_cell_watching(
    initial: Option<AccessSecurityConfig>,
    db: &std::sync::Arc<crate::server::database::PvDatabase>,
) -> AcfCell {
    let cell = new_acf_cell(initial);
    spawn_asg_inp_watcher(db, &cell);
    cell
}

/// HAG DNS re-resolution cadence — the same 60 s the CA client's
/// `refresh_dns` interval uses for its half of epics-base#863.
const HAG_DNS_REFRESH: std::time::Duration = std::time::Duration::from_secs(60);

/// Spawn the periodic HAG re-resolution task for `cell` (UI-107 /
/// epics-base#863, access-security half). Every `HAG_DNS_REFRESH`,
/// when `asCheckClientIP` is on and a policy is loaded, re-resolves the
/// raw HAG spellings through [`AccessSecurityConfig::with_refreshed_hags`]
/// and republishes a changed config via [`AcfCell::store`] — the same
/// notification path `asInit` uses, so live clients re-evaluate their
/// rights automatically. C recovers stale HAG IPs only on a manual
/// `asInit`; this is the sibling of the CA-side `refresh_dns` deviation.
///
/// Resolution runs inline in the task (the established `refresh_dns`
/// pattern): a wedged resolver delays this refresher, nothing else. The
/// task holds only a `Weak` to the cell and ends when the owning IOC
/// drops it.
pub fn spawn_hag_refresh(cell: &AcfCell) {
    let weak = std::sync::Arc::downgrade(&cell.0);
    crate::runtime::task::spawn_background(async move {
        loop {
            crate::runtime::task::sleep_background(HAG_DNS_REFRESH).await;
            let Some(inner) = weak.upgrade() else { break };
            if !as_check_client_ip() {
                continue;
            }
            let Some(config) = inner.load_full() else {
                continue;
            };
            if let Some(refreshed) = config.with_refreshed_hags() {
                tracing::info!(
                    target: "epics_base_rs::access_security",
                    "HAG DNS refresh: re-resolved members changed; republishing policy"
                );
                AcfCell(inner).store(Some(std::sync::Arc::new(refreshed)));
            }
        }
    });
}

/// Retry cadence for an ASG `INP*` link whose record is not in the database
/// yet. C reaches the same place through CA: `asCaStart` creates a channel per
/// link (`asCa.c:180-205`) and the search retries until the record appears, so
/// an input declared before its record loads is still monitored afterwards.
const ASG_INP_RETRY: std::time::Duration = std::time::Duration::from_secs(10);

/// Spawn the ASG `INP*` value watcher for `cell` over `db` — C `asCa.c`.
///
/// C monitors every ASG input link and each update runs
/// `pasg->inpChanged |= (1<<idx); if(!caInitializing) asComputeAsg(pasg);`
/// (`asCa.c:148-161`), reaching `asComputePvt` (`asLibRoutines.c:1049-1051`)
/// which fires `asClientCOAR` for every client whose level moved. The port had
/// no such monitor: a level was recomputed only on an ACF reload or a write to
/// a record's `ASG` field, so shutting a `CALC`-gated interlock left every
/// already-connected client holding the WRITE grant it was given when the gate
/// was open, and a client that connected while it was shut stayed read-only
/// after it opened.
///
/// This is that monitor, in-process: one `EventMask::VALUE` subscription per
/// distinct link target ([`AccessSecurityConfig::inp_link_targets`]), and
/// [`notify_asg_field_changed`] on any post. That is the signal
/// [`AcfCell::store`] already raises, so the CA server's `reeval_access_rights`
/// and the QSRV grant cache need no new plumbing. Like C's `asComputeAsg` this
/// re-evaluates every client rather than only the ASGs reading the changed
/// link; the downstream `oldaccess != access` gate keeps the wire cost at zero
/// when no level moved.
///
/// The task holds only `Weak`s to the cell and the database, and ends when the
/// owning IOC drops either.
fn spawn_asg_inp_watcher(db: &std::sync::Arc<crate::server::database::PvDatabase>, cell: &AcfCell) {
    let weak_cell = std::sync::Arc::downgrade(&cell.0);
    let weak_db = std::sync::Arc::downgrade(db);
    let mut acf_rx = subscribe_asg_changes();
    crate::runtime::task::spawn_background(async move {
        enum Wake {
            /// A watched link posted a new value.
            Values,
            /// Re-derive the watch set (policy may have been replaced, or a
            /// link's record may have loaded since the last attempt).
            Rebuild,
            Stop,
        }
        let mut readers: Vec<crate::server::event_queue::EventReader> = Vec::new();
        // Targets not yet attached because their record is not loaded.
        let mut pending: Vec<(String, String)> = Vec::new();
        // Identity of the policy `readers` was built from. A notification this
        // task raises itself does not move it, so the watcher cannot re-enter
        // its own rebuild.
        let mut built_from: usize = 0;

        loop {
            let (Some(inner), Some(db)) = (weak_cell.upgrade(), weak_db.upgrade()) else {
                break;
            };
            let config = inner.load_full();
            drop(inner);
            let id = config
                .as_ref()
                .map_or(0, |c| std::sync::Arc::as_ptr(c) as usize);
            if id != built_from {
                built_from = id;
                readers.clear();
                pending = config.map(|c| c.inp_link_targets()).unwrap_or_default();
            }
            pending.retain(|(record, field)| !attach_asg_inp(&db, record, field, &mut readers));
            drop(db);

            let wake = tokio::select! {
                r = acf_rx.recv() => match r {
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => Wake::Stop,
                    _ => Wake::Rebuild,
                },
                () = drain_any_asg_inp(&mut readers) => Wake::Values,
                () = crate::runtime::task::sleep_background(ASG_INP_RETRY) => Wake::Rebuild,
            };
            match wake {
                Wake::Stop => break,
                Wake::Rebuild => {}
                Wake::Values => notify_asg_field_changed(),
            }
        }
    });
}

/// Subscribe one `INP*` target to its record's value events. `false` = not
/// attached, retry later; the record is not in the database yet, or its
/// subscriber cap is full (which frees again as clients disconnect, and losing
/// a security monitor is worth the retry's log noise).
fn attach_asg_inp(
    db: &crate::server::database::PvDatabase,
    record: &str,
    field: &str,
    readers: &mut Vec<crate::server::event_queue::EventReader>,
) -> bool {
    let Some(rec) = db.get_record(record) else {
        return false;
    };
    let reader = rec.write().add_subscriber(
        field,
        0,
        crate::types::DbFieldType::Double,
        crate::server::recgbl::EventMask::VALUE.bits(),
    );
    match reader {
        Some(reader) => {
            readers.push(reader);
            true
        }
        None => false,
    }
}

/// Resolve once any watched link has posted, having drained every queued post
/// so one re-evaluation covers a burst. C coalesces the same way — many
/// `asComputeAsg` calls, one `asClientCOAR` per actual level change.
async fn drain_any_asg_inp(readers: &mut [crate::server::event_queue::EventReader]) {
    std::future::poll_fn(|cx| {
        let mut fired = false;
        for reader in readers.iter_mut() {
            while let std::task::Poll::Ready(Some(_)) = reader.poll_recv(cx) {
                fired = true;
            }
        }
        if fired {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await
}

#[derive(Clone)]
enum AccessGateInner {
    /// ACF cell + resolver. The cell may hold `None` for "no
    /// policy attached" — the gate then issues permissive tokens
    /// (level = `ReadWrite`) so legacy behaviour is preserved when
    /// the operator hasn't loaded an ACF file.
    Required {
        acf: AcfCell,
        resolver: AsgAslResolver,
    },
    /// Always-permissive. Used by sources that have no security
    /// boundary by design (composite test fixtures, ControlSource
    /// for gateway diagnostic PVs, etc.).
    Open,
}

impl AccessGate {
    /// Build a gate that consults an ACF cell + a per-name
    /// `(ASG, ASL)` resolver. Allocates a fresh `acl_version`
    /// counter; use [`Self::required_with_version`] to share the
    /// counter with the owning server (so its `reload_acf_from`
    /// can signal the same generation bump this gate observes).
    pub fn required(acf: AcfCell, resolver: AsgAslResolver) -> Self {
        Self::required_with_version(
            acf,
            resolver,
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        )
    }

    /// Build a gate with an externally-supplied `acl_version`
    /// counter. The owning server (e.g. `PvaServer`) keeps the
    /// same `Arc` and `fetch_add`s on every `reload_acf_from` /
    /// `clear_acf` so monitor tasks holding the gate observe a
    /// version bump on their next event.
    pub fn required_with_version(
        acf: AcfCell,
        resolver: AsgAslResolver,
        acl_version: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        Self {
            inner: AccessGateInner::Required { acf, resolver },
            acl_version: AclVersionSource::Atomic(acl_version),
            inp_resolver: None,
            check_cache: std::sync::Arc::new(parking_lot::RwLock::new(HashMap::new())),
        }
    }

    /// attach an `INP*`-link value resolver so CALC-gated ACF
    /// rules are evaluated against live values instead of failing
    /// closed. The owning server installs one backed by its PV value
    /// registry.
    pub fn with_inp_resolver(mut self, resolver: InpResolver) -> Self {
        self.inp_resolver = Some(resolver);
        self
    }

    /// Build a gate that grants `ReadWrite` to everyone. Used for
    /// sources that have no ACF semantics — composite test
    /// fixtures, in-process diagnostic sources, etc.
    pub fn open() -> Self {
        Self {
            inner: AccessGateInner::Open,
            acl_version: AclVersionSource::Atomic(std::sync::Arc::new(
                std::sync::atomic::AtomicU64::new(0),
            )),
            inp_resolver: None,
            check_cache: std::sync::Arc::new(parking_lot::RwLock::new(HashMap::new())),
        }
    }

    /// Build a permissive gate whose `acl_version()` is derived
    /// from a caller-supplied closure. Used by `CompositeSource`
    /// to aggregate inner sub-gates' versions — the closure
    /// returns `wrapping_sum(inner.access_gate().acl_version())`
    /// so a bump on any sub-source moves the aggregate (every
    /// per-inner version is monotonic via `fetch_add`, so the sum
    /// changes iff some inner moved). NOT `max(...)` — that shape
    /// produced false negatives when a smaller inner bumped under
    /// the existing peak. This gate is only
    /// a **change signal** for the monitor reload loop; the
    /// allow/deny authority is the matched inner source's gate,
    /// reached via `ChannelSource::revalidate_read`.
    ///
    /// `bump_acl_version()` on an `Aggregator` gate is a no-op:
    /// the version is derived, not owned. The aggregator's
    /// underlying gates own their own counters.
    pub fn open_with_aggregator(f: std::sync::Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self {
            inner: AccessGateInner::Open,
            acl_version: AclVersionSource::Aggregator(f),
            inp_resolver: None,
            check_cache: std::sync::Arc::new(parking_lot::RwLock::new(HashMap::new())),
        }
    }

    /// Current ACL generation. Monitor / subscription tasks capture
    /// this at spawn time and compare on each event. A bump (via
    /// [`Self::bump_acl_version`]) signals "the underlying ACF
    /// changed — re-check before forwarding the next event".
    pub fn acl_version(&self) -> u64 {
        match &self.acl_version {
            AclVersionSource::Atomic(a) => a.load(std::sync::atomic::Ordering::Acquire),
            AclVersionSource::Aggregator(f) => f(),
        }
    }

    /// Bump the ACL generation. Called by the owning server after
    /// swapping the ACF policy. Long-lived consumers detect the
    /// change on their next event and re-check.
    ///
    /// On an `Aggregator`-backed gate this is a no-op — the
    /// version is read-through to the underlying gates, which own
    /// their own counters.
    pub fn bump_acl_version(&self) {
        if let AclVersionSource::Atomic(a) = &self.acl_version {
            a.fetch_add(1, std::sync::atomic::Ordering::Release);
        }
    }

    /// Perform the access check for `pv_name` under the connecting
    /// peer's `(host, user, method, authority)`. Returns the only
    /// kind of value the source's op methods will accept.
    pub async fn check(
        &self,
        pv_name: impl Into<String>,
        host: &str,
        user: &str,
        method: &str,
        authority: &str,
    ) -> AccessChecked {
        self.check_with_roles(pv_name, host, user, &[], method, authority)
            .await
    }

    /// Like [`Self::check`] but with the client's
    /// `roles` (QSRV local-group-derived credentials) so a `role/<name>`
    /// UAG member can match, and with CALC-gated rules evaluated against
    /// the installed [`InpResolver`] (fail closed when none is set).
    pub async fn check_with_roles(
        &self,
        pv_name: impl Into<String>,
        host: &str,
        user: &str,
        roles: &[String],
        method: &str,
        authority: &str,
    ) -> AccessChecked {
        let pv_name = pv_name.into();
        // An `Open` gate and an unattached ACF cell both grant
        // `ReadWrite`; neither resolved through an ACF rule, so the
        // trap mask is `false` (no `TRAPWRITE` rule applied).
        let (level, rule_was_trap) = match &self.inner {
            AccessGateInner::Open => (AccessLevel::ReadWrite, false),
            AccessGateInner::Required { acf, resolver } => {
                match acf.load_full() {
                    None => (AccessLevel::ReadWrite, false),
                    Some(cfg) => {
                        // See the [`Self::check_cache`] field doc: cache
                        // only walk results that read no live values, and
                        // snapshot every stamp BEFORE the compute so a
                        // change racing it invalidates the entry instead
                        // of being lost.
                        let acl_version = self.acl_version();
                        let asg_generation = asg_change_generation();
                        let cfg_ident = std::sync::Arc::as_ptr(&cfg) as usize;
                        let key = self.inp_resolver.is_none().then(|| CheckKey {
                            pv_name: pv_name.clone(),
                            host: host.to_string(),
                            user: user.to_string(),
                            method: method.to_string(),
                            authority: authority.to_string(),
                            roles: roles.to_vec(),
                        });
                        if let Some(ref key) = key
                            && let Some(hit) = self.check_cache.read().get(key)
                            && hit.acl_version == acl_version
                            && hit.asg_generation == asg_generation
                            && hit.cfg_ident == cfg_ident
                        {
                            return AccessChecked {
                                pv_name,
                                level: hit.level,
                                rule_was_trap: hit.rule_was_trap,
                                _seal: AccessSeal,
                            };
                        }
                        let (asg, asl) = resolver(pv_name.clone()).await;
                        // pre-resolve the ASG's INP* links up
                        // front — the resolver is async (it reads the
                        // server DB). `Some(inputs)` when every declared
                        // link resolved; `None` when there is no resolver
                        // or any input is bad/disconnected → CALC fails
                        // closed. Each rule's expression is then evaluated
                        // synchronously in `compute_rules`.
                        let inp_values: Option<AsgInputs> = match self.inp_resolver {
                            None => None,
                            Some(ref res) => {
                                let mut inputs = AsgInputs::default();
                                if let Some(group) =
                                    cfg.asg.get(&asg).or_else(|| cfg.asg.get("DEFAULT"))
                                {
                                    for inp in &group.inp {
                                        inputs.record(inp.index, res(inp.link.clone()).await);
                                    }
                                }
                                Some(inputs)
                            }
                        };
                        let (level, rule_was_trap) = cfg.compute_for_name(
                            &asg,
                            host,
                            user,
                            roles,
                            asl,
                            method,
                            authority,
                            inp_values.as_ref(),
                        );
                        if let Some(key) = key {
                            let mut cache = self.check_cache.write();
                            if cache.len() >= CHECK_CACHE_CAP {
                                cache.clear();
                            }
                            cache.insert(
                                key,
                                CachedCheck {
                                    acl_version,
                                    asg_generation,
                                    cfg_ident,
                                    level,
                                    rule_was_trap,
                                },
                            );
                        }
                        (level, rule_was_trap)
                    }
                }
            }
        };
        AccessChecked {
            pv_name,
            level,
            rule_was_trap,
            _seal: AccessSeal,
        }
    }
}

#[cfg(test)]
mod access_checked_tests {
    use super::*;
    use std::sync::Arc;

    #[epics_macros_rs::epics_test]
    async fn open_gate_grants_read_write() {
        let gate = AccessGate::open();
        let checked = gate.check("any:pv", "h", "u", "anonymous", "").await;
        assert_eq!(checked.level(), AccessLevel::ReadWrite);
        assert!(checked.allows_read());
        assert!(checked.allows_write());
        assert_eq!(checked.pv_name(), "any:pv");
    }

    #[epics_macros_rs::epics_test]
    async fn required_gate_with_no_acf_attached_is_permissive() {
        let cell = crate::server::access_security::new_acf_cell(None);
        let resolver: AsgAslResolver =
            Arc::new(|_pv| Box::pin(async { ("DEFAULT".to_string(), 0u8) }));
        let gate = AccessGate::required(cell, resolver);
        let checked = gate.check("any:pv", "h", "u", "anonymous", "").await;
        assert_eq!(checked.level(), AccessLevel::ReadWrite);
    }

    #[epics_macros_rs::epics_test]
    async fn required_gate_with_acf_denies_unprivileged_peer() {
        let cfg = parse_acf(
            r#"
UAG(ops) { alice }
ASG(DEFAULT) {
    RULE(0, READ) { UAG(ops) }
}
"#,
        )
        .unwrap();
        let cell = crate::server::access_security::new_acf_cell(Some(cfg));
        let resolver: AsgAslResolver =
            Arc::new(|_pv| Box::pin(async { ("DEFAULT".to_string(), 0u8) }));
        let gate = AccessGate::required(cell, resolver);

        let allowed = gate.check("x", "h", "alice", "anonymous", "").await;
        assert!(allowed.allows_read());
        assert!(!allowed.allows_write());

        let denied = gate.check("x", "h", "intruder", "anonymous", "").await;
        assert_eq!(denied.level(), AccessLevel::NoAccess);
        assert!(!denied.allows_read());
    }

    /// The gate's check cache must not outlive its policy: a cell swap
    /// (ACF reload) has to miss even when the caller forgets the
    /// `bump_acl_version` convention — the config-`Arc` identity stamp
    /// is what closes that path. The pre-swap repeat exercises the hit
    /// path against the same policy.
    #[epics_macros_rs::epics_test]
    async fn check_cache_misses_on_acf_swap_without_version_bump() {
        let cfg_deny = parse_acf(
            r#"
ASG(DEFAULT) {
}
"#,
        )
        .unwrap();
        let cfg_allow = parse_acf(
            r#"
ASG(DEFAULT) {
    RULE(1, WRITE)
}
"#,
        )
        .unwrap();
        let cell = crate::server::access_security::new_acf_cell(Some(cfg_deny));
        let resolver: AsgAslResolver =
            Arc::new(|_pv| Box::pin(async { ("DEFAULT".to_string(), 0u8) }));
        let gate = AccessGate::required(cell.clone(), resolver);

        assert!(
            !gate
                .check("x", "h", "u", "anonymous", "")
                .await
                .allows_write()
        );
        // Cache-hit path, same policy.
        assert!(
            !gate
                .check("x", "h", "u", "anonymous", "")
                .await
                .allows_write()
        );

        // Swap the policy WITHOUT bumping acl_version.
        cell.store(Some(Arc::new(cfg_allow)));
        assert!(
            gate.check("x", "h", "u", "anonymous", "")
                .await
                .allows_write()
        );
    }
}

/// Access granted by a matching `RULE`. Mirrors the C three-way
/// `asAccessRights` enum (`asNOACCESS` / `asREAD` / `asWRITE`) used by
/// `rule_head_mandatory` in `asLib.y:253-269`. The Rust port previously
/// collapsed this to a `write: bool`, which turned `RULE(0, NONE)` —
/// and any misspelled keyword — into a READ-granting rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RuleAccess {
    /// `RULE(N, NONE)` — grants `asNOACCESS`.
    #[default]
    None,
    /// `RULE(N, READ)` — grants `asREAD`.
    Read,
    /// `RULE(N, WRITE)` — grants `asWRITE`.
    Write,
}

/// A single access rule within an ASG.
#[derive(Debug, Clone, Default)]
pub struct AccessRule {
    pub level: u8,
    /// Three-way access this rule grants when it matches. C
    /// `asLib.y:259-267` distinguishes `NONE`/`READ`/`WRITE`.
    pub access: RuleAccess,
    pub uag: Vec<String>,
    pub hag: Vec<String>,
    /// Authentication method scope (epics-base PR #563). When set,
    /// the rule only applies when the requesting client authenticated
    /// via one of the listed methods. Common values: `"anonymous"`,
    /// `"ca"`, `"x509"`, `"cap-token"`. Empty vector means "any method".
    pub method: Vec<String>,
    /// Cert authority / issuer scope (epics-base PR #563 + #618).
    /// When set, the rule only applies when the client's authenticator
    /// was vouched by one of the listed authorities — e.g. an
    /// X.509 issuer DN, or the cap-token issuer ID. Empty means "any
    /// authority".
    pub authority: Vec<String>,
    /// Write-trap mask (epics-base `asLib.y:272-283` `rule_log_option`,
    /// `AS_TRAP_WRITE`). `true` when the RULE header carried the
    /// `TRAPWRITE` option, `false` for `NOTRAPWRITE` or no option.
    /// The grammar is honoured here; the `asTrapWrite` put-logging
    /// listener that consumes this mask is a separate subsystem not
    /// present in this crate (see the UNFIXED note).
    pub trap: bool,
    /// CALC condition expression (epics-base `asLib.y:294-299`,
    /// `RULE(...) { CALC("A=1") }`). `None` means an unconditional
    /// rule. When `Some`, the rule only grants access while the
    /// expression evaluates to 1 against the ASG's `INP*` link values.
    pub calc: Option<String>,
    /// The expression compiled at ACF parse — C compiles a RULE's CALC
    /// once at load (`asAsgRuleCalc` runs `postfix()`), then every
    /// `asComputePvt` evaluates the stored RPN. `parse_acf` upholds
    /// `calc.is_some() ⟹ calc_compiled.is_some()`; a hand-built rule
    /// that carries `calc` text without the compiled form fails closed
    /// in `compute_rules`.
    pub calc_compiled: Option<crate::calc::CompiledExpr>,
    /// The arguments this rule's CALC expression READS, as a bitmap over
    /// A..U — C's `pasgrule->inpUsed`, computed once at load by
    /// `calcArgUsage` (`asLibRoutines.c:1416`). `asComputePvt` (`:1048`)
    /// intersects it with the ASG's `inpBad` so an unresolvable link disables
    /// only the rules that actually read it. `0` for a rule with no CALC.
    pub inp_used: u32,
    /// True when the rule must be treated as inert by `asComputePvt`.
    /// C `asAsgRuleDisable` (`asLib.y:300-306`) sets `pasgrule->ignore`
    /// for a RULE that contains an unsupported keyword. This port also
    /// sets it for a `CALC` clause that cannot be evaluated here (no
    /// `INP*` link resolution), so an un-evaluable conditional rule
    /// fails CLOSED instead of becoming unconditional.
    pub ignore: bool,
}

/// The access a matching rule grants, with the `ignore` flag folded in
/// — an ignored rule is inert (`None`). Helper for `asComputePvt`.
fn rule_access(rule: &AccessRule) -> AccessLevel {
    if rule.ignore {
        return AccessLevel::NoAccess;
    }
    match rule.access {
        RuleAccess::None => AccessLevel::NoAccess,
        RuleAccess::Read => AccessLevel::Read,
        RuleAccess::Write => AccessLevel::ReadWrite,
    }
}

/// C's truth test for a `RULE(...) { CALC(...) }` result
/// (`asLibRoutines.c:972`):
///
/// ```c
/// pasgrule->result = ((result>.99) && (result<1.01)) ? 1 : 0;
/// ```
///
/// consumed at `:1048` as `pasgrule->result==1`. The open interval is a
/// deliberate tolerance for float error around 1, NOT a shorthand for
/// "non-zero": C refuses a rule whose CALC returns 2, -1, 0.5 or 3. Testing
/// `result != 0.0` instead — which both of this port's former evaluators did
/// — grants WRITE on every truthy non-unity result.
fn calc_result_is_true(result: f64) -> bool {
    result > 0.99 && result < 1.01
}

/// Monotonic ordering of access levels used by `asComputePvt`'s
/// `access >= pasgrule->access` short-circuit.
fn rule_rank(level: AccessLevel) -> u8 {
    match level {
        AccessLevel::NoAccess => 0,
        AccessLevel::Read => 1,
        AccessLevel::ReadWrite => 2,
    }
}

/// Access Security Group.
#[derive(Debug, Clone, Default)]
pub struct AccessSecurityGroup {
    pub rules: Vec<AccessRule>,
    /// `INP(A..U)` database link declarations (epics-base
    /// `asLib.y:234-243`). Index 0 = `INPA`, .. 20 = `INPU`. Each
    /// entry is the link string. Stored for `asdbdump` / `ascar`
    /// inspection and to feed `CALC` rule evaluation; the link
    /// values are not resolved by this crate (see `AccessRule::calc`).
    pub inp: Vec<AsgInp>,
}

/// The live state of an ASG's `INP(A..U)` links: the resolved values C keeps
/// in `pasg->pavalue[]` and the `inpBad` bitmap it keeps alongside them.
///
/// C sets a bit per *input* (`asCa.c connectCallback:91-105`, on a channel
/// that is not connected) and `asComputePvt` (`asLibRoutines.c:1048`) tests it
/// against the *rule's* own `inpUsed`:
///
/// ```c
/// if(!pasgrule->calc
/// || (!(pasg->inpBad & pasgrule->inpUsed) && (pasgrule->result==1)))
/// ```
///
/// so a bad input disables only the rules that read it. Both of this port's
/// resolvers used to abort their link walk on the first unresolvable link and
/// hand `None` to the evaluator, which failed EVERY CALC rule in the group —
/// one typo in an `INPB` no rule mentions took writes away from the whole ASG.
#[derive(Debug, Clone, Default)]
pub struct AsgInputs {
    /// Resolved values, indexed A..U.
    pub values: crate::calc::NumericInputs,
    /// Bit `i` set ⟹ `INP(i)` is declared but could not be resolved.
    pub bad: u32,
}

impl AsgInputs {
    /// Record one declared link's resolution. `None` — no such record, no such
    /// field, a non-numeric value, a disconnected CA link — sets the input's
    /// `bad` bit and leaves its value at 0, which is what C holds for a
    /// channel that never connected.
    ///
    /// This is the single owner of "what an unresolvable INP link means";
    /// every resolver drives it rather than deciding for itself.
    pub fn record(&mut self, index: u8, value: Option<f64>) {
        let idx = index as usize;
        if idx >= crate::calc::CALC_NARGS {
            return;
        }
        match value {
            Some(v) => self.values.vars[idx] = v,
            None => self.bad |= 1u32 << idx,
        }
    }
}

/// A single `INP(A..U)` link declaration within an ASG.
#[derive(Debug, Clone)]
pub struct AsgInp {
    /// Letter index: 0 = `A`, .. 20 = `U`.
    pub index: u8,
    /// The link string (typically a record.field PV name).
    pub link: String,
}

/// Split an ASG `INP*` link into the `(record, field)` it names — C's
/// `dbNameToAddr` on the link string, with the `VAL` default a bare record
/// name carries.
///
/// The single owner of that split. The resolvers that READ a link and the
/// watcher that SUBSCRIBES to it must name the same field, or a value change
/// fires no re-evaluation.
pub fn inp_link_target(link: &str) -> (&str, &str) {
    let (record, field) = crate::server::database::parse_pv_name(link);
    (record, if field.is_empty() { "VAL" } else { field })
}

/// Access Security Configuration parsed from an ACF file.
#[derive(Debug, Clone)]
pub struct AccessSecurityConfig {
    pub uag: HashMap<String, Vec<String>>,
    pub hag: HashMap<String, Vec<String>>,
    /// The HAG members exactly as spelled in the ACF, keyed like `hag`.
    /// `hag` stores `hag_members` resolution *output* (dotted quads
    /// under `asCheckClientIP`), which cannot be re-resolved after a
    /// DNS change; [`Self::with_refreshed_hags`] re-runs the resolution
    /// from these raw spellings (epics-base#863 / UI-107).
    pub hag_raw: HashMap<String, Vec<String>>,
    pub asg: HashMap<String, AccessSecurityGroup>,
    pub unknown_access: AccessLevel,
}

impl AccessSecurityConfig {
    /// Re-run `hag_members` — the single resolution owner — over the
    /// raw HAG spellings and return the refreshed config when any
    /// stored member changed, `None` when resolution is unchanged.
    ///
    /// Only meaningful under `asCheckClientIP` (the default string
    /// mode stores lowercased literals that no DNS change can move);
    /// callers gate on [`as_check_client_ip`] before paying for
    /// resolution. C freezes HAG IPs at ACF load until a manual
    /// `asInit` (epics-base#863; its PR #862 moves upstream toward
    /// refresh) — this is the sibling of the CA-side `refresh_dns`
    /// deviation that closed the client half of that issue.
    pub fn with_refreshed_hags(&self) -> Option<Self> {
        let refreshed: HashMap<String, Vec<String>> = self
            .hag_raw
            .iter()
            .map(|(name, raw)| (name.clone(), hag_members(raw)))
            .collect();
        if refreshed == self.hag {
            return None;
        }
        let mut new = self.clone();
        new.hag = refreshed;
        Some(new)
    }

    /// Render the parsed ACF (UAG/HAG/ASG with their `INP*` links and
    /// RULEs) in C `asDumpFP` shape, as a `String`.
    ///
    /// This is the single owner of the dump format: the `asdbdump` iocsh
    /// command and the CA gateway's R3 access-security report both render
    /// through here, so the two cannot drift. UAG, HAG, and ASG names are
    /// emitted in sorted order so the dump is stable across `HashMap`
    /// iteration order.
    ///
    /// The verbose member/client listing of C's
    /// `asDumpFP(fp, NULL, NULL, verbose=TRUE)` is intentionally *not*
    /// included: this crate models no live AS-member/client registry (see
    /// the `aspmem` iocsh command, which derives membership by scanning
    /// records rather than from an `asgMemberList`). The dump therefore
    /// covers the parsed configuration structures only.
    pub fn dump_report(&self) -> String {
        let mut out = String::new();
        let mut uags: Vec<_> = self.uag.keys().collect();
        uags.sort();
        for name in uags {
            out.push_str(&format!("UAG({name})\n"));
            for m in &self.uag[name] {
                out.push('\t');
                dump_quoted(&mut out, m);
                out.push('\n');
            }
        }
        let mut hags: Vec<_> = self.hag.keys().collect();
        hags.sort();
        for name in hags {
            out.push_str(&format!("HAG({name})\n"));
            for h in &self.hag[name] {
                out.push('\t');
                dump_quoted(&mut out, h);
                out.push('\n');
            }
        }
        let mut asgs: Vec<_> = self.asg.keys().collect();
        asgs.sort();
        for name in asgs {
            out.push_str(&format!("ASG({name})\n"));
            self.fmt_asg(name, &mut out);
        }
        out
    }

    /// Append one ASG's `INP*` links and RULEs to `out`, in C `asDumpFP`
    /// shape. Shared by [`Self::dump_report`] and the `asprules` iocsh
    /// command's per-ASG renderer so the rule format has one owner.
    pub fn fmt_asg(&self, name: &str, out: &mut String) {
        let Some(asg) = self.asg.get(name) else {
            return;
        };
        for inp in &asg.inp {
            let letter = (b'A' + inp.index) as char;
            out.push_str(&format!("\tINP{letter}(\"{}\")\n", inp.link));
        }
        for rule in &asg.rules {
            let access = match rule.access {
                RuleAccess::None => "NONE",
                RuleAccess::Read => "READ",
                RuleAccess::Write => "WRITE",
            };
            let disabled = if rule.ignore { " [DISABLED]" } else { "" };
            out.push_str(&format!("\tRULE({},{access}){disabled}\n", rule.level));
            for u in &rule.uag {
                out.push_str(&format!("\t\tUAG({u})\n"));
            }
            for h in &rule.hag {
                out.push_str(&format!("\t\tHAG({h})\n"));
            }
            for m in &rule.method {
                out.push_str(&format!("\t\tMETHOD(\"{m}\")\n"));
            }
            for a in &rule.authority {
                out.push_str(&format!("\t\tAUTHORITY(\"{a}\")\n"));
            }
            if let Some(calc) = &rule.calc {
                out.push_str(&format!("\t\tCALC(\"{calc}\")\n"));
            }
        }
    }

    /// Check access for a given ASG, hostname, and username.
    ///
    /// Convenience that omits the ASL gate (treats every rule as
    /// applicable). Equivalent to `check_access_asl(..., 0)` with
    /// rules typically declared at level 0/1. New code should call
    /// [`Self::check_access_asl`] so a per-record ASL can correctly
    /// disable a rule whose level is below the record's ASL.
    pub fn check_access(&self, asg_name: &str, host: &str, user: &str) -> AccessLevel {
        self.check_access_asl(asg_name, host, user, 0)
    }

    /// Method/authority-aware access check. Mirrors epics-base PR
    /// #563 (METHOD/AUTHORITY) and PR #618 (cert-based ACF). When
    /// `method` and `authority` are provided, rules with non-empty
    /// `method`/`authority` lists are gated on a literal match.
    /// Rules with empty `method`/`authority` ignore those scopes
    /// (legacy behaviour preserved).
    pub fn check_access_method(
        &self,
        asg_name: &str,
        host: &str,
        user: &str,
        record_asl: u8,
        method: &str,
        authority: &str,
    ) -> AccessLevel {
        self.check_access_method_trap(asg_name, host, user, record_asl, method, authority)
            .0
    }

    /// Method/authority-aware access check that also returns the
    /// write-trap mask of the rule that resolved the access level.
    ///
    /// Mirrors C `asComputePvt` (`asLibRoutines.c:983-1048`): the
    /// function tracks `trapMask` alongside `access`, and on every
    /// rule that *raises* the access level it copies that rule's
    /// `trapMask` (`asLibRoutines.c:1041-1042`). The final
    /// `pasgclient->trapMask` (`:1048`) is therefore the trap flag of
    /// the last rule that set the granted access — exactly the value
    /// `asTrapWriteWithData` (`rsrv/camessage.c:799-802`) consults to
    /// decide whether to invoke put-logging listeners.
    ///
    /// Returns `(level, rule_was_trap)`. `rule_was_trap` is `false`
    /// when access stays `NoAccess` (no rule matched), when the
    /// matching rule carried `NOTRAPWRITE`, and when it carried no
    /// trap option at all.
    /// Resolve `asg_name` (falling back to `DEFAULT`) and evaluate its
    /// rules with the given `roles` and the ASG's resolved `INP*` values.
    /// The single entry every CALC-aware caller uses — the CA server and
    /// [`AccessGate::check_with_roles`] alike.
    #[allow(clippy::too_many_arguments)]
    pub fn compute_for_name(
        &self,
        asg_name: &str,
        host: &str,
        user: &str,
        roles: &[String],
        record_asl: u8,
        method: &str,
        authority: &str,
        inputs: Option<&AsgInputs>,
    ) -> (AccessLevel, bool) {
        let asg = match self.asg.get(asg_name) {
            Some(a) => a,
            None => match self.asg.get("DEFAULT") {
                Some(a) => a,
                None => return (AccessLevel::NoAccess, false),
            },
        };
        self.compute_rules(
            asg, host, user, roles, record_asl, method, authority, inputs,
        )
    }

    pub fn check_access_method_trap(
        &self,
        asg_name: &str,
        host: &str,
        user: &str,
        record_asl: u8,
        method: &str,
        authority: &str,
    ) -> (AccessLevel, bool) {
        // C `asAddMemberPvt` (asLibRoutines.c:893-928): a member whose
        // ASG name is not present in the parsed config is silently
        // reassigned to `DEFAULT`. `asInitialize` (asLibRoutines.c:107)
        // *always* synthesises a `DEFAULT` ASG before parsing, so this
        // lookup never legitimately misses — `parse_acf` reproduces
        // that by always inserting an (empty) `DEFAULT`. A missing
        // `DEFAULT` here would mean the config was built by hand
        // bypassing `parse_acf`; fail CLOSED rather than open.
        let asg = match self.asg.get(asg_name) {
            Some(a) => a,
            None => match self.asg.get("DEFAULT") {
                Some(a) => a,
                // Never grant ReadWrite on an ASG-lookup
                // miss. C resolves every miss to the always-present
                // empty `DEFAULT` ⇒ `asNOACCESS`.
                None => return (AccessLevel::NoAccess, false),
            },
        };
        // C `asComputePvt` (asLibRoutines.c:983) initialises
        // `access = asNOACCESS` and only ever *raises* it on a matching
        // RULE. An ASG with no RULE statements (`ASG(LOCKED) { }`)
        // therefore denies every client. Never short-circuit
        // an empty rule list to ReadWrite.
        //
        // An empty/unknown user or host cannot match a UAG/HAG-scoped
        // rule, but a rule with empty `uag`/`hag` lists still applies
        // (C `asComputePvt` only checks the UAG list when
        // `ellCount(&pasgrule->uagList) > 0`). So the loop below is run
        // unconditionally — it naturally denies a `("", "")` peer for
        // any UAG/HAG-scoped rule while still honouring an
        // unconditional `RULE(0, READ)`.
        // C `asComputePvt` initialises `trapMask = 0` and copies the
        // matching rule's `trapMask` only on the lines that also raise
        // `access` (`asLibRoutines.c:986`, `:1042`). A `NoAccess`
        // outcome therefore always carries `trap = false`.
        // No INP* resolution on this sync path, so a CALC-gated rule has no
        // values to evaluate against and fails CLOSED — see `compute_rules`.
        self.compute_rules(asg, host, user, &[], record_asl, method, authority, None)
    }

    /// The single rule-matching loop — C `asComputePvt`
    /// (`asLibRoutines.c:992-1062`) — parameterised by the client's `roles`
    /// (for `role/<name>` UAG members, QSRV `documentation/ioc.rst:181-188`)
    /// and by the ASG's resolved `INP*` values.
    ///
    /// CALC evaluation lives HERE and nowhere else. C has one owner too:
    /// `asComputeAsgPvt` (`asLibRoutines.c:953-990`) computes
    /// `pasgrule->result` and `asComputePvt` (`:1048`) consumes it. The port
    /// used to take a `calc_ok` closure instead, which every caller wrote for
    /// itself — and both callers wrote the same wrong truth test.
    ///
    /// `inputs` is `None` on the sync path
    /// ([`Self::check_access_method_trap`]), which resolves no links; a
    /// CALC-gated rule then fails CLOSED.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn compute_rules(
        &self,
        asg: &AccessSecurityGroup,
        host: &str,
        user: &str,
        roles: &[String],
        record_asl: u8,
        method: &str,
        authority: &str,
        inputs: Option<&AsgInputs>,
    ) -> (AccessLevel, bool) {
        let mut access = AccessLevel::NoAccess;
        let mut trap = false;
        for rule in &asg.rules {
            // C `asComputePvt`: a rule disabled by `asAsgRuleDisable`
            // (unsupported keyword) is skipped. A CALC clause no longer
            // forces `ignore`; it is gated by `calc_ok` below.
            if rule.ignore {
                continue;
            }
            // Monotonic raise: once WRITE is reached nothing can lower
            // it, and a rule whose access is not stronger than the
            // current level cannot change the outcome.
            if access == AccessLevel::ReadWrite {
                break;
            }
            if rule_rank(rule_access(rule)) <= rule_rank(access) {
                continue;
            }
            if record_asl > rule.level {
                continue;
            }
            // UAG: only consulted when the rule scopes one. An empty
            // UAG list means "any user" — including an empty username.
            // a `role/<name>` member matches when the client
            // holds that role (QSRV local-group-derived credentials);
            // a plain member matches the account string.
            let user_match = rule.uag.is_empty()
                || rule.uag.iter().any(|g| {
                    self.uag
                        .get(g)
                        .map(|members| {
                            members.iter().any(|m| {
                                // a member matches the account
                                // string exactly (this also covers a
                                // caller that pre-expands roles into
                                // synthesised `role/<name>` credential
                                // strings and passes them as `user`), OR
                                // a `role/<name>` member matches when the
                                // client's `roles` slice carries that role.
                                m == user
                                    || matches!(
                                        m.strip_prefix("role/"),
                                        Some(role) if roles.iter().any(|r| r == role)
                                    )
                            })
                        })
                        .unwrap_or(false)
                });
            if !user_match {
                continue;
            }
            // HAG: host comparison is case-insensitive. C stores
            // every HAG host lowercased (`asHagAddHost`) and lowercases
            // the connecting client's host before `asComputePvt`.
            let host_lc = host.to_ascii_lowercase();
            let host_match = rule.hag.is_empty()
                || rule.hag.iter().any(|g| {
                    self.hag
                        .get(g)
                        .map(|members| members.iter().any(|m| m.eq_ignore_ascii_case(&host_lc)))
                        .unwrap_or(false)
                });
            if !host_match {
                continue;
            }
            let method_match = rule.method.is_empty()
                || rule.method.iter().any(|m| m.eq_ignore_ascii_case(method));
            if !method_match {
                continue;
            }
            let authority_match = rule.authority.is_empty()
                || rule
                    .authority
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(authority));
            if !authority_match {
                continue;
            }
            // A CALC-gated rule grants only while its expression evaluates
            // true against the resolved INP* link values. The program was
            // compiled once at ACF parse; a rule holding `calc` text with no
            // compiled form (hand-built, bypassing `parse_acf`) fails closed
            // here, as does one with no resolved inputs at all.
            if rule.calc.is_some() {
                let Some(compiled) = rule.calc_compiled.as_ref() else {
                    continue;
                };
                let Some(inputs) = inputs else {
                    continue;
                };
                // C `asLibRoutines.c:1048`: `!(pasg->inpBad & pasgrule->inpUsed)`.
                // A bad input the rule READS disables it; a bad input elsewhere
                // in the group is none of this rule's business.
                if inputs.bad & rule.inp_used != 0 {
                    continue;
                }
                match crate::calc::eval(compiled, &mut inputs.values.clone()) {
                    Ok(result) if calc_result_is_true(result) => {}
                    _ => continue,
                }
            }
            // C `asLibRoutines.c:1041-1042`: a matching rule sets
            // both `access` and `trapMask` together. The trap mask of
            // the last access-raising rule is the one the put-logging
            // hook consults.
            access = rule_access(rule);
            trap = rule.trap;
        }
        (access, trap)
    }

    /// Walk `asg_name`'s declared `INP(A..U)` links (falling back to
    /// `DEFAULT`, as every other lookup here does) and resolve each with
    /// `resolve`, returning C's per-ASG input state. This is `asCa.c`'s job
    /// done on demand: the port has no standing CA monitor per link, so the
    /// values are read when the rules are evaluated.
    ///
    /// An unknown ASG with no `DEFAULT` yields empty inputs — no links, so no
    /// bad bits, and a CALC rule then evaluates against zeros exactly as C
    /// does for an ASG that declares none.
    pub fn resolve_asg_inputs(
        &self,
        asg_name: &str,
        resolve: &dyn Fn(&str) -> Option<f64>,
    ) -> AsgInputs {
        let mut inputs = AsgInputs::default();
        let Some(group) = self.asg.get(asg_name).or_else(|| self.asg.get("DEFAULT")) else {
            return inputs;
        };
        for inp in &group.inp {
            inputs.record(inp.index, resolve(&inp.link));
        }
        inputs
    }

    /// Every distinct `(record, field)` an `INP*` link in this policy names,
    /// across all ASGs — the set a re-evaluation trigger must watch. C builds
    /// the same set one CA channel at a time in `asCaStart`.
    pub fn inp_link_targets(&self) -> Vec<(String, String)> {
        let mut targets = std::collections::BTreeSet::new();
        for group in self.asg.values() {
            for inp in &group.inp {
                let (record, field) = inp_link_target(&inp.link);
                targets.insert((record.to_string(), field.to_string()));
            }
        }
        targets.into_iter().collect()
    }

    /// Check access taking the per-record ASL into account.
    ///
    /// Per epics-base `asLibRoutines.c::asCompute`: a rule with
    /// `RULE(N, …)` only applies when the record's ASL ≤ N. The
    /// canonical example is `RULE(0, READ) RULE(1, WRITE)` — every
    /// record is readable, but only records with ASL ≥ 1 are
    /// writable. Without this gate, a low-ASL record's protection
    /// is silently equivalent to ASL 0.
    pub fn check_access_asl(
        &self,
        asg_name: &str,
        host: &str,
        user: &str,
        record_asl: u8,
    ) -> AccessLevel {
        // Forward to the method-aware path with default scopes
        // (any method, any authority). Mirrors epics-base PR #563:
        // legacy ACF rules without `METHOD`/`AUTHORITY` clauses match
        // every authentication method and authority. New code should
        // call `check_access_method` directly when method/authority
        // negotiation is observable.
        self.check_access_method(asg_name, host, user, record_asl, "", "")
    }
}

/// TRAPWRITE listener subsystem.
///
/// C `libcom/src/as/asLib.h:57-62` defines `asTrapWriteWithData` which
/// is invoked unconditionally around every `dbChannel_put` in
/// `rsrv/camessage.c:799-810`. Listeners registered via
/// `asTrapWriteRegisterListener` receive the put event — this is the
/// hook `caPutLog` and site put-loggers attach to. Pre-fix Rust
/// parsed the `TRAPWRITE`/`NOTRAPWRITE` keyword into
/// `AccessRule::trap` but had no listener subsystem, so the field
/// was a no-op and every put-logging tool migrating from rsrv saw
/// silent regression.
///
/// Rust API: registrations live in a process-wide RwLock-protected
/// `Vec<TrapWriteListener>`. The CA TCP dispatcher
/// (`crates/epics-ca-rs/src/server/tcp.rs`) calls
/// [`dispatch_trap_write`] before each `dbChannel_put`-equivalent
/// (op = `BeforeWrite`) and after the put completes (op = `AfterWrite`
/// with the post-write status). Listeners that need ACF-rule
/// trap-mask filtering can consult [`AccessChecked::rule_was_trap`]
/// via the message's `rule_was_trap` field — when `false`, libca-
/// faithful loggers should skip the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrapWriteOp {
    BeforeWrite,
    AfterWrite,
}

/// Read-only message handed to a [`TrapWriteListener`]. Held by
/// reference so the listener does not own any of the strings —
/// matches the C `asTrapWriteMessage` lifetime semantics
/// (`libcom/src/as/asLib.h:51-56`).
///
/// the message now carries the wire-level `dbr_type` and
/// `no_elements` that C's `asTrapWriteMessage` exposes
/// (`asLib.h:34-56`), plus a monotonic `event_id` that pairs the
/// `BeforeWrite` and `AfterWrite` for one put. libca passes
/// `userPvt` to the listener for per-event state (`asLib.h:45-51`),
/// returned-then-restored across the pair; Rust's listener takes a
/// `&TrapWriteMessage`, so listeners that need per-event state
/// maintain a private `event_id → state` map.
#[derive(Debug, Clone, Copy)]
pub struct TrapWriteMessage<'a> {
    pub op: TrapWriteOp,
    pub pv_name: &'a str,
    pub user: &'a str,
    pub host: &'a str,
    pub peer: &'a str,
    /// Pre-rendered value string. Empty when the listener subsystem
    /// is being notified at audit-off cost (caller may pass `""` to
    /// avoid stringifying large arrays just for trap dispatch).
    pub value_str: &'a str,
    /// wire DBR type the put came in as (`DBR_*` constant
    /// from `db_access.h`). Listeners that want to log or filter
    /// by type read it here instead of reaching back through
    /// `serverSpecific`.
    pub dbr_type: u16,
    /// element count from the put header
    /// (`asTrapWriteMessage::no_elements`). 1 for scalar, N for
    /// waveform.
    pub no_elements: u32,
    /// monotonic id that pairs the `BeforeWrite` and the
    /// matching `AfterWrite` for a single put. The C `userPvt`
    /// continuation slot is not a fit for `&` message — listeners
    /// that need per-event state should index a private map by
    /// this id and clear the entry in `AfterWrite`.
    pub event_id: u64,
    /// `Some("ok"|"fail"|EPICS error code) once `op == AfterWrite`;
    /// always `None` for `BeforeWrite`.
    pub status: Option<&'a str>,
    /// True iff the matched ACF `RULE(...)` had the `TRAPWRITE`
    /// option set. Loggers that want libca-faithful filtering should
    /// skip events with this `false` (mirrors C `pclient->trapMask`
    /// gate inside `asTrapWriteWithData`).
    pub rule_was_trap: bool,
}

/// monotonic id allocator for `TrapWriteMessage::event_id`.
/// Wraps at u64::MAX (~10^19 events; ~580 years at 1 Mput/s).
static TRAP_WRITE_EVENT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Allocate the next trap-write event id. Call once at the start of
/// a put dispatch, thread the value through `BeforeWrite` and the
/// matching `AfterWrite`.
pub fn next_trap_write_event_id() -> u64 {
    TRAP_WRITE_EVENT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Listener closure. Must be `Send + Sync` because the CA TCP
/// dispatcher invokes it from arbitrary tokio worker tasks. No
/// `async` — listeners that need to await must spawn their own task
/// off the closure (matches C's synchronous-callback contract; long
/// work in a listener blocks the wire path).
pub type TrapWriteListener = std::sync::Arc<dyn Fn(&TrapWriteMessage<'_>) + Send + Sync>;

/// Opaque handle returned by [`register_trap_write_listener`].
/// Drop the handle to unregister the listener (equivalent to C
/// `asTrapWriteUnregisterListener`).
pub struct TrapWriteListenerHandle {
    id: u64,
}

impl Drop for TrapWriteListenerHandle {
    fn drop(&mut self) {
        if let Some(reg) = TRAP_WRITE_REGISTRY.get() {
            let mut guard = reg.write().expect("trap-write registry poisoned");
            guard.retain(|(id, _)| *id != self.id);
        }
    }
}

static TRAP_WRITE_REGISTRY: std::sync::OnceLock<std::sync::RwLock<Vec<(u64, TrapWriteListener)>>> =
    std::sync::OnceLock::new();
static TRAP_WRITE_NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn trap_write_registry() -> &'static std::sync::RwLock<Vec<(u64, TrapWriteListener)>> {
    TRAP_WRITE_REGISTRY.get_or_init(|| std::sync::RwLock::new(Vec::new()))
}

/// Register a TRAPWRITE listener. The returned handle unregisters
/// the listener when dropped — keep it alive for as long as you
/// want events.
pub fn register_trap_write_listener(listener: TrapWriteListener) -> TrapWriteListenerHandle {
    let id = TRAP_WRITE_NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut guard = trap_write_registry()
        .write()
        .expect("trap-write registry poisoned");
    guard.push((id, listener));
    TrapWriteListenerHandle { id }
}

/// Cheap probe: returns `true` if any TRAPWRITE listener is
/// currently registered. Lets the CA TCP dispatcher skip rendering
/// the per-write value string when nothing would consume it.
/// O(1) — `RwLock::read` + `is_empty`.
pub fn has_trap_write_listeners() -> bool {
    let Some(reg) = TRAP_WRITE_REGISTRY.get() else {
        return false;
    };
    let guard = reg.read().expect("trap-write registry poisoned");
    !guard.is_empty()
}

/// Dispatch a trap-write event to every registered listener.
/// Fast path when no listeners: an `RwLock::read` and a length
/// check, no allocation. Called by the CA TCP dispatcher before and
/// after every `dbChannel_put`-equivalent.
///
/// the listener list is *snapshotted* under the read
/// lock (a `Vec<Arc<...>>` clone — cheap, all `Arc`-bumps), then
/// the lock is released before any listener runs. This means a
/// listener may register or drop another listener handle mid-
/// callback without deadlocking on the registry's
/// `std::sync::RwLock` (which is not re-entrant on POSIX); a
/// `TrapWriteListenerHandle::drop` racing dispatch on a tokio
/// worker thread does not block the worker for the unbounded
/// listener-call duration; the writer waits at most for the Vec
/// clone.
///
/// each listener call is wrapped in `catch_unwind` so a
/// panicking listener does not unwind into the CA per-circuit task.
/// The listener `Fn` type does NOT carry an `UnwindSafe` bound;
/// `AssertUnwindSafe` is sound here because the dispatch shares no
/// mutable state with the listener (the snapshot is consumed in
/// loop order; the message is `Copy`).
pub fn dispatch_trap_write(msg: &TrapWriteMessage<'_>) {
    let Some(reg) = TRAP_WRITE_REGISTRY.get() else {
        return;
    };
    let snapshot: Vec<TrapWriteListener> = {
        let guard = reg.read().expect("trap-write registry poisoned");
        if guard.is_empty() {
            return;
        }
        guard.iter().map(|(_, l)| l.clone()).collect()
    };
    for listener in snapshot {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            listener(msg);
        }));
        if let Err(payload) = result {
            let descr = if let Some(s) = payload.downcast_ref::<&'static str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "(non-string panic payload)".to_string()
            };
            tracing::error!(
                target: "epics_base_rs::server::access_security",
                pv = msg.pv_name,
                event_id = msg.event_id,
                op = ?msg.op,
                panic = %descr,
                "TRAPWRITE listener panicked — isolating; remaining listeners will still run. \
                 C asTrapWriteWithData has no unwind concept; this is a Rust-only safety net \
                 to keep the per-circuit task alive."
            );
        }
    }
}

/// Owned trap-write identity used to construct a [`TrapWriteGuard`].
///
/// Unlike [`TrapWriteMessage`] (borrowed and `Copy`), these fields are
/// owned so the guard can outlive the call frame that created it —
/// survive a move into a spawned put-completion task and live across
/// `.await` points until the put really finishes, is superseded, or the
/// connection tears down.
pub struct TrapWriteFields {
    pub pv_name: String,
    pub user: String,
    pub host: String,
    pub peer: String,
    pub value_str: String,
    pub dbr_type: u16,
    pub no_elements: u32,
    pub event_id: u64,
    pub rule_was_trap: bool,
    /// AfterWrite `status` dispatched when the guard is dropped without
    /// a preceding [`TrapWriteGuard::complete`] — i.e. the put was
    /// cancelled / superseded / torn down before its real status was
    /// known. C `asTrapWriteAfter` carries no status; this Rust-only
    /// field lets listeners distinguish a cancelled tail from a clean
    /// completion.
    pub cancel_status: String,
}

impl TrapWriteFields {
    fn message<'a>(&'a self, op: TrapWriteOp, status: Option<&'a str>) -> TrapWriteMessage<'a> {
        TrapWriteMessage {
            op,
            pv_name: &self.pv_name,
            user: &self.user,
            host: &self.host,
            peer: &self.peer,
            value_str: &self.value_str,
            dbr_type: self.dbr_type,
            no_elements: self.no_elements,
            event_id: self.event_id,
            status,
            rule_was_trap: self.rule_was_trap,
        }
    }
}

/// RAII guard that pairs one `asTrapWrite` BeforeWrite/AfterWrite
/// bracket so the AfterWrite fires on *every* exit path of a record
/// put — normal completion, early return, async cancellation (the
/// future that owns the guard is dropped mid-`.await`), or task abort
/// (a superseding WRITE_NOTIFY or a client teardown aborting the
/// completion task).
///
/// This makes the C invariant hold *by construction*: every
/// `asTrapWriteWithData` (BeforeWrite) is matched by exactly one
/// `asTrapWriteAfter` (AfterWrite) on all rsrv exit paths — normal
/// completion (`rsrv/camessage.c:1431`), still-busy teardown
/// (`rsrvFreePutNotify`, `camessage.c:1621-1660`), and supersede-cancel
/// (`write_notify_action`, `camessage.c:1741-1744`) — and mirrors pvxs's
/// `SecurityLogger`, whose destructor calls `asTrapWriteAfterWrite`
/// (`ioc/securitylogger.h:23-59`). Before this guard the Rust emitters
/// dispatched AfterWrite from an explicit call that an
/// aborted/superseded/cancelled put skipped, leaving a BeforeWrite with
/// no matching AfterWrite in the put-log.
///
/// Lifecycle:
/// - [`TrapWriteGuard::begin`] fires BeforeWrite and arms the guard.
/// - [`TrapWriteGuard::complete`] fires AfterWrite *now* with the real
///   put status and disarms the guard (Drop becomes a no-op). Call it on
///   the normal path once the put status is known.
/// - If the guard is dropped while still armed (any cancel path), Drop
///   fires AfterWrite with [`TrapWriteFields::cancel_status`].
///
/// AfterWrite therefore fires exactly once: either from `complete` or
/// from Drop, never both, never neither.
pub struct TrapWriteGuard {
    /// `Some` while an AfterWrite is still owed. `begin` leaves it
    /// `None` when no listener is registered (the whole bracket is a
    /// no-op); `complete` takes it to fire-and-disarm.
    armed: Option<Box<TrapWriteFields>>,
}

impl TrapWriteGuard {
    /// Fire BeforeWrite and arm the AfterWrite finalizer.
    ///
    /// When no trap-write listener is registered this returns a
    /// disarmed no-op guard (its `complete`/Drop dispatch nothing), so a
    /// caller may hold a guard unconditionally. Callers still gate on
    /// the ACF trap mask (`rule_was_trap`) before constructing one — a
    /// non-trapped put must never open a bracket (C `asActive &&
    /// trapMask`, `asLib.h:57`).
    pub fn begin(fields: TrapWriteFields) -> Self {
        if !has_trap_write_listeners() {
            return Self { armed: None };
        }
        dispatch_trap_write(&fields.message(TrapWriteOp::BeforeWrite, None));
        Self {
            armed: Some(Box::new(fields)),
        }
    }

    /// Fire AfterWrite now with the real put `status` and disarm the
    /// guard so Drop does nothing. A no-op on an already-disarmed guard
    /// (no listener at `begin`, or `complete` already called), so it is
    /// safe to call on every normal-completion path.
    pub fn complete(&mut self, status: &str) {
        if let Some(fields) = self.armed.take() {
            dispatch_trap_write(&fields.message(TrapWriteOp::AfterWrite, Some(status)));
        }
    }
}

impl Drop for TrapWriteGuard {
    fn drop(&mut self) {
        if let Some(fields) = self.armed.take() {
            dispatch_trap_write(
                &fields.message(TrapWriteOp::AfterWrite, Some(&fields.cancel_status)),
            );
        }
    }
}

/// Per-write trap-log identity that does not depend on the value being
/// written. Borrows the caller's identity strings (matching the C
/// `asTrapWriteMessage` by-reference lifetime, `asLib.h:34-56`).
pub struct TrapWriteMeta<'a> {
    /// The channel (`record.FIELD`) being written — pvxs passes
    /// `dbChannelName(pChan)`.
    pub pv_name: &'a str,
    /// Authenticated account name (pvxs `cred->account`).
    pub user: &'a str,
    /// Client host (pvxs `cred->host`).
    pub host: &'a str,
    /// Client peer ("ip:port") when the caller has the socket address;
    /// callers whose identity block carries no separate peer pass the
    /// host again.
    pub peer: &'a str,
    /// Final field DBF type of the channel (pvxs
    /// `dbChannelFinalFieldType`).
    pub dbr_type: u16,
}

/// The C `asActive && trapMask` gate (`asLib.h:57-60`), as one function.
///
/// `rule_was_trap` is the matched ACF/ASG rule's `TRAPWRITE` flag,
/// resolved once by the access layer ([`AccessChecked::rule_was_trap`]);
/// the listener probe is the `asActive` half. A caller that must pay for
/// something *before* opening the bracket — rendering a value, resolving
/// the channel's DBF type — asks here rather than re-spelling the
/// conjunction, so there is exactly one statement of when a put is
/// audited.
pub fn trap_write_armed(rule_was_trap: bool) -> bool {
    rule_was_trap && has_trap_write_listeners()
}

/// Bracket one backing record PUT with the EPICS `asTrapWrite`
/// put-logging hook, then run and return the write's result.
///
/// The single write-owner shared by every server that writes a local
/// record on behalf of a remote client: the QSRV bridge, the native PVA
/// [`ChannelSource`](crate::server::database::PvDatabase) over a
/// `PvDatabase`, and any future source with the same job. pvxs keeps the
/// equivalent bracket in ONE place too — `IOCSource::doPreProcessing`
/// builds a `SecurityLogger` (`ioc/iocsource.cpp:363-374`,
/// `ioc/securitylogger.h:29-58`) that every IOC source's put runs
/// through (`ioc/singlesource.cpp:354-360`,
/// `ioc/groupsource.cpp:594-602`).
///
/// When the write is not trapped ([`trap_write_armed`] is false) the
/// write runs unbracketed and nothing is dispatched. On a trapped write
/// this emits exactly one `BeforeWrite` (before the put) and exactly one
/// `AfterWrite` (after the put completes, on every exit path — including
/// the future being dropped mid-write) carrying the same `event_id`,
/// value string and `ok`/`fail` status. The value is rendered once
/// (truncated to 64 elements, like the CA dispatcher) only when actually
/// emitting.
pub async fn put_with_trap<T, E, F, Fut>(
    rule_was_trap: bool,
    meta: TrapWriteMeta<'_>,
    value: crate::types::EpicsValue,
    write: F,
) -> Result<T, E>
where
    F: FnOnce(crate::types::EpicsValue) -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    if !trap_write_armed(rule_was_trap) {
        return write(value).await;
    }

    let mut guard = TrapWriteGuard::begin(trap_fields(&meta, &value));
    let result = write(value).await;
    guard.complete(if result.is_ok() { "ok" } else { "fail" });
    result
}

/// [`put_with_trap`]'s synchronous twin, for a write already holding the
/// record's advisory gate (the QSRV atomic group PUT, `already_locked`
/// entries). C's `SecurityLogger` bracket is plain synchronous C++ with
/// no `async` concept at all — this is that shape. There is no
/// cancellation-mid-write case; the same RAII guard still balances the
/// trap log on a panic unwinding through `write`.
pub fn put_with_trap_blocking<T, E, F>(
    rule_was_trap: bool,
    meta: TrapWriteMeta<'_>,
    value: crate::types::EpicsValue,
    write: F,
) -> Result<T, E>
where
    F: FnOnce(crate::types::EpicsValue) -> Result<T, E>,
{
    if !trap_write_armed(rule_was_trap) {
        return write(value);
    }

    let mut guard = TrapWriteGuard::begin(trap_fields(&meta, &value));
    let result = write(value);
    guard.complete(if result.is_ok() { "ok" } else { "fail" });
    result
}

fn trap_fields(meta: &TrapWriteMeta<'_>, value: &crate::types::EpicsValue) -> TrapWriteFields {
    TrapWriteFields {
        pv_name: meta.pv_name.to_string(),
        user: meta.user.to_string(),
        host: meta.host.to_string(),
        peer: meta.peer.to_string(),
        value_str: value.display_truncated(64),
        dbr_type: meta.dbr_type,
        no_elements: value.count(),
        event_id: next_trap_write_event_id(),
        rule_was_trap: true,
        cancel_status: "cancel".to_string(),
    }
}

/// ASG-field change notifier.
///
/// C `database/src/ioc/as/asDbLib.c:107-110,144` registers
/// `asSpcAsCallback` as the per-record `ASG` field's special
/// callback; `dbPut record.ASG NEW_ASG` invokes `asChangeGroup` →
/// `asAddMemberPvt` → `asComputePvt` for every `ASGCLIENT` and
/// fires the COAR callback for each affected CA connection. Pre-fix
/// Rust mutated `instance.common.asg` directly with no notification
/// — the *next* CA op used live `compute_access` so enforcement was
/// correct, but the wire ACCESS_RIGHTS the client saw still
/// reflected the OLD ASG until something else (CLIENT_NAME / ACF
/// reload) triggered a re-eval. UIs gating put-button enable on the
/// cached level showed stale state.
///
/// Rust path: every record put that targets the `ASG` field calls
/// [`notify_asg_field_changed`]; the CA server (ca-rs
/// `server/tcp.rs`) subscribes via [`subscribe_asg_changes`] at
/// startup and routes the event into the same per-client
/// `reeval_access_rights` path the ACF reload uses. Coarser than
/// libca (we re-eval every connection on any ASG-field change, not
/// just the connections whose `ASGCLIENT` referenced the changed
/// record), but the wire shape (ACCESS_RIGHTS push only when level
/// actually changed) already keeps the cost bounded by the
/// `oldaccess != access` gate downstream.
static ASG_CHANGE_BROADCAST: std::sync::OnceLock<tokio::sync::broadcast::Sender<()>> =
    std::sync::OnceLock::new();

fn asg_change_broadcast() -> &'static tokio::sync::broadcast::Sender<()> {
    ASG_CHANGE_BROADCAST.get_or_init(|| {
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        tx
    })
}

/// Monotonic count of ASG-field changes, for pull-style consumers
/// (see [`asg_change_generation`]) that cannot hold a broadcast
/// receiver — e.g. a sync access-check cache that must know whether
/// a cached (channel → ASG) resolution is still current.
static ASG_CHANGE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Fire from the field-I/O layer when a record's `ASG` field is
/// successfully written. Idempotent: if no subscriber exists yet
/// the send is a no-op (lagged subscribers also tolerated — the
/// wire re-eval is coarse and one missed beat is recovered by the
/// downstream `oldaccess != access` filter).
pub fn notify_asg_field_changed() {
    ASG_CHANGE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
    let _ = asg_change_broadcast().send(());
}

/// Current ASG-field-change generation. A consumer that caches
/// anything derived from a record's `ASG` field snapshots this before
/// resolving and treats its entry as stale once the value moves —
/// the pull-side counterpart of [`subscribe_asg_changes`]. C's
/// equivalent invalidation is `asChangeGroup` re-running
/// `asComputePvt` for every `ASGCLIENT` on `dbPut record.ASG`.
pub fn asg_change_generation() -> u64 {
    ASG_CHANGE_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

/// Subscribe to ASG-field-change notifications. Called once at
/// server start by the CA TCP dispatcher; events are folded into
/// the per-client `reeval_access_rights` path.
pub fn subscribe_asg_changes() -> tokio::sync::broadcast::Receiver<()> {
    asg_change_broadcast().subscribe()
}

/// Parse an ACF (Access Control File).
/// C `asDumpQuoted` (asLibRoutines.c:660-666, epics-base #871): print a
/// UAG/HAG member as `"` + `epicsStrPrintEscaped` + `"`. C passes
/// `strlen(s)`, so the escape never runs past a NUL byte.
fn dump_quoted(out: &mut String, s: &str) {
    let bytes = s.as_bytes();
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    out.push('"');
    out.push_str(&crate::runtime::epics_string::print_escaped(&bytes[..len]));
    out.push('"');
}

pub fn parse_acf(content: &str) -> CaResult<AccessSecurityConfig> {
    let mut config = AccessSecurityConfig {
        uag: HashMap::new(),
        hag: HashMap::new(),
        hag_raw: HashMap::new(),
        asg: HashMap::new(),
        unknown_access: AccessLevel::Read,
    };

    // C `asInitialize` (asLibRoutines.c:107) calls `asAsgAdd(DEFAULT)`
    // *before* parsing the file, so a `DEFAULT` ASG always exists.
    // Synthesise it here unconditionally: any record whose
    // `ASG` field names an unknown group resolves to this empty
    // `DEFAULT`, which has no RULEs ⇒ `asNOACCESS` ⇒ access denied.
    // A `DEFAULT` block declared in the file simply overwrites this
    // placeholder below.
    config
        .asg
        .insert("DEFAULT".to_string(), AccessSecurityGroup::default());

    let mut chars = content.chars().peekable();
    let mut buf = String::new();

    while chars.peek().is_some() {
        skip_ws_comments(&mut chars);
        buf.clear();
        read_word(&mut chars, &mut buf);

        match buf.as_str() {
            "UAG" => {
                let name = read_paren_name(&mut chars)?;
                let members = read_brace_list(&mut chars)?;
                config.uag.insert(name, members);
            }
            "HAG" => {
                let name = read_paren_name(&mut chars)?;
                let members = read_brace_list(&mut chars)?;
                // C `asHagAddHost` reads `asCheckClientIP` at ACF-parse
                // time and stores names or resolved IPs accordingly.
                config.hag.insert(name.clone(), hag_members(&members));
                config.hag_raw.insert(name, members);
            }
            "ASG" => {
                let name = read_paren_name(&mut chars)?;
                let asg = parse_asg_body(&mut chars)?;
                config.asg.insert(name, asg);
            }
            "" => {
                // `read_word` only consumes `[A-Za-z0-9_]`, and
                // `skip_ws_comments` already ran above. So an empty
                // word means one of two things:
                //
                //   * genuine EOF / whitespace-only / comment-only
                //     input — `chars.peek()` is `None` ⇒ break, `Ok`
                //     (the pre-existing, deliberate empty-file
                //     divergence from C; see `empty_acf_denies_all_access`);
                //   * a stray top-level punctuation token where a
                //     block keyword is expected (`(`, `)`, `{`, `}`,
                //     `,`) — C's grammar has no production starting
                //     with bare punctuation at top level ⇒ `yyerror`.
                //     A file of only `(((` or only `}` is genuine
                //     garbage and must fail closed.
                match chars.peek() {
                    Some(&c) if matches!(c, '(' | ')' | '{' | '}' | ',') => {
                        return Err(CaError::Protocol(format!(
                            "ACF: unexpected '{c}' where a top-level block keyword is expected"
                        )));
                    }
                    // EOF, or any other stray character — preserve the
                    // pre-existing break-and-`Ok` behaviour; only the
                    // stray block-punctuation case is in scope here.
                    _ => break,
                }
            }
            other => {
                // C `asLib.y:88-103` (`generic_item`) treats an
                // unrecognised top-level *block* as a *warning*
                // (`yywarn "Ignoring unsupported TOP LEVEL block"`) and
                // parsing continues — forward-compat with future/vendor
                // ACF extensions.
                //
                // The leniency is bounded by the grammar: every
                // `generic_item` alternative is `tokenSTRING
                // generic_head [...]`, and `generic_head`
                // (asLib.y:105-108) is `'(' ... ')'` — a *mandatory*
                // balanced parenthesised head. There is no
                // `generic_item: tokenSTRING` alone. So C only warns
                // when the unknown keyword is immediately followed by
                // `(`; a bare keyword (followed by another word, or at
                // EOF) matches no rule ⇒ `yyerror` ⇒ `asInitialize`
                // fails. `skip_unknown_top_level_block` enforces exactly
                // that: it returns `Err` for genuine garbage and `Ok`
                // (after warning) for a well-formed unknown block.
                skip_unknown_top_level_block(other, &mut chars)?;
            }
        }
    }

    Ok(config)
}

/// Skip an unrecognised top-level block: a *mandatory* `(...)` head and
/// an optional `{...}` body. Mirrors C `asLib.y` `generic_item`
/// (asLib.y:88-103) + `generic_head` (asLib.y:105-108) — the only
/// recover-and-continue posture C allows for an unknown keyword.
///
/// C's grammar makes the parenthesised head mandatory: every
/// `generic_item` alternative is `tokenSTRING generic_head [...]`, and
/// `generic_head` is `'(' ')'` | `'(' generic_element ')'` |
/// `'(' generic_list ')'`. So:
///
/// * unknown keyword **followed by `(`** with a balanced head ⇒ warn
///   and continue (`yywarn "Ignoring unsupported TOP LEVEL block"`);
/// * unknown keyword **not** followed by `(` (another bare word, or
///   EOF) ⇒ no grammar rule matches ⇒ C `yyerror` ⇒ `asInitialize`
///   fails. Return `Err`;
/// * unbalanced parens/braces (depth never returns to 0 before EOF) ⇒
///   the C lexer/parser raises `yyerror` ⇒ return `Err`.
fn skip_unknown_top_level_block(
    keyword: &str,
    chars: &mut std::iter::Peekable<std::str::Chars>,
) -> CaResult<()> {
    skip_ws_comments(chars);
    // C `generic_head` requires a `(` here. A bare keyword with another
    // word or EOF after it matches no production ⇒ hard parse error.
    if chars.peek() != Some(&'(') {
        return Err(CaError::Protocol(format!(
            "ACF: unexpected token '{keyword}' — expected a top-level \
             UAG/HAG/ASG block or an unknown keyword followed by '('"
        )));
    }
    // Consume the balanced `(...)` head. Unbalanced ⇒ error.
    let mut depth = 0;
    let mut closed = false;
    while let Some(&c) = chars.peek() {
        chars.next();
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    closed = true;
                    break;
                }
            }
            _ => {}
        }
    }
    if !closed {
        return Err(CaError::Protocol(format!(
            "ACF: unbalanced '(' in unsupported top-level block '{keyword}'"
        )));
    }
    skip_ws_comments(chars);
    // The `{...}` body is optional (the `tokenSTRING generic_head` bare
    // form). If present it must be balanced.
    if chars.peek() == Some(&'{') {
        let mut depth = 0;
        let mut closed = false;
        while let Some(&c) = chars.peek() {
            chars.next();
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        closed = true;
                        break;
                    }
                }
                _ => {}
            }
        }
        if !closed {
            return Err(CaError::Protocol(format!(
                "ACF: unbalanced '{{' in unsupported top-level block '{keyword}'"
            )));
        }
    }
    // Well-formed unknown block: warn and continue.
    tracing::warn!(
        target: "epics_base_rs::access_security",
        keyword = %keyword,
        "ACF: ignoring unsupported top-level block"
    );
    Ok(())
}

/// C `asCheckClientIP` (`asLibRoutines.c:34`) — process-global, default
/// `0`/false, set from the shell before the ACF is loaded.
///
/// It is the **single owner of what a host identity means** across access
/// security, and it decides two things that must agree or nothing matches:
///
/// * how `HAG` members are stored ([`hag_members`]) — lowercased literal
///   names, or resolved dotted-quad IPs;
/// * what the CA server records as a client's host — the name the client
///   claims over `CA_PROTO_HOST_NAME`, or its peer IP
///   (`camessage.c:839-843`, `caservertask.c:1425-1437`).
///
/// C's default is `0`: rsrv stores the client-supplied hostname
/// unconditionally and HAGs match on names. The IP-checking mode is opt-in.
static AS_CHECK_CLIENT_IP: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Read the `AS_CHECK_CLIENT_IP` mode.
pub fn as_check_client_ip() -> bool {
    AS_CHECK_CLIENT_IP.load(std::sync::atomic::Ordering::Relaxed)
}

/// Set the `AS_CHECK_CLIENT_IP` mode. C exposes this as an iocsh
/// *variable* (`var asCheckClientIP 1`, registered in
/// `libComRegister.c:491-495`, `:535-537`), and so does this port —
/// `var asCheckClientIP 1` reaches this setter through the iocsh
/// variable table.
///
/// Ordering is C's: `hag_members` reads the flag when the ACF is
/// *parsed*, so — exactly as in C — it must be set **before** `asInit`,
/// or the HAG entries are stored in the wrong form.
pub fn set_as_check_client_ip(on: bool) {
    AS_CHECK_CLIENT_IP.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Store one HAG's members the way C `asHagAddHost`
/// (`asLibRoutines.c:1218-1256`) does, which depends on
/// [`as_check_client_ip`]:
///
/// * **default (`false`)** — each host is stored as a lowercased literal
///   name. The client identity it is matched against is the name the
///   client claimed over `CA_PROTO_HOST_NAME`, so no DNS is involved on
///   either side.
/// * **`true`** — each host is resolved to a dotted-quad IP at parse time;
///   an unresolvable entry is stored as `unresolved:<host>` (C's own
///   sentinel, which simply never matches) rather than aborting the load.
///   The client identity is then the peer IP.
///
/// The two halves are read from the same flag on purpose: a mixed
/// configuration (names on one side, IPs on the other) matches nothing,
/// which is precisely the R7-16 defect.
fn hag_members(members: &[String]) -> Vec<String> {
    if !as_check_client_ip() {
        return members.iter().map(|m| m.to_ascii_lowercase()).collect();
    }

    use std::net::ToSocketAddrs;
    members
        .iter()
        .map(|m| match format!("{m}:0").to_socket_addrs() {
            // C `aToIPAddr` resolves via `AF_INET` and the CA server keys ACF on
            // an IPv4 peer address, so a HAG host must store its **IPv4** dotted
            // quad. Taking `iter.next()` blindly would store an IPv6 address on a
            // dual-stack host that resolves `::1` first (e.g. `localhost` on many
            // CI runners) — an entry no IPv4 CA peer could ever match. A host with
            // no IPv4 address is stored as the `unresolved:` sentinel, exactly as
            // C does when `aToIPAddr` yields nothing.
            Ok(iter) => match iter.filter(|sa| sa.is_ipv4()).map(|sa| sa.ip()).next() {
                Some(ip) => ip.to_string(),
                None => format!("unresolved:{m}"),
            },
            Err(e) => {
                tracing::warn!(
                    target: "epics_base_rs::access_security",
                    host = %m,
                    error = %e,
                    "ACF: Unable to resolve host (asCheckClientIP=1)"
                );
                format!("unresolved:{m}")
            }
        })
        .collect()
}

fn skip_ws_comments(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '#' {
            // Skip line comment
            while let Some(&c) = chars.peek() {
                chars.next();
                if c == '\n' {
                    break;
                }
            }
        } else {
            break;
        }
    }
}

fn read_word(chars: &mut std::iter::Peekable<std::str::Chars>, buf: &mut String) {
    while let Some(&c) = chars.peek() {
        if c.is_alphanumeric() || c == '_' {
            buf.push(c);
            chars.next();
        } else {
            break;
        }
    }
}

fn read_paren_name(chars: &mut std::iter::Peekable<std::str::Chars>) -> CaResult<String> {
    skip_ws_comments(chars);
    if chars.next() != Some('(') {
        return Err(CaError::Protocol("ACF: expected '('".into()));
    }
    skip_ws_comments(chars);
    // L-4: C's lexer requires a single `tokenSTRING` then `')'`.
    // Accept an optional double-quoted form; in the unquoted form
    // interior whitespace ends the name — a second non-space run
    // before `)` is a parse error rather than being silently merged
    // (`UAG(my group)` must NOT become `mygroup`). EOF before `)` is
    // also an error.
    let mut name = String::new();
    if chars.peek() == Some(&'"') {
        chars.next();
        let mut closed = false;
        while let Some(&c) = chars.peek() {
            chars.next();
            if c == '"' {
                closed = true;
                break;
            }
            name.push(c);
        }
        if !closed {
            return Err(CaError::Protocol("ACF: unterminated quoted name".into()));
        }
        skip_ws_comments(chars);
        if chars.next() != Some(')') {
            return Err(CaError::Protocol(
                "ACF: expected ')' after quoted name".into(),
            ));
        }
        return Ok(name);
    }
    loop {
        match chars.peek() {
            Some(&')') => {
                chars.next();
                break;
            }
            Some(&c) if c.is_whitespace() => {
                // Whitespace ends the name. Allow only trailing
                // whitespace before `)`; reject embedded whitespace.
                skip_ws_comments(chars);
                match chars.peek() {
                    Some(&')') => {
                        chars.next();
                        break;
                    }
                    Some(_) => {
                        return Err(CaError::Protocol(
                            "ACF: whitespace inside parenthesised name".into(),
                        ));
                    }
                    None => {
                        return Err(CaError::Protocol(
                            "ACF: unterminated '(' — missing ')'".into(),
                        ));
                    }
                }
            }
            Some(&c) => {
                name.push(c);
                chars.next();
            }
            None => {
                return Err(CaError::Protocol(
                    "ACF: unterminated '(' — missing ')'".into(),
                ));
            }
        }
    }
    Ok(name)
}

fn read_brace_list(chars: &mut std::iter::Peekable<std::str::Chars>) -> CaResult<Vec<String>> {
    skip_ws_comments(chars);
    if chars.next() != Some('{') {
        return Err(CaError::Protocol("ACF: expected '{'".into()));
    }
    let mut items = Vec::new();
    let mut current = String::new();

    loop {
        skip_ws_comments(chars);
        match chars.peek() {
            Some(&'}') => {
                chars.next();
                break;
            }
            Some(&',') => {
                chars.next();
                if !current.is_empty() {
                    items.push(current.clone());
                    current.clear();
                }
            }
            // Quoted string: asLib_lex.l `{doublequote}({stringchar}|{escape})*{doublequote}`
            // where stringchar is [^"\n\\]. Allows '/' so "role/groupname" entries work.
            // pvxs/documentation/ioc.rst shows: UAG(special) { someone, "role/op" }
            Some(&'"') => {
                chars.next(); // consume opening '"'
                if !current.is_empty() {
                    items.push(current.clone());
                    current.clear();
                }
                let mut quoted = String::new();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => {
                            if let Some(esc) = chars.next() {
                                quoted.push(esc);
                            }
                        }
                        Some('\n') | None => {
                            return Err(CaError::Protocol(
                                "ACF: unterminated quoted string".into(),
                            ));
                        }
                        Some(c) => quoted.push(c),
                    }
                }
                if !quoted.is_empty() {
                    items.push(quoted);
                }
            }
            // Unquoted name: asLib_lex.l `name [a-zA-Z0-9_\-+:.\[\]<>;]`
            Some(&c)
                if c.is_alphanumeric()
                    || matches!(c, '_' | '.' | '-' | '+' | ':' | '[' | ']' | '<' | '>' | ';') =>
            {
                current.push(c);
                chars.next();
            }
            Some(_) => {
                chars.next();
            }
            None => return Err(CaError::Protocol("ACF: unterminated '{'".into())),
        }
    }
    if !current.is_empty() {
        items.push(current);
    }
    Ok(items)
}

fn parse_asg_body(
    chars: &mut std::iter::Peekable<std::str::Chars>,
) -> CaResult<AccessSecurityGroup> {
    skip_ws_comments(chars);
    if chars.next() != Some('{') {
        return Err(CaError::Protocol("ACF: expected '{' after ASG name".into()));
    }

    let mut asg = AccessSecurityGroup::default();

    loop {
        skip_ws_comments(chars);
        match chars.peek() {
            Some(&'}') => {
                chars.next();
                break;
            }
            Some(_) => {
                let mut kw = String::new();
                read_word(chars, &mut kw);
                if kw == "RULE" {
                    let rule = parse_rule(chars)?;
                    asg.rules.push(rule);
                } else if let Some(stripped) = kw.strip_prefix("INP") {
                    // `INP(A..U)("link")` — C `asLib_lex.l:48-52`
                    // lexes `INP[A-U]` as one token whose `Int64`
                    // payload is the letter index (`yytext[3] - 'A'`).
                    // `asLib.y:234-243` then reads the parenthesised
                    // link string.
                    let index = match parse_inp_index(stripped) {
                        Some(i) => i,
                        None => {
                            return Err(CaError::Protocol(format!(
                                "ACF: invalid INP link selector 'INP{stripped}' \
                                 (expected INPA..INPU)"
                            )));
                        }
                    };
                    let link = read_paren_name(chars)?;
                    asg.inp.push(AsgInp { index, link });
                } else if kw.is_empty() {
                    chars.next(); // skip unknown char
                }
                // Unknown alphanumeric keywords inside an ASG body are
                // skipped (forward-compat); the next loop iteration
                // resumes from the following token.
            }
            None => return Err(CaError::Protocol("ACF: unterminated ASG".into())),
        }
    }

    Ok(asg)
}

/// Parse the `A..U` selector suffix of an `INP` token into a 0-based
/// letter index. `"A"` → 0, .. `"U"` → 20. Anything else → `None`.
///
/// Case-SENSITIVE: C `asLib_lex.l:21,47` lexes the selector with the
/// flex pattern `INP[A-U]` (uppercase range only), so `INPa` does not
/// match the `tokenINP` rule — it is a syntax error in C, not selector
/// index 0. Matching only uppercase here keeps that behaviour; a
/// lowercase suffix returns `None` and the caller rejects the ASG.
fn parse_inp_index(suffix: &str) -> Option<u8> {
    let mut it = suffix.chars();
    let c = it.next()?;
    if it.next().is_some() {
        return None; // INP selector is exactly one letter
    }
    if ('A'..='U').contains(&c) {
        Some((c as u8) - b'A')
    } else {
        None
    }
}

fn parse_rule(chars: &mut std::iter::Peekable<std::str::Chars>) -> CaResult<AccessRule> {
    skip_ws_comments(chars);
    if chars.next() != Some('(') {
        return Err(CaError::Protocol("ACF: expected '(' after RULE".into()));
    }

    // Read level. C `asLib.y:253-258` requires `tokenINT64` and
    // rejects a negative or non-numeric level with `yyerror`, which
    // fails the whole ACF load (a fail-safe abort). Accept an optional
    // leading sign so a `RULE(-1, ...)` is detected and rejected
    // rather than silently re-read as level 1.
    skip_ws_comments(chars);
    let mut level_str = String::new();
    if matches!(chars.peek(), Some('+') | Some('-')) {
        level_str.push(chars.next().unwrap());
    }
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            level_str.push(c);
            chars.next();
        } else {
            break;
        }
    }
    let level_num: i64 = level_str.parse().map_err(|_| {
        CaError::Protocol(format!(
            "ACF: RULE level must be an integer, got '{level_str}'"
        ))
    })?;
    if level_num < 0 {
        return Err(CaError::Protocol(format!(
            "ACF: RULE LEVEL must be positive: {level_num}"
        )));
    }
    let level: u8 = u8::try_from(level_num)
        .map_err(|_| CaError::Protocol(format!("ACF: RULE level out of range: {level_num}")))?;

    skip_ws_comments(chars);
    if chars.peek() == Some(&',') {
        chars.next();
    }

    // Read access keyword. C `asLib.y:259-264` matches `NONE`/`READ`/
    // `WRITE` with `strcmp` (case-SENSITIVE); any other keyword triggers
    // `yywarn "Ignoring RULE that contains an unsupported keyword"`
    // and the rule is dropped. Match case-sensitively here too: a
    // case variant like `write` is an unsupported keyword in C, so it
    // must not build an active Write rule. We keep the rule but mark it
    // `ignore` (inert) so any unsupported keyword fails CLOSED — the
    // same effect as C dropping the rule.
    skip_ws_comments(chars);
    let mut access_str = String::new();
    read_word(chars, &mut access_str);
    let (access, mut ignore) = if access_str == "WRITE" {
        (RuleAccess::Write, false)
    } else if access_str == "READ" {
        (RuleAccess::Read, false)
    } else if access_str == "NONE" {
        (RuleAccess::None, false)
    } else {
        tracing::warn!(
            target: "epics_base_rs::access_security",
            keyword = %access_str,
            "ACF: ignoring RULE with unsupported access keyword"
        );
        (RuleAccess::None, true)
    };

    // Optional log option: `RULE(level, access, TRAPWRITE)` /
    // `RULE(level, access, NOTRAPWRITE)`. C `asLib.y:272-283`
    // (`rule_log_option`) matches both with `strcmp` (case-SENSITIVE):
    // a lowercase `trapwrite` matches neither and hits
    // `yyerror "Log options must be TRAPWRITE or NOTRAPWRITE"`, which
    // fails the whole ACF load. Match case-sensitively here too so a
    // case variant is rejected rather than silently accepted. The trap
    // mask is captured in `AccessRule::trap`; the `asTrapWrite`
    // put-logging listener that would consume it is a separate
    // subsystem not present in this crate (see the UNFIXED note).
    let mut trap = false;
    skip_ws_comments(chars);
    if chars.peek() == Some(&',') {
        chars.next();
        skip_ws_comments(chars);
        let mut log_opt = String::new();
        read_word(chars, &mut log_opt);
        if log_opt == "TRAPWRITE" {
            trap = true;
        } else if log_opt != "NOTRAPWRITE" {
            return Err(CaError::Protocol(format!(
                "ACF: RULE log option must be TRAPWRITE or NOTRAPWRITE, got '{log_opt}'"
            )));
        }
    }

    skip_ws_comments(chars);
    if chars.peek() == Some(&')') {
        chars.next();
    }

    // Optional body with UAG/HAG/METHOD/AUTHORITY/CALC.
    let mut uag = Vec::new();
    let mut hag = Vec::new();
    let mut method = Vec::new();
    let mut authority = Vec::new();
    let mut calc: Option<String> = None;

    skip_ws_comments(chars);
    if chars.peek() == Some(&'{') {
        chars.next();
        loop {
            skip_ws_comments(chars);
            match chars.peek() {
                Some(&'}') => {
                    chars.next();
                    break;
                }
                Some(_) => {
                    let mut kw = String::new();
                    read_word(chars, &mut kw);
                    if kw == "UAG" {
                        let name = read_paren_name(chars)?;
                        uag.push(name);
                    } else if kw == "HAG" {
                        let name = read_paren_name(chars)?;
                        hag.push(name);
                    } else if kw == "METHOD" {
                        // PR #563: METHOD("ca", "x509", ...)
                        method.extend(read_paren_string_list(chars)?);
                    } else if kw == "AUTHORITY" {
                        // PR #563/#618: AUTHORITY("CA Issuer", ...)
                        authority.extend(read_paren_string_list(chars)?);
                    } else if kw == "CALC" {
                        // `CALC("<expr>")` — C `asLib.y:294-299`.
                        // The expression gates the rule against the
                        // ASG's INP* link values. Take the *last*
                        // CALC clause if several are given (matches
                        // C `asAsgRuleCalc` last-wins overwrite).
                        let expr = read_paren_name_raw(chars)?;
                        calc = Some(expr);
                    } else if kw.is_empty() {
                        // Unknown punctuation — advance to avoid infinite loop.
                        chars.next();
                    } else {
                        // C `asLib.y:300-306`: a RULE body with
                        // an unsupported keyword is *disabled* by
                        // `asAsgRuleDisable`. Mark the rule inert and
                        // consume the keyword's `(...)` argument if
                        // present so parsing recovers.
                        tracing::warn!(
                            target: "epics_base_rs::access_security",
                            keyword = %kw,
                            "ACF: ignoring RULE with unsupported keyword — rule disabled"
                        );
                        ignore = true;
                        skip_ws_comments(chars);
                        if chars.peek() == Some(&'(') {
                            let _ = read_paren_name(chars)?;
                        }
                    }
                }
                None => break,
            }
        }
    }

    // a CALC clause must actually gate the rule. This crate's
    // access-security layer has no `INP*` database-link resolution
    // (the `AsgInp` links are stored but never read), so the calc
    // expression cannot be evaluated at access-check time. C disables
    // any rule it cannot fully honour (`asAsgRuleDisable`); to fail
    // CLOSED we do the same — a present-but-unevaluable CALC condition
    // marks the rule inert rather than letting it become an
    // unconditional grant. The expression is still validated (compiled)
    // here so a syntactically broken CALC is rejected exactly as C's
    // `postfix()` rejects it in `asAsgRuleCalc`.
    // compile the CALC expression at parse (C `postfix()` rejects a
    // broken one in `asAsgRuleCalc` and stores the RPN for every later
    // `asComputePvt`). The rule is conditionally active and gated at
    // access-check time by `compute_rules`'s `calc_ok`, which resolves
    // the ASG's INP* links and evaluates the stored program. When no
    // INP* resolver is installed the evaluator returns false (fail
    // closed), preserving the previous deny behaviour without
    // hard-disabling the rule.
    let mut inp_used: u32 = 0;
    let calc_compiled = match calc {
        Some(ref expr) => {
            let compiled = crate::calc::compile(expr).map_err(|e| {
                CaError::Protocol(format!("ACF: bad CALC expression '{expr}': {e}"))
            })?;
            // C `asAsgRuleCalc` (`asLibRoutines.c:1416-1425`) runs
            // `calcArgUsage` right after `postfix()` and refuses the rule when
            // the expression stores into an argument:
            //
            //     /* Until someone proves stores are not dangerous, don't allow them */
            //     if (stores) { … status = S_asLib_badCalc; … }
            //
            // `asLib.y:294-299` turns that status into `yyerror("")`, so the
            // WHOLE file is rejected and a running IOC keeps its previous rule
            // set. Accepting the rest of the file instead would be strictly
            // less safe than C: the operator's edit would silently install a
            // weaker policy than the one they wrote.
            //
            // The danger is concrete — `CALC("A:=1")` evaluates to 1 whatever
            // the INP links read, so the rule becomes an unconditional grant
            // to everyone in the group.
            let (used, stores) = compiled.arg_usage();
            if stores != 0 {
                return Err(CaError::Protocol(format!(
                    "ACF: assignment operator used in CALC expression '{expr}'"
                )));
            }
            inp_used = used;
            Some(compiled)
        }
        None => None,
    };

    Ok(AccessRule {
        level,
        access,
        uag,
        hag,
        method,
        authority,
        trap,
        calc,
        calc_compiled,
        inp_used,
        ignore,
    })
}

/// Read a parenthesised, double-quoted string verbatim — used for the
/// `CALC("<expr>")` clause where the expression contains operators and
/// spaces that `read_paren_name` would mangle (it strips whitespace).
/// Accepts `( "expr" )` or `( expr )`; whitespace around the parens is
/// skipped, whitespace *inside* a quoted body is preserved.
fn read_paren_name_raw(chars: &mut std::iter::Peekable<std::str::Chars>) -> CaResult<String> {
    skip_ws_comments(chars);
    if chars.next() != Some('(') {
        return Err(CaError::Protocol("ACF: expected '(' after CALC".into()));
    }
    skip_ws_comments(chars);
    let mut body = String::new();
    if chars.peek() == Some(&'"') {
        chars.next();
        while let Some(&c) = chars.peek() {
            chars.next();
            if c == '"' {
                break;
            }
            body.push(c);
        }
        skip_ws_comments(chars);
        if chars.next() != Some(')') {
            return Err(CaError::Protocol(
                "ACF: expected ')' after CALC expression".into(),
            ));
        }
    } else {
        // Unquoted form — read until the closing paren.
        while let Some(&c) = chars.peek() {
            if c == ')' {
                chars.next();
                break;
            }
            body.push(c);
            chars.next();
        }
    }
    Ok(body.trim().to_string())
}

/// Parse `(item1, "item 2", ...)` — commas separate items, optional
/// quotes around each item are stripped. Used for METHOD/AUTHORITY
/// rule clauses (epics-base PR #563/#618). Whitespace inside an
/// unquoted item is preserved verbatim *between* word characters but
/// trimmed at the boundaries; the typical caller passes quoted strings.
fn read_paren_string_list(
    chars: &mut std::iter::Peekable<std::str::Chars>,
) -> CaResult<Vec<String>> {
    skip_ws_comments(chars);
    if chars.next() != Some('(') {
        return Err(CaError::Protocol(
            "ACF: expected '(' after METHOD/AUTHORITY".into(),
        ));
    }
    let mut items = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    loop {
        match chars.peek() {
            Some(&'"') => {
                chars.next();
                in_quotes = !in_quotes;
            }
            Some(&')') if !in_quotes => {
                chars.next();
                break;
            }
            Some(&',') if !in_quotes => {
                chars.next();
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    items.push(trimmed);
                }
                current.clear();
            }
            Some(&c) => {
                current.push(c);
                chars.next();
            }
            None => {
                return Err(CaError::Protocol(
                    "ACF: unterminated METHOD/AUTHORITY list".into(),
                ));
            }
        }
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        items.push(trimmed);
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`AS_CHECK_CLIENT_IP`] is a process-global flag, mirroring C's
    /// global IOC variable — every `parse_acf` call whose ACF has a
    /// `HAG(...)` block reads it via [`hag_members`]. Rust's default test
    /// runner runs every `#[test]` fn in this module concurrently on its
    /// own thread, so without serialization a test that never touches the
    /// flag can still observe a value flipped mid-flight by a sibling
    /// test's `set_as_check_client_ip(true)` / `(false)` window — that
    /// race is what turned `"host1"` into `"unresolved:host1"` under plain
    /// `cargo test`. Every test that reads or writes the flag (directly,
    /// or by parsing an ACF containing a `HAG` block) takes this lock for
    /// its whole body so at most one such test runs at a time; poisoning
    /// is ignored so one test's panic doesn't cascade-fail its siblings.
    static AS_CHECK_CLIENT_IP_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_as_check_client_ip() -> std::sync::MutexGuard<'static, ()> {
        AS_CHECK_CLIENT_IP_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn test_parse_acf_basic() {
        let _guard = lock_as_check_client_ip();
        let acf = r#"
UAG(admins) { user1, user2 }
HAG(operators) { host1, host2 }
ASG(DEFAULT) {
    RULE(1, WRITE) { UAG(admins) HAG(operators) }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        assert_eq!(config.uag.get("admins").unwrap(), &["user1", "user2"]);
        assert_eq!(config.hag.get("operators").unwrap(), &["host1", "host2"]);
        assert!(config.asg.contains_key("DEFAULT"));
        assert_eq!(config.asg["DEFAULT"].rules.len(), 2);
    }

    #[test]
    fn test_parse_acf_hag_uag() {
        let _guard = lock_as_check_client_ip();
        // Use `.invalid` so DNS resolution is guaranteed to fail
        // (RFC 6761 — every resolver returns NXDOMAIN). This isolates
        // the test from `expand_hag_members`' soft-DNS path: the
        // literal entry is preserved, and no resolved IPs are
        // appended.
        let acf = r#"
UAG(ops) { alice, bob }
HAG(lab) { lab-pc1.invalid }
ASG(SECURE) {
    RULE(1, WRITE) { UAG(ops) HAG(lab) }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        assert_eq!(config.uag["ops"], vec!["alice", "bob"]);
        assert_eq!(config.hag["lab"], vec!["lab-pc1.invalid"]);
    }

    /// R7-16 / C `asHagAddHost` (`asLibRoutines.c:1218-1256`): with
    /// `asCheckClientIP` at its default 0, a HAG host is stored as a
    /// lowercased **name** and nothing else — no DNS runs, and no resolved
    /// IP is appended. The identity it matches is the name the client
    /// claimed over `CA_PROTO_HOST_NAME`, so a peer IP must NOT match.
    ///
    /// The port used to append resolved IPs to every entry, because its CA
    /// server keyed ACF on the peer IP. Both halves are C's now.
    #[test]
    fn hag_stores_names_by_default() {
        let _guard = lock_as_check_client_ip();
        set_as_check_client_ip(false);
        let acf = r#"
HAG(local) { LocalHost }
ASG(DEFAULT) {
    RULE(1, WRITE) { HAG(local) }
}
"#;
        let config = parse_acf(acf).unwrap();
        assert_eq!(
            config.hag["local"],
            vec!["localhost"],
            "C stores the lowercased literal name and resolves nothing"
        );
        assert_eq!(
            config.check_access("DEFAULT", "localhost", "alice"),
            AccessLevel::ReadWrite,
            "the claimed host name matches the HAG"
        );
        assert_eq!(
            config.check_access("DEFAULT", "127.0.0.1", "alice"),
            AccessLevel::NoAccess,
            "a peer IP does not match a name HAG — that is what asCheckClientIP=1 is for"
        );
    }

    /// The other side of C's flag: `asCheckClientIP = 1` resolves every HAG
    /// host to a dotted-quad IP at ACF-parse time, and the CA server keys
    /// on the peer IP.
    #[test]
    fn hag_stores_resolved_ips_under_as_check_client_ip() {
        let _guard = lock_as_check_client_ip();
        set_as_check_client_ip(true);
        let acf = r#"
HAG(local) { localhost }
ASG(DEFAULT) {
    RULE(1, WRITE) { HAG(local) }
}
"#;
        let config = parse_acf(acf).unwrap();
        set_as_check_client_ip(false); // restore before asserting
        assert_eq!(
            config.hag["local"],
            vec!["127.0.0.1"],
            "C resolves the host to its IP under asCheckClientIP"
        );
        assert_eq!(
            config.check_access("DEFAULT", "127.0.0.1", "alice"),
            AccessLevel::ReadWrite,
            "the peer IP matches the resolved HAG"
        );
    }

    /// C `asHagAddHost` under `asCheckClientIP = 1` does not abort on a
    /// name it cannot resolve: it logs and stores `unresolved:<host>`, a
    /// sentinel that simply never matches.
    #[test]
    fn hag_unresolvable_under_as_check_client_ip_becomes_sentinel() {
        let _guard = lock_as_check_client_ip();
        set_as_check_client_ip(true);
        let config = parse_acf("HAG(lab) { lab-pc1.invalid }\n").unwrap();
        set_as_check_client_ip(false);
        assert_eq!(config.hag["lab"], vec!["unresolved:lab-pc1.invalid"]);
    }

    #[test]
    fn hag_unresolvable_name_does_not_abort_parser() {
        let _guard = lock_as_check_client_ip();
        // `.invalid` TLD guarantees NXDOMAIN (RFC 6761). Pre-fix
        // upstream would `abort()` here; we keep the literal entries
        // verbatim — no resolved IPs are appended and the parser
        // returns Ok. Comma separator matches the brace-list parser's
        // tokenization (whitespace alone is consumed silently).
        let acf = r#"
HAG(quarantine) { gone.invalid, alive.invalid }
ASG(DEFAULT) {
    RULE(1, WRITE) { HAG(quarantine) }
}
"#;
        let config = parse_acf(acf).expect("parser must not abort on bad DNS");
        let entries = &config.hag["quarantine"];
        assert_eq!(
            entries.len(),
            2,
            "literal entries preserved verbatim; no resolved IPs appended"
        );
        assert_eq!(entries[0], "gone.invalid");
        assert_eq!(entries[1], "alive.invalid");
    }

    /// UI-107 / epics-base#863 (access-security half): under
    /// `asCheckClientIP` the parsed `hag` holds resolution *output*
    /// frozen at load time. `with_refreshed_hags` re-runs `hag_members`
    /// over the raw spellings — the periodic refresher's engine.
    #[test]
    fn with_refreshed_hags_recovers_a_stale_resolution() {
        let _guard = lock_as_check_client_ip();
        set_as_check_client_ip(true);
        let mut config = parse_acf("HAG(local) { localhost }\n").unwrap();
        assert_eq!(config.hag_raw["local"], vec!["localhost"]);

        // Simulate DNS moving after load: the stored quad no longer
        // matches what `localhost` resolves to.
        config.hag.insert("local".into(), vec!["192.0.2.1".into()]);

        let refreshed = config
            .with_refreshed_hags()
            .expect("a moved resolution must produce a refreshed config");
        set_as_check_client_ip(false);
        assert_eq!(refreshed.hag["local"], vec!["127.0.0.1"]);
        assert_eq!(
            refreshed.hag_raw["local"],
            vec!["localhost"],
            "raw spellings survive the refresh for the next round"
        );
    }

    /// An unchanged resolution yields `None` — the refresher
    /// republishes (re-notifying every connected client) only on real
    /// movement.
    #[test]
    fn with_refreshed_hags_is_none_when_resolution_is_unchanged() {
        let _guard = lock_as_check_client_ip();
        set_as_check_client_ip(true);
        let config = parse_acf("HAG(local) { localhost }\n").unwrap();
        let idempotent = config.with_refreshed_hags();
        set_as_check_client_ip(false);
        assert!(
            idempotent.is_none(),
            "a freshly parsed config re-resolves to itself"
        );
    }

    /// In default string mode the stored members are lowercased
    /// literals no DNS change can move — a refresh is always a no-op
    /// (`spawn_hag_refresh` gates on the flag, but the method must
    /// hold on its own for direct callers).
    #[test]
    fn with_refreshed_hags_is_none_in_name_mode() {
        let _guard = lock_as_check_client_ip();
        set_as_check_client_ip(false);
        let config = parse_acf("HAG(local) { LocalHost }\n").unwrap();
        assert_eq!(config.hag_raw["local"], vec!["LocalHost"]);
        assert_eq!(config.hag["local"], vec!["localhost"]);
        assert!(config.with_refreshed_hags().is_none());
    }

    #[test]
    fn test_check_access_default_rw() {
        let acf = "ASG(DEFAULT) { RULE(1, WRITE) RULE(1, READ) }";
        let config = parse_acf(acf).unwrap();
        assert_eq!(
            config.check_access("DEFAULT", "host1", "user1"),
            AccessLevel::ReadWrite
        );
    }

    #[test]
    fn test_check_access_read_only() {
        let acf = r#"
UAG(admins) { admin1 }
ASG(READONLY) {
    RULE(1, READ)
    RULE(1, WRITE) { UAG(admins) }
}
"#;
        let config = parse_acf(acf).unwrap();
        // admin1 gets RW
        assert_eq!(
            config.check_access("READONLY", "host1", "admin1"),
            AccessLevel::ReadWrite
        );
        // Other users get read only
        assert_eq!(
            config.check_access("READONLY", "host1", "regular"),
            AccessLevel::Read
        );
    }

    #[test]
    fn test_check_access_hag_uag_match() {
        let _guard = lock_as_check_client_ip();
        let acf = r#"
UAG(ops) { alice }
HAG(lab) { lab-pc1 }
ASG(CONTROLLED) {
    RULE(1, WRITE) { UAG(ops) HAG(lab) }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        // Alice on lab-pc1 gets RW
        assert_eq!(
            config.check_access("CONTROLLED", "lab-pc1", "alice"),
            AccessLevel::ReadWrite
        );
        // Alice on wrong host gets READ
        assert_eq!(
            config.check_access("CONTROLLED", "other-host", "alice"),
            AccessLevel::Read
        );
        // Wrong user on lab-pc1 gets READ
        assert_eq!(
            config.check_access("CONTROLLED", "lab-pc1", "bob"),
            AccessLevel::Read
        );
    }

    #[test]
    fn test_check_access_unknown_user() {
        let acf = r#"
ASG(DEFAULT) {
    RULE(1, WRITE)
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        // C `asComputePvt` parity: a RULE with an empty UAG list
        // applies to *every* client regardless of user/host — the
        // UAG check is skipped when `ellCount(&pasgrule->uagList)==0`.
        // So an unconditional `RULE(1, WRITE)` grants WRITE even to a
        // client with an empty/unknown user. (The old port returned
        // `Read` here via a `unknown_access` special-case that C does
        // not have.)
        assert_eq!(
            config.check_access("DEFAULT", "", ""),
            AccessLevel::ReadWrite
        );
    }

    /// epics-base #871 (7e18b8cff): `asDump*` quotes every UAG and HAG
    /// member through `asDumpQuoted` — `"` + `epicsStrPrintEscaped` + `"`
    /// — so a member that needed ACF quoting (`"role/op"`, an embedded
    /// `"`) survives the dump unambiguously instead of printing raw.
    #[test]
    fn dump_report_quotes_uag_and_hag_members() {
        let cfg =
            parse_acf("UAG(special) { someone, \"role/op\", \"a\\\"b\" }\nHAG(hosts) { HostA }\n")
                .unwrap();
        let dump = cfg.dump_report();
        assert!(dump.contains("\t\"someone\"\n"), "{dump}");
        assert!(dump.contains("\t\"role/op\"\n"), "{dump}");
        assert!(dump.contains("\t\"a\\\"b\"\n"), "{dump}");
        // HAG members are stored lowercased; quoted the same way.
        assert!(dump.contains("\t\"hosta\"\n"), "{dump}");
    }

    // ----- epics-base PR #563/#618: METHOD / AUTHORITY -----

    #[test]
    fn parse_acf_captures_method_and_authority() {
        let acf = r#"
ASG(SECURE) {
    RULE(1, WRITE) {
        METHOD("ca", "x509")
        AUTHORITY("ANL CA")
    }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        let asg = &config.asg["SECURE"];
        assert_eq!(asg.rules.len(), 2);
        assert_eq!(asg.rules[0].method, vec!["ca", "x509"]);
        assert_eq!(asg.rules[0].authority, vec!["ANL CA"]);
        assert!(
            asg.rules[1].method.is_empty(),
            "READ rule must not inherit METHOD list",
        );
        assert!(asg.rules[1].authority.is_empty());
    }

    #[test]
    fn tls_x509_acf_rule_grants_write_on_issuer_match() {
        // PR #641 end-to-end: an ACF rule that requires both
        // METHOD("x509") and AUTHORITY(<issuer>) must succeed only
        // when an mTLS peer presents a cert signed by that issuer.
        let cfg = parse_acf(
            r#"
ASG(TLS_ONLY) {
    RULE(1, WRITE) { METHOD("x509") AUTHORITY("CN=ops-ca, O=Lab") }
    RULE(1, READ)
}
"#,
        )
        .unwrap();
        // Plaintext (no method) → READ only.
        assert_eq!(
            cfg.check_access_method("TLS_ONLY", "h", "u", 0, "", ""),
            AccessLevel::Read
        );
        // mTLS, wrong issuer → READ only.
        assert_eq!(
            cfg.check_access_method("TLS_ONLY", "h", "u", 0, "x509", "CN=other-ca"),
            AccessLevel::Read
        );
        // mTLS, matching issuer → WRITE granted.
        assert_eq!(
            cfg.check_access_method("TLS_ONLY", "h", "u", 0, "x509", "CN=ops-ca, O=Lab"),
            AccessLevel::ReadWrite
        );
    }

    #[test]
    fn check_access_method_gates_on_method() {
        let acf = r#"
ASG(METHOD_GATED) {
    RULE(1, WRITE) {
        METHOD("x509")
    }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        // x509 method → WRITE matches.
        assert_eq!(
            config.check_access_method("METHOD_GATED", "h", "u", 0, "x509", ""),
            AccessLevel::ReadWrite
        );
        // ca method → only the unconstrained READ rule matches.
        assert_eq!(
            config.check_access_method("METHOD_GATED", "h", "u", 0, "ca", ""),
            AccessLevel::Read
        );
    }

    #[test]
    fn check_access_method_gates_on_authority() {
        let acf = r#"
ASG(AUTH_GATED) {
    RULE(1, WRITE) {
        AUTHORITY("Trusted Root")
    }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        assert_eq!(
            config.check_access_method("AUTH_GATED", "h", "u", 0, "x509", "Trusted Root"),
            AccessLevel::ReadWrite
        );
        assert_eq!(
            config.check_access_method("AUTH_GATED", "h", "u", 0, "x509", "Other CA"),
            AccessLevel::Read
        );
    }

    #[test]
    fn check_access_asl_legacy_path_matches_when_method_empty() {
        // Legacy ACF without METHOD/AUTHORITY clauses must continue
        // to match every method/authority — exactly what
        // `check_access_asl` forwards as ("", "").
        let acf = r#"
ASG(LEGACY) {
    RULE(1, WRITE)
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        assert_eq!(
            config.check_access_asl("LEGACY", "h", "u", 0),
            AccessLevel::ReadWrite
        );
    }

    #[test]
    fn check_access_method_match_is_case_insensitive() {
        let acf = r#"
ASG(MIXED_CASE) {
    RULE(1, WRITE) {
        METHOD("X509")
    }
}
"#;
        let config = parse_acf(acf).unwrap();
        assert_eq!(
            config.check_access_method("MIXED_CASE", "h", "u", 0, "x509", ""),
            AccessLevel::ReadWrite
        );
    }

    // ----- access security must fail CLOSED -----

    /// an ASG declared with no RULE statements denies every
    /// client. C `asComputePvt` starts `access = asNOACCESS` and only
    /// raises it on a matching RULE — an empty rule list never raises.
    #[test]
    fn empty_rule_asg_denies_access() {
        let config = parse_acf("ASG(LOCKED) { }").unwrap();
        assert_eq!(
            config.check_access("LOCKED", "host", "user"),
            AccessLevel::NoAccess,
            "ASG with no RULE must deny — C asComputePvt fails closed"
        );
    }

    /// a record whose ASG names a group not in the file resolves
    /// to the always-present (empty) `DEFAULT`, which denies access.
    #[test]
    fn unknown_asg_falls_back_to_empty_default_and_denies() {
        let config = parse_acf("UAG(ops) { alice }").unwrap();
        // `DEFAULT` is auto-synthesised by parse_acf (C asInitialize
        // always calls asAsgAdd("DEFAULT")) and has no rules.
        assert!(config.asg.contains_key("DEFAULT"));
        assert_eq!(
            config.check_access("TYPO", "host", "alice"),
            AccessLevel::NoAccess,
            "unknown ASG must resolve to empty DEFAULT ⇒ NoAccess"
        );
    }

    /// Corner case: even `DEFAULT` itself, when never declared with
    /// rules, denies — the auto-synthesised placeholder is empty.
    #[test]
    fn default_asg_without_rules_denies() {
        let config = parse_acf("UAG(ops) { alice }").unwrap();
        assert_eq!(
            config.check_access("DEFAULT", "host", "alice"),
            AccessLevel::NoAccess
        );
    }

    /// an empty ACF file, or one with only comments / only
    /// UAG/HAG blocks, yields a fail-closed config — every check
    /// denies, matching a C IOC whose only ASG is the empty DEFAULT.
    #[test]
    fn empty_acf_denies_all_access() {
        let _guard = lock_as_check_client_ip();
        for acf in ["", "# just a comment\n", "UAG(ops){alice}\nHAG(h){pc1}\n"] {
            let config = parse_acf(acf).unwrap();
            assert_eq!(
                config.check_access("DEFAULT", "host", "alice"),
                AccessLevel::NoAccess,
                "empty/rule-less ACF must deny (input was {acf:?})"
            );
            assert_eq!(
                config.check_access("ANY_GROUP", "host", "alice"),
                AccessLevel::NoAccess,
                "unknown ASG against empty ACF must deny (input was {acf:?})"
            );
        }
    }

    /// A config built by hand (bypassing `parse_acf`) with no
    /// `DEFAULT` and an unknown ASG must still fail closed.
    #[test]
    fn handbuilt_config_missing_default_denies() {
        let config = AccessSecurityConfig {
            uag: HashMap::new(),
            hag: HashMap::new(),
            hag_raw: HashMap::new(),
            asg: HashMap::new(),
            unknown_access: AccessLevel::Read,
        };
        assert_eq!(
            config.check_access("WHATEVER", "host", "user"),
            AccessLevel::NoAccess
        );
    }

    // ----- NONE keyword and unsupported keywords -----

    /// `RULE(0, NONE)` grants asNOACCESS — it must not be treated as a
    /// READ-granting rule. With only a NONE rule, access stays denied.
    #[test]
    fn rule_none_grants_no_access() {
        let config = parse_acf("ASG(N) { RULE(0, NONE) }").unwrap();
        assert_eq!(
            config.check_access("N", "host", "user"),
            AccessLevel::NoAccess
        );
    }

    /// A misspelled access keyword disables the rule (C warns and
    /// drops it) — it must not silently become a READ rule.
    #[test]
    fn rule_unsupported_access_keyword_is_inert() {
        let config = parse_acf("ASG(B) { RULE(0, WRIET) }").unwrap();
        assert_eq!(config.asg["B"].rules.len(), 1);
        assert!(config.asg["B"].rules[0].ignore, "bad keyword ⇒ inert rule");
        assert_eq!(
            config.check_access("B", "host", "user"),
            AccessLevel::NoAccess
        );
    }

    // ----- RULE level validation -----

    #[test]
    fn rule_negative_level_is_rejected() {
        let err = parse_acf("ASG(X) { RULE(-1, READ) }");
        assert!(err.is_err(), "negative RULE level must fail the parse");
    }

    #[test]
    fn rule_non_numeric_level_is_rejected() {
        let err = parse_acf("ASG(X) { RULE(abc, READ) }");
        assert!(err.is_err(), "non-numeric RULE level must fail the parse");
    }

    // ----- unknown top-level block tolerated -----

    #[test]
    fn unknown_top_level_block_is_skipped_not_fatal() {
        let acf = r#"
VENDOR(extension) { whatever }
ASG(DEFAULT) { RULE(1, READ) }
"#;
        let config = parse_acf(acf).expect("unknown top-level block must not abort the parse");
        assert_eq!(
            config.check_access("DEFAULT", "host", "user"),
            AccessLevel::Read,
            "the ASG after the unknown block must still parse"
        );
    }

    /// A well-formed unknown top-level block — keyword + balanced
    /// `(...)` head + balanced `{...}` body — must parse to `Ok` with a
    /// warning. Mirrors C `asLib.y` `generic_item`
    /// (`tokenSTRING generic_head generic_block`, asLib.y:93-97).
    #[test]
    fn unknown_well_formed_block_parses_ok_with_warning() {
        let acf = r#"
VENDOR(x) { FOO(1) }
ASG(DEFAULT) { RULE(1, READ) }
"#;
        let config = parse_acf(acf)
            .expect("a well-formed unknown top-level block must warn-and-continue, not fail");
        assert_eq!(
            config.check_access("DEFAULT", "host", "user"),
            AccessLevel::Read
        );
    }

    /// The `tokenSTRING generic_head` bare form (asLib.y:98-102): an
    /// unknown keyword followed only by a balanced `(...)` head, no
    /// `{...}` body, still parses.
    #[test]
    fn unknown_block_bare_head_parses_ok() {
        let acf = "VENDOR(x) ASG(DEFAULT) { RULE(1, READ) }";
        let config = parse_acf(acf).expect("bare unknown-block head must warn-and-continue");
        assert!(config.asg.contains_key("DEFAULT"));
    }

    /// Genuine garbage — a bare token where a top-level block keyword
    /// is expected, with unbalanced parens — must return `Err`. C's
    /// grammar has no `generic_item: tokenSTRING` alone; an unknown
    /// keyword *not* followed by `(` matches no production ⇒ `yyerror`
    /// ⇒ `asInitialize` fails. This is the `reload_rpc` regression.
    #[test]
    fn genuine_garbage_acf_is_rejected() {
        assert!(
            parse_acf("this is not valid ACF (((").is_err(),
            "unparseable ACF must fail, not silently skip to EOF"
        );
    }

    /// A file containing only stray block punctuation where a
    /// top-level keyword is expected (`(`, `)`, `{`, `}`, `,`) is
    /// genuine garbage — C's grammar has no production starting with
    /// bare punctuation at top level ⇒ `yyerror`. It must fail, not
    /// silently break to a successful empty config.
    #[test]
    fn stray_top_level_punctuation_is_rejected() {
        assert!(
            parse_acf("(((").is_err(),
            "a file of only '(((' must fail, not silently skip to EOF"
        );
        assert!(
            parse_acf("}").is_err(),
            "a file of only '}}' must fail, not silently skip to EOF"
        );
    }

    /// A genuinely empty file and a whitespace/comment-only file must
    /// still parse `Ok` — the stray-punctuation fix above must not
    /// touch the pre-existing empty-file divergence from C.
    #[test]
    fn empty_and_comment_only_acf_still_parses_ok() {
        assert!(parse_acf("").is_ok(), "empty file must parse Ok");
        assert!(
            parse_acf("   \n\t  \n").is_ok(),
            "whitespace-only file must parse Ok"
        );
        assert!(
            parse_acf("# just a comment\n# another\n").is_ok(),
            "comment-only file must parse Ok"
        );
    }

    /// An unknown top-level keyword followed by another bare word (no
    /// `(`) is a syntax error, not a skippable block.
    #[test]
    fn unknown_keyword_without_paren_head_is_rejected() {
        assert!(parse_acf("VENDOR something").is_err());
    }

    /// An unknown top-level keyword alone at EOF is a syntax error —
    /// C's `generic_head` is mandatory.
    #[test]
    fn unknown_keyword_at_eof_is_rejected() {
        assert!(parse_acf("VENDOR").is_err());
    }

    /// An unknown block with an unbalanced `(...)` head must fail
    /// rather than consume to EOF.
    #[test]
    fn unknown_block_unbalanced_paren_is_rejected() {
        assert!(parse_acf("VENDOR(((").is_err());
    }

    /// An unknown block with an unbalanced `{...}` body must fail.
    #[test]
    fn unknown_block_unbalanced_brace_is_rejected() {
        assert!(parse_acf("VENDOR(x) { unterminated").is_err());
    }

    // ----- HAG host matching is case-insensitive -----

    #[test]
    fn hag_host_match_is_case_insensitive() {
        let _guard = lock_as_check_client_ip();
        let acf = r#"
HAG(lab) { LabPC1.invalid }
ASG(C) {
    RULE(1, WRITE) { HAG(lab) }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        // Client reports a differently-cased hostname.
        assert_eq!(
            config.check_access("C", "labpc1.invalid", "user"),
            AccessLevel::ReadWrite,
            "lowercased HAG entry must match a mixed-case client host"
        );
        assert_eq!(
            config.check_access("C", "LABPC1.INVALID", "user"),
            AccessLevel::ReadWrite
        );
        // A genuinely different host still only gets READ.
        assert_eq!(
            config.check_access("C", "other.invalid", "user"),
            AccessLevel::Read
        );
    }

    // ----- TRAPWRITE / NOTRAPWRITE log option parses -----

    #[test]
    fn rule_trapwrite_log_option_parses() {
        let config =
            parse_acf("ASG(T) { RULE(1, WRITE, TRAPWRITE) RULE(1, READ, NOTRAPWRITE) }").unwrap();
        assert_eq!(config.asg["T"].rules.len(), 2);
        assert_eq!(config.asg["T"].rules[0].access, RuleAccess::Write);
        assert!(
            config.asg["T"].rules[0].trap,
            "TRAPWRITE must set the trap mask"
        );
        assert_eq!(config.asg["T"].rules[1].access, RuleAccess::Read);
        assert!(
            !config.asg["T"].rules[1].trap,
            "NOTRAPWRITE must clear the trap mask"
        );
    }

    #[test]
    fn rule_bad_log_option_is_rejected() {
        assert!(parse_acf("ASG(T) { RULE(1, WRITE, BOGUS) }").is_err());
    }

    #[test]
    fn rule_log_option_is_case_sensitive() {
        // C `asLib.y:274,278` matches TRAPWRITE/NOTRAPWRITE with
        // `strcmp`; a lowercase variant matches neither and hits
        // `yyerror`, failing the whole ACF load. Reject case variants
        // here too rather than silently accepting them.
        assert!(
            parse_acf("ASG(T) { RULE(1, WRITE, trapwrite) }").is_err(),
            "lowercase `trapwrite` is not a valid log option (C strcmp)"
        );
        assert!(
            parse_acf("ASG(T) { RULE(1, WRITE, notrapwrite) }").is_err(),
            "lowercase `notrapwrite` is not a valid log option (C strcmp)"
        );
    }

    #[test]
    fn rule_access_keyword_is_case_sensitive() {
        // C `asLib.y:259-264` matches NONE/READ/WRITE with `strcmp`
        // (case-SENSITIVE). A lowercase `write` is an unsupported
        // keyword that C drops (`yywarn`, no rule added), so it must
        // grant nothing — not build an active Write rule.
        let cfg = parse_acf("ASG(L) { RULE(1, write) }").unwrap();
        assert_eq!(
            cfg.check_access_method("L", "h", "u", 0, "", ""),
            AccessLevel::NoAccess,
            "lowercase `write` is an unsupported keyword (C strcmp); grants nothing"
        );
        // Canonical uppercase still grants write at the rule's ASL.
        let cfg = parse_acf("ASG(U) { RULE(1, WRITE) }").unwrap();
        assert_eq!(
            cfg.check_access_method("U", "h", "u", 0, "", ""),
            AccessLevel::ReadWrite
        );
    }

    // `check_access_method_trap` must return the trap mask of
    // the rule that resolved the access level — not a hard-coded
    // `true`. Mirrors C `asComputePvt`/`pasgclient->trapMask`
    // (`asLibRoutines.c:986`, `:1041-1042`, `:1048`).
    #[test]
    fn mr_r20_trap_mask_reflects_matched_rule() {
        // Three ASGs, one per trap-option shape, each granting WRITE
        // to the same `(host, user)`.
        let cfg = parse_acf(
            r#"
ASG(TRAPPED)   { RULE(0, WRITE, TRAPWRITE) }
ASG(UNTRAPPED) { RULE(0, WRITE, NOTRAPWRITE) }
ASG(PLAIN)     { RULE(0, WRITE) }
ASG(LOCKED)    { }
"#,
        )
        .unwrap();

        // TRAPWRITE rule → granted WRITE with trap == true.
        let (lvl, trap) = cfg.check_access_method_trap("TRAPPED", "h", "u", 0, "", "");
        assert_eq!(lvl, AccessLevel::ReadWrite);
        assert!(trap, "a TRAPWRITE rule must resolve rule_was_trap = true");

        // NOTRAPWRITE rule → granted WRITE but trap == false.
        let (lvl, trap) = cfg.check_access_method_trap("UNTRAPPED", "h", "u", 0, "", "");
        assert_eq!(lvl, AccessLevel::ReadWrite);
        assert!(
            !trap,
            "a NOTRAPWRITE rule must resolve rule_was_trap = false"
        );

        // Rule with no trap option → granted WRITE, trap == false.
        let (lvl, trap) = cfg.check_access_method_trap("PLAIN", "h", "u", 0, "", "");
        assert_eq!(lvl, AccessLevel::ReadWrite);
        assert!(
            !trap,
            "a rule with no trap option must resolve rule_was_trap = false"
        );

        // Denied (no matching rule) → trap == false, never true.
        let (lvl, trap) = cfg.check_access_method_trap("LOCKED", "h", "u", 0, "", "");
        assert_eq!(lvl, AccessLevel::NoAccess);
        assert!(
            !trap,
            "a denied resolution must carry rule_was_trap = false"
        );
    }

    // when several rules raise access, the trap mask must be
    // the option of the *last* rule that set the level — C
    // `asComputePvt` copies `trapMask` together with `access` on
    // every raise (`asLibRoutines.c:1041-1042`).
    #[test]
    fn mr_r20_trap_mask_follows_last_access_raising_rule() {
        // READ (no trap) then WRITE (TRAPWRITE): WRITE is the last
        // raise, so the trap mask is the WRITE rule's.
        let cfg = parse_acf("ASG(M) { RULE(0, READ) RULE(0, WRITE, TRAPWRITE) }").unwrap();
        let (lvl, trap) = cfg.check_access_method_trap("M", "h", "u", 0, "", "");
        assert_eq!(lvl, AccessLevel::ReadWrite);
        assert!(
            trap,
            "trap mask must follow the WRITE rule that raised access"
        );

        // READ (no trap) then WRITE (NOTRAPWRITE): same, trap false.
        let cfg = parse_acf("ASG(N) { RULE(0, READ) RULE(0, WRITE, NOTRAPWRITE) }").unwrap();
        let (lvl, trap) = cfg.check_access_method_trap("N", "h", "u", 0, "", "");
        assert_eq!(lvl, AccessLevel::ReadWrite);
        assert!(!trap, "NOTRAPWRITE on the access-raising rule must win");
    }

    // ----- CALC clause gates (or disables) the rule -----

    /// A CALC condition must never let a rule become unconditional.
    /// This crate cannot resolve INP* link values, so a CALC rule is
    /// disabled (fail closed) — it grants nothing.
    #[test]
    fn calc_rule_is_conditionally_active_and_fails_closed_without_resolver() {
        let config = parse_acf(r#"ASG(G) { INPA("ref") RULE(1, WRITE) { CALC("A=1") } }"#).unwrap();
        let rule = &config.asg["G"].rules[0];
        assert!(rule.calc.is_some(), "CALC clause must be parsed and stored");
        // a CALC rule is no longer hard-disabled — it is
        // conditionally active and gated at check time.
        assert!(
            !rule.ignore,
            "a CALC rule is conditionally active, not unconditionally ignored"
        );
        // The sync `check_access` path supplies no INP* resolver, so the
        // CALC rule still fails CLOSED (must not silently grant WRITE).
        assert_eq!(
            config.check_access("G", "host", "user"),
            AccessLevel::NoAccess,
            "CALC rule with no resolver must not grant WRITE"
        );
    }

    /// The watch set the ASG `INP*` re-evaluation trigger builds: one entry
    /// per distinct `(record, field)`, with the `VAL` default a bare record
    /// name carries, and no duplicate when two ASGs read the same link.
    #[test]
    fn inp_link_targets_are_deduplicated_across_groups() {
        let cfg = parse_acf(
            r#"
            ASG(A) { INPA("gate") INPB("gate.RVAL") RULE(1, WRITE) { CALC("A") } }
            ASG(B) { INPA("gate") INPB("other.SEVR") RULE(1, WRITE) { CALC("A") } }
            "#,
        )
        .expect("parse");
        assert_eq!(
            cfg.inp_link_targets(),
            vec![
                ("gate".to_string(), "RVAL".to_string()),
                ("gate".to_string(), "VAL".to_string()),
                ("other".to_string(), "SEVR".to_string()),
            ],
            "`gate` is read by both groups but is one subscription"
        );
    }

    /// with an `INP*` resolver installed, a CALC-gated rule
    /// grants when the expression is true, denies when false, denies on
    /// a bad input, and denies when no resolver is installed.
    #[epics_macros_rs::epics_test]
    async fn calc_gated_rule_evaluates_against_inp_resolver() {
        use std::sync::Arc;
        let cfg =
            parse_acf(r#"ASG(OPS) { INPA("permit.VAL") RULE(1, WRITE) { CALC("A=1") } }"#).unwrap();
        let cell = crate::server::access_security::new_acf_cell(Some(cfg));
        let asg_resolver: AsgAslResolver =
            Arc::new(|_name| Box::pin(async { ("OPS".to_string(), 0u8) }));

        let grant = AccessGate::required(cell.clone(), asg_resolver.clone()).with_inp_resolver(
            Arc::new(|link: String| Box::pin(async move { (link == "permit.VAL").then_some(1.0) })),
        );
        assert!(
            grant.check("x", "h", "u", "ca", "").await.allows_write(),
            "CALC A=1 with permit=1 grants WRITE"
        );

        let deny = AccessGate::required(cell.clone(), asg_resolver.clone()).with_inp_resolver(
            Arc::new(|link: String| Box::pin(async move { (link == "permit.VAL").then_some(0.0) })),
        );
        assert!(
            !deny.check("x", "h", "u", "ca", "").await.allows_write(),
            "CALC A=1 with permit=0 denies WRITE"
        );

        let bad = AccessGate::required(cell.clone(), asg_resolver.clone())
            .with_inp_resolver(Arc::new(|_link: String| Box::pin(async move { None })));
        assert!(
            !bad.check("x", "h", "u", "ca", "").await.allows_write(),
            "a bad/disconnected INP denies the CALC-gated rule"
        );

        let none = AccessGate::required(cell, asg_resolver);
        assert!(
            !none.check("x", "h", "u", "ca", "").await.allows_write(),
            "no INP resolver installed → CALC rule fails closed"
        );
    }

    /// a `role/<name>` UAG member matches a client that holds
    /// that role; a client without it does not match.
    #[test]
    fn uag_role_member_matches_client_role() {
        let cfg =
            parse_acf(r#"UAG(special) { "role/op" } ASG(G) { RULE(1, WRITE) { UAG(special) } }"#)
                .unwrap();
        let (lvl, _) =
            cfg.compute_for_name("G", "h", "acct", &["op".to_string()], 0, "ca", "", None);
        assert_eq!(
            lvl,
            AccessLevel::ReadWrite,
            "role/op member matches a client holding role 'op'"
        );
        let (lvl_none, _) = cfg.compute_for_name("G", "h", "acct", &[], 0, "ca", "", None);
        assert_eq!(
            lvl_none,
            AccessLevel::NoAccess,
            "a client without role 'op' must not match role/op"
        );
    }

    #[test]
    fn calc_rule_with_bad_expression_is_rejected() {
        assert!(
            parse_acf(r#"ASG(G) { RULE(1, WRITE) { CALC("A=") } }"#).is_err(),
            "syntactically broken CALC must fail the parse"
        );
    }

    // ----- INP(A..U) link declarations -----

    #[test]
    fn asg_inp_links_are_parsed() {
        let acf = r#"
ASG(G) {
    INPA("rec1.VAL")
    INPC("rec3.VAL")
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        let inp = &config.asg["G"].inp;
        assert_eq!(inp.len(), 2);
        assert_eq!(inp[0].index, 0);
        assert_eq!(inp[0].link, "rec1.VAL");
        assert_eq!(inp[1].index, 2);
        assert_eq!(inp[1].link, "rec3.VAL");
    }

    #[test]
    fn asg_inp_bad_selector_is_rejected() {
        // INPZ is out of the A..U range.
        assert!(parse_acf(r#"ASG(G) { INPZ("x") }"#).is_err());
    }

    #[test]
    fn asg_inp_selector_is_case_sensitive() {
        // C `asLib_lex.l:21,47` lexes the selector as `INP[A-U]`
        // (uppercase range only), so a lowercase `INPa` is not a valid
        // INP token — a syntax error, not selector index 0.
        assert!(
            parse_acf(r#"ASG(G) { INPa("x") }"#).is_err(),
            "lowercase INP selector must be rejected (C flex [A-U])"
        );
    }

    // ----- L-4: parenthesised name robustness -----

    #[test]
    fn paren_name_rejects_embedded_whitespace() {
        // `UAG(my group)` must NOT silently become `mygroup`.
        assert!(parse_acf("UAG(my group) { x }").is_err());
    }

    #[test]
    fn paren_name_rejects_unterminated() {
        assert!(parse_acf("UAG(unterminated").is_err());
    }

    #[test]
    fn paren_name_accepts_quoted_form() {
        let config = parse_acf(r#"UAG("my group") { x }"#).unwrap();
        assert!(config.uag.contains_key("my group"));
    }

    /// ASL gate still works: a low-level WRITE rule does not apply to
    /// a high-ASL record. C `RULE(N,…)` applies only when ASL ≤ N.
    #[test]
    fn asl_gate_still_honoured_after_fail_closed_rewrite() {
        let config = parse_acf("ASG(A) { RULE(0, READ) RULE(1, WRITE) }").unwrap();
        // ASL-0 record: READ rule applies, WRITE rule applies.
        assert_eq!(
            config.check_access_method("A", "h", "u", 0, "", ""),
            AccessLevel::ReadWrite
        );
        // ASL-2 record: both rules require ASL ≤ their level, so
        // neither applies ⇒ denied.
        assert_eq!(
            config.check_access_method("A", "h", "u", 2, "", ""),
            AccessLevel::NoAccess
        );
    }

    /// Collect (op, owned-status) pairs whose `pv_name` matches `pv`,
    /// so a guard test is not polluted by trap dispatches from other
    /// tests sharing the process-global listener registry.
    fn trap_capture(
        pv: &'static str,
    ) -> (
        std::sync::Arc<std::sync::Mutex<Vec<(TrapWriteOp, Option<String>)>>>,
        TrapWriteListenerHandle,
    ) {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = events.clone();
        let handle = register_trap_write_listener(std::sync::Arc::new(move |msg| {
            if msg.pv_name == pv {
                sink.lock()
                    .unwrap()
                    .push((msg.op, msg.status.map(str::to_owned)));
            }
        }));
        (events, handle)
    }

    fn trap_fields(pv: &'static str) -> TrapWriteFields {
        TrapWriteFields {
            pv_name: pv.to_string(),
            user: "u".to_string(),
            host: "h".to_string(),
            peer: "h:5064".to_string(),
            value_str: "42".to_string(),
            dbr_type: 5,
            no_elements: 1,
            event_id: next_trap_write_event_id(),
            rule_was_trap: true,
            cancel_status: "superseded".to_string(),
        }
    }

    /// `complete` fires exactly one AfterWrite with the real status and
    /// disarms Drop, so the bracket is Before+After("ok") and not a
    /// second AfterWrite on scope exit. Owner-path of the invariant.
    #[test]
    fn trap_write_guard_complete_fires_one_after_and_disarms_drop() {
        let (events, _handle) = trap_capture("guard:complete");
        {
            let mut guard = TrapWriteGuard::begin(trap_fields("guard:complete"));
            guard.complete("ok");
        } // guard dropped here — must NOT emit a second AfterWrite
        let got = events.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                (TrapWriteOp::BeforeWrite, None),
                (TrapWriteOp::AfterWrite, Some("ok".to_string())),
            ]
        );
    }

    /// A guard dropped without `complete` (the cancel / supersede /
    /// teardown path) still fires its AfterWrite, carrying
    /// `cancel_status`. Bypass-path of the invariant: this is the case
    /// the pre-guard explicit-dispatch emitters skipped, leaving an
    /// unbalanced BeforeWrite.
    #[test]
    fn trap_write_guard_drop_without_complete_fires_cancel_after() {
        let (events, _handle) = trap_capture("guard:cancel");
        {
            let _guard = TrapWriteGuard::begin(trap_fields("guard:cancel"));
            // no complete() — simulate an aborted/superseded put
        }
        let got = events.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                (TrapWriteOp::BeforeWrite, None),
                (TrapWriteOp::AfterWrite, Some("superseded".to_string())),
            ]
        );
    }
}
