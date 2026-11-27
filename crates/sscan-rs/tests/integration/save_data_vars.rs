//! `sscanSupport.dbd`'s `registrar(saveDataRegistrar)` and its three
//! `variable()` lines reach an IOC that loads sscan.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use sscan_rs::save_data::{mda, task};

#[epics_base_rs::epics_test]
async fn the_save_data_variables_are_set_by_var() {
    let _ = sscan_rs::sscan_record_factory();
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let db = Arc::new(PvDatabase::new());
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        let shell = IocShell::new(db, bridge);
        for line in [
            "var debug_saveData 1",
            "var debug_saveDataMsg 2",
            "var saveData_MessagePolicy 3",
            "saveData_CVS",
        ] {
            shell
                .execute_line(line)
                .unwrap_or_else(|e| panic!("{line}: {e:?}"));
        }
    })
    .join()
    .expect("the shell thread");
    assert_eq!(mda::DEBUG_SAVE_DATA.load(Ordering::Relaxed), 1);
    assert_eq!(mda::DEBUG_SAVE_DATA_MSG.load(Ordering::Relaxed), 2);
    assert_eq!(task::MESSAGE_POLICY.load(Ordering::Relaxed), 3);
}
