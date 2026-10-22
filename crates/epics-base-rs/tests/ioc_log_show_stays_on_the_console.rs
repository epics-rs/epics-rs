//! logClient.c does not include `epicsStdio.h`, so `iocLogShow`'s lines stay
//! on the process's stdout under `iocLogShow > f`. C oracle (`oracleStdIoc`
//! with a listener on `EPICS_IOC_LOG_PORT`): `f` empty, the lines on the
//! console. The log client is once per process, so the shell runs in a child.

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const CHILD: &str = "IOC_LOG_SHOW_STAYS_ON_THE_CONSOLE_FILE";

#[epics_macros_rs::epics_test]
async fn child_shell() {
    let Some(file) = std::env::var_os(CHILD) else {
        return;
    };
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    unsafe {
        std::env::set_var("EPICS_IOC_LOG_INET", "127.0.0.1");
        std::env::set_var("EPICS_IOC_LOG_PORT", port.to_string());
    }
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    // iocsh reads an unquoted `\` as an escape, as C `split()` does.
    let file = file.into_string().unwrap().replace('\\', "/");
    std::thread::spawn(move || {
        let shell = IocShell::new(Arc::new(PvDatabase::new()), bridge);
        shell.execute_line("iocLogInit").unwrap();
        let _peer = server.accept().unwrap();
        shell
            .execute_line(format!("iocLogShow 0 > {file}"))
            .unwrap();
    })
    .join()
    .unwrap();
}

#[test]
fn ioc_log_show_s_lines_stay_on_the_console() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f.txt");
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_shell", "--nocapture", "--test-threads=1"])
        .env(CHILD, &file)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "");
    let console = String::from_utf8_lossy(&out.stdout);
    assert!(
        console.contains("log client: ") && console.contains(" log server at '127.0.0.1:"),
        "{console}"
    );
}
