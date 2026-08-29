//! The server-wide QSRV group drain — the port of pvxs's single
//! `"qsrvGroup"` event pump (`ioc/groupsource.cpp:96`).
//!
//! # Why one drain
//!
//! Upstream starts ONE `db_start_events(.., "qsrvGroup", ..)` thread per
//! `GroupSource`; every member subscription of every group is a callback on
//! that thread, which locks the trigger set, refreshes the group's
//! `currentValue` and posts the coalesced update
//! (`groupsource.cpp:307-353`). The pre-drain Rust shape instead spawned
//! ~2 forwarder tasks per member onto the shared Medium-band callback pool;
//! a 20-member group at a 10 Hz posting rate put ~400 task wakes/second on
//! the band every other PV's monitor delivery uses, collapsing group
//! delivery to ~0.35 Hz and stalling an unrelated scalar monitor for up to
//! 16.9 s on the RTEMS target. This module is the structural fix: member
//! events stay queued in their own
//! [`EvQue`](epics_base_rs::server::event_queue::EvQue)s and ONE drain —
//! shared by every group subscription on the server — consumes them,
//! assembles the atomic snapshot and posts the group update. Tasks woken
//! per member tick: O(members) → O(1).
//!
//! # Why the drain is a dedicated thread, not a pool task
//!
//! Upstream's pump IS a thread — `db_start_events` creates it — and the
//! first landing of this module learned why the hard way. It spawned
//! [`pump_main`] with `runtime::task::spawn`, which on the exec backend is
//! a task on the Medium callback band: one cooperative worker, released
//! only when a task's poll RETURNS, and `pump_main`'s poll returns only
//! when the command channel and every member queue are simultaneously
//! empty. On the target the `.1 second` scans (EPICS 66, above cbMedium's
//! 64) refilled the member queues faster than the drain emptied them, so
//! that poll never returned: the band's only worker never ran another
//! Medium task (all per-PV monitor forwarding dead) and never slept
//! (SCHED_FIFO 64 busy on a single core — every thread below it starved,
//! protocol echo included). One group subscription wedged the whole server,
//! permanently. A dedicated thread removes the failure class instead of
//! relocating it: the drain parks on its own
//! stack, occupies no shared worker, and runs BELOW the connection threads
//! so even a saturated drain cannot preempt protocol traffic — see
//! [`QSRV_GROUP_PUMP_PRIORITY`].
//!
//! # Invariant
//!
//! * MUST: only the pump task ([`pump_main`]) consumes member subscription
//!   events and posts group monitor updates; a member event MUST NOT spawn
//!   or wake a per-member pool task.
//! * MUST: a registration leaves the live set only through
//!   [`pump_main`]'s single removal path — a [`PumpCmd::Deregister`] from
//!   the [`RegistrationHandle`] finalizer, or the pump observing the
//!   consumer side closed on push. Nothing else may drop a registration's
//!   member subscriptions or its update queue producer.
//! * MUST: the drain task exists exactly while `PumpShared::cmd_tx` is
//!   `Some` — set only by [`GroupPump::register`] (spawning it), cleared
//!   only by the pump's own exit finalizer, both under the `PumpShared`
//!   lock, with the command queue drained under that same lock before the
//!   clear. A `Register` can therefore never land in a channel no live
//!   pump will read.
//! * MUST NOT: the drain run while no group subscription exists — C parity
//!   (forwarding work only under subscription); on last-deregistration the
//!   thread terminates through the finalizer above.
//! * MUST: the drain run on its own dedicated OS thread ([`pump_main`]
//!   under `block_on_sync`), never on a shared callback-band worker — and
//!   every one of its awaits stay runtime-agnostic (waker-based, no
//!   reactor, no internal thread-blocking bridge), so the only thread its
//!   park points ever park is its own.
//!
//! # Drain liveness (who wakes the parked drain thread)
//!
//! The drain thread parks between polls (`park_on`'s `ThreadWaker`);
//! every park point of [`pump_main`] keeps its waker in a structure owned
//! by the sender side, so the unpark cannot be lost:
//!
//! 1. the command channel — waker held by the `UnboundedReceiver`'s shared
//!    channel state, whose `UnboundedSender` lives in `PumpShared` for the
//!    pump's whole life (cleared only by the exit finalizer, after which
//!    the thread returns without awaiting again);
//! 2. member queues — waker held in each polled `EvQue::poll_wakers`,
//!    inside the `Arc<EvQue>` the record's subscriber slot (the
//!    `EventSink`) keeps alive; a record post flushes it via
//!    `wake_readers` — the same contract the CA blocking driver's
//!    per-connection drain loops already park on;
//! 3. `read_group().await` — the same DB await surface the per-monitor
//!    poll path already used, unchanged; its awaits are `tokio::sync` /
//!    `poll_fn` futures (no reactor, no timers), which is what lets the
//!    same body run under a hosted runtime and under `park_on`;
//! 4. the update queue never parks the pump: [`UpdatePusher::push`] is
//!    synchronous and coalesces instead of blocking (below).
//!
//! # Update queue (per subscription, C monitor semantics)
//!
//! The pump posts assembled updates into a bounded per-registration
//! [`update_queue`] whose overflow rule is C `db_queue_event_log`'s: while
//! room remains, distinct updates append; once full, the newest update
//! REPLACES the queue's tail in place, value newest-wins and marked leaf
//! sets unioned (pvxs's monitor FIFO squash). Push therefore never blocks,
//! so one slow subscriber cannot stall the drain for other groups.

