//! C's `afterIocRunningHook` prints each queued line before running it, and a
//! line naming its failure after one that fails, both with a plain `printf`
//! (`afterIocRunning.c:47-51`, no epicsStdio.h), so a redirected `iocInit`
//! leaves them on fd 1. The lines it runs go through `iocshCmd` on the
//! `iocInit` thread and write into the redirect.

#![cfg(unix)]

use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

#[test]
fn each_queued_line_is_echoed_and_a_failed_one_is_named() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("ioc_init.out");
    let err = dir.path().join("ioc_init.err");
    let st_cmd = dir.path().join("st.cmd");
    std::fs::write(
        &st_cmd,
        format!(
            "afterIocRunning \"epicsEnvSet AIR_ECHO 1\"\n\
             afterIocRunning \"echo AIR_ECHOED\"\n\
             afterIocRunning \"noSuchAfterIocRunningCommand\"\n\
             iocInit > {} 2> {}\n",
            out.display(),
            err.display()
        ),
    )
    .unwrap();

    // RTEMS-EXEC-MODEL-ALLOW(1): the runtime only captures the shell's bridge; passes in the exec-backend suite.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    let path = st_cmd.to_str().unwrap().to_string();
    let console = fd1_during(|| {
        std::thread::spawn(move || {
            IocShell::new(Arc::new(PvDatabase::new()), bridge).execute_script(path)
        })
        .join()
        .unwrap()
        .unwrap();
    });

    let hook: Vec<&str> = console
        .lines()
        .filter(|l| l.contains("afterIocRunning:"))
        .collect();
    assert_eq!(
        hook,
        [
            "\x1b[32;1mafterIocRunning:\x1b[0m epicsEnvSet AIR_ECHO 1",
            "\x1b[32;1mafterIocRunning:\x1b[0m echo AIR_ECHOED",
            "\x1b[32;1mafterIocRunning:\x1b[0m noSuchAfterIocRunningCommand",
            "\x1b[31;1mERROR\x1b[0m afterIocRunning: command \
             'noSuchAfterIocRunningCommand' failed to run",
        ],
        "{console:?}"
    );
    let written = std::fs::read_to_string(&out).unwrap();
    assert_eq!(
        written
            .lines()
            .filter(|l| !l.starts_with("##"))
            .collect::<Vec<_>>(),
        ["AIR_ECHOED"],
        "{written:?}"
    );
    let diagnosed = std::fs::read_to_string(&err).unwrap();
    assert!(
        diagnosed.contains("Command 'noSuchAfterIocRunningCommand' not registered."),
        "{diagnosed:?}"
    );
    assert_eq!(std::env::var("AIR_ECHO").as_deref(), Ok("1"));
    assert_eq!(epics_base_rs::server::ioc_app::ioc_shutdown(), 0);
}

/// What `f` wrote to fd 1.
fn fd1_during(f: impl FnOnce()) -> String {
    let sink = tempfile::NamedTempFile::new().unwrap();
    // SAFETY: the shell writes through fd 1 for the length of one script and
    // this test owns the process, so nothing else holds it.
    let saved = unsafe {
        let saved = libc::dup(1);
        libc::dup2(sink.as_file().as_raw_fd(), 1);
        saved
    };
    f();
    let _ = std::io::stdout().flush();
    // SAFETY: restoring the descriptor this call replaced.
    unsafe {
        libc::dup2(saved, 1);
        libc::close(saved);
    }
    std::fs::read_to_string(sink.path()).unwrap()
}
