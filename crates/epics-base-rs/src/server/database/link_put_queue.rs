//! External-link OUT-write queue — the Rust counterpart of `dbCa`'s
//! `workList` (`modules/database/src/ioc/db/dbCa.c`).
//!
//! # The C model this mirrors
//!
//! C never performs a Channel Access network write inside `dbScanLock`.
//! `dbCaPutLink` (`dbCa.c:627-631`) forwards to `dbCaPutLinkCallback`
//! (`dbCa.c:544-625`), which — under the per-link `pca->lock`, *not* the
//! record lock —
//!
//! 1. refuses the put outright when the link is not connected or has no
//!    write access, returning `-1` with nothing staged (`dbCa.c:558-561`);
//! 2. converts the value into the link's single pending-value buffer
//!    `pca->pputNative` / `pca->pputString` (`dbCa.c:562-613`);
//! 3. records the delivery flavour — `CA_PUT` for a plain put,
//!    `CA_PUT_CALLBACK` plus the completion callback for
//!    `dbCaPutLinkCallback` (`dbCa.c:614-621`);
//! 4. calls `addAction(pca, CA_WRITE_NATIVE)` (`dbCa.c:132-161`), which ORs
//!    the action bit, appends the link to `workList` **once**
//!    (`callAdd = (pca->link_action == 0)`, `dbCa.c:137`, `:155-157`) and
//!    signals `workListEvent`;
//! 5. **returns**.
//!
//! The network write happens later on the `dbCaTask` thread
//! (`dbCa.c:1158-1333`): it pops one link, clears `link_action` *before*
//! doing the work (`dbCa.c:1195`) so a put arriving mid-flight re-queues the
//! link, and issues `ca_array_put` (plain) or `ca_array_put_callback`
//! (completion flavour) at `dbCa.c:1226-1248`.
//!
//! Three consequences this module reproduces verbatim:
//!
//! * **Latest-wins per link, not a per-put queue.** `pputNative` is one
//!   buffer. A second put staged before the task services the link
//!   overwrites the first and only bumps a counter
//!   (`if (pca->newOutNative) pca->nNoWrite++;`, `dbCa.c:611-612`; the
//!   string twin at `:577-578`). There is therefore no "queue full" state
//!   in C to match — the bound is one pending value per link, times the
//!   number of links, and that is the bound here too.
//! * **The link is enqueued at most once.** `addAction`'s `callAdd` gate
//!   (`dbCa.c:137`) appends the `caLink` to `workList` only when it carries
//!   no pending action; a put on an already-queued link just ORs the bit.
//! * **The returned status is the *staging* status, never the network
//!   status.** `dbCaPutLinkCallback` returns the conversion status (or the
//!   `-1` refusal); the outcome of the wire write is reported only through
//!   `errlogPrintf` on the task (`dbCa.c:1240-1244`) or, for the completion
//!   flavour, through `putComplete` (`dbCa.c:1056-1074`). C's
//!   `dbPutLink` → `setLinkAlarm` gate (`dbLink.c:434-448`) therefore alarms
//!   on *refusal to stage*, not on a failed transmission.
//!
//! # What this module owns
//!
//! One [`LinkPutQueue`] per [`super::PvDatabase`]. A record-processing
//! thread stages a write and returns; a single owner task drains the queue
//! and performs the actual `LinkSet::put_value` / `LinkSet::flush_puts`
//! away from the record's advisory write gate.
//!
//! **Invariant.** *Every staged completion is resolved exactly once* —
//! by the owner (with the lset's result), by a superseding put, or by the
//! queue's teardown. [`PutCompletion`] enforces it by construction: its
//! `Drop` resolves an unresolved completion, so no path — early return,
//! `?`, panic, or the whole database dropping — can drop a pending put
//! silently.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Weak;

use super::link_set::{DynLinkSet, LinkPutOp};
use crate::types::EpicsValue;

/// Which lset a staged write is bound to.
///
/// `Scheme(s)` is a `pva://`/`ca://` prefixed link — the write goes to that
/// one lset. `Any` is the bare-name form (the body stored in
/// `ParsedLink::Ca`/`Pva` once `record/link.rs` has stripped the scheme):
/// every registered lset is tried in turn, first to accept wins, which is
/// what the pre-queue `write_external_pv` did.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) enum LinkTarget {
    Scheme(String),
    Any,
}

