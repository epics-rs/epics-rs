//! `streamReload` holds the record, where C's runs unlocked
//! (StreamEpics.cc:193): the bus it replaces may wait for a callback of its
//! that waits for the record, as asyn's cancelRequest waits for a running
//! handler (asynManager.c:1647-1655), so the bus must go only once the
//! record is let go.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;

use epics_base_rs::runtime::task::test_block_on;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use parking_lot::{Condvar, Mutex};
use stream_rs::bus::{self, Client, StreamBusInterface, StreamIoStatus};

/// The buses dropped so far.
static DROPPED: AtomicUsize = AtomicUsize::new(0);

/// Grants the lock from a thread of its own once it is released, and on
/// its drop waits for that thread.
struct Waiting {
    client: Weak<dyn Client>,
    released: Arc<(Mutex<bool>, Condvar)>,
    callback: Mutex<Option<JoinHandle<()>>>,
}

impl StreamBusInterface for Waiting {
    fn lock_request(&self, _: u64) -> bool {
        let (client, released) = (self.client.clone(), self.released.clone());
        *self.callback.lock() = Some(std::thread::spawn(move || {
            let mut go = released.0.lock();
            released.1.wait_while(&mut go, |go| !*go);
            drop(go);
            if let Some(client) = client.upgrade() {
                client.lock_callback(StreamIoStatus::StreamIoSuccess);
            }
        }));
        true
    }
    fn unlock(&self) -> bool {
        true
    }
    fn write_request(&self, _: &[u8], _: u64) -> bool {
        true
    }
    fn release(&self) {
        *self.released.0.lock() = true;
        self.released.1.notify_all();
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if let Some(callback) = self.callback.lock().take() {
            callback.join().unwrap();
        }
        DROPPED.fetch_add(1, Ordering::SeqCst);
    }
}

fn waiting_bus(
    client: Weak<dyn Client>,
    busname: &str,
    _: i32,
    _: &str,
) -> Option<Box<dyn StreamBusInterface>> {
    (busname == "W").then(|| {
        Box::new(Waiting {
            client,
            released: Default::default(),
            callback: Mutex::new(None),
        }) as _
    })
}

#[test]
fn a_reload_returns_with_the_replaced_bus_gone_after_its_callback() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("reload_drops_bus");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("w.proto"), "p { out \"W\"; }\n").unwrap();
    // SAFETY: set before any thread of this test binary reads it.
    unsafe { std::env::set_var("STREAM_PROTOCOL_PATH", &dir) };
    bus::register_stream_bus_interface("Waiting", waiting_bus);
    test_block_on(async {
        let ioc = stream_rs::dev::register(IocBuilder::new())
            .db_string(
                "record(longout, \"w\") { field(DTYP, \"stream\") field(OUT, \"@w.proto p W\") }\n",
                &HashMap::new(),
            )
            .unwrap()
            .build_ioc()
            .await
            .unwrap();
        let db = ioc.database().clone();
        assert!(ioc.run().await);
        // The process waits for the lock, which comes only after the reload
        // released the bus, while the reload holds the record.
        db.put_record_field_from_ca_no_notify("w", "PROC", EpicsValue::Long(1))
            .await
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let status = stream_rs::epics::stream_reload(Some("w"), &mut out);
            let _ = tx.send((status, String::from_utf8(out).unwrap()));
        });
        let reloaded = rx.recv_timeout(fact_budget::FACT_BUDGET);
        assert_eq!(reloaded, Ok((0, "w: Protocol reloaded\n".to_string())));
        assert_eq!(
            DROPPED.load(Ordering::SeqCst),
            1,
            "the replaced bus is gone"
        );
    });
}
