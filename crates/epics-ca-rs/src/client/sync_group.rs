//! [`SyncGroup`] — batch async CA ops + collective wait, reusable
//! across batches like libca `ca_sg_*` (`syncgrp.cpp`):
//!
//! ```text
//! CA_SYNC_GID gid;
//! ca_sg_create(&gid);
//! ca_sg_array_get(gid, ...);   // schedule a get (issued immediately)
//! ca_sg_array_put(gid, ...);   // schedule a put
//! ca_sg_block(gid, 5.0);       // wait for THIS batch
//! ca_sg_test(gid);             // poll completion without consuming
//! ca_sg_reset(gid);            // discard outstanding, keep the gid
//! ca_sg_block(gid, 5.0);       // reuse for the next batch
//! ca_sg_delete(gid);
//! ```
//!
//! each scheduled `get`/`put` spawns its op immediately (it is
//! "in flight" the moment it is scheduled, matching libca), and the
//! group tracks the outstanding tasks. [`SyncGroup::block`] takes `&mut self`
//! and waits only for the requests issued since the last `block`/`reset`.
//! libca `ca_sg_block` ends the batch on **every** return — success
//! *and* timeout — because it unconditionally calls `sync_group_reset`
//! after `CASG::block` returns (`syncgrp.cpp:139-147`); the next
//! `ca_sg_block` then waits only for ops issued afterward (`cadef.h`).
//! So a timed-out `block` here also empties the batch: retry means
//! scheduling fresh ops, not re-blocking the old ones. Ending the batch
//! is not losing what arrived, though: C hands each result to the caller
//! as it completes (`syncGroupReadNotify::completion` memcpy's into the
//! caller's buffer, `syncGroupReadNotify.cpp:91-94`) and
//! `CASG::reset` then destroys the notify records, not the data
//! (`CASG.cpp:132-137`). [`SyncGroup::block`] therefore returns the
//! results that arrived alongside the timeout status. [`SyncGroup::test`]
//! reports completion without consuming the group, [`SyncGroup::reset`] aborts
//! and discards the outstanding batch, and [`SyncGroup::stat`] exposes the
//! outstanding/completed counts.

use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::task::spawn;
// The seam handle, not a bare `tokio::task::JoinHandle`: `spawn` (imported
// below) returns this, and under `rtems-exec-model` it is the executor's
// `JoinFuture`. Byte-identical to `tokio::task::JoinHandle` under the default.
use epics_base_rs::runtime::task::TaskHandle as JoinHandle;
use epics_base_rs::types::{DbFieldType, EpicsValue};

use super::CaChannel;

/// Result of a single scheduled get.
type GetOutput = CaResult<(DbFieldType, EpicsValue)>;

/// Which operation a tracked task is running — needed to classify a
/// `JoinError` (aborted/panicked task) when there is no `Outcome`.
#[derive(Clone, Copy)]
enum OpKind {
    Get,
    Put,
}

/// The typed result of a completed op.
enum Outcome {
    Get(GetOutput),
    Put(CaResult<()>),
}

/// One tracked operation: a spawned, in-flight task plus its collected
/// result once `block`/`test` observes completion.
struct SyncOp {
    kind: OpKind,
    handle: JoinHandle<Outcome>,
    done: Option<Outcome>,
}

impl SyncOp {
    /// Completed = result already collected, or the task has finished
    /// (result not yet drained). Non-blocking.
    fn is_complete(&self) -> bool {
        self.done.is_some() || self.handle.is_finished()
    }
}

/// `ca_sg_test()` outcome — completion status without consuming the
/// group. Mirrors C `ECA_IODONE` / `ECA_IOINPROGRESS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncGroupStatus {
    /// Every outstanding op has finished (or there are none).
    Done,
    /// At least one op is still in flight.
    InProgress,
}

/// `ca_sg_stat()` diagnostic snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncGroupStat {
    /// Ops issued since the last `block`/`reset` that have not finished.
    pub outstanding: usize,
    /// Ops in the current batch that have finished.
    pub completed: usize,
}

/// Reusable op group. Mirrors libca `CA_SYNC_GID`.
#[derive(Default)]
pub struct SyncGroup {
    /// The current batch: ops issued since the last successful `block`
    /// or `reset`. Drained on a successful `block`.
    ops: Vec<SyncOp>,
}

