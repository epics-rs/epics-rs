//! `ProcessOutcome::post_write_fields`: a completion flag becomes visible only
//! after the cycle's queued link writes have run.
//!
//! C runs both under ONE `dbScanLock`. `sseqRecord.c::processCallback` puts
//! `LNKn` (`:714-792`) and `asyncFinish` clears `busy` (`:498-505`) inside the
//! lock `dbProcess` took; `scalerRecord.c::process` clears `cnt` (`:370`) and
//! puts `COUT`/`COUTP` (`:457`, `:463`) in the same call. `dbGetField` takes
//! that lock too, so a reader lands either wholly before the region or wholly
//! after it — `BUSY == 0` with `LNKn` unwritten is a state C cannot expose.
//!
//! This port cannot hold the record's data guard across the writes (a
//! self/cyclic OUT link would dead-lock the non-reentrant guard), so the flag
//! store travels out of `process()` in `post_write_fields` and the framework's
//! drain applies it after the writes.
//!
//! The reader here is the write TARGET: its `put_field` samples the source's
//! flag at the instant the source's own link write lands on it — the exact
//! moment a `caget` could return between `process()` and the drain.
//!
//! Boundaries, one test each:
//!   * the synchronous `Complete` arm (`processing.rs`, after the link-write
//!     partition),
//!   * the `AsyncPendingNotify` arm (same partition, before
//!     `notify_from_snapshot`),
//!   * a cycle with NO queued link writes,
//!   * a group whose first field cannot store,
//!   * and the control: a source that stores the clear inside `process()` — the
//!     shape this replaces — which the same reader catches.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicI16, AtomicI32, Ordering};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::*;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// What the source hands back from `process()`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arm {
    /// `Complete` + a `WriteDbLink`, the flag clear deferred.
    SyncComplete,
    /// `AsyncPendingNotify` + a `WriteDbLink`, the flag clear deferred.
    AsyncNotify,
    /// `Complete`, no actions at all, the flag clear deferred.
    NoLinkWrites,
    /// `Complete` + a `WriteDbLink`, the group led by a field that cannot
    /// store.
    DeferredAfterAFailingField,
    /// The pre-`post_write_fields` shape: `Complete` + a `WriteDbLink`, with
    /// the clear stored inside `process()`.
    StoredInProcess,
}

/// A record with one completion flag (`FLAG`) and one link (`LNK`).
///
/// `FLAG` is backed by a shared atomic so the probe below can sample exactly
/// what `get_field("FLAG")` — hence `dbGetField` / `caget` — would return.
/// `REJECT` is declared but refuses every store, standing in for a field whose
/// put fails at the drain.
struct FlagSource {
    flag: Arc<AtomicI16>,
    arm: Arm,
}

static SRC_FIELDS: &[FieldDesc] = &[
    FieldDesc::new("VAL", DbFieldType::Long, false),
    FieldDesc::new("FLAG", DbFieldType::Short, false),
    FieldDesc::new("REJECT", DbFieldType::Short, false),
    FieldDesc::new("LNK", DbFieldType::String, false),
];

const CLEAR: (&str, EpicsValue) = ("FLAG", EpicsValue::Short(0));

impl Record for FlagSource {
    fn record_type(&self) -> &'static str {
        "flagsrc"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let write = ProcessAction::WriteDbLink {
            link_field: "LNK",
            value: EpicsValue::Long(42),
        };
        let clear = vec![(CLEAR.0.to_string(), CLEAR.1)];
        Ok(match self.arm {
            Arm::SyncComplete => ProcessOutcome {
                post_write_fields: clear,
                ..ProcessOutcome::complete_with(vec![write])
            },
            Arm::AsyncNotify => ProcessOutcome {
                result: RecordProcessResult::AsyncPendingNotify(vec![(
                    "VAL".to_string(),
                    EpicsValue::Long(7),
                )]),
                actions: vec![write],
                device_did_compute: false,
                post_write_fields: clear,
            },
            Arm::NoLinkWrites => ProcessOutcome {
                post_write_fields: clear,
                ..ProcessOutcome::complete()
            },
            Arm::DeferredAfterAFailingField => ProcessOutcome {
                post_write_fields: vec![
                    ("REJECT".to_string(), EpicsValue::Short(1)),
                    (CLEAR.0.to_string(), CLEAR.1),
                ],
                ..ProcessOutcome::complete_with(vec![write])
            },
            Arm::StoredInProcess => {
                self.flag.store(0, Ordering::SeqCst);
                ProcessOutcome::complete_with(vec![write])
            }
        })
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Long(0)),
            "FLAG" => Some(EpicsValue::Short(self.flag.load(Ordering::SeqCst))),
            "REJECT" => Some(EpicsValue::Short(0)),
            "LNK" => Some(EpicsValue::String("PROBE.VAL".into())),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "FLAG" => match value {
                EpicsValue::Short(v) => {
                    self.flag.store(v, Ordering::SeqCst);
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("FLAG".into())),
            },
            // Always refuses, whatever it is handed.
            "REJECT" => Err(CaError::TypeMismatch("REJECT".into())),
            "VAL" | "LNK" => Ok(()),
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        SRC_FIELDS
    }
}