// RTEMS-EXEC-MODEL-ALLOW(7): checked - these run and pass in the exec-backend
// suite.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::task::{Poll, Waker};

use epics_base_rs::runtime::task::{
    StackSizeClass, ThreadPriority, block_on_sync, spawn_dedicated_thread,
};
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::pv::MonitorEvent;
use epics_base_rs::server::snapshot::PropertySupport;

use super::group::{EventMark, GroupChannel, GroupMonitor, MemberEventKind};
use super::group_config::GroupPvDef;
use super::provider::MonitorPoll;

/// The EPICS priority of the `qsrvGroup` drain thread — deliberately ONE
/// BELOW the PVA connection threads, where pvxs puts its pump one ABOVE.
///
/// pvxs starts the pump with `db_start_events(.., "qsrvGroup", ..,
/// epicsThreadPriorityCAServerLow - 1)` (`ioc/groupsource.cpp:96`) = 19 on
/// the ladder this workspace already copied its PVA numbers from: the
/// blocking driver's connection threads sit at `CAServerLow - 2` = 18
/// (`PVA_SERVER_PRIORITY`, `server_native/blocking.rs`) and its UDP search
/// responder at `CAServerLow - 4` = 16. Taking upstream's 19 literally
/// would put the drain ABOVE the connection threads, and the §9.15
/// regression measured exactly what a drain above the protocol threads
/// does to a single-core SCHED_FIFO target when it saturates: echo
/// unanswered, fresh clients timing out, delivery dead server-wide. Per
/// event this drain also does strictly more work than upstream's pump
/// callback budgeted for that slot (a full atomic `read_group` of every
/// member). So the deviation: 17 keeps the pump above the SEARCH responder
/// (preserving pvxs's drain > search ordering — a discovery storm must not
/// starve group delivery) but below established connections, so a
/// saturated drain degrades group update latency (member queues coalesce,
/// newest-wins) instead of protocol liveness.
const QSRV_GROUP_PUMP_PRIORITY: ThreadPriority = ThreadPriority::Custom(17);

// ---------------------------------------------------------------------------
// Update queue — pump → monitor, bounded, replace-in-place on overflow
// ---------------------------------------------------------------------------

struct UpdateState {
    queue: VecDeque<MonitorPoll>,
    cap: usize,
    /// Waker of the consumer parked in [`UpdatePoller::recv`]. Held here —
    /// inside state the PRODUCER (the pump's registration) keeps alive — so
    /// the monitor's forward task stays wake-able on the exec backend.
    consumer_waker: Option<Waker>,
    consumer_gone: bool,
    producer_gone: bool,
}

/// Producer half, owned by the pump's registration. Dropping it (the
/// registration leaving the live set) ends the consumer's stream.
pub(crate) struct UpdatePusher {
    state: Arc<parking_lot::Mutex<UpdateState>>,
}

/// Consumer half, owned by the `GroupMonitor`; `recv().await` is what
/// `GroupMonitor::poll` parks on.
pub(crate) struct UpdatePoller {
    state: Arc<parking_lot::Mutex<UpdateState>>,
}

/// The consumer is gone — the monitor is tearing down. The pump treats this
/// as the teardown observation and removes the registration through its
/// single removal path.
pub(crate) struct UpdateClosed;

/// A bounded pump→monitor update queue with C monitor overflow semantics
/// (append while room, replace the tail in place once full). `cap` is the
/// operation's negotiated `queueSize`, clamped ≥ 1.
pub(crate) fn update_queue(cap: usize) -> (UpdatePusher, UpdatePoller) {
    let state = Arc::new(parking_lot::Mutex::new(UpdateState {
        queue: VecDeque::new(),
        cap: cap.max(1),
        consumer_waker: None,
        consumer_gone: false,
        producer_gone: false,
    }));
    (
        UpdatePusher {
            state: state.clone(),
        },
        UpdatePoller { state },
    )
}

/// Union of two marked-leaf sets. `None` means "derive / everything
/// changed" and absorbs any explicit set.
fn merge_marks(a: Option<Vec<String>>, b: Option<Vec<String>>) -> Option<Vec<String>> {
    match (a, b) {
        (Some(mut a), Some(b)) => {
            for path in b {
                if !a.contains(&path) {
                    a.push(path);
                }
            }
            Some(a)
        }
        _ => None,
    }
}

