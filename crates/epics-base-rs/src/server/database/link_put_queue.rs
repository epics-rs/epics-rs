//! External-link OUT-write queue — the Rust counterpart of `dbCa`'s
//! `workList` (`modules/database/src/ioc/db/dbCa.c`).
//!
//! # The C model this mirrors
//!
//! C never performs a Channel Access network write inside `dbScanLock`.
//! `dbCaPutLink` (`dbCa.c:598-602`) forwards to `dbCaPutLinkCallback`
//! (`dbCa.c:515-596`), which — under the per-link `pca->lock`, *not* the
//! record lock —
//!
//! 1. refuses the put outright when the link is not connected or has no
//!    write access, returning `-1` with nothing staged (`dbCa.c:529-532`);
//! 2. converts the value into the link's single pending-value buffer
//!    `pca->pputNative` / `pca->pputString` (`dbCa.c:533-584`);
//! 3. records the delivery flavour — `CA_PUT` for a plain put,
//!    `CA_PUT_CALLBACK` plus the completion callback for
//!    `dbCaPutLinkCallback` (`dbCa.c:585-592`);
//! 4. calls `addAction(pca, CA_WRITE_NATIVE)` (`dbCa.c:130-159`), which ORs
//!    the action bit, appends the link to `workList` **once**
//!    (`callAdd = (pca->link_action == 0)`, `dbCa.c:135`, `:153-155`) and
//!    signals `workListEvent`;
//! 5. **returns**.
//!
//! The network write happens later on the `dbCaTask` thread
//! (`dbCa.c:1093-1260`): it pops one link, clears `link_action` *before*
//! doing the work (`dbCa.c:1130`) so a put arriving mid-flight re-queues the
//! link, and issues `ca_array_put` (plain) or `ca_array_put_callback`
//! (completion flavour) at `dbCa.c:1161-1183`.
//!
//! Three consequences this module reproduces verbatim:
//!
//! * **Latest-wins per link, not a per-put queue.** `pputNative` is one
//!   buffer. A second put staged before the task services the link
//!   overwrites the first and only bumps a counter
//!   (`if (pca->newOutNative) pca->nNoWrite++;`, `dbCa.c:582-583`; the
//!   string twin at `:548-549`). There is therefore no "queue full" state
//!   in C to match — the bound is one pending value per link, times the
//!   number of links, and that is the bound here too.
//! * **The link is enqueued at most once.** `addAction`'s `callAdd` gate
//!   (`dbCa.c:135`) appends the `caLink` to `workList` only when it carries
//!   no pending action; a put on an already-queued link just ORs the bit.
//! * **The returned status is the *staging* status, never the network
//!   status.** `dbCaPutLinkCallback` returns the conversion status (or the
//!   `-1` refusal); the outcome of the wire write is reported only through
//!   `errlogPrintf` on the task (`dbCa.c:1175-1179`) or, for the completion
//!   flavour, through `putComplete` (`dbCa.c:994-1012`). C's
//!   `dbPutLink` → `setLinkAlarm` gate (`dbLink.c:434-448`) therefore alarms
//!   on *refusal to stage*, not on a failed transmission.
//!
//! # What this module owns
//!
//! One [`LinkPutQueue`] per [`super::PvDatabase`]. A record-processing
//! thread stages a write and returns; a single owner task drains the queue
//! and performs the actual `LinkSet::put_value` away from the record's
//! advisory write gate.
//!
//! **Invariant.** *Every staged completion is resolved exactly once* —
//! by the owner (with the lset's result), by a superseding put, by the
//! link's removal ([`LinkPutQueue::remove_link`]), or by the queue's
//! teardown. A put the lset could not issue
//! ([`LinkPutDone::NotIssued`]) is dropped, never re-sent, and the owner
//! resolves its completion then. [`PutCompletion`] enforces it by
//! construction: its
//! `Drop` resolves an unresolved completion, so no path — early return,
//! `?`, panic, or the whole database dropping — can drop a pending put
//! silently.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Weak;

use super::link_set::{DynLinkSet, LinkPutDone, LinkPutOp, LinkRef};
use crate::types::EpicsValue;

/// The channel an external link addresses: the scheme of the one lset it
/// belongs to, the name that lset is addressed with, and the link itself.
/// The unit of an OPEN, because C stages one `CA_CONNECT` per `caLink`
/// (`dbCa.c:397-401`); equal by the link's serial.
#[derive(Clone, Debug)]
pub(crate) struct Channel {
    pub(crate) scheme: &'static str,
    pub(crate) name: crate::types::RecordName,
    link: crate::server::record::OpenLatch,
}

impl Channel {
    pub(crate) fn new(
        scheme: &'static str,
        name: &[u8],
        link: &crate::server::record::OpenLatch,
    ) -> Self {
        Self {
            scheme,
            name: name.into(),
            link: link.clone(),
        }
    }

    /// The link as its lset is asked about it.
    pub(crate) fn link_ref(&self) -> LinkRef<'_> {
        LinkRef::new(&self.name, &self.link)
    }
}

impl PartialEq for Channel {
    fn eq(&self, other: &Self) -> bool {
        self.link.serial() == other.link.serial()
    }
}

impl Eq for Channel {}

impl std::hash::Hash for Channel {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.link.serial().hash(state);
    }
}

/// The link a put comes from: C's `DBLINK` and the `caLink` it holds.
/// `rec` is `None` only for a cell built outside a database.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct LinkId {
    pub(crate) rec: Option<super::RecordIdx>,
    pub(crate) field: &'static str,
    pub(crate) install: LinkInstall,
}

/// The parse the source record has installed on the link field when the put
/// is made: C's `caLink` (`plink->value.pv_link.pvt`), which every relink
/// replaces, same name or not (`dbCaRemoveLink` then
/// `dbCaAddLinkCallbackOpt`, `dbCa.c:431-446`, `:389-414`). Held weakly, so
/// the relink that drops the parse is what retires the values staged
/// through it. Read from the record, never from the caller's parse, which
/// may be a copy made for the one put.
#[derive(Clone, Debug)]
pub(crate) enum LinkInstall {
    Installed(Weak<crate::server::record::ParsedLink>),
    /// A put from no record, or through a link the record does not store:
    /// nothing can relink it.
    Detached,
}

impl LinkInstall {
    pub(crate) fn of_field(instance: &crate::server::record::RecordInstance, field: &str) -> Self {
        instance
            .stored_link_shared(field)
            .map_or(Self::Detached, |link| Self::Installed(Arc::downgrade(link)))
    }

    /// C `dbCaRemoveLink`'s `CA_CLEAR_CHANNEL`: the task drops whatever the
    /// link still had to write (`dbCa.c:1200-1203`).
    fn removed(&self) -> bool {
        match self {
            Self::Installed(link) => link.strong_count() == 0,
            Self::Detached => false,
        }
    }
}

impl PartialEq for LinkInstall {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Installed(a), Self::Installed(b)) => Weak::ptr_eq(a, b),
            (Self::Detached, Self::Detached) => true,
            _ => false,
        }
    }
}

impl Eq for LinkInstall {}

impl std::hash::Hash for LinkInstall {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            Self::Installed(link) => link.as_ptr().hash(state),
            Self::Detached => std::ptr::null::<()>().hash(state),
        }
    }
}

/// Identity of one external link: the link plus the channel it names. The
/// unit of latest-wins coalescing, exactly as `caLink` is in C — each link
/// gets its own (`dbCa.c:398-414`), so two links naming one PV have two
/// `pputNative` buffers and neither put supersedes the other (`:611`).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct LinkKey {
    pub(crate) link: LinkId,
    pub(crate) channel: Channel,
}

