//! A port thread's first contended lock allocates nothing. C creates it with
//! `epicsThreadCreate` (asynManager.c:2081), and as an IOC thread here it
//! builds its `parking_lot_core` park state at entry; otherwise that state is
//! built in the first park, and the table grows when live threads cross 6,
//! 11, 22, 43, 86, … — two allocations inside a driver call. One port proves
//! nothing: each of `PORTS` ports, all kept alive, makes its registration
//! connect while its driver's lock is held, so its thread's first park is that
//! acquire and the live count climbs through those thresholds.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use alloc_count::thread;
use asyn_rs::error::AsynResult;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;

const PORTS: usize = 100;

/// A driver whose connect takes a lock the test holds, and reports what the
/// acquire cost its thread.
struct Device {
    base: PortDriverBase,
    gate: Arc<parking_lot::Mutex<()>>,
    /// Told when connect is about to take `gate`, so the holder releases it
    /// only after the port thread started, however long that took.
    entered: std::sync::mpsc::Sender<()>,
    contended: Arc<AtomicBool>,
    allocs: Arc<AtomicU64>,
}

impl PortDriver for Device {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        let held = self.gate.try_lock().is_none();
        let _ = self.entered.send(());
        let window = thread::open();
        drop(self.gate.lock());
        let allocs = window.close().allocs;
        self.contended.store(held, Ordering::SeqCst);
        self.allocs.store(allocs, Ordering::SeqCst);
        self.base_mut().set_connected(true);
        Ok(())
    }
}

#[test]
fn a_port_thread_s_first_contended_lock_allocates_nothing() {
    let mut ports = Vec::with_capacity(PORTS);
    for i in 0..PORTS {
        let gate = Arc::new(parking_lot::Mutex::new(()));
        let contended = Arc::new(AtomicBool::new(false));
        let allocs = Arc::new(AtomicU64::new(u64::MAX));
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (entered, entered_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn({
            let gate = Arc::clone(&gate);
            move || {
                let _gate = gate.lock();
                held_tx.send(()).unwrap();
                let _ = entered_rx.recv();
                // Long enough for connect to park on the lock.
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        held_rx.recv().unwrap();
        let mut base = PortDriverBase::new(
            &format!("PARK{i}"),
            1,
            PortFlags {
                can_block: true,
                ..PortFlags::default()
            },
            InterfaceMask::NONE,
            InterfaceMask::NONE,
        );
        // Disconnected, so registration hands the thread a connect before it
        // ever idles: tokio's idle park would otherwise build the park state.
        base.set_connected(false);
        let (rt, jh) = create_port_runtime_unclaimed(
            Device {
                base,
                gate,
                entered,
                contended: Arc::clone(&contended),
                allocs: Arc::clone(&allocs),
            },
            RuntimeConfig::default(),
        )
        .unwrap();
        holder.join().unwrap();
        while allocs.load(Ordering::SeqCst) == u64::MAX {
            std::thread::yield_now();
        }
        assert!(
            contended.load(Ordering::SeqCst),
            "port {i} connected before the lock was held"
        );
        assert_eq!(
            allocs.load(Ordering::SeqCst),
            0,
            "port thread {i}'s first contended lock"
        );
        ports.push((rt, jh));
    }
    for (rt, jh) in ports {
        rt.shutdown();
        let _ = jh.join();
    }
}
