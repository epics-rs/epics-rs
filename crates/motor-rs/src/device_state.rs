use asyn_rs::interfaces::motor::MotorStatus;
use asyn_rs::trace::TraceMask;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::trace::MotorTrace;

/// Stamped motor status with sequence number for change detection.
#[derive(Debug, Clone)]
pub struct StampedStatus {
    pub seq: u64,
    pub status: MotorStatus,
}

/// Delay request with unique ID for stale timer prevention.
#[derive(Debug, Clone)]
pub struct DelayRequest {
    pub id: u64,
    pub duration: Duration,
}

/// Atomic bundle of actions from Record → DeviceSupport.
#[derive(Debug, Default)]
pub struct DeviceActions {
    pub commands: Vec<crate::flags::MotorCommand>,
    /// Poll now and post even an unchanged status: C `motorUpdateStatus_`
    /// forcing `statusChanged_ = 1` (asynMotorController.cpp:217-222), for
    /// `request_poll` / `status_refresh`, so STUP=BUSY clears on a
    /// stationary axis.
    pub force_poll: bool,
    pub schedule_delay: Option<DelayRequest>,
    pub status_refresh: bool,
}

impl DeviceActions {
    /// Fold a newer batch into `self` when a previous batch has not yet been
    /// consumed by DeviceSupport.write(). Commands are appended (FIFO order
    /// preserved); a forced poll from either survives; schedule_delay and
    /// status_refresh take the newer value if present. Prevents a dropped move command when two process()
    /// cycles run without an intervening write().
    pub fn merge_newer(&mut self, newer: DeviceActions) {
        self.commands.extend(newer.commands);
        self.force_poll |= newer.force_poll;
        if newer.schedule_delay.is_some() {
            self.schedule_delay = newer.schedule_delay;
        }
        self.status_refresh |= newer.status_refresh;
    }
}

/// Shared mailbox between MotorRecord, MotorDeviceSupport, and PollLoop.
///
/// Data flow:
///   PollLoop → [`Callbacks`] → latest_status, expired_delay_id on the pass
///   io_intr runs → Record.process() reads
///   Record.process() → pending_actions → DeviceSupport.write() consumes
#[derive(Debug, Default)]
pub struct MotorDeviceState {
    /// C `pPvt->status` as the record's pass sees it; a new `seq` is
    /// `needUpdate` (devMotorAsyn.c:446).
    pub latest_status: Option<StampedStatus>,
    /// The `seq` [`Self::update_values`] last took: `needUpdate` is
    /// `latest_status` past it.
    consumed_seq: u64,
    pub expired_delay_id: Option<u64>,

    // Record → DeviceSupport
    pub pending_actions: Option<DeviceActions>,

    pub callbacks: Callbacks,
}

/// One process a callback asked for, with what that pass reads.
#[derive(Debug, Clone, PartialEq)]
enum Callback {
    /// `statusCallback` or a finished move: `needUpdate` with this status.
    Status(MotorStatus),
    /// The settle delay's `callbackFunc` (motorRecord.cc:460-480).
    Delay(u64),
}

/// devMotorAsyn's per-record callback state (`motorAsynPvt`): the single
/// owner that turns a driver post, a finished move or the settle delay into
/// a record process.
///
/// C calls `dbProcess` from the callback itself, so that pass reads the
/// status the callback copied. Here the pass runs later on the record's
/// io_intr task, so each asked-for process is queued with what it reads and
/// paired with exactly one io_intr pulse; [`MotorDeviceState::begin_callback_pass`]
/// takes one per pass.
#[derive(Debug, Default)]
pub struct Callbacks {
    /// `pPvt->status`: the last status posted.
    status: MotorStatus,
    /// `pPvt->moveRequestPending`.
    move_request_pending: u32,
    /// devMotorAsyn's `dbScanLockOK`, here its database's `finishDevSup` gate.
    accepting: bool,
    /// `pmr->stat == COMM_ALARM` as the record's last pass committed it.
    comm_alarm: bool,
    queued: std::collections::VecDeque<Callback>,
}