/// Outcome of [`SyncGroup::block`]: every scheduled get's result in
/// submission order, plus every put's result in submission order, plus
/// the collective status of the wait.
#[derive(Debug)]
pub struct SyncGroupResults {
    /// One entry per scheduled get, in submission order. An op that had
    /// not completed when the deadline fired holds
    /// `Err(CaError::Timeout)` — its slot is kept so index `i` always
    /// means the `i`-th scheduled get.
    pub gets: Vec<GetOutput>,
    /// One entry per scheduled put, same ordering rule.
    pub puts: Vec<CaResult<()>>,
    /// `Ok(())` when every op in the batch finished, `Err(CaError::Timeout)`
    /// when the deadline fired first. libca returns `ECA_TIMEOUT` here and
    /// still leaves every completed read in the caller's buffer, so a
    /// timeout means "not everything arrived", never "nothing arrived".
    pub status: CaResult<()>,
}

impl SyncGroup {
    pub fn new() -> Self {
        Self::default()
    }

    /// Schedule a get. The op is issued (spawned) immediately and runs
    /// concurrently; the group tracks it until [`Self::block`].
    pub fn get(&mut self, ch: &CaChannel) {
        let ch = ch.clone();
        let handle = spawn(async move { Outcome::Get(ch.get().await) });
        self.ops.push(SyncOp {
            kind: OpKind::Get,
            handle,
            done: None,
        });
    }

    /// Schedule a put. Same in-flight semantics as [`Self::get`].
    pub fn put(&mut self, ch: &CaChannel, value: EpicsValue) {
        let ch = ch.clone();
        let handle = spawn(async move { Outcome::Put(ch.put(&value).await) });
        self.ops.push(SyncOp {
            kind: OpKind::Put,
            handle,
            done: None,
        });
    }

    /// Wait until every op in the current batch completes or `timeout`
    /// elapses, then end the batch and return what arrived. Mirrors libca
    /// `ca_sg_block(gid, timeout)`, which ends the batch on **every**
    /// return path: `ca_sg_block` runs `CASG::block` and then calls
    /// `sync_group_reset` unconditionally (`syncgrp.cpp:139-147`).
    ///
    /// That reset destroys the group's notify records
    /// (`CASG::destroyCompletedIO` / `destroyPendingIO`,
    /// `CASG.cpp:132-137`); it does not and cannot take back a value,
    /// because each read is copied into the caller's own buffer the
    /// moment it completes (`syncGroupReadNotify::completion`,
    /// `syncGroupReadNotify.cpp:91-94`). A timed-out `ca_sg_block`
    /// returns `ECA_TIMEOUT` with the fast channels' values already in
    /// place, and this returns the same thing: the results collected so
    /// far, with [`SyncGroupResults::status`] carrying
    /// `Err(CaError::Timeout)`.
    ///
    /// After either return path the batch is empty: [`Self::test`] reports
    /// [`SyncGroupStatus::Done`], [`Self::stat`] shows zero outstanding, and
    /// a later `block` waits only for freshly scheduled ops — retry means
    /// scheduling a new batch, not re-blocking the timed-out one.
    pub async fn block(&mut self, timeout: Duration) -> SyncGroupResults {
        // The tasks already run concurrently (spawned at schedule time), so
        // this only *collects* results. Collect them concurrently rather
        // than in submission order: each op's result lands in its own slot
        // the moment that op finishes, so a slow op cannot hide the results
        // of ops behind it in the batch when the deadline fires. This is
        // where C's per-completion memcpy into the caller's buffer happens.
        let collect = futures_util::future::join_all(self.ops.iter_mut().map(|op| async move {
            if op.done.is_none() {
                let kind = op.kind;
                let outcome = match (&mut op.handle).await {
                    Ok(o) => o,
                    // Aborted (reset) or panicked task — surface as a
                    // disconnect for that op rather than failing the
                    // whole block.
                    Err(_) => match kind {
                        OpKind::Get => Outcome::Get(Err(CaError::Disconnected)),
                        OpKind::Put => Outcome::Put(Err(CaError::Disconnected)),
                    },
                };
                op.done = Some(outcome);
            }
        }));

        // `runtime::task::timeout`, not `tokio::time::timeout`: this module
        // is compiled for the RTEMS target under `client-core`, where no
        // tokio timer exists to drive the latter.
        let status = match epics_base_rs::runtime::task::timeout(timeout, collect).await {
            Ok(_) => Ok(()),
            Err(_) => Err(CaError::Timeout),
        };

        let (gets, puts) = self.end_batch();
        SyncGroupResults { gets, puts, status }
    }

