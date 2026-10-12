//! A `WAITn` put-with-completion that is REFUSED aborts the sequence.
//!
//! C `sseqRecord.c::processCallback` reads the status of
//! `dbCaPutLinkCallback` itself, in three identical arms (`:722-730`,
//! `:744-752`, `:774-782`):
//!
//! ```c
//! status = dbCaPutLinkCallback(&(plinkGroup->lnk), DBR_DOUBLE, ...);
//! if (status) {
//!     pR->abort = 1;
//!     db_post_events(pR, &pR->abort, DBE_VALUE);
//!     printf("sseq:processCallback: dbCaPutLinkCallback for link %d failed.  Aborting.\n", pcb->index);
//! } else {
//!     plinkGroup->waiting = 1;
//!     db_post_events(pR, &plinkGroup->waiting, DBE_VALUE);
//!     did_putCallback = 1;
//! }
//! ```
//!
//! The status comes from `dbCa.c:528-532` — `if (!pca->isConnected ||
//! !pca->hasWriteAccess) return -1;`. With `abort` set, `processNextLink`
//! (`sseqRecord.c:443`) skips the `DLYn` delay and the callback it requests
//! bails at `processCallback`'s abort gate (`sseqRecord.c:621-627`) into
//! `asyncFinish` (`sseqRecord.c:460-508`),
//! so every LATER link group is skipped.
//!
//! The port raised `waiting`/`WTGn` and pushed the step into `in_flight`
//! unconditionally, so a refused put looked like an issued one: the sequence
//! ran on through the remaining steps C never reaches.
//!
//! The link here reports a DBF class while refusing the put — C's cached
//! `lnk_field_type` (resolved at `init_record`/`checkLinks` while the channel
//! was up, `sseqRecord.c:225-238`) with the put refused at issue time. A link
//! that never
//! resolved a class takes C's `default:` arm instead, makes no put at all and
//! raises nothing — that boundary is `sseq_unresolved_lnk_no_wait.rs`.
//!
//! The line is C's `printf`, on stdout: oracleIoc (calc 3-7-5 sseq) with LNK1
//! `T1 CA` into a second IOC's `ASG RO` record prints, on `dbpf SQ.PROC 1`,
//! `sseq:processCallback: dbCaPutLinkCallback for link 0 failed.  Aborting.`
//! to stdout and nothing to stderr.

use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::{
    LinkDbfType, LinkMetadata, LinkPutOp, LinkSet, PutAdmission, PvDatabase,
};
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// A remote channel whose class is known but whose put admission is the test's
/// parameter — C `pca->isConnected && pca->hasWriteAccess`.
struct AdmissionLset {
    admission: PutAdmission,
}

#[async_trait::async_trait]
impl LinkSet for AdmissionLset {
    fn is_connected(&self, _: &str) -> bool {
        self.admission == PutAdmission::Connected
    }
    fn put_admission(&self, _: &str) -> PutAdmission {
        self.admission
    }
    fn link_metadata(&self, _: &str) -> Option<LinkMetadata> {
        Some(LinkMetadata {
            dbf_type: Some(LinkDbfType::Double),
            element_count: Some(1),
            ..Default::default()
        })
    }
    fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, name: &str) -> Option<EpicsValue> {
        self.get_cached_value(name)
    }
    async fn put_value(&self, _: &str, _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
        Ok(())
    }
}

async fn kick(db: &PvDatabase, name: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(name, &mut visited)
        .await
        .unwrap();
}