impl MotorDeviceState {
    /// C `statusCallback` (devMotorAsyn.c:743-770): copy the post, and ask
    /// for a process unless a move is pending. Before the gate opens it only
    /// copies, with `needUpdate`. True when the caller must pulse io_intr.
    pub fn status_callback(&mut self, status: &MotorStatus) -> bool {
        self.callbacks.status = status.clone();
        if !self.callbacks.accepting {
            self.set_need_update(status.clone());
            return false;
        }
        if self.callbacks.move_request_pending > 0 {
            return false;
        }
        self.queue(Callback::Status(status.clone()));
        true
    }

    /// C `build_trans` queuing a move-class command (devMotorAsyn.c:547,
    /// :565, :582).
    pub fn move_queued(&mut self) {
        self.callbacks.move_request_pending += 1;
    }

    /// C `asynCallback`'s tail for a move-class command, whatever the write
    /// returned (devMotorAsyn.c:717-728): the last move to finish processes
    /// the record with the last status posted. Before the gate a
    /// SET_POSITION clears the count instead. True when the caller must
    /// pulse io_intr.
    pub fn move_done(&mut self, command: &crate::flags::MotorCommand) -> bool {
        let callbacks = &mut self.callbacks;
        if !callbacks.accepting {
            if matches!(command, crate::flags::MotorCommand::SetPosition { .. }) {
                callbacks.move_request_pending = 0;
            }
            return false;
        }
        callbacks.move_request_pending = callbacks.move_request_pending.saturating_sub(1);
        if callbacks.move_request_pending > 0 {
            return false;
        }
        let status = callbacks.status.clone();
        self.queue(Callback::Status(status));
        true
    }

    /// The settle delay expired. True when the caller must pulse io_intr;
    /// before the gate the expiry only waits for the next pass.
    pub fn delay_expired(&mut self, id: u64) -> bool {
        if !self.callbacks.accepting {
            self.expired_delay_id = Some(id);
            return false;
        }
        self.queue(Callback::Delay(id));
        true
    }

    /// The record's `stat` as its passes before this batch committed it;
    /// COMM_ALARM or not is all `build_trans` reads of it.
    pub fn set_comm_alarm(&mut self, comm_alarm: bool) {
        self.callbacks.comm_alarm = comm_alarm;
    }

    /// C `build_trans` (devMotorAsyn.c:529-532): a record in COMM_ALARM
    /// queues nothing, so counts no move pending either.
    pub(crate) fn comm_alarm(&self) -> bool {
        self.callbacks.comm_alarm
    }

    /// `finishDevSup` ran: callbacks process the record from now on.
    pub fn accept(&mut self) {
        self.callbacks.accepting = true;
    }

    /// The pass one io_intr pulse runs: hand it the oldest asked-for
    /// process.
    pub fn begin_callback_pass(&mut self) {
        match self.callbacks.queued.pop_front() {
            Some(Callback::Status(status)) => self.set_need_update(status),
            Some(Callback::Delay(id)) => self.expired_delay_id = Some(id),
            None => {}
        }
    }

    /// The axis' cached status, as `readGenericPointer` returns it at bind.
    fn seed(&mut self, status: &StampedStatus) {
        self.latest_status = Some(status.clone());
        self.callbacks.status = status.status.clone();
    }

    /// C `update_values` (devMotorAsyn.c:437-480): the status a pending
    /// `needUpdate` carries, clearing it.
    pub fn update_values(&mut self) -> Option<MotorStatus> {
        let stamped = self
            .latest_status
            .as_ref()
            .filter(|s| s.seq != self.consumed_seq)?;
        self.consumed_seq = stamped.seq;
        Some(stamped.status.clone())
    }

    /// The status init_record reads, then takes with motorRecord's
    /// init-time `update_values` (motorRecord.cc:690): the axis' cache, or
    /// C's calloc-zeroed status when the poller never filled it.
    pub fn init_status(&mut self) -> MotorStatus {
        let stamped = self.latest_status.get_or_insert_with(|| StampedStatus {
            seq: 1,
            status: MotorStatus {
                done: false,
                ..Default::default()
            },
        });
        self.consumed_seq = stamped.seq;
        stamped.status.clone()
    }

