//! The bare shell's `iocInit` runs every dset's `init(0)` and `init(1)`,
//! used or not, as C `initDevSup` and `finishDevSup` do
//! (`iocInit.c:441-480`).

use crate::dset_init_pass_probe as probe;

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

#[epics_macros_rs::epics_test]
async fn unused_dsets_init_in_record_type_order() {
    probe::register();
    let db = Arc::new(PvDatabase::new());
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    std::thread::spawn(move || IocShell::new(db, bridge).execute_line("iocInit"))
        .join()
        .unwrap()
        .unwrap();
    probe::assert_logged(true);
}