/// The completion half of a staged put — the `pca->putCallback` slot
/// (`dbCaPvt.h:83`, set at `dbCa.c:585-588`, consumed by `putComplete` at
/// `:994-1011`).
///
/// Unlike C, which simply overwrites `putCallback` when a second
/// `dbCaPutLinkCallback` lands before the first completes — dropping the
/// earlier callback with no notification (`dbCa.c:585-592`) — a
/// `PutCompletion` cannot be lost: `Drop` resolves it. That is strictly
/// stronger than C and is what makes "no pending put loses its
/// classification silently" hold by construction rather than by review.
enum PutCompletion {
    /// A `dbCaPutLinkCallback` callback: the oneshot its writer awaits.
    Notify(Option<tokio::sync::oneshot::Sender<Result<(), String>>>),
    /// C `dbCaCallbackProcess` → `dbLinkAsyncComplete` (`dbCa.c:317-322`,
    /// `dbLink.c`): the `devXxxSoftCallback` record's completing pass, which
    /// takes no status, run on the band its put's waiter named.
    Record(Option<(super::DeviceDone, crate::runtime::task::CallbackPriority)>),
}

impl PutCompletion {
    /// Resolve with the outcome the owner (or a superseding put) determined.
    /// `inline` is [`LinkPutDone::Inline`].
    fn resolve(mut self, inline: bool, result: Result<(), String>) {
        self.finish(inline, result);
    }

    fn finish(&mut self, inline: bool, result: Result<(), String>) {
        match self {
            Self::Notify(tx) => {
                if let Some(tx) = tx.take() {
                    let _ = tx.send(result);
                }
            }
            Self::Record(done) => {
                if let Some((done, band)) = done.take() {
                    // An inline `callDone` runs the completing pass on
                    // `dbCaTask` itself, at priorityMedium (`dbCa.c:340`).
                    let band = if inline {
                        crate::runtime::task::CallbackPriority::Medium
                    } else {
                        band
                    };
                    let _ = done.complete(band);
                }
            }
        }
    }
}

impl Drop for PutCompletion {
    fn drop(&mut self) {
        self.finish(
            false,
            Err("external link put dropped before completion (link queue torn down)".to_string()),
        );
    }
}

/// Who a staged put reports its completion to.
#[derive(Clone, Copy)]
pub(crate) enum PutWaiter<'a> {
    /// C `dbCaPutLink`: nobody.
    None,
    /// `dbCaPutLinkCallback`: the receiver [`LinkPutQueue::stage_put`]
    /// returns.
    Notify,
    /// C `dbPutLinkAsync` from a `devXxxSoftCallback` (`dbCaPutAsync`,
    /// `dbCa.c:537-542`): the record's own completion, on the given band.
    Record(
        &'a super::DeviceDone,
        crate::runtime::task::CallbackPriority,
    ),
}

/// One link's pending value — `pca->pputNative` plus the `putType` /
/// `putCallback` pair that names its flavour (`dbCaPvt.h:81-84`).
struct StagedPut {
    value: EpicsValue,
    op: LinkPutOp,
    /// `Some` only for [`LinkPutOp::Async`] — C's `CA_PUT_CALLBACK`.
    completion: Option<PutCompletion>,
}

impl StagedPut {
    /// Drop a value whose link was removed before it was sent.
    fn removed(self) {
        if let Some(c) = self.completion {
            c.removed();
        }
    }
}

/// What the owner sends of a [`StagedPut`]; its completion stays with the
/// link ([`LinkState::InFlight`]).
struct WireWrite {
    value: EpicsValue,
    op: LinkPutOp,
}

/// What the owner's write of one value came to.
struct Written {
    /// [`LinkPutDone::Inline`].
    inline: bool,
    result: Result<(), String>,
    /// A `ca` put left unsent: `newOutNative` stays set.
    unsent: bool,
}

impl PutCompletion {
    fn removed(self) {
        self.resolve(
            false,
            Err("external link put dropped: the link was removed before it was sent".to_string()),
        );
    }
}

/// Per-link state. Membership of the ready deque is exactly `Queued`, so the
/// deque cannot disagree with the map: the illegal combinations
/// ("in the deque but in flight", "staged but nobody will pick it up") are
/// not representable.
enum LinkState {
    /// Staged and waiting for the owner. In the ready deque exactly once.
    Queued(StagedPut),
    /// The owner took the value; a network op is running. Not in the deque.
    /// Holds the op's completion: C's `pca->putCallback`, which `putComplete`
    /// takes when the server answers and `caLinkDec` takes when the link is
    /// removed first (`dbCa.c:1056-1072`, `:184-197`).
    InFlight(Option<PutCompletion>),
    /// A network op is running AND a newer value was staged behind it.
    /// Not in the deque; `finish` moves it back to `Queued` and enqueues.
    InFlightRestaged(Option<PutCompletion>, StagedPut),
}