    fn need_update(&self) -> bool {
        self.latest_status
            .as_ref()
            .is_some_and(|s| s.seq != self.consumed_seq)
    }

    fn set_need_update(&mut self, status: MotorStatus) {
        let seq = self.latest_status.as_ref().map_or(1, |s| s.seq + 1);
        self.latest_status = Some(StampedStatus { seq, status });
    }

    fn queue(&mut self, callback: Callback) {
        self.callbacks.queued.push_back(callback);
    }
}

pub type SharedDeviceState = Arc<Mutex<MotorDeviceState>>;

pub fn new_shared_state() -> SharedDeviceState {
    Arc::new(Mutex::new(MotorDeviceState::default()))
}

/// A record bound to an axis, by the order it attached.
pub type RecordId = usize;

/// The records bound to one axis, C's `registerInterruptUser` list on its
/// `motorStatus` parameter: every status the poller posts reaches each of
/// them (devMotorAsyn.c:376-387, asynMotorAxis.cpp:316-322).
#[derive(Clone, Default)]
pub struct AxisSubscribers(Arc<Mutex<Subscribed>>);

#[derive(Default)]
struct Subscribed {
    /// The axis' parameter cache: what `readGenericPointer` returns to a
    /// record that binds after the poller primed it.
    latest: Option<StampedStatus>,
    records: Vec<Bound>,
}

/// One bound record: its mailbox, its io_intr pulse and the user its
/// `statusCallback` prints through.
struct Bound {
    device_state: SharedDeviceState,
    io_intr: tokio::sync::mpsc::Sender<()>,
    trace: Arc<MotorTrace>,
}

/// The io_intr pulses a post or a finished move owes, one per process.
pub type Pulses = Vec<tokio::sync::mpsc::Sender<()>>;

impl AxisSubscribers {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Subscribed> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Bind a record: its mailbox starts from the axis' cache, and every
    /// later post reaches its [`Callbacks`].
    pub fn attach(
        &self,
        device_state: SharedDeviceState,
        io_intr: tokio::sync::mpsc::Sender<()>,
        trace: Arc<MotorTrace>,
    ) -> RecordId {
        let mut subscribed = self.lock();
        if let (Some(latest), Ok(mut ds)) = (&subscribed.latest, device_state.lock()) {
            ds.seed(latest);
        }
        subscribed.records.push(Bound {
            device_state,
            io_intr,
            trace,
        });
        subscribed.records.len() - 1
    }