impl UpdatePusher {
    /// Post one assembled group update. Never blocks: while the queue has
    /// room the update appends; once full it replaces the tail in place —
    /// value newest-wins, marks unioned (C `db_queue_event_log`'s replace
    /// branch, pvxs's monitor FIFO squash). `Err(UpdateClosed)` when the
    /// consumer dropped — nothing is queued, and the caller must route the
    /// registration through the pump's removal path.
    fn push(&self, update: MonitorPoll) -> Result<(), UpdateClosed> {
        let waker = {
            let mut st = self.state.lock();
            if st.consumer_gone {
                return Err(UpdateClosed);
            }
            if st.queue.len() < st.cap {
                st.queue.push_back(update);
            } else {
                let tail = st
                    .queue
                    .back_mut()
                    .expect("cap >= 1 means a full queue has a tail");
                tail.marked = merge_marks(tail.marked.take(), update.marked);
                tail.value = update.value;
            }
            st.consumer_waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
        Ok(())
    }
}

impl Drop for UpdatePusher {
    fn drop(&mut self) {
        let waker = {
            let mut st = self.state.lock();
            st.producer_gone = true;
            st.consumer_waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }
}

impl UpdatePoller {
    /// Await the next assembled group update. `None` ⟺ the producer is
    /// gone AND the backlog is drained — i.e. this subscription left the
    /// pump — which `GroupMonitor::poll` surfaces as teardown.
    pub(crate) async fn recv(&mut self) -> Option<MonitorPoll> {
        std::future::poll_fn(|cx| {
            let mut st = self.state.lock();
            if let Some(update) = st.queue.pop_front() {
                return Poll::Ready(Some(update));
            }
            if st.producer_gone {
                return Poll::Ready(None);
            }
            st.consumer_waker = Some(cx.waker().clone());
            Poll::Pending
        })
        .await
    }
}

impl Drop for UpdatePoller {
    fn drop(&mut self) {
        self.state.lock().consumer_gone = true;
        // No producer waker to flush: the pump never parks on this queue
        // (push is synchronous); it observes `consumer_gone` on its next
        // push, and the RegistrationHandle finalizer covers the quiet case.
    }
}

// ---------------------------------------------------------------------------
// GroupPump — registry + lifecycle arbiter
// ---------------------------------------------------------------------------

/// One member subscription inside a registration: which group member index
/// it belongs to and whether it is the value or the property channel.
pub(crate) struct MemberSub {
    pub(crate) member_index: usize,
    pub(crate) kind: MemberEventKind,
    pub(crate) sub: DbSubscription,
}

/// Everything the pump needs to drain one group subscription.
pub(crate) struct RegistrationSpec {
    pub(crate) def: GroupPvDef,
    pub(crate) member_props: Vec<PropertySupport>,
    pub(crate) group_channel: GroupChannel,
    pub(crate) subs: Vec<MemberSub>,
    pub(crate) update_tx: UpdatePusher,
}

struct LiveReg {
    spec: RegistrationSpec,
}

enum PumpCmd {
    Register(u64, Box<RegistrationSpec>),
    Deregister(u64),
}

struct PumpShared {
    /// `Some` ⟺ a pump task is alive and will process every command
    /// already sent (the lifecycle invariant — see the module docs). Set
    /// only by [`GroupPump::register`], cleared only by the pump task's
    /// exit finalizer, both under this lock.
    cmd_tx: Option<tokio::sync::mpsc::UnboundedSender<PumpCmd>>,
    next_id: u64,
}

/// The server-wide group drain registry. One per `BridgeProvider` (== one
/// per served IOC database — C's one `GroupSource` per server); every group
/// subscription registers here and the single drain task serves them all.
pub(crate) struct GroupPump {
    shared: parking_lot::Mutex<PumpShared>,
}

/// Detached finalizer for one registration. Dropping it is THE teardown
/// path: it queues [`PumpCmd::Deregister`] to the live pump (a no-op id if
/// the pump already removed the registration on consumer-close).
pub(crate) struct RegistrationHandle {
    pump: Arc<GroupPump>,
    id: u64,
}

impl Drop for RegistrationHandle {
    fn drop(&mut self) {
        let shared = self.pump.shared.lock();
        if let Some(tx) = &shared.cmd_tx {
            // Send failure means the pump task died abnormally; the
            // registration's resources died with it, so there is nothing
            // left to finalize.
            let _ = tx.send(PumpCmd::Deregister(self.id));
        }
        // cmd_tx == None ⟹ no live pump ⟹ the live set is empty ⟹ this
        // registration was already removed (consumer-close observation).
    }
}

impl GroupPump {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            shared: parking_lot::Mutex::new(PumpShared {
                cmd_tx: None,
                next_id: 0,
            }),
        })
    }

    /// True while the drain task is alive. Test probe for the lifecycle
    /// invariant (drain ⟺ ≥ 1 registration, modulo the window in which the
    /// pump is processing its final deregistration).
    #[cfg(test)]
    pub(crate) fn has_live_drain(&self) -> bool {
        self.shared.lock().cmd_tx.is_some()
    }

    /// Register one group subscription with the drain, spawning the drain
    /// task if none is alive. Returns the registration's finalizer handle.
    pub(crate) fn register(self: &Arc<Self>, spec: RegistrationSpec) -> RegistrationHandle {
        let mut shared = self.shared.lock();
        let id = shared.next_id;
        shared.next_id += 1;
        let mut cmd = PumpCmd::Register(id, Box::new(spec));
        if let Some(tx) = &shared.cmd_tx {
            match tx.send(cmd) {
                Ok(()) => {
                    return RegistrationHandle {
                        pump: self.clone(),
                        id,
                    };
                }
                // The pump task died abnormally (its receiver is gone
                // without the finalizer having cleared cmd_tx). Fall
                // through and spawn a fresh drain.
                Err(tokio::sync::mpsc::error::SendError(c)) => cmd = c,
            }
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(cmd).expect("fresh channel with a live receiver");
        shared.cmd_tx = Some(tx);
        let pump = self.clone();
        // A dedicated thread, NEVER `runtime::task::spawn`: on the exec
        // backend that is a Medium-band pool task, and a drain whose poll
        // returns only when every member queue is dry held the band's one
        // worker forever on the target — the §9.15 server-wide wedge (see
        // the module docs). Upstream's pump is a thread for the same
        // reason (`db_start_events`, `groupsource.cpp:96`); its name is
        // upstream's, and fits RTEMS's 16-byte thread-name cap.
        //
        // `Big` stack, not upstream's event-task `Medium` (`dbEvent.c:1117`):
        // `block_on_sync` pins `pump_main` — including each `read_group` +
        // NT-assembly state machine — on this stack, the same audited
        // future the `Big`-stacked connection threads run for a group GET;
        // C's `Medium` sizes a shallower C callback frame.
        let spawned = spawn_dedicated_thread(
            "qsrvGroup".into(),
            QSRV_GROUP_PUMP_PRIORITY,
            StackSizeClass::Big,
            move || {
                if let Err(e) = block_on_sync(pump_main(pump, rx)) {
                    // Unreachable by construction: a fresh dedicated thread
                    // is neither a facility worker nor a current-thread
                    // runtime. If it ever fires, the exit finalizer never
                    // ran — the next `register` recovers through its
                    // send-failure arm.
                    tracing::error!(error = ?e, "qsrv group drain thread could not block on its pump");
                }
            },
        );
        if let Err(e) = spawned {
            // pvxs treats a pump that cannot start as fatal to source setup
            // (`groupsource.cpp:97` throws). Restore the invariant
            // (`cmd_tx` `Some` ⟺ live pump) before propagating, so the
            // failed spawn cannot masquerade as a live drain.
            shared.cmd_tx = None;
            panic!("qsrv group drain thread could not be created: {e}");
        }
        RegistrationHandle {
            pump: self.clone(),
            id,
        }
    }
}

