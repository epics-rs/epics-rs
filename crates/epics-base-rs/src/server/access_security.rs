use std::collections::HashMap;

use epics_libcom_rs::cfmt::CFloat;

use crate::error::{CaError, CaResult};
use crate::server::pv::SubscribeRefused;

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
    pv_name: crate::types::RecordName,
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
    pub fn pv_name(&self) -> &crate::types::RecordName {
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
    /// C `asAddClient` computes a client's access once per channel and
    /// every operation is a bit test; the Rust op layer called
    /// [`Self::check_with_roles`] — resolver + full rule walk — on every
    /// EXEC. The walk result is deterministic in (policy snapshot,
    /// pv name, credential identity, published [`AsgState`]), so checks
    /// are cached here, shared across gate clones.
    /// An entry is valid only while all three of its stamps hold: the
    /// `acl_version` generation, the ASG-field-change generation
    /// ([`asg_change_generation`] — the resolver reads `record.ASG`, and the
    /// `INP*` watcher bumps it after publishing a state),
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
    pv_name: crate::types::RecordName,
    host: String,
    user: Vec<u8>,
    method: String,
    authority: String,
    roles: Vec<crate::types::PvString>,
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
    dyn Fn(
            crate::types::RecordName,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = (String, u8)> + Send>>
        + Send
        + Sync,
>;

/// The shared Access Security policy cell — one per server, cloned into every
/// [`AccessGate`] built from it.
///
/// Lock-free by construction: a reader takes an `Arc` snapshot of the policy
/// and an operator reload publishes a whole new one. This is the ACF cell
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
pub struct AcfCell(std::sync::Arc<AcfShared>);

struct AcfShared {
    policy: arc_swap::ArcSwapOption<AccessSecurityConfig>,
    /// What the `INP*` watcher has attached, for [`AcfCell::wait_served`].
    served: parking_lot::Mutex<Served>,
    served_cv: parking_lot::Condvar,
    /// [`AcfCell::install_async`]'s wake, fired beside `served_cv`.
    served_notify: tokio::sync::Notify,
}

#[derive(Default)]
struct Served {
    /// A watcher is running for this cell.
    watching: bool,
    /// [`load_ident`] of the load whose inputs its last attach pass added.
    load: usize,
}

/// Which load of the file `policy` is, 0 for none. A HAG refresh republishes
/// the same load under a new policy `Arc`; only `asInit` makes a new one.
fn load_ident(policy: Option<&std::sync::Arc<AccessSecurityConfig>>) -> usize {
    policy.map_or(0, |c| std::sync::Arc::as_ptr(&c.load.0) as usize)
}

impl AcfCell {
    /// Snapshot the current policy (lock-free guard).
    pub fn load(&self) -> arc_swap::Guard<Option<std::sync::Arc<AccessSecurityConfig>>> {
        self.0.policy.load()
    }

    /// Snapshot the current policy as an owned `Option<Arc<..>>`.
    pub fn load_full(&self) -> Option<std::sync::Arc<AccessSecurityConfig>> {
        self.0.policy.load_full()
    }

    /// Publish a new policy (or `None` to clear it) and fire the
    /// process-wide access-policy change notification so live
    /// connections re-evaluate their rights and derived caches drop
    /// their entries.
    pub fn store(&self, value: Option<std::sync::Arc<AccessSecurityConfig>>) {
        self.0.policy.store(value);
        notify_asg_field_changed();
    }

    /// Publish `derived`, computed from `from`, only while `from` is still the
    /// published policy: a reload that landed meanwhile is newer than anything
    /// derived from the policy it replaced, so the derived one is dropped.
    /// True if it was published.
    fn replace_derived(
        &self,
        from: &std::sync::Arc<AccessSecurityConfig>,
        derived: AccessSecurityConfig,
    ) -> bool {
        let prev = self
            .0
            .policy
            .compare_and_swap(from, Some(std::sync::Arc::new(derived)));
        let replaced = prev
            .as_ref()
            .is_some_and(|p| std::sync::Arc::ptr_eq(p, from));
        if replaced {
            notify_asg_field_changed();
        }
        replaced
    }

    /// Block until the `INP*` watcher has added the current policy's inputs,
    /// each one present taking its first reading — C `asCaStart` waiting on
    /// `asCaTaskWait`. That wait puts an `asInit`'s initial events before
    /// `iocInit`'s PINI posts, so when two inputs share a letter the record
    /// processed last decides. Returns at once for a cell no watcher serves.
    pub fn wait_served(&self) {
        let mut served = self.0.served.lock();
        while !self.is_served(&served) {
            self.0.served_cv.wait(&mut served);
        }
    }

    fn is_served(&self, served: &Served) -> bool {
        !served.watching || served.load == load_ident(self.load().as_ref())
    }

    /// C `asInit` for a parsed file: publish it as a new load, then
    /// [`Self::wait_served`]. Every path that loads a file comes through here
    /// or [`Self::install_async`], so none returns with the new inputs still
    /// unread. Blocks the calling thread, so it is for a thread of its own
    /// (iocsh's): the watcher it waits for runs on cbMedium, and a band worker
    /// parked here can starve it. Async callers use [`Self::install_async`].
    pub fn install(&self, config: AccessSecurityConfig) {
        self.store(Some(std::sync::Arc::new(config)));
        self.wait_served();
    }

    /// [`Self::install`] that awaits the first readings instead of parking a
    /// thread, so it is safe on any executor, the watcher's band included.
    pub async fn install_async(&self, config: AccessSecurityConfig) {
        self.store(Some(std::sync::Arc::new(config)));
        self.wait_served_async().await;
    }

    /// [`Self::wait_served`] for async callers: parks no thread.
    pub async fn wait_served_async(&self) {
        loop {
            let notified = self.0.served_notify.notified();
            let mut notified = std::pin::pin!(notified);
            // Registered before the check, so a mark between them still wakes.
            notified.as_mut().enable();
            if self.is_served(&self.0.served.lock()) {
                return;
            }
            notified.await;
        }
    }

    fn mark_served(&self, watching: bool, load: usize) {
        *self.0.served.lock() = Served { watching, load };
        self.0.served_cv.notify_all();
        self.0.served_notify.notify_waiters();
    }
}

/// Build a shared [`AcfCell`] holding `initial`. The single construction
/// point, so no caller has to name `arc_swap` or get the `Arc` nesting right.
pub fn new_acf_cell(initial: Option<AccessSecurityConfig>) -> AcfCell {
    AcfCell(std::sync::Arc::new(AcfShared {
        policy: arc_swap::ArcSwapOption::new(initial.map(std::sync::Arc::new)),
        served: Default::default(),
        served_cv: Default::default(),
        served_notify: tokio::sync::Notify::new(),
    }))
}

/// Build a shared [`AcfCell`] that serves `db`, with its ASG `INP*` watcher
/// already running (C `asCa.c`, see `spawn_asg_inp_watcher`).
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

/// Start the housekeeping an [`AcfCell`] that gates `db` must have: the ASG
/// `INP*` watcher (C `asCaStart`, `asCa.c:180-205`) and the HAG DNS
/// refresher.
///
/// Split from [`new_acf_cell_watching`] for one reason. Both tasks run on the
/// process-global callback pool, and building that pool is `iocInit`'s job in
/// C — `callbackInit` sits inside `iocBuild_1` (`iocInit.c:152`) — because the
/// pool reads `callbackSetQueueSize` / `callbackParallelThreads` once, when it
/// is constructed, and both commands refuse afterwards (`callback.c:106-109`,
/// `:162-165`). Anything that spawns on the pool before the startup script
/// runs therefore silently disarms those two commands for the whole script.
/// So an IOC with a lifecycle creates its cell with [`new_acf_cell`] before
/// the script and calls this from the build.
///
/// That leaves the cell unwatched for the length of the script, which cannot
/// re-open the cached-grant hole [`new_acf_cell_watching`] exists to close:
/// nothing is serving the database yet, so no channel is holding an access
/// level to go stale. A caller with no such lifecycle — a bare
/// `CaServerBuilder`/`PvaServerBuilder`, already past `iocInit` by
/// construction — keeps using [`new_acf_cell_watching`].
///
/// Must be called from within the runtime: it spawns.
pub fn start_acf_watchers(
    db: &std::sync::Arc<crate::server::database::PvDatabase>,
    cell: &AcfCell,
) {
    spawn_asg_inp_watcher(db, cell);
    spawn_hag_refresh(cell);
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
/// Resolution runs on [`AS_OFFLOAD`], not on the band: a wedged resolver
/// holds one of its workers and delays this refresher, while cbMedium, which
/// the INP watcher shares, keeps running. The task holds only a `Weak` to
/// the cell and ends when the owning IOC drops it.
fn spawn_hag_refresh(cell: &AcfCell) {
    let weak = std::sync::Arc::downgrade(&cell.0);
    // Middle band: an IOC-wide HAG re-resolution owns no record and so has no
    // PRIO to read. It is one of the two entries that make `callbackQueueShow`
    // report a cbMedium high-water of 2 where C reports 0 — C has no HAG
    // refresher at all — and that difference is intended, not a leak.
    crate::runtime::task::spawn_background(
        crate::runtime::task::CallbackPriority::Medium,
        async move {
            loop {
                crate::runtime::task::sleep_background(HAG_DNS_REFRESH).await;
                if !refresh_hags_once(&weak, AccessSecurityConfig::with_refreshed_hags).await {
                    break;
                }
            }
        },
    );
}

/// One [`spawn_hag_refresh`] tick, `resolve` run on [`AS_OFFLOAD`]. False once
/// the cell is gone.
async fn refresh_hags_once<F>(weak: &std::sync::Weak<AcfShared>, resolve: F) -> bool
where
    F: FnOnce(&AccessSecurityConfig) -> Option<AccessSecurityConfig> + Send + 'static,
{
    let Some(inner) = weak.upgrade() else {
        return false;
    };
    if !as_check_client_ip() {
        return true;
    }
    let Some(config) = inner.policy.load_full() else {
        return true;
    };
    drop(inner);
    let from = config.clone();
    let refreshed = match AS_OFFLOAD.run(move || resolve(&config)) {
        Ok(job) => job.await,
        Err(e) => {
            tracing::debug!(
                target: "epics_base_rs::access_security",
                error = %e,
                "HAG DNS refresh: no offload thread; skipping this tick"
            );
            return true;
        }
    };
    let Some(refreshed) = refreshed else {
        return true;
    };
    let Some(inner) = weak.upgrade() else {
        return false;
    };
    if AcfCell(inner).replace_derived(&from, refreshed) {
        tracing::debug!(
            target: "epics_base_rs::access_security",
            "HAG DNS refresh: re-resolved members changed; republishing policy"
        );
    }
    true
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
/// This is that monitor, in-process: one `VALUE | ALARM` subscription per
/// distinct link target, each post delivered into the [`AsgState`] of every
/// input naming it, then [`notify_asg_field_changed`]. That is the signal
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
    cell.mark_served(true, 0);
    // Clears `watching` however the task ends, so `wait_served` cannot hang
    // on a watcher that is gone.
    struct Unwatch(std::sync::Weak<AcfShared>);
    impl Drop for Unwatch {
        fn drop(&mut self) {
            if let Some(inner) = self.0.upgrade() {
                AcfCell(inner).mark_served(false, 0);
            }
        }
    }
    let unwatch = Unwatch(weak_cell.clone());
    // Middle band: the watcher spans every ASG INP link in the IOC, so no one
    // record's PRIO applies. It is the other entry behind the cbMedium
    // high-water of 2: C does this work on `asCaTask`, a CA client thread that
    // is not in the callback pool, so its own `callbackQueueShow` reports 0.
    crate::runtime::task::spawn_background(
        crate::runtime::task::CallbackPriority::Medium,
        async move {
            let _unwatch = unwatch;
            // C `asCaTask` registers with the watchdog as its first act
            // (`asCa.c:171`) and removes on the way out (`:232`) — but the
            // thread itself only exists between `asCaStart` and `asCaStop`,
            // which nothing but `asInitCommon` calls (`asDbLib.c:147`, `:136`).
            // A C IOC with no access-security file therefore lists no
            // `asCaTask` at all. Here the watcher is welded to the cell
            // instead, so it is the entry that carries C's condition: held
            // exactly while there is a policy to serve, and released the
            // moment there is not. Unbounded, as C's is: the loop parks on its
            // ASG INP readers, and access inputs that never change are not a
            // fault.
            let mut watched: Option<crate::runtime::taskwd::TaskwdEntry> = None;
            let mut watch = InpWatch::default();
            // The policy whose groups take the deliveries. `watch` is rebuilt
            // only for a new load: a republish of the same one (a HAG refresh,
            // or a notification this task raises itself) keeps the
            // subscriptions and the state C's events built, as C has no
            // refresher to replay them.
            let mut built_from: Option<std::sync::Arc<AccessSecurityConfig>> = None;

            loop {
                let (Some(inner), Some(db)) = (weak_cell.upgrade(), weak_db.upgrade()) else {
                    break;
                };
                let cell = AcfCell(inner);
                let config = cell.load_full();
                match (config.is_some(), watched.is_some()) {
                    (true, false) => {
                        watched = Some(crate::runtime::taskwd::taskwd_insert(
                            "asCaTask",
                            crate::runtime::taskwd::CheckIn::Unbounded,
                            None,
                        ))
                    }
                    (false, true) => watched = None,
                    _ => {}
                }
                if load_ident(config.as_ref()) != load_ident(built_from.as_ref()) {
                    watch = config.as_deref().map(InpWatch::of).unwrap_or_default();
                }
                built_from = config;
                let attached = built_from
                    .as_ref()
                    .is_some_and(|cfg| watch.attach(&db, cfg));
                drop(db);
                // C's `asComputeAllAsg` after the adds and before it signals
                // `asCaTaskWait` (asCa.c:204-207): a waiter that wakes must
                // not see the attach's change signal still to come.
                if attached {
                    notify_asg_field_changed();
                }
                cell.mark_served(true, load_ident(built_from.as_ref()));
                drop(cell);

                let wake = tokio::select! {
                    r = acf_rx.recv() => match r {
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => Wake::Stop,
                        _ => Wake::Rebuild,
                    },
                    posted = drain_any_asg_inp(&watch.user) => Wake::Values(posted),
                    () = crate::runtime::task::sleep_background(ASG_INP_RETRY) => Wake::Rebuild,
                };
                match wake {
                    Wake::Stop => break,
                    Wake::Rebuild => {}
                    Wake::Values(posted) => {
                        let (Some(cfg), Some(db)) = (&built_from, weak_db.upgrade()) else {
                            continue;
                        };
                        watch.deliver(&db, cfg, posted);
                        notify_asg_field_changed();
                    }
                }
            }
        },
    );
}

/// Why the watcher woke.
enum Wake {
    /// Watched links posted, in post order.
    Values(Vec<crate::server::event_queue::CircuitEvent>),
    /// Re-derive the watch set (policy may have been replaced, or a link's
    /// record may have loaded since the last attempt).
    Rebuild,
    Stop,
}

/// One subscription the watcher holds — C's channel per `ASGINP`, shared by
/// every input naming the same `(record, field)`.
struct InpTarget {
    record: String,
    field: String,
    /// `(ASG, INP index)` of each input reading this target.
    inputs: Vec<(Vec<u8>, u8)>,
    /// `None` until attached, and again once the record goes away.
    reader: Option<crate::server::event_queue::EventReader>,
    /// Whether a full subscriber cap has been reported: C's exception arrives
    /// once per `ca_add_event`.
    reported: bool,
}

/// Everything the watcher holds for one policy: C's CA context and its
/// channels.
#[derive(Default)]
struct InpWatch {
    /// One event queue for every subscription, read in post order as C's
    /// `asCaTask` context receives its events: when two inputs share a letter
    /// the last event decides, so the record processing order does.
    user: crate::server::event_queue::EventUser,
    /// Distinct link targets; a subscription's `sid` is its index here.
    targets: Vec<InpTarget>,
    /// `(ASG, INP index, target)` in `asCaTask`'s add order (`asCa.c:178-200`):
    /// ASGs by name, as `asAsgAdd` keeps them, inputs as the file lists them.
    order: Vec<(Vec<u8>, u8, usize)>,
}

impl InpWatch {
    fn of(config: &AccessSecurityConfig) -> Self {
        let mut watch = Self::default();
        let mut names: Vec<_> = config.asg.keys().collect();
        names.sort();
        for name in names {
            for inp in &config.asg[name].inp {
                let Some(link) = inp.record_link() else {
                    continue;
                };
                let (record, field) = inp_link_target(link);
                let found = watch
                    .targets
                    .iter()
                    .position(|t| t.record == record && t.field == field);
                let ix = found.unwrap_or_else(|| {
                    watch.targets.push(InpTarget {
                        record: record.to_string(),
                        field: field.to_string(),
                        inputs: Vec::new(),
                        reader: None,
                        reported: false,
                    });
                    watch.targets.len() - 1
                });
                watch.targets[ix].inputs.push((name.clone(), inp.index));
                watch.order.push((name.clone(), inp.index, ix));
            }
        }
        watch
    }

    /// Add every unattached input in `asCaTask` order, each taking the
    /// reading its `ca_add_event` delivers first. True when any attached.
    fn attach(
        &mut self,
        db: &crate::server::database::PvDatabase,
        config: &AccessSecurityConfig,
    ) -> bool {
        let mut fresh = vec![false; self.targets.len()];
        for (asg, index, ix) in &self.order {
            let target = &mut self.targets[*ix];
            let reading = if target.reader.is_none() {
                match attach_asg_inp(db, &self.user, *ix, &target.record, &target.field) {
                    Ok((reader, initial)) => {
                        target.reader = Some(reader);
                        fresh[*ix] = true;
                        initial
                    }
                    Err(SubscribeRefused::CapReached) if !target.reported => {
                        target.reported = true;
                        report_asg_inp_refused(db, Some(config), &target.record, &target.field);
                        continue;
                    }
                    Err(_) => continue,
                }
            } else if fresh[*ix] {
                // C's next channel to the same PV takes its own first event.
                db.get_record(&target.record)
                    .and_then(|rec| inp_reading(&rec.read(), &target.field))
            } else {
                continue;
            };
            if let (Some(reading), Some(group)) = (reading, config.asg.get(asg)) {
                group.deliver([(*index, InpEvent::Reading(reading))]);
            }
        }
        fresh.contains(&true)
    }

    /// Events off the queue, in order, each into every input its target
    /// feeds.
    fn deliver(
        &mut self,
        db: &crate::server::database::PvDatabase,
        config: &AccessSecurityConfig,
        posted: Vec<crate::server::event_queue::CircuitEvent>,
    ) {
        for post in posted {
            self.deliver_one(db, config, post);
        }
    }

    fn deliver_one(
        &mut self,
        db: &crate::server::database::PvDatabase,
        config: &AccessSecurityConfig,
        post: crate::server::event_queue::CircuitEvent,
    ) {
        use crate::server::event_queue::CircuitEvent;
        let (ix, event) = match post {
            CircuitEvent::Event { sid, event, .. } => {
                let ix = sid as usize;
                let Some(target) = self.targets.get(ix) else {
                    return;
                };
                let value = event
                    .value()
                    .as_value()
                    .and_then(|v| v.to_f64())
                    .or_else(|| read_inp_value(db, &target.record, &target.field));
                let Some(value) = value else { return };
                let severity = crate::server::record::AlarmSeverity::from_u16(event.sevr());
                (ix, InpEvent::Reading(InpReading { value, severity }))
            }
            CircuitEvent::Ended(id) => {
                let Some(ix) = self
                    .targets
                    .iter()
                    .position(|t| t.reader.as_ref().is_some_and(|r| r.id() == id))
                else {
                    return;
                };
                self.targets[ix].reader = None;
                (ix, InpEvent::Disconnected)
            }
        };
        for (asg, index) in &self.targets[ix].inputs {
            if let Some(group) = config.asg.get(asg) {
                group.deliver([(*index, event)]);
            }
        }
    }
}

/// Subscribe one `INP*` target to its record's value and alarm events, the
/// default mask of C's `ca_add_event(DBR_STS_DOUBLE, ...)` (`asCa.c:193`),
/// and take the reading that call delivers first. Both happen under one
/// record lock, so no post falls between them.
/// Refused = not attached, retry later; the record is not in the database yet,
/// or its subscriber cap is full (which frees again as clients disconnect).
fn attach_asg_inp(
    db: &crate::server::database::PvDatabase,
    user: &crate::server::event_queue::EventUser,
    sid: usize,
    record: &str,
    field: &str,
) -> Result<(crate::server::event_queue::EventReader, Option<InpReading>), SubscribeRefused> {
    let rec = db.get_record(record).ok_or(SubscribeRefused::NoRecord)?;
    let mut inst = rec.write();
    let reader = inst.add_subscriber_on(
        user,
        field,
        sid as u32,
        crate::types::DbFieldType::Double,
        (crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::ALARM).bits(),
        crate::server::event_queue::no_context(),
    )?;
    Ok((reader, inp_reading(&inst, field)))
}

/// asCa.c:79's `exceptionCallback` line for an `ECA_ALLOCMEM` on
/// `ca_add_event`, with rsrv's `send_err` context (camessage.c:1872) and the
/// rights rsrv would grant asCa's own circuit.
fn report_asg_inp_refused(
    db: &crate::server::database::PvDatabase,
    config: Option<&AccessSecurityConfig>,
    record: &str,
    field: &str,
) {
    let Some(rec) = db.get_record(record) else {
        return;
    };
    let (native, (asg, asl)) = {
        let rec = rec.read();
        (
            rec.resolve_field(field),
            (rec.common.text.asg.clone(), rec.common.asl),
        )
    };
    let level = config.map_or(AccessLevel::ReadWrite, |c| {
        // asCa's libca circuit names itself as `cac.cpp:176-178` does.
        let user = crate::runtime::env::user_name().unwrap_or_default();
        c.check_access_asl(&asg, crate::runtime::env::ca_local_host_name(), &user, asl)
    });
    crate::runtime::log::errlog_vprintf(format_args!(
        "dbCa:exceptionCallback stat \"Unable to allocate additional dynamic memory\" \
         channel \"{record}.{field}\" context \"subscription install into record {record} failed\"\n \
         nativeType {} requestType DBR_STS_DOUBLE nativeCount {} requestCount 0 {} {}\n",
        native
            .as_ref()
            .map_or("DBR_invalid", |v| crate::types::dbr_type_to_text(
                v.dbr_type() as u16
            )),
        native.as_ref().map_or(0, |v| v.count()),
        if level == AccessLevel::NoAccess {
            "noReadAccess"
        } else {
            "readAccess"
        },
        if level == AccessLevel::ReadWrite {
            "writeAccess"
        } else {
            "noWriteAccess"
        },
    ));
}

/// Resolve once any watched link has posted, with every queued post in post
/// order, so one wake covers a burst.
async fn drain_any_asg_inp(
    user: &crate::server::event_queue::EventUser,
) -> Vec<crate::server::event_queue::CircuitEvent> {
    std::future::poll_fn(|cx| {
        let mut posted = Vec::new();
        while let std::task::Poll::Ready(event) = user.poll_next(cx) {
            posted.push(event);
        }
        if posted.is_empty() {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(posted)
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
            check_cache: std::sync::Arc::new(parking_lot::RwLock::new(HashMap::new())),
        }
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
        pv_name: impl Into<crate::types::RecordName>,
        host: &str,
        user: impl AsRef<[u8]>,
        method: &str,
        authority: &str,
    ) -> AccessChecked {
        self.check_with_roles(pv_name, host, user, &[], method, authority)
            .await
    }

    /// Like [`Self::check`] but with the client's
    /// `roles` (QSRV local-group-derived credentials) so a `role/<name>`
    /// UAG member can match.
    pub async fn check_with_roles(
        &self,
        pv_name: impl Into<crate::types::RecordName>,
        host: &str,
        user: impl AsRef<[u8]>,
        roles: &[crate::types::PvString],
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
                        // See the [`Self::check_cache`] field doc: snapshot
                        // every stamp BEFORE the compute so a change racing
                        // it invalidates the entry instead of being lost.
                        let acl_version = self.acl_version();
                        let asg_generation = asg_change_generation();
                        let cfg_ident = std::sync::Arc::as_ptr(&cfg) as usize;
                        let key = CheckKey {
                            pv_name: pv_name.clone(),
                            host: host.to_string(),
                            user: user.as_ref().to_vec(),
                            method: method.to_string(),
                            authority: authority.to_string(),
                            roles: roles.to_vec(),
                        };
                        if let Some(hit) = self.check_cache.read().get(&key)
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
                        let (level, rule_was_trap) = cfg.compute_for_name(
                            &asg,
                            host,
                            user.as_ref(),
                            roles,
                            asl,
                            method,
                            authority,
                        );
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
        assert_eq!(&checked.pv_name()[..], b"any:pv");
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
ASG(DEFAULT)
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
    /// C `pasgrule->level`, an `int`.
    pub level: i32,
    /// Three-way access this rule grants when it matches. C
    /// `asLib.y:259-267` distinguishes `NONE`/`READ`/`WRITE`.
    pub access: RuleAccess,
    pub uag: Vec<Vec<u8>>,
    pub hag: Vec<Vec<u8>>,
    /// Authentication method scope (epics-base PR #563). When set,
    /// the rule only applies when the requesting client authenticated
    /// via one of the listed methods. Common values: `"anonymous"`,
    /// `"ca"`, `"x509"`, `"cap-token"`. Empty vector means "any method".
    pub method: Vec<Vec<u8>>,
    /// Cert authority / issuer scope (epics-base PR #563 + #618).
    /// When set, the rule only applies when the client's authenticator
    /// was vouched by one of the listed authorities — e.g. an
    /// X.509 issuer DN, or the cap-token issuer ID. Empty means "any
    /// authority".
    pub authority: Vec<Vec<u8>>,
    /// Write-trap mask (epics-base `asLib.y:272-283` `rule_log_option`,
    /// `AS_TRAP_WRITE`). `true` when the RULE header carried the
    /// `TRAPWRITE` option, `false` for `NOTRAPWRITE` or no option.
    /// The mask is what arms the put-log bracket: a matching rule hands
    /// it out with the access it grants, [`trap_write_armed`] folds it
    /// with C's `asActive` test (`asLib.h:57-60`
    /// `asTrapWriteWithData`), and [`TrapWriteGuard`] fires the
    /// Before/After pair that C's `asTrapWriteBeforeWithData`
    /// (`asTrapWrite.c:114`) and `asTrapWriteAfterWrite` fire. What is
    /// per-server is only the LISTENER, registered through
    /// [`register_trap_write_listener`] — C keeps the same split
    /// (`asTrapWrite.c:122-123` returns 0 on an empty `listenerList`).
    pub trap: bool,
    /// CALC condition expression (epics-base `asLib.y:294-299`,
    /// `RULE(...) { CALC("A=1") }`). `None` means an unconditional
    /// rule. When `Some`, the rule only grants access while the
    /// expression evaluates to 1 against the ASG's `INP*` link values.
    pub calc: Option<Vec<u8>>,
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
    /// What `asCa.c` has delivered for [`Self::inp`] and the rule results
    /// computed from it. Shared by clones, so a HAG refresh keeps it and only
    /// a reloaded file starts afresh, as C's `asInit` builds a new `ASG`.
    state: std::sync::Arc<arc_swap::ArcSwap<AsgState>>,
}

/// C's per-ASG `asCa` state: `pavalue`, `inpBad`, `inpChanged` and every
/// rule's `result`. The ASG `INP*` watcher is its only writer, through
/// `AccessSecurityGroup::deliver`; access checks and the dumps read the
/// published snapshot and never resolve a link themselves.
#[derive(Debug, Clone, Default)]
pub struct AsgState {
    /// C `pasg->pavalue`.
    values: crate::calc::NumericInputs,
    /// The declared inputs whose last delivery was good — C's `inpBad`
    /// inverted, so the state before any delivery has every declared input
    /// bad, as `asCaTask` leaves it (`asCa.c:183`).
    good: u32,
    /// C `pasg->inpChanged`, consumed by [`Self::compute`].
    changed: u32,
    /// C `pasgrule->result`, by rule index; a rule never computed is FALSE.
    results: Vec<bool>,
}

/// One `asCa.c` callback for an input.
#[derive(Debug, Clone, Copy)]
pub(crate) enum InpEvent {
    /// `eventCallback` with a `DBR_STS_DOUBLE`.
    Reading(InpReading),
    /// `connectCallback` on a disconnect: the record went away.
    Disconnected,
}

impl AsgState {
    /// `asCa.c eventCallback` (`:146-162`) and `connectCallback` (`:91-105`).
    fn apply(&mut self, index: u8, event: InpEvent) {
        let idx = index as usize;
        if idx >= crate::calc::CALC_NARGS {
            return;
        }
        let bit = 1u32 << idx;
        match event {
            InpEvent::Reading(r) if r.severity == crate::server::record::AlarmSeverity::Invalid => {
                self.good &= !bit;
                self.changed |= bit;
            }
            InpEvent::Reading(r) => {
                self.good |= bit;
                self.values.vars[idx] = r.value;
                self.changed |= bit;
            }
            InpEvent::Disconnected => self.good &= !bit,
        }
    }

    /// C `asComputeAsgPvt` (`asLibRoutines.c:953-978`): rerun each CALC that
    /// reads a changed input, with `VAL` its previous result.
    fn compute(&mut self, rules: &[AccessRule]) {
        self.results.resize(rules.len(), false);
        for (rule, result) in rules.iter().zip(self.results.iter_mut()) {
            if rule.ignore || self.changed & rule.inp_used == 0 {
                continue;
            }
            let Some(compiled) = &rule.calc_compiled else {
                continue;
            };
            let mut args = self.values.clone();
            args.prev_val = f64::from(u8::from(*result));
            *result = crate::calc::eval(compiled, &mut args).is_ok_and(calc_result_is_true);
        }
        self.changed = 0;
    }

    /// C `pasg->inpBad` for a group declaring `declared`.
    fn bad(&self, declared: u32) -> u32 {
        declared & !self.good
    }

    /// C `pasg->pavalue[index]`: the last good value delivered, 0 before one.
    pub fn value(&self, index: u8) -> f64 {
        self.values.vars.get(index as usize).copied().unwrap_or(0.0)
    }

    /// C `pasgrule->result` of rule `index`.
    pub fn result(&self, index: usize) -> bool {
        self.results.get(index).copied().unwrap_or(false)
    }
}

impl AccessSecurityGroup {
    /// The inputs this group declares, as a bitmap over A..U.
    fn declared(&self) -> u32 {
        self.inp
            .iter()
            .filter(|inp| (inp.index as usize) < crate::calc::CALC_NARGS)
            .fold(0, |bits, inp| bits | 1u32 << inp.index)
    }

    /// The published state.
    pub fn state(&self) -> std::sync::Arc<AsgState> {
        self.state.load_full()
    }

    /// C `pasg->inpBad`: the declared inputs with no good value delivered.
    pub fn inp_bad(&self) -> u32 {
        self.state.load().bad(self.declared())
    }

    /// Apply `events` in order, each followed by `asComputeAsg` as
    /// `asCa.c:161` runs it, and publish the result once.
    pub(crate) fn deliver(&self, events: impl IntoIterator<Item = (u8, InpEvent)>) {
        let mut state = AsgState::clone(&self.state.load());
        for (index, event) in events {
            state.apply(index, event);
            state.compute(&self.rules);
        }
        self.state.store(std::sync::Arc::new(state));
    }
}

/// One `INP*` link's value with its alarm severity: the `DBR_STS_DOUBLE`
/// `asCa.c` subscribes for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InpReading {
    pub value: f64,
    pub severity: crate::server::record::AlarmSeverity,
}

/// A single `INP(A..U)` link declaration within an ASG.
#[derive(Debug, Clone)]
pub struct AsgInp {
    /// Letter index: 0 = `A`, .. 20 = `U`.
    pub index: u8,
    /// The link string (typically a record.field PV name).
    pub link: Vec<u8>,
}

impl AsgInp {
    /// The link as a record name. Database names are text, so a link that is
    /// not UTF-8 names no record and its input is bad, as an unknown name is.
    pub fn record_link(&self) -> Option<&str> {
        std::str::from_utf8(&self.link).ok()
    }
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
    // A prefix of `link` cut at an ASCII `.`, so still UTF-8.
    let record = &link[..record.len()];
    (record, if field.is_empty() { "VAL" } else { field })
}

/// `inst`'s `field` as a `DBR_STS_DOUBLE`.
fn inp_reading(inst: &crate::server::record::RecordInstance, field: &str) -> Option<InpReading> {
    let value = inst.resolve_field(field).and_then(|v| v.to_f64())?;
    Some(InpReading {
        value,
        severity: inst.common.sevr,
    })
}

/// `record.field` as a double, for a post whose log holds no value.
fn read_inp_value(
    db: &crate::server::database::PvDatabase,
    record: &str,
    field: &str,
) -> Option<f64> {
    db.get_record(record)?.read().resolve_field(field)?.to_f64()
}

/// Access Security Configuration parsed from an ACF file.
#[derive(Debug, Clone)]
pub struct AccessSecurityConfig {
    /// Names and members as the file spells them: C keys and compares every
    /// ACF token by its bytes (`gphFind`, `strcmp`).
    pub uag: HashMap<Vec<u8>, Vec<Vec<u8>>>,
    pub hag: HashMap<Vec<u8>, Vec<Vec<u8>>>,
    /// The HAG members exactly as spelled in the ACF, keyed like `hag`.
    /// `hag` stores `hag_members` resolution *output* (dotted quads
    /// under `asCheckClientIP`), which cannot be re-resolved after a
    /// DNS change; [`Self::with_refreshed_hags`] re-runs the resolution
    /// from these raw spellings (epics-base#863 / UI-107).
    pub hag_raw: HashMap<Vec<u8>, Vec<Vec<u8>>>,
    pub asg: HashMap<Vec<u8>, AccessSecurityGroup>,
    pub unknown_access: AccessLevel,
    /// This load of the file, kept by clones as each group's state is.
    load: LoadId,
}

/// The identity of one parse of an ACF: what C's `asInit` builds afresh with
/// its `ASG`s and `asCa` channels. Fresh per [`Default`], shared by clones.
#[derive(Debug, Clone, Default)]
struct LoadId(std::sync::Arc<()>);

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
        let refreshed: HashMap<Vec<u8>, Vec<Vec<u8>>> = self
            .hag_raw
            .iter()
            .map(|(name, raw)| (name.clone(), hag_members(raw, false)))
            .collect();
        if refreshed == self.hag {
            return None;
        }
        let mut new = self.clone();
        new.hag = refreshed;
        Some(new)
    }

    /// C `asDumpFP(fp, memcallback, NULL, verbose=TRUE)`, the `asdbdump` and
    /// CA gateway R3 dump, without its MEMBERLIST: this crate keeps no
    /// `asgMemberList`. Inputs and CALC results are each group's published
    /// [`AsgState`], as C prints `pasg`'s. Names come out sorted because C's
    /// `asUagAdd`/`asHagAdd`/`asAsgAdd` keep each list sorted.
    pub fn dump_report(&self) -> Vec<u8> {
        let mut out = Vec::new();
        dump_groups(&mut out, b"UAG", &self.uag);
        dump_groups(&mut out, b"HAG", &self.hag);
        self.dump_asgs(None, AsgDump::Verbose, &mut out);
        out
    }

    /// C `asDumpRulesFP`, the `asprules` dump: every ASG, or only `asgname`.
    pub fn dump_rules(&self, asgname: Option<&[u8]>) -> Vec<u8> {
        let mut out = Vec::new();
        self.dump_asgs(asgname, AsgDump::Rules, &mut out);
        out
    }

    fn dump_asgs(&self, only: Option<&[u8]>, style: AsgDump, out: &mut Vec<u8>) {
        if self.asg.is_empty() {
            out.extend_from_slice(b"No ASGs\n");
        }
        let mut names: Vec<_> = self.asg.keys().collect();
        names.sort();
        for name in names.into_iter().filter(|n| only.is_none_or(|o| o == *n)) {
            let asg = &self.asg[name];
            push_all(out, &[b"ASG(", name, b")"]);
            let braced = !asg.inp.is_empty() || !asg.rules.is_empty();
            out.extend_from_slice(if braced { b" {\n" } else { b"\n" });
            let state = asg.state();
            let inp_bad = state.bad(asg.declared());
            for inp in &asg.inp {
                push_all(out, &[b"\tINP", &[b'A' + inp.index], b"(", &inp.link, b")"]);
                let bad = inp_bad & (1u32 << inp.index) != 0;
                out.extend_from_slice(match (bad, style) {
                    (true, _) => b" INVALID".as_slice(),
                    (false, AsgDump::Verbose) => b"   VALID",
                    (false, AsgDump::Rules) => b"",
                });
                let value = state.value(inp.index);
                out.extend_from_slice(format!(" value={}\n", CFloat(value)).as_bytes());
            }
            for (index, rule) in asg.rules.iter().enumerate().filter(|(_, r)| !r.ignore) {
                let access = match rule.access {
                    RuleAccess::None => "NONE",
                    RuleAccess::Read => "READ",
                    RuleAccess::Write => "WRITE",
                };
                let trap = if rule.trap {
                    "TRAPWRITE"
                } else {
                    "NOTRAPWRITE"
                };
                out.extend_from_slice(format!("\tRULE({},{access},{trap})", rule.level).as_bytes());
                let braced = !rule.uag.is_empty()
                    || !rule.hag.is_empty()
                    || rule.calc.is_some()
                    || !rule.method.is_empty()
                    || !rule.authority.is_empty();
                out.extend_from_slice(if braced { b" {\n" } else { b"\n" });
                for (kw, names) in [(b"UAG", &rule.uag), (b"HAG", &rule.hag)] {
                    if !names.is_empty() {
                        push_all(out, &[b"\t\t", kw, b"(", &names.join(&b','), b")\n"]);
                    }
                }
                // Not in C base, which disables a rule naming either keyword.
                for m in &rule.method {
                    push_all(out, &[b"\t\tMETHOD(\"", m, b"\")\n"]);
                }
                for a in &rule.authority {
                    push_all(out, &[b"\t\tAUTHORITY(\"", a, b"\")\n"]);
                }
                if let Some(calc) = &rule.calc {
                    let result: &[u8] = if state.result(index) {
                        b"TRUE"
                    } else {
                        b"FALSE"
                    };
                    push_all(out, &[b"\t\tCALC(\"", calc, b"\") result=", result, b"\n"]);
                }
                if braced {
                    out.extend_from_slice(b"\t}\n");
                }
            }
            if braced {
                out.extend_from_slice(b"}\n");
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
    pub fn check_access(
        &self,
        asg_name: impl AsRef<[u8]>,
        host: impl AsRef<[u8]>,
        user: impl AsRef<[u8]>,
    ) -> AccessLevel {
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
        asg_name: impl AsRef<[u8]>,
        host: impl AsRef<[u8]>,
        user: impl AsRef<[u8]>,
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
    /// rules with the given `roles`.
    #[allow(clippy::too_many_arguments)]
    pub fn compute_for_name(
        &self,
        asg_name: impl AsRef<[u8]>,
        host: impl AsRef<[u8]>,
        user: impl AsRef<[u8]>,
        roles: &[crate::types::PvString],
        record_asl: u8,
        method: &str,
        authority: &str,
    ) -> (AccessLevel, bool) {
        let Some(asg) = self.asg_or_default(asg_name.as_ref()) else {
            return (AccessLevel::NoAccess, false);
        };
        self.compute_rules(
            asg,
            host.as_ref(),
            user.as_ref(),
            roles,
            record_asl,
            method,
            authority,
        )
    }

    pub fn check_access_method_trap(
        &self,
        asg_name: impl AsRef<[u8]>,
        host: impl AsRef<[u8]>,
        user: impl AsRef<[u8]>,
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
        // Never grant ReadWrite on an ASG-lookup miss. C resolves every miss
        // to the always-present empty `DEFAULT` ⇒ `asNOACCESS`.
        let Some(asg) = self.asg_or_default(asg_name.as_ref()) else {
            return (AccessLevel::NoAccess, false);
        };
        // C `asComputePvt` (asLibRoutines.c:983) initialises
        // `access = asNOACCESS` and only ever *raises* it on a matching
        // RULE. An ASG with no RULE statements (`ASG(LOCKED)`)
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
        self.compute_rules(
            asg,
            host.as_ref(),
            user.as_ref(),
            &[],
            record_asl,
            method,
            authority,
        )
    }

    /// The single rule-matching loop — C `asComputePvt`
    /// (`asLibRoutines.c:992-1062`) — parameterised by the client's `roles`
    /// (for `role/<name>` UAG members, QSRV `documentation/ioc.rst:181-188`).
    /// A CALC-gated rule reads the result the `INP*` watcher published
    /// ([`AsgState`]); C's `asComputePvt` (`:1048`) likewise only consumes the
    /// `pasgrule->result` that `asComputeAsgPvt` stored.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn compute_rules(
        &self,
        asg: &AccessSecurityGroup,
        host: &[u8],
        user: &[u8],
        roles: &[crate::types::PvString],
        record_asl: u8,
        method: &str,
        authority: &str,
    ) -> (AccessLevel, bool) {
        let mut access = AccessLevel::NoAccess;
        let mut trap = false;
        let state = asg.state.load();
        let bad = state.bad(asg.declared());
        for (index, rule) in asg.rules.iter().enumerate() {
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
            if i32::from(record_asl) > rule.level {
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
                                m.as_slice() == user
                                    || matches!(
                                        m.strip_prefix(b"role/"),
                                        Some(role) if roles.iter().any(|r| r.as_bytes() == role)
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
            let host_match = rule.hag.is_empty()
                || rule.hag.iter().any(|g| {
                    self.hag
                        .get(g)
                        .map(|members| members.iter().any(|m| m.eq_ignore_ascii_case(host)))
                        .unwrap_or(false)
                });
            if !host_match {
                continue;
            }
            let method_match = rule.method.is_empty()
                || rule
                    .method
                    .iter()
                    .any(|m| m.eq_ignore_ascii_case(method.as_bytes()));
            if !method_match {
                continue;
            }
            let authority_match = rule.authority.is_empty()
                || rule
                    .authority
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(authority.as_bytes()));
            if !authority_match {
                continue;
            }
            // C `asLibRoutines.c:1048`:
            // `!(pasg->inpBad & pasgrule->inpUsed) && (pasgrule->result==1)`.
            // A bad input the rule READS disables it; a bad input elsewhere
            // in the group is none of this rule's business.
            if rule.calc.is_some() && (bad & rule.inp_used != 0 || !state.result(index)) {
                continue;
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

    /// `asg_name`'s group, or `DEFAULT` for a name the file does not declare
    /// (C `asAddMemberPvt`).
    fn asg_or_default(&self, asg_name: &[u8]) -> Option<&AccessSecurityGroup> {
        self.asg
            .get(asg_name)
            .or_else(|| self.asg.get(b"DEFAULT".as_slice()))
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
        asg_name: impl AsRef<[u8]>,
        host: impl AsRef<[u8]>,
        user: impl AsRef<[u8]>,
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
/// `rsrv/camessage.c:768-779`. Listeners registered via
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
    /// C `asTrapWriteMessage::serverSpecific` channel name: bytes, never decoded.
    pub pv_name: &'a [u8],
    /// C `asTrapWriteMessage::userid`: the client's bytes, never decoded.
    pub user: &'a [u8],
    pub host: &'a [u8],
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
            // asTrapWrite.c prints nothing, and the panic hook has already.
            tracing::debug!(
                target: "epics_base_rs::server::access_security",
                pv = %String::from_utf8_lossy(msg.pv_name),
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
    pub pv_name: Vec<u8>,
    pub user: Vec<u8>,
    pub host: Vec<u8>,
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
/// completion (`rsrv/camessage.c:1400`), still-busy teardown
/// (`rsrvFreePutNotify`, `camessage.c:1621-1660`), and supersede-cancel
/// (`write_notify_action`, `camessage.c:1697-1700`) — and mirrors pvxs's
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
    pub pv_name: &'a [u8],
    /// Authenticated account name (pvxs `cred->account`), as bytes.
    pub user: &'a [u8],
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
        pv_name: meta.pv_name.to_vec(),
        user: meta.user.to_vec(),
        host: meta.host.as_bytes().to_vec(),
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
/// Rust mutated `instance.common.text.asg` directly with no notification
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

/// Which of C's two ASG dumps: `asDumpFP` verbose pads a good input with
/// `   VALID`, `asDumpRulesFP` prints nothing for it.
#[derive(Clone, Copy)]
enum AsgDump {
    Verbose,
    Rules,
}

/// `asDumpFP`'s UAG or HAG list: `NAME(g) {a,b}`, members raw, or `No UAGs`.
fn dump_groups(out: &mut Vec<u8>, kind: &[u8], groups: &HashMap<Vec<u8>, Vec<Vec<u8>>>) {
    if groups.is_empty() {
        push_all(out, &[b"No ", kind, b"s\n"]);
    }
    let mut names: Vec<_> = groups.keys().collect();
    names.sort();
    for name in names {
        push_all(out, &[kind, b"(", name, b")"]);
        let members = &groups[name];
        if members.is_empty() {
            out.push(b'\n');
        } else {
            push_all(out, &[b" {", &members.join(&b','), b"}\n"]);
        }
    }
}

/// C `asDumpFP` prints names, links and CALC with `%s`: the bytes as stored.
fn push_all(out: &mut Vec<u8>, parts: &[&[u8]]) {
    for p in parts {
        out.extend_from_slice(p);
    }
}

/// The ACF reader, and the single owner of the line counter every
/// diagnostic out of this parser carries.
///
/// The counter is the reason this is a type rather than a bare
/// `Peekable<Chars>`: [`Self::next`] is the ONLY way the input advances, so
/// `line` cannot drift from the text — no caller can step over a newline
/// without the counter seeing it. C keeps the same number in
/// `asLib_lex.l`'s `line_num`.
struct AcfScanner<'a> {
    /// C lexes bytes: a quoted string takes any byte but `"`, `\n` and `\`.
    src: &'a [u8],
    /// Byte offset of the next character.
    pos: usize,
    /// 1-based, counting every `\n` consumed.
    line: u32,
    /// asLib.y `yyWarned`: only a parse's first warning prints.
    warned: bool,
    /// The line that warning printed.
    warning: Option<String>,
    /// What asLib.y wrote on the thread's stderr, in order: every `yyerror`
    /// and the first `yywarn`.
    said: Vec<String>,
    /// Each `yyerror`, with the errlog line that explains an empty one, for
    /// the parse's `Err`. Any entry fails the parse, as `yyFailed` does.
    errors: Vec<String>,
    /// `asInitialize`'s errlog lines for a user, then a host, repeated
    /// within one group, in file order.
    duplicated_users: Vec<String>,
    duplicated_hosts: Vec<String>,
}

/// asLib.y's grammar refusing the token on `line`: bison's `syntax error`,
/// which ends the parse.
struct SyntaxError {
    line: u32,
}

type Parse<T> = Result<T, SyntaxError>;

impl<'a> AcfScanner<'a> {
    fn new(content: &'a [u8]) -> Self {
        Self {
            src: content,
            pos: 0,
            line: 1,
            warned: false,
            warning: None,
            said: Vec::new(),
            errors: Vec::new(),
            duplicated_users: Vec::new(),
            duplicated_hosts: Vec::new(),
        }
    }

    /// asLib.y `yywarn`. `line` is where C's grammar action runs: a
    /// construct C must see past to recognise is reported at the line of
    /// the token after it.
    fn warn(&mut self, what: &str, line: u32, token: &[u8]) {
        if !self.warned && !token.is_empty() {
            let warning = format!(
                "{} {what} at line {line}: {}",
                crate::runtime::log::ERL_WARNING,
                String::from_utf8_lossy(token)
            );
            self.said.push(warning.clone());
            self.warning = Some(warning);
        }
        self.warned = true;
    }

    /// asLib.y `yyerror(what)` at the current line: said, and the parse
    /// goes on.
    fn error(&mut self, what: &str) {
        let line = self.line;
        self.said.push(format!(
            "{} {what} at line {line}",
            crate::runtime::log::ERL_ERROR
        ));
        self.errors.push(format!("{what} at line {line}"));
    }

    /// An asLibRoutines.c refusal: its errlog line, then asLib.y's
    /// `yyerror("")`.
    fn refuse(&mut self, why: String) {
        crate::runtime::log::errlog_vprintf(format_args!("{why}\n"));
        self.error_saying_where(why);
    }

    /// asLib.y `yyerror("")`, which says only where; `why` is for the `Err`.
    fn error_saying_where(&mut self, why: String) {
        let line = self.line;
        self.said
            .push(format!("{} at line {line}", crate::runtime::log::ERL_ERROR));
        self.errors.push(format!("{why} at line {line}"));
    }

    /// The grammar refusing the lookahead, which the caller has just read
    /// or stands on.
    fn reject(&self) -> SyntaxError {
        SyntaxError { line: self.line }
    }

    fn peek(&mut self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    /// The one place the input advances, so the one place `line` moves.
    fn next(&mut self) -> Option<u8> {
        let c = *self.src.get(self.pos)?;
        self.pos += 1;
        if c == b'\n' {
            self.line += 1;
        }
        Some(c)
    }

    /// Step over everything asLib_lex.l reads without returning a token, so
    /// the cursor stands on a token's first byte or at the end: whitespace,
    /// comments, and each lexeme C's lexer reports with `yyerror` and drops
    /// — a byte that starts no token, a quoted string a newline cuts, a
    /// number that does not parse. Those are said here, so they interleave
    /// with the grammar's own errors as C's lexer and parser do.
    fn skip_ws_comments(&mut self) {
        while let Some(c) = self.peek() {
            match c {
                c if is_acf_space(c) => {
                    self.next();
                }
                b'#' => {
                    while self.peek().is_some_and(|c| c != b'\n') {
                        self.next();
                    }
                }
                b'(' | b')' | b'{' | b'}' | b',' => return,
                b'"' => match self.quoted_end() {
                    QuoteEnd::Closed => return,
                    QuoteEnd::Newline(newline) => {
                        self.error("Newline in quoted string, closing quote missing");
                        while self.pos <= newline {
                            self.next();
                        }
                    }
                    QuoteEnd::Stray => {
                        self.error(&invalid_character(b'"'));
                        self.next();
                    }
                },
                c if is_acf_name_char(c) => {
                    let run = self.name_run();
                    let Some(why) = number_error(run) else {
                        return;
                    };
                    self.error(&why);
                    self.pos += run.len();
                }
                c => {
                    self.error(&invalid_character(c));
                    self.next();
                }
            }
        }
    }

    /// The `{name}+` run at the cursor.
    fn name_run(&self) -> &'a [u8] {
        let rest = &self.src[self.pos..];
        let len = rest.iter().take_while(|&&c| is_acf_name_char(c)).count();
        &rest[..len]
    }

    /// Which asLib_lex.l rule takes the `"` at the cursor.
    fn quoted_end(&self) -> QuoteEnd {
        let mut i = self.pos + 1;
        loop {
            match self.src.get(i) {
                Some(b'"') => return QuoteEnd::Closed,
                Some(b'\n') => return QuoteEnd::Newline(i),
                Some(b'\\') if self.src.get(i + 1).is_some_and(|&c| c != b'\n') => i += 2,
                Some(b'\\') | None => return QuoteEnd::Stray,
                Some(_) => i += 1,
            }
        }
    }

    /// The body of the quoted token at the cursor: asLib_lex.l
    /// `{doublequote}({stringchar}|{escape})*{doublequote}`, which C stores
    /// from `yytext+1` with every `\x` pair verbatim.
    fn quoted(&mut self) -> Vec<u8> {
        self.next();
        let mut body = Vec::new();
        loop {
            match self.next() {
                Some(b'"') | None => return body,
                Some(b'\\') => body.extend([b'\\', self.next().unwrap_or_default()]),
                Some(c) => body.push(c),
            }
        }
    }
}

/// asLib_lex.l's two quoted-string rules, or neither.
enum QuoteEnd {
    /// `{doublequote}({stringchar}|{escape})*{doublequote}`: a token.
    Closed,
    /// `…{newline}`, the newline at this offset: `yyerror` and no token.
    Newline(usize),
    /// Input ends, or a `\` meets a newline, first: only the `"` itself
    /// matches, and the `.` rule refuses it.
    Stray,
}

/// The `yyerror` asLib_lex.l's number rules give a `{name}+` run they take
/// whole and `epicsParseInt64` / `epicsParseDouble` refuse.
fn number_error(run: &[u8]) -> Option<String> {
    if !is_acf_number(run) {
        return None;
    }
    let text = std::str::from_utf8(run).expect("a number run is ASCII");
    if !run.contains(&b'.') {
        return text
            .parse::<i64>()
            .is_err()
            .then(|| format!("Error parsing Int64: {text}"));
    }
    // `strtod`'s ERANGE: an overflow, or a result below the normal range.
    let v: f64 = text.parse().expect("a float run parses");
    let underflow = v.abs() < f64::MIN_POSITIVE && mantissa_nonzero(run);
    (v.is_infinite() || underflow).then(|| format!("Error parsing Float64: {text}"))
}

/// A non-zero digit before the exponent.
fn mantissa_nonzero(run: &[u8]) -> bool {
    run.iter()
        .take_while(|&&c| c != b'e' && c != b'E')
        .any(|c| (b'1'..=b'9').contains(c))
}

/// asLib_lex.l `whitespace [ \t\r]` plus `newline`; form feed and vertical
/// tab are not whitespace to C.
fn is_acf_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n')
}

/// asLib_lex.l `name [a-zA-Z0-9_\-+:.\[\]<>;]`.
fn is_acf_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            b'_' | b'.' | b'-' | b'+' | b':' | b'[' | b']' | b'<' | b'>' | b';'
        )
}

/// asLib_lex.l's `.` rule message. e_flex's 8-bit `yytext` is `unsigned
/// char *`, so a high byte prints as `0xff`, not a sign-extended
/// `0xffffffff` (ascheck agrees).
fn invalid_character(c: u8) -> String {
    if c.is_ascii_graphic() || c == b' ' {
        format!("Invalid character '{}'", c as char)
    } else {
        format!("Invalid character 0x{c:02x}")
    }
}

/// The threads access security blocks on: an ACF file read, and the host
/// lookups a HAG parse or refresh makes. Not a callback band, because the
/// INP watcher shares cbMedium and C does this work on the iocsh thread.
pub static AS_OFFLOAD: crate::runtime::offload::OffloadPool =
    crate::runtime::offload::OffloadPool::new(
        "asOffload",
        crate::runtime::task::ThreadPriority::Low,
    );

/// [`parse_acf`] of a file, for async callers, on [`AS_OFFLOAD`]: the read can
/// stall on a slow mount and a HAG parse resolves its hosts.
pub async fn read_acf(path: impl AsRef<std::path::Path>) -> CaResult<AccessSecurityConfig> {
    let path = path.as_ref().to_path_buf();
    AS_OFFLOAD
        .run(move || parse_acf(std::fs::read(path).map_err(CaError::Io)?))
        .map_err(CaError::Io)?
        .await
}

pub fn parse_acf(content: impl AsRef<[u8]>) -> CaResult<AccessSecurityConfig> {
    parse_acf_reporting(content.as_ref(), &|line| crate::stderr_println!("{line}"))
}

/// [`parse_acf`] with asLib.y's `yyerror` and `yywarn` lines handed to
/// `report`, in order: C's `fprintf(stderr)` there is the calling thread's
/// stderr, which `asInit 2> f` swaps. The `Err` carries each error again,
/// so the caller has nothing left to print.
pub(crate) fn parse_acf_reporting(
    content: &[u8],
    report: &dyn Fn(&str),
) -> CaResult<AccessSecurityConfig> {
    let mut sc = AcfScanner::new(content);
    let parsed = parse_acf_in(&mut sc);
    if let Err(SyntaxError { line }) = parsed {
        sc.line = line;
        sc.error("syntax error");
    }
    for line in &sc.said {
        report(line);
    }
    match parsed {
        Ok(config) if sc.errors.is_empty() => {
            // `asInitialize` hashes the members only once the parse succeeded.
            for line in sc.duplicated_users.iter().chain(&sc.duplicated_hosts) {
                crate::runtime::log::errlog_vprintf(format_args!("{line}"));
            }
            Ok(config)
        }
        _ => Err(CaError::Protocol(format!("ACF {}", sc.errors.join("; ")))),
    }
}

/// [`parse_acf`] on a scanner a test can ask what it said.
fn parse_acf_in(sc: &mut AcfScanner) -> Parse<AccessSecurityConfig> {
    let mut config = AccessSecurityConfig {
        uag: HashMap::new(),
        hag: HashMap::new(),
        hag_raw: HashMap::new(),
        asg: HashMap::new(),
        unknown_access: AccessLevel::Read,
        load: LoadId::default(),
    };

    // C `asInitialize` (asLibRoutines.c:107) calls `asAsgAdd(DEFAULT)`
    // *before* parsing the file, so a `DEFAULT` ASG always exists.
    // Synthesise it here unconditionally: any record whose
    // `ASG` field names an unknown group resolves to this empty
    // `DEFAULT`, which has no RULEs ⇒ `asNOACCESS` ⇒ access denied.
    // A `DEFAULT` block in the file takes this placeholder over while it is
    // still empty, as `asAsgAdd` returns it.
    config
        .asg
        .insert(b"DEFAULT".to_vec(), AccessSecurityGroup::default());

    loop {
        let keyword = match read_token(sc) {
            Some(Token::Keyword(k)) => k,
            Some(Token::Name(k) | Token::Quoted(k)) => {
                skip_generic_item(sc, &k)?;
                continue;
            }
            Some(Token::Number(_)) => return Err(sc.reject()),
            // An empty or comment-only file is the deliberate divergence
            // from C kept by `empty_acf_denies_all_access`.
            None => match sc.peek() {
                Some(_) => return Err(sc.reject()),
                None => break,
            },
        };
        match keyword.as_slice() {
            // `asUagAdd` / `asHagAdd` refuse a name already defined, and the
            // members that follow go nowhere.
            b"UAG" => {
                let name = read_paren_name(sc)?;
                let fresh = !config.uag.contains_key(&name);
                if !fresh {
                    sc.refuse(duplicate("User Access Group", &name));
                }
                let members = read_optional_brace_list(sc)?;
                if !fresh {
                    continue;
                }
                for user in repeated(&members) {
                    sc.duplicated_users.push(format!(
                        "Duplicated user '{}' in UAG '{}'\n",
                        String::from_utf8_lossy(user),
                        String::from_utf8_lossy(&name)
                    ));
                }
                config.uag.insert(name, members);
            }
            b"HAG" => {
                let name = read_paren_name(sc)?;
                let fresh = !config.hag.contains_key(&name);
                if !fresh {
                    sc.refuse(duplicate("Host Access Group", &name));
                }
                let members = read_optional_brace_list(sc)?;
                if !fresh {
                    continue;
                }
                // C `asHagAddHost` reads `asCheckClientIP` at ACF-parse
                // time and stores names or resolved IPs accordingly.
                let hosts = hag_members(&members, true);
                // C compares the stored form: lowercased, or the resolved IP.
                for host in repeated(&hosts) {
                    sc.duplicated_hosts.push(format!(
                        "Duplicated host '{}' in HAG '{}'\n",
                        String::from_utf8_lossy(host),
                        String::from_utf8_lossy(&name)
                    ));
                }
                config.hag.insert(name.clone(), hosts);
                config.hag_raw.insert(name, members);
            }
            b"ASG" => {
                let name = read_paren_name(sc)?;
                let taken = |g: &AccessSecurityGroup| {
                    name != b"DEFAULT" || !g.rules.is_empty() || !g.inp.is_empty()
                };
                // `asAsgAdd` makes the group, or hands back the empty DEFAULT,
                // before the body is read; a bare head leaves it empty. A
                // refused name leaves `yyAsg` NULL for the body.
                let asg = if config.asg.get(&name).is_some_and(taken) {
                    sc.refuse(duplicate("Access Security Group", &name));
                    None
                } else {
                    config.asg.entry(name.clone()).or_default();
                    Some(name)
                };
                sc.skip_ws_comments();
                if sc.peek() == Some(b'{') {
                    parse_asg_body(sc, &mut config, asg.as_deref())?;
                }
            }
            // The other lexer keywords cannot start a `generic_item`.
            _ => return Err(sc.reject()),
        }
    }

    Ok(config)
}

/// Each member a group already holds, once per repeat: the entries C
/// `gphAdd` refuses, as it keys a name by its group.
fn repeated(members: &[Vec<u8>]) -> impl Iterator<Item = &[u8]> {
    let mut seen = std::collections::HashSet::new();
    members
        .iter()
        .map(Vec::as_slice)
        .filter(move |m| !seen.insert(*m))
}

/// The errlog line `asUagAdd` / `asHagAdd` / `asAsgAdd` give a name already
/// defined.
fn duplicate(kind: &str, name: &[u8]) -> String {
    format!("Duplicate {kind} named '{}'", String::from_utf8_lossy(name))
}

/// asLib.y `generic_item`: a `generic_head`, then a `generic_block`, or a
/// one-element block followed by a `{ generic_list }` (the nested form).
/// C warns and parses on; anything else is a syntax error.
fn skip_generic_item(sc: &mut AcfScanner, keyword: &[u8]) -> Parse<()> {
    read_token_list(sc, b'(', b')', ListKind::Generic)?;
    sc.skip_ws_comments();
    let mut what = "Ignoring unsupported TOP LEVEL bare block";
    if sc.peek() == Some(b'{') {
        what = "Ignoring unsupported TOP LEVEL block";
        if skip_generic_block(sc)? {
            sc.skip_ws_comments();
            if sc.peek() == Some(b'{') {
                if read_token_list(sc, b'{', b'}', ListKind::Generic)?.len() < 2 {
                    return Err(sc.reject());
                }
                what = "Ignoring unsupported TOP LEVEL nested block";
            }
        }
    }
    // The cursor is where C's action runs: past a bare head or a `{ e }`, at
    // the next token, which only it tells from the longer form.
    sc.warn(what, sc.line, keyword);
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
///   (`camessage.c:839-843`, `caservertask.c:1425-1439`).
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
/// `libComRegister.c:475-479`, `:518-520` at `R7.0.10`), and so does this port —
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
///
/// `at_load` prints C's line for each unresolvable host, as `asHagAddHost`
/// does; the port's own refresh re-resolves silently.
fn hag_members(members: &[Vec<u8>], at_load: bool) -> Vec<Vec<u8>> {
    if !as_check_client_ip() {
        // C `tolower` in the C locale moves only ASCII letters.
        return members.iter().map(|m| m.to_ascii_lowercase()).collect();
    }

    let unresolved = |m: &[u8]| {
        if at_load {
            crate::runtime::log::errlog_vprintf(format_args!(
                "ACF: Unable to resolve host '{}'\n",
                String::from_utf8_lossy(m)
            ));
        }
        [b"unresolved:".as_slice(), m].concat()
    };
    members
        .iter()
        .map(|m| {
            // Name resolution takes text; bytes that are not UTF-8 name no host.
            let Ok(name) = std::str::from_utf8(m) else {
                return unresolved(m);
            };
            resolve_hag_host(name).unwrap_or_else(|| unresolved(m))
        })
        .collect()
}

/// `aToIPAddr` for one HAG host: its IPv4 dotted quad, or `None`.
fn resolve_hag_host(m: &str) -> Option<Vec<u8>> {
    // C `aToIPAddr(host, 0, …)` then `"%u.%u.%u.%u"` (asLibRoutines.c:1243-1260).
    let (addr, _) = crate::runtime::socket::a_to_ip_addr(m, 0)?;
    Some(addr.ip().to_string().into_bytes())
}

/// asLib.y `'(' tokenSTRING ')'`: a UAG/HAG/ASG head, an `INP*` link or a
/// CALC.
fn read_paren_name(sc: &mut AcfScanner) -> Parse<Vec<u8>> {
    expect(sc, b'(')?;
    let name = read_string(sc)?;
    expect(sc, b')')?;
    Ok(name)
}

/// The punctuation byte the grammar requires next.
fn expect(sc: &mut AcfScanner, want: u8) -> Parse<()> {
    sc.skip_ws_comments();
    if sc.peek() == Some(want) {
        sc.next();
        Ok(())
    } else {
        Err(sc.reject())
    }
}

/// asLib.y `tokenSTRING`: a quoted string or a bare run.
fn read_string(sc: &mut AcfScanner) -> Parse<Vec<u8>> {
    match read_token(sc) {
        Some(Token::Name(t) | Token::Quoted(t)) => Ok(t),
        _ => Err(sc.reject()),
    }
}

/// asLib.y `tokenUAG uag_head` / `tokenHAG hag_head`: the member list is
/// optional, and a bare head is a group with no members.
fn read_optional_brace_list(sc: &mut AcfScanner) -> Parse<Vec<Vec<u8>>> {
    sc.skip_ws_comments();
    if sc.peek() == Some(b'{') {
        read_token_list(sc, b'{', b'}', ListKind::Strings)
    } else {
        Ok(Vec::new())
    }
}

/// Which asLib.y comma list: `tokenSTRING` items only, or `generic_head`,
/// whose items may also be numbers and which `()` may close at once.
#[derive(Clone, Copy, PartialEq)]
enum ListKind {
    Strings,
    Generic,
}

/// asLib.y's comma lists between `open` and `close`. Each item is one token,
/// so `a b`, `a,,b` and `a,` are syntax errors.
fn read_token_list(
    sc: &mut AcfScanner,
    open: u8,
    close: u8,
    kind: ListKind,
) -> Parse<Vec<Vec<u8>>> {
    expect(sc, open)?;
    let mut items = Vec::new();
    sc.skip_ws_comments();
    if kind == ListKind::Generic && sc.peek() == Some(close) {
        sc.next();
        return Ok(items);
    }
    loop {
        match read_token(sc) {
            Some(Token::Number(_) | Token::Keyword(_)) if kind == ListKind::Strings => {
                return Err(sc.reject());
            }
            Some(token) => items.push(token.into_bytes()),
            None => return Err(sc.reject()),
        }
        sc.skip_ws_comments();
        match sc.next() {
            Some(b',') => {}
            Some(c) if c == close => return Ok(items),
            _ => return Err(sc.reject()),
        }
    }
}

/// An asLib_lex.l token: a quoted string, a bare `{name}+` run, a run the
/// longest match hands to the integer or float rule, or a run that is
/// exactly a keyword. Only the first two are `tokenSTRING`, so a keyword is
/// refused wherever the grammar wants a name.
enum Token {
    Quoted(Vec<u8>),
    Name(Vec<u8>),
    Number(Vec<u8>),
    Keyword(Vec<u8>),
}

impl Token {
    fn into_bytes(self) -> Vec<u8> {
        match self {
            Token::Quoted(b) | Token::Name(b) | Token::Number(b) | Token::Keyword(b) => b,
        }
    }
}

/// asLib_lex.l's keyword rules: `UAG`, `HAG`, `ASG`, `RULE`, `CALC`, `INP[A-U]`.
fn is_acf_keyword(run: &[u8]) -> bool {
    matches!(run, b"UAG" | b"HAG" | b"ASG" | b"RULE" | b"CALC")
        || run.strip_prefix(b"INP").and_then(parse_inp_index).is_some()
}

/// The next token; `None` leaves a punctuation byte, or the end, unread.
fn read_token(sc: &mut AcfScanner) -> Option<Token> {
    sc.skip_ws_comments();
    match sc.peek()? {
        // `""` is a tokenSTRING, so `asUagAddUser` stores an empty
        // member that an empty user name matches.
        b'"' => Some(Token::Quoted(sc.quoted())),
        c if is_acf_name_char(c) => {
            let run = sc.name_run().to_vec();
            sc.pos += run.len();
            Some(if is_acf_number(&run) {
                Token::Number(run)
            } else if is_acf_keyword(&run) {
                Token::Keyword(run)
            } else {
                Token::Name(run)
            })
        }
        _ => None,
    }
}

/// asLib.y `rule_generic_block_elem` after its name: a `generic_head`, then
/// an optional `generic_block`. Read only to be skipped.
fn skip_generic_elem(sc: &mut AcfScanner) -> Parse<()> {
    read_token_list(sc, b'(', b')', ListKind::Generic)?;
    sc.skip_ws_comments();
    if sc.peek() == Some(b'{') {
        skip_generic_block(sc)?;
    }
    Ok(())
}

/// asLib.y `generic_block`: `{ e }`, `{ e, e }`, or a run of named elements
/// `{ X(..) Y(..) {..} }`. True for `{ e }`, which a top-level item may
/// follow with a `{ generic_list }`.
fn skip_generic_block(sc: &mut AcfScanner) -> Parse<bool> {
    sc.next();
    let first = read_token(sc).ok_or_else(|| sc.reject())?;
    sc.skip_ws_comments();
    if sc.peek() == Some(b'(') {
        if let Token::Number(_) = first {
            return Err(sc.reject());
        }
        loop {
            skip_generic_elem(sc)?;
            sc.skip_ws_comments();
            if sc.peek() == Some(b'}') {
                sc.next();
                return Ok(false);
            }
            match read_token(sc) {
                Some(Token::Number(_)) | None => return Err(sc.reject()),
                Some(_) => sc.skip_ws_comments(),
            }
            if sc.peek() != Some(b'(') {
                return Err(sc.reject());
            }
        }
    }
    let mut single = true;
    loop {
        match sc.peek() {
            Some(b'}') => {
                sc.next();
                return Ok(single);
            }
            Some(b',') => {
                single = false;
                sc.next();
                read_token(sc).ok_or_else(|| sc.reject())?;
                sc.skip_ws_comments();
            }
            _ => return Err(sc.reject()),
        }
    }
}

/// asLib_lex.l `[-+]?{digit}+` or `[-+]?{digit}*\.{digit}+([eE][-+]?{digit}+)?`
/// matching a whole `{name}+` run: a tie goes to the earlier number rules.
fn is_acf_number(run: &[u8]) -> bool {
    let unsigned = |t: &[u8]| -> usize { t.iter().take_while(|c| c.is_ascii_digit()).count() };
    let sign = |t: &'_ [u8]| -> usize { usize::from(matches!(t.first(), Some(b'-' | b'+'))) };
    let t = &run[sign(run)..];
    let int = unsigned(t);
    let t = &t[int..];
    let Some(t) = t.strip_prefix(b".") else {
        return int > 0 && t.is_empty();
    };
    let frac = unsigned(t);
    let t = &t[frac..];
    if frac == 0 {
        return false;
    }
    let Some(e) = t.strip_prefix(b"e").or_else(|| t.strip_prefix(b"E")) else {
        return t.is_empty();
    };
    let e = &e[sign(e)..];
    !e.is_empty() && unsigned(e) == e.len()
}

/// What one RULE's clauses act on. C has one `yyAsgRule` for the whole file,
/// and a RULE that makes no rule leaves it on the last one made, so an
/// unsupported access keyword's clauses edit that rule, in any ASG (CBUG-Q1).
enum RuleTarget {
    /// The ASG and index of the rule `asAsgAddRule` made.
    Made(Vec<u8>, usize),
    /// An unsupported access keyword. C warns that it ignores the RULE, and
    /// upstream `aslibtest.c` expects the rest of the file to load, so no
    /// clause of it acts or fails.
    Ignored,
    /// A negative LEVEL or a refused ASG, which fail the parse anyway: C's
    /// NULL rule, which every clause passes but `asAsgRuleDisable`.
    Refused,
}

impl RuleTarget {
    fn rule<'a>(
        &self,
        asgs: &'a mut HashMap<Vec<u8>, AccessSecurityGroup>,
    ) -> Option<&'a mut AccessRule> {
        let RuleTarget::Made(asg, index) = self else {
            return None;
        };
        asgs.get_mut(asg)?.rules.get_mut(*index)
    }
}

/// asLib.y `asg_body`. `name` is `yyAsg`: `None` for a group `asAsgAdd`
/// refused, whose INP and RULE heads then add nothing.
fn parse_asg_body(
    sc: &mut AcfScanner,
    config: &mut AccessSecurityConfig,
    name: Option<&[u8]>,
) -> Parse<()> {
    sc.next();

    // asLib.y `asg_body_list`: one or more `inp_config | rule_config`.
    let mut items = 0;
    loop {
        sc.skip_ws_comments();
        if items > 0 && sc.peek() == Some(b'}') {
            sc.next();
            return Ok(());
        }
        items += 1;
        let Some(Token::Keyword(kw)) = read_token(sc) else {
            return Err(sc.reject());
        };
        if kw == b"RULE" {
            parse_rule(sc, config, name)?;
        } else if let Some(index) = kw.strip_prefix(b"INP").and_then(parse_inp_index) {
            // `INP[A-U]` is one token, `tokenINP` (`asLib_lex.l:48-52`), whose
            // payload is the letter index; `asLib.y:237-243` reads the link.
            let link = read_paren_name(sc)?;
            if let Some(asg) = name.and_then(|name| config.asg.get_mut(name)) {
                asg.inp.push(AsgInp { index, link });
            }
        } else {
            return Err(sc.reject());
        }
    }
}

/// Parse the `A..U` selector suffix of an `INP` token into a 0-based
/// letter index. `"A"` → 0, .. `"U"` → 20. Anything else → `None`.
///
/// Case-SENSITIVE: C `asLib_lex.l:21,47` lexes the selector with the
/// flex pattern `INP[A-U]` (uppercase range only), so `INPa` does not
/// match the `tokenINP` rule — it is a syntax error in C, not selector
/// index 0. Matching only uppercase here keeps that behaviour; a
/// lowercase suffix returns `None` and the caller rejects the ASG.
fn parse_inp_index(suffix: &[u8]) -> Option<u8> {
    match suffix {
        [c @ b'A'..=b'U'] => Some(c - b'A'),
        _ => None,
    }
}

/// A token for a `got '…'` diagnostic, capped so a megabyte-long run cannot
/// put a megabyte on the operator's console.
fn shown(token: &[u8]) -> String {
    const CAP: usize = 32;
    let token = String::from_utf8_lossy(token);
    if token.chars().count() > CAP {
        format!("{}…", token.chars().take(CAP).collect::<String>())
    } else {
        token.into_owned()
    }
}

/// asLib.y `rule_config`. Every clause, the head's TRAPWRITE included, goes to
/// the rule `target` names, as C sends it to `yyAsgRule`.
///
/// Each action runs where C's does: one C reduces on its last token says
/// that token's line, before the next token is lexed.
fn parse_rule(
    sc: &mut AcfScanner,
    config: &mut AccessSecurityConfig,
    asg: Option<&[u8]>,
) -> Parse<()> {
    // asLib.y `rule_head`: `'(' tokenINT64 ',' tokenSTRING [',' tokenSTRING] ')'`.
    // The lexer refused any integer `epicsParseInt64` cannot take.
    expect(sc, b'(')?;
    let level_num = match read_token(sc) {
        Some(Token::Number(n)) if !n.contains(&b'.') => std::str::from_utf8(&n)
            .ok()
            .and_then(|n| n.parse::<i64>().ok())
            .expect("the lexer passes only an Int64 it parsed"),
        _ => return Err(sc.reject()),
    };
    expect(sc, b',')?;

    // C matches `NONE`/`READ`/`WRITE` with `strcmp` and calls
    // `asAsgAddRule(yyAsg, access, (int)$1)`; any other string only warns
    // and adds no rule.
    let access_str = read_string(sc)?;
    let access = match access_str.as_slice() {
        b"WRITE" => Some(RuleAccess::Write),
        b"READ" => Some(RuleAccess::Read),
        b"NONE" => Some(RuleAccess::None),
        _ => None,
    };
    let target = if level_num < 0 {
        sc.error(&format!("RULE: LEVEL must be positive: {level_num}"));
        RuleTarget::Refused
    } else if let Some(access) = access {
        // `asAsgAddRule(NULL, ...)` returns NULL.
        match asg {
            Some(asg) => {
                let rules = &mut config
                    .asg
                    .get_mut(asg)
                    .expect("parse_acf adds the ASG first")
                    .rules;
                rules.push(AccessRule {
                    level: level_num as i32,
                    access,
                    ..AccessRule::default()
                });
                RuleTarget::Made(asg.to_vec(), rules.len() - 1)
            }
            None => RuleTarget::Refused,
        }
    } else {
        sc.warn(
            "Ignoring RULE that contains an unsupported keyword",
            sc.line,
            &access_str,
        );
        RuleTarget::Ignored
    };

    // `rule_log_option`, matched with `strcmp`.
    sc.skip_ws_comments();
    if sc.peek() == Some(b',') {
        sc.next();
        let log_opt = read_string(sc)?;
        match log_opt.as_slice() {
            b"TRAPWRITE" => {
                if let Some(rule) = target.rule(&mut config.asg) {
                    rule.trap = true;
                }
            }
            b"NOTRAPWRITE" => {}
            _ => sc.error("Log options must be TRAPWRITE or NOTRAPWRITE"),
        }
    }
    expect(sc, b')')?;

    sc.skip_ws_comments();
    if sc.peek() != Some(b'{') {
        return Ok(());
    }
    sc.next();
    // asLib.y `rule_list`: one or more items, no separator. With no rule, C's
    // clause helpers return 0 at once: a group is not looked up and a CALC is
    // not compiled.
    let mut items = 0;
    loop {
        sc.skip_ws_comments();
        if items > 0 && sc.peek() == Some(b'}') {
            sc.next();
            return Ok(());
        }
        items += 1;
        let name = match read_token(sc) {
            // `asAsgRuleUagAdd` / `asAsgRuleHagAdd`, one name at a time.
            Some(Token::Keyword(kw)) if kw == b"UAG" || kw == b"HAG" => {
                expect(sc, b'(')?;
                loop {
                    let name = read_string(sc)?;
                    let (kind, defined) = if kw == b"UAG" {
                        ("User", &config.uag)
                    } else {
                        ("Host", &config.hag)
                    };
                    if matches!(target, RuleTarget::Made(..)) && !defined.contains_key(&name) {
                        sc.refuse(format!(
                            "No {kind} Access Group named '{}' defined",
                            String::from_utf8_lossy(&name)
                        ));
                    } else if let Some(rule) = target.rule(&mut config.asg) {
                        if kw == b"UAG" {
                            &mut rule.uag
                        } else {
                            &mut rule.hag
                        }
                        .push(name);
                    }
                    sc.skip_ws_comments();
                    match sc.next() {
                        Some(b',') => {}
                        Some(b')') => break,
                        _ => return Err(sc.reject()),
                    }
                }
                continue;
            }
            // `CALC("<expr>")` — C `asLib.y:294-299`. The *last* CALC
            // clause wins, as C `asAsgRuleCalc` overwrites.
            Some(Token::Keyword(kw)) if kw == b"CALC" => {
                let expr = read_paren_name(sc)?;
                if let Some(rule) = target.rule(&mut config.asg) {
                    match compile_rule_calc(&expr) {
                        Ok((compiled, used)) => {
                            rule.calc = Some(expr);
                            rule.calc_compiled = Some(compiled);
                            rule.inp_used = used;
                        }
                        Err(why) => sc.refuse(why),
                    }
                }
                continue;
            }
            // PR #563: METHOD("ca", "x509", ...); PR #563/#618:
            // AUTHORITY("CA Issuer", ...). C base reads either as the
            // unsupported keyword below, so with no rule it is refused.
            Some(Token::Name(kw)) if kw == b"METHOD" || kw == b"AUTHORITY" => {
                let items = read_token_list(sc, b'(', b')', ListKind::Generic)?;
                // `rule_generic_block_elem` takes an optional `generic_block`
                // after the head; it carries nothing a rule keeps.
                sc.skip_ws_comments();
                if sc.peek() == Some(b'{') {
                    skip_generic_block(sc)?;
                }
                match target.rule(&mut config.asg) {
                    Some(rule) => if kw == b"METHOD" {
                        &mut rule.method
                    } else {
                        &mut rule.authority
                    }
                    .extend(items),
                    None if matches!(target, RuleTarget::Refused) => no_rule_to_disable(sc, &kw),
                    None => {}
                }
                continue;
            }
            // asLib.y `rule_generic_block_elem_name`: a `non_rule_keyword`
            // or a string.
            Some(Token::Name(name) | Token::Quoted(name) | Token::Keyword(name)) => name,
            Some(Token::Number(_)) | None => return Err(sc.reject()),
        };
        // C `asLib.y:300-306`: any other element disables the rule
        // (`asAsgRuleDisable`), which fails on a NULL `yyAsgRule`. An ignored
        // RULE has nothing to disable.
        skip_generic_elem(sc)?;
        sc.warn(
            "Ignoring RULE that contains an unsupported keyword",
            sc.line,
            &name,
        );
        match target.rule(&mut config.asg) {
            Some(rule) => rule.ignore = true,
            None if matches!(target, RuleTarget::Refused) => no_rule_to_disable(sc, &name),
            None => {}
        }
    }
}

/// `asAsgRuleDisable(NULL)` returns 1 and asLib.y makes it `yyerror("")`,
/// with no errlog line to say why.
fn no_rule_to_disable(sc: &mut AcfScanner, keyword: &[u8]) {
    sc.error_saying_where(format!(
        "unsupported keyword '{}' with no RULE to disable",
        shown(keyword)
    ));
}

/// C `asAsgRuleCalc`: `postfix()`, then `calcArgUsage` for the arguments the
/// expression reads. `Err` is the errlog line C gives a refusal.
fn compile_rule_calc(expr: &[u8]) -> Result<(crate::calc::CompiledExpr, u32), String> {
    let shown = String::from_utf8_lossy(expr);
    // A byte that is not UTF-8 is no CALC token, as in C `postfix`.
    let compiled = std::str::from_utf8(expr)
        .map_err(|_| crate::calc::CalcError::Syntax)
        .and_then(crate::calc::compile)
        .map_err(|e| {
            let what = crate::calc::calc_error_str(e.code()).unwrap_or("Unknown error");
            format!("{what} in CALC expression '{shown}'")
        })?;
    // C `asAsgRuleCalc` (`asLibRoutines.c:1416-1425`) refuses the rule when
    // the expression stores into an argument:
    //
    //     /* Until someone proves stores are not dangerous, don't allow them */
    //     if (stores) { … status = S_asLib_badCalc; … }
    //
    // `asLib.y:294-299` turns that status into `yyerror("")`, so the WHOLE
    // file is rejected and a running IOC keeps its previous rule set.
    // `CALC("A:=1;1")` evaluates to 1 whatever the INP links read, so
    // accepting it would make the rule an unconditional grant.
    let (used, stores) = compiled.arg_usage();
    if stores != 0 {
        return Err(format!(
            "Assignment operator used in CALC expression '{shown}'"
        ));
    }
    Ok((compiled, used))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `yywarn` printed for `acf`, against softIoc R7.0.10 on the same
    /// file (`asLib.y:88-103`, `:265`, `:300-306`).
    fn warned(acf: &str) -> Option<String> {
        let mut sc = AcfScanner::new(acf.as_bytes());
        let _ = parse_acf_in(&mut sc);
        sc.warning
            .map(|w| w.replacen(crate::runtime::log::ERL_WARNING, "WARNING", 1))
    }

    /// What the parse of `acf` said on the thread's stderr, and whether it
    /// failed.
    fn said(acf: &[u8]) -> (Vec<String>, bool) {
        let said = std::sync::Mutex::new(Vec::new());
        let parsed = parse_acf_reporting(acf, &|line| {
            said.lock().unwrap().push(
                line.replacen(crate::runtime::log::ERL_ERROR, "ERROR", 1)
                    .replacen(crate::runtime::log::ERL_WARNING, "WARNING", 1),
            )
        });
        (said.into_inner().unwrap(), parsed.is_err())
    }

    /// asLib.y `yyerror`, against `ascheck` and softIoc R7.0.10 on the same
    /// files: a grammar error ends the parse, any other error is said and
    /// the parse goes on. The line is where C's lexer stands when the action
    /// runs: the last token of a rule C reduces without a lookahead, else
    /// the lookahead.
    #[test]
    fn errors_say_cs_lines() {
        let cases: &[(&[u8], &[&str])] = &[
            (b"UAG(u) {a b}", &["ERROR syntax error at line 1"]),
            (b"UAG(u) {a}\n@", &["ERROR Invalid character '@' at line 2"]),
            (
                b"UAG(u){a @ , b}\nASG(g){RULE(1,READ)}\n",
                &["ERROR Invalid character '@' at line 1"],
            ),
            // C's `Newline in quoted string` rule eats the newline without
            // counting it, so every later line of C's is one short; this
            // port counts it.
            (
                b"UAG(u) {\"a\n}",
                &[
                    "ERROR Newline in quoted string, closing quote missing at line 1",
                    "ERROR syntax error at line 2",
                ],
            ),
            (
                b"UAG(u){a}\n\"abc",
                &[
                    "ERROR Invalid character '\"' at line 2",
                    "ERROR syntax error at line 2",
                ],
            ),
            (
                b"ASG(DEFAULT) {\n RULE(-1, READ)\n}\n",
                &["ERROR RULE: LEVEL must be positive: -1 at line 2"],
            ),
            (
                b"ASG(DEFAULT) {\n RULE(1, READ, FOO)\n}\n",
                &["ERROR Log options must be TRAPWRITE or NOTRAPWRITE at line 2"],
            ),
            (
                b"ASG(g) {\nRULE(-1\n,\nREAD\n,\nFOO\n)\n}\n",
                &[
                    "ERROR RULE: LEVEL must be positive: -1 at line 4",
                    "ERROR Log options must be TRAPWRITE or NOTRAPWRITE at line 6",
                ],
            ),
            (
                b"ASG(g){RULE(-1\nREAD)}\n",
                &["ERROR syntax error at line 2"],
            ),
            (b"ASG(g){RULE(1 READ)}\n", &["ERROR syntax error at line 1"]),
            (
                b"UAG(u){a}\nASG(g){RULE(1,READ){UAG(u)}\n",
                &["ERROR syntax error at line 3"],
            ),
            (
                b"ASG(DEFAULT) {\n RULE(1, READ) {\n  UAG(nouag)\n }\n}\n",
                &["ERROR at line 3"],
            ),
            (
                b"UAG(u){a}\nASG(g){RULE(1,READ){UAG(\nx\n,\nu\n,\ny\n)}}\n",
                &["ERROR at line 3", "ERROR at line 7"],
            ),
            (
                b"ASG(g){RULE(1,READ){UAG(x) HAG(y)}}\n",
                &["ERROR at line 1", "ERROR at line 1"],
            ),
            (
                b"ASG(DEFAULT) {\n INPA(x)\n RULE(1,READ) { CALC(\"A+\") }\n}\n",
                &["ERROR at line 3"],
            ),
            (
                b"ASG(g){RULE(1,READ){CALC(\n\"A+\"\n)\n}}\n",
                &["ERROR at line 3"],
            ),
            (
                b"ASG(g){RULE(1,READ){CALC(\"A\xff\")}}\n",
                &["ERROR at line 1"],
            ),
            (b"ASG(g){RULE(1,READ){CALC(\"\")}}\n", &["ERROR at line 1"]),
            (
                b"ASG(g){RULE(-1,READ)\nRULE(1,READ,X)\nRULE(1,READ){CALC(\"A:=1;1\")}\n}\n",
                &[
                    "ERROR RULE: LEVEL must be positive: -1 at line 1",
                    "ERROR Log options must be TRAPWRITE or NOTRAPWRITE at line 2",
                    "ERROR at line 3",
                ],
            ),
            (b"UAG(u){a}\nUAG(\nu\n)\n{b}\n", &["ERROR at line 4"]),
            (
                b"HAG(h){x}\nHAG(h){y}\nASG(g){RULE(1,READ)}\n",
                &["ERROR at line 2"],
            ),
            (
                b"ASG(g){RULE(1,READ)}\nASG(g){RULE(1,WRITE){FOO(x)}}\n",
                &[
                    "ERROR at line 2",
                    "WARNING Ignoring RULE that contains an unsupported keyword at line 2: FOO",
                    "ERROR at line 2",
                ],
            ),
            (
                b"FOO(1.0e999)\nASG(g){RULE(1,READ)}\n",
                &[
                    "ERROR Error parsing Float64: 1.0e999 at line 1",
                    "WARNING Ignoring unsupported TOP LEVEL bare block at line 2: FOO",
                ],
            ),
            (
                b"FOO(1.0e-999)\n",
                &[
                    "ERROR Error parsing Float64: 1.0e-999 at line 1",
                    "WARNING Ignoring unsupported TOP LEVEL bare block at line 2: FOO",
                ],
            ),
            (
                b"FOO(4.9e-320)\n",
                &[
                    "ERROR Error parsing Float64: 4.9e-320 at line 1",
                    "WARNING Ignoring unsupported TOP LEVEL bare block at line 2: FOO",
                ],
            ),
            // C formats this into a 40-byte buffer and aborts on the
            // overflow; the line is the one it means to print.
            (
                b"ASG(g){RULE(99999999999999999999,READ)}\n",
                &[
                    "ERROR Error parsing Int64: 99999999999999999999 at line 1",
                    "ERROR syntax error at line 1",
                ],
            ),
        ];
        for (acf, want) in cases {
            let want = want.iter().map(|l| l.to_string()).collect::<Vec<_>>();
            assert_eq!(
                said(acf),
                (want, true),
                "{:?}",
                String::from_utf8_lossy(acf)
            );
        }
        let ok = b"ASG(g) {\n RULE(1,\"READ\x01\")\n}\n";
        assert_eq!(
            said(ok),
            (
                vec![
                    "WARNING Ignoring RULE that contains an unsupported keyword at line 2: READ\x01"
                        .to_string()
                ],
                false
            )
        );
    }

    #[test]
    fn an_unsupported_element_warns_with_cs_line() {
        let asg = "ASG(DEFAULT) { RULE(1, READ) }\n";
        for (acf, want) in [
            (
                "ASG(DEFAULT) {\n  RULE(1, READ)\n}\nFOO(a) {\n  BAR(b)\n}\n\nASG(Z) { RULE(1,WRITE) }\n".to_string(),
                "Ignoring unsupported TOP LEVEL block at line 6: FOO",
            ),
            (format!("{asg}FOO(a)\n{{\nx\n}}\n\n"), "Ignoring unsupported TOP LEVEL block at line 7: FOO"),
            (format!("{asg}FOO(a)\n{{\nx,\ny\n}}\n\n"), "Ignoring unsupported TOP LEVEL block at line 6: FOO"),
            (format!("{asg}FOO(a) {{x}}\n{{\na,\nb\n}}\n\n\n"), "Ignoring unsupported TOP LEVEL nested block at line 6: FOO"),
            (format!("{asg}FOO(a)\n\n\n"), "Ignoring unsupported TOP LEVEL bare block at line 5: FOO"),
            (format!("FOO(a)\n\n\n{asg}"), "Ignoring unsupported TOP LEVEL bare block at line 4: FOO"),
            (format!("\"FOO\"(a)\n\n{asg}"), "Ignoring unsupported TOP LEVEL bare block at line 3: FOO"),
            // Only the first of a parse prints.
            (format!("FOO(a)\nBAR(b)\n{asg}"), "Ignoring unsupported TOP LEVEL bare block at line 2: FOO"),
            (
                "ASG(DEFAULT) {\n  RULE(1, READ)\n  RULE(2, EXECUTE)\n}\n".to_string(),
                "Ignoring RULE that contains an unsupported keyword at line 3: EXECUTE",
            ),
            (
                "ASG(DEFAULT) { RULE(1, \nFROB\n\n)\n}\n".to_string(),
                "Ignoring RULE that contains an unsupported keyword at line 2: FROB",
            ),
            (
                "ASG(DEFAULT) { RULE(1, READ)\n{\nFOO(a)\n{\nBAR\n}\n\n}\n}\n".to_string(),
                "Ignoring RULE that contains an unsupported keyword at line 6: FOO",
            ),
            (
                "ASG(DEFAULT) { RULE(1, READ) {\nFOO(a)\n{\nx,\ny\n}\n\n}\n}\n".to_string(),
                "Ignoring RULE that contains an unsupported keyword at line 6: FOO",
            ),
            (
                "ASG(DEFAULT) { RULE(1, READ)\n{\nFOO(a)\n\n\n}\n}\n".to_string(),
                "Ignoring RULE that contains an unsupported keyword at line 6: FOO",
            ),
            (
                "ASG(DEFAULT) { RULE(1, READ) {\nFOO(a) # c\n# c\n\n}\n}\nBAR(b)\n".to_string(),
                "Ignoring RULE that contains an unsupported keyword at line 5: FOO",
            ),
        ] {
            assert_eq!(warned(&acf), Some(format!("WARNING {want}")), "{acf}");
        }
        assert_eq!(warned(asg), None);
    }

    /// A delivered reading with no alarm.
    /// What the `INP*` watcher delivers for `links`: each declared input whose
    /// link names one of them takes its reading, then the change signal fires.
    fn serve(cfg: &AccessSecurityConfig, links: &[(&str, Option<InpReading>)]) {
        let watch = InpWatch::of(cfg);
        for (asg, index, ix) in &watch.order {
            let target = &watch.targets[*ix];
            for (link, reading) in links {
                if inp_link_target(link) == (target.record.as_str(), target.field.as_str()) {
                    let event = reading.map_or(InpEvent::Disconnected, InpEvent::Reading);
                    cfg.asg[asg].deliver([(*index, event)]);
                }
            }
        }
        notify_asg_field_changed();
    }

    fn ok(value: f64) -> InpReading {
        InpReading {
            value,
            severity: crate::server::record::AlarmSeverity::NoAlarm,
        }
    }

    fn bs(items: &[&str]) -> Vec<Vec<u8>> {
        items.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

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
        assert_eq!(
            config.uag[b"admins".as_slice()],
            [b"user1".to_vec(), b"user2".to_vec()]
        );
        assert_eq!(
            config.hag.get(b"operators".as_slice()).unwrap(),
            &bs(&["host1", "host2"])
        );
        assert!(config.asg.contains_key(b"DEFAULT".as_slice()));
        assert_eq!(config.asg[b"DEFAULT".as_slice()].rules.len(), 2);
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
        assert_eq!(
            config.uag[b"ops".as_slice()],
            [b"alice".to_vec(), b"bob".to_vec()]
        );
        assert_eq!(config.hag[b"lab".as_slice()], bs(&["lab-pc1.invalid"]));
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
            config.hag[b"local".as_slice()],
            bs(&["localhost"]),
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
            config.hag[b"local".as_slice()],
            bs(&["127.0.0.1"]),
            "C resolves the host to its IP under asCheckClientIP"
        );
        assert_eq!(
            config.check_access("DEFAULT", "127.0.0.1", "alice"),
            AccessLevel::ReadWrite,
            "the peer IP matches the resolved HAG"
        );
    }

    /// `asHagAddHost` stores what `aToIPAddr` makes of the host
    /// (asLibRoutines.c:1243-1260): `%u` is decimal, a port is dropped, and a
    /// bare number is a raw address.
    #[test]
    fn hag_host_under_as_check_client_ip_is_c_a_to_ip_addr() {
        let _guard = lock_as_check_client_ip();
        set_as_check_client_ip(true);
        let config = parse_acf("HAG(lab) { \"010.0.0.1\", \"1.2.3.4:5\", \"16909061\" }\n");
        set_as_check_client_ip(false);
        assert_eq!(
            config.unwrap().hag[b"lab".as_slice()],
            bs(&["10.0.0.1", "1.2.3.4", "1.2.3.5"])
        );
    }

    /// C `asHagAddHost` under `asCheckClientIP = 1` does not abort on a
    /// name it cannot resolve: it logs and stores `unresolved:<host>`, a
    /// sentinel that simply never matches.
    #[test]
    fn hag_unresolvable_under_as_check_client_ip_becomes_sentinel() {
        use crate::runtime::log::{errlog_add_listener, errlog_flush, errlog_remove_listener};
        let _guard = lock_as_check_client_ip();
        let heard = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = heard.clone();
        let id = errlog_add_listener(move |m| {
            if m.contains("lab-pc1.invalid") {
                sink.lock().unwrap().push(m.to_string());
            }
        });
        set_as_check_client_ip(true);
        let config = parse_acf("HAG(lab) { lab-pc1.invalid }\n").unwrap();
        // The port's own refresh re-resolves without C's line.
        assert!(config.with_refreshed_hags().is_none());
        set_as_check_client_ip(false);
        errlog_flush();
        errlog_remove_listener(id);
        assert_eq!(
            config.hag[b"lab".as_slice()],
            bs(&["unresolved:lab-pc1.invalid"])
        );
        // softIoc R7.0.10, `var asCheckClientIP 1`.
        assert_eq!(
            *heard.lock().unwrap(),
            ["ACF: Unable to resolve host 'lab-pc1.invalid'\n"]
        );
    }

    /// C `asInitialize` hashes every UAG user, then every HAG host, and
    /// errlogs each repeat within one group; a failed parse never gets there.
    #[test]
    fn duplicated_members_are_c_s_errlog_lines() {
        use crate::runtime::log::{errlog_add_listener, errlog_flush, errlog_remove_listener};
        let _guard = lock_as_check_client_ip();
        let heard = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = heard.clone();
        let id = errlog_add_listener(move |m| {
            if m.contains("dupuser-q") || m.contains("duphost-q") {
                sink.lock().unwrap().push(m.to_string());
            }
        });
        let acf = "HAG(h) {dupHost-q, DUPHOST-Q, other}\n\
                   UAG(u) {dupuser-q, b, dupuser-q, dupuser-q}\n\
                   UAG(v) {dupuser-q}\n\
                   HAG(g) {duphost-q}\n\
                   ASG(DEFAULT) { RULE(1,READ) }\n";
        parse_acf(acf).unwrap();
        parse_acf(format!("{acf}ASG(X) {{\n")).unwrap_err();
        errlog_flush();
        errlog_remove_listener(id);
        // softIoc R7.0.10, `asInit` of each file.
        assert_eq!(
            *heard.lock().unwrap(),
            [
                "Duplicated user 'dupuser-q' in UAG 'u'\n",
                "Duplicated user 'dupuser-q' in UAG 'u'\n",
                "Duplicated host 'duphost-q' in HAG 'h'\n",
            ]
        );
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
        let entries = &config.hag[b"quarantine".as_slice()];
        assert_eq!(
            entries.len(),
            2,
            "literal entries preserved verbatim; no resolved IPs appended"
        );
        assert_eq!(entries[0], b"gone.invalid");
        assert_eq!(entries[1], b"alive.invalid");
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
        assert_eq!(config.hag_raw[b"local".as_slice()], bs(&["localhost"]));

        // Simulate DNS moving after load: the stored quad no longer
        // matches what `localhost` resolves to.
        config.hag.insert(b"local".to_vec(), bs(&["192.0.2.1"]));

        let refreshed = config
            .with_refreshed_hags()
            .expect("a moved resolution must produce a refreshed config");
        set_as_check_client_ip(false);
        assert_eq!(refreshed.hag[b"local".as_slice()], bs(&["127.0.0.1"]));
        assert_eq!(
            refreshed.hag_raw[b"local".as_slice()],
            bs(&["localhost"]),
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

    /// A HAG refresh tick awaited on cbMedium leaves cbMedium free while its
    /// lookups block, and publishes what they return.
    #[test]
    fn a_hag_refresh_tick_leaves_cb_medium_free() {
        use crate::runtime::task::{CallbackPriority, spawn_background};
        use std::sync::mpsc::channel;
        use std::time::Duration;
        let _guard = lock_as_check_client_ip();
        set_as_check_client_ip(true);
        let cell = new_acf_cell(Some(parse_acf("ASG(DEFAULT) { RULE(1, READ) }\n").unwrap()));
        let weak = std::sync::Arc::downgrade(&cell.0);
        let (release, held) = channel::<()>();
        let (ticked, tick) = channel();
        spawn_background(CallbackPriority::Medium, async move {
            let alive = refresh_hags_once(&weak, move |c| {
                let _ = held.recv();
                let mut moved = c.clone();
                moved.hag.insert(b"h".to_vec(), bs(&["127.0.0.1"]));
                Some(moved)
            })
            .await;
            let _ = ticked.send(alive);
        });
        // Queued only once the tick is blocked in its lookup.
        std::thread::sleep(Duration::from_millis(200));
        let (ran, beside) = channel();
        spawn_background(CallbackPriority::Medium, async move {
            let _ = ran.send(());
        });
        let free = beside.recv_timeout(fact_budget::FACT_BUDGET).is_ok();
        release.send(()).unwrap();
        let alive = tick.recv_timeout(fact_budget::FACT_BUDGET).unwrap();
        set_as_check_client_ip(false);
        assert!(free, "the HAG lookup parked cbMedium");
        assert!(alive);
        assert!(cell.load_full().unwrap().hag.contains_key(b"h".as_slice()));
    }

    /// A reload that lands while a HAG lookup is blocked wins: the refresh,
    /// derived from the policy the reload replaced, is dropped.
    #[test]
    fn a_reload_during_a_hag_lookup_is_not_overwritten() {
        use crate::runtime::task::{CallbackPriority, spawn_background};
        use std::sync::mpsc::channel;
        use std::time::Duration;
        let _guard = lock_as_check_client_ip();
        set_as_check_client_ip(true);
        let cell = new_acf_cell(Some(parse_acf("ASG(DEFAULT) { RULE(1, READ) }\n").unwrap()));
        let weak = std::sync::Arc::downgrade(&cell.0);
        let (looking, in_lookup) = channel::<()>();
        let (release, held) = channel::<()>();
        let (ticked, tick) = channel();
        spawn_background(CallbackPriority::Medium, async move {
            let alive = refresh_hags_once(&weak, move |c| {
                let _ = looking.send(());
                let _ = held.recv_timeout(Duration::from_secs(5));
                let mut moved = c.clone();
                moved.hag.insert(b"h".to_vec(), bs(&["127.0.0.1"]));
                Some(moved)
            })
            .await;
            let _ = ticked.send(alive);
        });
        in_lookup.recv_timeout(fact_budget::FACT_BUDGET).unwrap();
        let reload = std::sync::Arc::new(parse_acf("ASG(RELOADED) { RULE(1, READ) }\n").unwrap());
        cell.store(Some(reload.clone()));
        release.send(()).unwrap();
        let alive = tick.recv_timeout(fact_budget::FACT_BUDGET).unwrap();
        set_as_check_client_ip(false);
        assert!(alive);
        assert!(
            std::sync::Arc::ptr_eq(&cell.load_full().unwrap(), &reload),
            "a refresh derived from the replaced policy overwrote the reload"
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
        assert_eq!(config.hag_raw[b"local".as_slice()], bs(&["LocalHost"]));
        assert_eq!(config.hag[b"local".as_slice()], bs(&["localhost"]));
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

    /// C `asCompute` compares the user with `strcmp`: bytes, so a non-UTF-8
    /// user matches only its own bytes, never its U+FFFD rendering.
    #[test]
    fn a_uag_member_matches_the_user_s_bytes() {
        let _guard = lock_as_check_client_ip();
        let acf = "UAG(ops) { \"op\u{fffd}\", \"op\u{e9}\" }\nASG(G) {\n    RULE(1, WRITE) { UAG(ops) }\n}\n";
        let config = parse_acf(acf).unwrap();
        assert_eq!(
            config.check_access("G", "h", b"op\xff"),
            AccessLevel::NoAccess
        );
        assert_eq!(
            config.check_access("G", "h", "op\u{e9}"),
            AccessLevel::ReadWrite
        );
    }

    /// asLib_lex.l whitespace is `[ \t\r]` and newline: a form feed is the
    /// `.` rule wherever it stands, as `ascheck` reports it.
    #[test]
    fn a_form_feed_is_an_invalid_character_not_whitespace() {
        let _guard = lock_as_check_client_ip();
        for acf in [
            b"UAG(u) {a,\x0cb}\n".as_slice(),
            b"ASG(DEFAULT) {\x0c RULE(1,READ) }\n",
            b"UAG(a\x0c) {x}\n",
            b"\x0c\n",
        ] {
            let err = parse_acf(acf).unwrap_err().to_string();
            assert!(
                err.contains("Invalid character 0x0c at line 1"),
                "{acf:?}: {err}"
            );
        }
        // Inside a quoted string or a comment it is the lexer's to keep.
        let _ = parse_acf(b"# \x0c\nUAG(u) {\"a\x0cb\"}\n").unwrap();
    }

    /// `ascheck` on these files prints `0xff`, `0x80` and `0x7f`: e_flex's
    /// 8-bit `yytext` is unsigned, so no byte sign-extends to `0xffffff..`.
    #[test]
    fn a_high_byte_is_reported_as_ascheck_prints_it() {
        let _guard = lock_as_check_client_ip();
        for (acf, want) in [
            (b"UAG(u) {a,\xffb}\n".as_slice(), "Invalid character 0xff"),
            (b"UAG(u) {a,\x80b}\n", "Invalid character 0x80"),
            (b"UAG(u) {a,\x7fb}\n", "Invalid character 0x7f"),
            (b"\xff\n", "Invalid character 0xff"),
        ] {
            let err = parse_acf(acf).unwrap_err().to_string();
            assert!(err.contains(&format!("{want} at line 1")), "{acf:?}: {err}");
        }
    }

    #[test]
    fn an_empty_quoted_member_is_kept_and_matches_an_empty_user() {
        let _guard = lock_as_check_client_ip();
        let acf = "UAG(u) { a, \"\", b }\nHAG(h) { \"\" }\n\
                   ASG(G) {\n    RULE(1, WRITE) { UAG(u) }\n}\n";
        let config = parse_acf(acf).unwrap();
        assert_eq!(config.uag[b"u".as_slice()], bs(&["a", "", "b"]));
        assert_eq!(config.hag_raw[b"h".as_slice()], bs(&[""]));
        assert_eq!(config.check_access("G", "h", ""), AccessLevel::ReadWrite);
    }

    /// asLib.y:57-63 takes a UAG, HAG or ASG head with no body; `ascheck`
    /// accepts the file.
    #[test]
    fn a_bare_group_head_is_an_empty_group() {
        let _guard = lock_as_check_client_ip();
        let config = parse_acf("UAG(e)\nHAG(h)\nASG(G)\nASG(DEFAULT) { RULE(1,READ) }\n").unwrap();
        assert!(config.uag[b"e".as_slice()].is_empty());
        assert!(config.hag[b"h".as_slice()].is_empty());
        assert!(config.asg[b"G".as_slice()].rules.is_empty());
        assert_eq!(config.check_access("G", "h", "u"), AccessLevel::NoAccess);
        assert_eq!(config.check_access("DEFAULT", "h", "u"), AccessLevel::Read);
    }

    #[test]
    fn a_non_utf8_uag_member_keeps_its_bytes() {
        let _guard = lock_as_check_client_ip();
        let acf = b"UAG(ops) { \"op\xff\" }\nASG(G) {\n    RULE(1, WRITE) { UAG(ops) }\n}\n";
        let config = parse_acf(acf).unwrap();
        assert_eq!(config.uag[b"ops".as_slice()], [b"op\xff".to_vec()]);
        assert_eq!(
            config.check_access("G", "h", b"op\xff"),
            AccessLevel::ReadWrite
        );
        assert_eq!(
            config.check_access("G", "h", "op\u{fffd}"),
            AccessLevel::NoAccess
        );
        // Unquoted, the byte is outside C's name class.
        assert!(parse_acf(b"UAG(ops) { op\xff }\n").is_err());
    }

    /// C keys groups and compares HAG hosts by their bytes, lowercasing only
    /// ASCII letters (`asLibRoutines.c:1233-1236`, `:378-381`).
    #[test]
    fn non_utf8_group_names_and_hosts_keep_their_bytes() {
        let _guard = lock_as_check_client_ip();
        let acf = b"HAG(\"h\xff\") { \"Lab\xc0pc\" }\n\
                    ASG(\"g\xff\") {\n    RULE(1, WRITE) { HAG(\"h\xff\") }\n}\n";
        let config = parse_acf(acf).unwrap();
        assert_eq!(config.hag[b"h\xff".as_slice()], [b"lab\xc0pc".to_vec()]);
        assert_eq!(
            config.check_access(b"g\xff", b"LAB\xc0PC", "u"),
            AccessLevel::ReadWrite
        );
        assert_eq!(
            config.check_access("g\u{fffd}", b"lab\xc0pc", "u"),
            AccessLevel::NoAccess
        );
        assert_eq!(
            config.check_access(b"g\xff", "lab\u{fffd}pc", "u"),
            AccessLevel::NoAccess
        );
    }

    /// asLib_lex.l keeps a quoted token's `\\x` pairs verbatim (`yytext+1`),
    /// in every position a quoted token can take.
    #[test]
    fn quoted_tokens_keep_their_escapes_verbatim() {
        let _guard = lock_as_check_client_ip();
        let acf = br#"UAG("u\"g") { "op\"x", "a\\b" }
HAG(h) { "lab\-pc" }
ASG(G) {
    RULE(1, WRITE) { UAG("u\"g") }
    RULE(1, READ) { METHOD("c\"a") }
}
"#;
        let config = parse_acf(acf).unwrap();
        assert_eq!(
            config.uag[br#"u\"g"#.as_slice()],
            [br#"op\"x"#.to_vec(), br#"a\\b"#.to_vec()]
        );
        assert_eq!(config.hag[b"h".as_slice()], [br"lab\-pc".to_vec()]);
        let rule = &config.asg[b"G".as_slice()].rules[1];
        assert_eq!(rule.method, [br#"c\"a"#.to_vec()]);
        assert_eq!(
            config.check_access("G", "h", br#"op\"x"#),
            AccessLevel::ReadWrite
        );
        assert_eq!(
            config.check_access("G", "h", "op\"x"),
            AccessLevel::NoAccess
        );
        // `\` before a newline is no escape: the string never closes.
        assert!(parse_acf(b"UAG(\"a\\\n\") { x }\n").is_err());
        assert!(parse_acf(b"UAG(a) { \"x\ny\" }\n").is_err());
    }

    /// `ascheck` (R7.0.10) on each list: one token per item, one comma
    /// between items, and only `generic_head` may be empty.
    #[test]
    fn list_items_are_single_tokens_as_ascheck_reads_them() {
        for (acf, accepted) in [
            ("UAG(u) { a b }", false),
            ("UAG(u) { a,,b }", false),
            ("UAG(u) { a, }", false),
            ("UAG(u) { }", false),
            ("UAG(u) { \"a\" \"b\" }", false),
            ("UAG(u) { a # c\n b }", false),
            ("UAG(u) { a , \"b\" }", true),
            ("ASG(G) { RULE(1,READ) { METHOD(ca x509) } }", false),
            ("ASG(G) { RULE(1,READ) { METHOD() } }", true),
            ("ASG(G) { RULE(1,READ) { METHOD(\"ca\", x509) } }", true),
        ] {
            let parsed = parse_acf(format!("{acf}\n").as_bytes());
            assert_eq!(parsed.is_ok(), accepted, "{acf}: {parsed:?}");
        }
    }

    /// asLib.y `rule_uag_list` / `rule_hag_list`: a rule names any number of
    /// groups in one clause, which `ascheck` accepts.
    #[test]
    fn a_rule_names_several_groups_in_one_clause() {
        let config = parse_acf(
            b"UAG(a) {x}\nUAG(b) {y}\nHAG(h) {p}\nHAG(k) {q}\n\
              ASG(G) { RULE(1,WRITE) { UAG(a, \"b\") HAG(h,k) } }\n",
        )
        .unwrap();
        let rule = &config.asg[b"G".as_slice()].rules[0];
        assert_eq!(rule.uag, [b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(rule.hag, [b"h".to_vec(), b"k".to_vec()]);
        assert_eq!(config.check_access("G", "q", "y"), AccessLevel::ReadWrite);
        assert!(parse_acf(b"UAG(a) {x}\nASG(G) { RULE(1,READ) { UAG() } }\n").is_err());
        assert!(parse_acf(b"UAG(a) {x}\nASG(G) { RULE(1,READ) { UAG(a,) } }\n").is_err());
    }

    /// `ascheck` (R7.0.10) refuses a rule naming a group the file has not
    /// defined above it, forward references included.
    #[test]
    fn a_rule_naming_an_undefined_group_fails_the_load() {
        for (acf, error) in [
            (
                "ASG(G) { RULE(1,READ) { UAG(a) } }\nUAG(a) {x}\n",
                "No User Access Group named 'a' defined",
            ),
            (
                "ASG(G) { RULE(1,READ) { HAG(nosuch) } }\n",
                "No Host Access Group named 'nosuch' defined",
            ),
            (
                "UAG(a) {x}\nASG(G) { RULE(1,READ) { UAG(a, zz) } }\n",
                "No User Access Group named 'zz' defined",
            ),
        ] {
            let err = parse_acf(acf.as_bytes()).unwrap_err().to_string();
            assert!(err.contains(error), "{acf}: {err}");
        }
        assert!(parse_acf(b"HAG(h)\nASG(G) { RULE(1,READ) { HAG(h) } }\n").is_ok());
    }

    /// `ascheck` (R7.0.10): a name is one `tokenSTRING`, and a bare run
    /// the lexer reads as a number is not one.
    #[test]
    fn a_name_is_one_string_token_as_ascheck_reads_it() {
        for (acf, accepted) in [
            ("UAG(a,b) {x}", false),
            ("UAG() {x}", false),
            ("UAG(\"5\") {\"7\"}", true),
            ("ASG(G) { INPA(5) }", false),
            ("ASG(G) { INPA(-1.5e3) }", false),
            ("ASG(G) { INPA(-1.5e3x) }", true),
            ("UAG(u) { 5 }", false),
            ("UAG(u) { 1.5 }", false),
            ("UAG(u) { +7 }", false),
            ("UAG(u) { .5 }", false),
            ("UAG(u) { 5a }", true),
            ("UAG(u) { 5. }", true),
            ("UAG(u) { 1e5 }", true),
            ("UAG(u) { - }", true),
            ("HAG(h) { 10.0.0.1 }", true),
            ("ASG(G) { RULE(1,READ) { FOO(5, 1.5) } }", true),
        ] {
            let parsed = parse_acf(format!("{acf}\n").as_bytes());
            assert_eq!(parsed.is_ok(), accepted, "{acf}: {parsed:?}");
        }
    }

    /// `ascheck` (R7.0.10) on ASG and RULE bodies: no separator between
    /// items, and an unsupported keyword takes any `generic_head` and an
    /// optional nested block, which disables the rule.
    #[test]
    fn rule_and_asg_bodies_parse_as_ascheck_reads_them() {
        for (acf, accepted) in [
            (
                "UAG(a) {x}\nASG(G) { RULE(1,READ) { UAG(a), UAG(a) } }",
                false,
            ),
            ("UAG(a) {x}\nASG(G) { RULE(1,READ) { UAG(a); } }", false),
            ("ASG(G) { RULE(1,READ) { , } }", false),
            ("ASG(G) { RULE(1,READ) { FOO(a,b) } }", true),
            ("ASG(G) { RULE(1,READ) { FOO(a) { b } } }", true),
            ("ASG(G) { RULE(1,READ) { FOO } }", false),
            ("ASG(G) { RULE(1,READ) { FOO() } }", true),
            ("ASG(G) { RULE(1,READ) { \"x\"(a) } }", true),
            ("ASG(G) { RULE(1,READ) { FOO(a) { BAR(c) { d } } } }", true),
            ("ASG(G) { RULE(1,READ), RULE(1,WRITE) }", false),
            ("ASG(G) { , }", false),
            ("ASG(G) { FOO(a) }", false),
            ("ASG(G) { }", false),
            ("ASG(G) { RULE(1,READ) { } }", false),
            ("ASG(G) { RULE(1,READ) { FOO(a) { } } }", false),
            ("ASG(G) { RULE(1,READ) { FOO(a) { b, c } } }", true),
            ("ASG(G) { RULE(1,READ) { FOO(a) { b c } } }", false),
            ("ASG(G) { RULE(1,READ) { FOO(a) { B(c) d } } }", false),
            ("ASG(G) { RULE(1,READ) { FOO(a) { B(c) D() } } }", true),
            ("ASG(G) { RULE(1,READ) { FOO(a) { 5(b) } } }", false),
            ("ASG(G) { RULE(1,READ) { FOO(a) { 5, 6 } } }", true),
            ("ASG(G) { RULE(1,READ) { RULE(a) } }", true),
            ("UAG(a) {x}\nASG(G) { RULE(1,READ) { \"UAG\"(a) } }", true),
        ] {
            let parsed = parse_acf(format!("{acf}\n").as_bytes());
            assert_eq!(parsed.is_ok(), accepted, "{acf}: {parsed:?}");
            if let Ok(config) = parsed {
                assert!(config.asg[b"G".as_slice()].rules[0].ignore, "{acf}");
            }
        }
    }

    /// `ascheck` (R7.0.10) on RULE heads: both commas and the `)` are
    /// required, the level is an integer token kept as C's `(int)`, and the
    /// access and log option are strings, quoted or not.
    #[test]
    fn rule_heads_parse_as_ascheck_reads_them() {
        for (head, accepted) in [
            ("RULE(1 READ)", false),
            ("RULE(1,READ { UAG(a) }", false),
            ("RULE(1.5,READ)", false),
            ("RULE(1,\"READ\")", true),
            ("RULE(1,READ,\"TRAPWRITE\")", true),
            ("RULE(1,5)", false),
            ("RULE(1)", false),
            ("RULE(1,READ,)", false),
            ("RULE(01,READ)", true),
            ("RULE(+1,READ)", true),
            ("RULE(256,READ)", true),
            ("RULE(99999999999,READ)", true),
            ("RULE(1x,READ)", false),
            ("RULE(1,write)", true),
            ("RULE(\"1\",READ)", false),
            ("RULE(1,READ,TRAPWRITE,x)", false),
        ] {
            let parsed = parse_acf(format!("UAG(a) {{x}}\nASG(G) {{ {head} }}\n").as_bytes());
            assert_eq!(parsed.is_ok(), accepted, "{head}: {parsed:?}");
        }
        let level = |head: &str| {
            parse_acf(format!("ASG(G) {{ {head} }}\n").as_bytes())
                .unwrap()
                .asg[b"G".as_slice()]
            .rules[0]
                .level
        };
        assert_eq!(level("RULE(256,READ)"), 256);
        assert_eq!(level("RULE(99999999999,READ)"), 99999999999_i64 as i32);
    }

    /// A quoted METHOD/AUTHORITY item is one token, as a UAG member is: its
    /// spaces are bytes of it, not padding to trim.
    #[test]
    fn method_and_authority_items_keep_their_quoted_bytes() {
        let config = parse_acf(
            b"ASG(G) { RULE(1, READ) { METHOD(\" ca \", x509) AUTHORITY(\" Root \") } }\n",
        )
        .unwrap();
        let rule = &config.asg[b"G".as_slice()].rules[0];
        assert_eq!(rule.method, [b" ca ".to_vec(), b"x509".to_vec()]);
        assert_eq!(rule.authority, [b" Root ".to_vec()]);
    }

    /// No record has a name that is not UTF-8, so such a link is a bad input;
    /// a byte outside UTF-8 is no CALC token.
    #[test]
    fn a_non_utf8_inp_link_is_bad_and_a_non_utf8_calc_is_refused() {
        let config = parse_acf(b"ASG(G) {\n    INPA(\"r\xff\")\n    RULE(1, WRITE)\n}\n").unwrap();
        assert_eq!(config.asg[b"G".as_slice()].inp[0].link, b"r\xff");
        assert_eq!(config.asg[b"G".as_slice()].inp_bad(), 1);
        assert!(InpWatch::of(&config).targets.is_empty());
        assert!(parse_acf(b"ASG(G) {\n    RULE(1, WRITE) { CALC(\"A\xff\") }\n}\n").is_err());
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

    /// The expected text is softIoc R7.0.10 `asdbdump` and `asprules` on
    /// this ACF with `okrec` at 1 and no `missing` record, MEMBERLIST cut.
    /// `asDumpFP` prints members raw; only `asDumpUagFP`/`asDumpHagFP` quote.
    #[test]
    fn dumps_match_c_as_dump_fp_and_as_dump_rules_fp() {
        let cfg = parse_acf(
            b"UAG(zops) { bob, \"role/op\" }
UAG(aempty)
HAG(lab) { pc1, pc2 }
ASG(ZZ) {
    INPA(okrec)
    INPB(missing)
    RULE(1, WRITE, TRAPWRITE) { UAG(zops, aempty) HAG(lab) }
    RULE(0, READ)
    RULE(1, WRITE) { FOO(x) }
    RULE(1, WRITE, NOTRAPWRITE) { CALC(\"A=1\") }
}
ASG(BARE)
ASG(DEFAULT) { RULE(1, READ) }
",
        )
        .unwrap();
        serve(&cfg, &[("okrec", Some(ok(1.0)))]);
        let zz_rules = "\tRULE(1,WRITE,TRAPWRITE) {
\t\tUAG(zops,aempty)
\t\tHAG(lab)
\t}
\tRULE(0,READ,NOTRAPWRITE)
\tRULE(1,WRITE,NOTRAPWRITE) {
\t\tCALC(\"A=1\") result=TRUE
\t}
}
";
        let head = "ASG(BARE)\nASG(DEFAULT) {\n\tRULE(1,READ,NOTRAPWRITE)\n}\nASG(ZZ) {\n";
        assert_eq!(
            String::from_utf8(cfg.dump_report()).unwrap(),
            format!(
                "UAG(aempty)\nUAG(zops) {{bob,role/op}}\nHAG(lab) {{pc1,pc2}}\n{head}\
                 \tINPA(okrec)   VALID value=1.000000\n\
                 \tINPB(missing) INVALID value=0.000000\n{zz_rules}"
            )
        );
        assert_eq!(
            String::from_utf8(cfg.dump_rules(None)).unwrap(),
            format!(
                "{head}\tINPA(okrec) value=1.000000\n\
                 \tINPB(missing) INVALID value=0.000000\n{zz_rules}"
            )
        );
        assert_eq!(cfg.dump_rules(Some(b"NONE")), b"");
        // The CALC result is the published one: FALSE until A delivers.
        let fresh =
            parse_acf(b"ASG(ZZ) { INPA(okrec) RULE(1, WRITE) { CALC(\"A=1\") } }\n").unwrap();
        let off = String::from_utf8(fresh.dump_rules(Some(b"ZZ"))).unwrap();
        assert!(off.contains("CALC(\"A=1\") result=FALSE"), "{off}");
    }

    /// softIoc R7.0.10 on an ACF with neither UAG nor ASG lines.
    #[test]
    fn dump_report_names_an_empty_list_as_c_does() {
        let cfg = parse_acf(b"HAG(h) { a }\n").unwrap();
        assert_eq!(
            String::from_utf8(cfg.dump_report()).unwrap(),
            "No UAGs\nHAG(h) {a}\nASG(DEFAULT)\n"
        );
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
        let asg = &config.asg[b"SECURE".as_slice()];
        assert_eq!(asg.rules.len(), 2);
        assert_eq!(asg.rules[0].method, bs(&["ca", "x509"]));
        assert_eq!(asg.rules[0].authority, bs(&["ANL CA"]));
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
        let config = parse_acf("ASG(LOCKED)").unwrap();
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
        assert!(config.asg.contains_key(b"DEFAULT".as_slice()));
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

    /// A grammar error is said at the lookahead's line, so at the end of the
    /// input it is the last line: C names where it found out, not where the
    /// unclosed construct opened. `ascheck` (R7.0.10) on the same files.
    #[test]
    fn a_syntax_error_names_the_lookaheads_line() {
        let tail = "\n\n\n\n\n";
        for (acf, line) in [
            ("UAG(a) { x }\n\nUAG(my group) { b }\n".to_string(), 3),
            ("UAG(a) { x }\n}\n".to_string(), 2),
            (format!("UAG(a{tail}"), 6),
            (format!("UAG(a) {{ x{tail}"), 6),
            (format!("ASG(G) {{ INPA(\"g\"){tail}"), 6),
            (format!("BOGUS{tail}"), 6),
        ] {
            let want = vec![format!("ERROR syntax error at line {line}")];
            assert_eq!(said(acf.as_bytes()), (want, true), "{acf:?}");
        }
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
            load: LoadId::default(),
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

    /// A misspelled access keyword adds no rule and its clauses act on none.
    /// C hands them to the last rule made anywhere above it (CBUG-Q1): softIoc
    /// R7.0.10 `asprules` gives the first case `RULE(1,READ,NOTRAPWRITE) {
    /// UAG(u) }`, the second `RULE(1,READ,TRAPWRITE)`, and disables the READ
    /// rule in the third. The expected text is in `asprules` form with the
    /// file's other rules as written.
    #[test]
    fn an_unsupported_access_keywords_clauses_act_on_no_rule() {
        let read = "ASG(DEFAULT)\nASG(g) {\n\tRULE(1,READ,NOTRAPWRITE)\n}\n";
        for (acf, asprules) in [
            ("UAG(u){x}\nASG(g){RULE(1,READ) RULE(1,FOO){UAG(u)}}", read),
            ("ASG(g){RULE(1,READ) RULE(1,FOO,TRAPWRITE)}", read),
            ("ASG(g){RULE(1,READ) RULE(1,FOO){FOO(x)}}", read),
            ("ASG(g){RULE(1,READ) RULE(1,FOO){CALC(\"A\")}}", read),
            ("ASG(g){RULE(1,READ) RULE(1,FOO){METHOD(\"x\")}}", read),
            (
                "UAG(u){x}\nASG(A){RULE(1,READ)}\nASG(B){RULE(1,FOO){UAG(u)}}",
                "ASG(A) {\n\tRULE(1,READ,NOTRAPWRITE)\n}\nASG(B)\nASG(DEFAULT)\n",
            ),
            (
                "ASG(g){RULE(1,READ)}\nASG(h){RULE(1,FOO){UAG(nope)}}",
                "ASG(DEFAULT)\nASG(g) {\n\tRULE(1,READ,NOTRAPWRITE)\n}\nASG(h)\n",
            ),
            (
                "ASG(g){RULE(1,READ)}\nASG(h){RULE(1,FOO){CALC(\"A>\")}}",
                "ASG(DEFAULT)\nASG(g) {\n\tRULE(1,READ,NOTRAPWRITE)\n}\nASG(h)\n",
            ),
            ("ASG(g){RULE(1,FOO){UAG(nope)}}", "ASG(DEFAULT)\nASG(g)\n"),
            (
                "ASG(g){RULE(1,FOO){CALC(\"A>\")}}",
                "ASG(DEFAULT)\nASG(g)\n",
            ),
            ("ASG(g){RULE(1,FOO){FOO(x)}}", "ASG(DEFAULT)\nASG(g)\n"),
            (
                "ASG(g){RULE(1,FOO){METHOD(\"x\")}}",
                "ASG(DEFAULT)\nASG(g)\n",
            ),
            (
                "ASG(DEFAULT){RULE(1,FOO)}\nASG(DEFAULT){RULE(1,READ)}",
                "ASG(DEFAULT) {\n\tRULE(1,READ,NOTRAPWRITE)\n}\n",
            ),
        ] {
            let config =
                parse_acf(format!("{acf}\n").as_bytes()).unwrap_or_else(|e| panic!("{acf}: {e}"));
            let dump = config.dump_rules(None);
            assert_eq!(String::from_utf8(dump).unwrap(), asprules, "{acf}");
        }
        // The READ rule still grants a stranger read.
        let config = parse_acf("UAG(u){x}\nASG(g){RULE(1,READ) RULE(1,FOO){UAG(u)}}").unwrap();
        assert_eq!(config.check_access("g", "h", "stranger"), AccessLevel::Read);
        // The log option is checked whatever the access keyword, as in C.
        assert!(parse_acf("ASG(g){RULE(1,FOO,BOGUS)}\n").is_err());
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
        assert!(config.asg.contains_key(b"DEFAULT".as_slice()));
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

    /// `ascheck` (R7.0.10) reads METHOD and AUTHORITY as C base's
    /// `rule_generic_block_elem`: a `generic_head`, then an optional
    /// `generic_block`, which the rule does not keep.
    #[test]
    fn method_and_authority_take_a_generic_block_as_ascheck_does() {
        for (acf, method, authority) in [
            (
                "ASG(g){RULE(1,READ){METHOD(\"a\"){b,c}}}",
                vec!["a"],
                vec![],
            ),
            (
                "ASG(g){RULE(1,WRITE){METHOD(\"ca\"){x}}}",
                vec!["ca"],
                vec![],
            ),
            (
                "ASG(g){RULE(1,READ){AUTHORITY(\"a\"){b}}}",
                vec![],
                vec!["a"],
            ),
            (
                "ASG(g){RULE(1,READ){METHOD(\"a\"){X(y) Z()}}}",
                vec!["a"],
                vec![],
            ),
        ] {
            let config =
                parse_acf(format!("{acf}\n").as_bytes()).unwrap_or_else(|e| panic!("{acf}: {e}"));
            let rule = &config.asg[b"g".as_slice()].rules[0];
            assert_eq!(rule.method, bs(&method), "{acf}");
            assert_eq!(rule.authority, bs(&authority), "{acf}");
        }
        for acf in [
            "ASG(g){RULE(1,READ){METHOD}}",
            "ASG(g){RULE(1,READ){METHOD(\"a\"){}}}",
        ] {
            assert!(parse_acf(format!("{acf}\n").as_bytes()).is_err(), "{acf}");
        }
    }

    /// `ascheck` (R7.0.10): `UAG`, `HAG`, `ASG`, `RULE`, `CALC` and `INP<A-U>`
    /// lex as keywords, so every place asLib.y wants a `tokenSTRING` refuses
    /// them with a syntax error, while a generic element or a quoted string
    /// takes them.
    #[test]
    fn keywords_are_refused_where_a_name_is_wanted_as_ascheck_does() {
        for (acf, refused_at) in [
            ("ASG(HAG){RULE(1,READ)}", Some("HAG")),
            ("ASG(g){RULE(1,READ){CALC(INPA)}}", Some("INPA")),
            ("FOO(x){UAG(y) RULE(z){a}}", None),
            ("FOO(UAG,RULE,INPA)", None),
            ("HAG(h){ASG}", Some("ASG")),
            ("ASG(g){INPA(CALC)}", Some("CALC")),
            ("ASG(g){RULE(1,READ){METHOD(UAG)}}", None),
            ("ASG(g){RULE(1,RULE)}", Some("RULE")),
            ("ASG(g){RULE(1,READ){ASG(x)}}", None),
            ("ASG(g){RULE(1,READ){FOO(x){UAG(y)}}}", None),
            ("ASG(g){RULE(1,READ){INPA(x)}}", None),
            ("ASG(g){RULE(1,READ,UAG)}", Some("UAG")),
            ("UAG(a){x}\nASG(g){RULE(1,READ){UAG(UAG)}}", Some("UAG")),
            ("UAG(a){\"RULE\"}", None),
            ("UAG(UAG){x}", Some("UAG")),
            ("UAG(CALC){x}", Some("CALC")),
            ("UAG(INPA){x}", Some("INPA")),
            ("UAG(INPV){x}", None),
            ("UAG(RULE){x}", Some("RULE")),
            ("UAG(a){RULE}", Some("RULE")),
        ] {
            let parsed = parse_acf(format!("{acf}\n").as_bytes());
            match refused_at {
                None => assert!(parsed.is_ok(), "{acf}: {parsed:?}"),
                Some(_) => {
                    let e = parsed.unwrap_err().to_string();
                    assert!(e.contains("syntax error at line"), "{acf}: {e}");
                }
            }
        }
    }

    /// `ascheck` (R7.0.10) on `CALC '(' tokenSTRING ')'`: one string token,
    /// quoted or bare, kept as written.
    #[test]
    fn a_calc_expression_is_one_string_token_as_ascheck_reads_it() {
        for (clause, expr) in [
            ("CALC(\"A\")", Some("A")),
            ("CALC(A)", Some("A")),
            ("CALC( \"A\" )", Some("A")),
            ("CALC(\"A\",\"B\")", None),
            ("CALC()", None),
            ("CALC(\" A \")", Some(" A ")),
            ("CALC(A>1)", Some("A>1")),
            ("CALC(A+1)", Some("A+1")),
            ("CALC(5)", None),
            ("CALC(A B)", None),
            ("CALC", None),
        ] {
            let acf = format!("ASG(G) {{ INPA(r) RULE(1,READ) {{ {clause} }} }}\n");
            let parsed = parse_acf(acf.as_bytes());
            match expr {
                None => assert!(parsed.is_err(), "{clause}: {parsed:?}"),
                Some(e) => {
                    let config = parsed.unwrap_or_else(|err| panic!("{clause}: {err}"));
                    let rule = &config.asg[b"G".as_slice()].rules[0];
                    assert_eq!(rule.calc.as_deref(), Some(e.as_bytes()), "{clause}");
                }
            }
        }
    }

    /// `ascheck` (R7.0.10): `asUagAdd`, `asHagAdd` and `asAsgAdd` refuse a
    /// name already defined, except a `DEFAULT` ASG with no INP or RULE yet.
    #[test]
    fn a_group_name_defined_twice_fails_the_load() {
        for (acf, error) in [
            (
                "UAG(a) {x}\nUAG(a) {y}",
                Some("Duplicate User Access Group named 'a'"),
            ),
            (
                "UAG(a)\nUAG(a)",
                Some("Duplicate User Access Group named 'a'"),
            ),
            (
                "HAG(h) {x}\nHAG(h) {y}",
                Some("Duplicate Host Access Group named 'h'"),
            ),
            (
                "ASG(G)\nASG(G)",
                Some("Duplicate Access Security Group named 'G'"),
            ),
            ("ASG(DEFAULT)\nASG(DEFAULT)", None),
            (
                "ASG(DEFAULT) {RULE(1,READ)}\nASG(DEFAULT)",
                Some("Duplicate Access Security Group named 'DEFAULT'"),
            ),
            ("ASG(DEFAULT)\nASG(DEFAULT) {RULE(1,READ)}", None),
            (
                "ASG(DEFAULT) {INPA(x)}\nASG(DEFAULT)",
                Some("Duplicate Access Security Group named 'DEFAULT'"),
            ),
            (
                "ASG(DEFAULT) {RULE(1,READ) {FOO(a)}}\nASG(DEFAULT)",
                Some("Duplicate Access Security Group named 'DEFAULT'"),
            ),
            ("UAG(a) {x,x}", None),
            ("UAG(a) {x}\nHAG(a) {y}\nASG(a)", None),
        ] {
            let parsed = parse_acf(format!("{acf}\n").as_bytes());
            match error {
                None => assert!(parsed.is_ok(), "{acf}: {parsed:?}"),
                Some(e) => {
                    let got = parsed.unwrap_err().to_string();
                    assert!(got.contains(&format!("{e} at line 2")), "{acf}: {got}");
                }
            }
        }
    }

    /// `ascheck` (R7.0.10) on top-level items: any string but RULE, CALC and
    /// `INP<A-U>` names a `generic_item`, whose head is mandatory, whose
    /// block is not empty, and whose second block follows only a one-element
    /// first block and holds a list.
    #[test]
    fn top_level_items_parse_as_ascheck_reads_them() {
        for (acf, accepted) in [
            ("FOO(a)", true),
            ("FOO(a) {b}", true),
            ("\"x\"(a)", true),
            ("-x(a)", true),
            ("FOO", false),
            ("5(a)", false),
            ("FOO() {}", false),
            ("FOO(a) {}", false),
            ("RULE(a)", false),
            ("INPA(a)", false),
            ("CALC(a)", false),
            ("FOO(a) {b} {c,d}", true),
            ("FOO(a) {b} {c}", false),
            ("FOO(a) {b,c} {d,e}", false),
            ("FOO(a) {B(x)} {c,d}", false),
            ("FOO(a) {b} {c,d} {e,f}", false),
            ("FOO(a)(b)", false),
            ("FOO()", true),
            ("FOO(1,2.5,UAG,\"q\")", true),
            ("FOO(a) {X(1) Y() {z}}", true),
            ("\"UAG\"(a) {b}", true),
            ("INPAB(a)", true),
            ("INPV(a)", true),
            ("FOO(a) {b} ASG(G)", true),
            (",", false),
            ("FOO(a,)", false),
            ("\"x\" {a}", false),
            ("FOO(a) {b} {c,}", false),
        ] {
            let parsed = parse_acf(format!("{acf}\n").as_bytes());
            assert_eq!(parsed.is_ok(), accepted, "{acf}: {parsed:?}");
        }
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
        assert_eq!(config.asg[b"T".as_slice()].rules.len(), 2);
        assert_eq!(
            config.asg[b"T".as_slice()].rules[0].access,
            RuleAccess::Write
        );
        assert!(
            config.asg[b"T".as_slice()].rules[0].trap,
            "TRAPWRITE must set the trap mask"
        );
        assert_eq!(
            config.asg[b"T".as_slice()].rules[1].access,
            RuleAccess::Read
        );
        assert!(
            !config.asg[b"T".as_slice()].rules[1].trap,
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
ASG(LOCKED)
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
        let rule = &config.asg[b"G".as_slice()].rules[0];
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
        let targets: Vec<_> = InpWatch::of(&cfg)
            .targets
            .into_iter()
            .map(|t| (t.record, t.field))
            .collect();
        assert_eq!(
            targets,
            vec![
                ("gate".to_string(), "VAL".to_string()),
                ("gate".to_string(), "RVAL".to_string()),
                ("other".to_string(), "SEVR".to_string()),
            ],
            "`gate` is read by both groups but is one subscription"
        );
    }

    /// softIoc R7.0.10 with `okrec` at 1: `asprules` prints `CALC("1")
    /// result=FALSE` and `CALC("A") result=TRUE`, and `cainfo` reports
    /// "read, no write". A CALC reading no input is never recomputed.
    #[epics_macros_rs::epics_test]
    async fn a_calc_reading_no_input_never_grants() {
        use std::sync::Arc;
        let cfg = parse_acf(
            br#"ASG(g) { INPA(okrec) RULE(1,WRITE){CALC("1")} RULE(1,READ){CALC("A")} }"#,
        )
        .unwrap();
        serve(&cfg, &[("okrec", Some(ok(1.0)))]);
        let dump = String::from_utf8(cfg.dump_rules(Some(b"g"))).unwrap();
        assert!(dump.contains("CALC(\"1\") result=FALSE"), "{dump}");
        assert!(dump.contains("CALC(\"A\") result=TRUE"), "{dump}");

        let cell = crate::server::access_security::new_acf_cell(Some(cfg));
        let asg_resolver: AsgAslResolver =
            Arc::new(|_name| Box::pin(async { ("g".to_string(), 0u8) }));
        let gate = AccessGate::required(cell, asg_resolver);
        let access = gate.check("r", "h", "u", "ca", "").await;
        assert!(access.allows_read() && !access.allows_write(), "{access:?}");
    }

    /// C runs a CALC only when `inpChanged & inpUsed` (`asLibRoutines.c:966`),
    /// and `asCa.c:160` sets `inpChanged` only for a declared INP that
    /// delivered a value. softIoc R7.0.10 `cainfo`: no read for the first two,
    /// and `asprules` prints `result=FALSE` for all three.
    #[epics_macros_rs::epics_test]
    async fn a_calc_reading_only_undeclared_or_undelivered_inputs_never_runs() {
        use std::sync::Arc;
        for acf in [
            r#"ASG(DEFAULT) { INPA(rv:ai) RULE(1,READ) { CALC("B<1") } }"#,
            r#"ASG(DEFAULT) { RULE(1,READ) { CALC("A<1") } }"#,
            r#"ASG(DEFAULT) { INPA(nope:pv) RULE(1,READ) { CALC("A<1") } }"#,
        ] {
            let cfg = parse_acf(acf).unwrap();
            serve(&cfg, &[("rv:ai", Some(ok(5.0)))]);
            let dump = String::from_utf8(cfg.dump_rules(None)).unwrap();
            assert!(dump.contains("result=FALSE"), "{acf}: {dump}");

            let cell = crate::server::access_security::new_acf_cell(Some(cfg));
            let asg_resolver: AsgAslResolver =
                Arc::new(|_name| Box::pin(async { ("DEFAULT".to_string(), 0u8) }));
            let gate = AccessGate::required(cell, asg_resolver);
            let access = gate.check("q", "h", "u", "ca", "").await;
            assert!(!access.allows_read(), "{acf}: {access:?}");
        }
    }

    /// Records `(name, VAL, SEVR)` in a database the `INP*` links read; the
    /// watcher's attach pass delivers each existing one. Returns `asprules`
    /// and whether `DEFAULT` grants read.
    async fn inp_case(
        acf: &str,
        records: &[(&str, f64, crate::server::record::AlarmSeverity)],
    ) -> (String, bool) {
        use crate::server::database::PvDatabase;
        use std::sync::Arc;
        let db = Arc::new(PvDatabase::new());
        for &(name, value, sevr) in records {
            db.add_record(
                name,
                Box::new(crate::server::records::ai::AiRecord::new(value)),
            )
            .await
            .unwrap();
            db.get_record(name).unwrap().write().common.sevr = sevr;
        }
        let cfg = parse_acf(acf).unwrap();
        InpWatch::of(&cfg).attach(&db, &cfg);
        let dump = String::from_utf8(cfg.dump_rules(None)).unwrap();
        let cell = crate::server::access_security::new_acf_cell(Some(cfg));
        let asg_resolver: AsgAslResolver =
            Arc::new(|_name| Box::pin(async { ("DEFAULT".to_string(), 0u8) }));
        let gate = AccessGate::required(cell, asg_resolver);
        let read = gate.check("r", "h", "u", "ca", "").await.allows_read();
        (dump, read)
    }

    /// `asCa.c:149-151`: a value delivered at INVALID marks the input bad and
    /// is not stored, but still sets `inpChanged`, so the CALC runs on the old
    /// value and the rule denies (`asLibRoutines.c:1049`). softIoc R7.0.10
    /// with `q` a UDF ai: `INPA(q) INVALID value=0.000000`,
    /// `CALC("A<1") result=TRUE`, and `cainfo` reports no read.
    #[epics_macros_rs::epics_test]
    async fn an_inp_delivered_at_invalid_is_bad_and_denies() {
        use crate::server::record::AlarmSeverity;
        let (dump, read) = inp_case(
            r#"ASG(DEFAULT) { INPA(q) RULE(1,READ) { CALC("A<1") } }"#,
            &[("q", 5.0, AlarmSeverity::Invalid)],
        )
        .await;
        assert!(
            dump.contains("\tINPA(q) INVALID value=0.000000\n"),
            "{dump}"
        );
        assert!(dump.contains("CALC(\"A<1\") result=TRUE"), "{dump}");
        assert!(!read);
    }

    /// A link that never resolves delivers nothing (`asCa.c:133-162` runs only
    /// on an event), so a second `INPA` naming a missing PV leaves the bad bit
    /// the first one's delivery cleared. softIoc R7.0.10, `rv:ai` at 5: read,
    /// both lines `value=5.000000` with no INVALID, `result=TRUE`.
    #[epics_macros_rs::epics_test]
    async fn an_unresolved_duplicate_input_is_no_event() {
        use crate::server::record::AlarmSeverity;
        let (dump, read) = inp_case(
            r#"ASG(DEFAULT) { INPA(rv:ai) INPA(nope:pv) RULE(1,READ) { CALC("A>1") } }"#,
            &[("rv:ai", 5.0, AlarmSeverity::NoAlarm)],
        )
        .await;
        assert!(
            dump.contains("\tINPA(rv:ai) value=5.000000\n\tINPA(nope:pv) value=5.000000\n"),
            "{dump}"
        );
        assert!(dump.contains("CALC(\"A>1\") result=TRUE"), "{dump}");
        assert!(read);
    }

    /// A good delivery clears the input's bad bit (`asCa.c:154`), so a
    /// missing PV listed first under the same letter does not keep it bad.
    /// softIoc R7.0.10, `rv:ai` at 5: read, both lines `value=5.000000`.
    #[epics_macros_rs::epics_test]
    async fn a_good_delivery_clears_the_bad_bit_whatever_the_order() {
        use crate::server::record::AlarmSeverity;
        let (dump, read) = inp_case(
            r#"ASG(DEFAULT) { INPA(nope:pv) INPA(rv:ai) RULE(1,READ) { CALC("A>1") } }"#,
            &[("rv:ai", 5.0, AlarmSeverity::NoAlarm)],
        )
        .await;
        assert!(
            dump.contains("\tINPA(nope:pv) value=5.000000\n\tINPA(rv:ai) value=5.000000\n"),
            "{dump}"
        );
        assert!(read);
    }

    /// With nothing processed, each channel's first event arrives in
    /// `asCaTask`'s add order (`asCa.c:178-200`), so of two inputs on one
    /// letter the one listed last holds the value. softIoc R7.0.10, `a1` at 1
    /// and `a2` at 2 with no PINI: `value=2.000000` on both lines, and
    /// `value=1.000000` with the listing reversed.
    #[epics_macros_rs::epics_test]
    async fn two_first_readings_on_one_letter_follow_the_listing() {
        use crate::server::record::AlarmSeverity;
        let records = [
            ("a1", 1.0, AlarmSeverity::NoAlarm),
            ("a2", 2.0, AlarmSeverity::NoAlarm),
        ];
        for (acf, want) in [
            (
                r#"ASG(DEFAULT) { INPA(a1) INPA(a2) RULE(1,READ) { CALC("A>1") } }"#,
                "2",
            ),
            (
                r#"ASG(DEFAULT) { INPA(a2) INPA(a1) RULE(1,READ) { CALC("A>1") } }"#,
                "1",
            ),
        ] {
            let (dump, _) = inp_case(acf, &records).await;
            for line in ["\tINPA(a1)", "\tINPA(a2)"] {
                assert!(
                    dump.contains(&format!("{line} value={want}.000000\n")),
                    "{dump}"
                );
            }
        }
    }

    /// Only INVALID makes a delivery bad (`asCa.c:149`): an input in MINOR or
    /// MAJOR alarm is stored and grants. softIoc R7.0.10 with the ai at 5 in a
    /// HIGH alarm of either severity: read, `INPA(..) value=5.000000`.
    #[epics_macros_rs::epics_test]
    async fn a_minor_or_major_input_stays_good() {
        use crate::server::record::AlarmSeverity;
        for sevr in [AlarmSeverity::Minor, AlarmSeverity::Major] {
            let (dump, read) = inp_case(
                r#"ASG(DEFAULT) { INPA(alm) RULE(1,READ) { CALC("A>1") } }"#,
                &[("alm", 5.0, sevr)],
            )
            .await;
            assert!(
                dump.contains("\tINPA(alm) value=5.000000\n"),
                "{sevr:?}: {dump}"
            );
            assert!(read, "{sevr:?}");
        }
    }

    /// `asCa.c:183` starts every declared input bad, so an input that never
    /// delivers still disables the rules reading it. softIoc R7.0.10:
    /// `INPB(nope:pv) INVALID value=0.000000`, `result=TRUE`, no read.
    #[epics_macros_rs::epics_test]
    async fn a_declared_input_that_never_delivers_is_bad() {
        use crate::server::record::AlarmSeverity;
        let (dump, read) = inp_case(
            r#"ASG(DEFAULT) { INPA(rv:ai) INPB(nope:pv) RULE(1,READ) { CALC("A>1&&B<1") } }"#,
            &[("rv:ai", 5.0, AlarmSeverity::NoAlarm)],
        )
        .await;
        assert!(
            dump.contains("\tINPB(nope:pv) INVALID value=0.000000\n"),
            "{dump}"
        );
        assert!(dump.contains("CALC(\"A>1&&B<1\") result=TRUE"), "{dump}");
        assert!(!read);
    }

    /// A CALC-gated rule grants while the published result is true, denies
    /// when it is false, when its input disconnects, and before any delivery.
    #[epics_macros_rs::epics_test]
    async fn calc_gated_rule_follows_the_published_state() {
        use std::sync::Arc;
        let acf = r#"ASG(OPS) { INPA("permit.VAL") RULE(1, WRITE) { CALC("A=1") } }"#;
        let cfg = parse_acf(acf).unwrap();
        let cell = crate::server::access_security::new_acf_cell(Some(cfg));
        let asg_resolver: AsgAslResolver =
            Arc::new(|_name| Box::pin(async { ("OPS".to_string(), 0u8) }));
        let gate = AccessGate::required(cell.clone(), asg_resolver);
        let writes = || async { gate.check("x", "h", "u", "ca", "").await.allows_write() };
        let cfg = cell.load_full().unwrap();

        assert!(!writes().await, "no delivery yet: INPA is bad");
        serve(&cfg, &[("permit", Some(ok(1.0)))]);
        assert!(writes().await, "CALC A=1 with permit=1 grants WRITE");
        serve(&cfg, &[("permit", Some(ok(0.0)))]);
        assert!(!writes().await, "CALC A=1 with permit=0 denies WRITE");
        serve(&cfg, &[("permit", Some(ok(1.0)))]);
        serve(&cfg, &[("permit", None)]);
        assert!(!writes().await, "a disconnected INP denies the rule");
    }

    /// a `role/<name>` UAG member matches a client that holds
    /// that role; a client without it does not match.
    #[test]
    fn uag_role_member_matches_client_role() {
        let cfg =
            parse_acf(r#"UAG(special) { "role/op" } ASG(G) { RULE(1, WRITE) { UAG(special) } }"#)
                .unwrap();
        let (lvl, _) =
            cfg.compute_for_name("G", "h", "acct", &["op".to_string().into()], 0, "ca", "");
        assert_eq!(
            lvl,
            AccessLevel::ReadWrite,
            "role/op member matches a client holding role 'op'"
        );
        let (lvl_none, _) = cfg.compute_for_name("G", "h", "acct", &[], 0, "ca", "");
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
        let inp = &config.asg[b"G".as_slice()].inp;
        assert_eq!(inp.len(), 2);
        assert_eq!(inp[0].index, 0);
        assert_eq!(inp[0].link, b"rec1.VAL");
        assert_eq!(inp[1].index, 2);
        assert_eq!(inp[1].link, b"rec3.VAL");
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
        assert!(config.uag.contains_key(b"my group".as_slice()));
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
            if msg.pv_name == pv.as_bytes() {
                sink.lock()
                    .unwrap()
                    .push((msg.op, msg.status.map(str::to_owned)));
            }
        }));
        (events, handle)
    }

    fn trap_fields(pv: &'static str) -> TrapWriteFields {
        TrapWriteFields {
            pv_name: pv.as_bytes().to_vec(),
            user: b"u".to_vec(),
            host: b"h".to_vec(),
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

    #[test]
    fn a_non_utf8_user_reaches_the_trap_listener_byte_for_byte() {
        let users = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = users.clone();
        let _handle = register_trap_write_listener(std::sync::Arc::new(move |msg| {
            if msg.pv_name == b"trap:nonutf8" {
                sink.lock().unwrap().push(msg.user.to_vec());
            }
        }));
        let meta = TrapWriteMeta {
            pv_name: b"trap:nonutf8",
            user: b"op\xff",
            host: "h",
            peer: "h:5064",
            dbr_type: 5,
        };
        put_with_trap_blocking(true, meta, crate::types::EpicsValue::Long(1), |_| {
            Ok::<(), ()>(())
        })
        .unwrap();
        assert_eq!(*users.lock().unwrap(), vec![b"op\xff".to_vec(); 2]);
    }

    /// C `asTrapWriteMessage` hands listeners the channel name as stored;
    /// a Latin-1 name must not be decoded on the way.
    #[test]
    fn a_non_utf8_pv_name_reaches_the_trap_listener_byte_for_byte() {
        let names = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = names.clone();
        let _handle = register_trap_write_listener(std::sync::Arc::new(move |msg| {
            if msg.pv_name.starts_with(b"trap:latin1") {
                sink.lock().unwrap().push(msg.pv_name.to_vec());
            }
        }));
        let meta = TrapWriteMeta {
            pv_name: b"trap:latin1\xb5",
            user: b"op",
            host: "h",
            peer: "h:5064",
            dbr_type: 5,
        };
        put_with_trap_blocking(true, meta, crate::types::EpicsValue::Long(1), |_| {
            Ok::<(), ()>(())
        })
        .unwrap();
        assert_eq!(*names.lock().unwrap(), vec![b"trap:latin1\xb5".to_vec(); 2]);
    }
}

#[cfg(test)]
mod as_ca_task_tests {
    use super::*;
    use crate::server::database::PvDatabase;
    use crate::server::records::ai::AiRecord;
    use crate::types::EpicsValue;
    use std::sync::Arc;

    /// Poll `DEFAULT`'s published state until `done` holds.
    async fn wait_state(cell: &AcfCell, done: impl Fn(&AsgState) -> bool) -> Arc<AsgState> {
        let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
        loop {
            let state = cell.load_full().unwrap().asg[b"DEFAULT".as_slice()].state();
            if done(&state) {
                return state;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "state never settled: {state:?}"
            );
            crate::runtime::task::sleep_background(std::time::Duration::from_millis(10)).await;
        }
    }

    /// A database holding `rv` at `value`, processed so it is not UDF.
    async fn rv_db(value: f64) -> Arc<PvDatabase> {
        let db = Arc::new(PvDatabase::new());
        add_rv(&db, value).await;
        db
    }

    async fn add_rv(db: &PvDatabase, value: f64) {
        db.add_record("rv", Box::new(AiRecord::new(value)))
            .await
            .unwrap();
        db.put_record_field_from_ca("rv", "VAL", EpicsValue::Double(value))
            .await
            .unwrap();
    }

    /// `asCa.c:149-151`: an INVALID post marks the input bad and keeps the
    /// value and the result the last good one gave. softIoc R7.0.10, `rv` at 5
    /// then driven INVALID: `INPA(rv) INVALID value=5.000000`,
    /// `CALC("A>1") result=TRUE`.
    #[epics_macros_rs::epics_test]
    async fn an_invalid_post_keeps_the_last_good_value_and_result() {
        let db = rv_db(5.0).await;
        db.put_record_field_from_ca("rv", "HIGH", EpicsValue::Double(6.0))
            .await
            .unwrap();
        db.put_record_field_from_ca("rv", "HSV", EpicsValue::String("INVALID".into()))
            .await
            .unwrap();
        let cfg = parse_acf(r#"ASG(DEFAULT) { INPA(rv) RULE(1,READ) { CALC("A>1") } }"#).unwrap();
        let cell = new_acf_cell_watching(Some(cfg), &db);
        let good = wait_state(&cell, |s| s.bad(1) == 0).await;
        assert_eq!((good.values.vars[0], good.result(0)), (5.0, true));

        db.put_record_field_from_ca("rv", "VAL", EpicsValue::Double(7.0))
            .await
            .unwrap();
        let bad = wait_state(&cell, |s| s.bad(1) == 1).await;
        assert_eq!((bad.values.vars[0], bad.result(0)), (5.0, true));
        // softIoc R7.0.10 `asprules` after the same two posts.
        assert_eq!(
            String::from_utf8(cell.load_full().unwrap().dump_rules(None)).unwrap(),
            "ASG(DEFAULT) {\n\tINPA(rv) INVALID value=5.000000\n\
             \tRULE(1,READ,NOTRAPWRITE) {\n\t\tCALC(\"A>1\") result=TRUE\n\t}\n}\n"
        );
    }

    /// `asCa.c connectCallback`: a channel that disconnects is bad and keeps
    /// its value; the record's return is a fresh connection.
    #[epics_macros_rs::epics_test]
    async fn a_removed_input_record_is_bad_until_it_returns() {
        let db = rv_db(5.0).await;
        let cfg = parse_acf(r#"ASG(DEFAULT) { INPA(rv) RULE(1,READ) { CALC("A>1") } }"#).unwrap();
        let cell = new_acf_cell_watching(Some(cfg), &db);
        wait_state(&cell, |s| s.bad(1) == 0).await;

        assert!(db.remove_record("rv").await);
        let gone = wait_state(&cell, |s| s.bad(1) == 1).await;
        assert_eq!((gone.values.vars[0], gone.result(0)), (5.0, true));

        add_rv(&db, 0.0).await;
        notify_asg_field_changed();
        let back = wait_state(&cell, |s| s.bad(1) == 0).await;
        assert_eq!((back.values.vars[0], back.result(0)), (0.0, false));
    }

    /// A reload is a new `ASG` (C `asInit`): the new file's inputs start bad
    /// and take their own first delivery, whatever the old file had.
    #[epics_macros_rs::epics_test]
    async fn a_reload_starts_from_a_fresh_state() {
        let db = rv_db(5.0).await;
        let acf = r#"ASG(DEFAULT) { INPA(rv) RULE(1,READ) { CALC("A>1") } }"#;
        let cell = new_acf_cell_watching(Some(parse_acf(acf).unwrap()), &db);
        wait_state(&cell, |s| s.bad(1) == 0).await;

        let reloaded = Arc::new(parse_acf(acf).unwrap());
        assert_eq!(reloaded.asg[b"DEFAULT".as_slice()].state().bad(1), 1);
        cell.store(Some(reloaded));
        let state = wait_state(&cell, |s| s.bad(1) == 0).await;
        assert_eq!((state.values.vars[0], state.result(0)), (5.0, true));
    }

    /// `asCaStart` returns only after `asCaTask` has added the inputs
    /// (`asCaTaskWait`), so the first readings are in place when `asInit`
    /// returns, with no wait of the caller's own.
    #[epics_macros_rs::epics_test]
    async fn wait_served_returns_with_the_first_readings_in_place() {
        let db = rv_db(5.0).await;
        let cell = new_acf_cell_watching(None, &db);
        // Two ASGs on one record: C opens a channel per input, so each takes
        // its own first event.
        let acf = r#"ASG(DEFAULT) { INPA(rv) RULE(1,READ) { CALC("A>1") } }
            ASG(B) { INPB(rv) RULE(1,READ) { CALC("B>1") } }"#;
        cell.store(Some(Arc::new(parse_acf(acf).unwrap())));
        cell.wait_served();
        let cfg = cell.load_full().unwrap();
        for (asg, index) in [(&b"DEFAULT"[..], 0u8), (b"B", 1)] {
            let state = cfg.asg[asg].state();
            assert_eq!(
                (state.bad(1 << index), state.value(index), state.result(0)),
                (0, 5.0, true),
                "{}",
                String::from_utf8_lossy(asg)
            );
        }
    }

    /// `read_acf` awaited from cbMedium leaves cbMedium free while the file is
    /// read: the fifo's writer waits for a second cbMedium future, which a read
    /// on the band never lets run. Under tokio the seam it replaced panicked
    /// here instead, a band future having no runtime.
    #[cfg(unix)]
    #[epics_macros_rs::epics_test]
    async fn read_acf_from_cb_medium_leaves_the_band_free() {
        use crate::runtime::task::{CallbackPriority, spawn_background};
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("acf");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let path = fifo.clone();
        let writer = std::thread::spawn(move || {
            let waited = wait.recv_timeout(fact_budget::FACT_BUDGET).is_ok();
            std::fs::write(path, "ASG(DEFAULT) { RULE(1, READ) }\n").unwrap();
            waited
        });
        let read = spawn_background(CallbackPriority::Medium, read_acf(fifo));
        // Queued only once the read is parked on the fifo, wherever it runs.
        std::thread::sleep(std::time::Duration::from_millis(200));
        spawn_background(CallbackPriority::Medium, async move {
            let _ = go.send(());
        });
        let cfg = read.await.unwrap();
        assert!(writer.join().unwrap(), "read_acf parked cbMedium");
        assert!(cfg.unwrap().asg.contains_key(b"DEFAULT".as_slice()));
    }

    /// `install_async` awaits the first readings without parking a thread,
    /// so it completes on cbMedium beside the watcher it waits for, where a
    /// blocking `install` starves it.
    #[epics_macros_rs::epics_test]
    async fn install_async_completes_on_the_watchers_band() {
        let db = rv_db(5.0).await;
        let cell = new_acf_cell_watching(None, &db);
        let cfg = parse_acf(r#"ASG(DEFAULT) { INPA(rv) RULE(1,READ) { CALC("A>1") } }"#).unwrap();
        // Hold `rv`'s lock so the first reading lands only after the install
        // is waiting for it.
        let (locked, held) = std::sync::mpsc::channel();
        let rec = db.get_record("rv").unwrap();
        let holder = std::thread::spawn(move || {
            let _guard = rec.write();
            locked.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(200));
        });
        held.recv().unwrap();
        let installing = cell.clone();
        let installed = crate::runtime::task::spawn_background(
            crate::runtime::task::CallbackPriority::Medium,
            async move { installing.install_async(cfg).await },
        );
        crate::runtime::task::timeout(fact_budget::FACT_BUDGET, installed)
            .await
            .expect("install_async never returned")
            .unwrap();
        assert_eq!(
            cell.load_full().unwrap().asg[b"DEFAULT".as_slice()].inp_bad(),
            0
        );
        holder.join().unwrap();
    }

    /// U11: two inputs on one letter that both deliver. C keeps whichever
    /// event arrives last (`asCa.c:146-161`), so the record processed last
    /// decides, whichever the ACF lists last. softIoc R7.0.10 with `g` an ai
    /// at 5 and `bi` an ai at 20 with HIHI 10, HHSV INVALID: read exactly
    /// when `g` is processed after `bi`.
    #[epics_macros_rs::epics_test]
    async fn two_inputs_on_one_letter_follow_the_processing_order() {
        for acf in [
            r#"ASG(DEFAULT) { INPA(bi) INPA(g) RULE(1,READ) { CALC("A>1") } }"#,
            r#"ASG(DEFAULT) { INPA(g) INPA(bi) RULE(1,READ) { CALC("A>1") } }"#,
        ] {
            for last in ["g", "bi"] {
                let db = Arc::new(PvDatabase::new());
                for name in ["g", "bi"] {
                    db.add_record(name, Box::new(AiRecord::new(0.0)))
                        .await
                        .unwrap();
                }
                for (field, value) in [
                    ("HIHI", EpicsValue::Double(10.0)),
                    ("HHSV", EpicsValue::String("INVALID".into())),
                ] {
                    db.put_pv(&format!("bi.{field}"), value).await.unwrap();
                }
                let cell = new_acf_cell_watching(None, &db);
                cell.store(Some(Arc::new(parse_acf(acf).unwrap())));
                cell.wait_served_async().await;
                let order = if last == "g" {
                    ["bi", "g"]
                } else {
                    ["g", "bi"]
                };
                for name in order {
                    let value = if name == "g" { 5.0 } else { 20.0 };
                    db.put_record_field_from_ca(name, "VAL", EpicsValue::Double(value))
                        .await
                        .unwrap();
                }
                let want = u32::from(last == "bi");
                let state = wait_state(&cell, |s| s.bad(1) == want && s.value(0) == 5.0).await;
                assert!(state.result(0), "{acf}, {last} last: {state:?}");
                let (level, _) =
                    cell.load_full()
                        .unwrap()
                        .compute_for_name("x", "h", "u", &[], 0, "ca", "");
                assert_eq!(
                    level == AccessLevel::Read,
                    last == "g",
                    "{acf}, {last} processed last"
                );
            }
        }
    }

    /// A HAG refresh republishes the same load. C has no refresher and moves
    /// the state only on events, so the record processed last still decides:
    /// the republish must not replay the first readings in listing order.
    #[epics_macros_rs::epics_test]
    async fn a_hag_refresh_keeps_the_state_the_events_built() {
        let db = Arc::new(PvDatabase::new());
        for name in ["g", "bi"] {
            db.add_record(name, Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
        }
        for (field, value) in [
            ("HIHI", EpicsValue::Double(10.0)),
            ("HHSV", EpicsValue::String("INVALID".into())),
        ] {
            db.put_pv(&format!("bi.{field}"), value).await.unwrap();
        }
        let acf = r#"HAG(local) { localhost }
            ASG(DEFAULT) { INPA(g) INPA(bi) RULE(1,READ) { CALC("A>1") } }"#;
        let cell = new_acf_cell_watching(None, &db);
        cell.store(Some(Arc::new(parse_acf(acf).unwrap())));
        cell.wait_served_async().await;
        for (name, value) in [("bi", 20.0), ("g", 5.0)] {
            db.put_record_field_from_ca(name, "VAL", EpicsValue::Double(value))
                .await
                .unwrap();
        }
        wait_state(&cell, |s| s.bad(1) == 0 && s.value(0) == 5.0).await;

        // What `spawn_hag_refresh` publishes once a HAG resolution moves.
        let mut moved = AccessSecurityConfig::clone(&cell.load_full().unwrap());
        moved
            .hag
            .insert(b"local".to_vec(), vec![b"192.0.2.1".to_vec()]);
        let refreshed = moved.with_refreshed_hags().expect("the HAG moved");
        cell.store(Some(Arc::new(refreshed)));
        cell.wait_served_async().await;
        // Let the watcher take the republish's wake.
        crate::runtime::task::sleep_background(std::time::Duration::from_millis(200)).await;
        let cfg = cell.load_full().unwrap();
        assert_eq!(cfg.asg[b"DEFAULT".as_slice()].state().bad(1), 0);
        let (level, _) = cfg.compute_for_name("x", "h", "u", &[], 0, "ca", "");
        assert_eq!(level, AccessLevel::Read);
    }

    /// A post is published before the change signal fires, so whoever sees
    /// the new generation sees the new state: [`AccessGate`]'s check cache
    /// stamps the generation before it computes, and a signal first would
    /// let it cache the old result under the new stamp for good.
    #[epics_macros_rs::epics_test]
    async fn the_change_signal_follows_the_published_state() {
        let db = rv_db(0.0).await;
        let cfg = parse_acf(r#"ASG(DEFAULT) { INPA(rv) RULE(1,READ) { CALC("A>1") } }"#).unwrap();
        let cell = new_acf_cell_watching(Some(cfg), &db);
        cell.wait_served_async().await;
        for k in 1..=50 {
            let before = asg_change_generation();
            let spinning = Arc::new(std::sync::Barrier::new(2));
            let checker = {
                let (cell, spinning) = (cell.clone(), spinning.clone());
                std::thread::spawn(move || {
                    spinning.wait();
                    while asg_change_generation() == before {
                        std::hint::spin_loop();
                    }
                    cell.load_full().unwrap().asg[b"DEFAULT".as_slice()]
                        .state()
                        .value(0)
                })
            };
            spinning.wait();
            db.put_record_field_from_ca("rv", "VAL", EpicsValue::Double(f64::from(k)))
                .await
                .unwrap();
            let seen = crate::runtime::task::spawn_blocking(move || checker.join().unwrap())
                .await
                .unwrap();
            assert_eq!(seen, f64::from(k), "post {k}");
        }
    }

    /// Posts that queue up between two wakes are delivered in post order, so
    /// of two inputs on one letter the later post wins.
    #[epics_macros_rs::epics_test]
    async fn a_burst_of_posts_is_delivered_in_post_order() {
        for (first, second, want) in [("a1", "a2", 2.0), ("a2", "a1", 1.0)] {
            let db = Arc::new(PvDatabase::new());
            for name in ["a1", "a2"] {
                db.add_record(name, Box::new(AiRecord::new(0.0)))
                    .await
                    .unwrap();
            }
            let cfg =
                parse_acf(r#"ASG(DEFAULT) { INPA(a1) INPA(a2) RULE(1,READ) { CALC("A>1") } }"#)
                    .unwrap();
            let mut watch = InpWatch::of(&cfg);
            assert!(watch.attach(&db, &cfg));
            for name in [first, second] {
                let value = if name == "a1" { 1.0 } else { 2.0 };
                db.put_record_field_from_ca(name, "VAL", EpicsValue::Double(value))
                    .await
                    .unwrap();
            }
            let posted = drain_any_asg_inp(&watch.user).await;
            assert!(posted.len() >= 2, "both posts queued");
            watch.deliver(&db, &cfg, posted);
            let state = cfg.asg[b"DEFAULT".as_slice()].state();
            assert_eq!(state.value(0), want, "{second} posted last");
        }
    }

    /// `asComputeAsgPvt` passes the rule's previous result as `VAL`
    /// (`asLibRoutines.c:961`).
    #[test]
    fn a_calc_reads_its_previous_result_as_val() {
        let cfg = parse_acf(r#"ASG(G) { INPA(x) RULE(1,READ) { CALC("A>1||VAL") } }"#).unwrap();
        let g = &cfg.asg[b"G".as_slice()];
        let at = |v| {
            InpEvent::Reading(InpReading {
                value: v,
                severity: crate::server::record::AlarmSeverity::NoAlarm,
            })
        };
        g.deliver([(0, at(5.0))]);
        g.deliver([(0, at(0.0))]);
        assert!(g.state().result(0), "VAL latched the TRUE result");
    }

    /// `asComputeAsgPvt` clears `inpChanged` once the CALCs ran
    /// (`asLibRoutines.c:979`), so an event on B does not rerun a CALC that
    /// reads only A — which would flip this one, as it toggles on `VAL`.
    #[test]
    fn a_calc_reruns_only_for_its_own_inputs() {
        let cfg = parse_acf(
            r#"ASG(G) { INPA(x) INPB(y) RULE(1,READ) { CALC("A&&!VAL") } RULE(1,WRITE) { CALC("B") } }"#,
        )
        .unwrap();
        let g = &cfg.asg[b"G".as_slice()];
        let at = |v| {
            InpEvent::Reading(InpReading {
                value: v,
                severity: crate::server::record::AlarmSeverity::NoAlarm,
            })
        };
        g.deliver([(0, at(1.0))]);
        assert!(g.state().result(0));
        g.deliver([(1, at(1.0))]);
        let state = g.state();
        assert_eq!((state.result(0), state.result(1)), (true, true));
        assert_eq!(state.changed, 0);
    }

    /// C only has an `asCaTask` between `asCaStart` and `asCaStop`, and
    /// nothing but `asInitCommon` calls either (`asDbLib.c:147`, `:136`), so
    /// `taskwdShow` on a softIoc with no access-security file lists 15 threads
    /// and none of them is `asCaTask`. The port welds the watcher to the cell
    /// so a policy loaded later cannot find it missing, which left the row
    /// standing on an IOC that has no policy at all.
    ///
    /// Both transitions are asserted, not just the initial absence: an
    /// absence-only test would also pass if the entry were simply never taken.
    #[epics_macros_rs::epics_test]
    async fn the_as_ca_task_row_tracks_the_loaded_policy() {
        fn table() -> String {
            let out = std::cell::RefCell::new(String::new());
            crate::runtime::taskwd::taskwd_show(1, &|line| {
                out.borrow_mut().push_str(line);
                out.borrow_mut().push('\n');
            });
            out.into_inner()
        }
        async fn wait_until(want: bool) {
            let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
            while table().contains("asCaTask") != want {
                assert!(
                    std::time::Instant::now() < deadline,
                    "`asCaTask` never became {}:\n{}",
                    if want { "present" } else { "absent" },
                    table()
                );
                crate::runtime::task::sleep_background(std::time::Duration::from_millis(10)).await;
            }
        }

        let db = std::sync::Arc::new(crate::server::database::PvDatabase::new());
        let cell = new_acf_cell_watching(None, &db);
        assert!(
            !table().contains("asCaTask"),
            "a cell built with no policy registered the access-security task"
        );

        cell.store(Some(std::sync::Arc::new(
            parse_acf("ASG(DEFAULT) { RULE(1, READ) }").expect("minimal ACF"),
        )));
        wait_until(true).await;

        cell.store(None);
        wait_until(false).await;
    }
}