/// Identity of one external link: the lset it belongs to plus the name that
/// lset is addressed with. The unit of latest-wins coalescing, exactly as
/// `caLink` is in C.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct LinkKey {
    pub(crate) target: LinkTarget,
    pub(crate) name: String,
}

/// The completion half of a staged put — the `pca->putCallback` slot
/// (`dbCa.c:83`, set at `:614-617`, consumed by `putComplete` at
/// `:1066-1073`).
///
/// Unlike C, which simply overwrites `putCallback` when a second
/// `dbCaPutLinkCallback` lands before the first completes — dropping the
/// earlier callback with no notification (`dbCa.c:614-621`) — a
/// `PutCompletion` cannot be lost: `Drop` resolves it. That is strictly
/// stronger than C and is what makes "no pending put loses its
/// classification silently" hold by construction rather than by review.
struct PutCompletion(Option<tokio::sync::oneshot::Sender<Result<(), String>>>);

impl PutCompletion {
    /// Resolve with the outcome the owner (or a superseding put) determined.
    fn resolve(mut self, result: Result<(), String>) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(result);
        }
    }
}

impl Drop for PutCompletion {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(Err(
                "external link put dropped before completion (link queue torn down)".to_string(),
            ));
        }
    }
}

/// One link's pending value — `pca->pputNative` plus the `putType` /
/// `putCallback` pair that names its flavour (`dbCa.c:81-84`).
struct StagedPut {
    value: EpicsValue,
    op: LinkPutOp,
    /// `Some` only for [`LinkPutOp::Async`] — C's `CA_PUT_CALLBACK`.
    completion: Option<PutCompletion>,
}

/// Per-link state. Membership of the ready deque is exactly `Queued`, so the
/// deque cannot disagree with the map: the illegal combinations
/// ("in the deque but in flight", "staged but nobody will pick it up") are
/// not representable.
enum LinkState {
    /// Staged and waiting for the owner. In the ready deque exactly once.
    Queued(StagedPut),
    /// The owner took the value; a network op is running. Not in the deque.
    InFlight,
    /// A network op is running AND a newer value was staged behind it.
    /// Not in the deque; `finish` moves it back to `Queued` and enqueues.
    InFlightRestaged(StagedPut),
}

/// Per-link OPEN state — C's `CA_CONNECT` action, staged once per link by
/// `dbCaAddLink` (`dbCa.c:735-800`) and serviced on the `dbCaTask`
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
    opens: HashMap<LinkKey, OpenState>,
    ready_opens: VecDeque<LinkKey>,
    /// Number of staged values a later put on the same link overwrote — the
    /// counterpart of C's `pca->nNoWrite` (`dbCa.c:611-612`). Diagnostic.
    coalesced: u64,
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

/// The queue itself. Lives in `PvDatabaseInner`; one owner task drains it.
pub(crate) struct LinkPutQueue {
    inner: parking_lot::Mutex<QueueInner>,
    /// Wakes the owner when a link becomes `Queued`. Held by the owner as an
    /// `Arc` so it can park on the signal without holding the queue itself
    /// alive — the queue must be free to drop when the database does.
    work: Arc<tokio::sync::Notify>,
    /// Wakes [`LinkPutQueue::sync`] waiters when the queue goes idle — the
    /// `dbCaSync` barrier (`dbCa.c:1191-1194`, `CA_SYNC`).
    idle: tokio::sync::Notify,
    /// Set once the owner task has been spawned.
    owner_started: std::sync::atomic::AtomicBool,
    /// The reactor the network half of this queue needs, captured where one is
    /// known to exist — construction time, which for an IOC is inside
    /// `IocApplication::run` and for a test is inside its `#[tokio::test]`.
    ///
    /// The queue is the one place on the record-processing chain that cannot
    /// live on the background executor: `LinkSet::connect_link` for a `ca://`
    /// target builds a `CaClient`, whose transport manager and search engine
    /// own `tokio::net` sockets. Staging, by contrast, happens on whatever
    /// thread processed the record — often a blocking connection thread with
    /// no runtime — so the capability has to be carried here rather than read
    /// off the staging thread. `None` means no runtime existed at all when the
    /// database was built, and then an external link could not have worked
    /// under any arrangement; the tail falls back to the background executor
    /// so a purely local `LinkSet` still runs.
    reactor: Option<crate::runtime::task::BlockingBridge>,
}