// ---------------------------------------------------------------------------
// The drain task
// ---------------------------------------------------------------------------

enum Wake {
    Cmd(PumpCmd),
    /// The command channel returned `None`. Unreachable while this task is
    /// alive (the `PumpShared` sender lives until this task's own
    /// finalizer), kept as a defensive exit so a bug cannot busy-spin.
    CmdClosed,
    Event {
        reg_id: u64,
        sub_pos: usize,
        /// Boxed so the `Wake` enum stays small (a `MonitorEvent` carries a
        /// full field snapshot); one box per drained event is noise next to
        /// the group read it triggers.
        event: Box<MonitorEvent>,
    },
    /// A member subscription's stream ended (its record went away). The
    /// registration stays — the group keeps serving its other members,
    /// like the old per-member forwarder simply exiting.
    SubGone {
        reg_id: u64,
        sub_pos: usize,
    },
}

/// The single drain. Owns every live registration; the ONLY consumer of
/// member subscription events and the ONLY poster of group monitor updates.
async fn pump_main(
    pump: Arc<GroupPump>,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<PumpCmd>,
) {
    let mut regs: BTreeMap<u64, LiveReg> = BTreeMap::new();
    // Round-robin cursor over the flattened (registration, member-sub) scan
    // order so a hot member cannot starve other members or other groups: the
    // scan after serving position k starts at k+1. C's pump reads its ring
    // in strict arrival order; per-subscription FIFOs plus rotation give the
    // same no-starvation property.
    let mut cursor: usize = 0;
    loop {
        let wake = next_wake(&mut cmd_rx, &mut regs, &mut cursor).await;
        match wake {
            Wake::Cmd(PumpCmd::Register(id, spec)) => {
                regs.insert(id, LiveReg { spec: *spec });
            }
            Wake::Cmd(PumpCmd::Deregister(id)) => {
                // Removing the registration drops its DbSubscriptions
                // (detaching the member queues — any event still queued
                // there vanishes with them) and its UpdatePusher (ending
                // the monitor's update stream).
                regs.remove(&id);
                if regs.is_empty() && finalize_if_idle(&pump, &mut cmd_rx, &mut regs) {
                    return;
                }
            }
            Wake::CmdClosed => {
                debug_assert!(false, "cmd sender outlives the pump by invariant");
                return;
            }
            Wake::SubGone { reg_id, sub_pos } => {
                if let Some(reg) = regs.get_mut(&reg_id) {
                    reg.spec.subs.remove(sub_pos);
                }
            }
            Wake::Event {
                reg_id,
                sub_pos,
                event,
            } => {
                process_event(&mut regs, reg_id, sub_pos, *event).await;
                if regs.is_empty() && finalize_if_idle(&pump, &mut cmd_rx, &mut regs) {
                    return;
                }
            }
        }
    }
}

