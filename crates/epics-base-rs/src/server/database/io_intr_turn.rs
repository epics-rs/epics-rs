//! Turns on the I/O Intr pumps: a CP link's first event reads an I/O Intr
//! record as an item on that record's pump, behind every interrupt queued to
//! it before the request.

use std::collections::HashMap;
use std::sync::Arc;

use crate::runtime::interrupt_accept::Outstanding;
use crate::runtime::sync::{mpsc, oneshot};
use crate::server::record::{RecordCell, ScanType};

/// The edge sweeps `record`'s device still owes it, for a record on I/O Intr
/// scan: what a CP link's first event waits for before its turn, so the
/// turn comes behind the interrupts they queue. `None` when none is owed.
pub(crate) fn owed_sweeps(record: &Arc<RecordCell>) -> Option<Arc<Outstanding>> {
    let inst = record.read();
    if inst.common.scan != ScanType::IoIntr {
        return None;
    }
    inst.device
        .as_ref()?
        .edge_sweeps()
        .filter(|sweeps| !sweeps.is_clear())
}

/// A turn request: answered once the pump has processed every interrupt
/// queued to it when it took the request.
pub(crate) type Turn = oneshot::Sender<()>;

/// Each pumped record's turn queue, by the record's cell. The pump holds its
/// record, so a key's record lives while its pump does.
#[derive(Default)]
pub(crate) struct IoIntrTurns {
    pumps: parking_lot::Mutex<HashMap<usize, mpsc::UnboundedSender<Turn>>>,
}

impl IoIntrTurns {
    /// `record`'s pump's turn requests.
    pub(crate) fn register(&self, record: &Arc<RecordCell>) -> mpsc::UnboundedReceiver<Turn> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.pumps.lock().insert(Arc::as_ptr(record) as usize, tx);
        rx
    }

    /// A turn on `record`'s pump, answered once the pump has processed
    /// every interrupt queued to it now. `None` for a record with no
    /// running pump.
    pub(crate) fn ask(&self, record: &Arc<RecordCell>) -> Option<oneshot::Receiver<()>> {
        let (turn, done) = oneshot::channel();
        self.pumps
            .lock()
            .get(&(Arc::as_ptr(record) as usize))
            .is_some_and(|pump| pump.send(turn).is_ok())
            .then_some(done)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::task::Poll;
    use std::time::Duration;

    use crate::error::CaResult;
    use crate::runtime::sync::mpsc;
    use crate::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
    use crate::server::ioc_builder::IocBuilder;
    use crate::server::record::{DbLinkType, DevSup, Dset, Record};

    static HELD: DevSup = DevSup {
        name: "devAiHeld",
        choice: "Held",
        link_type: DbLinkType::InstIo,
        dset: Dset::new(6, &[2, 3, 4], false),
    };

    /// An I/O Intr source whose every read reports its start and then waits
    /// for the test to release it.
    struct Held {
        intr: Option<mpsc::Receiver<()>>,
        started: std::sync::mpsc::Sender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl DeviceSupport for Held {
        fn dtyp(&self) -> &str {
            "Held"
        }
        fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
            self.intr.take()
        }
        fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
            let _ = self.started.send(());
            let _ = self
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10));
            Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
        }
        fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
            Ok(())
        }
    }

    /// The turn is answered only after the pump has processed the interrupt
    /// that was queued behind the read in flight when the turn was asked.
    #[epics_macros_rs::epics_test]
    async fn a_turn_waits_for_the_interrupt_queued_before_it() {
        let (pulse, intr) = mpsc::channel(4);
        let (started_tx, started) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let device = Mutex::new(Some(Held {
            intr: Some(intr),
            started: started_tx,
            release: Mutex::new(release_rx),
        }));
        let (db, _) = IocBuilder::new()
            .register_device_support("ai", &HELD, move || {
                Box::new(device.lock().unwrap().take().expect("one Held record"))
            })
            .db_string(
                r#"record(ai, "SRC") { field(DTYP, "Held") field(INP, "@x") field(SCAN, "I/O Intr") }"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let src = db.get_record("SRC").unwrap();
        db.scan_control().run();
        let started = || {
            started
                .recv_timeout(fact_budget::FACT_BUDGET)
                .expect("a read starts")
        };

        pulse.try_send(()).unwrap();
        started();
        // Queued behind the read in flight, before the turn is asked.
        pulse.try_send(()).unwrap();
        let mut turn = std::pin::pin!(db.ask_io_intr_turn(&src).expect("SRC has a pump"));
        let mut poll =
            async || std::future::poll_fn(|cx| Poll::Ready(turn.as_mut().poll(cx))).await;
        assert!(poll().await.is_pending(), "the turn is asked");

        release.send(()).unwrap();
        started();
        assert!(
            poll().await.is_pending(),
            "the turn is not answered while the queued interrupt's read runs"
        );

        release.send(()).unwrap();
        crate::runtime::task::timeout(fact_budget::FACT_BUDGET, turn)
            .await
            .expect("the turn is answered once the queued interrupt is processed")
            .expect("the pump answers the turn");
    }
}
