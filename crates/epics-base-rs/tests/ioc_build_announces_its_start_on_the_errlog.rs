//! `iocBuild` must say `Starting iocInit` on the errlog.
//!
//! C `iocBuild_1` prints it (`iocInit.c:129`) between the
//! `initHookAtIocBuild` and `initHookAtBeginning` announces, and it is the
//! first thing a C IOC's console shows for a boot. This port emitted it
//! nowhere, so a consumer that tells "the IOC began initialising" from "the
//! process printed nothing" — `epics_oracle_rs::ioc`'s boot classifier does
//! exactly that — could never reach `Ready` on a Rust IOC's own output.
//!
//! Only the line's presence is asserted. Its position relative to the two
//! announces is a fact of the source, and the errlog delivers on its own
//! worker thread, so ordering it against a hook callback would be timing,
//! not parity.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::ioc_app::IocApplication;

#[epics_macros_rs::epics_test]
async fn a_boot_prints_starting_iocinit() {
    let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    let id = epics_base_rs::runtime::log::errlog_add_listener(move |m| {
        sink.lock().unwrap().push(m.to_string());
    });

    IocApplication::new()
        .port(0)
        .run(|_config| async move { Ok(()) })
        .await
        .unwrap();

    // Both buffers handled, so every message logged during the boot has
    // reached the listener.
    epics_base_rs::runtime::log::errlog_flush();
    epics_base_rs::runtime::log::errlog_remove_listener(id);

    let seen = lines.lock().unwrap();
    assert!(
        seen.iter().any(|m| m.contains("Starting iocInit")),
        "iocBuild announced no start; the errlog carried {seen:?}"
    );
}
