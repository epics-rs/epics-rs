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
//! CA-FR-5: each scheduled `get`/`put` spawns its op immediately (it is
//! "in flight" the moment it is scheduled, matching libca), and the
//! group tracks the outstanding tasks. [`Self::block`] takes `&mut self`
//! and waits only for the requests issued since the last successful
//! `block`/`reset` (libca clears the outstanding set on a successful
//! `ca_sg_block`); a timeout leaves them outstanding so the caller can
//! [`Self::test`] / retry / [`Self::reset`]. [`Self::test`] reports
//! completion without consuming the group, [`Self::reset`] aborts and
//! discards the outstanding batch, and [`Self::stat`] exposes the
//! outstanding/completed counts.

use std::time::Duration;

use tokio::task::JoinHandle;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::task::spawn;
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
/// submission order, plus every put's result in submission order.
#[derive(Debug)]
pub struct SyncGroupResults {
    pub gets: Vec<GetOutput>,
    pub puts: Vec<CaResult<()>>,
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
    /// elapses. Mirrors libca `ca_sg_block(gid, timeout)`: on success
    /// the outstanding set is cleared and the results returned (the
    /// group is then reusable for a fresh batch); on timeout the ops
    /// remain outstanding (`Err(Timeout)`) for [`Self::test`] / retry /
    /// [`Self::reset`].
    pub async fn block(&mut self, timeout: Duration) -> CaResult<SyncGroupResults> {
        // The tasks already run concurrently (spawned at schedule time),
        // so awaiting them in order just *collects* results — wall time
        // is bounded by the slowest op, and the single outer timeout
        // fires if that exceeds `timeout`. Each awaited handle is
        // borrowed by `&mut`, so a timeout leaves every still-running
        // task — and its handle — intact in `self.ops`.
        let collect = async {
            for op in self.ops.iter_mut() {
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
            }
        };

        match tokio::time::timeout(timeout, collect).await {
            Ok(()) => {
                // libca: a successful block clears the outstanding set.
                let mut gets = Vec::new();
                let mut puts = Vec::new();
                for op in std::mem::take(&mut self.ops) {
                    match op.done.expect("collect filled every op on success") {
                        Outcome::Get(r) => gets.push(r),
                        Outcome::Put(r) => puts.push(r),
                    }
                }
                Ok(SyncGroupResults { gets, puts })
            }
            // Timeout: collected ops keep `done = Some`, the rest stay in
            // flight; the batch remains for a later block/test/reset.
            Err(_) => Err(CaError::Timeout),
        }
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
    /// while keeping the group usable (libca `ca_sg_reset`).
    pub fn reset(&mut self) {
        for op in &self.ops {
            op.handle.abort();
        }
        self.ops.clear();
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

#[cfg(test)]
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
        let r = g
            .block(Duration::from_millis(50))
            .await
            .expect("empty group never times out");
        assert!(r.gets.is_empty() && r.puts.is_empty());
    }

    /// CA-FR-5: a successful `block` clears the batch and the same group
    /// then accepts a second batch and waits only for it.
    #[tokio::test]
    async fn reusable_block_waits_only_for_current_batch() {
        let mut g = SyncGroup::new();
        g.push_delayed_get(10, 1);
        g.push_delayed_get(20, 2);
        let r1 = g.block(Duration::from_secs(2)).await.unwrap();
        assert_eq!(r1.gets.len(), 2, "first batch");
        assert!(g.is_empty(), "successful block clears the batch");

        g.push_delayed_get(10, 3);
        let r2 = g.block(Duration::from_secs(2)).await.unwrap();
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
        let r = g.block(Duration::from_secs(1)).await.unwrap();
        assert_eq!(r.gets.len(), 1);
    }

    /// `reset` after a timeout discards the outstanding batch and the
    /// group reports complete/empty again.
    #[tokio::test]
    async fn reset_after_timeout_discards_outstanding() {
        let mut g = SyncGroup::new();
        g.push_delayed_get(60_000, 1); // never completes in the test window
        let r = g.block(Duration::from_millis(20)).await;
        assert!(matches!(r, Err(CaError::Timeout)), "block times out");
        assert_eq!(
            g.stat().outstanding,
            1,
            "op still outstanding after timeout"
        );
        g.reset();
        assert!(g.is_empty());
        assert_eq!(g.test(), SyncGroupStatus::Done);
        assert_eq!(
            g.stat(),
            SyncGroupStat {
                outstanding: 0,
                completed: 0
            }
        );
    }
}