    /// How many records are bound; their ids are `0..len`.
    pub fn len(&self) -> usize {
        self.lock().records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Fill the cache and every bound mailbox without a callback: the
    /// poller's startup poll.
    pub fn store(&self, status: StampedStatus) {
        let mut subscribed = self.lock();
        for bound in &subscribed.records {
            if let Ok(mut ds) = bound.device_state.lock() {
                ds.seed(&status);
            }
        }
        subscribed.latest = Some(status);
    }

    /// Fill the cache and run every bound record's `statusCallback`; the
    /// pulses the processes it asked for are owed.
    pub fn post(&self, status: StampedStatus) -> Pulses {
        let mut subscribed = self.lock();
        let mut pulses = Pulses::new();
        for bound in &subscribed.records {
            if let Ok(mut ds) = bound.device_state.lock() {
                print_status_callback(&bound.trace, &ds, &status.status);
                if ds.status_callback(&status.status) {
                    pulses.push(bound.io_intr.clone());
                }
            }
        }
        subscribed.latest = Some(status);
        pulses
    }

    /// `record`'s move-class command finished ([`MotorDeviceState::move_done`]).
    pub fn move_done(
        &self,
        record: RecordId,
        command: &crate::flags::MotorCommand,
    ) -> Option<tokio::sync::mpsc::Sender<()>> {
        let subscribed = self.lock();
        let bound = subscribed.records.get(record)?;
        let mut ds = bound.device_state.lock().ok()?;
        ds.move_done(command).then(|| bound.io_intr.clone())
    }
}

/// C `statusCallback`'s line (devMotorAsyn.c:750-754), printed before the
/// copy: `N` for a `needUpdate` no pass has taken, `P` for a pending move.
fn print_status_callback(trace: &MotorTrace, ds: &MotorDeviceState, status: &MotorStatus) {
    trace.print(
        TraceMask::IO_DEVICE,
        trace.status_reason(),
        format_args!(
            "{} devMotorAsyn::statusCallback new value=[p:{:.6},e:{:.6},s:{:x}] {}{}\n",
            trace.record(),
            status.position,
            status.encoder_position,
            crate::flags::MstaFlags::from_status(status).bits(),
            if ds.need_update() { 'N' } else { ' ' },
            if ds.callbacks.move_request_pending > 0 {
                'P'
            } else {
                ' '
            },
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flags::MotorCommand;

    fn stamped(seq: u64, position: f64) -> StampedStatus {
        StampedStatus {
            seq,
            status: MotorStatus {
                position,
                ..Default::default()
            },
        }
    }

    fn seq_of(state: &SharedDeviceState) -> Option<u64> {
        state.lock().unwrap().latest_status.as_ref().map(|s| s.seq)
    }

    fn accepting() -> SharedDeviceState {
        let state = new_shared_state();
        state.lock().unwrap().accept();
        state
    }

    fn at(position: f64) -> MotorStatus {
        MotorStatus {
            position,
            ..Default::default()
        }
    }

    /// The position the next io_intr pass reads, if it reads a status.
    fn pass(state: &SharedDeviceState) -> Option<f64> {
        let mut ds = state.lock().unwrap();
        let before = ds.latest_status.as_ref().map(|s| s.seq);
        ds.begin_callback_pass();
        let after = ds.latest_status.as_ref().unwrap();
        (Some(after.seq) != before).then_some(after.status.position)
    }

    fn move_to() -> MotorCommand {
        MotorCommand::MoveAbsolute {
            position: 1.0,
            min_velocity: 0.0,
            velocity: 1.0,
            acceleration: 1.0,
        }
    }

    #[test]
    fn an_axis_with_no_record_keeps_its_post_for_the_first_to_bind() {
        let axis = AxisSubscribers::new();
        assert!(axis.post(stamped(4, 1.5)).is_empty());
        let state = accepting();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        assert_eq!(axis.attach(state.clone(), tx, Arc::default()), 0);
        assert_eq!(seq_of(&state), Some(4));
        assert!(
            rx.try_recv().is_err(),
            "binding does not process the record"
        );
    }

    #[test]
    fn store_fills_the_mailbox_without_a_callback() {
        let axis = AxisSubscribers::new();
        let state = accepting();
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        axis.attach(state.clone(), tx, Arc::default());
        axis.store(stamped(2, 0.0));
        assert_eq!(seq_of(&state), Some(2));
        assert_eq!(pass(&state), None);
    }

    #[test]
    fn every_record_on_the_axis_gets_one_process_per_post() {
        let axis = AxisSubscribers::new();
        let (a, b) = (accepting(), accepting());
        let (tx_a, _rx_a) = tokio::sync::mpsc::channel(1);
        let (tx_b, _rx_b) = tokio::sync::mpsc::channel(1);
        assert_eq!(axis.attach(a.clone(), tx_a, Arc::default()), 0);
        assert_eq!(axis.attach(b.clone(), tx_b, Arc::default()), 1);
        assert_eq!(axis.post(stamped(2, 1.0)).len(), 2);
        assert_eq!(axis.post(stamped(3, 2.0)).len(), 2);
        for state in [&a, &b] {
            assert_eq!(pass(state), Some(1.0));
            assert_eq!(pass(state), Some(2.0));
            assert_eq!(pass(state), None);
        }
    }

    #[test]
    fn a_post_before_the_gate_copies_with_need_update_and_asks_no_process() {
        let state = new_shared_state();
        let mut ds = state.lock().unwrap();
        assert!(!ds.status_callback(&at(3.0)));
        assert_eq!(ds.latest_status.as_ref().unwrap().status.position, 3.0);
        assert!(ds.callbacks.queued.is_empty());
    }

    #[test]
    fn each_pass_reads_its_own_post_in_order() {
        let state = accepting();
        let mut ds = state.lock().unwrap();
        assert!(ds.status_callback(&at(1.0)));
        assert!(ds.status_callback(&at(2.0)));
        drop(ds);
        assert_eq!(pass(&state), Some(1.0));
        assert_eq!(pass(&state), Some(2.0));
    }

    #[test]
    fn a_post_while_one_move_is_pending_waits_for_its_completion() {
        let state = accepting();
        let mut ds = state.lock().unwrap();
        ds.move_queued();
        assert!(!ds.status_callback(&at(1.0)));
        assert!(!ds.status_callback(&at(2.0)));
        assert!(ds.move_done(&move_to()));
        drop(ds);
        assert_eq!(pass(&state), Some(2.0));
        assert_eq!(pass(&state), None);
    }

    #[test]
    fn only_the_last_of_two_pending_moves_processes() {
        let state = accepting();
        let mut ds = state.lock().unwrap();
        ds.move_queued();
        ds.move_queued();
        assert!(!ds.move_done(&move_to()));
        assert!(!ds.status_callback(&at(5.0)));
        assert!(ds.move_done(&move_to()));
        assert!(ds.status_callback(&at(6.0)), "nothing pending");
        drop(ds);
        assert_eq!(pass(&state), Some(5.0));
        assert_eq!(pass(&state), Some(6.0));
    }

    #[test]
    fn before_the_gate_only_a_set_position_clears_the_pending_count() {
        let state = new_shared_state();
        let mut ds = state.lock().unwrap();
        ds.move_queued();
        ds.move_queued();
        assert!(!ds.move_done(&move_to()));
        assert_eq!(ds.callbacks.move_request_pending, 2);
        assert!(!ds.move_done(&MotorCommand::SetPosition { position: 0.0 }));
        assert_eq!(ds.callbacks.move_request_pending, 0);
    }

    #[test]
    fn a_delay_expiry_takes_its_turn_behind_the_posts_before_it() {
        let state = accepting();
        let mut ds = state.lock().unwrap();
        assert!(ds.status_callback(&at(1.0)));
        ds.delay_expired(7);
        drop(ds);
        assert_eq!(pass(&state), Some(1.0));
        assert_eq!(state.lock().unwrap().expired_delay_id, None);
        assert_eq!(pass(&state), None);
        assert_eq!(state.lock().unwrap().expired_delay_id, Some(7));
    }

    #[test]
    fn merge_newer_appends_commands_fifo() {
        let mut a = DeviceActions {
            commands: vec![MotorCommand::Stop { acceleration: 1.0 }],
            ..Default::default()
        };
        let b = DeviceActions {
            commands: vec![MotorCommand::Poll],
            ..Default::default()
        };
        a.merge_newer(b);
        assert_eq!(a.commands.len(), 2);
        // earlier batch's command stays first
        assert!(matches!(a.commands[0], MotorCommand::Stop { .. }));
        assert!(matches!(a.commands[1], MotorCommand::Poll));
    }

    #[test]
    fn merge_newer_keeps_a_forced_poll_from_either_batch() {
        for (older, newer) in [(true, false), (false, true)] {
            let mut a = DeviceActions {
                force_poll: older,
                ..Default::default()
            };
            a.merge_newer(DeviceActions {
                force_poll: newer,
                ..Default::default()
            });
            assert!(a.force_poll);
        }
    }

    #[test]
    fn merge_newer_takes_newer_delay_and_refresh() {
        let mut a = DeviceActions::default();
        let b = DeviceActions {
            schedule_delay: Some(DelayRequest {
                id: 7,
                duration: Duration::from_millis(5),
            }),
            status_refresh: true,
            ..Default::default()
        };
        a.merge_newer(b);
        assert_eq!(a.schedule_delay.as_ref().unwrap().id, 7);
        assert!(a.status_refresh);
    }
}