    /// End the current batch: abort whatever is still in flight, drain what
    /// completed, and leave the group empty and reusable.
    ///
    /// The single owner of that transition. Every path that empties the
    /// batch goes through here and is handed the results it just took, so
    /// no path can throw away a result that already arrived — which is what
    /// the timeout arm used to do by routing through `reset`.
    fn end_batch(&mut self) -> (Vec<GetOutput>, Vec<CaResult<()>>) {
        let mut gets = Vec::new();
        let mut puts = Vec::new();
        for op in std::mem::take(&mut self.ops) {
            let SyncOp { kind, handle, done } = op;
            let outcome = match done {
                Some(o) => o,
                // Never delivered before the batch ended. Abort it and
                // record the deadline as this op's own outcome, so the
                // slot still stands for the op that was scheduled there.
                None => {
                    handle.abort();
                    match kind {
                        OpKind::Get => Outcome::Get(Err(CaError::Timeout)),
                        OpKind::Put => Outcome::Put(Err(CaError::Timeout)),
                    }
                }
            };
            match outcome {
                Outcome::Get(r) => gets.push(r),
                Outcome::Put(r) => puts.push(r),
            }
        }
        (gets, puts)
    }

    /// Poll completion without consuming the group (libca `ca_sg_test`).
    /// Returns [`SyncGroupStatus::Done`] when every outstanding op has
    /// finished (or there are none), else `InProgress`.
    pub fn test(&self) -> SyncGroupStatus {
        if self.ops.iter().all(SyncOp::is_complete) {
            SyncGroupStatus::Done
        } else {
            SyncGroupStatus::InProgress
        }
    }

    /// Discard the outstanding batch, aborting any still-running tasks,
    /// while keeping the group usable (libca `ca_sg_reset`). The caller
    /// is explicitly throwing the batch away, so the drained results have
    /// nowhere to go.
    pub fn reset(&mut self) {
        self.end_batch();
    }

    /// Diagnostic snapshot of the current batch (libca `ca_sg_stat`).
    pub fn stat(&self) -> SyncGroupStat {
        let completed = self.ops.iter().filter(|op| op.is_complete()).count();
        SyncGroupStat {
            outstanding: self.ops.len() - completed,
            completed,
        }
    }

    /// Number of ops in the current batch (outstanding + collected).
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// True if no ops are scheduled in the current batch.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

// Host/tokio-only: the shared `push_delayed_get` harness spawns
// `tokio::time::sleep` through the `runtime::task` seam, which under
// `rtems-exec-model` lands on a background-executor worker with no tokio
// reactor. Three of the four tests here build their batch with it. Module
// granularity follows `server_connection_drop_tests`; only
// `empty_group_blocks_immediately` would stand alone under the feature.
#[cfg(all(test, not(feature = "rtems-exec-model")))]
mod tests {
    use super::*;

    /// Test-only: schedule a fake get that resolves to `val` after
    /// `ms`, so the batch/reset/test/stat semantics can be exercised
    /// deterministically without a live IOC.
    impl SyncGroup {
        fn push_delayed_get(&mut self, ms: u64, val: i32) {
            let handle = spawn(async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Outcome::Get(Ok((DbFieldType::Long, EpicsValue::Long(val))))
            });
            self.ops.push(SyncOp {
                kind: OpKind::Get,
                handle,
                done: None,
            });
        }
    }

    #[tokio::test]
    async fn empty_group_blocks_immediately() {
        let mut g = SyncGroup::new();
        assert!(g.is_empty());
        assert_eq!(g.test(), SyncGroupStatus::Done);
        let r = g.block(Duration::from_millis(50)).await;
        assert!(r.status.is_ok(), "empty group never times out");
        assert!(r.gets.is_empty() && r.puts.is_empty());
    }

    /// a successful `block` clears the batch and the same group
    /// then accepts a second batch and waits only for it.
    #[tokio::test]
    async fn reusable_block_waits_only_for_current_batch() {
        let mut g = SyncGroup::new();
        g.push_delayed_get(10, 1);
        g.push_delayed_get(20, 2);
        let r1 = g.block(Duration::from_secs(2)).await;
        assert!(r1.status.is_ok());
        assert_eq!(r1.gets.len(), 2, "first batch");
        assert!(g.is_empty(), "successful block clears the batch");

        g.push_delayed_get(10, 3);
        let r2 = g.block(Duration::from_secs(2)).await;
        assert!(r2.status.is_ok());
        assert_eq!(r2.gets.len(), 1, "second block waits only for the new op");
    }