impl Default for LinkPutQueue {
    fn default() -> Self {
        Self {
            inner: parking_lot::Mutex::new(QueueInner::default()),
            work: Arc::new(tokio::sync::Notify::new()),
            idle: tokio::sync::Notify::new(),
            owner_started: std::sync::atomic::AtomicBool::new(false),
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
        self.inner.get_mut().links.clear();
        // Wake the owner so it observes the dead `Weak` and exits instead of
        // parking on a signal nobody will ever send again.
        self.work.notify_waiters();
    }
}

impl LinkPutQueue {
    /// Dispatch one link's network work on the captured reactor — the single
    /// owner of "this future may touch a socket", so no other site in the
    /// queue has to know which executor it is on.
    fn spawn_network<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        match &self.reactor {
            Some(reactor) => {
                reactor.spawn(fut);
            }
            None => {
                crate::runtime::task::spawn_background(fut);
            }
        }
    }

    /// Stage `value` on `key`, coalescing latest-wins onto any pending value
    /// (C `dbCa.c:611-612`). Returns the completion receiver for an
    /// [`LinkPutOp::Async`] put — the caller awaits it, which is the
    /// `dbCaPutLinkCallback` route; [`LinkPutOp::Plain`] gets `None` and the
    /// caller returns immediately, which is `dbCaPutLink`.
    pub(crate) fn stage_put(
        &self,
        key: LinkKey,
        value: EpicsValue,
        op: LinkPutOp,
    ) -> Option<tokio::sync::oneshot::Receiver<Result<(), String>>> {
        let (completion, rx) = match op {
            LinkPutOp::Plain => (None, None),
            LinkPutOp::Async => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                (Some(PutCompletion(Some(tx))), Some(rx))
            }
        };
        let staged = StagedPut {
            value,
            op,
            completion,
        };
        let wake = {
            let mut inner = self.inner.lock();
            match inner.links.remove(&key) {
                None => {
                    inner.links.insert(key.clone(), LinkState::Queued(staged));
                    inner.ready.push_back(key);
                    true
                }
                Some(LinkState::Queued(prev)) => {
                    Self::supersede(&mut inner, prev);
                    // Already in the ready deque — C's `callAdd` is false
                    // here too (`dbCa.c:137`), so no second enqueue.
                    inner.links.insert(key, LinkState::Queued(staged));
                    false
                }
                Some(LinkState::InFlight) => {
                    inner.links.insert(key, LinkState::InFlightRestaged(staged));
                    false
                }
                Some(LinkState::InFlightRestaged(prev)) => {
                    Self::supersede(&mut inner, prev);
                    inner.links.insert(key, LinkState::InFlightRestaged(staged));
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
    fn supersede(inner: &mut QueueInner, prev: StagedPut) {
        inner.coalesced += 1;
        if let Some(c) = prev.completion {
            c.resolve(Err(
                "external link put superseded by a newer put on the same link".to_string(),
            ));
        }
    }

    /// Owner-only: take the next ready link, moving it `Queued` → `InFlight`.
    /// Mirrors `dbCaTask` popping `workList` and clearing `link_action`
    /// before doing the work (`dbCa.c:1180-1197`).
    fn take_ready(&self) -> Option<(LinkKey, StagedPut)> {
        let mut inner = self.inner.lock();
        loop {
            let key = inner.ready.pop_front()?;
            match inner.links.remove(&key) {
                Some(LinkState::Queued(staged)) => {
                    inner.links.insert(key.clone(), LinkState::InFlight);
                    return Some((key, staged));
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

    /// Owner-only: the write for `key` finished. `InFlight` → idle;
    /// `InFlightRestaged` → `Queued` and back in the deque, which is how a
    /// put that landed mid-flight is not lost (C clears `link_action` before
    /// the wire write for the same reason, `dbCa.c:1195`).
    fn finish(&self, key: LinkKey) {
        let (wake_work, wake_idle) = {
            let mut inner = self.inner.lock();
            inner.completed += 1;
            let wake_work = match inner.links.remove(&key) {
                Some(LinkState::InFlight) | None => false,
                Some(LinkState::InFlightRestaged(staged)) => {
                    inner.links.insert(key.clone(), LinkState::Queued(staged));
                    inner.ready.push_back(key);
                    true
                }
                // Cannot happen: only `finish` and `take_ready` move a key
                // out of `InFlight*`, and both run on the owner path.
                Some(other) => {
                    inner.links.insert(key, other);
                    false
                }
            };
            (wake_work, inner.is_idle())
        };
        if wake_work {
            self.work.notify_one();
        }
        if wake_idle {
            self.idle.notify_waiters();
        }
    }

    /// Stage the OPEN of `key` — C `dbCaAddLink`'s
    /// `addAction(pca, CA_CONNECT)` (`dbCa.c:735-800`). Idempotent and
    /// terminal: a link that has ever been staged is never staged again, so a
    /// record scanning at 10 Hz against a down remote produces one connect
    /// attempt, not ten per second. Reconnection is the lset's job, as it is
    /// libca's in C.
    ///
    /// Returns true when this call is the one that staged it.
    pub(crate) fn stage_open(&self, key: LinkKey) -> bool {
        let staged = {
            let mut inner = self.inner.lock();
            if inner.opens.contains_key(&key) {
                false
            } else {
                inner.opens.insert(key.clone(), OpenState::Queued);
                inner.ready_opens.push_back(key);
                true
            }
        };
        if staged {
            self.work.notify_one();
        }
        staged
    }

    /// Owner-only: take the next link to open, `Queued` → `InFlight`.
    fn take_ready_open(&self) -> Option<LinkKey> {
        let mut inner = self.inner.lock();
        loop {
            let key = inner.ready_opens.pop_front()?;
            match inner.opens.get_mut(&key) {
                Some(state @ OpenState::Queued) => {
                    *state = OpenState::InFlight;
                    return Some(key);
                }
                // Only `Queued` is ever in the deque.
                _ => continue,
            }
        }
    }

    /// Owner-only: the open for `key` returned. `InFlight` → `Done`.
    fn finish_open(&self, key: LinkKey) {
        let wake_idle = {
            let mut inner = self.inner.lock();
            inner.opened += 1;
            inner.opens.insert(key, OpenState::Done);
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

    /// Number of staged values a later put on the same link overwrote — C's
    /// `pca->nNoWrite` (`dbCa.c:611-612`).
    pub(crate) fn coalesced_count(&self) -> u64 {
        self.inner.lock().coalesced
    }

    /// Number of writes the owner has completed.
    pub(crate) fn completed_count(&self) -> u64 {
        self.inner.lock().completed
    }

    /// Block until the queue is idle — the `dbCaSync` barrier
    /// (`dbCa.c:1191-1194`: a `CA_SYNC` action the task answers only once it
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

    /// Start the single owner task, once. `db` is weak so the task cannot
    /// keep the database alive; it exits when the database is dropped.
    ///
    /// Reached from the staging path, which record processing enters on
    /// whatever thread processed the record, so the loop itself goes to the
    /// background executor. The network work it dispatches goes to
    /// [`Self::reactor`] instead — see that field.
    pub(crate) fn ensure_owner(self: &Arc<Self>, db: Weak<super::PvDatabaseInner>) {
        if self
            .owner_started
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let queue = Arc::downgrade(self);
        let work = self.work.clone();
        crate::runtime::task::spawn_background(owner_loop(queue, work, db));
    }
}

/// The single consumer — `dbCaTask`'s loop (`dbCa.c:1170-1333`).
///
/// Per dequeued link it resolves the lset and spawns the network write,
/// keeping at most one write in flight per link (so per-link write ordering
/// is preserved) while distinct links proceed independently. C gets that for
/// free because `ca_array_put` only queues into libca's send buffer; ours has
/// to spawn because `LinkSet::put_value` for `pva://` is a full PUT round
/// trip, and one slow target must not stall every other link's writes.
async fn owner_loop(
    queue: Weak<LinkPutQueue>,
    work: Arc<tokio::sync::Notify>,
    db: Weak<super::PvDatabaseInner>,
) {
    loop {
        // Register on the wake signal BEFORE draining, so a put staged
        // during the drain cannot be missed.
        let notified = work.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let Some(q) = queue.upgrade() else { return };
            // Opens first: a pending open is what a blocked cached read is
            // waiting on, and C services `CA_CONNECT` on the same task ahead
            // of that link's other actions (`dbCa.c:1198-1225`).
            while let Some(key) = q.take_ready_open() {
                let Some(inner) = db.upgrade() else { return };
                let lsets = resolve_lsets(&inner, &key.target);
                drop(inner);
                let q2 = q.clone();
                q.spawn_network(async move {
                    for lset in &lsets {
                        lset.connect_link(&key.name).await;
                    }
                    q2.finish_open(key);
                });
            }
            while let Some((key, staged)) = q.take_ready() {
                let Some(inner) = db.upgrade() else { return };
                let lsets = resolve_lsets(&inner, &key.target);
                drop(inner);
                let q2 = q.clone();
                q.spawn_network(async move {
                    run_put(&lsets, &key, staged).await;
                    q2.finish(key);
                });
            }
            // The strong reference must not survive the await below, or the
            // queue could never drop and its teardown would never run.
        }
        notified.await;
    }
}

/// The lsets a write (or an open) may be offered to, in order. Single owner
/// of the target → lset mapping, shared with `PvDatabase::resolve_external_pv`
/// so a link's cached read and its staged open address the same lsets.
pub(super) fn resolve_lsets(
    inner: &super::PvDatabaseInner,
    target: &LinkTarget,
) -> Vec<DynLinkSet> {
    let registry = inner.link_sets.load();
    match target {
        LinkTarget::Scheme(s) => registry.get(s).into_iter().collect(),
        LinkTarget::Any => registry
            .schemes()
            .iter()
            .filter_map(|s| registry.get(s))
            .collect(),
    }
}

/// Perform one dequeued write. The network suspension lives here — on the
/// owner's spawned task, never on a record-processing thread.
async fn run_put(lsets: &[DynLinkSet], key: &LinkKey, staged: StagedPut) {
    let StagedPut {
        value,
        op,
        completion,
    } = staged;
    let mut result = Err(format!(
        "no link set accepted the write to external link '{}'",
        key.name
    ));
    for lset in lsets {
        match lset.put_value(&key.name, value.clone(), op).await {
            Ok(()) => {
                // Production drain of any retry-queued OUT writes on this
                // lset now that a write has reached it (the channel may have
                // just reconnected). pvxs replays the queued put from record
                // processing, not test code (`pvalink_channel.cpp:220-263`).
                lset.flush_puts().await;
                result = Ok(());
                break;
            }
            Err(e) => result = Err(e),
        }
    }
    // C reports a failed wire write on the task, via `errlogPrintf`
    // (`dbCa.c:1240-1244`) — for BOTH flavours: the `status != ECA_NORMAL`
    // arm sits below the `CA_PUT` / `CA_PUT_CALLBACK` fork and covers each
    // (`dbCa.c:1226-1244`). The record has already returned; its alarm came
    // from the staging gate, exactly as `dbCaPutLink`'s `-1` does.
    if let Err(e) = &result {
        eprintln!("dbCa: external link put to '{}' failed: {e}", key.name);
    }
    if let Some(c) = completion {
        c.resolve(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    fn key(name: &str) -> LinkKey {
        LinkKey {
            target: LinkTarget::Scheme("ca".to_string()),
            name: name.to_string(),
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
    /// tests running under `--features rtems-exec-model` and adds no site to
    /// this file's RTEMS-EXEC-MODEL-ALLOW census.
    fn resolved(rx: tokio::sync::oneshot::Receiver<Result<(), String>>) -> Result<(), String> {
        let mut rx = std::pin::pin!(rx);
        match rx.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(out) => out.expect("completion resolved, not dropped"),
            Poll::Pending => panic!("completion was left unresolved"),
        }
    }

    /// Boundary: two plain puts staged on ONE link before the owner picks it
    /// up. C keeps a single `pputNative` buffer, so the second overwrites the
    /// first and only bumps `nNoWrite` (`dbCa.c:611-612`) — latest-wins, and
    /// the link is enqueued once (`callAdd = (pca->link_action == 0)`,
    /// `dbCa.c:137`). This is C's answer to "what happens when the queue is
    /// full": there is no queue to fill.
    #[test]
    fn second_put_on_same_link_coalesces_latest_wins() {
        let q = LinkPutQueue::default();
        assert!(
            q.stage_put(key("A"), EpicsValue::Long(1), LinkPutOp::Plain)
                .is_none()
        );
        assert!(
            q.stage_put(key("A"), EpicsValue::Long(2), LinkPutOp::Plain)
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

    /// Boundary: distinct links do NOT coalesce — one ready entry each, in
    /// staging order (`workList` is a FIFO: `ellAdd` / `ellGet`).
    #[test]
    fn distinct_links_keep_their_own_pending_value() {
        let q = LinkPutQueue::default();
        q.stage_put(key("A"), EpicsValue::Long(1), LinkPutOp::Plain);
        q.stage_put(key("B"), EpicsValue::Long(2), LinkPutOp::Plain);
        assert_eq!(q.coalesced_count(), 0);
        assert_eq!(q.take_ready().expect("A first").0, key("A"));
        assert_eq!(q.take_ready().expect("B second").0, key("B"));
        assert!(q.take_ready().is_none());
    }

    /// Boundary: a put staged while the link is IN FLIGHT is not lost — it
    /// becomes ready again when the in-flight write finishes, and never
    /// overtakes it. C gets this by clearing `link_action` before the wire
    /// write (`dbCa.c:1195`), so a concurrent `addAction` re-appends the link.
    #[test]
    fn put_staged_during_flight_is_requeued_on_finish() {
        let q = LinkPutQueue::default();
        q.stage_put(key("A"), EpicsValue::Long(1), LinkPutOp::Plain);
        let (k, first) = q.take_ready().expect("ready");
        assert_eq!(first.value, EpicsValue::Long(1));
        assert!(q.take_ready().is_none(), "in flight, nothing ready");

        q.stage_put(key("A"), EpicsValue::Long(9), LinkPutOp::Plain);
        assert!(
            q.take_ready().is_none(),
            "still in flight — at most one write per link keeps write order"
        );

        q.finish(k);
        let (_, second) = q.take_ready().expect("re-queued after finish");
        assert_eq!(second.value, EpicsValue::Long(9));
    }

    /// Boundary: the completion flavour is resolved — never dropped — when a
    /// newer put supersedes it. C overwrites `pca->putCallback`
    /// (`dbCa.c:614-621`) and the earlier callback never fires; we resolve it
    /// with an error instead, so no pending put loses its classification.
    #[test]
    fn superseded_completion_is_resolved_not_dropped() {
        let q = LinkPutQueue::default();
        let rx = q
            .stage_put(key("A"), EpicsValue::Long(1), LinkPutOp::Async)
            .expect("Async put yields a completion");
        q.stage_put(key("A"), EpicsValue::Long(2), LinkPutOp::Async);

        assert!(
            resolved(rx).unwrap_err().contains("superseded"),
            "a superseded completion must say so"
        );
    }

    /// Boundary: tearing the queue down resolves every pending completion.
    /// The `Drop` on `PutCompletion` is what makes this hold on *every* exit
    /// path, not just this one.
    #[test]
    fn teardown_resolves_pending_completions() {
        let q = LinkPutQueue::default();
        let rx = q
            .stage_put(key("A"), EpicsValue::Long(1), LinkPutOp::Async)
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
    /// (`dbCa.c:630`).
    #[test]
    fn plain_put_has_no_completion() {
        let q = LinkPutQueue::default();
        assert!(
            q.stage_put(key("A"), EpicsValue::Long(1), LinkPutOp::Plain)
                .is_none()
        );
    }

    /// Boundary: idle accounting. The queue is idle before anything is
    /// staged, busy while a write is in flight, idle again after `finish`.
    #[test]
    fn idle_tracks_staged_and_in_flight() {
        let q = LinkPutQueue::default();
        assert!(q.is_idle());
        q.stage_put(key("A"), EpicsValue::Long(1), LinkPutOp::Plain);
        assert!(!q.is_idle(), "staged but not run");
        let (k, _) = q.take_ready().expect("ready");
        assert!(!q.is_idle(), "in flight");
        q.finish(k);
        assert!(q.is_idle());
        assert_eq!(q.completed_count(), 1);
    }
}