/// A `caLink`'s counters outlive its staged values: one per link field and
/// channel, holding the install it counts for.
type Site = (Option<super::RecordIdx>, &'static str, Channel);

fn site(key: &LinkKey) -> Site {
    (key.link.rec, key.link.field, key.channel.clone())
}

/// Per-link OPEN state — C's `CA_CONNECT` action, staged once per link by
/// `dbCaAddLink` (`dbCa.c:397-401`) and serviced on the `dbCaTask`
/// (`ca_create_channel` + `ca_add_array_event`).
///
/// `Done` is terminal on purpose: C stages exactly one `CA_CONNECT` per link
/// and libca owns every reconnection attempt from then on. Re-staging on each
/// cache miss would turn a down remote into one connect attempt per record
/// scan.
#[derive(PartialEq, Eq, Debug)]
enum OpenState {
    Queued,
    InFlight,
    Done,
}

#[derive(Default)]
struct QueueInner {
    links: HashMap<LinkKey, LinkState>,
    ready: VecDeque<LinkKey>,
    /// Each `ca` link whose last put the owner dropped unsent, with the
    /// install it was dropped on: C consumes the action bits before
    /// `dbCaTask` skips a link that is down (`dbCa.c:1195`, `:1225`) or logs
    /// a failed `ca_array_put` (`:1246`), and no connect re-arms them —
    /// `dbCaPutLinkCallback` never sets `pvlOptOutNative` (`dbCa.c:556`), so
    /// `connectionCallback` writes nothing (`:918-923`). `newOutNative`
    /// stays set, so the link's next put counts in `nNoWrite`
    /// (`dbCa.c:582-583`). Never pending for the `dbCaSync` barrier.
    unsent: HashMap<Site, LinkInstall>,
    opens: HashMap<Channel, OpenState>,
    ready_opens: VecDeque<(Channel, Option<super::CaLinkInit>)>,
    /// Per-link count of staged values a later put on the same link
    /// overwrote — C's `pca->nNoWrite` (`dbCa.c:548`, `:582`), which lives on
    /// the `caLink` and is therefore per link, not per IOC. Keyed rather than
    /// summed because `dbcar` prints it beside each link's name
    /// (`dbCaTest.c:111-116`) and a whole-queue total cannot be attributed
    /// back to one of them.
    ///
    /// One entry per link field and channel, holding the install it counts
    /// for: C's counter lives as long as the `caLink`, so it outlives the
    /// link going idle and starts over at a relink (`dbCa.c:398`).
    coalesced: HashMap<Site, (LinkInstall, u64)>,
    /// Number of writes the owner has completed. Diagnostic.
    completed: u64,
    /// Number of link opens the owner has completed. Diagnostic.
    opened: u64,
}

impl QueueInner {
    fn is_idle(&self) -> bool {
        self.links.is_empty()
            && !self
                .opens
                .values()
                .any(|s| matches!(s, OpenState::Queued | OpenState::InFlight))
    }
}

/// The `dbCaLink` worker's life, written only by [`LinkPutQueue::start_owner`]
/// and [`LinkPutQueue::shutdown`].
mod owner_state {
    /// No worker: before `dbCaLinkInit`. A put staged now waits for one.
    pub(super) const INIT: u8 = 0;
    /// The worker runs.
    pub(super) const RUN: u8 = 1;
    /// C `dbCaCtl == ctlExit` (`dbCa.c:329`): the worker drains what is
    /// ready and returns. A put staged afterwards is queued and never
    /// serviced, as C's `workList` is, until a rebuild's `dbCaLinkInit`.
    pub(super) const STOPPED: u8 = 2;
}

/// The queue itself. Lives in `PvDatabaseInner`; one owner task drains it.
pub(crate) struct LinkPutQueue {
    inner: parking_lot::Mutex<QueueInner>,
    /// Wakes the owner when a link becomes `Queued`. Held by the owner as an
    /// `Arc` so it can park on the signal without holding the queue itself
    /// alive — the queue must be free to drop when the database does.
    work: Arc<tokio::sync::Notify>,
    /// Wakes [`LinkPutQueue::sync`] waiters when the queue goes idle — the
    /// `dbCaSync` barrier (`dbCa.c:1126-1129`, `CA_SYNC`).
    idle: tokio::sync::Notify,
    /// The [`owner_state`] of the `dbCaLink` worker.
    state: std::sync::atomic::AtomicU8,
    /// The `dbCaLink` thread, joined by [`Self::shutdown`].
    owner: parking_lot::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Woken CP workers in wake order: `workList`'s links with a
    /// `CA_DBPROCESS` (`dbCa.c:132-161`).
    cp_ready: parking_lot::Mutex<VecDeque<Arc<CpSlot>>>,
    /// The reactor the network half of this queue needs, captured where one is
    /// known to exist — construction time, which for an IOC is inside
    /// `IocApplication::run` and for a test is inside its `#[tokio::test]`.
    ///
    /// The queue is the one place on the record-processing chain that cannot
    /// live on the background executor: `LinkSet::connect_link` for a CA
    /// target builds a `CaClient`, whose transport manager and search engine
    /// own `tokio::net` sockets. Staging, by contrast, happens on whatever
    /// thread processed the record — often a blocking connection thread with
    /// no runtime — so the capability has to be carried here rather than read
    /// off the staging thread. `None` means no runtime existed at all when the
    /// database was built, and then an external link cannot work under any
    /// arrangement — so it is refused rather than dispatched somewhere that
    /// would only move the panic; see [`LinkPutQueue::network`]. A purely
    /// local `LinkSet` is unaffected: it never reaches this queue.
    reactor: Option<crate::runtime::task::BlockingBridge>,
}

impl Default for LinkPutQueue {
    fn default() -> Self {
        Self {
            inner: parking_lot::Mutex::new(QueueInner::default()),
            work: Arc::new(tokio::sync::Notify::new()),
            idle: tokio::sync::Notify::new(),
            state: std::sync::atomic::AtomicU8::new(owner_state::INIT),
            owner: parking_lot::Mutex::new(None),
            cp_ready: parking_lot::Mutex::new(VecDeque::new()),
            reactor: crate::runtime::task::BlockingBridge::try_capture(),
        }
    }
}

impl Drop for LinkPutQueue {
    fn drop(&mut self) {
        // Teardown resolves every staged completion. `PutCompletion::drop`
        // does the resolving, so clearing the map is enough — and stays
        // correct if a future state variant carries a completion somewhere
        // this function would forget to visit.
        let inner = self.inner.get_mut();
        inner.links.clear();
        inner.unsent.clear();
        // Wake the owner so it observes the dead `Weak` and exits instead of
        // parking on a signal nobody will ever send again.
        self.work.notify_waiters();
    }
}

impl LinkPutQueue {
    /// The reactor every network future dispatched by this queue runs on —
    /// the single owner of "this future may touch a socket", so no other site
    /// in the queue has to know which executor it is on.
    ///
    /// `None` is the no-runtime-anywhere state described on
    /// [`Self::reactor`], and it is a *refusal*, never a fallback: there is no
    /// second executor that could do the work. The process-global background
    /// executor cannot — `runtime::task::spawn_background` documents that it
    /// deliberately has no `tokio::net` reactor — so sending a CA open or
    /// put there only moves the panic onto a callback-pool worker. Callers
    /// refuse instead; [`super::PvDatabase::external_put_gate`] turns that
    /// into the record's LINK alarm before anything is staged.
    pub(crate) fn network(&self) -> Option<&crate::runtime::task::BlockingBridge> {
        self.reactor.as_ref()
    }

    /// Stage `value` on `key`, coalescing latest-wins onto any pending value
    /// (C `dbCa.c:582-583`). Returns the completion receiver for a
    /// [`PutWaiter::Notify`] put — the caller awaits it, which is the
    /// `dbCaPutLinkCallback` route; [`PutWaiter::None`] gets `None` and the
    /// caller returns immediately, which is `dbCaPutLink`. A
    /// [`PutWaiter::Record`] put completes its record and returns `None`.
    pub(crate) fn stage_put(
        &self,
        key: LinkKey,
        value: EpicsValue,
        waiter: PutWaiter<'_>,
    ) -> Option<tokio::sync::oneshot::Receiver<Result<(), String>>> {
        let (op, completion, rx) = match waiter {
            PutWaiter::None => (LinkPutOp::Plain, None, None),
            PutWaiter::Notify => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                (
                    LinkPutOp::Async,
                    Some(PutCompletion::Notify(Some(tx))),
                    Some(rx),
                )
            }
            PutWaiter::Record(done, band) => (
                LinkPutOp::Async,
                Some(PutCompletion::Record(Some((done.clone(), band)))),
                None,
            ),
        };
        let staged = StagedPut {
            value,
            op,
            completion,
        };
        let wake = {
            let mut inner = self.inner.lock();
            // Empty unless a put was dropped: no hashing on the hot path.
            if !inner.unsent.is_empty()
                && inner
                    .unsent
                    .remove(&site(&key))
                    .is_some_and(|install| install == key.link.install)
            {
                Self::supersede(&mut inner, &key, None);
            }
            match inner.links.remove(&key) {
                None => {
                    inner.links.insert(key.clone(), LinkState::Queued(staged));
                    inner.ready.push_back(key);
                    true
                }
                Some(LinkState::Queued(prev)) => {
                    Self::supersede(&mut inner, &key, prev.completion);
                    // Already in the ready deque — C's `callAdd` is false
                    // here too (`dbCa.c:135`), so no second enqueue.
                    inner.links.insert(key, LinkState::Queued(staged));
                    false
                }
                Some(LinkState::InFlight(sent)) => {
                    inner
                        .links
                        .insert(key, LinkState::InFlightRestaged(sent, staged));
                    false
                }
                Some(LinkState::InFlightRestaged(sent, prev)) => {
                    Self::supersede(&mut inner, &key, prev.completion);
                    inner
                        .links
                        .insert(key, LinkState::InFlightRestaged(sent, staged));
                    false
                }
            }
        };
        if wake {
            self.work.notify_one();
        }
        rx
    }

    /// Resolve a displaced value's completion — the only place a staged put
    /// is superseded, so the `nNoWrite` accounting has one owner.
    fn supersede(inner: &mut QueueInner, key: &LinkKey, prev: Option<PutCompletion>) {
        let count = inner
            .coalesced
            .entry(site(key))
            .or_insert_with(|| (key.link.install.clone(), 0));
        if count.0 != key.link.install {
            *count = (key.link.install.clone(), 0);
        }
        count.1 += 1;
        if let Some(c) = prev {
            c.resolve(
                false,
                Err("external link put superseded by a newer put on the same link".to_string()),
            );
        }
    }

    /// Owner-only: take the next ready link, moving it `Queued` → `InFlight`.
    /// Mirrors `dbCaTask` popping `workList` and clearing `link_action`
    /// before doing the work (`dbCa.c:1115-1132`). A removed link's value is
    /// dropped instead (`dbCa.c:1200-1203`).
    fn take_ready(&self) -> Option<(LinkKey, WireWrite)> {
        let mut inner = self.inner.lock();
        let mut dropped = false;
        loop {
            let Some(key) = inner.ready.pop_front() else {
                let idle = dropped && inner.is_idle();
                drop(inner);
                if idle {
                    self.idle.notify_waiters();
                }
                return None;
            };
            match inner.links.remove(&key) {
                Some(LinkState::Queued(staged)) if key.link.install.removed() => {
                    staged.removed();
                    dropped = true;
                }
                Some(LinkState::Queued(StagedPut {
                    value,
                    op,
                    completion,
                })) => {
                    inner
                        .links
                        .insert(key.clone(), LinkState::InFlight(completion));
                    return Some((key, WireWrite { value, op }));
                }
                // Only `Queued` is ever in the deque; restore anything else
                // rather than silently dropping it.
                Some(other) => {
                    inner.links.insert(key, other);
                }
                None => {}
            }
        }
    }

    /// Owner-only: the write for `key` finished as `written` says.
    /// `InFlight` → idle, keeping `unsent`; `InFlightRestaged` → `Queued`
    /// and back in the deque, which is how a put that landed mid-flight is
    /// not lost (C clears `link_action` before the wire write for the same
    /// reason, `dbCa.c:1130`), superseding `unsent`. The write's completion
    /// is resolved unless [`Self::remove_link`] took it.
    fn finish(&self, key: LinkKey, written: Written) {
        let (sent, wake_work, wake_idle) = {
            let mut inner = self.inner.lock();
            inner.completed += 1;
            let (sent, wake_work) = match inner.links.remove(&key) {
                Some(LinkState::InFlight(sent)) => {
                    if written.unsent && !key.link.install.removed() {
                        inner.unsent.insert(site(&key), key.link.install.clone());
                    }
                    (sent, false)
                }
                None => (None, false),
                Some(LinkState::InFlightRestaged(sent, staged)) => {
                    if written.unsent {
                        Self::supersede(&mut inner, &key, None);
                    }
                    if key.link.install.removed() {
                        staged.removed();
                        (sent, false)
                    } else {
                        inner.links.insert(key.clone(), LinkState::Queued(staged));
                        inner.ready.push_back(key);
                        (sent, true)
                    }
                }
                // Cannot happen: only `finish` and `take_ready` move a key
                // out of `InFlight*`, and both run on the owner path.
                Some(other) => {
                    inner.links.insert(key, other);
                    (None, false)
                }
            };
            (sent, wake_work, inner.is_idle())
        };
        // A put callback dropped at `dbCa.c:1225` is left unanswered in C
        // until `caLinkDec` completes it on a relink (`dbCa.c:184-197`), so a
        // transient disconnect hangs the record PACT. That is a C defect, not
        // copied: the callback completes now, as `caLinkDec` completes it.
        if let Some(c) = sent {
            c.resolve(written.inline, written.result);
        }
        if wake_work {
            self.work.notify_one();
        }
        if wake_idle {
            self.idle.notify_waiters();
        }
    }

    /// C `dbCaRemoveLink` (`dbCa.c:431-446`) of every link `field` of `rec`
    /// had before `current`: its `CA_CLEAR_CHANNEL` reaches `caLinkDec`,
    /// which calls the `putCallback` still waiting on the server
    /// (`dbCa.c:184-197`), and the value staged behind it is never sent
    /// (`dbCa.c:1200-1203`). The write in flight runs on, and its answer
    /// completes nothing: `putComplete` finds `plink` cleared (`dbCa.c:1063`).
    pub(crate) fn remove_link(
        &self,
        rec: Option<super::RecordIdx>,
        field: &str,
        current: &LinkInstall,
    ) {
        let mut sent = Vec::new();
        let mut behind = Vec::new();
        {
            let mut inner = self.inner.lock();
            for (key, state) in inner.links.iter_mut() {
                if key.link.rec != rec
                    || !key.link.field.eq_ignore_ascii_case(field)
                    || key.link.install == *current
                {
                    continue;
                }
                match std::mem::replace(state, LinkState::InFlight(None)) {
                    LinkState::InFlight(c) => sent.extend(c),
                    LinkState::InFlightRestaged(c, staged) => {
                        sent.extend(c);
                        behind.push(staged);
                    }
                    // `take_ready` drops it.
                    queued @ LinkState::Queued(_) => *state = queued,
                }
            }
        }
        // `caLinkDec` runs on `dbCaTask`, so the record completes as an
        // inline callback would.
        for c in sent {
            c.resolve(
                true,
                Err(
                    "external link put dropped: the link was removed before it completed"
                        .to_string(),
                ),
            );
        }
        for staged in behind {
            staged.removed();
        }
    }

    /// Stage the OPEN of `key` — C `dbCaAddLink` (`dbCa.c:397-401`)
    /// delegates to `dbCaAddLinkCallback` (`:373-395`), whose one
    /// `addAction(pca, CA_CONNECT)` is `dbCa.c:393`. The checkout
    /// `R7.0.10-146-g8f5015b66` routes this through
    /// `dbCaAddLinkCallbackOpt` instead, which arrived with the iocInit
    /// connection-wait (`717d69e1f`, post-`R7.0.10` and in no tag) and does
    /// not exist at the pin. See `PvDatabase::external_link_targets`.
    /// Idempotent and
    /// terminal: a link that has ever been staged is never staged again, so a
    /// record scanning at 10 Hz against a down remote produces one connect
    /// attempt, not ten per second. Reconnection is the lset's job, as it is
    /// libca's in C.
    ///
    /// Returns true when this call is the one that staged it. `init` runs
    /// only then, before the owner can see the open, and what it returns
    /// rides with the open to the lsets.
    pub(crate) fn stage_open(
        &self,
        key: Channel,
        init: impl FnOnce() -> Option<super::CaLinkInit>,
    ) -> bool {
        let staged = {
            let mut inner = self.inner.lock();
            if inner.opens.contains_key(&key) {
                false
            } else {
                inner.opens.insert(key.clone(), OpenState::Queued);
                inner.ready_opens.push_back((key, init()));
                true
            }
        };
        if staged {
            self.work.notify_one();
        }
        staged
    }

    /// Owner-only: take the next link to open, `Queued` → `InFlight`.
    fn take_ready_open(&self) -> Option<(Channel, Option<super::CaLinkInit>)> {
        let mut inner = self.inner.lock();
        loop {
            let (key, init) = inner.ready_opens.pop_front()?;
            match inner.opens.get_mut(&key) {
                Some(state @ OpenState::Queued) => {
                    *state = OpenState::InFlight;
                    return Some((key, init));
                }
                // Only `Queued` is ever in the deque.
                _ => continue,
            }
        }
    }

    /// Owner-only: the open for `key` returned. `InFlight` → `Done`, or
    /// forgotten for a link removed meanwhile.
    fn finish_open(&self, key: Channel) {
        let wake_idle = {
            let mut inner = self.inner.lock();
            inner.opened += 1;
            if key.link.is_removed() {
                inner.opens.remove(&key);
            } else {
                inner.opens.insert(key, OpenState::Done);
            }
            inner.is_idle()
        };
        if wake_idle {
            self.idle.notify_waiters();
        }
    }

    /// True when nothing is staged and nothing is in flight.
    pub(crate) fn is_idle(&self) -> bool {
        self.inner.lock().is_idle()
    }

    /// Number of link opens the owner has completed. Diagnostic.
    pub(crate) fn opened_count(&self) -> u64 {
        self.inner.lock().opened
    }

    /// Number of staged values a later put on the same link overwrote,
    /// summed over every link — C's `pca->nNoWrite` (`dbCa.c:548`, `:582`)
    /// aggregated the way `dbcar`'s trailing `(%lu disconnects, %lu writes
    /// prohibited)` total does (`dbCaTest.c:153-154`).
    pub(crate) fn coalesced_count(&self) -> u64 {
        self.inner
            .lock()
            .coalesced
            .values()
            .filter(|(install, _)| !install.removed())
            .map(|(_, n)| n)
            .sum()
    }

    /// The same count for one link — the per-`caLink` `pca->nNoWrite`
    /// `dbcar` prints beside the link's name.
    pub(crate) fn coalesced_for(
        &self,
        rec: Option<super::RecordIdx>,
        field: &str,
        scheme: &str,
        name: &[u8],
    ) -> u64 {
        self.inner
            .lock()
            .coalesced
            .iter()
            .filter(|((r, f, c), (install, _))| {
                *r == rec
                    && *f == field
                    && c.scheme == scheme
                    && *c.name == *name
                    && !install.removed()
            })
            .map(|(_, (_, n))| *n)
            .sum()
    }

    /// C `dbCaRemoveLink`'s `CA_CLEAR_CHANNEL` (`dbCa.c:431-446`), which
    /// `dbCaTask` services ahead of every other action on the link
    /// (`dbCa.c:1200`): an open still queued is dropped, and what the queue
    /// keeps per link is forgotten. An open in flight is forgotten when it
    /// returns ([`Self::finish_open`]).
    pub(crate) fn retire(&self, link: super::LinkSerial) {
        let mut inner = self.inner.lock();
        inner
            .opens
            .retain(|c, state| c.link.serial() != link || *state == OpenState::InFlight);
        inner.ready_opens.retain(|(c, _)| c.link.serial() != link);
        inner
            .coalesced
            .retain(|(_, _, c), _| c.link.serial() != link);
        inner.unsent.retain(|(_, _, c), _| c.link.serial() != link);
    }

    /// Number of writes the owner has completed.
    pub(crate) fn completed_count(&self) -> u64 {
        self.inner.lock().completed
    }

    /// Block until the queue is idle — the `dbCaSync` barrier
    /// (`dbCa.c:1126-1129`: a `CA_SYNC` action the task answers only once it
    /// has drained everything ahead of it). Tests use it to make an enqueued
    /// put observable; `iocPause`-class callers use it to quiesce.
    pub(crate) async fn sync(&self) {
        loop {
            let notified = self.idle.notified();
            if self.is_idle() {
                return;
            }
            notified.await;
        }
    }

    /// Start the single owner task on its own `dbCaLink` thread at
    /// `epicsThreadPriorityMedium`, as C `dbCaLinkInitImpl` does
    /// (`dbCa.c:339-355`): from `INIT`, or from `STOPPED` for a rebuild, which
    /// gets a fresh worker as C's does. `db` is weak so the task cannot keep the database
    /// alive; it exits when the database is dropped. The network work it
    /// dispatches goes to [`Self::reactor`] — see that field.
    ///
    /// The call that starts it gets a latch that opens once the loop has
    /// registered with the watchdog — C `dbCaLinkInit` waits for exactly
    /// that (`dbCa.c:355-357`). `None` when the worker already runs.
    pub(crate) fn start_owner(
        self: &Arc<Self>,
        db: Weak<super::PvDatabaseInner>,
    ) -> Option<crate::runtime::sync::StartWait> {
        use std::sync::atomic::Ordering;
        let start = |from| {
            self.state
                .compare_exchange(from, owner_state::RUN, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        };
        if !start(owner_state::INIT) && !start(owner_state::STOPPED) {
            return None;
        }
        let queue = Arc::downgrade(self);
        let work = self.work.clone();
        let (started, wait) = crate::runtime::sync::start_latch();
        let thread = crate::runtime::task::MandatoryThread::new(
            "dbCaLink",
            crate::runtime::task::ThreadPriority::Medium,
            crate::runtime::task::StackSizeClass::Big,
        )
        .spawn(move || {
            if let Err(e) =
                crate::runtime::task::block_on_sync(owner_loop(queue, work, db, started))
            {
                unreachable!("a fresh dbCaLink thread can always block: {e:?}");
            }
        });
        *self.owner.lock() = Some(thread);
        Some(wait)
    }

    /// Poll `worker` on the `dbCaLink` thread from now on: C `dbCaTask`
    /// runs every link's `CA_DBPROCESS` (`dbCa.c:1314-1321`). It waits for
    /// the owner to start, as a staged put does. Dropping the handle drops
    /// the worker.
    pub(crate) fn spawn_cp_worker(
        self: &Arc<Self>,
        worker: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> CpWorker {
        let slot = Arc::new(CpSlot {
            worker: parking_lot::Mutex::new(Some(Box::pin(worker))),
            queued: std::sync::atomic::AtomicBool::new(false),
            dropped: std::sync::atomic::AtomicBool::new(false),
            queue: Arc::downgrade(self),
        });
        std::task::Wake::wake_by_ref(&slot);
        CpWorker(slot)
    }

    fn take_ready_cp(&self) -> Option<Arc<CpSlot>> {
        self.cp_ready.lock().pop_front()
    }

    /// C `dbCaShutdown` (`dbCa.c:324-333`): tell the owner to exit once it
    /// has drained what is ready, and join it. Nothing to stop when no owner
    /// runs.
    pub(crate) fn shutdown(&self) {
        use std::sync::atomic::Ordering;
        if self
            .state
            .compare_exchange(
                owner_state::RUN,
                owner_state::STOPPED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        self.work.notify_one();
        let thread = self.owner.lock().take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

/// The single consumer — `dbCaTask`'s loop (`dbCa.c:1105-1260`).
///
/// Per dequeued link it resolves the lset and spawns the network write,
/// keeping at most one write in flight per link (so per-link write ordering
/// is preserved) while distinct links proceed independently. C gets that for
/// free because `ca_array_put` only queues into libca's send buffer; ours has
/// to spawn because `LinkSet::put_value` for pva is a full PUT round
/// trip, and one slow target must not stall every other link's writes.
async fn owner_loop(
    queue: Weak<LinkPutQueue>,
    work: Arc<tokio::sync::Notify>,
    db: Weak<super::PvDatabaseInner>,
    started: crate::runtime::sync::StartSignal,
) {
    // C `dbCaTask` registers with the watchdog before its loop
    // (`dbCa.c:1096`) and removes when told to exit (`:1262`). Unbounded: the
    // loop parks on `work` whenever no link has anything staged, which is an
    // IOC with quiet output links rather than a fault.
    let _watched = crate::runtime::taskwd::taskwd_insert(
        "dbCaLink",
        crate::runtime::taskwd::CheckIn::Unbounded,
        None,
    );
    drop(started);
    let ca_run = db.upgrade().map(|inner| inner.ca_run.clone());
    loop {
        // Register on the wake signal BEFORE draining, so a put staged
        // during the drain cannot be missed.
        let notified = work.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let serviced = {
            let Some(q) = queue.upgrade() else { return };
            match q.network() {
                Some(reactor) => {
                    // Opens first: a pending open is what a blocked cached read
                    // is waiting on, and C services `CA_CONNECT` on the same
                    // task ahead of that link's other actions
                    // (`dbCa.c:1133-1160`).
                    while let Some((key, init)) = q.take_ready_open() {
                        let Some(inner) = db.upgrade() else { return };
                        let lset = resolve_lset(&inner, key.scheme);
                        drop(inner);
                        let q2 = q.clone();
                        reactor.spawn(async move {
                            match lset {
                                Some(lset) if !key.link.is_removed() => {
                                    lset.connect_link(key.link_ref(), init).await
                                }
                                _ => drop(init),
                            }
                            q2.finish_open(key);
                        });
                    }
                    while let Some((key, write)) = q.take_ready() {
                        let Some(inner) = db.upgrade() else { return };
                        let lset = resolve_lset(&inner, key.channel.scheme);
                        let holder = key
                            .link
                            .rec
                            .and_then(|rec| inner.record_table.get(rec))
                            .map(|cell| cell.name().clone());
                        drop(inner);
                        let q2 = q.clone();
                        reactor.spawn(async move {
                            let holder = holder.as_ref().map(|name| name.as_bytes());
                            let written = run_put(lset.as_ref(), &key, holder, write).await;
                            q2.finish(key, written);
                        });
                    }
                }
                // No reactor anywhere. Refuse what is here rather than
                // dispatch it — see [`LinkPutQueue::network`]. Reachable only
                // through a direct `stage_put`/`stage_open`, because
                // `external_put_gate` refuses this database's external puts
                // before they are staged; draining here so a caller that
                // bypassed the gate still gets its completion resolved rather
                // than left pending until teardown.
                None => {
                    while let Some((key, _init)) = q.take_ready_open() {
                        q.finish_open(key);
                    }
                    while let Some((key, _)) = q.take_ready() {
                        let written = refuse_for_want_of_a_reactor(&key);
                        q.finish(key, written);
                    }
                }
            }
            // `dbCaTask` is paused until `dbCaRun`, and a first event's
            // `CA_DBPROCESS` is queued behind every link's monitor queued by
            // then (`dbCa.c:1171-1173`, `:1209-1236`): the queue waits whole,
            // in wake order.
            let serviced = ca_run.as_ref().is_none_or(|gate| gate.serviced());
            if serviced {
                while let Some(slot) = q.take_ready_cp() {
                    slot.poll();
                }
            }
            // C `dbCaTask` leaves once its `workList` is empty and `dbCaCtl`
            // is `ctlExit` (`dbCa.c:1187`).
            if q.state.load(std::sync::atomic::Ordering::Acquire) != owner_state::RUN {
                return;
            }
            // The strong reference must not survive the await below, or the
            // queue could never drop and its teardown would never run.
            serviced
        };
        // The poll's read, not a fresh one: a gate serviced since the poll
        // skipped the CP slots ends `monitors_serviced` at once, where a
        // fresh read would park on `work`, which their queued wakes never
        // signal again.
        match ca_run.as_ref().filter(|_| !serviced) {
            Some(gate) => {
                let serviced = gate.monitors_serviced();
                tokio::pin!(serviced);
                std::future::poll_fn(|cx| {
                    if notified.as_mut().poll(cx).is_ready()
                        || serviced.as_mut().poll(cx).is_ready()
                    {
                        std::task::Poll::Ready(())
                    } else {
                        std::task::Poll::Pending
                    }
                })
                .await
            }
            None => notified.await,
        }
    }
}

type CpFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// One link's CP worker. Only the owner polls it, so it never runs on the
/// thread that woke it.
struct CpSlot {
    /// Out of the slot while the owner polls it.
    worker: parking_lot::Mutex<Option<CpFuture>>,
    /// In `cp_ready`: `addAction`'s `link_action != 0` (`dbCa.c:135`).
    queued: std::sync::atomic::AtomicBool,
    dropped: std::sync::atomic::AtomicBool,
    queue: Weak<LinkPutQueue>,
}

impl CpSlot {
    fn poll(self: &Arc<Self>) {
        use std::sync::atomic::Ordering;
        // Cleared first, so a wake during the poll queues it again.
        self.queued.store(false, Ordering::SeqCst);
        let Some(mut worker) = self.worker.lock().take() else {
            return;
        };
        let waker = std::task::Waker::from(self.clone());
        let done = worker
            .as_mut()
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_ready();
        // Decided under the lock `CpWorker::drop` takes, so a drop during
        // the poll cannot miss the worker.
        let mut slot = self.worker.lock();
        if !done && !self.dropped.load(Ordering::SeqCst) {
            *slot = Some(worker);
        }
    }
}

impl std::task::Wake for CpSlot {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if self.queued.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        if let Some(queue) = self.queue.upgrade() {
            queue.cp_ready.lock().push_back(self.clone());
            queue.work.notify_one();
        }
    }
}

/// A link's CP worker on the `dbCaLink` thread
/// (`LinkPutQueue::spawn_cp_worker`); dropping it drops the worker.
pub struct CpWorker(Arc<CpSlot>);

impl Drop for CpWorker {
    fn drop(&mut self) {
        self.0
            .dropped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let worker = self.0.worker.lock().take();
        drop(worker);
    }
}

/// Offer `f` the one lset `scheme` names, if registered. Single owner of the
/// scheme → lset mapping, shared with `PvDatabase::resolve_external_pv` so a
/// link's cached read and its staged open address the same lset. Allocates
/// nothing: a cycle asks it.
pub(super) fn find_lset<T>(
    inner: &super::PvDatabaseInner,
    scheme: &str,
    f: impl FnOnce(&DynLinkSet) -> Option<T>,
) -> Option<T> {
    inner.link_sets.load().get_ref(scheme).and_then(f)
}

/// [`find_lset`]'s lset, held past the registry snapshot for an await.
pub(super) fn resolve_lset(inner: &super::PvDatabaseInner, scheme: &str) -> Option<DynLinkSet> {
    find_lset(inner, scheme, |lset| Some(lset.clone()))
}

/// Resolve a staged write that no owner can perform, because the database
/// captured no reactor ([`LinkPutQueue::network`]).
///
/// The failure is reported the way C reports a put it could not deliver — on
/// the task, through `errlogPrintf` (`db/dbCa.c:1111-1114`
/// (`dbCaTask`); epics-base R7.0.10) — and the completion is resolved so the
/// writer unblocks with an error instead of waiting for a wire write
/// that will never be attempted.
fn refuse_for_want_of_a_reactor(key: &LinkKey) -> Written {
    let msg = format!(
        "external link put to '{}' refused: this database was built with no \
         tokio runtime, so no link set can reach the network",
        key.channel.name
    );
    crate::runtime::log::errlog_vprintf(format_args!("dbCa: {msg}\n"));
    Written {
        inline: false,
        result: Err(msg),
        unsent: false,
    }
}

/// Perform one dequeued write. The network suspension lives here — on the
/// owner's spawned task, never on a record-processing thread.
/// The lset reports its own failure, as dbCa and pvxs each do.
async fn run_put(
    lset: Option<&DynLinkSet>,
    key: &LinkKey,
    holder: Option<&[u8]>,
    write: WireWrite,
) -> Written {
    let WireWrite { value, op } = write;
    let (done, result) = match lset {
        Some(lset) => {
            lset.put_value_done(key.channel.link_ref(), holder, value, op)
                .await
        }
        None => (
            LinkPutDone::Callback,
            Err(format!(
                "no link set accepted the write to external link '{}'",
                key.channel.name
            )),
        ),
    };
    let unsent = match op {
        LinkPutOp::Async => done == LinkPutDone::NotIssued,
        LinkPutOp::Plain => result.is_err(),
    };
    Written {
        inline: done == LinkPutDone::Inline,
        result,
        // `newOutNative` is dbCa's; pvxs keeps no put a disconnect cancelled.
        unsent: unsent && key.channel.scheme == "ca",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    fn coalesced(q: &LinkPutQueue, k: &LinkKey) -> u64 {
        q.coalesced_for(k.link.rec, k.link.field, k.channel.scheme, &k.channel.name)
    }

    /// One link identity per name per test thread, so a key keeps one
    /// channel across calls.
    fn latch(name: &str) -> crate::server::record::OpenLatch {
        thread_local! {
            static LATCHES: std::cell::RefCell<HashMap<String, crate::server::record::OpenLatch>> =
                Default::default();
        }
        LATCHES.with(|l| l.borrow_mut().entry(name.into()).or_default().clone())
    }

    /// One parse per test thread stands for every link field the tests name,
    /// alive for the thread, so a key keeps one identity across calls.
    fn installed() -> Arc<crate::server::record::ParsedLink> {
        thread_local! {
            static LINK: Arc<crate::server::record::ParsedLink> =
                Arc::new(crate::server::record::ParsedLink::None);
        }
        LINK.with(Arc::clone)
    }

    fn key_on(name: &str, link: &Arc<crate::server::record::ParsedLink>) -> LinkKey {
        LinkKey {
            link: LinkId {
                rec: None,
                field: "OUT",
                install: LinkInstall::Installed(Arc::downgrade(link)),
            },
            channel: Channel::new("ca", name.as_bytes(), &latch(name)),
        }
    }

    fn key(name: &str) -> LinkKey {
        key_on(name, &installed())
    }

    /// A write the lset answered, `unsent` or sent.
    fn written(unsent: bool) -> Written {
        Written {
            inline: false,
            result: Ok(()),
            unsent,
        }
    }

    /// Read a completion out of its receiver with no runtime at all.
    ///
    /// The resolutions these tests assert — supersession and teardown — are
    /// performed *synchronously*, inside `stage_put` and inside `Drop`, so
    /// the receiver is `Ready` on its first poll and no reactor is involved.
    /// Polling by hand rather than `.await`ing says exactly that: `Pending`
    /// means the completion was left unresolved, which is the invariant
    /// under test, and it fails here instead of hanging. It also keeps both
    /// tests running on the exec backend and adds no site to this file's
    /// RTEMS-EXEC-MODEL-ALLOW census.
    fn resolved(rx: tokio::sync::oneshot::Receiver<Result<(), String>>) -> Result<(), String> {
        let mut rx = std::pin::pin!(rx);
        match rx.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(out) => out.expect("completion resolved, not dropped"),
            Poll::Pending => panic!("completion was left unresolved"),
        }
    }

    /// Boundary: two plain puts staged on ONE link before the owner picks it
    /// up. C keeps a single `pputNative` buffer, so the second overwrites the
    /// first and only bumps `nNoWrite` (`dbCa.c:582-583`) — latest-wins, and
    /// the link is enqueued once (`callAdd = (pca->link_action == 0)`,
    /// `dbCa.c:135`). This is C's answer to "what happens when the queue is
    /// full": there is no queue to fill.
    #[test]
    fn second_put_on_same_link_coalesces_latest_wins() {
        let q = LinkPutQueue::default();
        assert!(
            q.stage_put(key("A"), EpicsValue::Long(1), PutWaiter::None)
                .is_none()
        );
        assert!(
            q.stage_put(key("A"), EpicsValue::Long(2), PutWaiter::None)
                .is_none()
        );

        assert_eq!(q.coalesced_count(), 1, "one put must be coalesced away");
        let (k, staged) = q.take_ready().expect("one ready link");
        assert_eq!(k, key("A"));
        assert_eq!(staged.value, EpicsValue::Long(2), "latest value wins");
        assert!(
            q.take_ready().is_none(),
            "the link is enqueued exactly once"
        );
    }

    /// Boundary: the count is attributed to the LINK that lost the value,
    /// not to the queue. C keeps `nNoWrite` on the `caLink` (`dbCa.c:548`,
    /// `:582`) and `dbcar` prints it beside that link's name, so a second
    /// link's supersessions must not show up against the first.
    #[test]
    fn the_coalesce_count_is_attributed_per_link() {
        let q = LinkPutQueue::default();
        for v in [1, 2, 3] {
            q.stage_put(key("A"), EpicsValue::Long(v), PutWaiter::None);
        }
        q.stage_put(key("B"), EpicsValue::Long(9), PutWaiter::None);

        assert_eq!(coalesced(&q, &key("A")), 2);
        assert_eq!(coalesced(&q, &key("B")), 0);
        // Never staged at all — C's `pca == NULL`, zero rather than absent.
        assert_eq!(coalesced(&q, &key("C")), 0);
        assert_eq!(q.coalesced_count(), 2, "the total is still the sum");
    }

    /// Boundary: two link fields naming ONE channel. C gives each `DBLINK` its
    /// own `caLink` and so its own `pputNative` (`dbCa.c:398-414`): neither
    /// put supersedes the other, and both are written.
    #[test]
    fn two_links_to_one_channel_do_not_coalesce() {
        let q = LinkPutQueue::default();
        let other = LinkKey {
            link: LinkId {
                field: "OUTB",
                ..key("A").link
            },
            ..key("A")
        };
        q.stage_put(key("A"), EpicsValue::Long(1), PutWaiter::None);
        q.stage_put(other.clone(), EpicsValue::Long(2), PutWaiter::None);

        assert_eq!(q.coalesced_count(), 0);
        let (k1, s1) = q.take_ready().expect("first link ready");
        let (k2, s2) = q.take_ready().expect("second link ready");
        assert_eq!((k1, s1.value), (key("A"), EpicsValue::Long(1)));
        assert_eq!((k2, s2.value), (other, EpicsValue::Long(2)));
    }

    /// Boundary: the per-link count SURVIVES the link going idle. C's counter
    /// lives on the `caLink`, which outlives any one staged value, so a link
    /// that drained and was written to again must not report a fresh zero.
    #[test]
    fn the_coalesce_count_outlives_the_links_pending_state() {
        let q = LinkPutQueue::default();
        q.stage_put(key("A"), EpicsValue::Long(1), PutWaiter::None);
        q.stage_put(key("A"), EpicsValue::Long(2), PutWaiter::None);
        assert_eq!(coalesced(&q, &key("A")), 1);
        let (k, _staged) = q.take_ready().expect("ready");
        q.finish(k, written(false));
        assert!(q.is_idle(), "nothing pending on any link");
        assert_eq!(coalesced(&q, &key("A")), 1);
    }

    /// Boundary: distinct links do NOT coalesce — one ready entry each, in
    /// staging order (`workList` is a FIFO: `ellAdd` / `ellGet`).
    #[test]
    fn distinct_links_keep_their_own_pending_value() {
        let q = LinkPutQueue::default();
        q.stage_put(key("A"), EpicsValue::Long(1), PutWaiter::None);
        q.stage_put(key("B"), EpicsValue::Long(2), PutWaiter::None);
        assert_eq!(q.coalesced_count(), 0);
        assert_eq!(q.take_ready().expect("A first").0, key("A"));
        assert_eq!(q.take_ready().expect("B second").0, key("B"));
        assert!(q.take_ready().is_none());
    }

    /// Boundary: a put staged while the link is IN FLIGHT is not lost — it
    /// becomes ready again when the in-flight write finishes, and never
    /// overtakes it. C gets this by clearing `link_action` before the wire
    /// write (`dbCa.c:1130`), so a concurrent `addAction` re-appends the link.
    #[test]
    fn put_staged_during_flight_is_requeued_on_finish() {
        let q = LinkPutQueue::default();
        q.stage_put(key("A"), EpicsValue::Long(1), PutWaiter::None);
        let (k, first) = q.take_ready().expect("ready");
        assert_eq!(first.value, EpicsValue::Long(1));
        assert!(q.take_ready().is_none(), "in flight, nothing ready");

        q.stage_put(key("A"), EpicsValue::Long(9), PutWaiter::None);
        assert!(
            q.take_ready().is_none(),
            "still in flight — at most one write per link keeps write order"
        );

        q.finish(k, written(false));
        let (_, second) = q.take_ready().expect("re-queued after finish");
        assert_eq!(second.value, EpicsValue::Long(9));
    }

    /// Boundary: the completion flavour is resolved — never dropped — when a
    /// newer put supersedes it. C overwrites `pca->putCallback`
    /// (`dbCa.c:585-592`) and the earlier callback never fires; we resolve it
    /// with an error instead, so no pending put loses its classification.
    #[test]
    fn superseded_completion_is_resolved_not_dropped() {
        let q = LinkPutQueue::default();
        let rx = q
            .stage_put(key("A"), EpicsValue::Long(1), PutWaiter::Notify)
            .expect("Async put yields a completion");
        q.stage_put(key("A"), EpicsValue::Long(2), PutWaiter::Notify);

        assert!(
            resolved(rx).unwrap_err().contains("superseded"),
            "a superseded completion must say so"
        );
    }

    /// Boundary: the put that could not be sent already has a newer one
    /// staged behind it, which counts it in `nNoWrite` at once.
    #[test]
    fn an_unsent_put_is_counted_by_the_put_staged_behind_it() {
        let q = LinkPutQueue::default();
        q.stage_put(key("A"), EpicsValue::Long(1), PutWaiter::None);
        let (k, _) = q.take_ready().expect("ready");
        q.stage_put(key("A"), EpicsValue::Long(2), PutWaiter::None);
        q.finish(k, written(true));

        assert_eq!(coalesced(&q, &key("A")), 1);
        let (k, staged) = q.take_ready().expect("the newer put is queued");
        assert_eq!(staged.value, EpicsValue::Long(2));
        q.finish(k, written(false));
        q.stage_put(key("A"), EpicsValue::Long(3), PutWaiter::None);
        assert_eq!(coalesced(&q, &key("A")), 1, "counted once");
    }

    /// Boundary: a plain put the lset could not send is dropped — nothing is
    /// queued again, and it holds up no `dbCaSync` — but it leaves
    /// `newOutNative` set, so the link's next put counts in `nNoWrite`
    /// (`dbCa.c:582-583`, `:1225`, `:1246`).
    #[test]
    fn an_unsent_plain_put_is_dropped_and_counted_by_the_next_put() {
        let q = LinkPutQueue::default();
        q.stage_put(key("A"), EpicsValue::Long(1), PutWaiter::None);
        let (k, _) = q.take_ready().expect("ready");
        q.finish(k, written(true));
        assert!(q.is_idle(), "a dropped put is not pending for dbCaSync");
        assert!(
            q.take_ready().is_none(),
            "a dropped put is not queued again"
        );

        q.stage_put(key("A"), EpicsValue::Long(2), PutWaiter::None);
        assert_eq!(coalesced(&q, &key("A")), 1);
        let (k, staged) = q.take_ready().expect("the newer put is queued");
        assert_eq!(staged.value, EpicsValue::Long(2));

        q.finish(k, written(false));
        q.stage_put(key("A"), EpicsValue::Long(3), PutWaiter::None);
        assert_eq!(coalesced(&q, &key("A")), 1, "a sent put is not counted");
    }

    /// Boundary: a relink makes a new `caLink` (`dbCa.c:398`), so a put
    /// left unsent on the old one is not counted by the new link's first put.
    #[test]
    fn a_relink_starts_the_unsent_mark_over() {
        let q = LinkPutQueue::default();
        let old = Arc::new(crate::server::record::ParsedLink::None);
        q.stage_put(key_on("A", &old), EpicsValue::Long(1), PutWaiter::None);
        let (k, _) = q.take_ready().expect("ready");
        q.finish(k, written(true));
        drop(old);

        let new = Arc::new(crate::server::record::ParsedLink::None);
        q.stage_put(key_on("A", &new), EpicsValue::Long(2), PutWaiter::None);
        assert_eq!(coalesced(&q, &key_on("A", &new)), 0);
    }

    /// Boundary: a relink while the old link's put callback waits on the
    /// server. `caLinkDec` calls it then (`dbCa.c:184-197`), the value
    /// staged behind it is never sent, and the server's late answer
    /// completes nothing (`dbCa.c:1063`). Decided by the install, not by
    /// whether the old parse is still alive; the new link's put is untouched.
    #[test]
    fn a_relink_completes_the_put_callback_in_flight() {
        let q = LinkPutQueue::default();
        let old = Arc::new(crate::server::record::ParsedLink::None);
        let sent = q
            .stage_put(key_on("A", &old), EpicsValue::Long(1), PutWaiter::Notify)
            .expect("Async put yields a completion");
        let (k, _) = q.take_ready().expect("ready");
        let behind = q
            .stage_put(key_on("A", &old), EpicsValue::Long(2), PutWaiter::Notify)
            .expect("Async put yields a completion");
        let new = Arc::new(crate::server::record::ParsedLink::None);
        let next = q
            .stage_put(key_on("A", &new), EpicsValue::Long(3), PutWaiter::Notify)
            .expect("Async put yields a completion");

        q.remove_link(None, "OUT", &LinkInstall::Installed(Arc::downgrade(&new)));
        assert!(resolved(sent).unwrap_err().contains("before it completed"));
        assert!(resolved(behind).unwrap_err().contains("removed"));

        q.finish(k, written(false));
        let (k, write) = q.take_ready().expect("the new link's put is sent");
        assert_eq!(write.value, EpicsValue::Long(3));
        assert!(q.take_ready().is_none(), "nothing of the old link is sent");
        q.finish(k, written(false));
        assert_eq!(resolved(next), Ok(()));
        assert!(q.is_idle());
        drop(old);
    }

    /// Boundary: tearing the queue down resolves every pending completion.
    /// The `Drop` on `PutCompletion` is what makes this hold on *every* exit
    /// path, not just this one.
    #[test]
    fn teardown_resolves_pending_completions() {
        let q = LinkPutQueue::default();
        let rx = q
            .stage_put(key("A"), EpicsValue::Long(1), PutWaiter::Notify)
            .expect("Async put yields a completion");
        drop(q);
        assert!(
            resolved(rx).is_err(),
            "a torn-down pending put must not report success"
        );
    }

    /// Boundary: a plain put yields no completion channel at all — it is
    /// fire-and-forget by type, so there is no receiver a caller could
    /// accidentally await. C's `dbCaPutLink` passes a null callback
    /// (`dbCa.c:601`).
    #[test]
    fn plain_put_has_no_completion() {
        let q = LinkPutQueue::default();
        assert!(
            q.stage_put(key("A"), EpicsValue::Long(1), PutWaiter::None)
                .is_none()
        );
    }

    /// Boundary: idle accounting. The queue is idle before anything is
    /// staged, busy while a write is in flight, idle again after `finish`.
    #[test]
    fn idle_tracks_staged_and_in_flight() {
        let q = LinkPutQueue::default();
        assert!(q.is_idle());
        q.stage_put(key("A"), EpicsValue::Long(1), PutWaiter::None);
        assert!(!q.is_idle(), "staged but not run");
        let (k, _) = q.take_ready().expect("ready");
        assert!(!q.is_idle(), "in flight");
        q.finish(k, written(false));
        assert!(q.is_idle());
        assert_eq!(q.completed_count(), 1);
    }

    /// Boundary: a value staged on a link that is then relinked to the SAME
    /// name, while the owner has not taken it. C's relink is
    /// `dbCaRemoveLink` + `dbCaAddLink`: the old `caLink` gets
    /// `CA_CLEAR_CHANNEL`, which the task services first and alone
    /// (`dbCa.c:1200-1203`), so the pending value is never written, and the
    /// new `caLink` starts with no pending value and `nNoWrite` 0.
    #[test]
    fn a_relink_drops_the_value_staged_through_the_old_link() {
        let q = LinkPutQueue::default();
        let old = Arc::new(crate::server::record::ParsedLink::None);
        let rx = q
            .stage_put(key_on("A", &old), EpicsValue::Long(1), PutWaiter::Notify)
            .expect("Async put yields a completion");
        q.stage_put(key_on("A", &old), EpicsValue::Long(2), PutWaiter::None);
        assert_eq!(coalesced(&q, &key_on("A", &old)), 1);
        let new = Arc::new(crate::server::record::ParsedLink::None);
        drop(old);

        assert!(q.take_ready().is_none(), "the old link's value is dropped");
        assert!(q.is_idle());
        assert!(resolved(rx).unwrap_err().contains("superseded"));
        assert_eq!(coalesced(&q, &key_on("A", &new)), 0, "a new caLink");
        assert_eq!(q.coalesced_count(), 0);

        q.stage_put(key_on("A", &new), EpicsValue::Long(3), PutWaiter::None);
        let (_, staged) = q.take_ready().expect("the new link's value is sent");
        assert_eq!(staged.value, EpicsValue::Long(3));
    }

    /// Boundary: the same relink while the old link has a write IN FLIGHT and
    /// a newer value staged behind it. The in-flight write is C's
    /// `ca_array_put` already issued; the value behind it is dropped.
    #[test]
    fn a_relink_drops_the_value_restaged_behind_a_write_in_flight() {
        let q = LinkPutQueue::default();
        let old = Arc::new(crate::server::record::ParsedLink::None);
        q.stage_put(key_on("A", &old), EpicsValue::Long(1), PutWaiter::None);
        let (k, first) = q.take_ready().expect("ready");
        assert_eq!(first.value, EpicsValue::Long(1));
        let rx = q
            .stage_put(key_on("A", &old), EpicsValue::Long(2), PutWaiter::Notify)
            .expect("Async put yields a completion");
        drop(old);

        q.finish(k, written(false));
        assert!(q.take_ready().is_none(), "the restaged value is dropped");
        assert!(q.is_idle());
        assert!(resolved(rx).unwrap_err().contains("removed"));
    }
}
