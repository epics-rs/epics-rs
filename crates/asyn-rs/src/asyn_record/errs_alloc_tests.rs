//! ERRS is C's `errs[ERR_SIZE]`, formatted in place by `reportError`
//! (asynRecord.c:2037), and the manager's diagnostic is C's fixed
//! `errorMessage` (asynManager.c:1194): every report a record makes under its
//! hold formats without allocating — its ASYN_TRACE_ERROR line included
//! (asynRecord.c:2037-2040). Counted with this binary's own allocator on the
//! calling thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

use super::io_intr::NoInterface;
use super::{AsynRecord, ERR_SIZE, Errs, IoOutcome, IoTrace, MessageOrEmpty};
use crate::error::{AsynError, AsynStatus};

thread_local! {
    /// Allocation calls made by this thread, so concurrently running tests
    /// cannot move each other's count.
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

// SAFETY: forwards every call to `System` unchanged; the counter is a
// const-initialised thread-local `Cell`, which never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.with(|n| n.set(n.get() + 1));
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.with(|n| n.set(n.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const LONG: &str = "Error in GPIB Serial Poll read, a driver diagnostic long enough \
                    that ERR_SIZE cuts it before its end";

#[test]
fn every_errs_report_formats_without_allocating() {
    let mut rec = AsynRecord::default();
    let e = AsynError::Status {
        status: AsynStatus::Timeout,
        message: "port down".into(),
    };
    let written = EpicsValue::CharArray(b"client text\0\0".to_vec().into());
    let mut trace = IoTrace::default();
    rec.fill_io_trace(&mut trace);
    // The first trace line resolves the global rung and this thread's zone.
    rec.report_error(AsynStatus::Error, format_args!("warm"));
    let before = ALLOCS.with(Cell::get);

    rec.report_error(
        AsynStatus::Error,
        format_args!("Int32 read error, {}", e.message_display()),
    );
    let int32 = rec.errs;
    rec.reset_error();
    rec.report_error(AsynStatus::Error, format_args!("{LONG}{LONG}"));
    let long = rec.errs;
    let mut out = IoOutcome::default();
    out.report_error(
        &trace,
        AsynStatus::Error,
        format_args!("Overflow nread {} {}", 5, MessageOrEmpty(Some(&e))),
    );
    rec.set_errs(out.errs.unwrap());
    let overflow = rec.errs;
    rec.report_error(
        AsynStatus::Error,
        format_args!("{}", NoInterface("asynInt32")),
    );
    let iface = rec.errs;
    // The manager's own refusal, spliced the way `connectDevice` reports it.
    let _ = rec.device_user.is_connected();
    rec.report_connect_error();
    let connect = rec.errs;
    let _ = rec.queue_special();
    let queue = rec.errs;
    rec.put_field_internal("ERRS", written).unwrap();
    let client = rec.errs;

    let allocs = ALLOCS.with(Cell::get) - before;
    assert_eq!(allocs, 0);
    assert_eq!(int32, "Int32 read error, port down");
    assert_eq!(long.len(), ERR_SIZE - 1);
    assert!(LONG.repeat(2).starts_with(long.as_str()));
    assert_eq!(overflow, "Overflow nread 5 port down");
    assert_eq!(iface, "No asynInt32 interface");
    assert_eq!(
        connect,
        "Connect error, status=3, asynManager:isConnected asynUser not connected to device"
    );
    assert_eq!(
        queue,
        "asynManager::queueRequest asynUser not associated with a port"
    );
    assert_eq!(client, "client text");
    assert_eq!(Errs::EMPTY, "");
}

/// C `process()` queues the record's one `asynUser`, made at init
/// (asynRecord.c:306-308, :342), and `queueRequest` links it without
/// allocating (asynManager.c:1606-1626). A cycle on a blocking port — the
/// queue under the hold and the completion re-entry that applies the result —
/// allocates nothing once the record has run one.
#[test]
fn a_blocking_port_cycle_queues_and_completes_without_allocating() {
    use std::sync::Arc;

    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::server::record::RecordProcessResult;

    use super::{InterfaceType, TransferMode};
    use crate::error::AsynResult;
    use crate::port::{PortDriver, PortDriverBase, PortFlags};
    use crate::port_actor::PortActor;
    use crate::port_handle::PortHandle;
    use crate::user::AsynUser;

    struct Device {
        base: PortDriverBase,
    }
    impl PortDriver for Device {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn read_int32(&mut self, _user: &AsynUser) -> AsynResult<i32> {
            Ok(7)
        }
        fn write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
            Ok(data.len())
        }
    }

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let actor = PortActor::new(
        Box::new(Device {
            base: PortDriverBase::new("ALLOCIO", 1, PortFlags::default()),
        }),
        rx,
    );
    let link = actor.link();
    std::thread::spawn(move || actor.run());
    let mut handle = PortHandle::new(
        tx,
        "ALLOCIO".into(),
        Arc::new(crate::interrupt::InterruptManager::new(16)),
        link,
        Arc::new(crate::trace::TraceManager::new()),
    );
    handle.set_can_block(true);

    let mut rec = AsynRecord::default();
    // The plan copies the port name for the trace line a report prints.
    rec.port = "ALLOCIO".into();
    rec.device_user
        .attach_for_test(crate::registry::PortEntry { handle }, rec.addr);
    // A database without this record: the re-entry it is owed reaches
    // nothing, and the test drives the completion pass itself.
    let db = PvDatabase::new();
    rec.set_async_context("ALLOC_REC".into(), db.async_handle());
    rec.aout = "query\\r\\n".into();

    let cycle = |rec: &mut AsynRecord| -> u64 {
        let before = ALLOCS.with(Cell::get);
        let queued = rec.process().unwrap();
        let queueing = ALLOCS.with(Cell::get) - before;
        assert_eq!(queued.result, RecordProcessResult::AsyncPending);
        let cell = rec.process_cell.clone().unwrap();
        while cell.is_outstanding() {
            std::thread::yield_now();
        }
        let before = ALLOCS.with(Cell::get);
        let done = rec.process().unwrap();
        let completing = ALLOCS.with(Cell::get) - before;
        assert_eq!(done.result, RecordProcessResult::Complete);
        queueing + completing
    };

    for (iface, tmod) in [
        (InterfaceType::Int32, TransferMode::Read),
        (InterfaceType::Octet, TransferMode::Write),
    ] {
        rec.iface = iface as i32;
        rec.tmod = tmod as i32;
        cycle(&mut rec);
        let allocs: u64 = (0..3).map(|_| cycle(&mut rec)).sum();
        assert_eq!(allocs, 0, "{iface:?} {tmod:?}");
        assert_eq!(rec.errs, "", "{iface:?} {tmod:?}");
    }
    assert_eq!(rec.i32inp, 7);
    assert_eq!(rec.nawt, 7, "query\\r\\n is seven bytes on the wire");
}

/// C `special()` queues each option, EOS and CNCT put on a `duplicateAsynUser`
/// copy with its `callbackMessage`, both from asynManager's freelists
/// (asynRecord.c:531-537, :571-572): once a request has been released, the
/// next put reuses it. The put's queueing — all `special()` does under the
/// hold on a blocking port — allocates nothing after one round of each arm.
#[test]
fn a_special_put_on_a_blocking_port_queues_without_allocating() {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use epics_base_rs::server::database::PvDatabase;

    use crate::port::{PortDriver, PortDriverBase, PortFlags};
    use crate::port_actor::PortActor;
    use crate::port_handle::PortHandle;

    struct Device {
        base: PortDriverBase,
    }
    impl PortDriver for Device {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
    }

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let actor = PortActor::new(
        Box::new(Device {
            base: PortDriverBase::new("ALLOCSP", 1, PortFlags::default()),
        }),
        rx,
    );
    let link = actor.link();
    std::thread::spawn(move || actor.run());
    let mut handle = PortHandle::new(
        tx,
        "ALLOCSP".into(),
        Arc::new(crate::interrupt::InterruptManager::new(16)),
        link,
        Arc::new(crate::trace::TraceManager::new()),
    );
    handle.set_can_block(true);

    let mut rec = AsynRecord::default();
    rec.device_user
        .attach_for_test(crate::registry::PortEntry { handle }, rec.addr);
    let db = PvDatabase::new();
    rec.set_async_context("ALLOC_SP".into(), db.async_handle());
    rec.hostinfo = "127.0.0.1:5025".into();
    rec.ieos = "\\r\\n".into();
    rec.lbaud = 115200;
    // CNCT asks for the state the port is in: no driver call, port stays up.
    rec.cnct = 1;

    let put = |rec: &mut AsynRecord, field: &str| -> u64 {
        let before = ALLOCS.with(Cell::get);
        rec.special(field, true).unwrap();
        let allocs = ALLOCS.with(Cell::get) - before;
        assert_eq!(rec.errs, "", "{field} was queued");
        let settled = |rec: &AsynRecord| {
            rec.special_lane.in_flight.load(Ordering::Acquire) == 0
                && rec.special_cells.iter().all(|s| !s.cell.is_outstanding())
        };
        while !settled(rec) {
            std::thread::yield_now();
        }
        allocs
    };

    let fields = ["BAUD", "LBAUD", "HOSTINFO", "IEOS", "CNCT"];
    for field in fields {
        put(&mut rec, field);
    }
    for field in fields {
        assert_eq!(put(&mut rec, field), 0, "{field}");
    }
    assert_eq!(rec.special_cells.len(), 1, "a released request is reused");
}

/// C's queue timer runs on `pasynBase->timerQueue`'s thread (asynManager.c:
/// 501-502, :1189-1190), so `queueTimeoutCallback` unlinks a queued request
/// while the port thread is inside the driver (:649-701), and the request's
/// `asynUser` may be queued again at once. A cell has no waiter to own its
/// deadline, so the timer must; the entry it left must not run the request
/// queued next; and neither queueing allocates.
#[test]
fn a_queued_cell_times_out_while_the_driver_blocks() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    use crate::error::AsynResult;
    use crate::port::{PortDriver, PortDriverBase, PortFlags};
    use crate::port_actor::PortActor;
    use crate::port_handle::PortHandle;
    use crate::request::{CellBody, PortCalls, RequestOp, Unrun};
    use crate::user::AsynUser;

    struct Gated {
        base: PortDriverBase,
        entered: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl PortDriver for Gated {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn read_int32(&mut self, _user: &AsynUser) -> AsynResult<i32> {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            Ok(0)
        }
    }

    #[derive(Default)]
    struct Counts {
        processed: AtomicUsize,
        timed_out: AtomicUsize,
    }
    impl CellBody for Counts {
        fn process(&self, _port: &mut dyn PortCalls) {
            self.processed.fetch_add(1, Ordering::SeqCst);
        }
        fn unrun(&self, why: Unrun) {
            assert!(matches!(why, Unrun::TimedOut), "only the deadline ends it");
            self.timed_out.fetch_add(1, Ordering::SeqCst);
        }
        fn done(self: Arc<Self>) {}
    }

    let (entered_tx, entered) = mpsc::sync_channel(1);
    let (release, release_rx) = mpsc::channel();
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let actor = PortActor::new(
        Box::new(Gated {
            base: PortDriverBase::new("QTIMER", 1, PortFlags::default()),
            entered: entered_tx,
            release: Mutex::new(release_rx),
        }),
        rx,
    );
    let link = actor.link();
    std::thread::spawn(move || actor.run());
    let handle = PortHandle::new(
        tx,
        "QTIMER".into(),
        Arc::new(crate::interrupt::InterruptManager::new(16)),
        link,
        Arc::new(crate::trace::TraceManager::new()),
    );
    let counts = Arc::new(Counts::default());
    let cell = handle.new_cell(counts.clone());

    let jammer = handle.clone();
    let jam = std::thread::spawn(move || {
        jammer
            .submit_blocking(RequestOp::Int32Read, AsynUser::new(0))
            .unwrap()
    });
    entered.recv().unwrap();

    // Each round's request is free to be queued again the moment it times
    // out, all while the port thread is in the driver.
    const ROUNDS: usize = 8;
    let user = AsynUser::new(0).with_queue_timeout(Duration::from_millis(5));
    let started = Instant::now();
    let mut allocs = 0;
    for round in 1..=ROUNDS {
        let before = ALLOCS.with(Cell::get);
        cell.queue(&user).unwrap();
        allocs += ALLOCS.with(Cell::get) - before;
        while counts.timed_out.load(Ordering::SeqCst) < round
            && started.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            counts.timed_out.load(Ordering::SeqCst),
            round,
            "the timeout fires while the driver is still blocked"
        );
    }
    let user = AsynUser::new(0);
    let before = ALLOCS.with(Cell::get);
    cell.queue(&user).unwrap();
    allocs += ALLOCS.with(Cell::get) - before;
    release.send(()).unwrap();
    jam.join().unwrap();
    while cell.is_outstanding() && started.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(1));
    }
    // Room for a stale entry to run after the last request did.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        counts.processed.load(Ordering::SeqCst),
        1,
        "the last request runs once; the timed-out ones' entries run nothing"
    );
    assert_eq!(counts.timed_out.load(Ordering::SeqCst), ROUNDS);
    assert_eq!(allocs, 0, "queueing allocates nothing");
}
