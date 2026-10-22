//! saveData_writeXDR.c includes `epicsStdio.h`, so `saveData_Init`'s lines
//! go to the shell thread's stdout and `saveData_Init(...) > f` puts them in
//! `f`. The expected files are the C oracle IOC's (`oracleSscanIoc`) for the
//! same lines in the same order.

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

#[epics_base_rs::epics_test]
async fn save_data_init_s_lines_go_to_the_line_s_file() {
    let _ = sscan_rs::sscan_record_factory();
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let db = Arc::new(PvDatabase::new());
    let dir = tempfile::tempdir().unwrap();
    // iocsh reads an unquoted `\` as an escape, as C `split()` does.
    let d = dir.path().display().to_string().replace('\\', "/");
    let files = std::thread::spawn(move || {
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        let shell = IocShell::new(db, bridge);
        let init = format!("saveData_Init(\"{d}/nosuch.req\", \"\")");
        [
            init.as_str(),
            &init,
            "saveData_Info",
            "saveData_PrintScanInfo(\"x\")",
        ]
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let file = format!("{d}/{i}.txt");
            shell.execute_line(format!("{line} > {file}")).unwrap();
            std::fs::read_to_string(file).unwrap()
        })
        .collect::<Vec<_>>()
    })
    .join()
    .unwrap();
    assert_eq!(
        files,
        [
            "saveData: message queue created\n",
            "saveData already initialized\n",
            "saveData: scan info:\n",
            "",
        ]
    );
}