/// Await the next command or member event. Commands are polled FIRST so a
/// queued `Deregister` wins over further member events of the same wake —
/// the event-arriving-during-teardown boundary resolves to teardown.
///
/// When this returns `Pending` it has polled EVERY member reader, so the
/// drain's waker is registered with the command channel and with every
/// member queue — any of them becoming ready re-wakes (unparks) the drain
/// thread.
fn next_wake<'a>(
    cmd_rx: &'a mut tokio::sync::mpsc::UnboundedReceiver<PumpCmd>,
    regs: &'a mut BTreeMap<u64, LiveReg>,
    cursor: &'a mut usize,
) -> impl std::future::Future<Output = Wake> + 'a {
    std::future::poll_fn(move |cx| {
        if let Poll::Ready(cmd) = cmd_rx.poll_recv(cx) {
            return match cmd {
                Some(cmd) => Poll::Ready(Wake::Cmd(cmd)),
                None => Poll::Ready(Wake::CmdClosed),
            };
        }
        let order: Vec<(u64, usize)> = regs
            .iter()
            .flat_map(|(id, reg)| (0..reg.spec.subs.len()).map(move |pos| (*id, pos)))
            .collect();
        let n = order.len();
        for k in 0..n {
            let (reg_id, sub_pos) = order[(*cursor + k) % n];
            let reg = regs.get_mut(&reg_id).expect("id taken from live map");
            match reg.spec.subs[sub_pos].sub.poll_recv_event(cx) {
                Poll::Ready(Some(event)) => {
                    *cursor = (*cursor + k + 1) % n;
                    return Poll::Ready(Wake::Event {
                        reg_id,
                        sub_pos,
                        event: Box::new(event),
                    });
                }
                Poll::Ready(None) => {
                    *cursor = (*cursor + k + 1) % n;
                    return Poll::Ready(Wake::SubGone { reg_id, sub_pos });
                }
                Poll::Pending => {}
            }
        }
        Poll::Pending
    })
}

/// One member event, end to end: resolve the marked leaves, assemble the
/// atomic group snapshot, post the update. The body pvxs runs on its
/// `qsrvGroup` thread per callback (`groupsource.cpp:307-353`).
async fn process_event(
    regs: &mut BTreeMap<u64, LiveReg>,
    reg_id: u64,
    sub_pos: usize,
    event: MonitorEvent,
) {
    let Some(reg) = regs.get(&reg_id) else {
        return;
    };
    let spec = &reg.spec;
    let (member_index, kind) = {
        let ms = &spec.subs[sub_pos];
        (ms.member_index, ms.kind)
    };
    let mark = match kind {
        MemberEventKind::Value => {
            GroupMonitor::value_event_mark(&spec.def, &spec.member_props, member_index, event.mask)
        }
        MemberEventKind::Property => {
            GroupMonitor::property_event_mark(&spec.def, &spec.member_props, member_index)
        }
    };
    let marked = match mark {
        EventMark::Skip => return,
        EventMark::Marked(paths) => paths,
    };
    let value = match spec.group_channel.read_group().await {
        Ok(v) => v,
        Err(e) => {
            // pvxs wraps each group refresh in a try/catch that logs and
            // returns from the callback WITHOUT posting, leaving the
            // subscription alive (`groupsource.cpp:350-352`): a per-event
            // read failure drops a single update, never the subscription.
            tracing::warn!(
                group = %spec.def.name,
                error = %e,
                "qsrv group drain: member read failed; skipping event, subscription kept open"
            );
            return;
        }
    };
    let marked = super::pvif::narrow_enum_value_leaves(marked, &value);
    if spec
        .update_tx
        .push(MonitorPoll {
            value,
            marked: Some(marked),
        })
        .is_err()
    {
        // Consumer gone — the monitor is tearing down. Remove through the
        // same path a Deregister takes; the handle's later Deregister for
        // this id is a no-op.
        regs.remove(&reg_id);
    }
}