    /// `test` reports in-progress while an op is pending and done once it
    /// completes — without consuming the group.
    #[tokio::test]
    async fn test_reports_in_progress_then_done() {
        let mut g = SyncGroup::new();
        g.push_delayed_get(80, 1);
        assert_eq!(g.test(), SyncGroupStatus::InProgress);
        assert_eq!(g.stat().outstanding, 1);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(g.test(), SyncGroupStatus::Done);
        assert_eq!(g.stat().completed, 1);
        // Still usable: block now collects the finished op.
        let r = g.block(Duration::from_secs(1)).await;
        assert!(r.status.is_ok());
        assert_eq!(r.gets.len(), 1);
    }

    /// C parity: `block` ends the batch on the timeout return path, just
    /// like a successful return. After a timed-out `block` the group is
    /// already empty — `test() == Done`, `stat().outstanding == 0`, no
    /// explicit `reset` needed — matching libca `ca_sg_block`'s
    /// unconditional `sync_group_reset` (syncgrp.cpp:139-147).
    #[tokio::test]
    async fn block_timeout_ends_the_batch_like_c() {
        let mut g = SyncGroup::new();
        g.push_delayed_get(60_000, 1); // never completes in the test window
        let r = g.block(Duration::from_millis(20)).await;
        assert!(matches!(r.status, Err(CaError::Timeout)), "block times out");
        assert!(
            matches!(r.gets[0], Err(CaError::Timeout)),
            "the op that never delivered keeps its slot"
        );

        // The timed-out batch is discarded WITHOUT an explicit reset.
        assert!(g.is_empty(), "timeout empties the batch");
        assert_eq!(g.test(), SyncGroupStatus::Done, "test() reports IODONE");
        assert_eq!(
            g.stat(),
            SyncGroupStat {
                outstanding: 0,
                completed: 0
            },
            "no outstanding ops after a timed-out block"
        );

        // A later block waits only for freshly scheduled ops — it does NOT
        // re-collect the timed-out task (which C had already discarded).
        g.push_delayed_get(10, 7);
        let r2 = g.block(Duration::from_secs(2)).await;
        assert!(r2.status.is_ok(), "fresh batch completes");
        assert_eq!(r2.gets.len(), 1, "only the new op is awaited");
        assert!(
            matches!(r2.gets[0], Ok((_, EpicsValue::Long(7)))),
            "the new op's result, not the discarded one"
        );
    }

    /// CA4-1: a timeout must not un-deliver what already arrived. C
    /// memcpy's each completed read into the caller's buffer as it
    /// completes, so `ECA_TIMEOUT` from `ca_sg_block` leaves the fast
    /// channel's value in place; the slow one's buffer is untouched.
    #[tokio::test]
    async fn block_timeout_keeps_the_results_that_arrived() {
        let mut g = SyncGroup::new();
        g.push_delayed_get(10, 7); // fast: completes well inside the window
        g.push_delayed_get(60_000, 9); // slow: never completes
        let r = g.block(Duration::from_millis(200)).await;

        assert!(matches!(r.status, Err(CaError::Timeout)), "batch times out");
        assert_eq!(r.gets.len(), 2, "one slot per scheduled get");
        assert!(
            matches!(r.gets[0], Ok((_, EpicsValue::Long(7)))),
            "the fast op's value survives the timeout, got {:?}",
            r.gets[0]
        );
        assert!(
            matches!(r.gets[1], Err(CaError::Timeout)),
            "the slow op reports the deadline"
        );
    }

    /// The same, with the slow op scheduled FIRST: results are collected
    /// concurrently, so an op that finished cannot be hidden behind an op
    /// that is still running earlier in the batch.
    #[tokio::test]
    async fn a_slow_op_does_not_hide_results_behind_it() {
        let mut g = SyncGroup::new();
        g.push_delayed_get(60_000, 9); // slow, scheduled first
        g.push_delayed_get(10, 7);
        let r = g.block(Duration::from_millis(200)).await;

        assert!(matches!(r.status, Err(CaError::Timeout)));
        assert!(
            matches!(r.gets[0], Err(CaError::Timeout)),
            "the slow op reports the deadline"
        );
        assert!(
            matches!(r.gets[1], Ok((_, EpicsValue::Long(7)))),
            "the op behind it still delivered, got {:?}",
            r.gets[1]
        );
    }
}