/// The write target, and the reader. Samples the source's `FLAG` at the
/// instant the source's link write lands here — the window between `process()`
/// returning and the drain running.
struct Probe {
    val: i32,
    flag: Arc<AtomicI16>,
    observed: Arc<AtomicI32>,
}

static PROBE_FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Long, false)];

/// No sample taken yet — distinct from every `i16` the flag can hold.
const UNSAMPLED: i32 = -1;

impl Record for Probe {
    fn record_type(&self) -> &'static str {
        "probe"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Long(self.val)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::Long(v) => {
                    self.observed
                        .store(self.flag.load(Ordering::SeqCst) as i32, Ordering::SeqCst);
                    self.val = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        PROBE_FIELDS
    }
}

/// One source (`SRC`, flag raised) wired to one probe (`PROBE`), processed
/// once. Returns `(what the probe saw during the write, the flag afterwards,
/// the probe's VAL)`.
async fn run(arm: Arm) -> (i32, EpicsValue, EpicsValue) {
    let db = PvDatabase::new();
    let flag = Arc::new(AtomicI16::new(1));
    let observed = Arc::new(AtomicI32::new(UNSAMPLED));

    db.add_record(
        "PROBE",
        Box::new(Probe {
            val: 0,
            flag: flag.clone(),
            observed: observed.clone(),
        }),
    )
    .await
    .unwrap();
    db.add_record(
        "SRC",
        Box::new(FlagSource {
            flag: flag.clone(),
            arm,
        }),
    )
    .await
    .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SRC", &mut visited, 0)
        .await
        .unwrap();

    (
        observed.load(Ordering::SeqCst),
        db.get_pv("SRC.FLAG").unwrap(),
        db.get_pv("PROBE.VAL").unwrap(),
    )
}

/// The synchronous `Complete` arm: the drain applies the withheld store after
/// the link-write partition, so the target — reading at the only instant a
/// `caget` could land between the two — still sees the flag SET.
#[epics_macros_rs::epics_test]
async fn the_sync_complete_arm_publishes_the_clear_after_its_link_write() {
    let (observed, flag_after, probe_val) = run(Arm::SyncComplete).await;
    assert_eq!(
        observed, 1,
        "a reader taking the record between `process()` and the drain must see \
         the flag still set — C's `dbScanLock` gives it no other choice"
    );
    assert_eq!(
        flag_after,
        EpicsValue::Short(0),
        "and the drain must then publish the clear"
    );
    assert_eq!(
        probe_val,
        EpicsValue::Long(42),
        "the queued link write itself must still land"
    );
}

/// The control, and the proof that the reader above discriminates: a source
/// that stores the clear inside `process()` — the shape `post_write_fields`
/// replaces — is caught by the same probe, which sees the flag ALREADY clear
/// while its own write has not yet been made.
#[epics_macros_rs::epics_test]
async fn a_clear_stored_inside_process_is_visible_before_the_write_lands() {
    let (observed, flag_after, probe_val) = run(Arm::StoredInProcess).await;
    assert_eq!(
        observed, 0,
        "the pre-fix shape exposes `FLAG == 0` with the link target unwritten"
    );
    assert_eq!(flag_after, EpicsValue::Short(0));
    assert_eq!(probe_val, EpicsValue::Long(42));
}

/// The `AsyncPendingNotify` arm, separately: it partitions the same way and
/// the publication belongs between the link writes and `notify_from_snapshot`.
#[epics_macros_rs::epics_test]
async fn the_async_notify_arm_publishes_the_clear_after_its_link_write() {
    let (observed, flag_after, probe_val) = run(Arm::AsyncNotify).await;
    assert_eq!(
        observed, 1,
        "the notify arm orders the publication after the link write too"
    );
    assert_eq!(flag_after, EpicsValue::Short(0));
    assert_eq!(probe_val, EpicsValue::Long(42));
}

/// A cycle with nothing queued: there is no write to be ordered against, and
/// the fields must still be applied — a record whose clear was silently
/// dropped here would stay busy forever.
#[epics_macros_rs::epics_test]
async fn a_cycle_with_no_queued_link_writes_still_publishes() {
    let (observed, flag_after, probe_val) = run(Arm::NoLinkWrites).await;
    assert_eq!(observed, UNSAMPLED, "no link write was queued, so none ran");
    assert_eq!(
        flag_after,
        EpicsValue::Short(0),
        "the withheld store is not conditional on there being a write"
    );
    assert_eq!(probe_val, EpicsValue::Long(0));
}

/// The group is one transition: a member that cannot store must not strand the
/// members behind it. `REJECT` refuses, and `FLAG` — the completion flag — is
/// still published.
#[epics_macros_rs::epics_test]
async fn a_field_that_cannot_store_does_not_strand_the_rest_of_the_group() {
    let (observed, flag_after, probe_val) = run(Arm::DeferredAfterAFailingField).await;
    assert_eq!(observed, 1, "ordering is unchanged by the failing member");
    assert_eq!(
        flag_after,
        EpicsValue::Short(0),
        "a failed member must not abandon the rest of the group"
    );
    assert_eq!(probe_val, EpicsValue::Long(42));
}