/// The exit finalizer — the ONLY place `cmd_tx` is cleared. Runs when the
/// live set just became empty: under the `PumpShared` lock it drains any
/// raced command; a raced `Register` keeps the pump alive (returns
/// `false`), otherwise the pump is marked dead and the task must return.
/// Because `register` sends under the same lock, no `Register` can land
/// between this drain and the clear.
fn finalize_if_idle(
    pump: &GroupPump,
    cmd_rx: &mut tokio::sync::mpsc::UnboundedReceiver<PumpCmd>,
    regs: &mut BTreeMap<u64, LiveReg>,
) -> bool {
    debug_assert!(regs.is_empty());
    let mut shared = pump.shared.lock();
    loop {
        match cmd_rx.try_recv() {
            Ok(PumpCmd::Register(id, spec)) => {
                regs.insert(id, LiveReg { spec: *spec });
                return false;
            }
            Ok(PumpCmd::Deregister(_)) => continue,
            Err(_) => break,
        }
    }
    shared.cmd_tx = None;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::records::ai::AiRecord;
    use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};

    use crate::qsrv::group_config::parse_group_config;
    use crate::qsrv::provider::PvaMonitor;

    fn upd(tag: i32, mark: &str) -> MonitorPoll {
        let mut value = PvStructure::new("");
        value
            .fields
            .push(("tag".into(), PvField::Scalar(ScalarValue::Int(tag))));
        MonitorPoll {
            value,
            marked: Some(vec![mark.to_string()]),
        }
    }

    fn tag_of(update: &MonitorPoll) -> i32 {
        match update.value.fields.first() {
            Some((_, PvField::Scalar(ScalarValue::Int(tag)))) => *tag,
            other => panic!("expected the tag field, got {other:?}"),
        }
    }

    async fn wait_drain_stopped(pump: &Arc<GroupPump>) {
        for _ in 0..500 {
            if !pump.has_live_drain() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the drain did not terminate after the last deregistration");
    }

    async fn one_member_group(name: &str, rec: &str) -> (Arc<PvDatabase>, GroupPvDef) {
        let db = Arc::new(PvDatabase::new());
        db.add_record(rec, Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        let cfg =
            format!(r#"{{ "{name}": {{ "v": {{"+type": "plain", "+channel": "{rec}.VAL"}} }} }}"#);
        let mut defs = parse_group_config(&cfg).unwrap();
        (db, defs.pop().unwrap())
    }

    fn post_value(db: &PvDatabase, rec: &str) {
        let rec = db.get_record(rec).expect("record exists");
        rec.write()
            .notify_field("VAL", EventMask::VALUE | EventMask::ALARM);
    }

    /// Boundary queueSize overflow: distinct updates append up to `cap`;
    /// past it the newest REPLACES the tail in place — value newest-wins,
    /// marked sets unioned (C `db_queue_event_log`'s replace branch). Then
    /// producer-drop ends the stream only AFTER the backlog drains.
    #[tokio::test]
    async fn update_queue_overflow_replaces_tail_and_merges_marks() {
        let (tx, mut rx) = update_queue(2);
        assert!(tx.push(upd(1, "a")).is_ok());
        assert!(tx.push(upd(2, "b")).is_ok());
        // Full: 3 and 4 coalesce into the tail.
        assert!(tx.push(upd(3, "c")).is_ok());
        assert!(tx.push(upd(4, "d")).is_ok());

        let first = rx.recv().await.expect("first distinct update");
        assert_eq!(tag_of(&first), 1);
        assert_eq!(first.marked, Some(vec!["a".to_string()]));

        let coalesced = rx.recv().await.expect("coalesced tail");
        assert_eq!(tag_of(&coalesced), 4, "latest value wins");
        assert_eq!(
            coalesced.marked,
            Some(vec!["b".to_string(), "c".to_string(), "d".to_string()]),
            "marks union across every squashed update"
        );

        drop(tx);
        assert!(
            rx.recv().await.is_none(),
            "producer gone + backlog drained = end of stream"
        );
    }

    /// Boundary zero subscribers: no registration ever happens, so no drain
    /// task exists — before, during and after member records post. Starting
    /// one subscription spawns the drain; stopping it terminates the drain
    /// through the single finalizer.
    #[tokio::test]
    async fn zero_group_subscriptions_run_no_drain() {
        let (db, def) = one_member_group("T0:GRP", "T0:a").await;
        let pump = GroupPump::new();
        assert!(!pump.has_live_drain(), "no drain before any subscription");

        post_value(&db, "T0:a");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !pump.has_live_drain(),
            "member posts without a subscription must start no drain work"
        );

        let mut mon = GroupMonitor::new(db.clone(), def).with_pump(pump.clone());
        mon.start().await.expect("start");
        assert!(pump.has_live_drain(), "first registration spawns the drain");

        mon.stop().await;
        wait_drain_stopped(&pump).await;
    }

    /// Boundary cross-group fairness: two groups share the one drain; group
    /// A's subscriber never polls while A floods, and group B's delivery
    /// must still proceed — the update queue coalesces instead of blocking,
    /// so a slow subscriber cannot stall the drain for a sibling group.
    #[tokio::test]
    async fn two_groups_share_one_drain_slow_consumer_does_not_block_the_other() {
        let db = Arc::new(PvDatabase::new());
        db.add_record("TF:a", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        db.add_record("TF:b", Box::new(AiRecord::new(2.0)))
            .await
            .unwrap();
        let cfg = r#"{
            "TF:GA": { "v": {"+type": "plain", "+channel": "TF:a.VAL"} },
            "TF:GB": { "v": {"+type": "plain", "+channel": "TF:b.VAL"} }
        }"#;
        let defs = parse_group_config(cfg).unwrap();
        let def_a = defs.iter().find(|d| d.name == "TF:GA").unwrap().clone();
        let def_b = defs.iter().find(|d| d.name == "TF:GB").unwrap().clone();

        let pump = GroupPump::new();
        let mut mon_a = GroupMonitor::new(db.clone(), def_a).with_pump(pump.clone());
        let mut mon_b = GroupMonitor::new(db.clone(), def_b).with_pump(pump.clone());
        mon_a.start().await.expect("start A");
        mon_b.start().await.expect("start B");

        // Flood A without ever polling its monitor. The short sleeps let
        // the drain assemble per event rather than EvQue-coalescing the
        // whole burst, so A's update queue genuinely overflows into
        // replace-in-place while its consumer stays stalled.
        for _ in 0..12 {
            post_value(&db, "TF:a");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // B must deliver regardless of A's backlog.
        post_value(&db, "TF:b");
        let polled = tokio::time::timeout(Duration::from_secs(2), mon_b.poll())
            .await
            .expect("B's delivery must not be blocked by A's stalled subscriber")
            .expect("B receives an assembled update");
        assert!(!polled.value.fields.is_empty());

        // A's backlog is intact (coalesced, not lost or torn down).
        let polled_a = tokio::time::timeout(Duration::from_secs(2), mon_a.poll())
            .await
            .expect("A still delivers after the flood")
            .expect("A's update stream is alive");
        assert!(!polled_a.value.fields.is_empty());

        mon_a.stop().await;
        assert!(
            pump.has_live_drain(),
            "the drain survives while B is still subscribed"
        );
        mon_b.stop().await;
        wait_drain_stopped(&pump).await;
    }

    /// Boundary last-unsubscribe-while-drain-mid-cycle: tear the only
    /// subscription down while a posted burst is still draining. The exit
    /// routes through the pump's single finalizer — the drain terminates —
    /// and a fresh subscription afterwards restarts it cleanly
    /// (resubscribe-after-teardown boundary).
    #[tokio::test]
    async fn last_unsubscribe_mid_burst_terminates_drain_through_finalizer() {
        let (db, def) = one_member_group("TM:GRP", "TM:a").await;
        let pump = GroupPump::new();
        let mut mon = GroupMonitor::new(db.clone(), def.clone()).with_pump(pump.clone());
        mon.start().await.expect("start");

        // Burst, then stop immediately — the drain is mid-cycle on the
        // backlog (or about to be) when the Deregister lands.
        for _ in 0..8 {
            post_value(&db, "TM:a");
        }
        mon.stop().await;
        wait_drain_stopped(&pump).await;

        // Resubscribe-after-teardown restarts cleanly on the same pump.
        let mut mon2 = GroupMonitor::new(db.clone(), def).with_pump(pump.clone());
        mon2.start().await.expect("restart");
        assert!(pump.has_live_drain(), "re-registration spawns a new drain");
        post_value(&db, "TM:a");
        let polled = tokio::time::timeout(Duration::from_secs(2), mon2.poll())
            .await
            .expect("the restarted drain delivers")
            .expect("update after resubscribe");
        assert!(!polled.value.fields.is_empty());
        mon2.stop().await;
        wait_drain_stopped(&pump).await;
    }

    /// Boundary event-arriving-during-teardown, consumer side first: the
    /// monitor's update-queue consumer drops BEFORE any deregistration
    /// command, and a member event lands in that window. The pump's push
    /// fails visibly (`UpdateClosed`), routes the registration out through
    /// the same removal path a Deregister takes, and — as the last one out
    /// — the drain terminates; the handle's late Deregister is a no-op.
    #[tokio::test]
    async fn event_during_teardown_routes_through_the_finalizer() {
        let (db, def) = one_member_group("TT:GRP", "TT:a").await;
        let pump = GroupPump::new();

        let sub = DbSubscription::subscribe_with_mask(
            &db,
            "TT:a.VAL",
            0,
            (EventMask::VALUE | EventMask::ALARM).bits(),
        )
        .await
        .expect("member subscription");
        let (update_tx, update_rx) = update_queue(4);
        let handle = pump.register(RegistrationSpec {
            def: def.clone(),
            member_props: vec![PropertySupport::NONE],
            group_channel: GroupChannel::new(db.clone(), def),
            subs: vec![MemberSub {
                member_index: 0,
                kind: MemberEventKind::Value,
                sub,
            }],
            update_tx,
        });
        assert!(pump.has_live_drain());

        // Consumer gone first, then an event arrives while the
        // registration is still live in the pump.
        drop(update_rx);
        post_value(&db, "TT:a");

        // The pump observes Closed on push and removes the registration —
        // last one out, so the drain terminates without any Deregister.
        wait_drain_stopped(&pump).await;

        // The handle's late Deregister must be a harmless no-op.
        drop(handle);
        assert!(!pump.has_live_drain());
    }

    /// The §9.15 fatal-regression test — group delivery MUST NOT depend on
    /// the shared callback pool's Medium band.
    ///
    /// The first pump landing spawned [`pump_main`] with
    /// `runtime::task::spawn`, which on the exec backend is a task on the
    /// Medium callback band: ONE cooperative worker, released only when a
    /// task's poll RETURNS — and `pump_main`'s poll returns only when the
    /// command channel and every member queue are simultaneously empty. On
    /// the target the `.1 second` scans (EPICS 66, above cbMedium's 64)
    /// refilled the member queues faster than the drain emptied them, so the
    /// poll never returned, the band's only worker never ran another Medium
    /// task or slept again, and every thread below 64 starved: one group
    /// subscription wedged ALL monitor delivery server-wide, killed protocol
    /// echo and timed out fresh clients, permanently (measured, §9.15). The
    /// same class was caught independently on the host: a name-server dial
    /// occupying cbMedium for its 40 s connect timeout froze delivery
    /// server-wide.
    ///
    /// So: occupy the Medium band's only worker for the WHOLE test, and
    /// require the drain to deliver anyway. On the band-task shape this
    /// fails deterministically (the pump task is queued behind the pin and
    /// can never run); on the dedicated-thread shape (pvxs's `qsrvGroup`
    /// thread, `db_start_events` at `groupsource.cpp:96`) it passes without
    /// the band ever becoming free. Exec backend only: on the tokio backend
    /// `spawn_blocking` goes to a large pool and pins nothing.
    #[cfg(exec_backend)]
    #[tokio::test]
    async fn drain_delivers_while_the_medium_band_worker_is_occupied() {
        // Pin the Medium band's ONLY worker (DEFAULT_THREADS_PER_PRIORITY
        // = 1): the closure holds it until released, so nothing spawned onto
        // the band can run for the duration of the test.
        let (pinned_tx, pinned_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let pin = epics_base_rs::runtime::task::spawn_blocking(move || {
            let _ = pinned_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(60));
        });
        pinned_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the pin closure reached the band worker");

        let (db, def) = one_member_group("TP:GRP", "TP:a").await;
        let pump = GroupPump::new();
        let mut mon = GroupMonitor::new(db.clone(), def).with_pump(pump.clone());
        mon.start().await.expect("start");
        post_value(&db, "TP:a");

        let polled = tokio::time::timeout(Duration::from_secs(3), mon.poll()).await;
        // Release the band before asserting, so a failure does not leave the
        // worker wedged for the rest of the process.
        let _ = release_tx.send(());
        let polled = polled
            .expect(
                "group delivery must not depend on the shared Medium callback band \
                 (§9.15: the band-task pump wedged the whole target)",
            )
            .expect("assembled update");
        assert!(!polled.value.fields.is_empty());

        mon.stop().await;
        wait_drain_stopped(&pump).await;
        drop(pin);
    }

    /// Delivery-keeps-up regression (host-scale stand-in for the on-target
    /// §9.14 rerun): a 20-member group posting fast, with a live group
    /// subscriber, must keep delivering assembled updates promptly — every
    /// member tick is O(1) woken tasks through the shared drain, and the
    /// update stream never wedges under the flood.
    #[tokio::test]
    async fn twenty_member_flood_keeps_delivering() {
        let db = Arc::new(PvDatabase::new());
        let mut members = String::new();
        for i in 0..20 {
            let rec = format!("TB:m{i:02}");
            db.add_record(&rec, Box::new(AiRecord::new(f64::from(i))))
                .await
                .unwrap();
            if i > 0 {
                members.push(',');
            }
            members.push_str(&format!(
                r#""f{i:02}": {{"+type": "plain", "+channel": "{rec}.VAL"}}"#
            ));
        }
        let cfg = format!(r#"{{ "TB:BIG": {{ {members} }} }}"#);
        let mut defs = parse_group_config(&cfg).unwrap();
        let def = defs.pop().unwrap();

        let pump = GroupPump::new();
        let mut mon = GroupMonitor::new(db.clone(), def).with_pump(pump.clone());
        mon.start().await.expect("start");

        // Flood: 5 rounds over all 20 members, polling in between.
        for _ in 0..5 {
            for i in 0..20 {
                post_value(&db, &format!("TB:m{i:02}"));
            }
            let polled = tokio::time::timeout(Duration::from_secs(2), mon.poll())
                .await
                .expect("delivery keeps up under a 20-member flood")
                .expect("assembled update");
            assert!(
                polled.value.get_field("f00").is_some() && polled.value.get_field("f19").is_some(),
                "the drained update is the complete assembled group"
            );
        }

        mon.stop().await;
        wait_drain_stopped(&pump).await;
    }
}