async fn poll_short(db: &PvDatabase, pv: &str, want: i16, label: &str) {
    for _ in 0..400 {
        if let Ok(EpicsValue::Short(v)) = db.get_pv(pv)
            && v == want
        {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "{label}: {pv} did not reach Short({want}) (last {:?})",
        db.get_pv(pv)
    );
}

/// Wait for a step's TARGET to hold the value the step put there — the fact a
/// "step n ran" assertion needs. `BUSY == 0` is a different fact: `finish`
/// clears `busy` inside `process()` (`sseq.rs:937-940`) while a no-wait step's
/// write is a queued [`ProcessAction::WriteDbLink`] (`sseq.rs:772-780`) the
/// framework runs after `process()` returns.
async fn poll_double(db: &PvDatabase, pv: &str, want: f64, label: &str) {
    for _ in 0..400 {
        if let Ok(EpicsValue::Double(v)) = db.get_pv(pv)
            && (v - want).abs() < 1e-10
        {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "{label}: {pv} did not reach Double({want}) (last {:?})",
        db.get_pv(pv)
    );
}

/// Three steps, the first a `WAITn` into a remote link that refuses the put.
async fn three_step_sseq(db: &PvDatabase, prefix: &str, admission: PutAdmission) {
    db.register_link_set("ca", Arc::new(AdmissionLset { admission }))
        .await;
    db.add_record(&format!("{prefix}_T2"), Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record(&format!("{prefix}_T3"), Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.put_field("SELM", EpicsValue::Short(0)).unwrap(); // All
    sseq.put_field("DO1", EpicsValue::Double(1.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("ca://NO:SUCH:PV".into()))
        .unwrap();
    sseq.put_field("WAIT1", EpicsValue::Short(1)).unwrap(); // Wait
    sseq.put_field("DO2", EpicsValue::Double(22.0)).unwrap();
    sseq.put_field("LNK2", EpicsValue::String(format!("{prefix}_T2 PP").into()))
        .unwrap();
    sseq.put_field("DO3", EpicsValue::Double(33.0)).unwrap();
    sseq.put_field("LNK3", EpicsValue::String(format!("{prefix}_T3 PP").into()))
        .unwrap();
    db.add_record(prefix, Box::new(sseq)).await.unwrap();
}

/// The finding: a refused put-callback aborts, so steps 2 and 3 never run.
#[epics_macros_rs::epics_test]
async fn a_refused_put_callback_aborts_the_whole_sequence() {
    let db = PvDatabase::new();
    three_step_sseq(&db, "SS_REF", PutAdmission::Refused).await;

    let mut abort_rx = db
        .get_record("SS_REF")
        .unwrap()
        .write()
        .add_subscriber(
            "ABORT",
            1,
            DbFieldType::Short,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .expect("an ABORT subscription must be accepted");

    kick(&db, "SS_REF").await;
    poll_short(&db, "SS_REF.BUSY", 0, "the aborted sequence must finish").await;
    // Settle before the negatives: `BUSY == 0` does not imply "no write will
    // land", because a step's write is a queued `ProcessAction::WriteDbLink`
    // the framework runs after `process()` returns, while `finish` clears
    // `busy` inside it. Without the settle a step that DID run could still be
    // in the queue and the assertions below would pass on a defect.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(40)).await;

    assert_eq!(
        db.get_pv("SS_REF_T2.VAL").unwrap(),
        EpicsValue::Double(0.0),
        "step 2 must never run — C aborts at step 1 (sseqRecord.c:744-747)"
    );
    assert_eq!(
        db.get_pv("SS_REF_T3.VAL").unwrap(),
        EpicsValue::Double(0.0),
        "step 3 must never run either"
    );
    assert_eq!(
        db.get_pv("SS_REF.WTG1").unwrap(),
        EpicsValue::Short(0),
        "`waiting` is raised only on the zero-status branch"
    );

    // C posts ABORT=1 at the refusal and ABORT=0 from `asyncFinish` (:477).
    let mut saw_raise = false;
    while let Ok(ev) = abort_rx.try_recv() {
        if ev.value().as_value().cloned().unwrap() == EpicsValue::Short(1) {
            saw_raise = true;
        }
    }
    assert!(
        saw_raise,
        "the refusal must post ABORT=1 (db_post_events(pR, &pR->abort, DBE_VALUE))"
    );
    assert_eq!(
        db.get_pv("SS_REF.ABORT").unwrap(),
        EpicsValue::Short(0),
        "and `asyncFinish` clears it again"
    );
}

/// The refusal's line goes to stdout, as C's `printf` does.
#[cfg(unix)]
#[epics_macros_rs::epics_test]
async fn the_refusal_is_printed_on_stdout() {
    use std::os::fd::AsRawFd;

    let db = PvDatabase::new();
    three_step_sseq(&db, "SS_OUT", PutAdmission::Refused).await;
    let out = tempfile::NamedTempFile::new().unwrap();
    let err = tempfile::NamedTempFile::new().unwrap();
    // SAFETY: nextest runs this test in its own process; nothing else holds
    // fds 1 and 2 while they are swapped.
    let saved = unsafe {
        let saved = (libc::dup(1), libc::dup(2));
        libc::dup2(out.as_file().as_raw_fd(), 1);
        libc::dup2(err.as_file().as_raw_fd(), 2);
        saved
    };
    kick(&db, "SS_OUT").await;
    poll_short(&db, "SS_OUT.BUSY", 0, "the aborted sequence must finish").await;
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    // SAFETY: restoring the descriptors this test replaced.
    unsafe {
        libc::dup2(saved.0, 1);
        libc::dup2(saved.1, 2);
        libc::close(saved.0);
        libc::close(saved.1);
    }
    let line = "sseq:processCallback: dbCaPutLinkCallback for link 0 failed.  Aborting.";
    let out = std::fs::read_to_string(out.path()).unwrap();
    let err = std::fs::read_to_string(err.path()).unwrap();
    assert!(out.lines().any(|l| l == line), "stdout: {out:?}");
    assert!(!err.contains(line), "stderr: {err:?}");
}

/// The counter-boundary, so the fix cannot be "never dispatch": the same
/// sequence with the put ADMITTED issues the callback, raises `WTG1`, and runs
/// to the end.
#[epics_macros_rs::epics_test]
async fn an_admitted_put_callback_still_waits_and_completes() {
    let db = PvDatabase::new();
    three_step_sseq(&db, "SS_OK", PutAdmission::Connected).await;

    let mut wtg_rx = db
        .get_record("SS_OK")
        .unwrap()
        .write()
        .add_subscriber(
            "WTG1",
            1,
            DbFieldType::Short,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .expect("a WTG1 subscription must be accepted");

    kick(&db, "SS_OK").await;
    poll_short(&db, "SS_OK.BUSY", 0, "the sequence must complete").await;
    poll_double(&db, "SS_OK_T2.VAL", 22.0, "step 2's write must land").await;
    poll_double(&db, "SS_OK_T3.VAL", 33.0, "step 3's write must land").await;

    assert_eq!(
        db.get_pv("SS_OK_T2.VAL").unwrap(),
        EpicsValue::Double(22.0),
        "an issued put-callback does not abort: step 2 runs"
    );
    assert_eq!(
        db.get_pv("SS_OK_T3.VAL").unwrap(),
        EpicsValue::Double(33.0),
        "and so does step 3"
    );
    let mut saw_wait = false;
    while let Ok(ev) = wtg_rx.try_recv() {
        if ev.value().as_value().cloned().unwrap() == EpicsValue::Short(1) {
            saw_wait = true;
        }
    }
    assert!(
        saw_wait,
        "the issued branch raises `waiting` and posts WTG1 (sseqRecord.c:749-751)"
    );
    assert_eq!(
        db.get_pv("SS_OK.ABORT").unwrap(),
        EpicsValue::Short(0),
        "nothing aborted"
    );
}
